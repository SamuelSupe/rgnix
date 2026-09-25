# OTLP 访问日志验证

> 阶段历史记录：下文保留验证当时的版本、镜像和发布状态。后续 v0.2.0 发布包的实际验收与摘要见[发布验证](validation-release-0.2.0.md)。

日期：2026-09-25，Asia/Singapore。新增 OTLP/HTTP protobuf 访问日志导出；[配置和投递契约](otlp.md)。本轮未发布新版本，也未更改已发布的 v0.1.0 下载包。

## 实际结果

| 检查 | 结果 |
|---|---|
| Rust 单元测试 | 6/6；OrbStack Ubuntu arm64，Rust 1.90.0 |
| fmt、Clippy（含所有 targets）、Helm lint | 通过；[记录](validation/otlp-linux-checks.txt) |
| 现有 HTTP/TLS/代理/插件回归 | 79/79；[记录](validation/otlp-http.json) |
| 现有模拟 Kubernetes API/恢复检查 | 45/45；[记录](validation/otlp-recovery.json) |
| 新增 OTLP 网络及故障检查 | 32/32，另对镜像提取的 release 二进制完成 32/32；[debug 检查](validation/otlp-transport.json)、[镜像检查](validation/otlp-image-transport.json) |
| 真实 Collector + 双副本 Ingress | HTTPS、Bearer 鉴权、私有 CA、protobuf 解码、每个 Pod 的资源属性均通过；[运行输出](validation/otlp-kubernetes.txt)、[Collector 解码结果](validation/otlp-collector-decoded.txt) |

使用仓库 Dockerfile 构建 Linux arm64 镜像（Rust 1.98.0 / Debian bookworm），部署于 OrbStack 的独立命名空间 `rgnix-otlp-qa-20260925`；两个 rgnix Pod 和一个 OpenTelemetry Collector Contrib 0.123.0 Pod。原有 Body 验证命名空间保持原状。镜像、二进制、源码及锁文件摘要见 [otlp-artifact.json](validation/otlp-artifact.json)。

已核对两个运行 Pod 的二进制 SHA-256 与镜像提取文件相同。该 release 二进制也在 OrbStack Ubuntu 中通过完整 OTLP 网络/故障检查；原有 6/79/45 回归使用同源 Rust 1.90.0 debug 构建。

首次镜像构建遇到环境代理证书不在镜像信任链的问题，随后通过 Dockerfile 已有的 `build_ca` secret 使用 OrbStack 的 CA bundle 完成构建。该构建证书没有写入最终镜像，未关闭 TLS 校验。

## 覆盖的行为边界

- HTTP/HTTPS 真实网络发送；标准基础 URL 追加 `/v1/logs`；日志专用认证头优先于通用头，转义后的值正确送达。
- 原有本地日志继续输出，`access_log off` 和 SIGHUP 更新同时影响 OTLP。导出排除 query、Body、Cookie 和请求 Authorization。
- Collector 503 或断连重试同一批字节；400/401/500、重定向、非法 protobuf、错误 Content-Type、过大响应不会被当作成功或重复发送。
- 部分成功响应的拒收计数生效，不重试；429 的 Retry-After 不突破导出总超时。
- 采集端卡住时，业务请求继续完成；队列满丢弃新记录、内存队列数量有界，超时丢弃有指标。
- 自签 CA 可显式信任；缺少 CA、主机名不匹配均拒绝 TLS。
- 未满批次在退出时刷新；SIGTERM 的在途请求先完成再导出日志；采集端持续卡住时退出刷新受 5 秒期限约束。
- 非法协议、URL userinfo、认证头换行、队列/批次冲突会拒绝启动，诊断不暴露测试认证信息。
- 真实 Collector 实际解码两副本记录中的时间、类型化状态码、字节数、耗时、后端、配置摘要及 Kubernetes 资源属性；确认收到的日志不含请求查询参数。

## 复现

```sh
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target cargo clippy --locked --all-targets -- -D warnings'
helm lint charts/rgnix
docker build -t rgnix:0.1.0-otlp-qa2 .
RGNIX_IMAGE_TAG=0.1.0-otlp-qa2 RGNIX_OTLP_EVIDENCE_DIR=.local/otlp-evidence \
  bash scripts/otlp-collector-e2e.sh rgnix-otlp-qa-example orbstack
```

Collector 脚本只接受专用 namespace，复用时要求 `rgnix-qa=true` 标签。它创建合成测试认证和 1 天有效期的测试证书，保留 namespace 便于检查；重复运行会轮换证书并重启 exporter。测试用 Collector 的 health 接口同时作为 Ingress 的真实 Service 后端，验证路由和日志导出链路。证书过期后应重跑脚本，不能将此测试配置当作生产部署。

按需清理此轮测试环境：

```sh
helm uninstall rgnix-otlp -n rgnix-otlp-qa-20260925 --kube-context orbstack
kubectl --context orbstack delete namespace rgnix-otlp-qa-20260925
```

## 尚未验证

没有第三方 SaaS 账户/endpoint 凭据，因此没有声称完成具体平台的鉴权、索引或查询验证。真实 Collector 的接收与解码已验证 OTLP logs 兼容性。未进行 amd64、本轮 NGINX 对照、完整 Kubernetes 生命周期重跑、多节点、长期断网或性能/容量基线测试；此前 Body 阶段的完整记录保留在[请求体路由验证](validation-body.md)。

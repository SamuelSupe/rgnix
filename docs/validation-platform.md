# 平台治理与变更管理验证 — 2026-09-25

> 阶段历史记录：下文保留验证当时的版本、镜像和发布状态。后续 v0.2.0 发布包的实际验收与摘要见[发布验证](validation-release-0.2.0.md)。

本轮实现多租户配额、声明式灰度和变更管理，并修复 HTTPS 健康检查、gRPC 错误观测、外部认证端点故障转移、认证前限流、上游 mTLS 及访问日志脱敏问题。配置说明见[中文操作指南](platform-policies.zh-CN.md)与[完整参数参考](platform-policies.md)。

验证针对当前工作区和本地 `rgnix:0.1.0-platform-qa4` 镜像。镜像由 Rust 1.98.0 / Debian bookworm 构建；Rust 单元测试与 Clippy 使用 OrbStack Ubuntu 的 Rust 1.90.0。下表的网络测试均使用最终镜像提取的 Linux arm64 release 二进制，真实 Kubernetes 使用该镜像。源码、锁文件、脚本、镜像和四个运行中控制器 Pod 的二进制摘要见 [platform-artifact.json](validation/platform-artifact.json)。工作区尚未提交，本轮未更新 GitHub 发布包。

## 实际结果

| 验证 | 结果与证据 |
|---|---|
| Rust 单元测试、格式、Clippy | 7/7，`cargo fmt --check` 和 `cargo clippy --locked --all-targets -- -D warnings` 通过；[单元测试](validation/platform-rust.txt)、[Clippy](validation/platform-clippy.txt) |
| HTTP/TLS、流式代理、插件、WebSocket、SSE | 79/79；[清单](validation/platform-http.json) |
| 产品行为及本轮新增独立模式能力 | 74/74；[清单](validation/platform-features.json)、[原始输出](validation/platform-release-features.txt) |
| NGINX 1.28.0 行为对照 | 46/46；[清单](validation/platform-nginx.json) |
| Kubernetes API 故障与检查点恢复 | 51/51；[清单](validation/platform-recovery.json) |
| OTLP 传输、凭证保护及故障 | 32/32；[清单](validation/platform-otlp.json) |
| 文件日志与外部轮转 | 20/20；[清单](validation/platform-log-rotation.json) |
| 真实 Kubernetes 平台策略 | 38/38；[清单](validation/platform-kubernetes.json)、[原始输出](validation/platform-kubernetes-policies.txt) |
| 真实 Kubernetes 基础生命周期 | 46/46，双副本滚动升级期间 300/300 请求成功；[运行记录](validation/platform-kubernetes-base.txt) |
| Helm | 默认 chart lint，以及租户策略、读写管理令牌挂载的模板渲染通过 |

## 覆盖的关键行为

### P1/P2 修复

- HTTPS 主动检查使用业务路由的私有 CA、SNI 和客户端证书，支持传统 PEM 私钥。上游 mTLS 与匿名请求使用不同连接池；CA 更新不能继续复用原信任策略下的连接。
- gRPC `UNAVAILABLE` 经 trailers 到达客户端，即使 HTTP 状态为 200，指标仍区分 gRPC 0/14，server/client span 均标记错误。
- 外部认证使用 EndpointSlice 就绪端点池，故障端点可切换到正常端点。认证请求在总超时内最多尝试三个端点，业务请求不因此重放。
- 两个请求触发认证前限流时返回 200/429，实际认证服务仅收到一次调用。
- 本地 JSON 日志隐藏 URI、Referer 中的凭证参数，并执行客户端地址关闭策略；OTLP 的 query、body、凭证排除继续通过回归。

### 多租户治理

- 管理员请求体上限覆盖应用的无限制配置；超时上限反映在生效配置中。
- 一个命名空间耗尽请求并发预算时，另一个命名空间仍返回 200；请求结束后许可释放。
- 超额 Ingress 不获接纳；限流键表在租户间隔离，不能用一个命名空间的键耗尽另一个命名空间的表。
- 策略由管理员 ConfigMap 挂载，应用注解不能提高上限。配置变更需滚动重启控制器。

以上是按控制器进程计数的逻辑预算与接纳隔离；没有提供跨副本的精确全局配额，也没有给每个命名空间建立独立的 CPU/内存 cgroup。强资源隔离仍需独立控制器部署、IngressClass 和 Kubernetes 资源限制。

### 声明式灰度与镜像

- 两个 Service 按 90/10 权重调度。
- 镜像收到完整且受限的请求体；超限时跳过镜像，主请求保持完整。慢镜像不会阻塞主响应。
- 候选后端错误率触发回退，状态写入 Ingress；两个副本采用稳定后端，重启后继续保持回退。
- 新 revision 可重新开启发布，p95 延迟阈值也能触发回退。权重和回退策略计入配置指纹。

回退指标来自各副本本地的有限请求窗口；本轮未集成第三方监控平台的业务指标查询。镜像会发送真实的第二份请求，gRPC、WebSocket 和超出请求体上限的请求跳过镜像。Kubernetes API 不可用时本地回退继续生效，但其他副本同步会延迟。

### 变更管理

- 只读令牌能访问诊断与模拟，执行回退返回 403；操作审计记录拒绝和成功发布，未泄漏令牌。
- 模拟器校验真实 JWT 签名和 claim，覆盖限流；使用隔离预算与外部认证 fixture，不访问业务上游。
- 发布归档包含展开后的配置和 RGL 依赖；回退恢复原插件。源文件失效、插件文件删除后，重启仍恢复最后提交版本，并能再次回退。
- 版本保持单调递增，历史目录和文件具有仅属主可访问的权限；归档编译仍保留原 include 文件的诊断位置。

历史不归档静态网站内容和远程 JWKS。Ingress 回退通过更新 Kubernetes 源资源完成；删除 Service、端点、Secret 和 Ingress 的撤销语义继续纳入真实集群测试。

## 环境与复现

Kubernetes 为 OrbStack 单节点 `v1.35.6+orb1`。本轮保留三个专用命名空间用于检查：

- `rgnix-platform-qa-20260925`：双副本平台策略与 Collector。
- `rgnix-platform-qa-20260925-tenant`：独立租户隔离验证。
- `rgnix-platform-base-qa-20260925`：双副本基础生命周期与滚动升级。

```sh
# OrbStack Ubuntu，需已有 Python grpcio/Brotli、OpenSSL、logrotate 和 NGINX 1.28.0
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target cargo clippy --locked --all-targets -- -D warnings'
docker build -t rgnix:0.1.0-platform-qa4 .

# 提取最终镜像二进制后，在 Ubuntu 使用该二进制重复网络行为测试
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && python3 scripts/product_features.py .local/rgnix-platform-qa4'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && NGINX=/usr/sbin/nginx python3 scripts/nginx_parity.py .local/rgnix-platform-qa4'

# 使用新的专用 QA 命名空间；基础测试先安装两副本控制器，再验证平台策略
RGNIX_IMAGE_TAG=0.1.0-platform-qa4 bash scripts/ingress-e2e.sh rgnix-platform-example orbstack
RGNIX_IMAGE_TAG=0.1.0-platform-qa4 python3 scripts/product_kubernetes.py rgnix-platform-example orbstack
```

原始产品测试输出保留了 Python 上游 fixture 在连接关闭时的 `ConnectionResetError`；全部行为断言通过，测试进程退出码为 0。较早的一次并行运行出现 WebSocket echo 断言失败，后续独立运行和最终 release 的 79 项回归均未复现，未据此归因于产品或环境。

本轮未验证 Linux amd64 运行、多节点故障、云 LoadBalancer、长时间压力及恶意租户容量极限，也未重新测量性能基线。先前的短时吞吐数据不能代替本轮多租户、认证与灰度配置的容量结果。

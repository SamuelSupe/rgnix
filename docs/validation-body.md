# 请求体路由验证

> 阶段历史记录：下文保留验证当时的版本、镜像和发布状态。后续 v0.2.0 发布包的实际验收与摘要见[发布验证](validation-release-0.2.0.md)。

日期：2026-09-25，Asia/Singapore。新增完整请求体与前缀检查、JSON Pointer 字符串查询、原始字节搜索及 Ingress 读取策略。使用方式见[请求体路由](request-body.md)。本轮为源码新增能力，未改动已发布的 v0.1.0 下载包。

## 实际结果

| 检查 | 结果与环境 |
|---|---|
| Rust 单元测试、fmt、Clippy | 6/6；OrbStack Ubuntu arm64，Rust 1.90.0；[记录](validation/body-linux-checks.txt) |
| HTTP/TLS/代理/插件 | 79/79；包括镜像中提取的 Rust 1.98.0 Linux arm64 release 二进制；[检查列表](validation/body-http.json) |
| NGINX 1.28.0 对照 | 46/46；OrbStack Ubuntu arm64 release；[记录](validation/body-nginx.json) |
| Kubernetes 模拟 API/恢复 | 45/45；OrbStack Ubuntu arm64；[记录](validation/body-recovery.json) |
| 真实 Kubernetes | 46/46；OrbStack `rgnix-qa-20260923`，两个副本，镜像 `rgnix:0.1.0-body-qa`；[检查列表](validation/body-ingress.json)、[运行输出](validation/body-kubernetes.txt) |
| 滚动更新 | 300/300 Service 请求成功，两个 Ready 副本最终配置摘要一致 |
| 使用示例 | `rgnix check -c examples/body-routing.conf` 通过；文档本地链接、shell 语法和 git diff 空白检查通过 |

镜像由仓库 Dockerfile 构建，使用 Rust 1.98.0 / Debian bookworm arm64。已核对镜像提取二进制与两个运行 Pod 中的 SHA-256 一致。源代码（含 vendor）、Cargo.lock、镜像和二进制身份见 [body-artifact.json](validation/body-artifact.json)。NGINX 对照及模拟 API 使用同源 Rust 1.90.0 构建；没有将两种工具链的二进制混称为同一产物。

## 请求体边界

- 完整 JSON 能按字符串字段选择 HTTPS 后端；逐请求隔离。嵌套数组、JSON Pointer 转义、重复键、无效 JSON、字段类型不匹配由单元/HTTP 检查覆盖。
- 一百万字节以上的二进制上传只用前缀决策，上游长度和 SHA-256 与原始完整请求相同。超过原有 64 KiB 回放缓冲的完整读取和前缀读取均通过。
- 分段发送、chunked、HTTP/2、`100-continue`、连接复用后的下一请求均通过。截断点后的内容不能参与选择；UTF-8 边界被切断时文本 API 返回 nil，字节搜索继续有效。
- 完整读取超限返回 413；预读总超时返回 408；前缀读够后可直接响应而不等待尾部。取消请求不影响服务，上游 POST 断连不产生重试。
- SIGHUP 更新读取模式时，预读中的旧请求沿用旧快照，新请求使用新策略。未开启功能时 Body API 不暴露内容，原流式行为保持。
- Ingress 仅更新注解可切换策略；策略/脚本失败后保留上一有效组合，新副本能恢复其检查点。无效更新不能阻止 EndpointSlice 撤销及 Ingress 删除。

JSON 查询遍历借用的原始子树，不构建 JSON DOM。视图与解析临时预算计入宿主额度；大输入扫描另扣 fuel。前缀不保证已经包含完整 JSON，截断视图的 `req.json_string` 一律返回 nil。

## 验证过程中修正的测试时序

首次 Kubernetes 检查在 `full` 策略尚未被 `prefix` 替换时发送大 Body，旧策略正确返回 413。此环境的 socat 随后发生 Broken pipe，导致整个 `kubectl port-forward` 退出。重新建转发后同一大请求路由正常；测试已改为先用小请求确认前缀边界生效，再发送大请求。

既有 Lease 故障注入步骤也出现过读取到已被上一步滚动更新删除的旧 Pod 名称。旧 Lease 尚未过期时这属于正常过渡状态；测试现在先等待有效、未终止的 rgnix Leader Pod，再注入删除故障。最终完整脚本退出码为 0；输出末尾的 port-forward Terminated 来自脚本清理自身转发进程。

## 复现与边界

```sh
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target cargo clippy --locked --all-targets -- -D warnings'
docker build -t rgnix:0.1.0-body-qa .
RGNIX_IMAGE_TAG=0.1.0-body-qa bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

构建使用一个[有来源记录的 Pingora 0.9.0 小补丁](../vendor/README.md)，只添加调用方可配置上限的请求体回放缓冲接口。未启用请求重试。未进行本轮 amd64 运行验收、多节点/云 LB、长期负载或新的性能基线测试；不能用上述正确性验证推导吞吐上限。

上一轮及 v0.1.0 发布产物的证据保留在[语义修复验证](validation-semantics.md)。

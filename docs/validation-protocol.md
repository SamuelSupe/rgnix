# WebSocket、URI 编码与 Kubernetes 报告修复

日期：2026-09-24，Asia/Singapore。修复上一轮审查已复现的三类问题，并处理扩展 URI 对照测试发现的重复解码。之前的 qa10 验证记录见 [Ingress 隔离修复](validation-isolation.md)。

## 问题与修复

| 问题 | 修复前证据 | 修复后的行为 |
|---|---|---|
| WebSocket 累计流量误用 HTTP 请求体限额 | qa10 在默认 1 MiB 限额下完成 101 握手，回显 983,040 字节后被重置；关闭限额的对照完整回显 2 MiB | 仅在 Pingora 确认协议已经升级后跳过 HTTP 请求体计数；普通 HTTP 及尚未完成升级的请求继续受限 |
| 补斜杠重定向使用解码后的裸路径 | `/a%23b` 被重定向到 `/a#b/`，路径尾部变成 fragment；`%3F` 和空格也损坏目标 | 静态目录和代理前缀重定向都重新编码路径，保留 query 的原始转义 |
| Event 失败后被去重且没有错误指标 | 注入 403，11 秒后仍只有一次尝试，`rgnix_report_errors_total` 仍为 0 | 仅缓存已确认成功的 Event，失败计数且周期重试；同批其他 Event 的失败或超时不会清除已确认成功的记录 |

原始 qa10 证据：[WebSocket 与重定向](validation/protocol-before.json)、[Event 403](validation/protocol-events-before.json)。

扩展 NGINX 对照时还发现静态路径被百分号解码两次，`/a%25b` 错误返回 400。现已把 URI 解码和路径规范化分开，静态文件使用已解码路径；字面 `%2F` 不会再次变成 `/`。静态插件修改后的 query 也用于目录重定向。请求路径检查和 root 目录能力隔离继续生效。

Event 去重按命名空间、名称、Ingress UID、原因及完整诊断内容区分，在每次创建成功后立即保存确认。诊断消失时清除相应记录。状态回写中非 404/409 的 API 失败同样计入报告错误指标，并在后续报告重试。去重是进程内行为，不保证跨副本或重启后的 Event 恰好一次；请求超时而 API 实际已写入时也可能重复。

运行契约更新至 [兼容矩阵](compatibility.md)、[RGL API](rgl.md) 和 [部署文档](deployment.md)。

## 验证

Linux 编译和运行在 OrbStack Ubuntu arm64 执行。下表中的 HTTP、NGINX 和模拟 API 验收使用从最终 `rgnix:0.1.0-qa11` 镜像提取的 release 二进制；真实集群运行同一镜像。调试二进制也通过了相应检查。

| 验证 | 结果 | 证据 |
|---|---|---|
| Rust fmt、clippy `-D warnings`、单元测试 | 通过，5/5 | [检查输出](validation/protocol-linux-checks.txt) |
| HTTP、TLS、代理、插件回归 | 48/48 | [结果](validation/protocol-http.json) |
| NGINX 1.28.0 行为对照 | 29/29 | [结果](validation/protocol-nginx.json) |
| 模拟 Kubernetes API 与真实 TLS 握手 | 25/25 | [结果](validation/protocol-recovery.json) |
| 真实 Kubernetes 生命周期 | 37/37；升级期间 300 次请求全部成功 | [结果及副本状态](validation/protocol-ingress.json)、[完整输出](validation/protocol-kubernetes.txt) |

新增验收覆盖以下实际行为，扩展原有脚本，没有新增测试框架：

- 在 2 MiB HTTP 请求体限额下，WebSocket 连续回显 48 个 64 KiB 二进制帧，共 3 MiB，最后正常交换关闭帧。普通 chunked HTTP 和携带 WebSocket Upgrade 头但尚未升级的请求，在 16 KiB 限额下发送 32 KiB 都得到 413。
- 静态目录及代理前缀补斜杠重定向与 NGINX 比较 path、query、fragment，并实际跟随重定向。覆盖 `#`、`?`、空格、字面 `%`、字面 `%2F`、中文和已转义 query；静态插件改写路径及 query 后也能跳转到正确资源。
- 模拟 API 拒绝 Event，验证错误指标增长、路由继续使用已接受配置、没有资源变更时周期重试、权限恢复后补发及成功后去重。另注入 Ingress status 403，验证错误指标和恢复后的重试。

最终镜像完整运行输出见 [release 验收记录](validation/protocol-release-checks.txt)。Helm lint、模板渲染及 Shell 语法检查通过。Kubernetes 日志中的 `Terminated: 15` 是验收脚本正常关闭自己创建的 port-forward。

真实集群使用已有专用命名空间 `rgnix-qa-20260923`。结束时两个副本均为 `qa11`、2/2 Ready，配置摘要一致，检查点健康值均为 1。测试临时创建的 TLS 冲突资源已删除，原验收环境保留。地址回写仍使用保留示例地址，不代表公网 LoadBalancer 验证。

## 产物与复现

- 镜像：`rgnix:0.1.0-qa11`，本地 `rgnix:0.1.0` 指向同一产物，未推送远端。
- manifest list：`sha256:a62407799d1ea5e1c3bd02d05a0546f61a5c5907e81e425c68610debca193eb5`。
- 二进制 SHA-256：`7389dd6f1c75f8125353620c3dc6240a4e269840c709ed2c6fce8c9c0f0e4193`。
- 源树 SHA-256：`24c954ace9e45b7f945dee41b03a05f3a3fc30961824366a7cfcc5a420db93fc`。

[产物记录](validation/protocol-artifact.json) 同时包含 Cargo.lock 和验收脚本摘要。源树摘要沿用此前算法：按相对路径排序，对 Cargo.toml、Cargo.lock 和 src 下的文件依次输入路径、NUL、文件原始内容。

```sh
# OrbStack Ubuntu 内，从仓库目录执行
export CARGO_TARGET_DIR=/tmp/rgnix-target
cargo fmt --check
cargo clippy --locked --all-targets -j 2 -- -D warnings
cargo test --locked -j 2
python3 scripts/integration.py .local/rgnix-linux-arm64
python3 scripts/nginx_parity.py .local/rgnix-linux-arm64
python3 scripts/ingress_recovery.py .local/rgnix-linux-arm64

# macOS / OrbStack Docker 和专用验收命名空间
docker build -t rgnix:0.1.0-qa11 .
RGNIX_IMAGE_TAG=0.1.0-qa11 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

本轮没有重新测量性能，也没有完成 amd64、多节点、云 LoadBalancer 或长期压力验证。WebSocket 回归验证了流量跨越 HTTP 限额及正常关闭，不代表长期空闲、无限连接数或生产容量认证。

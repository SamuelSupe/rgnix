# 产品功能补齐验证 — 2026-09-25

> 阶段历史记录：下文保留验证当时的版本、镜像和发布状态。后续 v0.2.0 发布包的实际验收与摘要见[发布验证](validation-release-0.2.0.md)。

本轮实现入口策略、认证、调度、诊断和网站能力。记录针对当前工作区源码和本地 `rgnix:0.1.0-product-qa3` 镜像；GitHub 的 v0.1.0 下载包尚未更新。先前请求体、OTLP logs 和文件轮转功能保留，并纳入回归。

## 实际运行结果

| 验证 | 结果与证据 |
|---|---|
| Rust、格式、Clippy | 7/7，fmt 与 `-D warnings` 通过；[集中验证](validation/product-linux-checks.txt)、[最后增量](validation/product-final-checks.txt) |
| HTTP/TLS/流式代理/插件 | 79/79；最终镜像提取的 release 二进制，见 [HTTP 清单](validation/product-http.json) |
| 新增产品行为 | 54/54；同源 debug 和最终 release 均通过，[清单](validation/product-features.json)、[镜像运行输出](validation/product-image-checks.txt) |
| NGINX 1.28.0 对照 | 46/46，Ubuntu arm64 debug；[清单](validation/product-nginx.json) |
| Kubernetes API 故障恢复 | 51/51，包括没有自定义脚本时的策略检查点恢复；[清单](validation/product-recovery.json) |
| OTLP 网络、认证及故障 | 32/32，保留访问日志回归；[清单](validation/product-otlp.json) |
| 本地日志与外部轮转 | 20/20；[清单](validation/product-log-rotation.json) |
| 真实 Kubernetes 新增策略 | 22/22，最终 qa3 镜像；[清单](validation/product-kubernetes.json) |
| 真实 Kubernetes 基础生命周期及滚动升级 | 46/46，双副本滚动升级期间 300/300 请求成功；最终 qa3 镜像，[运行记录](validation/product-kubernetes-base.txt) |
| Helm 与 CLI | 默认 chart lint、trace-only/认证/可信代理参数渲染通过；新配置 check 与离线模拟返回 201，[示例输出](validation/product-examples.txt) |

Ubuntu 构建使用 Rust 1.90.0；镜像为 Rust 1.98.0 / Debian bookworm arm64。两种工具链产物分别记录；没有把源码一致当成二进制相同。镜像、源码、锁文件、两副本运行二进制的身份见 [product-artifact.json](validation/product-artifact.json)。

## 功能与边界检查

- **身份及流量预算：** XFF/Forwarded/原始对端、非可信头、IPv6、CIDR 白名单、按租户速率及并发、后端请求预算、许可释放。PROXY v1/v2 以及 PROXY 头先于 TLS 握手均使用真实 socket 验证。
- **上游：** 最少活动请求、Cookie 哈希、自动 Cookie 黏性、主动健康摘除；Rust 测试使用真实 UDP DNS 检查 TTL 更新和旧请求的端点持有。私有 CA、验证名、跨 CA 连接池隔离及热更新后的旧连接拒绝均通过实际 HTTPS 验证。
- **gRPC：** TLS 客户端→代理→h2c 上游；客户端尚未结束发送时已收到首个响应，继续交换消息并保留 trailers。显式配置 gzip 的 gRPC MIME 也不会破坏协议。
- **认证：** JWT 签名、audience、过期与篡改；真实 HTTPS JWKS 在不重载配置的情况下换钥，旧 key 随之失效；外部认证拒绝、故障及伪造身份头替换；mTLS CA 变更后拒绝已有 TLS 连接上的后续请求。
- **网站及 RGL：** alias、SPA fallback、Range、gzip/Brotli 完整解码、质量权重与 q=0、no-transform；JSON 整数/布尔、query 解码、Cookie、稳定 hash、已验证 claim；实际编译执行和 CLI 模拟。
- **诊断：** Bearer token 保护、生效配置/路由解释、独立配置回退及版本单调增加；Ingress 回退要求修改源资源，不能恢复已撤销的端点或 Secret。
- **可观测性：** server/client span 父子关系、上游 traceparent、OTLP/本地日志关联、父级采样、路由前 400 日志、trace 不含 query/凭证。Kubernetes 的 trace-only Helm 配置由真实 OpenTelemetry Collector 0.123.0 解码，[Collector 输出](validation/product-collector-decoded.txt)。

真实 Kubernetes 在 `orbstack` 的专用命名空间 `rgnix-product-qa-20260925` 运行。新增场景覆盖 2 MiB POST、路由超时、限流、压缩、可信代理、管理认证、HTTPS CA/JWT Secret 撤销恢复、外部认证 Service 删除、mTLS、策略错误保留、两个新副本从无插件检查点恢复，以及错误策略下的端点/Ingress 删除。测试资源保留用于检查；没有修改其他命名空间的工作负载。

本轮实际发现并修复了 Brotli 未写完整终止标记、压缩忽略质量值、固定 IP 上游被 DNS 维护改变顺序、上游连接池未按 CA/协议区分，以及 CLI 参数组冲突。对应行为纳入本轮验收。

## 短时性能基线

最终镜像 release 二进制，NGINX 上游返回 16 KiB，Python HTTP/1.1 keepalive 客户端 8 并发，每种情形预热 1 秒，再运行 3 轮、每轮 3 秒。代理访问日志关闭，认证/限流/压缩/traces 默认关闭。原始 CPU、RSS、延迟和请求数见 [benchmark JSON](validation/product-benchmark.json)。

| 插件 | RPS 中位数 | p95 中位数 |
|---|---:|---:|
| 无插件 | 15,581 | 1.136 ms |
| pass 插件 | 16,453 | 1.052 ms |
| 分支及请求头修改 | 16,561 | 1.008 ms |

九轮请求均无错误。三类负载的代理 CPU 分别约 0.55–0.57、0.68–0.69、0.83–0.85 核，RSS 约 25.5–27.2 MiB。环境是共享 OrbStack 主机，Python 客户端和其他 QA 活动会影响吞吐；不能据此认定插件提高性能，也不是产品最大容量或长期稳定性认证。

## 复现与尚未覆盖项

```sh
# Ubuntu 还需要 logrotate、python3-grpcio、python3-brotli
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target cargo clippy --locked --all-targets -- -D warnings'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && NGINX=/usr/sbin/nginx python3 scripts/nginx_parity.py /tmp/rgnix-target/debug/rgnix'
docker build -t rgnix:0.1.0-product-qa3 .
RGNIX_IMAGE_TAG=0.1.0-product-qa3 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
RGNIX_IMAGE_TAG=0.1.0-product-qa3 python3 scripts/product_kubernetes.py rgnix-qa-example orbstack
```

这些测试覆盖 Linux arm64 和单节点 OrbStack。amd64 运行、多节点故障、云 LoadBalancer/真实外部 PROXY 发送器、长期负载和容量认证尚未完成。远程 JWKS 的换钥已实测，连续五分钟不可用后的过期策略未做完整时长故障试验。

完整参数和约定限制见[产品功能指南](product-features.md)。限流按进程计数；主动检查为 HTTP GET；trace 导出为 OTLP/HTTP protobuf。Gateway API、HTTP/3、响应缓存、完整 Lua/NGINX、正则/嵌套 location 及分布式限流保持原先的后续范围。

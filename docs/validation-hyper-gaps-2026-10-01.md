# Hyper 协议与运行管理补齐验证（2026-10-01）

本轮在 `experiment/replace-pingora` 的已有工作区上继续实现。默认构建和默认运行仍使用 Pingora；实验构建需要显式启用。当前工作区未提交、未发布，此记录不代表新版本交付，也不宣称接近 NGINX 的吞吐或生产容量。

## 本轮实现

- 限定配置后端的 H1/H2 TCP CONNECT，覆盖认证、RGL、并发预算与隧道空闲超时；不允许任意目的地址。
- H2 Extended CONNECT WebSocket，H1/H2 双向协议转换及独立 H1 自动协商池。
- 明文 h2c Upgrade，初始请求只执行一次，body 最多 1 MiB，并受路由检查和预算约束。
- 可选 Quinn/h3 HTTP/3，共享路由、RGL、认证、流式 body、日志、span 与快照；Helm HTTPS 同端口增加 UDP。无效 QUIC Initial 的认证失败只关闭该次连接，不能让监听服务或进程退出；真实损坏报文在修复前触发进程退出，修复后保持服务可用。
- 单域名且无 mTLS 的 TLS 1.2/1.3 会话恢复，发布时更换 TLS 上下文及票据密钥；多 SNI/mTLS 同时关闭缓存与 TLS 1.3 票据。仅设置 `NO_TICKET` 仍会发送 TLS 1.3 有状态票据，详见 [OpenSSL 选项语义](https://docs.openssl.org/3.5/man3/SSL_CTX_set_options/)。
- 独立下游首部、body 与响应发送期限；活跃 H2 stream 不受连接空闲期限误杀。
- Linux 明文 H1 静态文件 sendfile，保留安全文件打开、Range/HEAD、流水线顺序与文件截断失败行为。
- 独立 Tokio 进程宿主及两阶段退出。各服务排空后才释放运行时，避免先退出的 worker 中断其他 worker 使用的共享 H2 上游连接。
- 清理 TLS 操作前的线程局部 OpenSSL 错误队列；同线程其他加密操作留下的错误不能使正常连接误报 TLS 故障。回归测试在修复前失败、修复后通过。其依据是 [OpenSSL 3 的接口约束](https://docs.openssl.org/3.5/man3/SSL_get_error/)。

完整使用方式、支持边界与默认值见 [Hyper 内核文档](hyper-experimental.md)和[配置矩阵](compatibility.md)。

## 环境与实际检查

Linux 运行验证使用 OrbStack Ubuntu 25.10 ARM64、Rust 1.90；Kubernetes 使用已有隔离 Kind 1.34 集群的三个逻辑节点。这些节点共享同一物理主机，不能算跨物理节点验收。测试使用包含 `http3,jemalloc` 的 debug 二进制；镜像为 Ubuntu 25.10 本地测试镜像，未推送仓库。

| 检查 | 实际结果与边界 |
|---|---|
| Rust 单元测试 | 25 通过，1 个已有忽略项；包含 OpenSSL 错误队列回归 |
| 格式与 Clippy | 默认与最终 `http3,jemalloc` 构建通过，最终源文件复核通过 |
| 默认数据面完整脚本 | `scripts/check.sh` 已通过 |
| Hyper 主流程 | 冻结 TCP 构建 57 项通过，覆盖不重放 POST、流式请求、取消、预算、发布和退出 |
| 完整产品流程 | 冻结 TCP 构建 115 项通过，覆盖认证、配额、灰度、审计、body 路由、TLS、发布和回退 |
| 扩展协议 | 最终构建 39 项通过：CONNECT、各方向 WebSocket、h2c、期限、sendfile、TLS 恢复与禁用、H3 body/RGL/mTLS、无效 Initial 隔离 |
| NGINX 行为对照 | Hyper 与默认 Pingora 各 49 项通过；属于行为对照，未测吞吐 |
| 厂商库 | Hyper 120 通过、6 忽略；HyperUtil 连接池 26 通过、2 忽略 |
| Ingress | 冻结构建 46 项通过，包括 Secret/端点/插件撤销、Lease、双副本与滚动更新 |
| Kubernetes HTTP/3 | 最终构建 5 项通过：UDP ClusterIP 路由、损坏 Initial 后进程/服务保持可用、Secret 轮换、删除拒绝新握手、恢复；客户端按证书指纹检查本地自签证书 |
| Gateway | 冻结构建 115 项通过，22 秒双向 gRPC 在 Pod 终止时完整结束；100 条路由下 60 秒负载 10,078 请求、零失败，并验证准入证书轮换/无效更新保留。此前 300 秒负载另有 50,466 请求、零失败；都属于短期稳定性检查，未作容量结论 |
| mTLS CA 轮换 | 480 次并发连接撤销、60 次隔离客户端诊断均通过；冻结 TCP 构建的完整前置流程重复 12 次，全部使用原可信连接得到 403 |

`hyper_integration.py --pingora` 在自定义 Date 响应头断言处失败；同一问题在修改前保留的 release 二进制上也复现，属于已有的两种数据面差异。上面的默认完整脚本和 NGINX 49 项对照没有这个断言，不能用其通过覆盖该差异。

## mTLS 轮换 reset 的原因

诊断中，匿名 TLS 请求在写入时收到真实的连接重置，CPython 3.13.7 将 OpenSSL 的 system error 转为 Python 异常后提前返回，没有清除线程错误队列。该行为可在 [CPython 的 `PySSL_SetError`](https://github.com/python/cpython/blob/v3.13.7/Modules/_ssl.c#L624) 中看到。同线程的可信连接随后读取时，系统调用返回的是 `EAGAIN`；SSL 错误分类却沿用上一连接留下的 `0x80000068`，立即抛出 reset 并由客户端关闭连接。服务器此后的 EOF/写失败是这个关闭的结果。

保持两次请求在同线程的诊断得到 60 次中 10 次 reset；将匿名失败请求移至独立线程后，60 次全部得到预期的 403，主线程错误队列为空。验收脚本只隔离匿名失败请求，可信连接仍保持原 socket，CA 轮换、拒绝状态和响应读取断言保留；没有重试请求或屏蔽服务器 TLS 错误。这与 Rust 数据面中另一个已经用回归测试确认的错误队列缺陷分开处理。

## 本地证据

原始日志及 JSON 位于 `.local/hyper-gaps-20261001/`，最终复核包括 `clippy-final.log`、`unit-complete.log`、`hyper-complete.log`、`product-complete.log`、`protocols-guard.log`、`ca-complete.log`、`ingress-complete.log`、`h3-kubernetes-guard.json` 和 `gateway-complete.json`。此前默认数据面、NGINX、厂商库、300 秒 Gateway 负载记录亦保留。实际 sendfile 系统调用见 `sendfile.strace`。调试失败、损坏 Initial 修复前和缺少测试依赖的日志均保留，没有被通过记录覆盖。

Rust/Cargo 源文件清单 `source-manifest-final.json` 共 275 文件，清单 SHA-256：`1776f77c573d7dcdf753b96c6325f98ab109c5ea2d07ee7c7e1d731653ae61fa`。最终复核确认这些文件没有再次变化。

- 产品/Hyper/Ingress/Gateway 冻结二进制 SHA-256：`bb75544649b718dc756b54eda8da8e748bfe34f44767e45bfcac71f7a471fe90`；本地镜像 `rgnix:hyper-gaps-complete-20261001`，镜像 ID `sha256:ae81856f93527a099de93ce39c69ccd04cf4e1a8548527a4510d22dfb739d4e7`。
- 随后仅补充 H3 无效 Initial 错误隔离及其测试，最终二进制 SHA-256：`9c1a1a77c1ab1b266d1a68dba000e1b51856952bd46720684fb0eb4493fdf282`；本地镜像 `rgnix:hyper-gaps-guard-20261001`，镜像 ID `sha256:eeb661677e77f5593ebe23bf5e13fe616bf34cc75f517719b6599c3bcaf94aa3`。39 项协议回归与 Kubernetes H3 5 项均使用此版本；Ingress 检查环境已升级到此镜像并开启 H3。
- 早先 300 秒 Gateway 检查使用 `rgnix:hyper-gaps-drain-20261001`；不将不同构建的检查合并宣称为同一镜像的全量资格。上述镜像仅在本地 Kind 载入，未公开发布。

## 尚未取得的资格

- 上游 Gateway API 全量 conformance。当前控制器绑定一个选定 Gateway，不自动创建基础设施，不能用本项目的 Gateway 回归代替官方认证。
- 原生 amd64 运行与跨物理节点验证。CI 已加入 ARM64/amd64 协议矩阵，但本轮未触发远端 CI，没有可用主机结果。
- 48 小时稳定性、生产容量、浏览器 HTTP/3 互操作及新版本 NGINX 性能校准。
- 多 SNI/mTLS 的 TCP TLS 会话恢复；H3 CONNECT/WebTransport、0-RTT、H3 上游和会话恢复；逐 H2 stream 的未完成首部块总期限。
- 完全去除 Pingora 依赖：独立运行管理已经实现，但后台服务、管理协议和错误类型仍复用其接口。

以上边界继续保留，不能将本次功能接入描述为完整生产内核或性能对等。

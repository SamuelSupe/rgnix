# Hyper 主内核加固验证（2026-10-01）

本轮在 `experiment/replace-pingora` 的已有工作区继续实现资源耗尽恢复、分层连接准入、原生服务心跳、热更新复用和发行门槛。默认运行仍为 Pingora，Hyper 必须显式启用。当前源码未提交、未发布；本记录不代表生产容量、NGINX 性能对等或远端 CI 已通过。

## 实现与语义

- TCP accept 的临时错误不会结束监听服务：25 ms 起步，最多 1 s 的退避，资源恢复后重试；不可恢复错误保留宿主退出处理。实际故障注入覆盖 `EMFILE`，未注入系统级 `ENFILE`、内存或 socket 缓冲耗尽。
- TCP/TLS/QUIC 共用进程连接预算，再按对端 IP、监听地址及待建连接分层限制。TCP 待建许可在第一份完整请求首部到达处理器时释放，QUIC 在 TLS 握手后释放；持久连接和隧道继续占连接许可。取消/异常自动归还许可；来源计数仅保留存活连接，不为拒绝的 IP 建立永久条目。
- 每个原生服务 reactor 每 250 ms 更新心跳。超出配置期限时健康和就绪返回 503，恢复调度后自动恢复；Prometheus 暴露服务心跳年龄、健康状态、待建连接和各类拒绝/accept 错误。管理端口仍可运行不能掩盖业务 reactor 停顿。
- 热更新仅复用配置等价的上游池，包括后端/端点、监听器、超时、协议、SNI、CA 和客户端身份。请求/响应头及插件更新仍按新快照生效，旧流使用旧快照。TLS 按监听器的证书、私钥、SNI 归属及客户端认证策略判定安全代次；路由更新保留会话，凭据变化撤销旧票据。
- Docker 源码构建与未来原生发行包启用 `http3,jemalloc`。发行门槛执行同一构建的两种运行模式、发行二进制的 Hyper/HTTP3 回归，并让同一镜像完成两种 Gateway 验证。新增门槛已配置，尚未在远端执行；已有 v0.5.0 产物没有改变。

CLI/Helm 自动限额、共享代理/NAT 的调整方式及 TLS 边界见 [Hyper 内核文档](hyper-experimental.md)，指标契约见 [指标文档](metrics.md)。连接限制使用实际 socket 对端，不读取转发头或 PROXY 声明；QUIC 迁移后仍按初始来源计数。这是进程内连接治理，不是租户进程/CPU/内存隔离。

## 实际验证

Linux 运行验证使用 OrbStack Ubuntu 25.10 ARM64、Rust 1.90；Kubernetes 使用 OrbStack Docker 中已有 Kind 集群的三个逻辑节点，仍共享同一物理主机。运行构建包含 `http3,jemalloc`，采用 debug 配置；未测试 release 性能。

| 阶段 | 结果与范围 |
|---|---|
| 最终 Rust 单元检查 | 25 通过、1 个已有忽略项 |
| 格式、Clippy | 根项目格式通过；默认及 `http3,jemalloc` 的严格 Clippy 均通过 |
| 完整产品脚本 | 同一功能构建分别以 Hyper、Pingora 运行 `scripts/check.sh`，均通过；包含 115 项产品检查、Ingress 恢复、OTLP、日志轮转、迁移和共享限流 |
| 最终 Hyper 主流程 | 双 worker 68 项通过；包括旧请求完成、新请求采用新版本及未变上游连接复用 |
| 分层准入/故障恢复 | 主流程中 10 项通过；两来源及两监听器隔离、待建许可归还、单 reactor 停顿/恢复、描述符耗尽/恢复 |
| 最终扩展协议 | 71 项通过；包括路由更新保留 TCP/QUIC 会话、真实证书轮换撤销票据及现有 mTLS/CA 撤销边界 |
| 最终 NGINX 行为对照 | NGINX 1.28.0，49 项通过；属于语义对照，未测吞吐 |
| 厂商协议/连接池检查 | Hyper 120 通过、6 忽略；HyperUtil 26 通过、2 忽略 |
| Ingress | 最终构建 46 项通过，包括端点/插件/证书撤销、Lease、300 次 Service 请求的滚动更新和双副本配置收敛 |
| Gateway | 功能构建 115 项通过，包括 TLS Secret/后端 CA、策略、镜像、灰度、准入证书、双副本和终止中的双向 gRPC 排空 |
| Gateway 混合负载 | 100 条路由、120 秒，20,184 请求、0 失败；期间更新插件和替换 Pod，包含 HTTP/TLS/RGL/body/logs/traces；这不是容量测试 |
| Helm/发行配置 | Helm lint、Hyper Gateway 渲染及 CI/Release YAML 解析通过；远端作业与 amd64 构建未运行 |

文件描述符注入只对测试子进程设置 `RLIMIT_NOFILE=64`，不修改 VM 限额。线程停顿只暂停一个业务 reactor，保持管理和监督线程运行；探针失败、健康指标变为 0，恢复线程后重新就绪。管理端口自身仍需要文件描述符，系统资源耗尽时不保证它可连接。

双 worker 验收中，原脚本在一个流仍持有请求许可时连续发出其他断言，客户端读完前一响应不能保证另一 reactor 已结算该许可。脚本现在在这些阶段等待许可回到预期数量；没有重放请求、提前释放生产许可或提高测试预算。连接复用检查复用同一条下游连接，避免把 SO_REUSEPORT 分配到另一 worker 的独立上游池误判为重建。

## 构建与证据

完整产品/Gateway 功能构建 SHA-256：`b9ca0fa0ec766118898d2d0c275c4c4e553e37d4e79a06e3a1bc868651efa3f0`。随后 Clippy 移除 `prepared.rs` 中一个多余的返回变量，未改变行为；最终构建 SHA-256：`ad5dd72ff6358441ed396f6a4971a8ea933c285a990758333687b0ddc254e07f`。双 worker 主流程、扩展协议、NGINX 对照和 Ingress 使用最终构建。两组 Pod 内二进制摘要分别核对通过，未将它们写成同一产物的验收。

两个本地镜像都使用匹配构建环境的 Ubuntu 25.10：

- `rgnix:hyper-primary-20261001`：`sha256:2922c26f1c548c60e3ba77c1627080e95ca9bfcedf651b48763a42a5f16dccc1`，用于 Gateway。
- `rgnix:hyper-primary-checked-20261001`：`sha256:ceeff0ca7477d2fd7de7ef4cda19aca331a0a14f46d63074bf892aa14f14ef67`，用于最终 Ingress。

Ubuntu 构建直接放入 `Dockerfile.release` 的 Debian bookworm 镜像时启动失败，缺少 GLIBC 2.38/2.39；已保留失败日志，不能称该发行镜像通过。本轮本地镜像匹配 Ubuntu ABI；远端发行流程在 bookworm 原生构建，再组装 bookworm 镜像，其最终运行仍待 CI 验证。

最终 Rust/Cargo 文件清单共 434 文件，清单 SHA-256：`8f8c54400124cbf4c0f9aac5c82e4ba0f982e69c35358f06007ca3a84b0590ba`，结束前核对未变。[机器可读结果](validation-hyper-primary-2026-10-01.json)记录摘要和运行范围。原始日志、前置工作区清单、任务差异及产物保存在 `.local/hyper-primary-hardening-20261001/`；只统计本轮差异，没有清理此前未提交内容。

集群保留专用命名空间 `rgnix-hyper-primary-20261001`、其 `-peer` 和 `rgnix-primary-ingress-20261001` 供检查，未操作其他项目工作负载。

## 尚未验证或完成

原生 amd64、远端发行作业、release 产物镜像、跨物理主机、长期 soak、压力下的内存/CPU 资格及新的 NGINX 性能校准均未完成。多 SNI/mTLS 的 TCP TLS 恢复仍关闭；OCSP stapling、细粒度 TLS 参数和可配置 HTTP/2/3 流控等后续能力未在本轮加入。Hyper 尚未切换为默认内核。

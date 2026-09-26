# Gateway 产品问题修复与完整主流程验证 — 2026-09-26

最终源码镜像完成 Gateway 全流程 115 项、真实 Kubernetes Ingress 71 项、额外准入范围 2 项，以及 Linux 原生行为 327 项检查，全部通过。Rust 14 项测试、Clippy、Release 构建、Helm lint 和变更格式检查通过。没有用分阶段的局部结果拼成最终 Gateway 通过结论：115 项来自同一最终镜像的一次完整运行。

## 修复与实际证据

| 问题 | 修复 | 运行证据 |
|---|---|---|
| 准入服务失联会影响其他控制器的资源 | CEL 在调用 webhook 前筛选 IngressClass、Gateway 和 parentRefs，同时检查新旧对象；资源首次加入管理范围时也检查受保护的发布状态 | 关闭测试准入 Service 的端点后，选中资源拒绝写入，其他 Gateway、HTTPRoute、GRPCRoute 和 IngressClass 的 server-side dry-run 成功 |
| 准入证书更换需要重启 | 每 2 秒检查投影文件，验证证书与私钥后原子替换；无效更新保留有效证书，提供到期与失败指标 | 两个副本 UID 不变，均切换至新证书；移除旧 CA 后准入成功；不匹配的证书/私钥被拒绝，两个副本错误计数均为 1 |
| 分阶段灰度推进只看主副本流量 | 按资源、策略和阶段汇总各副本的请求、错误及延迟阈值样本；缺少或过期的观测阻止推进 | 候选请求全部发给非主副本，主副本仍正确完成审批后的阶段推进；持久化阶段和业务指标回退通过 Gateway/Ingress 重启验证 |
| 租户编译阻塞其他控制面更新 | 单独的编译工作线程、全局 128 项队列、每命名空间 2 项上限和管理员编译速率预算；缓存命中不占额度，不持有缓存锁执行 JIT | Rust 回归验证超额租户不阻止缓存命中和其他租户；Gateway/Ingress 验证坏插件期间端点撤销、资源删除仍生效 |
| 长连接停机行为不完整 | 可配置停机预算、preStop 排空标记、撤销 readiness、HTTP/1.1 关闭提示；Pingora 在预算内等待活跃异步请求完成后关闭运行时 | 已建立 WebSocket 在排空期间继续通信；22 秒 gRPC 双向流跨 Pod 终止完成；180 秒持久连接混合负载零错误 |

异步编译接入回归还发现两个问题：正常排队被当作 `InvalidPlugin`，以及新副本可能在持久化插件恢复前就报告就绪。现已区分等待状态，编译完成后主动唤醒控制器；新副本等待已有插件恢复，运行中的副本继续处理其他更新。原生恢复套件通过 57 项检查，Gateway 在副本就绪后立即验证坏源码对应的历史插件可以服务。

编译限制是进程内的调度和额度。独立编译进程、操作系统级 CPU/内存硬隔离没有在本轮实现。

## 测试对象与环境

- 基于 `6839556234dbb11b16d7299b6e7b0eafa446c53f` 的当前工作树，包含未发布改动。
- 编译环境：OrbStack Ubuntu 25.10、Linux arm64、Rust 1.90.0；运行测试镜像基于 Ubuntu 24.04.5。
- 二进制 SHA-256：`7408ee81974cf1131a43eb4dcf93e49869146c57fed682319fbfc9a49b9869a7`。
- 测试镜像：`rgnix:gateway-acceptance-20260926`；本地镜像 ID：`sha256:429bc782d4f5aeee5bb7822547f2baab61bc7c395e75b31a31c87ac24fc72961`。
- 专用 kind 集群：`rgnix-product-20260926`，Kubernetes v1.34.0，一控制节点和两工作节点，Gateway API v1.6.1 CRD；Helm v4.2.0。
- Gateway 两个控制器分别位于两个工作节点；最终检查均就绪，容器重启计数为 0。
- Gateway namespace：`rgnix-gateway-acceptance-20260926` 及其 `-peer`；Ingress namespace：`rgnix-ingress-complete-20260926`。测试资源保留以供复查。

制品标识及最终副本状态见[原始记录](validation/gateway-hardening-artifact-2026-09-26.json)。

## 完整运行结果

| 验证 | 结果 | 原始记录 |
|---|---:|---|
| Rust 单元测试 | 14 通过 | 最终 Release 构建前执行 `cargo test --locked -j 2` |
| HTTP、TLS、HTTP/2、WebSocket/SSE、body、取消、断连、无重放和排空 | 85 通过 | [原生行为](validation/gateway-hardening-native-2026-09-26.json) |
| Ingress 检查点、启动恢复、状态、事件、API 停顿和预检 | 57 通过 | 同上 |
| OTLP logs/traces | 32 通过 | 同上 |
| 本地日志与轮转 | 20 通过 | 同上 |
| 访问策略、压缩、JWT/JWKS、管理与发布能力 | 111 通过 | 同上 |
| 迁移工具 | 10 通过 | 同上 |
| Redis 共享限流 | 12 通过 | 同上 |
| Gateway 完整主流程 | 115 通过 | [Gateway](validation/gateway-hardening-gateway-2026-09-26.json) |
| 真实 Kubernetes Ingress | 71 通过 | [Ingress](validation/gateway-hardening-ingress-2026-09-26.json) |
| Ingress 准入故障作用范围 | 2 通过 | [准入范围](validation/gateway-hardening-admission-scope-2026-09-26.json) |

Gateway 主流程包括域名和路径匹配、请求条件、改写/重定向、Service 权重、ReferenceGrant、标准镜像、总超时、插件持久化与坏配置恢复、认证依赖撤销、灰度审批/回退、SNI/后端 TLS、CA 撤销、gRPC/h2c、共享租户限额、副本配置确认、规模配置、滚动更新、准入与证书轮换。

混合负载在基础路由之外增加 200 条 HTTPRoute，持续 180 秒。六个 worker 复用 HTTP/1.1 连接，收到 `Connection: close` 后为下一请求建立连接；不重试失败请求。涵盖 HTTP、TLS、RGL 请求/响应钩子、POST 前缀判断后完整 body 转发，同时开启 OTLP logs/traces。运行中更新插件并替换一个控制器 Pod。

结果为 **30,363 次请求，0 次失败，P99 45.67 ms**，保留全部 30,363 个延迟样本。每 worker 上限 40 请求/秒，源站和负载器均使用 Python；该结果用于连续性回归，容量和极限吞吐需单独评估。

## 回归中遇到并处理的失败

- [修复前的 gRPC 运行](validation/gateway-hardening-grpc-before-fix-2026-09-26.json)在 Pod 终止时出现 `Socket closed`。原因是 Pingora 关闭 Tokio 运行时会直接取消异步请求，原有 runtime timeout 不能保护这些流。增加预算内活跃请求等待后，最终镜像完整运行通过。
- 原生恢复用例捕获了编译等待误报、编译完成后恢复不及时、以及持久化 body 策略尚未恢复就报告就绪的问题，均在最终源码中修复并重跑通过。
- Gateway 用例增加就绪后立即请求断言时，暴露了测试自身尚未等到 `kubectl port-forward` 监听的问题。现在只等待转发端口建立，再立即验证应用响应；最终完整运行包含该断言。
- 首轮虚拟环境缺少 Brotli，产品能力套件停在依赖导入。改用已有 grpcio/Brotli 的系统 Python，111 项全部通过；原始结果保留该次环境原因。
- 共享 OrbStack Kubernetes 集群的 Service IP 池已满；真实 Ingress 验证改在本任务专用 kind 集群执行，没有清理其他项目资源。Collector 镜像用 arm64 归档导入专用节点。

## 重复执行与边界

使用当前源码构建的镜像及已有 gRPC fixture 镜像：

```sh
python3 scripts/gateway_e2e.py --context kind-rgnix-product-20260926 \
  --namespace rgnix-gateway-acceptance-20260926 \
  --image rgnix:gateway-acceptance-20260926 \
  --require-multiple-nodes --soak-seconds 180 --scale-routes 200 \
  --output gateway-results.json
```

全套原生行为入口为 `scripts/check.sh`；本轮七个行为脚本使用同一最终 Release 二进制执行。部署当前源码镜像时需要设置 `shutdown.enabled: true` 才会启用 Chart 的排空标记和停机参数；默认值继续兼容已发布的 v0.3.0 镜像。见[部署说明](deployment.md)、[治理](governance.md)和[编译额度](platform-policies.zh-CN.md)。

本轮没有执行 24 小时浸泡、物理节点断电/网络分区、完整上游 Gateway conformance、amd64 运行或 Debian 发行镜像验证。三个 kind 节点共享 OrbStack 宿主机。cert-manager CA 注入模板完成静态渲染检查，实际证书轮换使用手动重叠 CA。无限期或超过停机预算的连接仍需要业务重连策略。

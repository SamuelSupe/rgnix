# 生产验收与发布门禁

功能存在、行为测试通过和生产容量经过认证是不同的结论。v0.5.0 的范围与升级要求见[发行说明](releases/v0.5.0.md)；历史版本记录保持其原有范围。v0.5.0 是功能预览发布，未通过稳定性能/容量认证；A/A 波动及尾延迟限制见[性能指南](performance.md)。源码验证见[Gateway 完整主流程与修复验证](validation-gateway-hardening-2026-09-26.md)，发行产物的原生双架构与 Kubernetes 门禁见 GitHub Release workflow；此前的[产品验证记录](validation-product-2026-09-26.md)保留为历史证据。

[2026-10-25 OrbStack 实测](validation-kubernetes-soak-capacity-2026-10-25.md)补齐当前工作区源码的三节点两副本、1 小时混合负载及三轮容量矩阵：995 项运行检查通过，长测 613,597 次请求零失败、全程 P99 上界 45.5ms；24 个正式容量窗口的 54,706,014 次完成请求零 wrk 错误、全部满足暂定 SLO。仅 1/8 档位通过重复性门槛，高负载还有本地访问日志丢弃，因此仍未认证稳定生产容量。报告保留初始夹具失败、默认握手保护触发及全部正式窗口。

## 可重复的行为门禁

后端验证使用 Linux，OrbStack 可用于开发机验证：

```sh
bash scripts/check.sh
cargo clippy --locked --all-targets -j 2 -- -D warnings
cargo build --release --locked -j 2
```

`check.sh` 集中覆盖 HTTP/HTTPS、HTTP/2、WebSocket/SSE、流式 body、客户端取消、断连与无重放、配置恢复、日志/OTLP、访问策略、迁移和本地限流。其结果不能替代真实 Kubernetes watch、RBAC、准入及多副本验证。

响应交付总期限另有真实 socket 回归，覆盖 TCP/TLS 最终 flush 阻塞、QUIC 发送容量不足、request/backendRequest 的较早期限及显式关闭；同时检查 HTTP/1/2/3 完整 504 与兄弟流隔离、错误响应交付预算、HTTP/3 提示首部和 CONNECT/WebSocket 发送取消，要求恢复流控后不能继续发送过期隧道数据。夹具通过公开 API 给文件路由附加 Gateway 策略，运行正式数据面，不替代 Kubernetes watch 验收：

```sh
cargo build --locked --features http3,jemalloc --example gateway_deadlines -j 2
python3 -m venv .local/protocol-venv
.local/protocol-venv/bin/pip install -r scripts/requirements-protocols.txt
.local/protocol-venv/bin/python scripts/deadline_regressions.py target/debug/examples/gateway_deadlines --http3
```

固定 Gateway API v1.6.1 标准 CRD，并构建当前源码镜像与 `scripts/fixtures/gateway-grpc.Dockerfile` 后运行：

```sh
python3 scripts/gateway_e2e.py --context YOUR_TEST_CONTEXT \
  --namespace rgnix-production-qa --image YOUR_TEST_IMAGE:TAG \
  --soak-seconds 600 --scale-routes 200 --output gateway-results.json
```

只使用测试集群。脚本只复用其标签标识的两个测试 namespace，结束后保留以便检查。测试包含故意无效的路由与授权撤销；不要在业务 namespace 中运行。启用准入阶段会创建该 Helm release 的集群级 webhook 配置，清理时先卸载对应 release，再删除测试 namespace 和专用 GatewayClass，避免留下指向已删除 Service 的 webhook。

混合负载包含 HTTP GET、TLS GET、RGL 请求/响应钩子、开启 POST body 前缀模式后的完整转发，同时开启 OTLP logs/traces。连续性门禁复用 HTTP/1.1 连接，在收到 Connection: close 后为下一请求新建连接，不重试失败请求；每 worker 最多 40 请求/秒，六个 worker 总上限 240 请求/秒。测试显式开启 shutdown.enabled。负载过程中更新插件并替换一个控制器 Pod，之后要求副本重新收敛；另有持有旧请求并更新插件的快照连续性检查，以及 Pod 终止后仍需完成的 gRPC HTTP/2 流。输出请求数、错误样本、最近每 worker 3000 个成功请求的 P99，以及覆盖全程成功请求的 `full_run_latency`。全程直方图使用 100 微秒宽度的桶，报告 P50/P95/P99 上界、精确最大值及超过 3 秒的溢出数；分位数落入溢出桶时为 null。失败请求单独计数，不进入成功延迟分位数。源站和负载器均为 Python，结果用于连续性回归，不作为网关极限吞吐承诺。超过停机预算的长期空闲连接和无限期流仍会关闭；客户端需遵守其业务重连与幂等约定。

`--soak-seconds` 支持 0..86400，0 跳过混合负载；`--scale-routes` 支持 1..2000。规模测试是在基础路由以外增加指定数量的 HTTPRoute，不能据此外推最大容量。租户速率预算在负载阶段显式设为每秒 10000；限流与隔离使用独立检查验证，429 也会使连续性检查失败。

命名空间速率回归按[当前进程级契约](shared-rate-limits.md)逐副本验证：同一命名空间的两个不同路由合计耗尽该副本的 burst，另一个副本仍有自己的完整 burst，其他命名空间不受影响。它不要求已移除的跨副本共享令牌桶。

### Kubernetes 容量与尾延迟

在专用三节点测试集群中，可复用同一产品镜像运行原生 wrk 容量矩阵。先用 `scripts/fixtures/gateway-capacity.Dockerfile` 构建并载入夹具镜像，然后执行：

```sh
python3 scripts/gateway_capacity.py --context YOUR_TEST_CONTEXT \
  --namespace rgnix-capacity-qa --image YOUR_TEST_IMAGE:TAG \
  --fixture-image YOUR_CAPACITY_FIXTURE:TAG \
  --seconds 30 --warmup 5 --rounds 3 --concurrency 16 64 256 1024 \
  --output capacity-results.json
```

夹具部署两个分布于不同工作节点的 Hyper 副本，每副本 2 个 worker、2 CPU/512MiB 上限，201 条路由；NGINX 源站和 wrk 客户端位于控制节点，各限 4 CPU/1GiB。HTTP/1 keepalive 请求经 ClusterIP，响应体为 1/16KiB，保留默认 stdout 访问日志。单个受信压测 IP 使用 16384 连接、4096 待握手预算；默认自动值会将单 IP 待握手数限制为 16，可能在压测集中建连时主动拒绝连接，不能把这种保护触发混入数据面容量对比。这是夹具配置，不改变产品的默认保护策略。每窗口先检查 Service 及每个副本返回的完整内容，记录 wrk 延迟、所有错误计数、逐秒 RSS/FD/预算/连接与请求指标、CPU 和 cgroup 限流。先后两组源站直连测量用于辨别源站或负载器瓶颈。夹具不启用 TLS、RGL 或 OTLP，不能将该容量外推到混合业务负载。

三个相同镜像的重复轮次交替扫描并发档位；所有窗口（包括预热和失败窗口）都会保留。暂定门槛为每个测量窗口错误数/完成请求数 ≤0.1%、P99 ≤50ms，另要求吞吐与 P99 的 CV 及极差/中位数均 ≤10% 才通过重复性检查。任一门槛失败都须在报告中保留；脚本完成仅表示测量完成。wrk 是闭环负载，分位数来自其修正直方图，不能证明恒定到达率下的排队延迟或生产容量。运行时不要同时进行其他压力测试；同一物理宿主机仍可能存在背景竞争。测试 namespace 和 Helm release 会保留，清理要求与上面的行为夹具相同。

## 多节点 CI

[Release workflow](../.github/workflows/release.yml)使用固定 kind v0.30.0 / Kubernetes v1.34.0，以及 [一控制节点、两工作节点配置](../examples/kind-production.yaml)。`--require-multiple-nodes` 开启 Chart 的 `requireMultipleNodes` 硬约束，并要求控制器副本实际分布在至少两个节点，否则失败。约束按 pod-template-hash 区分修订，防止旧 Pod 掩盖新修订的集中放置，同时允许滚动更新。tag 发布至少运行 120 秒混合负载、200 条规模路由；手动运行可选 120..3600 秒和最多 2000 条路由。JSON 结果作为 artifact 保留，失败阻断后续发行步骤。

kind 的多个节点仍可能共享一台宿主机。该门禁验证跨节点路由、配置传播、Pod 替换与优雅滚动，不能代表物理节点断电、网络分区或云 LoadBalancer 行为。若测试环境在下载镜像或启动阶段受阻，应报告 BLOCKED，不能把未执行检查算作通过。

## Gateway 标准行为与认证边界

仓库行为套件对固定 CRD 检查镜像采样/授权、绝对超时、状态、预检、准入、TLS、gRPC、恢复和撤销。它不是上游 conformance suite。上游 v1.6.1 源码固定于 `8bb74df00e56ec8f944d48c25e6c1c9c2f6848e3`，见[官方测试入口](https://github.com/kubernetes-sigs/gateway-api/tree/v1.6.1/conformance)。

完整认证尚需处理测试所需的多 Gateway 部署、更多 listener 端口与尚不支持的标准能力。当前每个 Deployment 绑定一个预置 Gateway，不能把过滤或跳过不支持用例后的局部通过称作完整认证。产品说明继续明确未获得认证。

## 生产前仍需环境专属验收

在目标硬件、CNI、负载均衡和观测后端上记录二进制/镜像摘要、节点资源、路由/插件数量、流量组成、持续时间、错误、P99、CPU/RSS/FD/队列及遥测丢弃数。建议至少覆盖：

- 24 小时混合业务负载，以及单租户超过预算时其他租户的延迟。
- 两个或更多故障域中的节点故障、API Server 断连、Pod 滚动、EndpointSlice 和证书轮换。
- 最大预期路由/证书/脚本规模下的更新时间和内存增长。
- 观测平台故障及恢复时间；多副本扩缩容后的本地速率额度分配与过载保护。
- 升级前后与回退时的配置兼容，及物理 NIC 上 XDP/CNI 共存与性能。

使用[副本发布门禁](publication.md)确认目标摘要被足量副本采用，同时保留业务健康门禁；配置一致不等于业务成功。上述未执行项目不会因为实现了工具或工作流而标记完成。

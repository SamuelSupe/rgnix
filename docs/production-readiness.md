# 生产验收与发布门禁

功能存在、行为测试通过和生产容量经过认证是不同的结论。v0.4.0 的范围与升级要求见[发行说明](releases/v0.4.0.md)；v0.3.0 发行记录保持其原有范围。源码验证见[Gateway 完整主流程与修复验证](validation-gateway-hardening-2026-09-26.md)，发行产物的原生双架构与 Kubernetes 门禁见 GitHub Release workflow；此前的[产品验证记录](validation-product-2026-09-26.md)保留为历史证据。

## 可重复的行为门禁

后端验证使用 Linux，OrbStack 可用于开发机验证：

```sh
bash scripts/check.sh
cargo clippy --locked --all-targets -j 2 -- -D warnings
cargo build --release --locked -j 2
```

`check.sh` 集中覆盖 HTTP/HTTPS、HTTP/2、WebSocket/SSE、流式 body、客户端取消、断连与无重放、配置恢复、日志/OTLP、访问策略、迁移和 Redis 共享限流。其结果不能替代真实 Kubernetes watch、RBAC、准入及多副本验证。

固定 Gateway API v1.6.1 标准 CRD，并构建当前源码镜像与 `scripts/fixtures/gateway-grpc.Dockerfile` 后运行：

```sh
python3 scripts/gateway_e2e.py --context YOUR_TEST_CONTEXT \
  --namespace rgnix-production-qa --image YOUR_TEST_IMAGE:TAG \
  --soak-seconds 600 --scale-routes 200 --output gateway-results.json
```

只使用测试集群。脚本只复用其标签标识的两个测试 namespace，结束后保留以便检查。测试包含故意无效的路由与授权撤销；不要在业务 namespace 中运行。启用准入阶段会创建该 Helm release 的集群级 webhook 配置，清理时先卸载对应 release，再删除测试 namespace 和专用 GatewayClass，避免留下指向已删除 Service 的 webhook。

混合负载包含 HTTP GET、TLS GET、RGL 请求/响应钩子、开启 POST body 前缀模式后的完整转发，同时开启 OTLP logs/traces。连续性门禁复用 HTTP/1.1 连接，在收到 Connection: close 后为下一请求新建连接，不重试失败请求；每 worker 最多 40 请求/秒，六个 worker 总上限 240 请求/秒。测试显式开启 shutdown.enabled。负载过程中更新插件并替换一个控制器 Pod，之后要求副本重新收敛；另有持有旧请求并更新插件的快照连续性检查，以及 Pod 终止后仍需完成的 gRPC HTTP/2 流。输出请求数、错误样本和有界延迟样本的 P99。源站和负载器均为 Python，结果用于连续性回归，不作为网关极限吞吐承诺。超过停机预算的长期空闲连接和无限期流仍会关闭；客户端需遵守其业务重连与幂等约定。

`--soak-seconds` 支持 0..86400，0 跳过混合负载；`--scale-routes` 支持 1..2000。规模测试是在基础路由以外增加指定数量的 HTTPRoute，不能据此外推最大容量。租户速率预算在负载阶段显式设为每秒 10000；限流与隔离使用独立检查验证，429 也会使连续性检查失败。

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
- 实际 Redis 拓扑与观测平台故障；声明的失败模式与恢复时间。
- 升级前后与回退时的配置兼容，及物理 NIC 上 XDP/CNI 共存与性能。

使用[副本发布门禁](publication.md)确认目标摘要被足量副本采用，同时保留业务健康门禁；配置一致不等于业务成功。上述未执行项目不会因为实现了工具或工作流而标记完成。

# v0.5.0 候选发布验证

2026-09-28。源码候选提交 `93066459a95e31de2c6a33108f76e43087925aec`，Cargo/Chart 版本 0.5.0。发布整理中的后续提交仅更正 Helm values 注释并添加本记录及 README/发行说明，没有改变 Rust、依赖锁或渲染后的工作负载配置。最终标签工作流仍重新构建并执行发行门禁；本记录不把候选运行冒充为最终标签运行。

## 原生构建与行为

[候选 Release run 36386332371](https://github.com/SamuelSupe/rgnix/actions/runs/36386332371)在原生 Linux amd64/arm64 runner 的 Rust 1.98 / Debian bookworm 容器中构建，并以非 root 用户执行行为套件：

| 检查 | 实际结果 |
|---|---|
| Rust 格式、Clippy all-targets 拒绝 warning | 两种架构通过 |
| Rust 测试 | 每架构 22 项通过，1 项已有微基准 ignored |
| HTTP、恢复、OTLP、轮转、产品策略、迁移、共享速率 | 每架构 340 项通过 |
| locked release 构建与版本检查 | 两种架构通过，均为 rgnix 0.5.0 |
| Linux XDP/kernel/libxdp | [源码 CI 36386331276](https://github.com/SamuelSupe/rgnix/actions/runs/36386331276)的 61 项通过 |
| 打包 ARM64 二进制在 OrbStack 上运行 HTTP 套件 | 94 项通过 |

OrbStack 所测候选二进制 SHA-256：`76bc333c23ea137de8dec4a0bbfda833084f5b1c1099fd321a508a3da5e97fff`。这是从 GitHub 的 `package-arm64` artifact 下载解包的产物。最终公开压缩包以 Release 附带的 SHA256SUMS 和 attestation 为准。

## Kubernetes Gateway

三节点 kind（一个控制节点、两个工作节点），固定 Kubernetes v1.34.0 / Gateway API v1.6.1 CRDs，使用本次 amd64 原生产物，要求控制器副本跨工作节点分布。

全部 **115 项检查通过**，结果 complete=true、failure=null。覆盖路由、Service/EndpointSlice、TLS/授权撤销、gRPC、镜像/超时、预检/准入、恢复、副本配置状态、发布及排空。[完整 JSON](validation/release-0.5.0-candidate-gateway.json)。

混合负载参数为 600 秒、200 条额外路由，HTTP/TLS/RGL/body/logs/traces 同时启用，包含配置发布和 Pod 替换：

- 请求 **105,806**，失败 **0**；每个负载 worker 的错误列表为空。
- 保留延迟样本 **60,000**，样本 P99 **42.883 ms**。
- HTTP/1.1 keepalive；收到 Connection: close 后重连，不重放失败请求；每 worker 最多 40 req/s。

负载器主动限速，多个 kind 节点共享一台宿主机。这是连续性回归，不是极限吞吐、物理节点故障、长期稳定性或 Gateway 上游 conformance 认证。此前 NGINX 差距、A/A 波动及尾延迟问题仍见[性能指南](performance.md)，没有被本次发布门禁消除。

## 文档与产物检查

Cargo.toml、Cargo.lock、Chart version/appVersion、镜像默认 tag、下载示例和发行说明同步为 0.5.0。Helm lint、Ingress/Gateway 渲染、git diff 空白检查通过；新 README、性能指南和发行说明的本地链接有效。更正连接池兼容性注释前后的两种渲染输出字节一致。

最终公开产物由标签工作流生成；按照[发行指南](releases.md)核对原生档案、OCI 镜像、Chart、SHA256SUMS、签名和 GitHub attestations。不能只以候选 run 或源码已推送推断正式产物已公开。

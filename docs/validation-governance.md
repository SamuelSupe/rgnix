# 治理增量验证 — 2026-09-25

> 阶段历史记录：下文保留验证当时的版本、镜像和发布状态。后续 v0.2.0 发布包的实际验收与摘要见[发布验证](validation-release-0.2.0.md)。

本轮修复上次审查列出的两项 P1 与六类 P2。配置和接口见[治理操作指南](governance.md)。源码、镜像及实际运行 Pod 的二进制摘要见 [governance-artifact.json](validation/governance-artifact.json)。本轮未提交工作区，也未更新 GitHub release。

## 问题与结果

| 问题 | 实现与已验证行为 |
|---|---|
| RGL 绕过灰度回退 | 回退锁定优先于 `route.proxy`；强制候选后端的脚本在两副本均转向稳定 Service，升级后仍保留锁定 |
| 跨 namespace 域名抢占 | 管理员域名授权；无显式授权时域名归首个 namespace。更长路径不能抢占已有域名，准入也拒绝该候选 |
| 只有共享管理令牌 | 具名 reader/writer、namespace 范围、具名审计；路由诊断过滤、越权操作拒绝，身份撤销无需重启 |
| 全集群 watch/RBAC | namespace 列表约束 watch 与 Role/RoleBinding；实际 ServiceAccount 可读所选 namespace，不能读 default 的 Secret |
| 灰度缺少发布流程 | 跨副本稳定分组、阶段权重、暂停/恢复、审批、外部指标门禁；状态跨重启恢复，最终阶段可完成发布 |
| 模拟与预检不完整 | 最终后端/URI/脱敏头和灰度计划；文件 diff、候选预检、API server dry-run 与 TLS Admission Webhook 共用校验路径 |
| 管理策略需重启 | 凭证、身份、配额/域名与指标 provider 文件成组热更新；坏配置保留上一有效版本，合法撤销及时生效 |
| 证书与 HTTPS 运维不足 | 证书名称/有效期检查、到期诊断及指标；声明式 308 保留路径与 query，HTTPS 请求正常通过 |

预检还补充了 IngressClass 所属 controller 检查：Class 被删除后由其他 controller 重建时，管理接口不能报告该候选有效；准入则交由接收方处理，不继续套用 rgnix 策略。模拟 API 故障验收与真实集群验证了相关边界，以及 Class 恢复后的数据面恢复。

## 实际验证

| 验证 | 结果 | 镜像/证据 |
|---|---|---|
| Rust、格式、Clippy | 7/7；fmt 与 `-D warnings` 通过 | 当前源码；[测试](validation/governance-rust.txt)、[Clippy](validation/governance-clippy.txt) |
| HTTP/TLS、流式请求、WebSocket、SSE、插件 | 79/79 | qa2；[清单](validation/governance-http.json) |
| 独立模式产品行为 | 89/89 | qa2；[清单](validation/governance-features.json)、[输出](validation/governance-features.txt) |
| NGINX 1.28.0 对照 | 46/46 | qa2；[清单](validation/governance-nginx.json) |
| Kubernetes 基础生命周期 | 46/46；滚动升级 300/300 请求成功 | qa2；[输出](validation/governance-kubernetes-base.txt) |
| Kubernetes 策略及新增治理 | 65/65 | qa2；[清单](validation/governance-kubernetes.json)、[输出](validation/governance-kubernetes-policies.txt) |
| Kubernetes API 故障与恢复 | 53/53 | qa3；[清单](validation/governance-recovery.json) |
| OTLP 故障、凭证及退出刷新 | 32/32 | qa3；[清单](validation/governance-otlp.json) |
| 本地文件与外部轮转 | 20/20 | qa3；[清单](validation/governance-log-rotation.json) |
| 最终镜像升级、预检与 Class 撤销/恢复 | 9/9，两副本就绪且二进制摘要与镜像一致 | qa4；[清单](validation/governance-upgrade.json)、[输出](validation/governance-upgrade.txt)、[原始探针](validation/governance-upgrade-probe.py) |
| Helm | lint；包含 watch 范围、身份、指标、准入参数的模板渲染通过 | [lint](validation/governance-helm.txt)；真实安装使用相同 chart |

`qa2`、`qa3`、`qa4` 对应同后缀的 `rgnix:0.1.0-governance-*` 本地 Linux arm64 release 镜像。qa3 增加管理预检的 Class 所属 controller 检查；qa4 补充准入对其他 controller Class 的忽略行为。这两次生产改动都仅位于 `src/ingress/preflight.rs`，反向移除各项变更可复现前一镜像的源码摘要；其他生产文件逐字节一致。收尾边界由恢复测试及最终集群探针验证，没有把前面的整套回归描述为在 qa4 上重新运行。三项 Class 用例也已加入完整 Kubernetes 脚本，本轮通过最终探针验证，未重跑扩展后的整套脚本。

最终源码 SHA-256：`33c04a68b5856b0a1c732a5326e69f25b8ae67e54533d4e68bea3127a6a34416`。摘要涵盖 Cargo.toml、Cargo.lock、src 与 vendor，定义及当前脚本摘要见 artifact。Linux 测试在 OrbStack Ubuntu 执行，Rust 1.90.0；镜像通过 Dockerfile 的 Rust 1.98 bookworm 构建。真实集群为 OrbStack 单节点 `v1.35.6+orb1`，本机 kubectl 为 1.33，命令提示了版本偏差；上述 API 调用与断言均成功。

## 验收修正与边界

- 原准入测试对已有且未变化的对象执行 apply，kubectl 没有发起变更请求。改为新候选后，API server 实际调用 webhook 并返回 DomainDenied。
- 原恢复 fixture 的证书只包含 `example.test`，却声明 `tls.example.test`。修正 SAN 后继续验证撤销、坏证书、替换和所有权释放；跨 namespace 的重叠通配证书按新增域名边界拒绝。没有放宽生产校验来适配旧 fixture。
- 灰度流程用短阶段和零最小样本数验证审批、指标门禁、跨副本同步与持久化。错误率/p95 回退另用实际故障候选后端验证。外部指标采用集群内 HTTP JSON fixture；没有对线上 Prometheus 或第三方观测平台做认证。
- 配额仍按进程计数；没有跨副本精确配额或 namespace 独立 cgroup。强 CPU/内存隔离使用独立控制器部署、IngressClass、限定 watch 和 Kubernetes 资源限制。
- 外部指标失败阻止阶段推进；自动回退由本地错误率/p95 触发。准入证书更新需滚动控制器；业务 TLS Secret 支持热更新。没有增加 ACME 或证书签发功能。
- 本轮未进行 Linux amd64 运行、多节点/云 LoadBalancer、长期压力与恶意租户容量极限验收，也未重新测量性能基线。

## 复现与保留环境

```sh
# OrbStack Ubuntu 中已有 Rust、OpenSSL、Python grpcio/Brotli、logrotate 等依赖
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target cargo clippy --locked --all-targets -- -D warnings'
docker build -t rgnix:0.1.0-governance-qa4 .

# 使用新的专用 namespace；这两项会创建、修改、撤销测试资源，并保留检查环境
RGNIX_IMAGE_TAG=0.1.0-governance-qa4 bash scripts/ingress-e2e.sh rgnix-governance-example orbstack
RGNIX_IMAGE_TAG=0.1.0-governance-qa4 python3 scripts/product_kubernetes.py rgnix-governance-example orbstack
```

本轮保留 `rgnix-governance-final-20260925` 与 `-tenant`，最终两个控制器 Pod 运行 qa4。早期验证的 `rgnix-governance-qa-20260925` 与 `-tenant` 也保留；未停止或清理其他任务的环境。原始升级探针绑定最终 QA namespace，检查 `rgnix-qa=true` 与 Helm 归属后才执行短时 Class 撤销/恢复，不能直接用于生产环境。

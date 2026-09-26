# 产品增强验证 · 2026-09-26

本轮按生产验收、Gateway 标准能力、跨副本发布状态三个方向实施。改动位于未发布工作区；已发布的 v0.3.0 产物不包含这些能力，也没有推送新的 GitHub Release。此前的 XDP 与性能改动保留在同一工作区，其实测记录仍单独保存。

## 实现范围

- HTTPRoute 标准 RequestMirror：百分比/分数采样、Service 和 ReferenceGrant 校验、BackendTLSPolicy、受限完整请求体镜像；无效镜像引用停止镜像并报告条件，主请求继续使用有效后端。
- HTTPRoute `request` / `backendRequest` 绝对超时：涵盖请求处理、上游连接、上传及响应流，持续发送小块数据也不能绕过截止时间；已发响应头后关闭流，不重放请求。
- Gateway、HTTPRoute、GRPCRoute 候选预检及 TLS 准入：使用同一编译和授权规则，严格检查候选插件，不能靠历史检查点掩盖坏脚本；保护控制器持有的发布注解。
- 可选 Lease 副本报告、`/v1/fleet`、固定标签指标和 `rgnix wait`：比较目标摘要、输入/控制配置、就绪、拒绝原因和心跳；权限或观测故障使发布门禁失败，不中断已经接纳的业务流量。
- Helm 多节点硬分散选项、业务/准入 Service 与 PDB 的控制器专用选择器，避免同 release 的 XDP 节点代理混入；发布 CI 增加三节点 kind、路由规模、混合负载和发布期间的请求连续性验证。

使用与限制见 [Gateway](gateway-api.md)、[发布门禁](publication.md)和[生产验收](production-readiness.md)。

## 产物与环境

| 项目 | 实际值 |
| --- | --- |
| Git 基线 | `6839556234dbb11b16d7299b6e7b0eafa446c53f`，带本轮及先前未提交改动 |
| Linux | OrbStack Ubuntu arm64，内核 `7.0.14-orbstack-00380` |
| 构建 | Rust `1.90.0`、OpenSSL `3.5.3`，锁定 Cargo 依赖 |
| 二进制 SHA-256 | `2a7c07ab6984265c230bb0bd97120e4ae2fb2cd8975448538b3a4d677d5c2506` |
| 本地 QA 镜像 | `rgnix:publication-final-20260926`，Linux arm64 |
| 镜像 ID | `sha256:451834757f38335e14f4a03cb439b4e2bc7ccaabd967ba0f2d00836fdc1adc49` |
| Rust 源码与依赖摘要 | `cdbba9c3f1d83d4ceb5b43d3733bb3680a4873dcae3b6eaf0a3643fc2882131d`；计算范围和方法见原始 JSON |
| Gateway 集群 | kind `v0.30.0` / Kubernetes `v1.34.0`，一控制节点、两工作节点，运行在 OrbStack Docker |
| Ingress 集群 | OrbStack Kubernetes `v1.35.6+orb1`，单节点 |
| Gateway API | `v1.6.1` standard CRDs，下载 SHA-256 已校验 |

QA 镜像通过替换本地验证镜像中的二进制构建，不是已发布的 Debian 镜像或跨架构发行包。没有将本地镜像标签视为远端交付。

## 实际验证

最终二进制的[原生行为原始记录](validation/product-native-2026-09-26.json)：

| 检查 | 结果 |
| --- | ---: |
| Rust 边界测试 | 13/13 |
| HTTP 行为 | 82/82 |
| 模拟 Kubernetes API 的 Ingress 恢复 | 57/57 |
| OTLP 日志、span 与传播 | 32/32 |
| 本地日志轮转 | 20/20 |
| 产品策略、协议、认证和管理 | 111/111 |
| 迁移工具 | 10/10 |
| 双进程共享限流与故障恢复 | 12/12 |

合计 324 项行为检查及 13 项 Rust 测试通过。`cargo fmt`、Clippy 全 targets 拒绝 warnings、release 构建通过。Linux 服务验证均在 OrbStack 执行；宿主机仅编排 Kubernetes、Helm 和检查文档/脚本。

真实 Kubernetes 验证按阶段记录：

| 阶段 | 实际结果与证据 |
| --- | --- |
| Ingress 策略、治理和观测全流程 | **71/71**，最终二进制；[原始记录](validation/product-ingress-2026-09-26.json) |
| 三节点 Gateway 主流程 | **101 项检查通过**，包括标准镜像/总超时、预检、TLS/gRPC、撤销/恢复、命名空间配额、跨副本报告、200 条规模路由和混合负载；末尾准入部署失败，[原始记录](validation/product-gateway-before-admission-fix-2026-09-26.json)保持 `complete: false` |
| 准入安装顺序修复后专项 | **7/7**：先验证未注册服务的 TLS 与预检，再注册 Fail webhook，验证 Gateway/HTTPRoute/GRPCRoute dry-run、非法策略和控制器注解保护，最终两节点副本收敛；[原始记录](validation/product-gateway-admission-2026-09-26.json) |

Ingress 最终两个 Pod 与 Gateway 最终两个 Pod 的二进制 SHA-256 均实际核对，值与上表一致。Gateway 主流程结束后的改动限于 Chart 注册开关、部署文档及测试编排，没有修改 Rust 二进制。修复后按准入阶段集中复验，**没有把整条 Gateway 主脚本描述成一次运行全部成功，也没有再次重跑整条主脚本**。完整套件已改为同样的两阶段注册顺序，等待后续 CI 执行。

Gateway 混合负载持续 **180 秒**，在原有路由以外增加 **200 条 HTTPRoute**；六个 worker 分别覆盖 HTTP GET、TLS GET、RGL 请求/响应钩子，以及开启 4 KiB body 前缀模式的 POST 完整转发，同时启用 OTLP logs/traces。期间更新插件并替换一个 Pod：

- 成功 **35049** 次，失败 **0**；采集 **35049** 个延迟样本，P99 **47.45 ms**。
- 单独的延迟请求在发布过程中使用旧插件，新请求使用新插件；替换副本最终采用相同配置摘要。
- 同 release 的 XDP 标签 Pod 没有进入业务副本报告。撤销 Lease `list` 权限时报告不能通过发布门禁，而数据面仍返回 200；恢复权限后重新收敛。
- [Kubelet 资源采样](validation/product-resources-2026-09-26.json)记录两个控制器当时约 **0.065 / 0.075 CPU 核**、**44.6 / 38.1 MiB RSS**。这是单次采样，不是峰值、平均值或长期内存稳定性证明。

该负载限定每 worker 最多 40 请求/秒，不用于宣称极限吞吐或 24 小时生产稳定性。混合阶段的镜像和节点分散规则与最终产物相同；后续准入部署修正没有改变请求处理。

Chart 默认、Gateway/准入/XDP 共存和硬分散配置的 Helm lint/渲染检查通过；两阶段分别验证保留 TLS Service 但不注册 webhook、以及完整注册。Python 语法、相对文档链接和 `git diff --check` 通过。

## 失败实验和修正

- 早期混合负载使用默认租户速率上限，得到预期 429。连续性夹具改用显式的每秒 10000 请求预算；配额与隔离仍由独立检查覆盖，429 不会被计作连续性成功。
- 共享虚拟机曾因多个项目构建与 tmpfs 缓存共同占用内存而 OOM，退出码 137 的运行不计为产品通过或容量结论。将本项目构建缓存迁到磁盘后重跑，没有停止其他项目工作负载。
- 无间隔的新连接压力耗尽负载器的临时端口。最终连续性检查每 worker 最多 40 请求/秒，六个 worker，总请求量以实测为准，不作为峰值吞吐测试。
- 连接复用实验完成 546455 次成功请求，Pod 替换时出现 3 次请求失败（2 次 `RemoteDisconnected`、1 次 `BrokenPipe`）。[原始失败记录](validation/product-keepalive-drain-2026-09-26.json)保留 `complete: false`；该实验使用中间二进制 `b52f61bf7c58e8018c20ba2999e01855a77244313b348351b6ba28eb3abfd0af`。网关未自动重放请求，本轮没有解决所有持久连接的无损滚动，不能用新连接门禁替代这项结论。
- 初始软调度及未区分修订的硬分散都出现过滚动后两个新副本同节点。硬约束现在按 `pod-template-hash` 分组，并排除不能调度的污点节点；实测同时检查实际节点分布。默认关闭该选项以支持单节点环境。
- kind 的 `kubectl port-forward` 在本环境延后约 1 秒传递 EOF。相同流式截止场景在集群内约 160 毫秒返回截断；测试改为集群内计时，保留 800 毫秒上限，没有放宽网关时间约束。
- Ingress 回退夹具曾在配置可见但检查点尚未持久化时发送失败样本。源站自身也以 503 表示候选故障，不能只用状态码区分已激活候选与暂不可用路由；现在先检查正常请求返回候选身份，再发送失败请求驱动回退。夹具修正中的错误 500 断言也已移除。
- Gateway 首次启用准入时，Helm 可能先创建 webhook 再更新自身 Gateway，此时服务尚未监听，Fail 策略会阻止安装。新增 `admission.register`：第一次以 false 启动并等待 TLS 服务，第二次设为 true 注册。验证过程保持 `failurePolicy: Fail`；失败安装遗留的本测试 webhook 在核对 Helm 所属 release/namespace 后单独清理。最终注册配置仍由 Helm 管理。

## 复现与保留资源

在专用测试集群安装固定 CRD、准备当前源码镜像及 gRPC 源站镜像后：

```sh
python3 scripts/gateway_e2e.py --context YOUR_KIND_CONTEXT \
  --namespace YOUR_GATEWAY_QA_NAMESPACE --image YOUR_IMAGE:TAG \
  --require-multiple-nodes --soak-seconds 180 --scale-routes 200 \
  --output gateway-results.json

RGNIX_IMAGE_TAG=YOUR_TAG RGNIX_IMAGE_REPOSITORY=YOUR_IMAGE \
  python3 scripts/product_kubernetes.py YOUR_INGRESS_QA_NAMESPACE orbstack
```

Ingress 夹具要求预先创建专用 `rgnix-qa=true` namespace。本轮最终使用 `rgnix-product-kind-20260926` 及 `-peer`，和 `rgnix-final-ingress-20260926` 及 `-tenant`。测试有意制造无效资源与故障，结束状态不能直接作为生产示例。资源和本地证据保留供检查，未修改全局 kubectl context。

## 未验证范围

本轮没有执行 24 小时持续负载、物理节点故障/网络分区、真实云 LoadBalancer、目标硬件容量认证或物理 NIC 上的 XDP 性能。kind 的三个节点共享宿主机，不能等同三个物理故障域。XDP 内核行为没有因本轮控制面改动重复运行，证据见已有独立记录。

没有运行完整上游 Gateway conformance suite，也没有取得认证；当前 Deployment 绑定预置 Gateway，与上游测试需要的多 Gateway/更多 listener 部署仍有边界。此次行为套件不得称为完整标准认证。

原生 amd64 与 Debian 发行镜像的门禁仍由 CI 配置承担，本轮未执行远端 Actions、镜像推送、签名或 GitHub 发布。

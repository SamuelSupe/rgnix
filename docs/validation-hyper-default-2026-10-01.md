# Hyper 默认内核准备验证 — 2026-10-01

当前已补齐内核选择、配置预检、发行产物验证和长测记录。运行默认值仍为 Pingora；24 小时测试及性能资格尚未完成。原始阶段状态见[机器记录](validation-hyper-default-2026-10-01.json)，切换条件见[默认内核门槛](hyper-default-rollout.md)。

候选运行源码为 `100916e24ef2ffa895ce45bd27e305ceba215a24`；测试脚本跟进为 `4ebadcdab498ffa6053e9042aca33376a86cc2f3`。已推到独立 `validation/hyper-default-20261001` 分支，没有修改工作分支或发布正式 tag。

## 已验证

- CLI 新旧参数、环境变量优先级、默认启动和显式回退；实际运行内核通过 `/metrics` 核对。Pingora-only 构建也实际启动，并明确拒绝选择未编入的 Hyper。
- `check` 与 `serve` 都拒绝明文/动态端口的 HTTP/3 配置，修复了检查通过但启动失败的兼容性预检遗漏。
- 12 个 Helm 选择/拒绝组合，包括拒绝把布尔值或数字当作内核选择；Helm lint、格式与 diff 检查通过。本地单元检查 25 通过、1 忽略。
- OrbStack 原生 arm64 Bookworm/Rust 1.98 release 二进制，两种内核分别通过 104 基础行为、57 Ingress 恢复、32 OTLP、20 文件轮转、115 产品功能、10 迁移、12 共享限流检查；Hyper 另通过 68 内核检查和 71 协议检查。
- 同一二进制装入最终 Bookworm 镜像，默认 Pingora、显式 Hyper/Pingora 均实际启动；镜像中的可执行文件摘要与已测产物一致。
- 该镜像在本地三逻辑节点 Kind 集群通过 46 项 Ingress 主流程检查。集群仅有一台物理宿主机。

本地产物 SHA256：`14bc92121ee7aa4552d0681b3fd2fd2867b24bb7ac72df6fea287543fc786abf`。没有把 Ubuntu 构建的二进制装入 Bookworm 作为正式兼容性证据。

## 远端与长时间验证状态

[首轮 CI](https://github.com/SamuelSupe/rgnix/actions/runs/36847012578)的原生 amd64、arm64 release 二进制完整行为检查与最终镜像检查均通过。整个 CI 失败：两个未优化 Hyper 作业在约 1 MiB 的资源恢复场景触发 3 秒期限。本地相同恢复流程及两种架构的优化发行产物通过了原期限；[第二轮 CI](https://github.com/SamuelSupe/rgnix/actions/runs/36849418205)为该作业启用开发构建优化，保留原检查和期限，全部 7 个作业通过，包括双架构完整 Hyper 行为、严格 Clippy、协议与共享池回归。

Gateway 主流程进入了 24 小时混合负载，首次发布/轮换/替换后曾保持零请求错误。随后并行吞吐压测期间记录到 4 次 503，日志明确指向 Redis 共享限流依赖不可用；该轮已经停止并记为失败，没有把它算作 24 小时验收通过。是否由并行压测竞争引起需要串行复验，不能据此断言 Hyper 内核故障或排除产品问题。

性能前两轮与功能检查/稳定性测试重叠，已停止，不能用于发布资格或吞吐收益结论。接下来先完成独立的同二进制 Hyper/Pingora/NGINX 交错 A/A、A/B，再启动新一轮 24 小时混合负载。长测现在在出现请求错误后提前结束，并增加共享限流指标采样。

本线程已有每 30 分钟的持续跟进，读取 `.local/hyper-default-gates-20261001/` 的进程收据、趋势与 CI 结果。所有门槛通过后才一起修改 Rust 和 Chart 默认值，并重新验证默认启动及 Pingora 回退。完整 Gateway conformance、跨物理主机故障和生产容量未在本轮证明。

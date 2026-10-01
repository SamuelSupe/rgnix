# Hyper 默认内核准备验证 — 2026-10-01

当前已补齐内核选择、配置预检、发行产物验证和长测记录。随后按用户明确要求将 Rust 与 Chart 默认值切为 Hyper；24 小时测试及完整性能资格仍未通过，不因默认值改变而视为通过。下述产物和 CI 记录属于切换前的候选，默认变化后的产物单独记录。原始阶段状态见[机器记录](validation-hyper-default-2026-10-01.json)，选择及剩余验收见[默认内核记录](hyper-default-rollout.md)。

候选运行源码为 `100916e24ef2ffa895ce45bd27e305ceba215a24`；测试脚本跟进为 `4ebadcdab498ffa6053e9042aca33376a86cc2f3`。已推到独立 `validation/hyper-default-20261001` 分支，没有修改工作分支或发布正式 tag。

## 默认值切换复验

Rust 标准构建与 Helm 未指定内核时选择 Hyper；`--engine pingora`、`RGNIX_ENGINE=pingora`、旧 false 参数仍明确回退。配置检查与启动使用同一选择规则。Pingora-only 构建保持默认 Pingora，并明确拒绝选择未编入的 Hyper。

在 OrbStack 原生 arm64 的 Debian Bookworm/Rust 1.98.1 中重新构建 `http3,jemalloc` release 二进制。无内核参数的默认 Hyper 和显式 Pingora 分别通过 104 项集成检查，覆盖真实 HTTP/TLS、代理、插件/body、热更新和排空。13 组 Helm 选择/拒绝检查通过；精简构建实际启动与拒绝不可用 Hyper 通过。主项目格式、Python/Shell 语法及 diff 检查通过。

新产物 SHA256：`da8bac193c58e6c357ce5a83d3b495a70c99969929775f753eedeb45f0df3db7`。最终 Bookworm 镜像 `rgnix:hyper-default-switch-20261001` 中的二进制摘要相同；默认 Hyper、显式 Hyper 和显式 Pingora 的真实 HTTP、就绪和内核指标均通过。新镜像在未设置内核选择的 Helm Ingress 流程通过 46 项检查，包括 TLS/插件更新、资源删除/恢复、Service 地址状态和双副本滚动升级（期间 300 次 Service 请求通过）。两个就绪副本均为 Hyper，无重启，Pod 内二进制摘要与已测产物一致。

新默认值的[双架构 CI](https://github.com/SamuelSupe/rgnix/actions/runs/36856117213)全部 7 个作业通过：原生 amd64/arm64 Bookworm 发行产物完整行为、协议、最终镜像和严格代码检查均通过。下载的两种架构镜像收据均明确记录默认 Hyper、显式 Hyper/Pingora，镜像二进制摘要与相应已测产物一致。CI 对应运行源码 `d2c60aba05477568b176fdbc5220645fcd5fbd94`；切换前的候选证据仍单独保留。

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

性能前两轮与功能检查/稳定性测试重叠，已停止，不能用于发布资格或吞吐收益结论。串行对照完成 72 个窗口，零请求错误；Hyper 和 NGINX 的四组 A/A 均通过，Pingora 仅一组通过。原始数据见[吞吐记录](validation/hyper-default-benchmark-2026-10-01.json)，判定见[校准分析](validation/hyper-default-benchmark-2026-10-01-analysis.json)。

| 普通 HTTP/1 代理 | 并发连接 | Hyper 中位数 req/s | NGINX 1.28.0 中位数 req/s | Hyper/NGINX |
|---|---:|---:|---:|---:|
| 1 KiB | 64 | 77,719 | 114,429 | 67.9% |
| 1 KiB | 256 | 71,216 | 108,644 | 65.5% |
| 16 KiB | 64 | 67,106 | 61,770 | 108.6% |
| 16 KiB | 256 | 57,140 | 61,602 | 92.8% |

同一份本地 Bookworm 二进制、两个 worker、固定 CPU 集、三轮交错比较，关闭访问日志/插件/Tracing。来源为可信 loopback，显式调大 Hyper 来源准入上限。只有 16 KiB/256 连接的两种产品内核同时通过校准：Hyper 相对 Pingora 吞吐提高 37.2%；其他三组不宣称稳定百分比提升。该对照仅限这些窗口和普通代理，不能代表整产品容量或完整 NGINX 性能持平。

吞吐对照结束后，第二轮串行 Gateway 长测在 484.19 秒、79,264 次成功请求后出现 6 次 503，已提前结束并记为失败。日志均指向共享命名空间 Redis 限流不可用或耗尽；并行压测不是复现的必要条件，具体根因尚未确认。持续副本无重启，最近样本 RSS 约 43 MiB、描述符 104，许可没有持续增长。没有以增加依赖超时或 fail-open 掩盖失败。新一轮验证必须使用独立计时并保留这两轮失败记录。

本线程已有每 30 分钟的持续跟进，读取进程收据、趋势与 CI 结果；跟进指令已更新为尊重用户要求的 Hyper 默认值并继续定位未完成验收。第三轮候选 [CI](https://github.com/SamuelSupe/rgnix/actions/runs/36852190823)也已全部通过；该源码尚未包含后续默认值变更。完整 Gateway conformance、跨物理主机故障和生产容量未在本轮证明。

## 共享限流故障定位

从保留的持续副本指标确认，6 次失败全部为 `unavailable_closed`，并非 token 或 key 容量耗尽。Redis 没有重启、命令错误或连接拒绝；持续 Gateway 和 Redis 的 cgroup CPU 节流计数均为 0，无 OOM。旧故障的 6 次共享限流耗时合计 3.023 秒，超过每次配置的 200 毫秒预算。Redis 慢日志中单次 ZADD 17.6 毫秒、EVAL 最高约 29 毫秒；慢日志不包含客户端网络收发耗时，不能用它直接解释完整超时。[Redis 官方计量说明](https://redis.io/docs/latest/commands/slowlog-get/)

在保留的旧候选命名空间完成 15 分钟有界诊断：六个 HTTP/TLS/RGL/body worker，另有独立 Redis PING 和客户端调度探针。900.11 秒内 152,198 次请求零错误，Redis 探针无错误；Redis RTT 最高 64.37 毫秒、客户端调度间隙最高 140.81 毫秒。部分窗口与诊断镜像构建重叠，该轮不是新的 24 小时验收，也未复现 503，不能用这些独立样本确认上次失败原因。

额外错误分类日志仅构建在本地诊断镜像，未加入产品源码或发布分支；保持超时、限流和失败策略不变。诊断镜像内二进制 SHA256 为 `2ab9cf0b00138a2f3de350fbcc567bdd8c137d3b79929b0854b8c7b06eee9800`，基于新默认值源码，仅增加固定错误类别与耗时日志，不记录 Redis 地址或原始错误。

新的独立命名空间 `rgnix-hyper-limiter-diagnostic-20261001` 正在运行 Gateway 主流程预检，随后运行一小时有界复现，同时保留各副本错误日志、Redis RTT、客户端调度间隙和资源指标。进程收据位于 `.local/hyper-limiter-diagnosis-20261001/diagnostic-process.json`；它属于故障定位，不能作为最终产物的 24 小时资格。两轮失败证据和原命名空间均保留。根因仍未确认，没有宣称修复完成。

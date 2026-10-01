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

独立命名空间 `rgnix-hyper-limiter-diagnostic-20261001` 的一小时有界诊断提前失败：1952.28 秒内 322,745 次成功请求，另有一次 TLS worker 读取超时；经过四次插件发布。该轮没有记录 HTTP 503，共享限流 `unavailable_closed` 观察值为 0，固定类别的限流错误日志没有触发。两个 Gateway 和 Redis 均无重启。

失败期间，负载端调度间隙最高 33,879.91 毫秒，两个 Gateway 的服务心跳年龄最高均为 34.198 秒；Redis PING RTT 最高 525.15 毫秒。宿主机电源日志确认：本地时间 20:40:15 因电量 1% 进入 `Low Power Sleep`，20:42:33 接入电源后唤醒；对应 UTC 12:40:15–12:42:33。该轮出现了明确的宿主机暂停，不能用它证明产品稳定性通过，也不能将它改记为成功。此前六次 Redis 503 尚未找到对应休眠记录，根因仍未确认。

原始证据保留于 `.local/hyper-limiter-diagnosis-20261001/`：`diagnostic-process.json`、负载和资源样本、两个副本日志、Redis INFO/SLOWLOG、`runtime-probe.jsonl` 和 `failure-summary.json`。运行时观察器在失败后停止；每副本每秒 2.5 次 `/metrics` 是额外诊断负载。历史两轮长测和这轮失败均保留，不算最终产物的 24 小时资格。

在确认宿主机接电充电后，新建 `rgnix-hyper-limiter-awake-20261001` 命名空间单独计时，继续使用相同诊断二进制、200 毫秒超时、限流策略、失败关闭和不重试规则。临时防闲置/接电休眠保护最长三小时，不修改永久电源设置；不能保证强制休眠或断电后测试仍有效。新进程收据为 `.local/hyper-limiter-awake-20261001/diagnostic-process.json`，保护收据为同目录 `sleep-protection.json`。本次调整验证环境，产品源码没有改动；六次限流 503 仍需定位。

接电复验的第一次预检发现新加逐副本检查覆盖了主流程的 `admin_port` 变量，两个副本的 Hyper 指标检查通过后，后续管理请求连接了已经关闭的端口。已改用独立的 `replica_admin_port`；完整失败日志、结果、脚本和进程收据存入 `.local/hyper-limiter-awake-20261001/preflight-port-regression/`。该次尚未进入混合负载，属于验收脚本回归。修正后的流程已复跑，并通过了此前失败的管理请求阶段；一小时诊断仍待完成。

## Gateway 默认选择检查补充

发现 `gateway_e2e.py` 未指定内核时仍向 Chart 写入旧的 `experimentalHyper.enabled=false`，导致该用法选择 Pingora。本次将未指定值保留为 `null`，沿用 Chart 默认值，并在入口流程核对两个副本的 `rgnix_engine_info`；报告同时记录所检查的内核。显式 `--engine pingora` 和旧 Hyper 参数继续支持。

Python 语法检查及 Gateway 模式的默认 Hyper、显式 Pingora Helm 渲染通过；这些检查不算真实 Gateway 运行通过。默认 Gateway 复验因上述诊断失败而暂缓，旧收据另存为 `.local/hyper-default-switch-20261001/default-gateway-process-before-awake.json`。现已重新排队，等待新接电诊断成功结束后执行；仍使用真实产品镜像、独立命名空间 `rgnix-hyper-default-gateway-20261001`，不指定内核参数。收据为 `.local/hyper-default-switch-20261001/default-gateway-process.json`；新的诊断失败也会暂缓该复验。产品 Rust 源码与已测产物保持一致。

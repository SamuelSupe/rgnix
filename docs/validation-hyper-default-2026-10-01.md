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

接电复验的第一次预检发现新加逐副本检查覆盖了主流程的 `admin_port` 变量，两个副本的 Hyper 指标检查通过后，后续管理请求连接了已经关闭的端口。已改用独立的 `replica_admin_port`；完整失败日志、结果、脚本和进程收据存入 `.local/hyper-limiter-awake-20261001/preflight-port-regression/`。该次尚未进入混合负载，属于验收脚本回归。修正后的接电诊断已完成：3600.12 秒、608,855 次成功请求、零错误，完整 117 项检查通过。宿主机电源日志核对 UTC 13:00:17–14:08:54 窗口，没有记录 Sleep/Wake；临时保护在旧诊断和默认 Gateway 结束后释放。该轮使用 d2c60 的诊断镜像，包含额外观察请求和少量局部回归，未包含以下产品修复，不能替代最终产物的 24 小时验收。

## Gateway 默认选择检查补充

发现 `gateway_e2e.py` 未指定内核时仍向 Chart 写入旧的 `experimentalHyper.enabled=false`，导致该用法选择 Pingora。本次将未指定值保留为 `null`，沿用 Chart 默认值，并在入口流程核对两个副本的 `rgnix_engine_info`；报告同时记录所检查的内核。显式 `--engine pingora` 和旧 Hyper 参数继续支持。

Python 语法检查及 Gateway 模式的默认 Hyper、显式 Pingora Helm 渲染通过。接电诊断结束后，未指定内核的真实默认 Gateway 流程在独立命名空间 `rgnix-hyper-default-gateway-20261001` 完成全部 112 项检查。两个最终副本均就绪、无重启，实际指标为 Hyper；逐副本二进制摘要与 d2c60 产品产物 `da8bac…f3db7` 一致。收据为 `.local/hyper-default-switch-20261001/default-gateway-process.json` 和 `default-gateway-deployed-pods.json`。此前暂缓及预检失败记录保留；这份结果只证明 d2c60，后续修复用新的产物另外验证。

## 共享限流连接管理修复

在 OrbStack 用已测产品二进制 `da8bac…f3db7` 和受控 Redis 协议端点复现了独立缺陷：配置 200 毫秒预算、并发上限 1，暂存第一个扣额响应，再发送一个超额请求。三个请求依次得到 200、503、200，端点收到两次 EVAL，但正常连接被超额请求清除，下一次请求额外建立了第二条连接。原始记录在 `.local/hyper-limiter-reconnect-20261001/baseline.json`；新增的完整共享限流回归在原二进制上准确失败于“保留正常连接”检查，其余已执行的检查通过。

源码修正只清除本请求实际使用的连接，并比较连接身份，避免未访问 Redis 的并发拒绝清除正常连接，也避免旧连接的失败清除新连接。限流预算、失败关闭和禁止重放规则保持不变。Rust 格式、Python 语法及差异检查通过；后续完整双架构 CI、两种内核的受控 TCP 回归及最终镜像均已通过，见下方最新产物结果。此前一小时诊断和默认 Gateway 使用旧产物，不作为此修复的通过证据。

这次复现确认了连接管理缺陷，没有证明它导致此前六次约 0.5 秒的共享限流 503；原始故障根因仍待确认。上述少量请求及一次共享限流回归与诊断窗口有重叠，仅用于局部回归，不算无干扰的长测资格。

## Pingora 回退就绪检查修复

连接管理修复提交 `07355b3` 的新 CI 中，原生 Bookworm amd64 发行产物在现有反复启动/退出检查中失败：`/readyz` 已返回 200，紧接着访问业务监听端口出现 `ConnectionRefusedError`。该用例运行 Pingora，尚未执行到该产物的共享限流回归。失败日志存入 `.local/hyper-limiter-reconnect-20261001/ci-amd64-failure.log`；这次 CI 不能记为双架构产物通过。

Pingora 的业务监听器异步绑定，而管理服务可能更早启动；原先的就绪值只表示配置有效。新增监听完成回调，在全部业务端口绑定且接受连接的任务已经安排后才允许就绪。`/readyz` 与 `rgnix_ready` 共用同一判断，配置更新不能提前绕过监听器的就绪条件。Hyper 仍使用已在启动前绑定的监听器，不改变默认内核。保留原有反复启动检查及其期限，不将失败改成等待业务端口后再断言。修复的静态检查及下方最新双架构发行产物、最终镜像运行检查通过。

该修复提交 `ca46d76` 的 CI 又在日志回归脚本中捕获验证设施缺陷：工作目录以临时空闲端口命名，后续端口复用导致 `FileExistsError`，服务尚未启动。这不是日志产品行为失败；将目录改为原子创建的唯一临时目录，保留全部 20 项日志行为检查。完整日志存入 `.local/hyper-limiter-reconnect-20261001/ci-ca46-check-failure.log`，这次 CI 也不能记为整体通过。此项只改验证脚本，不改变 ca46d76 运行源码；下方新 CI 已取得完整通过结果。

## 两项运行修复后的产物验证

运行源码为 `ca46d76b52e2059cac4d20f1e5936f68de49d946`，验证脚本快照为 `70f46f492bc0b7d81d76c40e6e500a9970c21f17`。[新 CI](https://github.com/SamuelSupe/rgnix/actions/runs/36873436732)全部 7 个作业通过，包括严格代码检查、两种原生架构的完整行为及 Hyper 协议检查、Bookworm release 产物和最终镜像。两种架构的 Hyper/Pingora 分别通过 104 基础行为检查和扩展后的 14 共享限流检查；原有启动/退出检查及期限保留。

原生 arm64 产物 SHA256 为 `69bc0043db1adb669e4e7ebdeeca2c2dbc25524aaff39eb2259ad47f8c2231c3`，amd64 为 `dbf04693180c7a9a57eebd35afd45b9ae7863407bd6b50845cd75fbc6238bd51`。下载的两种架构 CI 镜像收据均核对到对应已测产物，默认 Hyper、显式 Hyper/Pingora 的真实 HTTP、就绪与内核指标通过。

用已测 arm64 二进制在 OrbStack 复跑受控 Redis TCP 场景，两种内核均保持 200 毫秒预算和并发上限 1：三个请求为 200、503、200，两次 EVAL 全部使用同一条 TCP 连接。超额请求继续失败关闭，没有重复扣额。该二进制装入 `rgnix:hyper-limiter-fix-20261001`，镜像内摘要一致；默认 Hyper、显式 Hyper/Pingora 的真实 HTTP、就绪及指标检查通过。旧连接复现与本次修复证据保留于 `.local/hyper-limiter-reconnect-20261001/`。

新镜像在未指定内核的 Helm Ingress 流程通过 46 项检查；两个最终副本的实际 Hyper 指标、无重启和 `69bc…31c3` 二进制摘要均已核对。未指定内核的完整 Gateway 主流程通过 112 项检查，两个最终副本同样核对实际 Hyper 和相同二进制摘要。独立 24 小时混合负载在 2026-10-02 UTC 03:01 提前失败：44,334.815 秒、7,478,122 次成功请求和两次 HTTP 503，未达到 24 小时资格。本地后续流程串行执行，进程收据为 `.local/hyper-runtime-fix-20261001/process.json`；当前状态为 `FAILED`。独立 24 小时负载使用新命名空间 `rgnix-hyper-runtime-fix-soak-20261001`，保存原始请求与逐副本资源样本，临时防休眠保护已随本轮进程结束释放。

原始六次 Redis 503 根因仍未确认；受控连接缺陷修复及一小时零错误诊断都不足以解释它们。新产物尚未完成独立 24 小时资格，不能沿用旧失败计时或把运行中记为通过。上述历史性能数字对应 `14bc…6abf`，不是本次 ca46d76 产物的吞吐测量。

## 新产物 24 小时长测失败 — 2026-10-02

本轮使用上述已测 `69bc…31c3` arm64 发行二进制和最终镜像，未指定内核选择，两个副本实际运行 Hyper。六个混合负载 worker 在约 12.3 小时后提前退出：7,478,122 次成功请求、两次失败，均为 TLS GET worker 收到真实 HTTP 503。期间完成 74 次插件发布；TLS 轮换和另一副本替换按既定间隔执行。失败负载没有重放。`process.json` 保存的最后轮询样本尚为零错误，最终计数以 `soak.load.jsonl` 的 `final: true` 行为准，两条计时时间轴分别保留。

失败后指标确认共享限流 `unavailable_closed=2`，两次累计耗时为 0.417989608 秒；访问日志中的两次 503 与共享命名空间限流错误对应，没有选择上游。该轮仍使用 200 毫秒预算、失败关闭和原并发上限，未改变 Redis 扣额语义。错误发生前的最近成功请求窗口 p99 上升到约 84 毫秒；每 worker 只保留最近 3000 个成功请求，这不是整个窗口的精确 p99。

持续副本 UID `c391fc0e-e6f5-40b0-8514-8147b5be768a` 在全部 1468 个资源样本中保持一致，最终无重启，实际二进制摘要仍为 `69bc…31c3`。RSS 观察范围约 39.6–51.0 MB、最终 44.7 MB，描述符最高 105、最终 104。Gateway 和 Redis 的 CPU 节流计数、OOM 计数为零；Redis 无重启、拒绝连接或命令错误。最终 SLOWLOG 中 EVAL 最大执行时间为 42.483 毫秒，不能据此排除网络、客户端队列或运行时延迟。资源采样间隔为 30 秒，也不能排除亚秒级停顿。

按电源日志第四列的确切事件类型核对，从本轮启动至失败后的窗口未记录 Sleep/Wake/DarkWake；宿主机仍接交流电、电量 100%。自有 runner、负载子进程及临时防休眠进程均已退出。原日志跟随进程在 UTC 18:42 后停止产生记录，存在采集缺口；已从仍保留的 Pod 日志补取 UTC 02:55 起的失败窗口，不能将旧跟随文件视为覆盖完整长测。

原始请求、资源、Pod、Redis、日志和电源证据，以及新增失败摘要，保留于 `.local/hyper-runtime-fix-soak-20261001/`。根因仍未确认：受控连接管理缺陷的修复没有解释本轮两次 503，也没有解释此前六次故障。下一步使用基于 ca46d76 的独立诊断镜像补充固定错误类别、限流阶段耗时、Redis RTT 与运行时调度观察；诊断结果不能替代发行产物资格。默认 Hyper 保持用户选择，24 小时和当前产物性能资格仍未通过。

## OTLP 长测接收端检查修正

新失败窗口的指标还确认：日志和 span 的接收数均为零，记录均因导出失败丢弃。实际接收端返回 HTTP 200、空响应，却没有 `Content-Type: application/x-protobuf`；产品导出器按 OTLP 响应契约拒绝它。因此上述长测只证明尝试输出，不能算日志或 span 成功交付。该验证设施缺陷尚不能解释两次 Redis 限流 503。

Gateway 接收端现在返回正确的 protobuf 响应类型。长测保存 OTLP 计数，在持续副本上比较初始和结束计数，要求实际接收日志与 span、结束时队列排空且窗口内没有丢弃。用同一份已测 `69bc…31c3` 发行二进制在 OrbStack 的真实 HTTP 上对照两个接收端：原接收端日志/span 接收均为零、各一次导出错误和丢弃；修正接收端两者各成功接收一次，错误与丢弃均为零。结果保留于 `.local/hyper-runtime-diagnosis-20261002/otlp-receiver-regression.json`。

验证脚本提交 `241951f26667dd95fe3ee121b286bb1d5a1aea25` 的[后续 CI](https://github.com/SamuelSupe/rgnix/actions/runs/36961961578)已完成，全部 7 个作业通过。独立下载并核对两种原生发行二进制、完整行为收据及最终镜像收据：arm64 仍为 `69bc…31c3`，amd64 仍为 `dbf046…bd51`，与前轮 `70f46` 已测产物完全一致；默认 Hyper、显式 Hyper/Pingora 的真实 HTTP、就绪与内核指标通过。收据为 `.local/hyper-runtime-diagnosis-20261002/qa-ci-verification.json`。本次 CI 没有运行 Kubernetes Gateway 或 24 小时混合负载；接收端的真实 HTTP 对照由上述 OrbStack 回归另行验证，CI 通过不能覆盖此前同一产物的长测失败。

第一轮诊断于 UTC 03:48:49 启动，在混合负载开始前因汇总可能误用诊断 probe 行而主动终止。完整预检和脚本收据归档到 `preflight-report-parser-regression/`；这次中断不算产品失败。负载汇总已改为只读取含 `workers` 的 JSON 行，新轮于 UTC 03:54:08 在 `rgnix-hyper-runtime-diagnostic-20261002-r1` 独立启动，完整 Gateway 预检后运行最多 86400 秒的混合负载；不是最终产品的 24 小时资格。诊断镜像基于 ca46d76，只增加限流错误类别、连接所属线程和阶段耗时记录，二进制摘要为 `369c81321ab6f7e14810bf3d6568f13dc17416cdec4abb528980538410b597b9`。镜像内摘要、默认 Hyper 和显式 Hyper/Pingora 的真实 HTTP/就绪/指标，以及 14 项共享限流检查通过。额外观察包括每秒 50 次 Redis PING、每秒 5 次持续副本指标请求和客户端调度间隙；日志跟随进程退出后会重新连接并保留重连记录。诊断使用修正后的 OTLP 接收端，保持 200 毫秒预算、失败关闭、不重放及串行负载。进程、构建与源码收据保留于同目录，状态以 `process.json` 为准。当前运行产物的 24 小时稳定性与性能资格均仍未通过。
## 独立诊断复现查询超时 — 2026-10-02

UTC 03:54 启动的独立诊断在 UTC 09:04 提前失败：实际混合负载 18,297.287 秒、3,088,846 次成功请求、一次 POST /plugin HTTP 503，失败请求未重放。诊断二进制仍为 `369c…97b9`，包含额外探针，不能记为正式产物资格。新增限流日志确认本次错误为 `redis_timeout`、`phase=query`，耗时 302,086 微秒；复用了原连接，缓存锁等待为零，查询在开始后约 5 微秒进入。失败指标 `unavailable_closed=1`，累计耗时 0.302113567 秒，没有选择上游。

同一失败窗口观察到 Redis PING 往返约 299.95 毫秒、管理指标请求往返约 322.02 毫秒、客户端调度间隙约 357.08 毫秒、服务心跳年龄最高约 0.595 秒。这些只是时间相关的观察，尚未分离网络、运行时、宿主机/VM 调度和负载 Python 进程内部的 GIL 影响。错误类别只确认本轮诊断的查询超时，不能据此解释此前正式产物的两次 503 或原始六次 503。

持续 Pod UID `2bdfed62-9674-4cba-8b53-32ec58a8e33b` 在 606 个资源样本中保持一致、无重启；实际诊断二进制摘要已再次核对。RSS 为 41.6–56.0 MB、最后样本 45.3 MB，描述符最高 105、最后 104。Gateway、Redis 和负载容器的 CPU 节流及 OOM 计数为零，压力文件不可用，不记为压力为零。Redis 无拒绝连接或命令错误，SLOWLOG 的 EVAL 最大执行时间为 87.231 毫秒；这不包含客户端网络和执行器延迟。失败后实际日志/span 接收计数分别为 3,084,373 和 6,168,745，导出错误为零；因负载失败，未执行成功结束的交付/排空门槛，不能记为完整 OTLP 长测通过。

按确切电源事件列核对，诊断启动至失败后没有 Sleep/Wake/DarkWake，宿主机接交流电且电量 100%。自有 runner、负载和临时防休眠进程已退出。日志跟随进程于 UTC 07:59 退出后重连，原始文件覆盖实际失败；保留重连记录，不将采集缺口改写为完整覆盖。失败摘要、查询日志、探针窗口、Redis/Pod/资源/电源证据保留于 `.local/hyper-runtime-diagnosis-20261002/`。

下一轮于 UTC 09:15:23 在 `rgnix-hyper-runtime-diagnostic-20261002-r2` 启动，记录目录为 `.local/hyper-runtime-clock-diagnosis-20261002/`；本次记录时仍在 Gateway 预检阶段，没有正式 24 小时通过结论。它复用同一冻结诊断镜像和六个 worker，保留 200 毫秒预算、并发上限、失败关闭和不重放，只增加两个独立解释器的时钟/资源观察：一个在 macOS 宿主机，一个在 Linux 负载 Pod 内，与负载 Python 进程的 GIL 分离。两者的实际短时间采样和自有停止标记退出检查通过。Linux 观察在实际混合负载开始后启用，宿主机观察已启动；两者均有最大运行期限，随自有 runner 结束释放。进程与观察阶段以新目录的 `start.json`、`process.json` 为准。该轮也是诊断，正式产物的 24 小时稳定性和性能资格继续保持未通过，默认 Hyper 保持用户选择。

## 独立时钟诊断失败与资源对照 — 2026-10-02

上述 r2 已于 UTC 09:25 提前失败并退出。实际混合负载 235.257 秒、35,564 次成功请求、四次 HTTP 503，分别来自两个 TLS GET worker、一个 GET /plugin 和一个 POST /plugin worker；没有重放。失败前 105 项 Gateway 检查通过，完成一次插件发布、TLS 轮换和非持续副本替换；成功结束检查未完成。这些结果使用 `369c…97b9` 诊断二进制，不能算发行产物资格。

四条限流诊断全部为 `redis_timeout`、`phase=query`，耗时分别为 203,997、310,740、309,331、309,940 微秒；缓存锁等待均为零，查询开始前分别约 6、3、7、2 微秒。它们复用了同一连接，连接创建在线程 `BG hyper-0.0.0.0:8080-1`，失败请求来自 HTTP 和 TLS 线程。指标 `unavailable_closed=4`，累计耗时 1.134077212 秒；四条访问日志都没有选择上游。仍保持 200 毫秒预算、并发限制和失败关闭。

UTC 09:24:59–09:25:05 的故障窗口中，Redis PING 往返最高约 310.31 毫秒、服务心跳年龄约 0.452 秒；独立 macOS 时钟的等待间隙最高约 12.83 毫秒，Linux 时钟约 72.37 毫秒。Linux 整轮最高间隙 315.48 毫秒发生在 UTC 09:24:17，早于本次故障约 45 秒，不能把该峰值直接当作四次超时的原因。Linux 观察由独立解释器运行，排除了与业务负载共享同一个 Python GIL；一个进程的时钟不能代表 Redis 或连接驱动线程的调度。Linux 观察于 UTC 09:22:02 开始，未覆盖最初约 55 秒负载。两条时钟都有正常结束记录，自有 runner、负载、时钟及临时防休眠进程均已退出。

持续 Pod UID `8ef905d6-b13e-4d59-8ae0-f9e7f32517e8` 在九个资源样本中一致、无重启，实际 Hyper 和诊断二进制摘要再次核对。RSS 为 42.7–50.3 MB、最后 44.0 MB，描述符最高 105、最后 97。Gateway、Redis 和负载容器没有 CPU 节流或 OOM；压力文件不可用。Redis 无重启、拒绝连接或命令错误，最终 SLOWLOG 的 EVAL 最大执行时间为 62.230 毫秒。日志/span 实际接收计数为 34,326/68,648，导出错误为零；负载失败，不能记为成功结束的 OTLP 交付/排空通过。按确切电源事件列核对，从启动至失败后没有 Sleep/Wake/DarkWake，宿主机交流电 100%。

故障窗口的 Linux 系统负载约为 67.0/57.2/51.7；失败后的独立资源检查还观察到交换内存使用和多个活跃容器。后采集的数据不证明故障时的具体资源竞争，未停止其他任务或改变永久宿主机设置。原始日志、四条错误、时钟/探针窗口、逐 Pod 指标、Redis、资源和电源证据及 `failure-summary.json` 保留于 `.local/hyper-runtime-clock-diagnosis-20261002/`。本轮确认的是查询超时及独立 Linux 等待间隙，根因仍未确认，也没有据此解释先前正式产物的两次或原始六次 503。

UTC 09:46:36 启动一个独立计时的 900 秒资源对照，收据目录为 `.local/hyper-runtime-resource-control-20261002/`。它复用仍保留的 r2 Gateway、Redis 和 origin Pod，实际 Hyper/`369c…97b9`/持续 UID/零重启已核对；业务 HTTP/TLS/RGL/body worker 数为零，只保留 Redis PING 每秒 50 次、管理指标每秒 5 次和独立 Mac/Linux 时钟，增加内存、交换与重大缺页计数观察。该阶段用于判断没有业务请求时是否仍有环境停顿，不能计为业务请求成功数、正式 24 小时或性能资格。启动时交流电 100%，自有临时防睡眠断言已核对；观察有最大运行期限，结束须核对停止收据和电源窗口。本次文档记录时仍在运行，结果以该目录的 `process.json` 和真实观察行为准。

## 无业务负载资源对照完成与线程采样诊断 — 2026-10-02

UTC 09:46 启动的资源对照已正常结束，实际窗口为 900.067 秒，业务 HTTP/TLS/RGL/body worker 数为零。Redis PING 往返最高 45.33 毫秒、探针错误为零，管理指标往返最高 86.44 毫秒；限流允许数、`unavailable_closed=4` 和累计失败耗时均与开始时一致，没有新增业务扣额或失败。独立 Mac/Linux 时钟都有正常结束记录，自有 runner、探针、时钟及临时防休眠进程均已退出。持续 Gateway UID 和零重启保持；确切电源事件列未记录 Sleep/Wake/DarkWake，宿主机交流电 100%。原始观察、相邻窗口、人工审核和摘要保留于 `.local/hyper-runtime-resource-control-20261002/`。

Linux 独立观察器的最大等待间隙为 463.38 毫秒，发生于 UTC 09:53:13。相邻 PING 报告窗口最高 13.26 毫秒；几乎同时的管理请求往返为 1.62 毫秒，HTTP/TLS 线程心跳年龄约 64–65 毫秒。最高 0.815 秒心跳属于 `BG rollout-metrics`，不能描述为 HTTP/TLS 线程的相同停顿。相邻一秒 Linux 报告中，全局交换读入和重大缺页计数分别增加 21,737 和 22,049；这些是 VM 全局计数，不能归为 Gateway/Redis 自身的缺页，更不能证明此前 503 的原因。时钟只测量每次十毫秒等待，资源读取、报告和刷新仍可能造成两次等待之间的采集间隙；单个观察器的间隙不足以确认整台 VM 暂停。

新一轮于 UTC 10:18:08 在 `rgnix-hyper-runtime-diagnostic-20261002-r3a` 启动，收据目录为 `.local/hyper-runtime-scheduling-diagnosis-20261002/`。本次记录时处于完整 Gateway 预检；`BOUNDED_DIAGNOSIS_RUNNING` 不等同实际混合负载开始。随后只运行最多 900 秒混合负载，复用同一 `369c…97b9` 诊断镜像和冻结负载脚本，原六个 worker、200 毫秒预算、并发限制、失败关闭、不重放、插件发布/TLS 轮换和持续副本约束均保留。

新增观察器通过现有 Kind 节点的 Python 读取持续 Gateway 与 Redis 的确切进程/线程调度等待、CPU 时间、缺页和上下文切换计数，每 100 毫秒采样；容器 UID、节点 PID、启动计数共同固定身份。它在第一条持续副本资源样本出现后启动，初始负载覆盖须依据实际时间戳核对。独立 Mac/Linux 时钟及原 PING/管理探针保留。调度计数的解释依据 [Linux 调度统计文档](https://docs.kernel.org/scheduler/sched-stats.html)，缺页字段依据 [proc 文档](https://docs.kernel.org/filesystems/proc.html)；调度等待是累计记账，各次读数还可能在等待结束后反映更早的等待；相邻差值不能直接当成一次连续停顿或严格归属这两个读取时间之间。内核可能截短线程名，各字段按顺序读取，也不是原子快照。保存观察器自身耗时、实际采样间隙和停止收据，没有修改调度参数。

线程采样器已在原保留的真实 Gateway/Redis 进程上完成两秒读数和自有停止标记回归，正常退出；这只证明采集设施局部可用。首次新轮预检中，滚动部署有三个临时 Pod，观察器提前执行最终两副本核对并主动中断，尚未进入业务负载；完整证据归档于 `preflight-replica-observation-regression/`。现在等待恰好两个活跃且就绪的 Pod 再执行同样的严格核对，没有放宽二进制、内核或重启检查。

新诊断有最大运行期限和自有停止标记，临时防休眠保护随 runner 结束释放。诊断正常结束也不替代正式发行产物的 24 小时资格。产品运行源码和正式二进制未变，原正式 12.3 小时失败以及 r1/r2 诊断失败全部保留；根因、24 小时和当前产物性能资格仍未确认，默认 Hyper 保持用户选择。

## 线程诊断失败与原生线程身份补充 — 2026-10-02

线程采样轮 r3a 在 UTC 10:34:46 的实际负载阶段出现一次 TLS GET HTTP 503，683.563 秒内 113,351 次成功请求，完成两次插件发布；105 项预检通过，900 秒混合负载门槛失败。该轮使用 `369c…97b9` 诊断二进制，不是正式发行产物验收。实际错误为 `redis_timeout/query`，耗时 235,590 微秒，cache 等待为零，query 在开始后 10 微秒发起，使用已有连接；`unavailable_closed=1`，累计失败耗时 0.245810 秒，未选择上游且没有重放。连接创建线程为 HTTP 8080-1，失败请求线程为 TLS 8443-1，旧内核线程名被截短，尚不能将它们精确对应到采样 TID。

失败附近的累计样本出现持续 Gateway 重大缺页增加 13、Redis 主线程增加 1；Redis 主线程调度等待记账增量约 179.81 毫秒，一个 Hyper 线程约 145.92 毫秒。观察器也有约 216 毫秒的采样间隔。它们是顺序读取的累计统计，等待可能延后记账，不能直接相加解释 235.59 毫秒，更不能证明某次连续停顿或缺页的具体原因。相同秒的 Redis SLOWLOG 有 133.688 毫秒及 30.502 毫秒 EVAL，缺少逐请求对应，不能将某条慢命令直接指定为失败请求。附近正负四秒窗口 PING 最高 166.55 毫秒，独立 Mac/Linux 等待间隙最高 14.78/126.48 毫秒；这些时间相关观察仍未分离网络、运行时和宿主机/VM 影响。

持续副本 UID `7f63b639-0d64-4ce8-91f4-0c679eff1c35` 的 23 个资源样本一致，零重启且实际二进制摘要匹配；RSS 42.9–51.4 MB，最后负载资源样本 44.7 MB，FD 最高 106、最后 104。Gateway、Redis、origin 没有 CPU 节流或 OOM；压力文件不可用，不能记为零。Redis 没有命令错误或拒绝连接。故障后的日志/span 导出计数为 109,584/219,167、导出错误零，后来队列为零；由于负载失败，成功结束的交付/排空门槛没有执行，不能算完整 OTLP 长测通过。两独立时钟和线程观察都有正常结束记录及 exit0，自有 runner/child/guard 全部退出。确切电源事件列从启动到失败后未记录 Sleep/Wake/DarkWake，宿主机交流电 100%。完整失败证据、采样、线程差值与 SHA 收据保留于 `.local/hyper-runtime-scheduling-diagnosis-20261002/`。

后续使用独立目录 `.local/hyper-runtime-thread-map-20261002/`，只在诊断源码增加完整运行服务名、原生 Linux TID、连接创建和失败请求 TID。采样器保留节点 TID 与容器命名空间 TID 的对应关系，已在原保留的真实进程上完成两秒采样和自有停止标记退出检查。连接创建身份不等于实际观察到连接驱动每次执行；须结合单线程运行配置和采样进一步核对。旧冻结源码、旧失败证据及原负载脚本保留；200 毫秒、并发限制、失败关闭、不重放和六个业务 worker 的要求不变。

新诊断二进制摘要为 `c75ac98223975e33d45a476c4ce7a0d3b403244adef258d1324085f7e03c692c`。14 项共享限流检查和最终镜像默认 Hyper、显式 Hyper/Pingora 的真实 HTTP、ready 和指标检查已通过，只证明诊断产物局部回归。

新一轮于 `2026-10-02T11:32:02.059514+00:00` 启动，命名空间为 `rgnix-hyper-runtime-diagnostic-20261002-r4b`。本次文档是启动阶段快照；是否进入实际混合负载以含 `workers` 的真实 JSON 行及进程收据为准，不能仅凭运行字段判断。 新轮完整预检后最多 900 秒混合负载，仍保留独立时钟、PING/管理探针、持续副本及线程采样约束。首次构建因镜像没有 cargo-fmt 中断，作为设施尝试归档，随后使用已有宿主机格式化工具；没有把它算作产品运行失败。

此前 r4a 预检在混合负载开始前因随附 `load-probes.py` 没有复制到新目录而退出，没有产生该轮混合负载请求；这是验证设施遗漏，完整预检、实际线程映射核对及停止记录归档于 `preflight-load-probe-path-regression/`。两个实际 Pod 的完整 Hyper 服务名、原生 TID 与节点 NSpid 对应已通过核对，但那些 Pod 属于旧预检，不能当作新轮持续副本。现补齐原冻结探针文件，SHA 与原文件一致，负载脚本未变，所有旧自有进程已退出；新的 r4b 单独预检和计时，实际副本身份仍须复核。

根因仍未确认，不据相关统计直接修改产品策略。正式 12.3 小时失败、r1/r2/r3a 失败全部保留；正式产品源码和发行二进制未变，默认 Hyper 保持用户选择，24 小时及当前产物性能资格仍未通过。

## 原生线程诊断的结束阶段设施问题 — 2026-10-02

r4b 的实际混合请求窗口正常结束：900.114 秒、148,568 次成功、零请求错误、两次插件发布，`unavailable_closed=0`。108 项检查已通过，其中包含请求窗口及持续副本 OTLP 交付、排空、无丢弃增量检查；实际交付日志/span 增量为 145,822/291,644。持续 Pod `gateway-6576c9cbf-mjbwm`、UID `be4bcb78-339d-4729-a41f-0c3dad300ed2` 的 61 个资源样本一致。RSS 42.5–51.4 MB，最后 44.0 MB；FD 最高 107、最后 98。它是 `c75…692c` 诊断二进制的短窗口，不是正式发行产物的 24 小时资格。

完整流程随后失败于验证设施：Admission 证书将完整服务 DNS 名放入 Common Name，65 个字符超过 OpenSSL 的 64 字符限制，尚未执行后续 Admission 检查。冻结脚本在请求与 OTLP 门槛之后调用 `reconnect()`，按原流程滚动重启 Gateway；线程观察器在请求最终报告后约 24.49 秒失去旧目标 PID，并记录设施错误。不能将这两项问题记为业务 503，也不能把整轮记为通过。原始报告、错误、线程结束状态、实际停止收据、请求/资源/时钟/日志和 SHA 记录保留于 `.local/hyper-runtime-thread-map-20261002/`，人工判定见 `review-summary.json`。

该轮四个 Hyper 服务的完整名称与容器原生 TID 已通过节点 `NSpid` 核对。当前 Kubernetes 日志已轮转掉启动行，映射使用同一实际 Pod 的保留 follower 启动日志，原始启动行与收据单独保存。观察器只覆盖真实请求开始后的线程窗口，不能声称覆盖最初约 4.66 秒；Linux 独立时钟缺少最初约 3.06 秒。两个时钟及 Redis 线程观察正常退出，Gateway 线程观察记录上述目标消失错误；所有自有进程、guard 已退出。按确切电源事件列从启动到结束后没有 Sleep/Wake/DarkWake，宿主交流电 100%。该健康窗口没有确认先前查询超时的根因。

QA 接收端已将初次及轮换证书的 Common Name 改为固定短名称，SAN 仍保留完整服务 DNS 名。在 OrbStack 实际执行从 QA 源码提取的证书命令：原 65 字符 CN 稳定失败，修正后的两种证书生成、完整 DNS 名验证通过，错误主机名被拒绝。这个局部回归证明证书 fixture 修正，完整 Gateway Admission 行为仍须新轮核对。产品运行源码和正式发行二进制未改。

新的独立目录 `.local/hyper-runtime-thread-map-r5-20261002/` 复用同一 c75 诊断镜像，只修改 QA 证书和观察器生命周期。负载及 OTLP 门槛成功后，脚本请求停止线程观察器，runner 核对正常 `thread-end` 和 exit0 后才确认停止；随后才允许原定滚动重启和 Admission 流程。真实 owned Gateway/Redis 上的停止请求/确认回归通过，原进程身份保持，没有发起业务请求或重启产品。此前冻结源码、脚本和失败证据保留。200 毫秒、并发限制、失败关闭、不重放、六个业务 worker 和持续副本要求未放宽。

新轮于 `2026-10-02T12:17:00.955319+00:00` 启动，命名空间 `rgnix-hyper-runtime-diagnostic-20261002-r5a`，最多 900 秒混合负载，先完整预检。本次记录是预检阶段快照，尚未观察到该轮含 `workers` 的真实负载报告；后续以 `process.json`、实际请求和观察器结束收据为准。诊断负载脚本 SHA 为 `3ee551dff9a813c3f79f1353b553727796ede5ed858788b52197cba6d49266c0`，runner SHA 为 `53a84fb8192571b6066a29eafaa9fcc2f1b8e33d42729d31f6a719c283dbcfb9`，与旧冻结脚本区分。

正式产品 12.3 小时失败、r1/r2/r3a 查询超时失败均持续有效；r4b 健康请求窗口及设施修正不能覆盖它们，也不能替代正式产物 24 小时或当前产物性能资格。默认 Hyper 保持用户选择，根因仍未确认。

## 原生线程映射复验的真实查询超时 — 2026-10-02

r5a 于 UTC 12:29:21 出现真实业务失败，runner 于 12:29:49 退出：实际混合负载 354.008 秒、57,122 次成功、worker 4 一次 `GET /plugin` HTTP 503，未重放。105 项 Gateway 预检通过，混合负载门槛失败，完成一次插件发布、TLS 轮换和另一副本替换。后续成功结束的 OTLP 交付/排空、线程停止握手、滚动重启和 Admission 门槛未执行，不能将上一轮设施修正或局部证书回归写成完整 Gateway 通过。它仍是同一 c75 诊断镜像，正式产品 12.3 小时失败及旧诊断失败持续有效。

实际限流日志为 `redis_timeout/query`，耗时 203,856 微秒，cache 等待 0，query 于第 8 微秒开始，复用连接 `0xffff85e3f100`；`unavailable_closed=1`、耗时计数和 0.205299162 秒，503 未选择上游。完整服务日志、精确 Pod UID、容器 ID、node PID/start_ticks 与 NSpid 将连接创建线程映射为 HTTP8080-0：容器 TID 15/节点 TID 165302；失败请求线程为 HTTP8080-1：容器 TID 16/节点 TID 165303。两服务实际为单线程 runtime，但连接创建者身份并不是 Redis driver 每次实际 poll 的观测证据。

包含失败日志时刻的相邻约 100.3 毫秒读数中，创建线程累计 runqueue 等待记账增加 190.342 毫秒，失败请求线程重大缺页增加 13。等待可能在发生后记账，顺序累计读取可能跨越不同执行时段；不能将增量当作一次连续暂停、相加到请求耗时，或证明缺页/调度就是原因。原始线程窗口、逐线程身份、读数和状态保存于 `failure-thread-window-raw.json`、`failure-thread-deltas.json` 和 `failure-thread-bracket.json`。

正负四秒报告窗口内 Redis PING 最高 34.500 毫秒，同进程客户端调度间隙 261.804 毫秒，独立 Mac/Linux 等待间隙最高 12.877/26.174 毫秒。管理异常读数 RTT 4.728 毫秒、服务心跳最高 0.371 秒；附近管理汇总报告的最高 RTT 73.012 毫秒属于其报告区间，不能当作精确同时读数。Redis 最终 SLOWLOG EVAL 最高 74.695 毫秒，实际失败秒没有超过该 SLOWLOG 阈值的 EVAL；这不能证明命令没有执行，也不能排除网络、连接 driver 或运行时排队。Redis 无重启、拒绝连接或命令错误，Gateway/Redis/origin 的 CPU 节流及 OOM 为零；压力文件不可用，不记为零。

持续 Pod `gateway-7548c7cb54-pf7gs`、UID `6402a68c-bf0b-42b6-aece-71b348efd0ca` 的 12 个资源样本一致，实际 Hyper/c75、零重启；RSS 42.7–53.1 MB，最后 45.0 MB，FD 最高及最后 104。失败后日志/span 接收计数为 55,087/110,173，导出错误为零、随后队列为零；因业务失败未执行成功结束门槛，不能算完整 OTLP 长测通过。线程和两个独立时钟均正常停止、exit0，无观察器设施错误，自有 runner/child/guard 已实际退出。启动至失败后按确切第四事件列没有 Sleep/Wake/DarkWake，交流电 100%。完整证据及 SHA 收据在 `.local/hyper-runtime-thread-map-r5-20261002/`。

证书 QA 提交 `0a56bff` 的 [CI](https://github.com/SamuelSupe/rgnix/actions/runs/37006103377)已全部 7 作业成功。已独立下载两架构完整发行二进制，实际 SHA 与原 69bc/dbf046 产物一致；原生 Hyper/Pingora 各 104 基础行为、14 共享限流、57 恢复、20 日志轮转、115 产品检查和最终镜像默认 Hyper/显式 Hyper/Pingora 的真实 HTTP、ready、metrics 及摘要收据核对通过。CI 不执行 Kubernetes Gateway 或 24 小时负载，不能覆盖同一正式产物之前的失败。

为完成证书修正的完整 Admission 验证，另在 `.local/hyper-admission-fixture-20261002/` 冻结公开 QA 提交 0a56bff，使用原已测 69bc 正式镜像单独运行完整 Gateway，`soak_seconds=0`、未传内核选择。首次准备误选 scratch 通用 QA 文件，其摘要不等于公开 QA，被启动前严格检查拒绝；第二次收据选择了不存在的 Chart 文件路径，也在启动子测试前退出。两次均零业务请求，没有子测试或命名空间，设施证据已分别归档。从公开提交冻结源码、按实际 Chart 文件生成收据后独立启动，不能将这些设施中断当产品请求失败。

当前完整 Gateway 复核于 `2026-10-02T12:51:14.150780+00:00` 启动，命名空间 `rgnix-hyper-runtime-admission-fixture-20261002-r3`。本次记录为 `GATEWAY_RUNNING` 阶段快照，后续以实际 `process.json`、全部 Admission 行为和最终两 Pod 的 Hyper/69bc/零重启收据判定。

限流根因仍未确认。下一步应直接观察该 Redis 连接 driver 的实际运行，再决定产品修复；不凭线程创建映射或相关性修改限流策略，不放宽 200 毫秒、不失败开放、不重放，不跳到性能资格。默认 Hyper 保持用户选择。

## 公开 Gateway 复核的调度容量中断 — 2026-10-02

正式 69bc 镜像的公开 QA 复核 r3 于 UTC 12:58:34 结束，99 项检查通过后，在命名空间配额检查后的两副本滚动部署阶段等待超时；完整 Gateway、后续 Admission 尚未通过。新 Pod `gateway-b796559b4-whg29` 始终 Pending、没有节点，调度事件明确报告两工作节点达到 Pod 数上限，随后同时受拓扑分布约束阻止。已运行的两个 Gateway Pod 仍就绪、零重启，实际二进制均为 69bc。该失败发生在调度新副本之前，不是新的混合负载限流 503。原始报告、Pending Pod UID、事件、部署和节点状态、现存日志、二进制及 SHA 收据保留于 `.local/hyper-admission-fixture-20261002/`。runner/child/guard 已退出；确切电源事件列没有 Sleep/Wake/DarkWake，交流电 100%。

已核对本线程三次结束且没有混合负载的预检，以及上述 r3 的精确 namespace 和退出记录。先保存这些 namespace 及 peer 的资源、现存日志、Pod UID、原副本数和 SHA，再仅将其中 Deployment 缩到零，保留 namespace 和恢复副本数收据。共八个 namespace 的 Pod 已退出；缩容前后，其余 194 个 Pod UID 保持，包含此前正式及诊断失败现场。本次未修改其他任务或系统调度参数。归档和复核收据在 `completed-fixture-capacity-archive/`。两工作节点由 Pod 上限附近恢复至 93/92 个活动 Pod；它只解释本次 Pending 调度中断，不能解释此前限流 503 或证明较轻环境下的稳定性。

独立 r4 于 UTC `2026-10-02T13:08:19.068220+00:00` 启动，目录 `.local/hyper-admission-fixture-r4-20261002/`，namespace `rgnix-hyper-runtime-admission-fixture-20261002-r4`。沿用公开 QA 0a56bff 的同一冻结脚本和正式 69bc 镜像，全部 Gateway/Admission 检查、原超时、资源要求和拓扑要求不变；启动前核对两节点各有 17/18 个 Pod 名额。没有混合长负载或性能对照，未传内核选择，交流电及临时防睡眠断言已核对。当前记录仍是运行阶段快照，完整报告和最终 Hyper/69bc/零重启副本收据须在结束后审核。限流 driver 的直接运行观测尚未构建或启动，须等本轮结束后串行进行。此前正式 12.3 小时及 r1/r2/r3a/r5a 的业务失败持续有效，默认 Hyper 保持，24 小时资格尚未通过。

## 正式产物的完整 Gateway / Admission 复核通过 — 2026-10-02

独立 r4 于 UTC `2026-10-02T13:16:20.701891+00:00` 正常结束，公开 QA 的完整报告 `complete=true`、`failure=null`，112 项检查全部通过。包含全部 10 项 Admission 行为：合法资源 dry-run、持久化前拒绝不支持策略、保护控制器自有状态、改变 parentRefs 不能导入伪造状态、服务不可用时对 owned 资源失败关闭但不阻止其他 Gateway、服务恢复、所有副本证书无重启轮换、移除旧 CA 后成功，以及无效证书更新保留已接受证书。71 字符的完整服务 DNS 仍保留于 SAN，初次及轮换证书使用固定短 CN，未关闭 TLS 验证。

最终两个 Pod `gateway-8855bfb88-scmjw` / `gateway-8855bfb88-v9tlm` 的 UID 分别为 `1dcf2f20-3317-4b3b-8098-409c3421e00f` / `312e8046-864e-4f06-933a-b66029626cfe`，位于不同工作节点，实际 Hyper、ready、零容器重启，进程二进制 SHA 均为原正式 69bc 产物。它们是常规 QA 计划滚动部署后的最终副本，零重启不代表整个 24 小时混合负载持续副本通过。runner/child/guard 均已实际退出；启动至结束后确切电源事件列无 Sleep/Wake/DarkWake，宿主交流电 100%。完整报告、实际副本、管理指标、电源、人工审核和 SHA 收据在 `.local/hyper-admission-fixture-r4-20261002/`。

这补齐了证书设施修正的完整公开 Gateway/Admission 行为，未修改产品运行源码或正式二进制。`soak_seconds=0`，没有本轮混合长负载或性能对照；不得覆盖正式 12.3 小时及 r1/r2/r3a/r5a 的限流查询超时失败。直接观察 Redis driver 的诊断仍仅处于准备计划阶段，尚未构建或启动。默认 Hyper 保持，限流根因、正式产品 24 小时和当前产物性能资格仍未确认。

## Redis driver 直接观测的诊断产物

UTC 2026-10-02 14:00 的独立诊断产物已完成构建与局部回归，目录 `.local/hyper-runtime-driver-probe-20261002/`，二进制 SHA `66895251b21d0a47c40c45e10b72d7138ab52a7c4a2ddc825418b41635cd017c`。它基于 r5/c75 冻结源码，只在 scratch 中复制并修改 Redis 1.7.1 和限流诊断记录；原 registry archive SHA 与全部 124 个缓存文件核对，共享 Cargo registry/git 只读挂载，旧冻结树和正式产品运行源码未改。原连接 driver 的 `T::spawn`、共享任务 handle 生命周期、timeout/response 配置、permit、连接身份清理、失败关闭和不重放保持。初始化完成后才开始直接记录 driver poll/wake、实际 native TID、连接与 query 标识；bootstrap 未覆盖，创建者身份不能替代实际 poll 观测。

14 项共享限流回归、最终诊断镜像默认 Hyper/显式 Hyper/Pingora 的真实 HTTP、ready、metrics 与实际二进制摘要通过。独立真实 Redis TCP 回归验证了原连接释放后 clone 仍可 PING、200ms blocking query 在 202.331ms 超时、Redis MONITOR 仅记录一次该 BLPOP、最后 clone 释放后独立观察 handle 仍看到 driver 结束，以及替换连接 PING 成功、driver ID 不同。首次 INFO 扣额次数收据断言和第二次既有镜像目录碰撞属于验证设施失败，原始日志与产物保留；后一轮沿用已重新编译并通过 14 项检查的精确产物完成镜像及真实 MONITOR 验证，未把旧缓存二进制作为替身。所有构建进程、临时 guard、回归监视器及其临时容器已退出。

观测新增每个 driver 的初始分配、每次 poll 的转发 waker 分配和 2048 条标量 ring；try-lock 争用、sequence、record/snapshot 时间都有记录，不能据局部计时宣称开销可忽略。原 poll 返回值和 wake 转发保留；idle gap 或 wake 入口至下次 poll 的间隔不是 OS runqueue 延迟。失败后的分块日志可影响之后并发，admission 完成时间和 bulk dump 时间分开记录；最终快照在 driver Drop 入口，随后才销毁 inner future。不会记录命令、URL、凭据或请求 body。

独立 r6a 于 UTC `2026-10-02T14:03:54.664141+00:00` 启动，namespace `rgnix-hyper-runtime-diagnostic-20261002-r6a`。先完整 Gateway 预检，再最多 900 秒原六 HTTP/TLS/RGL/body worker、600 秒事件/100 路由的混合负载，200ms 和原限流行为不变；保留独立时钟及精确 owned Gateway/Redis 线程采样。使用同一 r5 冻结 harness `3ee551dff9a813c3f79f1353b553727796ede5ed858788b52197cba6d49266c0`，包括短 CN、完整 DNS SAN、成功负载与 OTLP 门槛后先停止线程观察器的握手。启动前两 worker 分别有 14/12 个 Pod 空位，未停止其他任务。14:05 快照仍在预检，尚无含 workers 的混合负载行；AC100%、自有临时防睡眠断言和确切事件列已核对。最新以实际 process/workers/final 和停止收据为准。

上述只完成诊断产物与设施回归，不是新的正式发行产物、完整混合诊断或 24 小时资格。正式 69bc/dbf046 二进制及此前 12.3 小时和 r1/r2/r3a/r5a 的真实业务失败持续有效；根因仍未确认。默认 Hyper 保持，不放宽预算、不失败开放、不重放，不跳到性能。

## 直接 Redis driver 有限诊断完成与一小时复验 — 2026-10-02

独立 r6a 于 UTC 14:27:36 结束并完成人工审核。实际混合负载 900.123554 秒、148,322 次成功、零请求错误、两次插件发布；完整报告 complete=true、failure=null，118 项检查包含全部 10 项 Admission 均通过。持续副本实际交付日志 145,273 条、span 290,546 条，结束队列排空且没有丢弃增量。成功门槛后，线程观察器先按停止握手正常退出，再执行原定 Gateway 滚动重启及 Admission。线程和两个独立时钟均 stopped=true、exit 0，runner/child/guard 均已实际退出。

52 个资源样本的持续 UID 3755232f-9357-433e-9a42-e02ddbae2771 一致，实际 Hyper/668952 诊断产物，进程启动时间保持；RSS 43.5–56.9 MB、最后 46.3 MB，FD 最高 105、最后 98，共享限流 unavailable_closed 为零。样本包括负载结束后的交付和排空观察。计划滚动后的两个最终副本再次核对实际 Hyper/ready/同一 668952 摘要/零重启，不能用它们替代混合负载的持续 UID。启动至结束后确切第四事件列无 Sleep/Wake/DarkWake，宿主交流电 100%。原始报告、线程/时钟/负载/资源记录、最终副本、电源及审核 SHA 收据在 .local/hyper-runtime-driver-probe-20261002/。

本轮 Redis PING 窗口最大 RTT 130.088ms，管理窗口最大 RTT 119.721ms，客户端调度间隙最大 148.701ms；混合窗口独立 Mac/Linux 等待间隙最大 41.542/139.446ms，最终 SLOWLOG 最大 EVAL 65.166ms，Redis 拒绝连接和错误响应为零。成功查询不输出 bulk snapshot，因此本轮没有实际失败 query 的 driver 时间线、完整 missed-record/record_ns 汇总或观测开销标定。没有复现业务超时，仍不能解释以前的 503。顺序线程累计值、时钟间隙和时间相关不等于一次持续暂停或根因证明。

为继续定位，准备一小时同一诊断产物的独立 r7。先归档并核对已完成且审核通过的公开 Admission r4、r6a 及其 peer 的资源、Secret 配置、当前日志、Pod UID、原副本数及 SHA，再只将这四个 namespace 的 Deployment 缩至零，保留 namespace 和恢复收据。其余 194 个 Pod UID 前后完全保持，正式和所有失败诊断现场未改；两 worker 恢复至 93/92 个活动 Pod、各 17/18 个空位。这是容量整理及环境变化，不能解释以前 503 或作为稳定性修复。收据在新目录 completed-fixture-capacity-archive/。

r7a 首次预检将新输出目录名误写为镜像名，尚无含 workers 的负载行，已对精确自有 child 发出 SIGINT。完整预检、日志、Pod/Redis、clock-end、停止及资源归档保留于 preflight-image-name-regression/；零混合业务请求，不是新的产品请求失败。旧 runner/child/host/guard 及自有 Helm 均已退出，精确两个预检 namespace 归档后缩零，其他 194 个 Pod UID 保持。新 runner 明确使用原已核对镜像名，并在启动 child 前要求其与独立镜像收据和实际二进制摘要一致。

独立 r7b 于 UTC 2026-10-02T14:58:16.061776+00:00 启动，目录 .local/hyper-runtime-driver-probe-long-20261002/，namespace rgnix-hyper-runtime-diagnostic-20261002-r7b。同一 668952 镜像和 r5 冻结 harness 不变，1169 个 scratch 源码文件与 r6 逐文件相同，不新构建、不改产品或公开 QA。完整 Gateway 预检后最多 3600 秒原六混合 workers、600 秒事件/100 路由，200ms/并发限制/失败关闭/不重放保持。100ms 线程观察最多 4200 秒，两个独立时钟和 runner 最多 5400 秒，自有 stop marker 和成功结束停止握手保持。UTC 14:58:54 启动核对仍在 Gateway 预检、零 workers 行，自有 runner/child/guard/host 与交流电、防睡眠断言已核对。这只是阶段快照，实际开始和结束必须按最新 process、含 workers/final 行及停止收据判定。

r6 的完成只属于诊断产物有限窗口，不是正式发行资格。此前正式 12.3 小时和 r1/r2/r3a/r5a 业务失败仍有效，根因未确认。默认 Hyper 保持，24 小时资格及当前正式产物性能复核仍未通过；一小时诊断也不能代替它们，不跳到性能。

## 直接 Redis driver 一小时诊断审核与六小时独立观测 — 2026-10-02

r7b 已于 UTC 16:07:03 结束并人工审核。实际混合负载 3600.109079 秒、608,153 次成功、零请求错误、六次插件发布；完整报告 complete=true、failure=null，全部 118 项检查及其中 10 项 Admission 通过。持续副本实际交付 logs 605,558 条、spans 1,211,116 条，成功结束交付、队列排空、无丢弃增量门槛通过。负载门槛后按握手先停止线程观察器，再执行计划滚动重启和 Admission；线程及两个独立时钟均 stopped=true、exit 0，无设施错误，自有 runner/child/observers/guard 已实际退出。

150 个资源样本包含负载及结束交付排空阶段，持续 Pod gateway-78db9f659c-6c86v/UID 58ef9f2f-ac54-458f-ae47-b2a8d2147305 全部一致，实际 Hyper/668952 诊断产物、进程启动时间保持。RSS 41.7–55.4 MB、最后 41.8 MB，FD 最高 104、最后 98，unavailable_closed=0。最终两个计划滚动后的副本实际 Hyper/ready/同一二进制摘要/零重启再次核对；这些最终 UID 不是混合负载持续 UID。完整服务/native TID/NSpid 映射、原始 load/sample/thread/clock/follower、最终 Pod/Redis、停止及 SHA 收据保留于 .local/hyper-runtime-driver-probe-long-20261002/review-summary.json 和 review-evidence-sha256.json。

PING 报告窗口最大 RTT 76.776ms、管理报告窗口最大 RTT 46.910ms、客户端调度间隙最大 220.797ms；混合窗口独立 Mac/Linux 等待间隙最大 43.947/177.740ms，最终 SLOWLOG 最大 EVAL 36.864ms，Redis 拒绝连接和错误响应为零。顺序累计线程读数和时钟间隙不是一次连续停顿或根因证明。线程和 Linux 时钟实际未覆盖初始 3.822/3.368 秒。启动至结束后核对确切第四事件列没有 Sleep/Wake/DarkWake，宿主交流电 100%。本轮没有失败 query，成功查询没有 bulk snapshot，因而没有实际失败 driver 时间线、完整 missed-record/record_ns 汇总或完整观测开销标定。健康的一小时不能解释旧 503，也不能把观测包装当作稳定性修复。

继续使用同一镜像、同一 1169 文件冻结 scratch 源码及同一 r5 harness，独立 r8a 于 UTC 2026-10-02T16:21:02.651108+00:00 启动，目录 .local/hyper-runtime-driver-probe-six-hour-20261002/，namespace rgnix-hyper-runtime-diagnostic-20261002-r8a。没有新构建，没有改产品、公开 QA、共享 Redis registry 或旧失败证据。镜像 Docker ID 10e3240fdd0d94071fdeed82a6f99e6cb87cd8d255c6c4eacecdcffface4a6d3 及实际二进制 SHA66895251b21d0a47c40c45e10b72d7138ab52a7c4a2ddc825418b41635cd017c 重新直接核对；每个实际 Pod 仍须单独验证。原 14 共享限流、最终镜像和真实 TCP 生命周期回归是同一精确产物的既有局部证据，不是新的业务资格。

完整 Gateway 预检后最多 21,600 秒原六 HTTP/TLS/RGL/body 混合 workers，600 秒事件间隔/100 路由，200ms/并发限制/失败关闭/不重放保持，持续 Pod 不重启、仅另一副本替换及插件发布/TLS 轮换。PING50/s、admin5/s、同进程 scheduler、独立 host/Linux clocks 和 100ms 精确 owned Gateway/Redis 线程观察保持。线程最长 22,200 秒、clocks/runner 最长 25,200 秒，成功交付门槛后的停止握手及自有 stop marker 不变。此次启动核对仍处于 Gateway 预检，零 workers 行，不能把 RUNNING 当作混合负载开始。实际 UID、node PID/start_ticks/NSpid、poll/wake TID 和初始覆盖缺口必须按本轮记录核对。

启动前仅归档已完成且审核通过的 r7b 和 peer 两个 namespace 的资源、Secret 配置、当前日志、Pod UID、原副本数和 SHA，独立核对后 Deployment 缩零，保留 namespace 和恢复收据。其他 194 个 Pod UID 前后完全保持，所有正式/失败诊断及其他任务未改。两 worker 活动 Pod 93/92、空位 17/18；这是容量及环境变化，不是旧 503 根因或稳定性修复。收据保存在新目录 completed-fixture-capacity-archive/。

直接 driver 包装每次 poll 的转发 waker 分配、ring 争用/覆盖、日志 dump 都可能增加观测开销；record_ns/snapshot_ns 不是全部开销。idle gap、wake 至 poll 间隔不是 OS runqueue 延迟，连接创建者不是每次 driver 执行证据，失败 dump 可能影响随后并发。新六小时观测旨在保留真实失败 query 的直接时间线，不能代替正式 24 小时资格；若没有失败，仍不能据此确认根因。此前正式 12.3 小时及 r1/r2/r3a/r5a 业务失败持续有效。默认 Hyper 保持，不跳性能、不放宽预算或重放。

## 直接 Redis driver 六小时诊断 r8a 实际失败审核 — 2026-10-02

前述 UTC16:21 记录是启动阶段快照。r8a 已于 UTC16:34:19 退出，未完成六小时。真实 workers 最终负载 459.797460 秒、77,207 次成功、四次 HTTP503：一次 TLS GET /、一次 GET /plugin、两次 POST /plugin；未重放。105 项预检通过，混合负载门槛失败，完成一次插件发布/TLS 轮换/另一副本替换。unavailable_closed=4，失败耗时总和 0.870575462 秒，四条实际访问日志均未选择上游。成功结束 OTLP 交付/排空、线程停止握手后计划重启与 Admission 门槛没有执行；这与成功请求阶段后的计划停止不同。独立解析原始 JSON dict/workers 和日志/指标核对，不把 probe 计为请求。

三条实际错误是 redis_timeout/query，elapsed_us=203768/233577/212231，cache_wait_us=0、query_started_us=8、reused=true，共用 connection_id=0xffff576060d0；第四条是 redis_timeout/connect，elapsed_us=201796、query_started_us=None、reused=false、connection_id=0x0，没有本轮旧 driver 的 query_begin。后者属于重建连接阶段，不能把四次都写成旧连接查询超时。

首次保留到真实失败 driver 时间线：三份 query 快照和一份最终 Drop 入口快照的所有原始 chunks 完整、无重复或冲突，独立重组再次核对。各快照最多 2048 条，合并尾部 2058 条，sequence 579995–582052 连续；每份均累计 missed_records=118，早期 ring 覆盖及这些争用丢记录仍存在，不能声称整个运行无丢失。driver_id=1 的尾部 481 次实际 poll 均在持续 Pod 的 native TID15 上，已由完整服务名映射到 HTTP8080-0/node TID204103/runtime_threads=1，属于实际 poll 执行证据，不再仅凭连接创建者推断。

失败 query ID76005/76006/76007 分别在 UTC16:33:51.559618/.571341/.604804 开始；query_failed 分别在 .793187/.775101/.817027。最后一个 Pending 在 .607094，下一 wake 在 .903171，下一 poll 在 .906725，即 Pending 至后续 poll 299.630ms、至 wake 296.076ms，而这次 wake 入口至 poll 3.554ms。连续尾部在该间隙内没有 wake/poll 记录；Pending 仍可能等待 socket/channel，没有长时间未处理 wake 的记录，不等于已证明 driver 在 OS runqueue 上等待了 300ms。包装器不记录逐请求网络发送/接收或 Redis 执行时间，query_begin 位于 query_async 入队前，wake 时间位于转发前；wall 时间由单次墙钟锚点和单调时间偏移推导。

Redis 在不同 Kind 节点 worker，Gateway 在 worker2，同属 OrbStack。Redis 主线程在 UTC16:33:51.702 和 .785 的顺序读数是 R 且 CPU 累计不增长；随后 .785–.886 约101ms 读数区间的 runqueue 等待记账增加306.213ms。driver 实际执行线程在覆盖三次查询失败的约111ms 读数区间 CPU/等待增加4.921/56.732ms、major faults+1；HTTP8080-1 major faults+5，TLS8443-1 在 .837 被采到 D，随后区间 major faults+3。这些累计记账可能滞后、顺序读取不是原子快照，不能相加成一次暂停、把某次缺页当根因或对应某条 EVAL。完整 raw/bracket/deltas 保留。

失败正负4秒报告窗口 PING 最大335.140ms、管理异常 RTT最大139.784ms、heartbeat最高0.325秒、客户端 scheduler gap最大175.340ms、独立 Mac/Linux wait最大24.858/168.099ms。管理汇总窗口最大10.843ms属于另一报告粒度，不覆盖异常 RTT 的逐条读数。Redis 最终 EVAL SLOWLOG最大43.334ms，实际失败秒没有达到阈值的 EVAL，不能据此说未执行或排除客户端/网络/运行时/Redis 调度等待。Redis 拒绝连接/错误响应/重启为零；Gateway/Redis/origin cgroup CPU节流及OOM为零，pressure不可用不能记零。

同一持续 Pod gateway-6d8794547c-f68bm/UID96aba5c8-ce65-4a6d-bdd8-58c329469b05，16个资源样本身份、进程启动时间保持，实际 Hyper/668952/零重启；RSS44.2–53.7MB、最后45.6MB，FD最高105、最后104。失败后晚采样 RSS43.9MB/FD89分开保留；两个现存 Gateway 的实际 binary 摘要也再次独立核对。失败后 logs76011/spans152018、export错误0、queue0，未执行成功结束交付门槛，不能算完整 OTLP 长测通过。两节点 thread-end 和两个 clock-end 均 stopped=true/exit0/无设施错误，所有自有本地和远端观察进程及guard已退出。线程/Linux初始覆盖缺口7.135/6.533秒。确切第四事件列从启动到结束后无 Sleep/Wake/DarkWake、AC100%。

实际 bulk dump_us=47530/5251/2755，admission_completed_us及原latency指标在dump前，HTTP响应在诊断日志后；第一份失败输出可能扰动后续并发及重建连接超时。record_ns约68ms是461秒内标量record函数累计，不包含全部包装/分配/forward wake/日志开销，不能当完整观测开销。最终快照只标记driver Drop入口，inner future随后销毁。证据在 .local/hyper-runtime-driver-probe-six-hour-20261002/failure-summary.json、failure-driver-review.json、failure-driver-chunks.log、driver-snapshots.json、failure-independent-verification.json、failure-evidence-sha256.json 及原始 load/sample/thread/clock/follower/final Redis/Pod 中保留。审核脚本首次误按日志字段顺序筛选的断言失败已归档 review-parser-attempt-1，随后修正 selector；没有新增业务运行，不是新的产品故障。

根因仍未确认。直接执行线程、无 wake/poll 的尾部及 Redis CPU 不增长/等待记账是更具体的本轮关联证据，尚未分离 Redis/VM调度、网络和客户端运行时就绪，不能解释旧正式两次或最初六次503。没有改产品、默认 Hyper、200ms预算、并发限制、失败关闭或重放策略。此前 r6/r7 有限健康窗口仍有效，但不构成修复或正式资格。

下一步仅准备精确 owned 调度和 socket 就绪事件观测，目录 .local/hyper-runtime-scheduler-events-20261002/probe-plan.json 为只读可行性审核，尚未实现、启用 trace 或运行新诊断。当前内核有 sched_switch/wakeup/TCP状态与重传 tracefs 格式及 BTF，节点未发现 perf/bpftrace/bpftool。全局 tracing_on=1/current_tracer=nop 是既有共享状态，不能清空或改动。先以唯一隔离 trace instance 和自有 marker 校准 kernel event PID 到 node/container TID、时钟对齐、丢失计数、开销及停止清理，再安排有限因果观测；不直接按 node NSpid 猜内核全局PID，不改永久参数或其他任务。尚无逐请求网络/EVAL因果证据，不凭当前时间线迁移driver或修改产品，也不继续单纯扩大健康时长。正式12.3小时及所有旧业务失败保留，正式24小时资格和随后串行性能/A/A仍待完成。

## 隔离调度事件设施与有限 r9a 诊断启动 — 2026-10-02

上述只读可行性是 UTC17:00 的阶段快照。现已在全新 scratch 实现唯一 trace instance 观察器，并完成自有睡眠/CPU/pipe读写 marker 的真实回归和独立原始证据复核。首次精确过滤未收到 marker 事件，已归档 trace-observer-attempt-1：Docker 主机 /proc 的 NSpid 首项仍不是 trace 事件 PID，不能将该映射称为内核事件身份。后续自有 marker 的旧 syscall 文本格式断言失败也归档 trace-observer-attempt-2，业务请求均为零。该轮发现内核默认 syscall 显示会复制用户缓冲内容；仅对自有 marker 的原始记录保留，当前独立实例必须将 syscall_user_buf_size 设为0，并实查写入 marker 内容不出现在采集结果后，才可用于业务观察。没有改共享全局设置。

最终 marker 回归通过，独立核对102次sched_switch、7次sched_wakeup、六次各自pipe read/write的真实调用与返回、51个按自有线程打开的sched_switch原始样本，停止与清理均通过。唯一 marker 的 node PID206241、Docker 主机 PID2554483和trace事件PID1959022不同；先按准确node TID打开短时per-task事件句柄，再比较sample的node PID/TID与raw common_pid/prev_pid，获得trace过滤身份。唯一comm只用于该自有marker的交叉校准，不按业务线程截短comm或创建顺序推断。此前marker的trace mono时间与Python单调时间标记相差43–95微秒，属于本机校准范围。另在仍保留的r8精确UID/container/node PID/start_ticks上做两秒只读身份复核，四个Hyper线程和Redis主线程均核对到实际trace TID；该映射只属于r8，不能拼接进新的负载或重建此前失败。

最终marker观察器用户/系统CPU累计0.014379/0.016298秒、输出计时647420ns；这些不包括内核hook/filter、目标线程、全部缓冲和调度开销，不是完整观测开销标定。实例使用mono时钟、每CPU256KiB、100ms批读，保存sequence、逐CPUoverrun/commit overrun/dropped及观察器CPU/输出耗时；丢失记录或身份变更单独记为设施失败，并保存计数。当前只记录准确owned TID的调度切换/wake和标量syscall参数/返回、FD/socket端点元数据，不读取命令、URL、凭据或body。syscall成功及长度并不等于某个Redis EVAL的网络或执行成功，epoll等待返回也没有逐FD就绪结果。所有自有marker进程/短时事件句柄/instance已停止清理，共享tracing_on、tracer、clock、event enable/filter和用户缓冲设置前后保持，没有安装perf/bpftrace/bpftool，没有改sysctl、永久调度/电源参数或其他任务。

新独立r9a在UTC17:47:23.780487启动，目录.local/hyper-runtime-scheduler-diagnosis-20261002/、namespace rgnix-hyper-runtime-diagnostic-20261002-r9a，启动核对仍处于Gateway预检，没有workers混合负载行，实际trace尚未启用。复用同一668952诊断binary、原镜像DockerID10e3240fdd0d94071fdeed82a6f99e6cb87cd8d255c6c4eacecdcffface4a6d3、1169文件逐SHA相同的冻结源码及3ee551dff9a813c3f79f1353b553727796ede5ed858788b52197cba6d49266c0 harness，不新构建、不改正式69bc/dbf046/ca46或公开QA。启动前只读容量核对两个worker各13空位，没有缩零或重启任何失败现场，没有停止其他任务。完整预检后最多900秒原六HTTP/TLS/RGL/body混合workers、600秒事件/100路由；200ms、permit、clone/taskhandle生命周期、连接身份清理、失败关闭、不重放和持续Pod不重启保持。

本轮线程、Linux时钟及trace实际覆盖须按真实启动时间分别记录。先核对新精确UID/containerID/node PID/start_ticks/NSpid、完整服务名到native TID，再由per-task事件独立核对五个trace TID后才启动唯一instance，不能沿用r8映射。成功请求/OTLP交付排空后先停止并验证trace、再停止线程、最后ack允许原计划滚动重启与Admission；失败也保留全部raw driver chunks/trace loss/线程累计值/probes/clocks/电源和停止收据。线程/trace最多1200秒、clocks/runner最多2400秒。证据在.local/hyper-runtime-scheduler-events-20261002/review-summary.json、trace-observer-independent-verification.json、review-evidence-sha256.json及新目录start/process/source/harness/image/capacity中。

这是观测设施修正与新的有限诊断预检，不是产品稳定性修复。r8四次503、正式12.3小时失败及所有旧业务失败仍有效，根因未确认；默认Hyper保持，正式24小时、趋势/电源审核及随后同binary完整性能/A/A仍待完成。不得把启动RUNNING、marker回归或此后有限健康窗口记为完整验收。

## r9a 设施中断审核与 r9b 独立复验 — 2026-10-02

前述 r9a 启动记录是阶段快照。runner于UTC17:56:32退出：一次人工采集内容审核的正则将内核空截断占位符 `(, ...)` 误判为缓冲内容，触发显式停止自有tracer，随后runner按设施失联中断。原误报和停止记录保留，独立扫描全部2,228,888条事件，170,276处匹配均为固定空占位符，实例syscall_user_buf_size实查为0，没有匹配到实际缓冲字节。这是审核条件误报，不是新的产品503，也不算完整诊断通过。

105项Gateway预检通过。最后实际workers报告只覆盖180.094秒、30,483成功、0失败，final=false，没有最终请求汇总。中断时原harness终止本地kubectl exec，没有停止远端python负载。核对自有origin Pod UID和PID76/start_ticks23494965后，SIGINT未完全停止，随后仅对该精确身份SIGTERM，于UTC18:02:31确认/proc消失。未接收尾部的实际请求成功/失败数未知，不能把180秒或30,483当作整轮，也不能假设尾部零错误。原始报告、trace、workers、样本、晚取日志/指标、停止与独立核对证据在.local/hyper-runtime-scheduler-diagnosis-20261002/review-summary.json及review-evidence-sha256.json保留，原FAILED字段不改成功。

7个部分资源样本的持续UID74cae39e-7469-483f-8581-562bda5bc70c和进程启动时间保持，后补两个Gateway实际H/668952/零重启核对。trace每CPU overrun/commit overrun/dropped events均0，停止、实例移除和全局状态保持通过；这不补齐停止后的业务窗口。线程与两个clock stopped=true/exit0，guard释放；确切第四事件列启动至结束后无Sleep/Wake/DarkWake，AC100%。成功结束OTLP交付/排空、成功线程握手后的计划重启及Admission未执行。observer CPU仅为user1.040667/system1.833701秒，不含内核hooks/filter及目标扰动，不能当完整观测开销。

新独立r9b目录.local/hyper-runtime-scheduler-diagnosis-r9b-20261002仅修审核和停止设施：空占位符允许、真实自有marker字节拒绝；负载probe:start增加PID/start_ticks/PID namespace/命令身份，中断时核对精确Pod UID/container及该身份再停止远端自有负载。真实OrbStack双marker回归验证过期身份拒绝不发信号、正确身份停止、另一个marker保持运行、重复停止安全；r9a全部原始事件重审通过，模拟真实缓冲字节被拒绝。facility-regressions.json为准，该回归业务请求为0。

r9b于UTC18:08:30启动，全新namespace rgnix-hyper-runtime-diagnostic-20261002-r9b，runner88550/child88562/guard88560/hostclock88561。UTC18:09:50实查自有命令仍在Gateway预检，尚无workers行、tracer未启动，AC100%及自有两条防睡眠断言通过。这是启动快照，不能把RUNNING当负载开始或资格。1169个scratch源码文件和冻结Gateway harness SHA3ee551保持，只改外围runner、load probe身份元数据与审核/停止设施；镜像Docker10e3240、实际binary668952及三个Kind imageID0183e61再次核对。正式69bc/dbf046、ca46及公开QA未改。启动前两worker空位9/8，仅只读检查，r9a及全部正式/失败现场保留，没有缩零或停止其他任务。

沿用900秒上限、六HTTP/TLS/RGL/body workers、200ms、原permit/clone/taskhandle生命周期、身份清理、失败关闭及不重放；之后仍须OTLP交付排空、先停止trace与线程握手再计划滚动及Admission。新UID/TID/per-task trace身份必须实查，不能复用r9a映射。正式12.3小时与r8等旧业务失败仍有效，根因未确认，默认Hyper保持，正式24小时及随后同binary完整性能/A/A待完成。


## r9b 完整有限诊断审核与调度状态判读 — 2026-10-02

前述 UTC18:08 的 r9b 记录是启动快照。该轮已于 UTC18:32:32 正常结束并独立审核，实际六 workers 混合窗口 900.108215 秒、152,031 次成功、零请求错误、两次插件发布。完整报告 complete=true、failure=null，全部 118 项检查及其中 10 项 Admission 通过。持续副本实际交付 logs151,322/spans302,644，成功结束交付、队列排空、无丢弃增量门槛通过；初始/最终日志导出3/151325、span导出6/302650，导出错误与部分成功计数保持零、最终 pending均零。unavailable_closed=0。这仅属于668952诊断产物有限窗口，不能覆盖正式12.3小时失败、r8或其他旧真实503。

60个资源样本包括结束OTLP交付阶段，持续gateway-57cb75d9c6-sntkz/UID1ebeea96-a1e5-4a40-a751-2cc8077ecf8b一致、实际Hyper/同一诊断镜像与进程启动时间保持。初始精确UID/container/native映射零重启，样本不存每次restartCount，不能将它写成逐样本重启计数。RSS42.6–49.4MB、最后43.9MB，FD最高104/最后98。最终两个计划滚动后的副本再次直接核对H/ready/668952/零重启，它们不是持续负载UID。负载期间Gateway cgroup/pressure/OOM未另存快照，计划替换后的新Pod计数不能补作旧持续Pod证明。

完整1,230,484,404字节raw trace已流式独立核对，9,475,899事件、153,326连续批次，无JSON截断或sequence缺口。每个统计窗口及最终所有CPU overrun/commit overrun/dropped events均零；实例userbufsize0，缓冲显示只有允许的空形式，没有匹配到实际内容。2,491个per-task原始perf样本独立解码，核对node PID/TID、raw common_pid/prev_pid到五个实际trace TID及完整服务映射，不从Docker-host PID或comm猜测。trace初始未覆盖9.409秒，线程/Linuxclock缺口4.005/3.460秒。没有失败query/bulk snapshot，因此没有实际失败的driver与调度因果时间线，也没有整轮driver ring未丢失或完整开销标定。

成功请求/OTLP门槛后先停止trace，再停止线程、最后ack允许原计划滚动和Admission；实际顺序UTC18:28:52最终请求、18:29:02交付后stop request、18:29:09 trace-end、18:29:14 thread-end、18:29:15 ack。trace/thread及两clock均stopped=true/exit0、无设施错误，instance移除且global前后与复查状态保持。精确origin UID/container的load PID75和Linuxclock101、node observers196377/196316以及全部七个自有本地命令实际退出；正常负载已结束，stop receipt signal=null，没有仅凭本地exec结束假设远端停止。guard已释放，确切第四事件列启动至结束后无Sleep/Wake/DarkWake，宿主AC100%。原始证据和审核收据在.local/hyper-runtime-scheduler-diagnosis-r9b-20261002/review-summary.json、review-independent-verification.json、review-trace-content-and-loss.json、review-evidence-sha256.json；初次review误用不存在pod_status字段的副本保留，属于审核器错误，没有额外业务运行。

PING窗口最大69.015ms、admin窗口51.087ms、heartbeat0.285秒、client scheduler73.832ms；混合报告窗口独立Mac/Linux等待间隙20.924/76.986ms，最终SLOWLOG最大EVAL14.291ms、Redis拒绝/错误响应零。这些健康读数及顺序累计CPU/runqueue/fault不是旧故障原因。observer user/system CPU4.903983/8.308908秒、输出计时10.638475307秒、批次输出间隙最高129.552ms均是局部观测值，不含全部内核hook/filter、分配和目标扰动，也不等同目标暂停。

新离线判读器用保留的真实自有CPU/sleep/I/O marker六个周期核对：约15–18ms睡眠中blocked_S实际14.980–17.912ms，wake到switch-in可运行等待3–60us，未将整个睡眠算为runqueue等待。来源raw、精确trace身份、摘要及初次JSONL摘要/事件文本摘要范围断言修正均保留于.local/hyper-runtime-scheduler-state-analysis-20261002/。这没有新业务请求、没有重建r8失败。判读只接受完整switch/wake区间，记录首尾不完整与微秒时间戳同值的排序边界；R/R+抢占区间和S/D阻塞区间分开，标量syscall仍不能证明具体Redis EVAL的发送/接收/执行或epoll逐FD就绪。没有因果证据前不改产品、不迁移driver。

仅归档已正常结束并审核通过的r9b及peer两namespace资源、Secret、当前日志、UID、原Deployment副本数和SHA，独立核对后缩零，原namespace/恢复收据和全部raw保留。其他212个Pod UID前后相同，所有正式与失败现场及其他任务保持。新目录.local/hyper-runtime-scheduler-diagnosis-r10-20261002/completed-fixture-capacity-archive/保存收据；两worker活动101/102、空位9/8。容量和环境变化不是稳定性修复。

独立r10a于UTC19:08:50启动，namespace rgnix-hyper-runtime-diagnostic-20261002-r10a，仍最多900秒六HTTP/TLS/RGL/body workers、600秒事件/100路由、原200ms/permit/clone/taskhandle生命周期、身份清理、失败关闭、不重放。1169文件冻结源码、668952镜像及原harness/trace设施保持，只换独立输出/namespace/stop路径；归一化AST与r9b一致。目标是在真实失败时使用已校准判读区分阻塞与可运行等待，没有扩大健康窗口或改正式69bc/dbf046/ca46/公开QA。UTC19:09:27实查runner3222/child3227/guard3225/host3226仍在完整Gateway预检，尚无workers、trace未启动，AC100%和自有两防睡眠断言通过。这是阶段快照，必须按最新process、真实workers/final、新精确UID/TID及停止收据审核；不得沿用r9b身份。正式24小时、人工趋势/电源审核及之后同正式binary完整性能/A/A仍待完成，默认Hyper保持。


## r10a 有限窗口完成审核与下一处证据缺口 — 2026-10-02

前述 r10a UTC19:09 预检记录是历史快照。本轮于 UTC19:32:21 正常结束，真实六 workers 最终窗口 900.108328 秒、152,337 次成功、零请求错误、两次插件发布。complete=true、failure=null，全部118项检查及其中10项Admission通过。持续副本实际交付logs151,629/spans303,258，成功结束交付、队列排空、无丢弃增量门槛通过；初始/最终导出0/151629与0/303258，错误/部分成功保持零、最终pending均零。零值dropped_total系列未出现，不能把null写成测得绝对零；实际集成门槛已执行。unavailable_closed=0。这是668952诊断产物的有限窗口，正式12.3小时失败、r8及全部旧真实503保持，正式24小时资格仍未通过。

63个资源样本包括结束OTLP阶段，持续gateway-747df6959f-b2s4v/UIDd8f7c955-a681-40f3-ad74-729cdb6663ea、实际Hyper、诊断镜像和进程启动时间均保持。初始精确UID/native映射零重启；样本没有逐次restartCount，不能声称逐样本测得零重启。RSS44.8–57.4MB、最后46.2MB，FD最高104/最后98。负载期间Gateway cgroup/pressure/OOM未另存收据，最终计划滚动副本不能补作旧持续Pod证据。两个最终副本已独立核对实际H/ready/668952/零重启，UIDd0c06890...与249910c7...属于计划替换后的副本。

完整raw追踪流独立审核：10,399,961事件、167,539连续批次，SHA256为0b85b28d3fb81cf983b29f27120a9a6db4d03d94f204b2bd7a9ba15ed8b12f06。所有统计窗口及最终per-CPU overrun/commit overrun/dropped均零，无JSON截断、sequence缺口或实际缓冲内容显示；实例userbufsize0强制readback保持。2,873个原始per-task perf样本独立解码，将四服务native16/17/18/19、node225170/225171/225172/225173对应实际trace2175798/2175799/2175800/2175801，Redis node222695对应trace2161043。这些是本轮历史身份，不能给新的Pod沿用。trace/线程/Linuxclock初始覆盖缺口12.026/6.715/6.322秒。

请求与OTLP门槛后依次停止trace、线程，再ack允许原计划滚动和Admission。所有trace/thread/两clock stopped=true、exit0、无设施错误；instance已移除，共享global前后及复查状态保持。独立核对七个本地自有进程、node observers225611/225545、精确origin UID8336b680...的load77/start23979776与Linuxclock106的/proc均不存在；正常负载停止signal=null，guard已释放。启动至结束后UTC19:48复查确切第四事件列无Sleep/Wake/DarkWake，AC100%。原始报告、追踪、映射、停止及电源证据和审核SHA保存在.local/hyper-runtime-scheduler-diagnosis-r10-20261002/。

PING报告最大119.147ms、admin40.742ms、heartbeat0.296秒、client scheduler71.977ms，混合报告Mac/Linux等待间隙18.133/64.695ms，最终SLOWLOG最大EVAL24.446ms、Redis拒绝/错误响应零。trace observer CPU user4.956989/system8.662641秒、输出计时11.018844394秒、批次输出间隙151.066ms是局部观测值，不包含全部内核hook/filter/分配/目标成本，也不等同目标暂停。

已离线应用真实marker验证过的调度状态判读器，处理3,918,908项目标调度记录，保留475,551个同微秒时间戳排序边界、首尾不完整状态和异常计数。它区分S/D阻塞与wake后或R/R+切出后的可运行区间，健康窗口没有可关联的失败query，不从长阻塞区间、创建者、单syscall/epoll返回或累计记账推故障根因。本轮没有driver失败bulk，不能宣布整轮driver ring无丢失、完整开销标定或具体失败因果链。

下一步已做只读源码可行性审核，计划与摘要在.local/hyper-runtime-query-stage-probe-20261002/probe-plan.json，状态NOT_IMPLEMENTED_OR_RUNNING，没有新builder/runner/tracer。当前query_begin在query_async入队前，现有driver Pending和标量syscall不能确定哪个失败查询已进入codec或收到解析响应。下一独立scratch诊断拟只传播driver/query标量ID，记录入队、dequeue、codec接受/flush、解析响应分配与receiver交付/取消；不记录命令、凭据或payload，codec/flush也不冒称Redis执行或数据包交付证据。先用自有真实TCP marker核对正常回复、延迟回复超时、并发clone/取消、连接生命周期及清理，与668952基线行为比较，再独立核对新诊断产物并准备同900秒预算。保留原200ms、permit、spawn/taskhandle、身份清理、失败关闭、不重放。观测改变只在全新scratch，旧冻结证据与正式产品/公开QA保持；不因健康窗口扩大时长、不迁移产品driver、不跳到正式资格或性能。默认Hyper保持，最终正式24小时与随后同正式binary完整性能/A/A仍待完成。

## r11a 新逐查询阶段有限诊断与受控调度关联 — 2026-10-02

r11a 于 UTC21:25:04 正常结束并独立审核。实际六 workers 最终窗口900.119143秒、152,131次成功、零请求错误、两次插件发布；完整118项检查和其中10项Admission通过。持续副本实际交付日志148,359/span296,718，成功结束交付、排空、无丢弃增量门槛通过，导出错误/部分成功无增量、最终pending零。dropped_total系列缺省为null，不能写成测得绝对零。unavailable_closed=0。本轮使用新4a300a逐查询诊断产物，不能拼接668952窗口或覆盖正式12.3小时、r8和全部旧真实503，正式24小时仍未通过。

持续gateway-6987654b54-bb8p7/UID872cfa73-3051-4eba-8afb-31f2f297e834实际Hyper/4a300a、镜像和进程启动时间在59个资源样本中一致，RSS44.2–56.5MB、最后45.9MB，FD最高105/最后98。新增31次精确UID/container/nodePID/start/argv0守卫资源读取覆盖Gateway、Redis及自有origin，实际restartCount均零，cpu.stat节流及memory.events OOM计数均零；pressure文件不可用，不能记零。初始资源覆盖缺口18.817秒，单次controller约567–843ms、累计20.734秒属于顺序API读取观察器成本，不是目标暂停或完整扰动开销。负载及OTLP后、计划替换前保存了持续UID的最终资源快照。

1,200,730,064字节完整raw trace独立审核，共9,298,143事件、149,765连续批次，SHA256为66af771d24870995215def89205e8bd4befc2c7541b79dc9be8440670c6300a7；全部统计窗口与最终per-CPU丢失计数零，无JSON截断、sequence缺口或实际缓冲内容。实例userbufsize0强制readback保持。1,580个per-task原始perf样本独立解码：native14/15/16/17→node237319/237320/237321/237322→trace2430743/2430744/2430745/2430746，Redis node204849→trace2415099。它们仅是r11历史身份。trace/线程/Linuxclock初始缺口8.103/2.838/2.359秒。traceCPU user4.709415/system7.750530秒、输出计时10.740136082秒、批次输出间隙159.137ms是部分观测成本，不包含全部hook/filter/目标成本。

实际最终请求UTC21:21:37，OTLP后stop request21:21:47、持续资源快照与trace-end21:21:53、thread-end21:21:58、ack21:21:59后才允许原计划滚动及Admission。trace/thread/两clock均stopped=true/exit0、无设施错误，独立实例移除、global前后及复查保持。按各自所属节点实查observer进程退出，精确origin UID的load75/start24657493与Linux104的/proc不存在；正常负载stop signal=null，自有本地命令、followers/exec及guard都已退出。最终计划滚动的两个Pod另行直接核对H/ready/4a300a/零重启，不能替代持续负载UID。启动至UTC21:36复查确切第四电源事件列无Sleep/Wake/DarkWake，AC100%。完整审核和SHA在.local/hyper-runtime-query-stage-diagnosis-20261002/review-summary.json、review-independent-verification.json、review-resources.json、review-followers-stop-verification.json、review-evidence-sha256.json。

本轮PING最大46.006ms、admin40.445ms、heartbeat0.282秒、client scheduler68.860ms，混合Mac/Linux等待间隙20.316/74.301ms；最终EVAL SLOWLOG最大14.498ms，Redis拒绝和错误响应零。没有真实失败query或bulk snapshot，不能据此声明整轮driver ring无丢失、完整开销标定或故障因果链。

随后完成自有真实TCP查询阶段与内核调度关联marker。只修改独立marker示例，4a300a的运行/vendor源码保持；原200ms延迟回复超时、30ms并发clone取消、取消后FIFO占位、survivor正确回复、untagged pipeline、最后clone Drop/socket EOF及replacement均通过，6个真实ECHO、2连接结束，无重放。实际poll node/native246095经97个per-task raw样本对应trace2536259，不由创建者或comm推断。用CLOCK_MONOTONIC前后边界夹住相对Instant读数，锚点交集宽1,833ns；已知held区间内的保守完整线程片段blocked_S200.834ms、runnable70us、running16us，没有把整个超时算成可运行调度等待。该current-thread同时运行marker客户端/服务端，因此线程状态不能等同某个future状态或证明Redis EVAL/数据包因果。

该marker72次sched_switch、36次wake及392标量syscall完整保留，userbufsize0、无实际内容或记录丢失；独立实例移除、global保持、所有自有进程和远端文件实际清理。CPU与输出计时仍是部分成本。准备阶段输入字段、Docker cp、既有/tmp noexec及远端身份字符串设施错误分别归档；一次未启动trace的marker因本地身份读取器错误被仅自有握手放行后正常结束，不算Gateway请求或产品503。审核器poll名称断言修正保留，没有重跑业务。证据在.local/hyper-runtime-query-stage-correlation-20261002/review-summary.json、cleanup-independent-verification.json、review-evidence-sha256.json。

发现真实Gateway的4a300a snapshot只导出SystemTime锚点及相对Instant，marker精确CLOCK_MONOTONIC对齐尚未进入运行二进制，因此本次没有直接重复健康Gateway窗口。全新scratch .local/hyper-runtime-query-clock-anchor-probe-20261002仅在snapshot用两次Linux CLOCK_MONOTONIC夹住一次elapsed读取，导出monotonic_anchor_low_ns/high_ns；缺失为null、停顿扩大区间，不猜点时间。事件/队列/FIFO/200ms/permit/clone/canonicalspawn/taskhandle/身份清理/失败关闭/不重放保持。另复用带有限握手和外部时钟夹取的marker示例，合计相对4a300a仅两个scratch意图路径改变，没有每事件新增时钟读取。新产物真实TCP内部/外部锚点、限流生命周期、默认H/显式H/P镜像门槛在下文单独记录，不能借用旧4a窗口作为新产物验证；正式binary、公开QA、旧冻结source/raw和失败现场保持。本次没有新容量处理、混合业务或性能运行。


新快照时钟边界已完成真实行为与产物核对。内部锚点字段在同一全新自有TCP marker中与独立外部CLOCK_MONOTONIC上下界核对，四个snapshot观测边界交集宽0ns、各内部夹取宽0–125ns；相同整数读数不代表物理精度或零误差，内核事件仍有1微秒格式化边界；200ms延迟回复、取消、FIFO、survivor与最后clone清理均通过，并重新用实际per-task样本对应新node/native与trace线程，未沿用旧身份。所有raw、首尾边界和标量loss/停止/清理收据在.local/hyper-runtime-query-clock-anchor-probe-20261002/，这仍不是Gateway失败查询或Redis执行证据。

新诊断binary SHA256为8435435edff97fd7afcbb2fb2acdd20ab1819dff524f23fb31f2790214d1dc59，image rgnix:hyper-query-clock-anchor-probe-20261002，实际镜像binary/ELF64 AArch64、默认Hyper/显式H/P真实HTTP/ready/metrics、新H/P各14共享限流边界以及实际Redis canonical BLPOP200ms/最后cloneDrop/replacement核对通过。原4a300a、668952与正式69bc/dbf046产物保持，shared registry/git未修改。新源码仅上述两个scratch意图路径，原生命周期/200ms/失败关闭/不重放保持；时钟夹取/事件/格式化/trace有观测成本，局部计时不能算完整开销。builder、marker/tracer/guard、私有Redis/monitor容器/远端文件及实例均已实查停止和清理，共享trace状态保持。

新镜像已载入三个Kind节点并逐一核对实际binary为8435435edff97fd7afcbb2fb2acdd20ab1819dff524f23fb31f2790214d1dc59，imageID47e950b857ff016ea086c17f8a76867a1fae81ddb137565e299b59815428d342，原4a300a/668952/正式镜像ID在每节点前后保持。新900秒Gateway诊断尚未准备或启动，未做容量处理或吞吐测试。下一步按实时容量、新namespace/持续Pod UID/container/nodePID/start/NSpid/native/per-task trace身份准备同900秒预算；必要时仅允许归档已完成并独立审核通过r11a及peer后释放其副本，所有失败现场和其他任务保持。snapshot null/宽区间/跨快照交集、微秒排序边界和首尾unknown须明确保留，无逐请求因果证据不改产品、不迁移driver或放宽限流。默认Hyper保持，正式24小时和随后同正式binary完整性能/A/A仍待完成。

## r12a 真实查询失败、单调时钟与完整调度片段审核 — 2026-10-02

前述8435435产物“尚未启动新Gateway”的记录是当时发布快照。r12a于UTC22:32:06独立启动，UTC22:42:57收到真实最终workers报告，runner于22:43:23退出。实际347.637985秒、58,391次成功、4次HTTP503：两个GET /plugin、两个POST /plugin，未重放，最终尾部已接收。105项Gateway预检通过，混合负载门槛真实失败，期间一次插件发布/TLS轮换及另一副本替换，未完成900秒。本轮8435435诊断不是正式产物资格，不能拼接4a300a/668952窗口；默认Hyper、正式12.3小时失败及全部旧真实503保持，正式24小时仍未通过。

1169文件冻结源码、两处scratch时钟/marker改动、新镜像/三个节点实际binary和原Gateway harness、线程/时钟/trace/只读资源hook独立核对保持。启动前容量不足，只归档已经完成并独立PASS审核的r11a及peer两namespace资源、Secret、当前日志、UID、原副本数和SHA，核对后缩零；其原namespace/恢复收据/raw/review保留，其他212个Pod UID前后相同。启动前两个worker空位9/8。这是容量环境变化，不是稳定性修复；r12失败现场未缩零或重启，没有停止其他任务或运行性能。

四条实际错误均为redis_timeout/query，elapsed_us599397/603756/636317/644251、cache_wait0、query_started24/9/2/3us、reused=true，同一connection_id0xffff46c490f0；unavailable_closed=4、耗时sum2.484171661秒，四条真实访问日志全部未选择上游。精确持续Pod gateway-55c9884c85-r6txd/UID5cfd04cd-8e12-4dcc-8ff2-f784c376a06b/nodeworker2 PID250790/start25198442/container88951113…保持。native15/16/17/18对应node250812/250813/250814/250815及实际trace2649131/2649132/2649133/2649134，Redis node248248/start25170653对应trace2634146；1398个per-task原始样本独立核对，不能复用于新Pod。保留尾部实际driver poll全部native15/HTTP8080-0，这是执行证据。

160条raw chunk独立重组四个query及一个final snapshot，每份2048记录，无缺失、重复或冲突。合并2085记录，三处保留sequence洞961814/961817/961819，累计missed529，evicted960328–960365，不能写整轮无丢失；失败查询开始后的保留seq962856–962941连续。五个snapshot的CLOCK_MONOTONIC边界宽166/83/41/209/208ns，交集252005757514348–252005757514389ns，观测宽41ns不代表物理精度，trace文本仍有1us边界及同时间排序不确定性。四个query54599/54600/54601/54602均进入原队列、driver dequeue、codec接受及本地flush-ready；两个receiver约206.3/206.2ms取消，另两个约644.2/636.3ms取消，随后原FIFO分配解析回复时四个receiver均已关闭。前两个取消到admission结果记录又间隔393.1/397.6ms，因此599–644ms不能当作Redis执行时间。codec/flush不是网络交付、FIFO本地关联不是逐EVAL执行证明，receiver取消不是远端取消。

完整switch-out状态→wake→switch-in原始链独立核对：Redis先S阻塞15.398ms，wake后可运行等待433.677ms才被调度，该区间覆盖实际查询开始及200ms预算；HTTP8080-0在receiver取消后D阻塞362.955ms、wake后可运行1.530ms，HTTP8080-1对应D345.628ms、可运行17.452ms。它们是本轮明确的OS线程状态，不是累计读数推算，也没有把整个等待写成runqueue。线程运行多个future，缺具体EVAL执行/网络交付及D阻塞来源，尚不能确定这四条请求的完整因果链、为何发生这些等待，或解释旧正式/诊断503。扩大后的离线故障上下文保留32,375个原始事件、12,438条调度记录、首尾unknown、32个同微秒排序边界；不强补缺失状态。

完整475,685,983字节raw trace共3,653,208事件、59,222连续批次，SHA256 e05777c725e24eaf38726213812398ec40b59798de9a1384ffd67f6fe47ee7c9。170个统计窗口及最终全部perCPU overrun/commit overrun/dropped零，无JSON截断/seq洞，独立instance userbufsize0且只有空display，没有实际buffer内容。trace/线程/Linuxclock初始缺口7.642/2.394/1.953秒。trace observer CPU user1.817617/system3.211160秒、输出5.119249234秒、最大批次输出间隙920.157ms是部分成本和观察器间隙，不等于目标暂停。bulk dump20.546/25.923/7.252/5.914ms位于admission metrics之后、response之前，可能扰动并发及重连；record累计约79.3ms及snapshot/CPU计时不含完整分配、wake转发、innerpoll、kernel hook/filter/目标成本。

12个资源样本持续UID/Hyper/8435435/image/process_start一致，RSS43.5–54.6MB、最后44.1MB，FD最高及最后104。11次精确守卫Gateway/Redis/origin资源读取逐次restartCount、CPU节流和OOM零，pressure文件不可用不记零；资源初始缺口18.389秒，controller542–797ms/合计7.458秒是串行API读取成本，非目标暂停。故障后晚取资源单独标记，不重建失败瞬间。Redis拒绝连接/命令错误零，最终SLOWLOG为空，不能从空列表给出EVAL耗时上界或说没有执行。正负4秒报告窗口PING541.399ms、admin异常1907.890ms、heartbeat0.576秒、client scheduler1208.330ms、Mac/Linux等待47.410/1762.416ms；这些报告有不同粒度，部分峰值早于查询开始，不按同时逐查询读数或根因解释。顺序累计CPU/runqueue/fault原始窗口与625条增量分别保留，记账延迟不冒充连续暂停。

负载实际final=true，正常remote load停止收据signal=null；精确origin UIDd4c9306d…/container6bf3e72f…的load76/start25200808/Linux102、所属node观察器251158/251221、七个本地自有进程及所有owned followers/exec均已实查退出。trace/thread/两clock stopped=true/noerror/exit0，独立instance移除/global前后及复查保持，自有guard释放。两个现存Gateway实际Hyper/ready/8435435/零重启另行核对，持续UID保留。成功结束OTLP交付/排空、成功handoff后的计划重启及Admission均未执行；故障后晚读日志54604/span109204、export errors0、pending0不能补作完整OTLP门槛通过。确切第四电源事件列从22:32启动至23:08复查无Sleep/Wake/DarkWake，宿主AC100%charged。

原始证据及独立审核在.local/hyper-runtime-query-clock-anchor-diagnosis-20261002/failure-summary.json、failure-driver-review.json、failure-scheduler-state-review.json、failure-stage-correlation-independent-verification.json、failure-thread-window-raw.json、failure-thread-bracket.json、failure-resources-review.json、review-independent-verification.json、review-trace-content-and-loss.json、review-followers-stop-verification.json及failure-evidence-sha256.json；162个列明证据文件重新读取核对摘要通过。只读审核器字段误用和Python摘要API兼容错误分别保留，无新增业务或产品503。下一步先审核逐请求网络/Redis执行以及D状态来源的剩余证据缺口与只读观测可行性，不重复健康窗口、扩大时长、迁移driver、放宽timeout、failopen或重放。没有因果证据不改产品；正式24小时及随后同正式binary完整性能/A/A仍待，跟进保留。

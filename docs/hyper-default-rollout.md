# Hyper 默认内核与验收状态

当前开发版本包含 Hyper 和 Pingora，按用户要求默认运行 Hyper，Chart 的默认选择也为 Hyper。显式选择 Pingora 可回退；`--no-default-features` 构建只包含 Pingora，默认使用 Pingora。此次默认切换不等于全部发行资格通过：24 小时长测尚未通过，串行负载复现共享 Redis 限流路径的 503，根因仍需定位。下表继续作为验收要求，历史 debug 测试及短时间 benchmark 不能代替这些证据。

| 门槛 | 必须取得的证据 |
|---|---|
| 内核选择与回退 | CLI、环境变量、Helm 一致；两种内核真实启动；旧 true/false 参数兼容；`check` 与 `serve` 拒绝相同的不兼容配置 |
| 发行产物 | 原生 Linux amd64/arm64 在 Bookworm 构建；最终 release 二进制跑完整行为及 Hyper 协议检查；镜像内二进制摘要与已测产物一致；两种内核在最终镜像中启动 |
| Kubernetes | 最终镜像跑 Ingress/Gateway 主流程；24 小时混合负载，持续发布插件、轮换 TLS 和替换 Pod；检查请求错误、内存、CPU、描述符、连接、许可和尾延迟趋势 |
| 性能资格 | 同一个发行二进制运行 Hyper/Pingora，交错比较；分别做同内核 A/A 校准。记录差异与波动，校准失败不宣称稳定提升或 NGINX 性能持平 |

CI 的 `artifact` 作业在两种原生架构上运行 `scripts/validate_artifact.sh`；`image` 作业把该产物装入 Bookworm 镜像，再运行 `scripts/image_smoke.py`。镜像摘要检查防止测试的二进制与部署的二进制不同。Release 作业同样测试最终 release 二进制。

2026-10-01 的连接管理和 Pingora 就绪修复已取得新双架构 CI、发行产物及最终镜像通过证据，运行源码为 `ca46d76`。新镜像的默认 Ingress 46 项、Gateway 112 项检查通过；独立 24 小时长测在 2026-10-02 运行约 12.3 小时后出现两次共享限流 HTTP 503，提前失败。阶段结果和失败证据见[验证记录](validation-hyper-default-2026-10-01.md)。旧产物和诊断镜像的成功结果不替代该运行版本的资格；原始六次共享限流 503 的根因仍未确认。

## 运维选择

```sh
rgnix check -c nginx.conf
rgnix serve -c nginx.conf
rgnix serve -c nginx.conf --engine pingora
helm upgrade --install rgnix charts/rgnix --set image.repository=YOUR_REPOSITORY --set image.tag=YOUR_TESTED_TAG
helm upgrade rgnix charts/rgnix --reuse-values --set engine=pingora
```

Chart 的 `engine: null` 选择 Hyper；旧 `experimentalHyper.enabled: true/false` 仍固定选择对应内核，`engine` 非空时优先。Chart 同时设置新旧环境变量。Hyper 部署必须使用当前开发版本构建的镜像；已经发布的 v0.5.0 没有 Hyper，仍使用 v0.5.0 镜像时必须设置 `experimentalHyper.enabled=false`，并保持 `engine=null`，以免向旧二进制传递新 CLI 参数。切换开发镜像后，可用 `engine=pingora` 回退。现有 v0.5.0 二进制、镜像与 OCI Chart 不因源码默认值变更而改变。

`rgnix_engine_info{engine="hyper"|"pingora"} 1` 表示实际运行内核。回退需要重新启动或滚动部署；HTTP/3、CONNECT 等 Hyper 专属配置需要同时调整，配置不兼容时明确失败。

连接治理使用 socket 来源 IP。共享代理或 NAT 集中流量时，管理员应按可信来源流量调大 `--hyper-max-connections-per-ip`、`--hyper-max-handshakes-per-ip`，保留进程和监听器的总预算。性能脚本针对同一个可信 loopback 压测来源显式调整这两个上限，并将完整启动参数写入报告；产品默认保护不因此改变。

## 长时间验证

在专用命名空间运行：

```sh
python3 scripts/gateway_e2e.py --context YOUR_CONTEXT \
  --namespace rgnix-hyper-default-soak --image YOUR_TESTED_IMAGE \
  --engine hyper --require-multiple-nodes \
  --soak-seconds 86400 --soak-event-seconds 600 --scale-routes 100 \
  --output .local/hyper-default-soak.json
```

测试每 30 秒把各 Pod 的指标和实际 image ID 写入 `.samples.jsonl`；负载进程把请求数、错误和尾延迟写入 `.load.jsonl`。六个 worker 分别运行 HTTP、TLS 和 RGL 前缀 body 路由，合计最高 240 请求/秒；日志与 span 配置 OTLP 输出；长测结束须核对持续副本的实际接收计数、排空和丢弃增量。每十分钟发布插件，首次及每六次发布同时轮换 TLS、替换一个就绪 Pod。重连不会重放失败的请求；任何请求错误导致该负载检查失败。

吞吐对照与稳定性测试串行运行，避免压测抢占同一宿主机资源影响限流依赖。负载在首个请求错误后结束，并保留完整错误记录；修复或调整验证环境后，新的 24 小时计时与旧失败记录分别保存。

在 macOS/OrbStack 做长测时，宿主机需要持续接电并保持运行，可使用随测试结束释放的临时防闲置休眠保护。测试结束后核对宿主机 Sleep/Wake 记录；宿主机暂停会同时影响 Gateway、Redis 和负载端，不能将该窗口计为连续稳定性通过。2026-10-01 的一次诊断已记录到电量 1% 时休眠及 TLS 客户端超时，见[验证记录](validation-hyper-default-2026-10-01.md)。

延迟窗口最多保留每 worker 最近 3000 个成功请求，不能称为整个 24 小时的精确 p99。一份副本持续运行整个测试，禁止消失或重启；仅替换另一副本，避免重启掩盖长期增长。趋势须按 Pod 生命周期检查：RSS、描述符、连接或占用许可持续增长需要调查。该负载不代表生产容量，也不包含 24 小时 HTTP/2、HTTP/3 或跨物理主机故障验证；短时协议检查单独记录。

Rust 与 Chart 默认值已按用户明确要求切换。默认变化后的产物须重新验证默认启动和 Pingora 回退；旧二进制与镜像的证据不能作为新产物的证明。运行中或失败的 24 小时测试不能记为通过，未完成资格保持公开记录。

2026-10-02 还确认并修正 Gateway 长测的 OTLP 接收端响应类型遗漏：旧轮次日志/span 接收数为零，不能算成功交付。相同发行二进制的接收端对照通过；验证脚本提交 `241951f` 的[后续 CI](https://github.com/SamuelSupe/rgnix/actions/runs/36961961578)全部 7 作业通过，两架构发行二进制与前轮产物一致。该 CI 未执行 Kubernetes Gateway 长测。独立诊断于 UTC 09:04 在约 5.1 小时后出现一次 Redis 查询超时 503；带独立宿主机/Linux 时钟的新诊断又在 UTC 09:25、实际负载 235 秒后出现四次查询超时 503。故障附近 Linux 独立等待间隙约 72 毫秒，整轮 315 毫秒峰值发生在更早时间；时间相关不足以确认原因。已保存失败证据。没有业务 worker 的 15 分钟资源对照正常结束：Linux 独立观察器有 463 毫秒间隙，PING 往返最高 45 毫秒；不能把单个观察进程的间隙当作整台 VM 暂停或原故障原因。随后线程采样诊断在实际负载 683.56 秒、113,351 次成功请求后出现一次 Redis 查询超时 503。附近 Gateway/Redis 有调度等待和重大缺页增量，同一秒 SLOWLOG 有 133.69 毫秒 EVAL；累计顺序采样和时间相关均不足以证明原因。该轮已退出并保存完整证据。现补充原生线程身份映射的独立诊断，保持原预算、限流策略和负载要求；它仍不能替代正式产物资格。原生线程映射已在实际诊断 Pod 上核对；r4a 预检缺少随附探针的设施遗漏已保留证据并修正。r4b 随后的 900.114 秒混合请求有 148,568 次成功、零请求错误，OTLP 交付与排空门槛通过；完整流程仍因验证证书 CN 超长和结束后观察器生命周期问题失败，不能记为整轮通过。QA 已修正初次/轮换证书 CN，完整 DNS SAN 保留，实际证书回归通过；新独立复验还核对线程观察器正常停止后才进行计划中的滚动重启。它们只是验证设施修正，不能解释原限流 503 或代替正式产物资格。产品运行源码未变，根因、24 小时和当前产物性能资格仍未确认；默认 Hyper 保持用户选择。


2026-10-02 的 r5a 原生线程复验在实际 354.008 秒、57,122 次成功请求后发生一次共享限流查询超时 503，完整失败和精确线程映射证据已保留。调度等待及缺页增量仍只是时间相关，根因未确认；成功结束的 Admission 和观察器握手未执行。证书 QA 提交 0a56bff 的 CI 全部 7 作业通过，两架构实际发行二进制和最终镜像收据仍与原已测产物一致，但不覆盖产品 24 小时失败。另用原 69bc 发行镜像和公开 QA 单独复核完整 Gateway/Admission，未启动混合长负载；其运行状态不等于通过或长期资格。

公开 Gateway 复核 r3 的 99 项检查通过后，滚动部署因工作节点达到 Pod 数量上限而超时，Admission 尚未执行；调度与现存副本证据已保存。这不能算新限流业务失败或完整 Gateway 通过。归档本线程已结束的预检和该调度中断环境后，只释放它们占用的 Pod 名额，其余 Pod 身份保持；独立 r4 沿用同一正式镜像和完整检查重验，随后全部 112 项检查通过，包含 10 项完整 Admission 行为；最终两副本实际 Hyper/69bc/ready/零重启，停止和电源窗口已审核。该常规 QA 没有混合长负载，旧正式 12.3 小时失败及 24 小时未通过资格持续有效。

UTC 2026-10-02 14:00 的独立诊断产物已完成构建与局部回归，目录 `.local/hyper-runtime-driver-probe-20261002/`，二进制 SHA `66895251b21d0a47c40c45e10b72d7138ab52a7c4a2ddc825418b41635cd017c`。它基于 r5/c75 冻结源码，只在 scratch 中复制并修改 Redis 1.7.1 和限流诊断记录；原 registry archive SHA 与全部 124 个缓存文件核对，共享 Cargo registry/git 只读挂载，旧冻结树和正式产品运行源码未改。原连接 driver 的 `T::spawn`、共享任务 handle 生命周期、timeout/response 配置、permit、连接身份清理、失败关闭和不重放保持。初始化完成后才开始直接记录 driver poll/wake、实际 native TID、连接与 query 标识；bootstrap 未覆盖，创建者身份不能替代实际 poll 观测。 上述只完成诊断产物与设施回归，不是新的正式发行产物、完整混合诊断或 24 小时资格。正式 69bc/dbf046 二进制及此前 12.3 小时和 r1/r2/r3a/r5a 的真实业务失败持续有效；根因仍未确认。默认 Hyper 保持，不放宽预算、不失败开放、不重放，不跳到性能。

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

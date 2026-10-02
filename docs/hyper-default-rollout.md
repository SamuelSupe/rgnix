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

# OrbStack Kubernetes 多副本、1 小时混合负载与容量验证

日期：2026-10-25。对象是 `experiment/replace-pingora`、HEAD `1bd974f0f302b39268b4128e8e5a7785593a7176` 加工作区已有未提交修改。未将旧版本或先前的性能结果冒充当前源码证据。

## 结论

**整体评分由此前 88/100 调整为 90/100。** 当前发行构建的 995 项运行检查通过；三节点 Kubernetes 两副本完成 1 小时混合负载，零请求失败。容量正式矩阵的 24 个窗口全部满足暂定 SLO，但仅 1/8 档位通过吞吐与 P99 重复性门槛，且高负载存在本地访问日志丢弃。稳定生产容量仍未认证。

本轮是运行验收与容量测量，评分沿用前几轮源码审查的功能、安全和实现结论，没有重新宣称完成全仓逐行审计。

## 环境与产物身份

- macOS Mac17,7，18 个逻辑 CPU、64GiB 内存；OrbStack Docker 报告 18 CPU、31.36GiB 内存，Linux ARM64、cgroup v2。现有其他项目仍在同一物理宿主机运行。
- 专用 kind `rgnix-qa-20261025`，kind v0.30.0、Kubernetes v1.34.0，1 个控制节点、2 个工作节点。两个网关副本通过硬性节点分布约束运行在不同工作节点。三个节点共享一台 Mac，不代表三个物理故障域。
- Gateway API v1.6.1 CRD，SHA256 `24d931f22abd8e40c973264319ead7cfa09d0fb7716b7ab1ee2ff174cb063a73`。
- 当前源码在 OrbStack 原生 ARM64 上用 Rust 1.90.0、`cargo build --release --locked --features http3,jemalloc -j2` 构建；使用现有 thin LTO 配置及 `Dockerfile.release` 组装运行镜像。这不是 Rust 1.98 双架构正式发行认证。
- ELF SHA256：`4fa527eccde3898649d0610af38cb20a3d28e53bcc00671498c48ed5fa8349dd`；镜像 `rgnix:production-qa-20261025`。Docker 索引 ID 与 Pod 所报配置 ID 不同，完整值及逐 Pod ELF 校验见[环境](validation/kubernetes-2026-10-25/environment.json)和[产物身份](validation/kubernetes-2026-10-25/pod-binary-identity.json)。

宿主磁盘余量不足以满足 Kubernetes 默认百分比阈值，因此只在这个临时集群将 `nodefs/imagefs.available` 设为 2GiB，并将镜像 GC 高/低水位设为 100%/99%。未修改默认 OrbStack Kubernetes 集群。测试期间持续检查余量与节点状态；不能将这种测试环境设置作为生产建议。精确配置见 [kind.yaml](validation/kubernetes-2026-10-25/kind.yaml) 和[实际 kubelet 配置](validation/kubernetes-2026-10-25/kubelet-effective.yaml)。

## 当前产物的功能门禁

| 范围 | 实际结果 |
|---|---:|
| Hyper 与 Pingora：HTTP、Ingress 恢复、OTLP、日志轮换、产品能力、迁移 | 每引擎 345 项，共 690 项通过 |
| Hyper 集成 | 84 项通过 |
| Hyper 协议与 HTTP/3 | 76 项通过 |
| Kubernetes Gateway 完整行为验收 | 145 项通过 |

共 **995 项运行检查通过**。镜像默认引擎、显式 Hyper、显式 Pingora 的 HTTP/readiness/metrics 冒烟检查另外通过。依据：[产物摘要](validation/kubernetes-2026-10-25/artifact-summary.json)、[运行日志](validation/kubernetes-2026-10-25/artifact-validation.log)、[Gateway 检查清单](validation/kubernetes-2026-10-25/gateway.json)。

Kubernetes 检查包含副本配置收敛、RBAC 观察失败及恢复、检查点恢复、EndpointSlice/路由变更、限流与租户隔离、RGL 发布、200 条额外路由、TLS Secret 轮换/删除、HTTP/2 流控期限、WebSocket 期限、gRPC 双向流与 Pod 终止排空、滚动重启、admission 拒绝/故障隔离及证书无重启热更新。预期注入的错误证书会输出“保留最后有效证书”的错误日志，且对应回归通过；这不计为未处理的运行故障。

## 1 小时混合负载

用户选择 3,600 秒。实际客户端测量 **3,600.106 秒、613,597 次成功请求、0 次失败、平均 170.44 req/s**。六个 worker 使用 HTTP/1 keepalive，每个最多 40 req/s；组合 HTTP、TLS、RGL 请求/响应 hook、请求体前缀检查及完整转发、OTLP 日志和 trace。请求不重放，收到 `Connection: close` 后下个请求重新连接。

| 指标 | 全程结果 |
|---|---:|
| 成功请求 P50 / P95 / P99 上界 | 41.0 / 44.1 / **45.5ms** |
| 最大成功延迟 | 286.21ms |
| 超过 3 秒的直方图样本 | 0 |
| 资源快照 | 121 组，约每 30 秒一组 |
| RGL 发布 | 6 次 |
| TLS 证书轮换 / 计划 Pod 替换 | 各 2 次 |
| 持续运行副本重启 | 0 |
| 持续副本 OTLP 接收确认日志 / span | 610,970 / 1,221,940 |
| 持续副本 OTLP 丢弃增量 | 0；结束后队列排空 |

延迟来自覆盖全部 613,597 个成功请求的 100µs 桶直方图；错误另外统计。原有 `p99_ms` 字段仅是每个 worker 最近 3,000 个成功样本的滚动值，本报告没有用它代替全程 P99。完整直方图及逐窗口数据见 [load JSONL](validation/kubernetes-2026-10-25/gateway.load.jsonl)。

持续副本 UID `23cd9fb8-bff2-4b7d-98d5-7125f153732d` 在所有快照中存在且未重启。RSS 范围 60.22–72.93MiB；第 10–20 分钟中位数 63.06MiB，最后 10 分钟 64.31MiB；FD 范围 98–105，两个区间中位数都为 103；线程 18–20。平均进程 CPU 约 0.045 核，采样 inflight 最大 5、plugin 最大 1。未观察到无界增长，但 1 小时不足以排除慢性泄漏。

第一次 Pod 替换后，长连接主要汇聚到持续副本。因此这里证明的是带两副本编排及计划替换的服务连续性、单副本 1 小时寿命和遥测投递，不能声称两个副本整小时均匀满载。两副本实际返回正确内容另有逐 Pod 检查；容量测试使用更多连接并记录各副本请求分布。

原始证据：[资源 JSONL](validation/kubernetes-2026-10-25/gateway.samples.jsonl)、[汇总计算](validation/kubernetes-2026-10-25/analysis.json)。混合负载不是峰值吞吐长测，也没有将 HTTP/3 纳入 1 小时负载；HTTP/3 由上述独立运行门禁覆盖。

## 容量与 P99 曲线

配置：两个 Hyper 副本，各 2 个 worker、2 CPU/512MiB 上限，201 条 HTTPRoute。控制节点上分别运行 4 CPU/1GiB 的 NGINX 1.22.1 源站和 wrk 4.1.0 客户端。HTTP/1 keepalive 经 ClusterIP；保留默认 stdout 访问日志，不启用 TLS、RGL、OTLP。单个受信压测 IP 的连接预算为 16384，待握手全局/单 IP 预算均为 4096，max-inflight 为 4096。CPU 上限不代表独占或预留物理核心。

每个窗口先验证 Service 和两个 Pod 的状态码及完整响应体，5 秒预热后测量 30 秒。相同镜像和两个相同 Pod 连续执行三轮，第二轮反向扫描；共 24 个正式代理测量窗口、24 个代理预热窗口，另有 4 组源站基线及对应预热。所有 56 个窗口及 wrk 原始输出保留在 [capacity.json](validation/kubernetes-2026-10-25/capacity.json)。

正式代理测量共 **54,706,014 次完成请求、wrk 错误 0**；24/24 满足用户选择的错误事件数/完成请求数 ≤0.1%、窗口 P99 ≤50ms。代理预热也全部通过；含代理预热共完成 64,228,014 次请求。下面每格包含同档位的三个正式窗口，不能把不同窗口的分位数合并成“全程 P99”。

| 响应体 | 并发 | 吞吐中位数 [范围] req/s | P99 中位数 [范围] ms | 三轮 wrk 错误 | 三轮 SLO | 重复性 |
|---|---:|---:|---:|---:|---|---|
| 1KiB | 16 | 49,360 [35,427–66,295] | 1.521 [1.327–1.954] | 0 | 通过 | 未通过 |
| 16KiB | 16 | 47,422 [40,530–48,993] | 1.636 [1.422–1.751] | 0 | 通过 | 未通过 |
| 1KiB | 64 | 91,688 [79,438–96,691] | 3.315 [2.924–3.636] | 0 | 通过 | 未通过 |
| 16KiB | 64 | 86,150 [80,491–86,343] | 3.793 [3.022–4.322] | 0 | 通过 | 未通过 |
| 1KiB | 256 | 87,450 [68,304–101,764] | 7.645 [7.467–8.559] | 0 | 通过 | 未通过 |
| 16KiB | 256 | 77,183 [71,228–92,893] | 8.149 [8.103–9.397] | 0 | 通过 | 未通过 |
| 1KiB | 1024 | 91,593 [88,153–96,895] | 20.022 [19.728–20.894] | 0 | 通过 | 通过 |
| 16KiB | 1024 | 79,700 [76,262–82,046] | 24.314 [21.127–24.983] | 0 | 通过 | 未通过 |

重复性要求吞吐与 P99 的 CV 均 ≤10%，且各自极差/中位数均 ≤10%。**只有 1KiB / 并发 1024 通过，两项指标整体仅 1/8 档位合格**。例如该通过档位吞吐中位数 91,593 req/s、P99 中位数 20.022ms；单一档位三轮通过仍不是全产品稳定容量认证。完整曲线见 [CSV](validation/kubernetes-2026-10-25/capacity-curve.csv)，CV 和逐窗口 CPU/内存/FD 见[分析 JSON](validation/kubernetes-2026-10-25/analysis.json)。

首轮与第三轮的源站直连吞吐：1KiB 从 523,418 降到 292,281 req/s，16KiB 从 437,575 降到 258,491 req/s。源站观测吞吐高于代理，但所有进程共享同一物理宿主机，这不足以独立定位瓶颈。不要将不同时段的峰值当作稳定可复现结果。

正式窗口中，每个网关进程平均 CPU 范围约 0.83–1.97 核；最高窗口约 79.3% 的 cgroup 调度周期发生过节流。负载器 CPU 为 0.53–1.46 核，低于其 4 CPU 上限。单 Pod RSS 采样最大 117.84MiB，FD 最大 1183；各窗口两个副本的请求份额约为 35%–65%，两者都实际服务。两个 Pod UID 全程不变、容器重启数为 0，结束时三个节点均 Ready，MemoryPressure/DiskPressure/PIDPressure 均为 False。[最终状态与指标](validation/kubernetes-2026-10-25/final-capture.json)。

必须同时披露的两项边界：

- 全部代理窗口的服务端 499 计数增加 **3,451**，增量都出现在最后一次周期采样之后的结束快照中，周期采样阶段没有增量。这与 wrk 到时关闭在途连接一致；它们不属于 wrk 的已完成响应错误，也不能隐去。[取消分析](validation/kubernetes-2026-10-25/capacity-cancellation-analysis.json)保留对应统计；本轮没有逐请求 trace 归因。
- 两个正式容量 Pod 的本地访问日志丢弃计数累计 **8,204**（797 + 7,407），错误日志丢弃计数为 0。该计数从完整 metrics 的结束快照取得，没有按窗口归因。已有[本地日志契约](log-rotation.md)采用有界队列，允许压力下丢弃，并不承诺持久化审计。本轮没有证明峰值容量下日志无损，不能用低负载 OTLP 长测的零丢弃来替代这一结论。[原始 metrics](validation/kubernetes-2026-10-25/capacity-telemetry-final.json)。

这是闭环、有限并发、指定负载和资源预算下的观测；wrk P99 来自其修正直方图。尚未做开放到达率测试，也未搜索更高并发或最大可承载到达率，故没有给出最大生产容量或最低副本数承诺。

## 验收工具修正与保留的失败

本轮没有修改产品 `src/` 或 vendor 实现，修正了验收中发现的问题：

1. 原 Kubernetes namespace 配额检查仍假设跨副本共享桶，首次完整运行在 `[200,200,200]` 处失败。当前文档和实现明确是每进程/每副本配额，Redis 已移除。新检查在**每个副本内**交替访问同 namespace 的两条路由，分别验证 `[200,200,429]`，并验证另一个 namespace 仍为 200；同时保护路由聚合、独立副本 burst 和租户隔离。[原失败结果](validation/kubernetes-2026-10-25/gateway-stale-shared-quota.json)保留，修正后完整 145 项通过。
2. 混合负载增加全程有界直方图，避免最近 18,000 个成功样本掩盖早期慢请求。定向合成检查确认：早期 600 个 100ms 样本被滚动窗口淘汰后，全程 P99 仍是 100ms；溢出桶占据分位数时返回空值而不是伪造 3 秒上界。[检查记录](validation/kubernetes-2026-10-25/histogram-check.json)。
3. 最小 Debian 容量夹具没有 `/etc/services`，wrk 对省略端口的 URL 报 service 不存在。URL 改为显式 `:80` 后预检通过，再开始完整正式矩阵。[原预检失败](validation/kubernetes-2026-10-25/capacity-preflight-no-service-port.json)保留，不混入正式结果。
4. 第一轮容量尝试保留了默认待握手预算（单 IP 自动上限 16），虽然连接预算已提高，单 IP 集中建连仍触发保护。在已记录的 15 个窗口中，共有 3,681 次 wrk read 错误；每个窗口的错误数与 `handshake_ip` 拒绝增量**逐一相等**。并发 256 的两个预热窗口错误率分别为 0.145% 和 0.255%。确认原因后中止当时未完成的窗口，保留[全部已产生窗口](validation/kubernetes-2026-10-25/capacity-default-handshake.json)及[对应拒绝指标](validation/kubernetes-2026-10-25/capacity-default-handshake-analysis.json)。沿用仓库现有 `benchmark_compare.py` 的受信压测 IP 配置，将全局/单 IP 待握手预算设为 4096，然后**从头重跑三轮完整矩阵**。这区分了默认接入保护与调整过的压测容量；没有修改产品默认值，也没有从失败轮次挑选“好窗口”拼接正式结果。
5. 调整预算触发 Helm 滚动更新后，旧夹具立即断言 Pod 总数为 2，启动阶段失败，尚未产生测量窗口。改为有期限地等待旧 Pod 全部退出且两个副本 Ready，再采集身份和开始负载。随后主动执行一次 rollout restart，再运行完整夹具验证这个启动路径。[原失败日志](validation/kubernetes-2026-10-25/capacity-rollout-setup-failure.log)保留。

环境准备还遇到 Docker 多架构索引导入 kind 时缺少 OCI 子摘要，改用显式 `linux/arm64` image archive 后载入成功；未使用缺失层或旧产品镜像代替。短预检和容器内源站冒烟只验证夹具，不作为正式容量结论。

## 修改保留与清理

开始时对 1,082 个已有非忽略文件建立 SHA256 清单。最终核验保留所有原有产品源码、vendor、Cargo 文件及原有删除状态；本轮只修改原有 `scripts/gateway_e2e.py`、`docs/production-readiness.md`、`docs/performance.md`，新增容量夹具、Dockerfile、报告和证据。验证结束时没有 stash、reset、stage、commit 或 push；后续 GitHub 交付按用户新指令进行。[原清单](validation/kubernetes-2026-10-25/source-baseline-sha256.json)和[最终核验](validation/kubernetes-2026-10-25/preservation.json)可复查。

为避免共享磁盘逼近临时集群的 2GiB 保护线，执行过范围受限的 rgnix 编译缓存清理：本次构建生成文件，以及 rgnix debug/test cache 中的 `incremental`、`deps`。保留源文件、既有报告和独立调试/发行二进制；未来编译可能需要重建缓存。清单分别见[本次构建缓存](validation/kubernetes-2026-10-25/build-cache-cleanup.json)、[调试缓存](validation/kubernetes-2026-10-25/rebuildable-cache-cleanup.json)、[测试缓存](validation/kubernetes-2026-10-25/vendor-test-cache-cleanup.json)。没有执行全局 Docker prune。

在保存结果、Pod ELF 身份和最终指标后，删除了本轮 kind 集群及四个测试镜像保留容器；本轮三个专用镜像标签收尾时已不存在。发行 ELF 和构建/运行日志保留于 `.local/production-qa-20261025/`，可重新组装镜像。默认 OrbStack 集群未作配置修改；收尾只读检查显示其 `Ready=True`、`DiskPressure=True`，宿主磁盘压力仍是环境限制，不能将本轮临时阈值当作解决方案。[清理记录](validation/kubernetes-2026-10-25/cleanup.json)。

## 评分与尚未验证的边界

评分是工程成熟度判断，不是认证结果。此前四个源码维度沿用已有审查结论；本轮增加多副本、持续运行和容量边界的实测证据，将生产就绪维度提高 2 分。

| 维度 | 此前 | 本轮 |
|---|---:|---:|
| 功能完整度 | 18/20 | 18/20 |
| 协议与正确性 | 19/20 | 19/20 |
| 安全与资源边界 | 18/20 | 18/20 |
| 可维护性与测试 | 18/20 | 18/20 |
| 生产就绪证据 | 15/20 | 17/20 |
| **合计** | **88/100** | **90/100** |

目前具备更充分的受控试点依据。主要扣分仍是容量重复性不足、高负载本地日志可能丢弃，以及长时间与真实故障域的资格尚未补齐。

仍未验证 24 小时以上运行、独占生产硬件的可重复容量、真实物理节点故障/网络分区、真实 NIC/跨机延迟、开放到达率负载下的排队行为、混合 TLS/RGL/OTLP 的峰值容量、双架构发行及 Gateway API 官方 conformance 认证。容量指标只适用于本报告给定配置，不能承诺生产 SLA。

复现入口：[`scripts/gateway_e2e.py`](../scripts/gateway_e2e.py)、[`scripts/gateway_capacity.py`](../scripts/gateway_capacity.py)、[生产就绪指南](production-readiness.md)。原始测量、失败与环境元数据均保存在 [validation/kubernetes-2026-10-25](validation/kubernetes-2026-10-25/)，文件摘要见 [SHA256SUMS](validation/kubernetes-2026-10-25/SHA256SUMS)。

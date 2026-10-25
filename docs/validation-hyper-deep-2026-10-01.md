# Hyper 深度剖析与连接期限修复（2026-10-01）

这轮确认并修复了 HTTP/1 空闲上游错误运行响应读取期限的问题，同时减少请求状态搬运。**饱和吞吐仍明显落后于 NGINX**：最终两 worker / 64 并发通过 A/A 门槛，优化前后中位数基本持平，当前约为 NGINX 的 53.4%。不能把复制下降比例或通过功能检查当成吞吐提升。

当前工作在 `experiment/replace-pingora`，默认数据面仍为 Pingora。以下均为本轮 OrbStack Linux AArch64 的实际结果，尚未提交或发布。完整输出和源码/二进制摘要见[机器记录](validation/hyper-deep-2026-10-01.json)。

## 保留的改动

1. 禁止重放时，legacy 客户端使用低层 `send_request` 的 NoRetry 回调，响应通道不再保留用于恢复的完整请求；允许重试的路径保留原逻辑。连接获取、URI/Host、连接信息和归还池逻辑共用。
2. `RequestBody` 变为一个可空的上传状态指针。没有 body 的请求不分配上传状态，有 body 时继续使用原来的检查、回放、大小限制、镜像和超时逻辑。空请求的 full/prefix 检查仍返回完整的空视图，off 仍不暴露 body。
3. 自动 Date 只在需要编码该头时检查缓存，取消每次 HTTP/1 连接轮询的检查。显式或上游提供的 Date 保留，持续连接上的自动 Date 仍会推进。
4. HTTP/1 连接区分空闲探测与活动交换：空闲时继续读取 EOF，暂停响应期限，由连接池处理空闲过期；新请求 flush 后使用当前任务的 waker 登记响应期限。没有为了登记期限增加一次 socket 读取。HTTP/2 的底层和 stream 期限保持原路径。

新增行为检查保护空 body 检查、持续连接 Date、显式 Date 和空闲上游复用。最后一个检查在修复前失败：100ms 响应期限使健康连接在 250ms 空闲期间被关闭；修复后三个间隔请求使用同一个上游连接。初次修复通过空闲检查，却使慢响应头不能及时超时，已修正登记时机；保留版本同时通过原有慢上游、上传进展和响应流超时检查。

## 操作计数

单持久连接、预热十次、500 次逐请求状态/完整 1 KiB 响应校验。libc 和 jemalloc 入口探针在独立窗口运行，包含少量后台操作，没有丢失事件。复制不含编译器内联的操作，累计申请字节不是 RSS。

| 指标 / 请求 | 本轮优化前 | 最终保留版本 |
|---|---:|---:|
| 捕获的 libc 复制字节 | 14,357.872 B | 5,914.816 B（−58.80%） |
| memcpy 调用 | 68.272 | 44.262 |
| clock_gettime 调用 | 26.470 | 24.378 |
| jemalloc 分配/重分配入口 | 17.528 | 16.570 |
| 累计申请字节 | 4,614.994 B | 4,697.656 B（+1.79%） |

单独切换 NoRetry 回调后，复制约为 14,343.888 B/请求，几乎没有变化：原来的复制转移到了较大的异步 future。缩小上传状态后才明显下降。这也是保留分阶段数据的原因。[优化前探针](validation/hyper-deep-primitives-before-2026-10-01.json)、[回调阶段](validation/hyper-deep-primitives-dispatch-2026-10-01.json)、[最终探针](validation/hyper-deep-primitives-retained-2026-10-01.json)、[最终分配探针](validation/hyper-deep-alloc-retained-2026-10-01.json)可独立核对。

## 与 NGINX 的完整产品对照

共同 NGINX 1.28.0 源站、普通 HTTP/1 / 1 KiB 代理、关闭自动重试和代理缓冲。客户端四线程 CPU 2～5，源站四 worker CPU 6～9，前端 CPU 10 或 10～11。每组每程序每轮两个窗口，三轮轮换顺序，预热三秒、测量十二秒；上游直连另测余量。这里没有 OpenResty 前端比较。

| 阶段 / 并发 | 优化前 req/s | 候选 req/s | NGINX req/s | A/A：前 / 候选 / NGINX |
|---|---:|---:|---:|---|
| 单 worker，中间版本 / 64 | 80,718 | 81,890 | 125,265 | 通过 / 通过 / 通过 |
| 单 worker，中间版本 / 256 | 64,603 | 67,196 | 104,608 | 通过 / 失败 / 通过 |
| 双 worker，最终版本 / 64 | 126,177 | 126,497 | 237,101 | 通过 / 通过 / 通过 |
| 双 worker，最终版本 / 256 | 112,608 | 118,941 | 230,446 | 失败 / 通过 / 失败 |

单 worker 表对应 NoRetry、Date 和上传状态缩小后的中间版本；最终表还包含连接期限修复。不能把不同阶段相除计算最终版本的扩展效率。

两组完整对照共 **72 窗、105,605,253 请求、零错误**。单 worker / 64 的观测差为 +1.45%；最终双 worker / 64 为 +0.25%，CPU 从 15.55 到 15.53 µs/请求，NGINX 为 8.41 µs。这些小差值低于不少同程序窗口差，**通过 A/A 门槛也不能证明其统计显著性**。256 并发不能认定稳定收益或与 NGINX 的稳定差额。

最终版本最大窗口 P99 为 64 并发 1.232ms、256 并发 3.846ms；峰值 PSS 为 30.24 / 47.89 MiB。P99 是窗口最大值，未合并样本，也不是相同到达速率的延迟比较。没有性能对等或内存对等结论。

A/A 门槛为至少三对、零错误、每对 RPS 对称差及两列样本 CV 均不超过 10%。单 worker 候选 / 256 有一窗降至约 43,469 req/s。最终双 worker 的优化前 / 256 配对波动超标，NGINX 同组也有大波动；全部保留。

此前双 worker 中间版本在 **15 窗、24,154,435 请求**后发现四个状态错误，立即停止。日志为带底层超时的 Canceled 客户端错误，未发生请求重试。空闲期限缺陷随后单独复现；不能据此宣称所有波动都由该缺陷造成。最终复验 36 窗零错误。[失败窗口](validation/hyper-deep-comparison-2w-fixed-2026-10-01.json)、[最终窗口](validation/hyper-deep-comparison-retained-2026-10-01.json)、[校准分析](validation/hyper-deep-comparison-retained-analysis-2026-10-01.json)均归档。

## 间歇 HTTPS 上游

另用验证 CA 和主机名的 Python TLS 源站，响应读取期限 100ms、keepalive 5s，每次完整响应后等待 250ms。三个交替轮次，每程序每轮两窗、每窗十个正式请求；每窗的预热请求不计入。120 次请求均验证状态和完整 1 KiB body。

| 正式请求的获取结果 | 优化前（60 请求） | 最终版本（60 请求） |
|---|---:|---:|
| 新建连接获取 | 60 | 0 |
| 复用连接获取 | 0 | 60 |
| 每窗不同源站 TCP peer 数 | 10 | 1 |
| 窗口延迟中位数的中位数 | 1.538ms | 0.440ms |

连接复用变化得到指标和源站 peer 双重确认，避免了这些正式请求的重复 TCP/TLS 握手。优化前延迟配对差最大 33.10%，列 CV 最大 23.26%，未通过校准；最终延迟组通过。**不宣布稳定的延迟改善百分比**，此场景也不能代表饱和 TLS 容量。[原始数据](validation/hyper-deep-idle-tls-2026-10-01.json)及[复现脚本](validation/benchmark-hyper-idle-2026-10-01.py)已保存。

## 剩余瓶颈与已排除的解释

- **没有多一倍的网络系统调用。** 单 worker 优化前和 NGINX 均约每请求两次 `recvfrom`、两次 `writev`；最终双 worker 也保持这一数量。正式负载预热后的独立 syscall 窗口未见持续新建连接。
- **单 worker 锁阻塞不是主要已证实原因。** 当时 futex 仅约 0.000131 次/请求；最终双 worker 为约 0.025075，NGINX 为零。双 worker 上下文切换约 0.015261 / 请求，NGINX 约 0.002143。它们提示共享调度/状态的额外成本，但不能从调用次数确定锁等待耗时或将其解释成全部差距。
- **网络唤醒不能直接当成 Tokio 浪费。** 最终 flat 样本的 `__wake_up_sync_key` 为 19.45%，调用栈大量来自 TCP 收包和 socket 可读唤醒。协议、网络和运行时调用栈有重叠，不能将 inclusive 百分比相加。
- **仍有较多用户态固定成本。** 最终双 worker flat DSO 分布约为内核 49.72%、rgnix 38.76%、vDSO 6.51%、libc 4.99%。可见客户端 future、请求生命周期、引用计数/原子操作、首部解析与编码、请求/响应通道。源码仍使用共享 Tokio 调度器、共享池、MPSC 请求队列和 oneshot 响应回调；复制减少没有消除这条执行链。DSO 百分比只是采样分布，不能直接换算成加速上限。
- **精确时钟没有发现虚拟机陷入型的大开销。** 同机独立 C 微测中，MONOTONIC 约 9.94ns、COARSE 约 3.35ns/调用。即使按 24 次调用估算，差值量级也只有约 0.16µs；不是 Rust 全部计时成本的精确预算。没有牺牲超时精度引入粗时钟。

后续优先级是减少整条 HTTP/1 请求执行链的状态和跨任务交接，并在相同功能、预算和超时契约下验证连接归属；然后才考虑更直接的首部表示。指标标签查找和池元数据也有可缓存部分，但当前采样没有证明它们足以解释大幅差距。尚未完全隔离这些因素各自的贡献。

[单 worker syscall](validation/hyper-deep-syscalls-before-2026-10-01.json)、[最终双 worker syscall](validation/hyper-deep-syscalls-retained-2w-2026-10-01.json)、[最终 CPU 样本](validation/hyper-deep-profile-retained-2w-2026-10-01.json)包含命令、计数、CPU 集和原始 flat 输出。

## 独立连接归属原型

`experiments/hyper-proxy` 新增显式 `--owned-http1`。同一原型二进制比较 legacy 客户端与下游连接绑定模式：绑定模式由同一任务推进上下游连接，空闲时仍检测 EOF，截断和取消关闭连接，不重放 POST。

这是**连接绑定、低层客户端接口和绕过共享池的组合试验**，请求队列和响应回调通道仍存在。它使用注册表 Hyper/hyper-util、系统分配器和有限 HTTP/1 功能，未接入完整产品的指标、预算、TLS/快照隔离；不能把结果换算成 rgnix 的收益或单独归因于调度。

双 worker / 64 并发，18 窗、35,012,488 请求、零错误。legacy 中位数 125,879 req/s、绑定模式 139,879、NGINX 219,007；观测差 +11.12%。**绑定模式和 NGINX 校准失败**，最大 P99 分别为 15.574 / 51.880ms，不作为稳定收益或迁入产品的依据。源码仅留在独立实验用于复现；产品不启用该模式。[原始窗口](validation/hyper-deep-comparison-owned-fixed-2026-10-01.json)、[分析](validation/hyper-deep-comparison-owned-fixed-analysis-2026-10-01.json)。

两个模式分别通过单、双 worker 的十项行为检查，包含 8 MiB 定长/分块上传、双向流式、空闲 EOF、截断、取消和 POST 不重放。原 verifier 要求十次顺序请求永远使用一个 TCP 上游，在多 worker 共享池下不成立：连接后台任务尚未归还池时可建立另一条连接。已改为验证真实发生复用；不是发现或修复了代理重连错误。

## 验证边界与复现

- 最终产品：HTTP 集成 99、产品能力 115、双 worker 生命周期 55，共 **269 项运行检查通过**；覆盖请求体、认证/RGL/租户、TLS、HTTP/2/gRPC/trailers、WebSocket、取消、超时、配置更新、预算释放和终止。
- 连接/池库 37 项单元与 21 项集成通过，2 项原有忽略；上游活动期限的 2 项原有测试通过。产品和独立原型全 target Clippy `-D warnings`、默认构建检查通过。
- 默认 Pingora 没有重跑运行验收；本轮只做其默认构建检查。没有新跑 Kubernetes 控制器、amd64、HTTP/2 或饱和 TLS 吞吐、固定到达速率延迟、长期生产负载。
- 不在吞吐窗口同时跑构建或其他剖析。OrbStack 虚拟 CPU 绑定仍不保证宿主物理核心独占，因此保留全部校准失败和尾部异常。

最终产品 SHA-256：`6d20c43887a90dc110fe29d1becbddf0263a778ea437deed66a8090bbe04c084`；本轮优化前：`55bc3a5bf5bc0638a70525740bd15c002724c9ff2540bd84bae6e4b3c60f2cee`。Root 构建为 Rust 1.90、release/thin LTO/单 codegen unit、`hyper-experimental,jemalloc`。源码哈希记录相关传输模块及清单，保留仓库已有未提交改动；不等同于干净提交的完整构建证明。

正式对照配置与完整参数位于 JSON 的 `fixtures` / `settings`；使用 `scripts/benchmark_compare.py --plain-proxy --cases proxy-1k --engines rgnix rgnix candidate candidate nginx nginx --interleave --rounds 3 --seconds 12 --warmup 3 --concurrency 64 256`，并保持对应 worker、CPU 和传输参数。独立原型组的 `hyper-control` 是附加参数的包装器，底层 ELF 与 `hyper` 相同；包装器摘要不能替代 ELF 摘要。A/A 分析可由 `docs/validation/analyze-hyper-performance-2026-09-30.py` 重算。

原始 perf 二进制与完整调用图位于本次 OrbStack `/var/tmp/rgnix-deep-20261001/`，不是可移植交付物；关键 JSON、flat 采样、日志、时钟微测源码/CSV 和原型包装器映射已归档。[本轮代码差异](validation/hyper-deep-local-2026-10-01.patch)以开始时的脏工作区快照为基线，只记录相关 Rust 模块和原型 verifier，不是相对干净 HEAD 的完整补丁。数据采集脚本的导入/参数及 verifier 的 Python 上下文管理错误也保留在机器记录，不计为成功验证。

# Hyper 请求路径完整优化与验证（2026-10-24）

基线 main `3a44af7`，最终代码 `e9f85c5`。已实现请求准备、全局非等待预算、累计指标分片和普通 H1 内联连接驱动；精确超时、产品准入、健康状态和完整指标契约保留。普通 1 KiB/16 KiB 的匹配单 worker 用户态指令分别下降 **9.13% / 10.68%**。仍高于 NGINX 的用户态工作量，不能据此承诺容量或尾延迟。

72 个正式窗口全部完成，141,009,897 次请求，五类 wrk 错误合计 0。前后对照的吞吐重复性通过 0/4 格，P99 通过 0/4 格。表中的中位数属于本机观测；只有对应基线与候选都通过预先规定门槛，才可将该格用于稳定收益判断。

完整[实施方案](hyper-performance-plan-2026-10-24.md)与[机器可读证据](validation/hyper-execution-2026-10-24.json)包含所有窗口、对照、失败试验、逐函数计数、源文件及资源清理证明。本次仅本地提交，未合并 main，未推送 GitHub。

## 实施与取舍

| 项目 | 已执行的实现或判定 | 保留的约束 |
|---|---|---|
| 时钟 | 减少内联客户端的重复测时与轮询；复用现有懒布防和 Date 缓存 | 未以毫秒粗时钟替代精确截止时间、请求/连接直方图；实测只少约一次时钟/请求 |
| 全局请求预算 | 无等待 AtomicUsize CAS 准入与 RAII 归还，移除该路径 tokio Semaphore 等待队列开销 | 整个进程的强上限、拒绝、取消归还、draining；未平分 worker 配额 |
| 高频累计指标 | 按配置 worker 数建立有界 producer shard，scrape 同步合并 counter、sum、count、累计 bucket | 名称、标签、所有时长分布、复用连接观察和即时可见性保留；仍使用原子计数，producer 可碰撞，存储/scrape 成本增加 |
| Host/首部/路径 | 常用 ASCII Host 借用，复杂 authority 保留公有解析器；静态 HeaderName；短首部表只删除实际存在的 hop 字段，宽表保留有界九次查找；按配置前缀长度查路由 | 重复/非法 Host、Connection 指定字段、Unicode、Ingress 路径段语义；基准是 exact 路由，不量化前缀索引收益 |
| 空请求体 | 仅实际上传共享计数时创建 Arc，空体在 Guard 内直接计数 | 上传和响应字节、限制、取消、结束记录 |
| 普通 H1 上游 | 当前请求/响应 body 直接驱动 Hyper Connection；worker 私有闲置池、EOF 预检、完整缓冲响应直接归还连接 | 使用现有 Hyper 编解码，短 mutex 维护 Send/Sync 所有权；未声称无锁或移除 Hyper 内部 mpsc/oneshot |
| 池和 生命周期 | 与 legacy H1/H2、监听器及快照代际共享强 IdlePoolBudget；满额时淘汰本客户端最旧闲置项；每客户端一个弱引用清理任务 | 每 authority 上限、全局上限、闲置到期、半消费/取消/异常丢弃、不重放、流式背压；不为复用无界缓存 |
| 快照/后端 | 保留已有 prepared 下标和快照所有权、Backend Lease/active 计数 | 不用删除健康/least-connections/诊断观察换取速度；本轮未额外拆除这些 Arc |

普通无策略 H1 采用内联池。完整策略路径、HTTP/2、Auto、HTTP/3、升级/隧道沿用原客户端。原先 legacy HTTP/1 池本来已按 worker 分开，不能将其 mutex 自动解释为跨核热点。

## 测量资格与身份

OrbStack Ubuntu aarch64，Rust 1.90.0/LLVM 20.1.8，locked release、thin LTO、一个 codegen unit，`http3,jemalloc`。NGINX 1.28.0。编译功能与前一轮基线一致，Cargo.toml/Cargo.lock 未改。最终代码的 437 个源文件摘要同时核对工作树与 Git 提交；正式前冻结二进制，结束再次校验 SHA256。

| 二进制 | SHA256 |
|---|---|
| main 基线 | `491e0b97149253e9bf4269cb0c46b366a0ac9aa960e9b82f0708d1cdc569c6b4` |
| 最终候选 | `5b6b2b5fb5c2987962a39d0ec56d35d15f7982ca13ad164bd61407f83379655f` |
| NGINX | `f6543200b866fad21ee74f2e3b753e27c0a5c39c671e95ba001529270471f77d` |

虚拟 CPU 亲和性不等于独占物理核。cycles/instructions 的 perf stat 虽退出 0，实际计数是 `<not supported>`；c2c 报 `no PMU supports the memory events`。同负载的 `--mode ipc`/`--mode c2c` 入口也实际执行并明确失败，原始日志保留。已经完成 1/2 worker 软件事件对照，但**没有 IPC、HITM 或缓存行争用被消除的实测结论**。

## 用户态指令：通过 1% 重复性门槛

每个完整组合三个交错回合、每程序六个样本；每个 load 扣除匹配的 startup/100 请求预热/shutdown control，再除以额外 2,000 次精确校验响应。每对 A/A 对称差与样本 CV 都必须不超过 1%。负的函数差值、未解析 `???` 与全部 outlier 均保留；使用 flat self 指令，不使用 ARM inclusive Callgrind，也不把重叠关键词组相加。

| 阶段 / 请求 | 基线指令/请求 | 候选指令/请求 | 变化 | A/A 与 CV |
|---|---:|---:|---:|---|
| 请求准备、预算与指标 / 1 KiB | 50,354.51 | 47,550.03 | -5.57% | 通过 |
| 请求准备、预算与指标 / 1 KiB + 64 字段 | 106,873.73 | 105,008.02 | -1.75% | 通过 |
| 最终内联 H1 / 1 KiB | 50,392.75 | 45,792.44 | -9.13% | 通过 |
| 最终内联 H1 / 16 KiB | 53,268.91 | 47,578.66 | -10.68% | 通过 |
| 最终内联 H1 / 1 KiB + 64 字段 | 106,897.49 | 103,265.46 | -3.40% | 通过 |

最终 1 KiB / 16 KiB 的 NGINX 指令为 23,851.98 / 28,469.89，候选约为其 1.92 / 1.67 倍。这里 NGINX 为不 fork 的单 worker 仪器对照，不是生产进程拓扑。两个阶段分别对各自匹配基线比较，不能把百分比直接相加。

1 KiB 的若干 flat self 调用点如下，下降集中在首部删除/哈希及 legacy checkout/request，内联 forward/dispatch 也承担了新的工作。编译内联、符号改名和控制流转移会改变归属，表格不能相加成为互斥组件预算，也不把消失的符号自动算作工作消失。

| 函数 | 基线 self 指令/请求中位数 | 候选 | 变化 |
|---|---:|---:|---:|
| `http::header::map::HeaderMap<T>::remove` | 1,025.00 | 132.00 | -893.00 |
| `http::header::map::hash_elem_using` | 1,903.58 | 1,088.79 | -814.79 |
| `core::hash::BuildHasher::hash_one` | 740.00 | 143.00 | -597.00 |
| `hyper_util::client::legacy::client::Client<C,B>::request::{{closure}}` | 623.00 | 0.00 | -623.00 |
| `<hyper_util::client::legacy::pool::Checkout<T,K> as core::future::future::Future>::poll` | 591.00 | 0.00 | -591.00 |
| `rgnix::proxy::hyper::inline::Client::put` | 0.00 | 159.00 | +159.00 |
| `hyper::proto::h1::dispatch::Dispatcher<D,Bs,I,T>::poll_catch` | 0.00 | 787.21 | +787.21 |
| `rgnix::proxy::hyper::plain::forward::{{closure}}` | 0.00 | 1,630.02 | +1,630.02 |

## 分配、复制和时钟独立计数

每项单个持久客户端，10 次预热后 500 次 status/body 校验请求，服务配置仍是两个 worker；包含后台活动。分配入口探针排除 jemalloc 内部嵌套 API，字节是累计申请量而非存活内存或 RSS。libc 探针不含内联复制。

| 请求 / 版本 | 分配入口/请求 | 累计申请 B/请求 | libc 复制 B/请求 | 精确时钟/请求 |
|---|---:|---:|---:|---:|
| proxy-1k / base | 13.052 | 2,531.408 | 5,840.400 | 21.328 |
| proxy-1k / micro | 10.048 | 2,472.944 | 5,822.000 | 21.312 |
| proxy-1k / final | 8.046 | 784.688 | 5,479.000 | 20.328 |
| proxy-16k / base | 21.026 | 3,009.232 | 5,846.456 | 21.332 |
| proxy-16k / micro | 18.026 | 2,967.232 | 5,828.072 | 21.320 |
| proxy-16k / final | 16.042 | 1,411.392 | 5,482.000 | 20.320 |

1 KiB 对 legacy executor 调用点捕获到一次 128 B 分配；16 KiB 没有捕获到该调用点。后者不能证明没有 spawn，内联或未解析符号仍可能漏计；未把它当作精确任务数量。代码结构取消了 eligible 路径的每连接独立驱动及每响应 on_idle 任务，仍保留 Hyper 通道和每客户端清理任务。

## 完整 72 窗口吞吐与 P99

两个 worker；1/16 KiB × 64/256 并发 × 三程序 × 三回合 × 每回合双样本。每窗口预热 3 秒、测量 15 秒；A/C/N/N/C/A 顺序按回合轮换，第二回合反转大小/并发顺序。服务 CPU 10–11，客户端 0–3，origin 6–9；四 origin worker、四 wrk 线程。期间未运行自己的构建、回归或 profiler，其他项目进程保持原状。

所有前端真实使用两个 worker，NGINX reuseport 与 rgnix 一致；逐窗 CPU 记录验证两个 worker 实际参与。Host localhost、5 秒上游超时、60 秒 keepalive、每 worker 128 / 全局 256 闲置连接，NGINX 禁用代理缓冲、重试、access log；普通路由无 RGL/auth/rate 策略，产品预算与指标仍开启。每次启动先校验精确响应，正式 wrk 窗口统计 socket/status/timeout 错误。

| 请求 / 并发 | 基线 req/s | 候选 req/s | NGINX req/s | 候选相对基线 | P99 ms：基线 / 候选 / NGINX |
|---|---:|---:|---:|---:|---:|
| proxy-1k / 64 | 140,857 | 154,413 | 188,434 | +9.6% | 1.444 / 1.361 / 1.165 |
| proxy-16k / 64 | 138,855 | 130,490 | 119,497 | -6.0% | 1.563 / 1.832 / 1.595 |
| proxy-1k / 256 | 120,514 | 130,770 | 176,019 | +8.5% | 3.935 / 3.922 / 2.724 |
| proxy-16k / 256 | 109,402 | 125,942 | 110,339 | +15.1% | 4.066 / 3.740 / 4.053 |

以上是每格六窗口中位数；P99 来自 wrk 每窗口校正直方图，未合并为原始总分布，也不是同一 offered QPS。RPS 门槛为每对 A/A 对称差及 CV ≤10%，P99 ≤20%；错误必须为零。

| 请求 / 并发 / 程序 | RPS 最大 A/A / CV | P99 最大 A/A / CV | RPS / P99 门槛 |
|---|---:|---:|---|
| proxy-1k / 64 / 基线 | 61.38% / 25.04% | 56.29% / 32.83% | 失败 / 失败 |
| proxy-1k / 64 / 候选 | 55.21% / 23.65% | 65.11% / 39.17% | 失败 / 失败 |
| proxy-1k / 64 / NGINX | 7.86% / 18.43% | 10.78% / 48.80% | 失败 / 失败 |
| proxy-16k / 64 / 基线 | 37.28% / 28.28% | 75.01% / 73.42% | 失败 / 失败 |
| proxy-16k / 64 / 候选 | 27.90% / 20.72% | 18.34% / 26.68% | 失败 / 失败 |
| proxy-16k / 64 / NGINX | 35.84% / 19.15% | 50.51% / 25.98% | 失败 / 失败 |
| proxy-1k / 256 / 基线 | 7.68% / 22.52% | 18.30% / 19.56% | 失败 / 通过 |
| proxy-1k / 256 / 候选 | 18.77% / 24.65% | 32.55% / 20.48% | 失败 / 失败 |
| proxy-1k / 256 / NGINX | 8.65% / 23.37% | 21.08% / 26.60% | 失败 / 失败 |
| proxy-16k / 256 / 基线 | 11.45% / 11.99% | 19.38% / 14.42% | 失败 / 通过 |
| proxy-16k / 256 / 候选 | 11.58% / 15.23% | 39.09% / 26.86% | 失败 / 失败 |
| proxy-16k / 256 / NGINX | 16.67% / 17.62% | 5.51% / 16.82% | 失败 / 通过 |

| 请求 / 并发 | 三回合候选吞吐变化 | 三回合候选 P99 变化 |
|---|---|---|
| proxy-1k / 64 | +5.2% / +9.7% / +1.8% | -8.8% / -1.6% / +2.9% |
| proxy-16k / 64 | -3.4% / -0.6% / +21.6% | +30.0% / -6.9% / -43.7% |
| proxy-1k / 256 | +8.5% / +8.3% / +6.0% | -4.3% / -5.3% / -9.8% |
| proxy-16k / 256 | +15.1% / +14.0% / +6.6% | -8.2% / -1.1% / +12.9% |

没有筛除较慢、较快或不符合预期的窗口；所有失败门槛都如实保留，不把 CPU 指令的可靠改善表述为已确认的吞吐/P99 改善。

## 同窗 CPU 分解与内存

CPU 由同一窗口 `/proc` process-tree utime/stime 计数，包含 NGINX master/workers，按请求量加权汇总；逐窗及汇总验证 total = user + system。PSS 为 200 ms 采样峰值的六窗口中位数。CPU 样本量及宿主调度仍受共享环境影响，不具有独立容量资格。

| 请求 / 并发 / 程序 | 总 CPU µs/请求 | 用户态 | 内核态 | 峰值 PSS MiB |
|---|---:|---:|---:|---:|
| proxy-1k / 64 / 基线 | 14.991 | 6.659 | 8.332 | 30.9 |
| proxy-1k / 64 / 候选 | 14.224 | 5.593 | 8.631 | 30.4 |
| proxy-1k / 64 / NGINX | 11.080 | 2.787 | 8.292 | 20.8 |
| proxy-16k / 64 / 基线 | 15.904 | 6.595 | 9.309 | 33.6 |
| proxy-16k / 64 / 候选 | 15.470 | 5.561 | 9.909 | 32.4 |
| proxy-16k / 64 / NGINX | 17.724 | 3.633 | 14.092 | 20.2 |
| proxy-1k / 256 / 基线 | 17.021 | 7.763 | 9.259 | 47.2 |
| proxy-1k / 256 / 候选 | 15.797 | 6.912 | 8.884 | 46.1 |
| proxy-1k / 256 / NGINX | 11.690 | 3.278 | 8.411 | 22.6 |
| proxy-16k / 256 / 基线 | 18.898 | 7.582 | 11.316 | 59.3 |
| proxy-16k / 256 / 候选 | 16.823 | 6.504 | 10.319 | 52.9 |
| proxy-16k / 256 / NGINX | 19.158 | 4.266 | 14.892 | 21.8 |

单独 perf cpu-clock profile 包含用户/内核样本，软件 syscall 计数、1/2 worker 对照、DSO 原文均在 JSON；它们与正式窗口分开。不能将 perf 比例乘以另一次正式 CPU 中位数，形成“精确的用户/内核时间预算”；epoll 次数也不能唯一推出任务轮询次数。

两 worker 的独立 syscall 观察如下。1 KiB 的网络调用数量接近，内联路径的 epoll 约减半；16 KiB 中 NGINX 约六次 recvfrom/writev，Hyper 约两次，因此不能把 1 KiB 的“内核开销接近”推广到大响应。本表是仪器窗口，不是容量校准。

| 请求 / 版本 / 程序 | recvfrom/请求 | writev/请求 | epoll_pwait/请求 |
|---|---:|---:|---:|
| proxy-1k / base / hyper | 2.0007 | 2.0003 | 0.12605 |
| proxy-1k / base / nginx | 2.0004 | 2.0003 | 0.01728 |
| proxy-1k / final / hyper | 2.0006 | 2.0003 | 0.06340 |
| proxy-16k / base / hyper | 2.0009 | 2.0004 | 0.13043 |
| proxy-16k / base / nginx | 6.0015 | 6.0013 | 0.01645 |
| proxy-16k / final / hyper | 2.0009 | 2.0004 | 0.06378 |

正式计量期基线上游获取有 43,427,907 次复用、零次新建；候选有 46,440,702 次复用、1,602 次新建，新建占 0.00345%。候选仍有少量连接流失，触发原因未逐次追踪，不能声称实现完全复用。

单 worker / 256 并发诊断具有更强闲置池压力（每 authority 128、全局 128）：1 KiB 的 connect/请求从 0.00354 变为 0.06729，16 KiB 从 0.00143 变为 0.06826；两 worker 时则接近零。它们是各一次独立仪器窗口，没有 A/A，不能确认稳定的 CPU 回归，但明确暴露了内联池在超出闲置容量时的连接周转代价。不得把 1/2 worker 的这些软件 CPU 或 syscall 差异解释为共享原子争用。本轮保留强预算与不确定连接关闭规则，未为较好数字放大池容量；后续应单独跟踪归还未 ready、容量淘汰与 EOF 的关闭原因，再以相同预算校准。

## 回归、失败试验与回退

- 最终候选 OrbStack 实际运行 **430 项**：integration 105、Hyper 80、product 115、protocols 73（含 HTTP/3）、Ingress recovery 57。覆盖策略/无策略两条路径的大响应复用、stale EOF、部分消费取消、发送后不重放、提前拒绝上传，以及原有 body/协议/热更新/预算/draining 场景。
- 最终 root release unit **31 passed，1 ignored**；hyper-util 独立测试 **38 passed，2 ignored** 及 legacy client **21 passed**；Pingora 集成 **105 passed**，保护共享预算、指标与 Host 改动。最终 strict release Clippy 通过；vendored h3 原有依赖警告保留。Python 源码语法验证通过。
- micro 先通过原 419 项，扩展真实 socket 回归后 Hyper 80 项通过；最终验证不以早期候选结果替代。
- 初始内联候选满预算时直接丢弃新归还连接，产品 capacity=1 的复用测试失败；改为淘汰本客户端最旧闲置项后完整回归通过。失败二进制、源码摘要及日志保留，未采用。
- 第二版在不完整的指令试验中仍有重复 Connection/receiver/body 轮询。最终版先 poll Connection，响应体已独立持有时直接采用原 buffered Incoming 快路径；对流式 body 只在 Pending 时驱动连接。重新构建、完整回归并以全新三回合数据验收。未用 no-op waker 偷省调度。
- 该不完整试验还遇到 NGINX Cachegrind 收到 SIGTERM 后 35 秒未退出；其六条已完成记录与失败日志单独保留，不计入有效收益。仪器 NGINX 改 SIGQUIT 后，最终三组校准全部完成。正式服务与负载参数未据此改变。
- 扩展测试曾错误复用旧 attempt 计数器，修为独立 fixture 计数后重跑。开发期 proto API、递归 Send 类型和 clippy 失败日志同样保留；它们不是性能收益。

## 复现与后续资格

在原生 Linux 使用同一 locked release 构建与配置，先跑 `scripts/profile_http.py --mode ipc --workers 1`，再以 `--workers 2` 对照，配合 `--mode c2c --sudo-perf`。要求硬件事件有数值且加载相同，再判断共享原子是否确为 HITM/IPC 瓶颈。当前环境不支持这一项。

指令：`scripts/profile_http_instructions.py --binary BASE --candidate CANDIDATE --nginx /usr/sbin/nginx --case proxy-1k --work-dir NEW_DIR`；16 KiB 改 `--case proxy-16k`，宽首部加 `--extra-headers 64`。三次都保留独立 controls。

正式容量的完整原命令在 JSON `formal`，冻结计划在 `plan`。在独占物理核/原生 Linux 上先通过同程序 A/A，再复测两版本与 NGINX；扩大到 4/8 worker、TLS/H2/H3、物理 NIC、长时间慢消费者和 soak 仍属后续资格。现有功能通过不代表这些容量场景已测。

证据归档 `/home/livesite/.cache/rgnix-hyper-execution-20261024/raw-evidence.tar.gz`，SHA256 `22a08a9322bf183f839e3581af6727ada2170372e8e32885c1d9e475aa94cc1f`，含 156 个原始 Cachegrind profile、perf data/CSV、配置和日志；JSON 带逐文件大小与摘要，保存全部 4,052 个函数的对齐 self 计数向量，signed delta 可由 load-control 无损重建。失败试验与最终组合分开。二进制身份和代码通过独立核验，已结束所属服务、探针及防休眠；其他任务未修改。

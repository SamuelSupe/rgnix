# Hyper 产品路径 CPU 剖析与上下文搬移优化

在 `experiment/replace-pingora` 的 `ad516b6` 上继续，先检查产品 Hyper、最小 Hyper 原型、默认 Pingora 和 NGINX 的 CPU 栈、系统调用与 libc 操作次数，再修改请求状态的所有权传递。普通 HTTP/1、单 worker、64 连接、1 KiB 上游；OrbStack Ubuntu ARM64。**带 profiler 的吞吐不用于性能结论。**

## 发现与取舍

CPU 栈中，产品 Hyper 的可识别 `memcpy` 热点约占 4.34% 的 flat 样本，应用请求栈约占 21.03% 的 inclusive 样本。后者包含其调用的传输操作，并不等于业务策略本身的成本。可识别 Hyper 池栈约 1.43%，完成逻辑约 0.89%，Prometheus 栈约 0.21%；内联和缺失栈使这些比例不能作为精确成本预算。调度栈包含执行中的请求，不能把它的 inclusive 比例解释为纯调度开销。

独立 syscall 计数中，Hyper、最小原型和 NGINX 都约为每请求 **2 次 recvfrom、2 次 writev**；Pingora 约为 2 次 recvfrom、2 次 sendto。该普通代理场景没有观察到 Hyper 比 NGINX 多一倍读写调用。单 worker 下 futex 很少，本轮没有证明连接池锁竞争是主要原因；这不能代替多 worker 验证。

用 libc 入口 uprobes 对 500 个已预热、校验过 body 的请求计数，产品 Hyper 捕获约 **64 KB 拷贝/请求**，最小原型约 6 KB，Pingora 约 36 KB。回溯调用点后，最明确的额外成本是较大的请求状态在响应 body、HTTP 状态机、完成队列和最终记录之间反复按值移动。时间读取约 30 次/请求，仍是独立的后续候选；本轮没有取消超时或减少指标来换数字。

## 本轮修改

`RequestGuard` 从创建后通过 `Box` 传给响应和 socket 完成队列；完成时借用上下文，用标志保证只完成一次，避免 `Option::take` 再搬移整个上下文。保留原有路由、预算、超时、trace、访问日志和取消/排空语义。默认 Pingora 路径未修改，没有引入新的运行时依赖。

使用最终计数工具重新测量两份二进制：

| 指标 | 优化前 | 优化后 | 解释 |
|---|---:|---:|---|
| 捕获的 libc 拷贝字节/请求 | 64,034 | 26,485 | 减少 58.64%；不包含编译器内联复制 |
| memcpy 调用/请求 | 142.87 | 125.87 | 操作计数，非延迟 |
| clock_gettime 调用/请求 | 30.29 | 30.24 | 基本不变，含少量后台活动 |
| 堆分配次数/请求 | 49.32 | 50.32 | 固定位置的 guard 增加一次分配 |
| 累计申请字节/请求 | 22,266 | 24,423 | 约增加 2.1 KiB；不是存活内存或 RSS |

最显著的消除发生在 `Dispatcher::poll_write`、`Connection::queue`、`ResponseBody` 销毁以及完成记录路径。剩余复制主要位于请求处理和 Hyper 客户端状态传递；没有据此宣称这些剩余复制都可安全移除。

堆分配采用 Heaptrack，四条持久连接、2,000 请求，减去独立零请求进程的启动分配。初次误将 preload 作用到 taskset，结果只跟踪启动器，已判为无效；随后直接执行 rgnix 并在执行前设置 CPU affinity，核对 raw 文件中的进程身份后重测。有效原始计数包含在证据中。

## 验证与身份

- `cargo fmt --all -- --check`、特性构建 Clippy `-D warnings`、locked release 构建通过。
- OrbStack 原生 Hyper **36 项**检查、Pingora **33 项**共同契约检查通过。已有流式、请求取消、超时、热更新、OTLP 和排空检查继续通过；新增完成计数断言，保护显式完成和 Drop 路径不能重复记录。
- 没有重复运行上一轮的 94 项默认引擎全集或 Rust 单元测试；本轮生产修改仅位于实验性 Hyper 模块。没有 amd64、长时间浸泡、多 worker 或完整 TLS/H2/RGL 候选测试。

构建均为 Rust 1.90.0 / LLVM 20.1.8，release thin LTO、codegen-units=1。优化前二进制 SHA-256：`21a986afd26f64119b9a9b82b96567b460f10d8a67b7058f3b3ca9bca70d0852`；优化后：`d5f199b3bab4debc10510ef4a36533068d4a015ce03b5f6a5e80924bd5df4170`。

[完整诊断、操作计数、源码摘要和行为检查](validation/hyper-profile-2026-09-28.json)。CPU profile 中保留带工具的请求速度仅用于解释采样范围；不能与无工具的性能窗口混算。虚拟机不提供可用 cycles PMU，本轮使用 199 Hz cpu-clock 与 DWARF 栈，每个 CPU profile 15 秒，四个基线程序合计 11,578 个样本，没有报告丢失样本。profile 的 `settings` 是来源 benchmark fixture 的配置；实际模式和测量命令在各条 record 中，不能据来源字段判断当前采样引擎。

## 无 profiler 校准

沿用单 worker / CPU 10、wrk 单线程 / CPU 2、OpenResty 单 worker / CPU 6，64 连接、3 轮交替、预热 2 秒、测量 10 秒。分别对修改前和修改后的同一二进制分配两个标签做 A/A；最大对称配对差及各组 RPS 样本 CV 都不超过 10% 才继续 A/B。下面记录最终结果，不以挑选窗口替代校准。

| 二进制 | 最大配对差 | 两组 RPS CV | 结果 |
|---|---:|---:|---|
| 优化前 | 36.65% | 15.38% / 14.78% | FAIL |
| 优化后 | 2.43% | 0.87% / 1.48% | PASS |

12 个测量窗口合计 **8,466,081 请求、零 wrk 错误**，不含预热与上游基线。优化后的本次 A/A 通过，但优化前未通过，脚本在门槛处返回 2，**没有继续旧版/新版/NGINX A/B**。不能跨两个 A/A 批次直接计算提速比例，也不能归因于某个未经证实的宿主机因素。修改后的六窗最大 P99 为 1.712 ms，只覆盖本次单 worker、64 连接和 1 KiB 场景。

[修改前 A/A 原始窗口](validation/hyper-profile-aa-before-2026-09-28.json) · [修改后 A/A 原始窗口](validation/hyper-profile-aa-after-2026-09-28.json)。可确认的是拷贝开销下降和行为契约保持；吞吐提升及接近 NGINX 的结论仍未成立。后续优先在稳定环境验证这项改动，再决定是否处理剩余客户端状态复制、计时器更新与多 worker 调度。

## 复现诊断

准备原生 Linux 上可用的 `benchmark_compare.py --plain-proxy` 结果及其 fixture。保留旧二进制，使用相同 Rust 工具链构建候选；以下每个调用都使用新目录，在私有网络命名空间内顺序执行，避免不同诊断工具互相干扰：

```sh
python3 scripts/profile_http.py \
  --matrix /path/to/aa.json --binary /path/to/rgnix-before \
  --prototype /path/to/rgnix-hyper-prototype \
  --engines hyper prototype pingora nginx \
  --work-dir /tmp/new-profile

python3 scripts/profile_http.py \
  --matrix /path/to/aa.json --binary /path/to/rgnix-before \
  --engines hyper pingora nginx --mode syscalls --seconds 6 \
  --work-dir /tmp/new-syscalls

python3 scripts/profile_http_primitives.py \
  --profile /tmp/new-profile/result.json --matrix /path/to/aa.json \
  --engines hyper --work-dir /tmp/new-primitives-before

python3 scripts/profile_http_primitives.py \
  --profile /tmp/new-profile/result.json --matrix /path/to/aa.json \
  --engines hyper --binary /path/to/rgnix-after \
  --work-dir /tmp/new-primitives-after
```

采样需要系统 perf 权限；libc 操作计数目前仅实现 Linux AArch64 寄存器映射。工具动态解析 libc memcpy/memmove 入口，为本次进程创建独立 probe 组并在退出时清理。CPU profile 的 `perf.data` 与完整调用图保留在原生诊断目录，汇总、flat profile、操作计数和二进制身份进入仓库证据文件。

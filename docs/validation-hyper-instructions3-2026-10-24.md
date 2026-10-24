# Hyper 普通请求调度与分配优化（2026-10-24）

以 `57b069e` 的受测二进制（记录在 `c381113`）为基线，最终实现 `cb9f67a`。本轮普通 1 KiB / 16 KiB 用户态指令减少 **11.49% / 11.05%**，宽首部减少 **5.02%**；全部最终配对通过 1% A/A 与 CV 门槛。没有重新进行吞吐/P99 容量矩阵，不能将这些百分比换算为容量或尾延迟收益。

完整[逐样本、逐函数、构建、探针与验证记录](validation/hyper-instructions3-2026-10-24.json)保留成功、失败和重跑数据。改动仅本地提交，未合并 main 或推送 GitHub。

## 实现

- 普通 H1 使用单一所有者的请求槽和响应槽，替代每请求发送队列及 oneshot。连接仍由当前请求/响应体驱动，复用原 Hyper dispatcher、编解码、读写缓冲、超时与响应体背压；没有引入第二套 HTTP codec。握手沿用标准 Builder，转换时丢弃未使用的标准客户端通道。
- 普通 H1 的 102/103 直接转交拥有的响应，经过现有逐跳首部过滤和有界队列，省去通用回调 Arc、装箱和首部克隆。需要捕获上下文的策略/其他协议路径保留原回调 API。
- 通过 informational sender 所属的连接复用空 Extensions Map。含有其他元数据的 Map 不进入缓存，缓存始终没有 sender，避免引用环和跨请求泄漏。缓存以每连接保留一份空 Map 换取减少重复分配；申请字节下降不等于 RSS 下降。
- 保留 checkout 时 EOF/意外响应检查、全进程请求/闲置连接预算、后端租约、指标、精确超时及下游 flush 完成边界；取消中的连接不会归还，已发送请求不重放。

实现及其契约见 [vendored Hyper 说明](../vendor/hyper/RGNIX.md)。新增真实套接字回归连续三次转发 103→102→200 并过滤 Connection 指定的首部；库回归覆盖元数据和旧 sender 隔离，以及 1/8/16 KiB 缓冲、流式响应交界和复用条件。

## 指令对照

OrbStack Ubuntu aarch64，Rust 1.90.0 / LLVM 20.1.8，locked release、thin LTO、一个 codegen unit，`http3,jemalloc`。沿用 `scripts/profile_http_instructions.py`：单 worker、单持久客户端、100 次预热，每个 load 扣除完整匹配的启动/预热/停止 control，再除以额外 2,000 次 status/body 精确校验响应。每程序三回合六样本，所有 A/A 对称差及组 CV 都须 ≤1%。

| 负载 | 原 Hyper 指令/请求 | 最终 Hyper | 变化 | NGINX | 最终 / NGINX |
|---|---:|---:|---:|---:|---:|
| 1 KiB | 44,692.23 | 39,554.96 | -11.49% | 23,850.07 | 1.66 |
| 16 KiB | 46,473.27 | 41,337.21 | -11.05% | 28,472.95 | 1.45 |
| 1 KiB + 64 字段 | 102,147.87 | 97,021.35 | -5.02% | — | — |

最终三组所有引擎的最大 A/A 差为 0.107% 以下，最大 CV 为 0.058% 以下。NGINX 1.28.0 ELF SHA 为 `f6543200b866fad21ee74f2e3b753e27c0a5c39c671e95ba001529270471f77d`。NGINX 的非缓冲上游读取仍默认使用本机一页 4 KiB，Hyper 为 8 KiB 起步的自适应缓冲；两版 Hyper 配置相同，但 16 KiB 的跨产品比较包含缓冲策略差异。1 KiB 能放入双方缓冲。

阶段实验分别测量，不把百分比相加：

| 配对实验 / 1 KiB | 基线 | 候选 | 变化 |
|---|---:|---:|---:|
| 只直接转交 1xx / 原版本 | 44,671.65 | 44,475.05 | -0.44% |
| 直接转交 1xx + 单所有者调度 / 原版本 | 45,019.76 | 40,033.55 | -11.08% |
| 空 Map 复用 / 上述调度版本 | 39,712.06 | 39,538.37 | -0.44% |

以 JSON 中完整精度为准。不同实验之间的绝对指令有偏移，收益统一使用实验内匹配对照；最终结论使用第一张表的独立最终对照。

1 KiB 最终配对的平均 flat self 计数中，Tokio sync 从约 2,088.6 降为 666.1 指令/请求，其中请求 mpsc/oneshot 的净计数接近零；jemalloc self 从约 504.6 降为 190.3。分组包含内联归属影响，不能将它们作为可删除预算或相互重叠的贡献相加。全部函数的 load/control 向量、负 delta 与未知符号均保留。

## 分配探针

独立于 Cachegrind，使用原有双 worker fixture、单持久客户端，预热 10 次后精确校验 500 次响应，保留后台活动。采样 GlobalAlloc 的 jemalloc 分配入口；请求字节为累计申请量。

| 场景 | 原分配入口/请求 | 最终 | 原申请 B/请求 | 最终 |
|---|---:|---:|---:|---:|
| 1 KiB | 8.050 | 3.024 | 801.152 | 346.784 |
| 16 KiB | 16.034 | 11.024 | 1,345.312 | 842.784 |

1 KiB 最终稳定分配是 Guard 216 B、sender 装箱 24 B、动态首部名 8 B，各 500 次；不再出现每请求的 140 B 桶存储、回调分配和 208 B oneshot。16 KiB 仍保留原响应体通道。1xx 链由五笔稳定分配减少为一笔，再加去掉 oneshot，普通请求总体由八笔减少为三笔。

## 验证及限制

- 根库 release：32 通过、1 ignored；Hyper：122 通过、6 ignored；hyper-util：38 通过、2 ignored，以及 legacy client 集成 21 通过。
- Linux 真实运行：基础 Hyper 集成 105、双 worker/临时端口 84、产品功能 115、协议（含 HTTP/3）74、入口恢复 57，合计 435；另有 Pingora 集成 105，合计 **540 项通过**。
- 根项目 fmt、strict release Clippy `--all-targets -D warnings`、新增/重写 vendored 文件 rustfmt 通过。依赖 h3 原有九个编译警告保留。完整 vendored rustfmt 的两个既有 SendFile 格式差异已在基线重现，没有扩大改动格式化它们。
- 额外 client-only `rgnix-extensions` 组合在既有 SendFile 错误构造函数 feature guard 处编译失败，基线与候选均重现；生产使用的 server/client 组合及上述运行验证通过，本轮未修复该无关特性组合。
- 产品功能首轮误用了没有 grpc 的协议测试 venv；换用已有依赖的系统 Python 后 115 项通过，没有安装新依赖。16 KiB 初轮最后一个 NGINX control 在 Cachegrind 下退出超时，保留其 17 份完成记录和失败日志；本表使用同参数完整重跑，三个引擎各六份样本均通过门槛。
- 本轮没有 PMU cycles/IPC/c2c、真实 NIC、容量矩阵或 soak 证据；保持既有端到端测量环境资格限制。

最终 ELF SHA：`7f0f12498f8750a5d7af2f890ab3090c9e815cd25f285ac7c9fb3495570dc016`。冻结清单核对全部 461 个源码/脚本文件；独立核对逐函数 self 总和、signed delta、净指令、中位数、A/A、CV 和 ELF 摘要。原始归档位于 `/home/livesite/.cache/rgnix-hyper-instructions3-20261024/raw-evidence.tar.gz`，摘要见 JSON。自有服务、压测进程及 uprobe 事件已清理。

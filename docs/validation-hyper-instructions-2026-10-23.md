# Hyper 指令计数与唤醒诊断 — 2026-10-23

本轮将普通 1 KiB HTTP/1 代理的单 worker 用户态指令数降低 **1.90%**；额外 64 个首部时降低 **0.89%**。两边 A/A 校准均通过，三轮方向一致。**这是指令成本收益，尚未确认吞吐或 P99 提升。**

以 main `7f20b0061dba51b389304b8ba3f79385e09fdd9a` 为基线，先建立单 worker 用户态指令计数，再选择固定成本优化。没有沿用上一轮失效的吞吐比率，也没有将 Redis 移除解释为普通代理提速。

## 测量手段

OrbStack 原生 Linux arm64 的 `perf stat -e instructions true` 返回计数器不支持。安装发行版 Valgrind 3.25.1 后，Callgrind 先导实验能运行，但 `callgrind_annotate --inclusive=no` 将某个函数的自身成本报告为总指令数的 101.7%；退出段也出现超过总数的函数归因。本轮不使用该调用图估算 dispatch、pool 或 oneshot 的可节省上限。[Callgrind 官方说明](https://valgrind.org/docs/manual/cl-manual.html#cl-manual.functionality)明确提示其调用/返回推断依赖架构，在 ARM 上存在限制；这里保留异常，不将其处理为可信函数比例。

改用 [Cachegrind 的 flat instruction counts](https://valgrind.org/docs/manual/cg-manual.html)。新增 `scripts/profile_http_instructions.py` 复用现有 HTTP fixture 和启动/停止工具：单 worker、代理 guest CPU 10、源站 CPU 6、一条持久客户端连接，每次校验 HTTP 200 和完整 1 KiB body。每个样本分别运行相同二进制的 100 次预热控制，以及 100 次预热加 2,000 次请求的负载；两者总指令数相减后除以 2,000。启动/退出和后台工作均包含在原始记录中，控制与负载顺序轮换。Cache/branch 模拟关闭，不把指令数等同于 CPU 时间。

每个 raw profile 的逐函数自身指令之和必须严格等于总数，signed 函数差之和必须等于总差；否则脚本失败。六个独立基线样本的中位数为 **53,857.63 Ir/请求**，CV **0.159%**，三组 A/A 对称差为 **0.044%、0.153%、0.444%**，通过事先采用的 1% 门槛。此前缺少 fixture 参数的启动失败、Callgrind 归因异常和两个 Cachegrind 先导运行均与正式六样本记录分开保留。

## 选择的改动

flat profile 显示首部和哈希工作占有可观成本。先导候选尝试避免不存在的 `keep-alive`、`proxy-connection` 哈希查找，但实测基线 53,912.47、候选 53,943.59 Ir/请求（+0.058%），两边 A/A 虽通过，差异仍低于可解释的幅度，故撤回该代码。进一步查源码确认 HeaderMap 正常路径使用 FNV，不能把该负查找当成 profile 中的 SipHash 来源；自定义字段哈希并不都使用同一种算法。

保留的候选缓存首次选址取得的 prepared-route 索引。普通 HTTP/1 从原来的四次 `(listener, route identity)` 哈希查找降至一次，后续转发、响应头处理和完成计量直接索引同一不可变快照的计划数组，不增加每请求 Arc 克隆。请求仍持有原快照，索引不跨快照使用；完整策略路径继续通过原路由查找接口定位计划。扩展现有慢响应热更新回归，在响应在途时新增一条计划、改变响应首部并切换限流策略，验证旧响应、后续请求和计量分别保持所属快照。

新增 `--hyper-wakeup-probe`，默认关闭。启用后各原生服务以 1 ms 采样计划唤醒延迟，同时由独立线程采样 `service="reference"`；退出时停止并 join 参考线程。新增回归在当前线程 runtime 中同步阻塞 30 ms，验证 reactor 直方图捕获延迟、参考线程在阻塞期间继续计数、探针销毁后参考线程停止。它提供局部阻塞与共同调度延迟的排查线索，不能独立证明宿主暂停；两种定时器粒度不同，不应直接相减。

`record_completion` 已复用同一个 finished Instant，连接 Deadline 已复用 Sleep 并 reset。本轮不为重复实现这些已有机制而重构。也没有同时更改请求预算、租约生命周期、backend.active 计数、Wasm fuel 或客户端 dispatch 语义。

## 保留候选的指令结果

基线 SHA256 为 `ea77a305961260eaaaaf31e68b695830667f3df28e0c3be85921ff1593d53ca2`，候选为 `28270d59d4f777d69ef2b2ae36df313399498b0c697ea02d48d4f4d0ec5f6734`。两者均为 OrbStack 原生 arm64 / Rust 1.90.0 / LLVM 20.1.8，release、thin LTO、codegen-units=1、`http3,jemalloc`。复用已核对的 main 产物作基线；没有用不同工具链或历史 Pingora 产物比较。

每份产物每个场景有三轮、每轮两次独立样本，各自附带一个启动控制。正式 A/B 全部使用默认关闭的唤醒探针，测量期间没有并行执行本轮构建或测试。

| 首部负载 | 基线 → 候选 Ir/请求中位数 | 降低 | A/A 最差配对差：基线 / 候选 | CV：基线 / 候选 |
|---|---:|---:|---:|---:|
| 普通 1 KiB 代理 | 53,887.62 → 52,865.94 | 1.896% | 0.325% / 0.263% | 0.111% / 0.107% |
| 额外 64 个首部 | 110,369.42 → 109,385.89 | 0.891% | 0.138% / 0.041% | 0.063% / 0.059% |

普通场景三轮配对均值分别降低 1.856%、1.844%、1.924%；宽首部分别降低 0.918%、0.962%、0.816%。未剔除窗口，也没有根据某次好结果重跑取代失败记录。

普通场景中，每请求均值的主要函数差如下。这些是按符号归并的自身指令，泛型和内联归属可能跨越组件，不能相加当作完整调用树预算。

| 函数 | 基线 → 候选 Ir/请求均值 |
|---|---:|
| `core::hash::sip::Hasher::write` | 2,655 → 2,157 |
| `core::hash::BuildHasher::hash_one` | 1,178 → 950 |
| `hashbrown::map::HashMap::get_inner` | 240 → 56 |
| `SocketAddr::hash` | 160 → 64 |

按命名函数粗分，legacy client/pool 约 2,589 Ir/请求、channel primitives 约 1,166，前后基本不变；HTTP 首部/解析约 10,366，前后也基本不变。哈希相关命名函数从约 4,768 降至 3,762，符合移除三次 prepared-route 查找的源码变化。上述 self 计数并非客户端的 inclusive 上限，不能据此判断重写客户端最多能省多少或证明其总成本小于 15%。

这轮保留的是可核实的重复工作削减，没有修改 Hyper 协议状态机。真实物理核上的 4/8 worker 吞吐、共享缓存行争用、P99、内核工作及新版本 24 小时长测仍未验证。

## 行为和静态验证

最终候选的 release 库测试 27 通过、1 忽略。OrbStack 下共有 521 项真实运行检查通过：默认 Hyper 基础集成 105、显式 Pingora 105、Hyper 专项 68、产品策略 115、含 HTTP/3 的协议检查 71、Ingress 恢复 57。覆盖新增计划时的在途快照保持、计量恰好一次、配置撤回、策略与认证、HTTP/2、HTTP/3、gRPC、WebSocket、超时、取消、无重放、连接池及排空。

额外启动带 `--hyper-wakeup-probe` 的实际产品进程，检查所有服务与 reference 均产生样本；对该测试进程执行 80 ms SIGSTOP/SIGCONT 后，业务仍返回 200，reactor 和 reference 的 20 ms 以上样本均增加。这是受控进程暂停的探针连通性检查，不是正常请求 P99 或真实宿主停顿的诊断结论。原始前后 metrics 保留。

`cargo fmt --check`、`cargo clippy --release --locked --features http3,jemalloc --all-targets -j4 -- -D warnings`、Python/JSON/Shell/workflow YAML 语法以及 diff 检查通过。本轮未验证原生 amd64、最终 Bookworm 镜像、真实 Kubernetes Gateway 多副本及物理网络。

完整记录见[机器数据](validation/hyper-instructions-2026-10-23.json)。四组正式实验共 42 个差分样本、84 份启动/负载 profile：每份 profile 的完整按函数归并计数用全局 `function_names` 表和同位置的 `function_counts` 整数数组无损保存，可独立核对总数、重建 signed 差及校准。Cachegrind 原始行级文件、无效 Callgrind 先导和归档 SHA 另外保留在本地；代码、实际命令、fixture、二进制和源文件摘要随机器记录保存。42 个差分样本含已撤回的首部试验与独立 A/A，没有丢弃失败假设。

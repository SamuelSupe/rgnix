# Hyper 空闲检查与 HTTP/1 监视器精简（2026-10-24）

以 `cb9f67a` 生产源码、`ff09d36` 记录的冻结 ELF 为基线，最终实现 `b7c8858`。本轮 **1 KiB / 16 KiB 用户态指令减少 0.94% / 0.93%，宽首部减少 0.37%**，全部最终 A/A 与 CV 通过 1% 门槛。没有新的吞吐或 P99 结论。

[完整逐样本、逐函数、构建及验证记录](validation/hyper-instructions4-2026-10-24.json)同时保留撤销的试验、失败和重跑记录。本轮只做本地提交，未合并 main 或推送 GitHub。

## 保留的实现

- 普通 H1 连接池 checkout 改用拥有连接的 `poll_idle`。只有没有活动请求/响应体、读写状态处于 idle、写缓冲为空、上一次 transport flush 已完成时，才直接通过原 buffered IO/TLS reader 检查 EOF 或意外数据，省去完整 dispatcher 的 write/flush 等工作。忙碌连接、待完成 flush 和 EOF 走原驱动。
- 确认协议为 HTTP/1 后，HTTP/2 首部监视器直接转交读取，省去已停用监视器的计时检查、缓冲长度计算和观察调用。h2c 升级后仍新建 HTTP/2 监视器；H2 的首部超时保持原逻辑。
- 保留原 codec、超时、指标、全局预算、取消、背压与不重放契约。`is_ready` 仍表示原有帧就绪语义，不把它改成 transport flush 完成承诺；flush 条件只约束新 idle 快路径。

新增受控 pending-flush 回归：第一份响应帧完成时仍阻塞 transport flush，解除阻塞后 idle 检查必须继续驱动 flush。移除 `&& self.dispatch.flushed` 的对照以明确断言失败（exit 101），保留条件则通过；对照的整个 inline 源文件与最终版本只差这个条件。既有库回归改用 `poll_idle`，覆盖忙碌 body 的回退及 1 KiB / 8 KiB / 8193 B / 16 KiB 的帧完成与复用。真实 socket 回归继续验证关闭的 idle 上游、取消流式 body、h2c、H2/H3 和精确超时。

## 最终独立对照

OrbStack Ubuntu aarch64，Rust 1.90.0 / LLVM 20.1.8，locked release、thin LTO、一个 codegen unit，`http3,jemalloc`。沿用 `scripts/profile_http_instructions.py`：单 worker、单持久客户端、100 次预热；每个 load 扣除独立的完整启动/预热/停止 control，再除以额外 2,000 次 status/body 精确校验响应。每程序三回合六样本；每个 A/A 对称差和组 CV 均须 ≤1%。不把各阶段收益相加。

| 负载 | 原 Hyper 指令/请求 | 最终 Hyper | 变化 | NGINX | 最终 / NGINX |
|---|---:|---:|---:|---:|---:|
| 1 KiB | 39,524.43 | 39,151.57 | -0.94% | 23,848.39 | 1.64 |
| 16 KiB | 41,333.63 | 40,948.87 | -0.93% | 28,473.52 | 1.44 |
| 1 KiB + 64 字段 | 97,016.36 | 96,653.08 | -0.37% | — | — |

最终最大 A/A 对称差 **0.193%**，最大 CV **0.096%**。NGINX 1.28.0 ELF SHA 为 `f6543200b866fad21ee74f2e3b753e27c0a5c39c671e95ba001529270471f77d`。NGINX 非缓冲上游读取默认本机一页 4 KiB，Hyper 保持 8 KiB 起步的自适应缓冲；16 KiB 跨产品比较包含缓冲策略差异，两版 Hyper 配置相同。

1 KiB 平均 flat self 中，停用监视器的 `HeaderIo::check` 约 108 指令/请求降为零，dispatcher `poll_write` 和 buffered `poll_flush` 分别减少约 66 和 55。其他符号存在内联归属变化，例如原 `InlineConnection::poll` 的计数部分转移到 `poll_catch`；不能把函数消失等同于全部开销被删除，也不能将重叠分组相加。净收益以上表完整程序计数为准。1 KiB 仍为 NGINX 的约 1.64 倍，本轮只削减重复 idle 工作。

## 撤销的试验

| 首部名试验（均含 idle / 监视器优化） | 1 KiB 相对原版本 | 宽首部相对原版本 | 决定 |
|---|---:|---:|---|
| 常见标准字段的直接匹配 | -1.97% | +0.33% | 撤销 |
| 先筛首字母，再匹配标准字段 | -1.63% | +0.50% | 撤销 |
| 首字母 / 长度分派及 ASCII 比较 | 未继续测量 | +0.67% | 宽首部门槛失败即停止并撤销 |

这些宽首部测量的 A/A/CV 自身均通过；回退来自这组代码在该负载上的额外工作，不能忽略。最终完全恢复原 `role.rs` 首部名处理，未保留特殊字段表及其测试。

还试验过延迟布防 HTTP/1 header timer。独立比较中，撤掉该试验的差别仅 +0.056%，不足以支持增加逻辑；最终保留原计时器实现。早期阶段普通 1 KiB 的小收益不作为最终组合收益；最终结论使用上表单独完整配对。

## 分配、验证与限制

- 独立 jemalloc uprobe：双 worker、单持久客户端，10 次预热后校验 500 次响应，含后台活动。1 KiB 两版均 3.024 次分配入口/请求、346.784 B/请求，三笔稳定申请仍是 Guard 216 B、sender 装箱 24 B、动态首部名 8 B。16 KiB 的每请求稳定申请也相同；总计从 11.056 到 11.024 次及申请字节差别包含后台缓冲活动，不能宣称本轮减少了请求分配或 RSS。
- 根库 release 32 通过 / 1 ignored；Hyper 123 通过 / 6 ignored；hyper-util 库 38 通过 / 2 ignored，legacy client 集成 21 通过，合计 **214 项通过**。
- Linux 真实运行：基础 Hyper 集成 105、双 worker/临时端口 84、协议含 HTTP/3 74、产品功能 115、入口恢复 57；另有显式 Pingora 集成 105，合计 **540 项通过**。批处理脚本曾将一次重复 Hyper 集成标为 Pingora；已用显式 `RGNIX_ENGINE=pingora` 重跑通过，重复项不计入 540。
- 根 fmt、strict release Clippy `--all-targets -D warnings`、修改的 inline 和公开连接 API 文件 rustfmt 通过；依赖 h3 原有九个编译警告保留。没有顺带格式化原有 SendFile 区块或修复历史 client-only feature 组合问题。
- 冻结清单核对 461 个源码/脚本文件，独立 vendor 测试副本核对其中 121 个文件；核验全部 ELF 摘要、逐函数 self 总和、signed delta、净指令、中位数、A/A 和 CV。早期组合试验的 NGINX shutdown 超时和已完成部分样本保留，最终三组没有重跑或删样本。
- 本轮没有 PMU cycles/IPC/c2c、容量矩阵、P99、真实 NIC 或 soak 验证；共享宿主的既有环境资格限制继续适用。指令下降不能直接换算吞吐提升。

最终 ELF SHA：`e7e3f7be240ada8aa48d8b0f786683e25a76465cee31e55e01272ebb4cb8b9d9`。原始归档位于 `/home/livesite/.cache/rgnix-hyper-instructions4-20261024/raw-evidence.tar.gz`，摘要见 JSON。所有自有服务、测量进程及 uprobe 事件已清理。

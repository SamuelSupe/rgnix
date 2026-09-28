# 第八轮性能优化：请求状态、连接池、静态文件与 RGL

2026-09-27。在第七轮工作区版本上实现四类优化，保留既有功能、安全边界和用户改动。本轮没有创建提交、Release 或镜像；GitHub v0.4.0 不包含这些工作区改动。

## 结论与吞吐对照

四类固定成本优化已落地，但普通代理吞吐差距没有解决，**性能发布/容量验收仍未通过**。下表是本次共享虚拟机上的观测中位数，不是稳定提升承诺；A/A 对照的失败使小幅变化不能可靠归因于代码。

双 worker、64 连接、每组 3 轮，各测量 8 s，预热 3 s：

| 场景 | 第七轮 req/s | 第八轮 req/s | 观测变化 | NGINX req/s | NGINX / 第八轮 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2 B 直接响应 | 154,029 | 200,456 | +30.1% | 483,317 | 2.41× |
| 1 KiB 代理 | 88,572 | 88,022 | -0.6% | 181,363 | 2.06× |
| 16 KiB 静态文件 | 85,690 | 119,966 | +40.0% | 280,688 | 2.34× |
| 请求头分流 | 66,231 | 68,929 | +4.1% | 197,599 | 2.87× |

普通代理的 CPU 成本也基本未变（22.47→22.54 µs/请求）；直接响应 10.73→9.48、静态文件 22.22→15.24、请求头分流 29.63→28.57 µs/请求。复制/分配下降不能自动换算成同等吞吐提升。RGL 每请求仍保留独立 Store/instance 及预算检查，未通过牺牲隔离语义追平 NGINX 的原生 map。

256 连接仅补测第七/八轮各 2 轮，不作为新的 NGINX 高并发对照；下列 P99 是窗口 P99 的中位数，不是合并请求直方图的 P99：

| 场景 | 第七轮 req/s | 第八轮 req/s | 第七轮 P99 ms | 第八轮 P99 ms |
| --- | ---: | ---: | ---: | ---: |
| 2 B 直接响应 | 199,238 | 227,869 | 4.939 | 1.912 |
| 1 KiB 代理 | 68,994 | 75,347 | 10.889 | 6.801 |
| 16 KiB 静态文件 | 80,512 | 108,607 | 19.994 | 3.796 |
| 请求头分流 | 43,139 | 46,646 | 19.901 | 10.795 |

52 个正式新旧/NGINX 对照窗口完成 **62,827,894 请求、零错误**；另 12 个最终二进制 A/A 窗口完成 7,498,054 请求、零错误。请求总数不含预热和独立上游基线。

同一第八轮二进制分别放在 `rgnix` 与 `candidate` 槽位，普通代理 64/256 连接各做 3 组交错 A/A。预先沿用的 10% 对称配对差异及组内 CV 门槛失败：64 连接最大配对差异 **44.63%**，256 连接 **37.84%**、最大 CV **24.37%**。某次 256 连接 P99 达 **128.543 ms**，同组另一窗口为 **6.491 ms**。NGINX 对照也出现 24.180 ms 的代理 P99 与 45.366 ms 的静态 P99。没有删除这些窗口，也未重新抽样至通过；异常原因未定位，不能简单归因于虚拟机。

### 环境与公平性边界

- OrbStack Ubuntu 25.10 / aarch64，Linux 7.0.14，Rust 1.90.0；本轮未重启 OrbStack、未改 sysctl、未停止系统 NGINX（PID 494）。临时独立 network namespace 中运行；退出后测试服务和本轮 uprobes 均已清理。
- rgnix 与 NGINX 1.30.5 均 2 workers，服务 CPU 10–11；wrk 4 线程固定 CPU 2–5；共同 OpenResty 1.31.1.1 上游 4 workers 固定 CPU 6–9。CPU affinity 不等于独占物理 CPU。OpenResty 本轮只作为 origin，没有重新比较其前端。
- 明文 HTTP/1.1 keepalive；禁用访问日志/OTLP 输出，保留 rgnix 正常指标、预算和安全检查。NGINX 开启 sendfile。静态测试目录 `/tmp` 实际是 tmpfs，结果不代表冷盘、网络文件系统或 HTTPS。
- 请求头分流返回等价结果：NGINX 用原生 map，rgnix 用 RGL/Wasm；二者的执行与隔离成本不同。每个正式窗口重新启动前端，顺序交错；所有生成配置、命令、CPU/PSS、预检和上游基线均保存在原始 JSON。

## 已实现的变化

- **HTTP 请求生命周期**：HTTP/1 与 HTTP/2 分派接收现有的 boxed session，Session 和 Context 在进入异步处理前即放入稳定位置，完成写入及请求体排空后再提取可复用连接。保留原有按值入口的默认兼容实现，减少大型异步状态在不同入口之间的搬运。
- **连接池与计时**：用有总容量上限的 peer 队列代替每次归还时的 Arc/Mutex、通知对象和 idle task；每个 connector 只有一个弱引用维护任务，100 ms 扫描一次。取用时仍立即检查精确超时、连接身份、EOF 与意外数据；保留 worker/TLS 信任策略的复用隔离。完成钩子复用一个时间点记录各类耗时，未降低指标精度或关闭指标。
- **静态文件**：Linux 明文 HTTP/1、无压缩时，使用内核缓存路径解析、固定 inode 校验与 sendfile。每个请求重新检查文件，未引入内容缓存或过期窗口。Range、HEAD、条件请求、目录索引、越界符号链接及 FIFO 防护保留；TLS、HTTP/2、压缩、不支持的传输或路径保留原处理方式。
- **RGL**：新编译的字符串字面量放入只读常量区，用新增的 `rgnix_v1.constant` import 取受限句柄，不再为这些字面量创建和恢复可写 Wasm 线性内存。仍然每请求独立 Store/instance，保留 fuel、宿主预算、UTF-8/范围校验与失败回滚。旧 `.wasm` 的 `literal`/memory 接口继续有效，旧可写内存的重置语义保留。

连接池的淘汰策略现在是从最近最少使用的 peer 组中移除最老连接，总连接容量不变。空闲连接物理关闭可能延后到下一次维护扫描，但过期连接不能在扫描间隙被借出。sendfile 的冷数据读取仍可能等待内核文件 I/O，本轮没有把它描述为异步磁盘 I/O。

新常量 import 是向后兼容的宿主扩展：新服务器接受旧产物，旧服务器会拒绝使用新 import 的产物。滚动升级应先升级服务器，或继续使用最旧在运行编译器生成的 `.wasm`；RGL 源码由各服务器自行编译。

## 固定成本的实测变化

所有诊断在 OrbStack 原生 Linux 临时网络命名空间执行，与正式吞吐压测分开。下列 libc 复制只覆盖被探针捕获的 memcpy/memmove，不含编译器内联复制、内核复制，也不是总内存带宽。

| 每请求，500 个预热后请求 | 第七轮 | 最终候选 | 变化 |
| --- | ---: | ---: | ---: |
| 2 B 直接响应，捕获复制字节 | 40,148 | 18,444 | −54.1% |
| 1 KiB 代理，捕获复制字节 | 58,600 | 33,588 | −42.7% |
| RGL 请求头分流，捕获复制字节 | 128,093 | 37,246 | −70.9% |

原先每请求一次的 15,056 B 分派复制消失；RGL 的 500 次请求/500 次 65,536 B 内存恢复复制降为零。最终三个采集窗口的解码事件数与记录样本数逐一一致，全部零丢样；基线采集也已独立检查。第二次调整将 Session/Context 的装箱提前，进一步减少约 4.9 KiB/请求的复制；初版数据与最终数据分开保留。

堆分配事件以四个持久连接、2,000 个成功请求减去独立启动对照统计。普通代理从 106.39 降至 101.15 次/请求，RGL 从 151.37 降至 137.28 次。直接响应从 52.08 增至 54.08，静态文件从 61.09 增至 64.08：装箱和安全路径处理增加小对象，不能宣称所有场景的分配次数都下降。累计申请字节分别从 32,413→26,195（直接响应）、115,929→108,594（代理）、49,176→26,407（静态）、122,903→114,725（RGL）B/请求。累计申请字节、常驻内存和 libc 复制是不同指标。

最终热静态文件每请求：内容 read 约 1→0，sendfile 0→1，statx 2→1，futex 约 1.60→0.00014，上下文切换约 1.01→0.0054。根目录、inode 固定和安全重开仍需两次 openat2、一次 openat，未删掉这些检查追求与 NGINX 相同的打开次数。

时钟调用改善较小：直接响应约 12.1→11.1 次，代理 28.7→28.7 次，RGL 32.2→31.2 次/请求。它不是本轮主要收益。最终代理与 RGL 诊断窗口的上游复用率均超过 99.99%，没有通过减少正确性检查、自动重放请求或关闭观测功能获得这些数字。

## 功能验证与实际边界

- `cargo test --release --locked`：最终应用/RGL 源码的 22 项通过，1 个原有路由微基准保持 ignored；含常量区/旧线性内存 ABI、UTF-8 边界、fuel、宿主预算、内存/全局变量隔离。
- 最终二进制的 HTTP 集成：93 项通过。覆盖 TLS、HTTP/2、WebSocket、SSE、gRPC、流式请求、超限与截断、上游断连不重放、取消、热更新与排空。新增连续文件原子替换与跨缓冲边界 Range 检查。
- 最终二进制的产品功能：114 项通过。覆盖认证、并发/限流、压缩、健康检查、上游 TLS 信任与持久化发布等既有行为。
- vendored core：32 个不同的连接池/协议测试通过，包含容量、超时、意外上游数据、fd 身份不匹配、HTTP/2 身份检查、HTTP/1 过读及流水线边界。使用临时隔离的 vendor 测试工作区，其开发依赖锁从本机缓存解析；生产构建始终使用项目 Cargo.lock。
- release all-targets Clippy 拒绝 warning，通过；root Rust 格式检查和 diff 空白检查通过。

共 261 项不同检查通过。根测试在候选 D 完成；随后候选 E 仅修正 vendor 连接池的两个边界，E 重新完成 32 项 core、207 项端到端检查和 release all-targets Clippy。中途发现两个测试环境/时序问题：新原生目录缺少 `.local` 输出目录；读取响应可能早于前一次请求的完成计数，导致请求体字节的精确差值包含上一请求。补齐目录，并在读取基线前等待 inflight 许可释放，保留原来的严格字节断言。

最终编译器复核还发现并修复了 `constant` 普通函数名的兼容性回归：旧编译器可编译，中间候选因新增内部 import 的名字而拒绝。编译器现在仅从内部生成该宿主调用，源代码中的同名普通函数保持有效。既有函数/分支/响应钩子测试已扩展覆盖这一场景，最终 CLI 编译也通过。

本轮没有重跑真实 Kubernetes/Gateway 集群、amd64、专门 OTLP/XDP、冷盘与网络文件系统压力、长期内存/容量验收。常用服务及协议行为有运行验证，不将这些检查等同于上述未运行的验收。

连接池收尾补充了两个具体回归：满索引中有空队列时，不得整组淘汰仍有有效 idle 连接的 peer；checkout 等待锁之后必须使用新的时间判定超时。两个测试在中间实现分别失败，修正后通过。最终实现先回收空索引，并在取出连接后读取时钟，避免把锁等待前的时间用于有效性判断。

## 产物、版本与复现

本轮是在已有未提交的第七轮工作区上修改，HEAD 仍为 `464a43a4e6f40bd2b219848b6b54237d7048d8d6`；不能用 HEAD 单独重建该基线。保留了修改前输入备份和本轮差异，没有覆盖无关改动。

| 二进制 | SHA-256 |
| --- | --- |
| 第七轮基线 | `a5ae6cb44ba537b18e2c4995438aafd51827cdbbdc0f2af87ac06bb5c2ee36e3` |
| 第八轮最终候选 E | `d034acf2e68a3fb0cf1b4b7b7854f453c7b16519f9c2319f5c92bd8a6c5c5d1e` |
| NGINX 1.30.5 | `d7509a23828ead5af789b69f2042a2eae74d25f740ef3c071dbc35c1b0f41304` |

本机最终副本：`.local/optimization8-20260927/rgnix-round8-linux-arm64`；原生环境最终副本：`/tmp/rgnix-opt8-20260927/rgnix-candidate`。两者摘要一致。249 个应用、vendor、构建及脚本输入的本机/Linux 摘要逐一一致，清单包含在回归记录中。

- [最终 A/A 原始数据](validation/performance-round8-aa-2026-09-27.json)
- [64 并发新旧及 NGINX 原始数据](validation/performance-round8-http-2026-09-27.json)
- [256 并发新旧原始数据](validation/performance-round8-tail-2026-09-27.json)
- [中位数、方差门槛与异常窗口](validation/performance-round8-analysis-2026-09-27.json)
- [复制、分配与系统调用诊断](validation/performance-round8-diagnostics-2026-09-27.json)
- [回归清单、日志及源码摘要](validation/performance-round8-regression-2026-09-27.json)

中间候选 B/C/D 的 A/A、D 的完整比较，以及修正前失败日志保存在 `.local/optimization8-20260927/` 和原生 `/tmp/rgnix-opt8-20260927/`。候选 C 对照在 7 窗后因发现 `constant` 函数名回归而中断，其数据不计入最终结果。最终表格和正式窗口全部来自候选 E，未混用较好看的中间候选。

原生 Linux 源码在 `/tmp/rgnix-opt8-20260927/source`。生产构建使用 `cargo build --release --locked`；项目 release profile 为 thin LTO、1 codegen unit、strip debuginfo。可在独立命名空间复现相同配置，路径按本机调整：

```sh
sudo unshare -n bash
ip link set lo up
cd /tmp/rgnix-opt8-20260927/source
python3 scripts/benchmark_compare.py \
  --rgnix /tmp/rgnix-opt7-20260927/rgnix-final \
  --candidate /tmp/rgnix-opt8-20260927/rgnix-candidate \
  --nginx /tmp/rgnix-compare-20260927/nginx/sbin/nginx \
  --openresty /tmp/rgnix-compare-20260927/openresty/bin/openresty \
  --engines rgnix candidate nginx --cases return proxy-1k static header \
  --workers 2 --server-cpus 10,11 --client-cpus 2,3,4,5 --origin-cpus 6,7,8,9 \
  --seconds 8 --warmup 3 --concurrency 64 --rounds 3 --interleave \
  --work-dir /tmp/rgnix-opt8-reproduce --output /tmp/rgnix-opt8-reproduce.json
```

A/A 将 `--rgnix` 改为候选相同路径，`--engines rgnix candidate --cases proxy-1k --concurrency 64 256 --rounds 3`；高并发尾延迟比较保留不同二进制，`--engines rgnix candidate --concurrency 256 --rounds 2`。每次使用不同 work/output 路径，并串行运行。

诊断脚本及原始 perf/heaptrack 文件在原生 `diagnostics-e/`，`e-copies.py`、`e-allocations-direct.py`、`e-diagnose.py` 分别采集 libc 复制、堆分配、系统调用；最终数字的计算和采样完整性核对脚本保存在 `.local/optimization8-20260927/finalize-diagnostics.py`。诊断与正式吞吐运行分开，未把插桩后的吞吐用于上表。

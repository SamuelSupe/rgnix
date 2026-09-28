# 第五轮性能优化：哈希分流与粘性会话（2026-09-27）

本轮消除后端选择中的重复地址格式化、重复键哈希及重复评分，保留原有加权 rendezvous 哈希算法和会话落点。基线为[第四轮](validation-performance-round4-2026-09-27.md)最终二进制。源码尚未提交或发布，v0.4.0 下载和镜像不包含本轮改动。

**单 worker、64 后端哈希分流的吞吐中位数提升 70.4%，CPU/请求下降 41.5%；256 后端吞吐提升 146.2%，CPU/请求下降 59.4%。** 双 worker 的 64 后端场景吞吐提升 89.9%。普通单后端代理基本持平。收益适用于本次哈希/粘性分流场景，不代表所有流量或相对 NGINX 的整体提升。

## 实现与兼容性

生产变更集中在 `src/backend.rs`：

- 创建端点状态时缓存 `SocketAddr` 的文本表示，DNS 刷新为新增端点创建缓存，相同地址继续复用原状态。
- 每次选择后端时计算一次 `key + NUL` 的 SHA-256 前缀，随后克隆哈希状态并追加候选地址。
- 每个候选计算一次最终评分；旧实现的比较器会对两个候选重复计算哈希和对数。N 个可用端点时，评分次数从 `2(N−1)` 降为 `N`。
- 单端点快捷路径、权重、健康检查、被动故障摘除、DNS 更新及 Lease 并发额度保持原语义。哈希输入字节和浮点评分公式不变，没有引入新的哈希算法或配置项。

键长为 K、候选数为 N、地址文本长为 A 时，重复哈希的输入处理从 O(N × (K + A)) 降为 O(K + N × A)。候选遍历仍为 O(N)，后端池互斥锁和候选 Vec 仍然存在。每个端点增加一份地址字符串；未单独测量其常驻堆开销，不能由临时分配下降推导进程内存下降。

## HTTP 对照结果

[单 worker 原始记录](validation/performance-round5-http-2026-09-27.json)，三轮中位数。短键为 32 字节，64 个不同键轮换；端点权重按 1–5 循环。每窗启动新进程，两版交替先后，预热 3 秒，测量 8 秒。

| 场景 | 后端数 | 吞吐 req/s：旧 → 新 | 吞吐变化 | CPU µs/请求：旧 → 新 | P99 ms：旧 → 新 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 普通轮询代理 | 1 | 55,723 → 55,973 | +0.4% | 17.90 → 17.82 | 1.986 → 1.981 |
| header 哈希 | 8 | 44,711 → 48,792 | +9.1% | 22.25 → 20.34 | 2.357 → 2.424 |
| header 哈希 | 64 | 20,796 → 35,435 | +70.4% | 48.08 → 28.12 | 4.579 → 2.948 |
| header 哈希 | 256 | 7,644 → 18,819 | +146.2% | 130.53 → 52.94 | 12.621 → 4.988 |
| 粘性会话 cookie | 64 | 21,044 → 35,691 | +69.6% | 47.56 → 27.94 | 4.351 → 2.927 |
| 长 header 键，压力场景 | 64 | 958 → 26,141 | 27.3× 原吞吐 | 1046.66 → 38.21 | 101.837 → 3.588 |

普通代理的 +0.4% 小于 A/A 波动，不认定为提升。8 后端新版 P99 中位数略高，且第二轮出现 **3.994 ms** 的单窗 P99（旧版三个窗口为 2.274–2.418 ms）；保留为未解释的尾延迟波动，没有据此认定尾延迟改善。此前其他轮次的 256 并发等尾延迟问题也不由本轮结果关闭。

长键为 **4,128 字节**（4 KiB 前缀加 32 字节键），用于暴露重复哈希随输入长度放大的成本。该压力场景不混入普通请求的收益百分比。

[双 worker 原始记录](validation/performance-round5-two-workers-2026-09-27.json)：64 后端短键哈希，三轮中位数从 **24,581 → 46,674 req/s（+89.9%）**，CPU 从 **58.33 → 35.96 µs/请求（−38.3%）**，P99 从 **7.300 → 3.550 ms**。它仍未达到单 worker 吞吐的两倍，不能认为共享状态和运行时的扩展性瓶颈已经解决。

## A/A、错误与环境边界

两次 A/A 均在对应 A/B 前执行，与各自 A/B 使用同一最终二进制、worker 数和测试方式。延续此前门槛：最大成对吞吐差异和最大组内 CV 各不超过 10%，请求错误为零。

| A/A 范围 | 窗口数 | 最大成对差异 | 最大组内 CV | 门槛 |
| --- | ---: | ---: | ---: | --- |
| 单 worker，普通代理和 64 后端哈希 | 12 | 2.69% | 1.24% | 通过 |
| 双 worker，64 后端哈希 | 6 | 8.99% | 2.74% | 通过 |

原始记录：[单 worker A/A](validation/performance-round5-aa-2026-09-27.json)、[双 worker A/A](validation/performance-round5-aa-two-workers-2026-09-27.json)。双 worker 差异接近门槛，应保留这一限制。门槛只判断本次所列场景的短时测量波动，不是全产品、所有并发或尾延迟的发布验收。

合计 **60 个正式窗口、17,329,928 次请求，HTTP 状态/连接/读写/超时错误 0**。这个计数不包含预热、上游直连和前置验证。每窗另外对全部 64 个键检查 HTTP 200、完整 1 KiB 响应体，以及按旧哈希公式推导的实际目标地址，共 3,840 次前置请求检查。

- OrbStack Ubuntu / Linux 7.0.14 / aarch64，Rust 1.90.0；release、thin LTO、单 codegen unit，Cargo.lock 未变。
- HTTP/1.1 keepalive，64 并发、1 KiB 响应；单 worker 固定 VM CPU 10，双 worker 固定 10,11；wrk 两线程固定 2,3；NGINX 1.30.5 上游两 worker 固定 6,7。
- 每个端点使用不同 loopback 地址，但共享一个 NGINX 上游进程；它们不是独立 Kubernetes Pod。本轮不需要创建集群资源，也没有重做 Ingress/Gateway 控制面规模测试。
- 上游直连约 55–63 万 req/s，长键约 25.5 万 req/s，有足够余量。访问日志和 OTLP 导出关闭，正常 rgnix 指标及代理处理保留。
- 构建、回归测试和 Heaptrack 没有与正式吞吐窗口并行。本轮未重做 NGINX/OpenResty **前端**对比，也不能据此宣称整体超过它们。

## 分配次数与正确性

[Heaptrack 原始计数](validation/performance-round5-allocation-2026-09-27.json)。每版、每个场景各启动一个零业务请求控制进程，以及一个处理 2,000 次请求的进程；四条持久连接轮换相同键，检查完整响应体。下表为扣除各自同配置启动控制后的近似分配事件数，不是吞吐、mmap 次数或常驻内存。

| 场景 | 旧版分配/请求 | 新版分配/请求 | 降幅 |
| --- | ---: | ---: | ---: |
| 普通单后端代理 | 110.34 | 110.32 | 约持平 |
| 8 后端哈希 | 141.33 | 113.30 | 19.8% |
| 64 后端哈希 | 368.71 | 116.60 | 68.4% |

[验证记录与源码摘要](validation/performance-round5-regression-2026-09-27.json)：**221 项检查通过**，包括 Rust 18、HTTP 集成 92、产品功能 111。新增行为测试覆盖 6,150 组旧公式落点对照，以及 256 组故障摘除后的后备落点；包含 Hash/Sticky、空键、UTF-8/NUL、长键、IPv4/IPv6 scope ID、权重和零权重。另检查未知/失败主动健康状态、被动摘除及恢复、Lease 并发限额与释放；原有 DNS TTL 更新和旧 Lease 测试通过。

Release all-targets Clippy（拒绝 warning）、格式和 diff 空白检查通过。247 个源码、清单及既有测试工具文件在 macOS 工作区与 Linux 构建副本的摘要一致；相对第四轮，源码仅变更 `src/backend.rs` 和拆出的 `src/backend/tests.rs`。未重复专门的 OTLP 套件、完整 Kubernetes/Helm/XDP、amd64、长时间浸泡或物理主机容量测试。测试进程已停止，原有系统 NGINX 未改动。

## 复现与产物

[本轮压测脚本](validation/performance-round5-fixture-2026-09-27.py)复用 `scripts/benchmark_compare.py` 的采样、错误统计和进程清理功能；要求指定全新的原生 Linux 目录，以及可用的 CPU 2,3,6,7,10（双 worker 还需 11）。

```bash
python3 docs/validation/performance-round5-fixture-2026-09-27.py \
  --source "$PWD" --baseline /path/to/round4/rgnix \
  --candidate /path/to/round5/rgnix --nginx /path/to/nginx \
  --work-dir /tmp/rgnix-round5-ab
```

相同命令使用新目录并加 `--aa --cases rr-1 hash-64` 可重做单 worker A/A。双 worker 使用新目录和 `--workers 2 --cases hash-64`，其 A/A 再加 `--aa`。默认每组交替三轮，每轮预热 3 秒、测量 8 秒。不要同时运行这些任务。

[分配测量脚本](validation/performance-round5-allocation-fixture-2026-09-27.py)在 A/B 完成后运行，读取其 `results.json`；需要现有 `/usr/lib/heaptrack/libheaptrack_preload.so`：

```bash
python3 docs/validation/performance-round5-allocation-fixture-2026-09-27.py \
  --fixture /tmp/rgnix-round5-ab/results.json
```

| 产物 | SHA-256 |
| --- | --- |
| 第四轮基线二进制 | `733929ac86ccd7aceb453e5acb3ad7932cc61e4a81eeff1af04305139f79ae93` |
| 本轮最终二进制 | `e96732c1e484e4a76627e51eaa5e6307ef4b17c7b333f80474f09c29507e99bb` |
| Cargo.lock | `a0e1e51c67cb7828e8608875effd2078e2c37881d84f63e76b0ec25574bd0d88` |
| NGINX 上游二进制 | `d7509a23828ead5af789b69f2042a2eae74d25f740ef3c071dbc35c1b0f41304` |

最终二进制在工作区 `.local/optimization5-20260927/rgnix-round5-linux-arm64` 保留了核对摘要的副本；基线位于 `.local/optimization4-gateway-20260927/rgnix-round4-linux-arm64`。原生 Linux 运行记录位于 `/tmp/rgnix-opt5-20260927/`，原始 JSON 及脚本已复制到本报告的链接中。本轮没有创建 Git 提交、GitHub Release 或发布镜像。

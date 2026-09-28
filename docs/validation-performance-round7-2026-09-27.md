# 第七轮性能优化：后端选路表与 HTTP 健康探测复用

日期：2026-09-27。基线为第六轮最终二进制，本轮改动尚未发布。

收益集中在大量 HTTP 主动健康检查端点的场景。在**单 worker、显式空闲池预算 1,024、256 个端点、64 并发**下，最终版本三轮吞吐中位数提升 **66.3%**，CPU/请求下降 **40.4%**，P99 从 **6.239 ms 降至 2.419 ms**。该组吞吐 A/A 校准通过；普通单端点代理基本持平。

**默认空闲池、双 worker 的性能稳定性仍未通过验收**：最终版本 A/A 相邻窗口最大差异 13.24%，并出现一个 16.074 ms 的 P99。下面保留全部结果，不能把局部收益当作通用容量承诺或新的 NGINX/OpenResty 前端比较。

最终二进制的 42 个正式窗口共完成 **21,517,483 个请求，HTTP/连接错误均为 0**；227 项 Rust/原生检查通过。预热、正确性预检、分配诊断、CPU 采样和中间候选不计入这个请求总数。

## 实现

- [后端选路](../src/backend/selection.rs)复用健康端点与累计权重表。稳定状态下，轮询从每请求扫描、汇总、分配列表改为二分查找；哈希/粘性分流仍逐请求计算原来的评分，最少连接仍读取当前活动计数。
- 健康状态变更通过版本号使表失效；重建前读取版本，避免丢失扫描期间的并发更新。被动摘除记录最早冷却截止时间，恢复不依赖下一次维护任务；DNS 更新或过期撤销清空旧表。最少连接的选择与名额预留仍在同一把锁内，哈希评分仍在锁外，原有并发上限继续使用原子预留。
- [HTTP 健康探测](../src/backend.rs)在每个后端组内复用一个成功构建的客户端。URL 直接指定目标 IP/端口，保留配置的 Host；每次请求设置超时，仍禁用代理环境变量、重定向和空闲连接复用。客户端构建失败不缓存失败结果。HTTPS 探测保留原来的按端点解析覆盖、SNI、证书与主机名验证路径。

选路表空间随端点数增长，不按权重展开；健康变化时仍需 O(N) 重建。每个请求继续持有自己的 Lease，DNS 发布后的旧选择和旧请求可以完成。单端点保留直接判断路径。没有增加 CLI 参数或依赖，空闲池默认预算仍为每 worker 128。

## 最终版本：单 worker，显式池预算 1,024

[A/A 原始记录](validation/performance-round7-aa-2026-09-27.json)：同一二进制、两个场景、各三轮。最大相邻差异 **6.14%**，最大组内吞吐 CV **2.91%**，通过预先采用的 10% 阈值。这是吞吐校准，不是尾延迟或长期容量验收。

[新旧交替测试](validation/performance-round7-http-2026-09-27.json)，表中为三轮中位数；PSS 为每窗峰值的中位数：

| 场景 | 请求/秒，旧 → 新 | CPU µs/请求，旧 → 新 | P99 ms，旧 → 新 | 峰值 PSS MiB，旧 → 新 |
| --- | ---: | ---: | ---: | ---: |
| 普通单端点 | 58,011 → 57,591 | 17.18 → 17.28 | 1.915 → 1.931 | 28.69 → 28.86 |
| 256 端点，HTTP 主动健康检查 | 30,514 → 50,732 | 32.85 → 19.57 | 6.239 → 2.419 | 55.84 → 36.43 |

健康检查场景吞吐 **+66.3%**、CPU/请求 **−40.4%**、P99 **−61.2%**、峰值 PSS **−34.7%**。两端使用同一个显式连接池预算；正式窗口中新建数据连接接近零，健康探测仍按 10 秒周期运行。PSS 数据不等于长期内存上限。

普通单端点吞吐 **−0.7%**，处于该组校准波动内；新版本普通代理单窗最高 P99 为 2.302 ms，旧版本为 1.921 ms，不能据此声称所有尾延迟都改善了。这组 A/A 与 A/B 共 24 窗、10,042,552 个请求，零错误。

## 最终版本：双 worker，默认池预算 128

[轮询 A/A](validation/performance-round7-default-aa-2026-09-27.json)最大相邻差异 **13.24%**，最大组内吞吐 CV **3.81%**，**未通过**。其中一个新版本窗口的 P99 为 **16.074 ms**。两组新建上游连接占比分别约 32.2% / 24.0%，说明有限池容量下的连接淘汰仍影响测量；尚未完全解释波动和尾延迟的原因。

[交替测试原始记录](validation/performance-round7-default-http-2026-09-27.json)保留如下观察值，不作为稳定容量结论：

| 场景 | 请求/秒，旧 → 新 | CPU µs/请求，旧 → 新 | P99 ms，旧 → 新 |
| --- | ---: | ---: | ---: |
| 64 端点轮询 | 81,785 → 83,870 | 24.01 → 23.46 | 1.965 → 1.943 |
| 64 端点，HTTP 主动健康检查 | 66,063 → 81,530 | 29.58 → 23.91 | 3.886 → 1.900 |

普通轮询观察到 +2.5% 吞吐，健康检查场景观察到 +23.4%；后者 CPU/请求下降 19.2%。普通轮询新版本仍有一个 2.591 ms 的 P99，旧版本最高为 2.138 ms。此次复测没有重复中间候选的吞吐下降，但不能把它视为排除了所有回退。

这组 A/A 与 A/B 共 18 窗、11,474,931 个请求，零错误。全部最终窗口的 SYN 重传、监听丢包与监听溢出均为零；默认池场景的 TIME_WAIT 溢出计数仍增长，原共享网络命名空间的历史 TCP 停顿也没有在本轮得到根因修复。

## 分配与 CPU 证据

[分配诊断](validation/performance-round7-allocation-2026-09-27.json)使用 Heaptrack 原始分配事件：四条持久连接发送 2,000 个完整请求，减去另一个同配置零请求进程的启动开销；两端均为单 worker、显式池预算 1,024。它不用于衡量吞吐或保留内存。

| 场景 | 旧版事件/请求 | 新版事件/请求 | 变化 |
| --- | ---: | ---: | ---: |
| 单端点 | 110.511 | 110.371 | −0.1% |
| 64 端点轮询 | 116.153 | 111.149 | −4.3% |
| 64 端点哈希 | 116.689 | 111.710 | −4.3% |

多端点每请求约减少 **5 次分配事件**。这与选路表复用一致，不能单独推导吞吐提升。

[仅选路缓存时的 CPU 采样](validation/performance-round7-selection-profile-2026-09-27.json)发现：256 个 HTTP 健康检查端点、单 worker、显式池预算 1,024 时，`reqwest::ClientBuilder::build` 占进程 CPU 样本 **29.92%**，其中 `native_tls::TlsConnectorBuilder::build` 占 **29.83%**。普通 HTTP 探测也在反复初始化 TLS、读取和解析信任证书。

[最终版本采样](validation/performance-round7-profile-2026-09-27.json)中，该调用不再出现在占比至少 1% 的调用栈报告里。两次都使用 `cpu-clock`、199 Hz、11 秒、DWARF 栈，单独运行；样本百分比不是绝对 CPU 节省比例，也没有计入正式吞吐数据。HTTPS 探测仍保留原来的客户端构建路径，本轮收益只验证了 HTTP 探测。

## 中间候选与失败实验

仅加入选路表的候选 SHA 为 `3aa28396a746bfc57d0d0541b37c5d4c5f0984284067df472e1d4de53e12ba90`。这批结果保留用于说明优化依据，未混入最终版本统计：

- [默认池双 worker A/A](validation/performance-round7-selection-aa-2026-09-27.json)最大差异 32.42%，CV 21.31%，失败。256 端点轮询约 96.6% 的请求重新建立数据连接。
- [默认池双 worker A/B](validation/performance-round7-selection-http-2026-09-27.json)观察到普通单端点 −3.3%、16 端点轮询 −5.6%、64 端点轮询 −13.0%，64 端点健康检查 +11.8%；有多处 P99 回升，不能认定缓存本身已带来普遍提速。
- [显式池预算 1,024 的单 worker A/A](validation/performance-round7-selection-pooled-aa-2026-09-27.json)最大差异 20.80%，失败。[同条件 A/B](validation/performance-round7-selection-pooled-http-2026-09-27.json)中 256 端点健康检查观察到 +6.5% 吞吐，随后 CPU 采样定位到了更大的客户端构建开销。
- [中间候选回归记录](validation/performance-round7-selection-regression-2026-09-27.json)也保留了当时的源码摘要与 227 项检查，不能替代最终产物记录。

## 正确性与边界

[最终回归记录](validation/performance-round7-regression-2026-09-27.json)：**227 项通过**，包含 Rust 21、HTTP 集成 92、产品功能 114；另有一个原有路由微基准保持 ignored。release all-targets Clippy 拒绝 warning，通过。

新增或扩展的有意义边界包括：500 次权重/健康变化下的轮询顺序对照、四种策略的冷却到期自动恢复、全部端点撤销后的 503 与重新恢复分流、探测目标端口和虚拟 Host。原有 6,150 组哈希/粘性落点对照、并发额度与最少连接分散选择、DNS 更新与旧 Lease 隔离继续通过。

HTTP 与产品套件实际运行了 HTTP/TLS/HTTP2/WebSocket/SSE/gRPC、私有 CA、SNI、mTLS 健康检查、认证、请求体、上游故障、配置更新和排空等既有行为。250 个源码、清单与工具文件在 macOS 工作区和 Linux 构建副本间摘要一致。

未重跑完整 Kubernetes/Gateway/Helm 部署、专门 OTLP/XDP 套件、amd64、多节点、物理主机容量、长期内存与持续健康抖动测试。短窗口与局部 A/A 通过不替代这些验收，也没有重新比较 NGINX/OpenResty 前端。

## 环境与复现

- OrbStack Ubuntu / Linux 7.0.14 / aarch64，Rust 1.90.0；release、thin LTO、单 codegen unit，依赖锁未改动。
- 所有压测在临时 Linux 网络命名空间内，只启用 loopback；不修改共享网络或系统 TCP 参数。仍共享 VM 与宿主机 CPU/内存，VM CPU 绑定不等于独占物理核心。
- HTTP/1.1 keepalive、64 并发、完整 1 KiB 响应；端点权重 1–5 循环。地址不同的端点共用 NGINX 1.30.5 上游进程，不是独立 Pod。每个正式窗口前另校验 64 个请求的状态、完整响应与期望后端。
- 每窗启动新代理、预热 3 秒、测量 8 秒；三轮交替新旧顺序。代理单 worker 使用 VM CPU 10，双 worker 使用 10,11；wrk 两线程使用 2,3；上游两 worker 使用 6,7。
- 访问日志与 OTLP 导出关闭，代理正常流程与指标开启；主动健康检查间隔 10 秒。编译、回归、Heaptrack 与 perf 不和正式压测重叠。

[压测脚本](validation/performance-round7-fixture-2026-09-27.py)复用 `scripts/benchmark_compare.py`，需要现有 wrk、NGINX 和上述 CPU 编号；输出目录必须是新的。在原生 Linux 源码目录运行：

```bash
sudo unshare -n bash -c '
  ip link set lo up
  python3 docs/validation/performance-round7-fixture-2026-09-27.py \
    --source "$PWD" --baseline /path/to/round6/rgnix \
    --candidate /path/to/round7/rgnix --nginx /path/to/nginx \
    --work-dir /tmp/rgnix-round7-ab --workers 1 \
    --baseline-pool-size 1024 --candidate-pool-size 1024 \
    --cases rr-1 health-256
'
```

A/A 换新输出目录并增加 `--aa`。默认池测试省略两个 pool-size 参数，使用 `--workers 2 --cases rr-64 health-64`；对应 A/A 使用 `--cases rr-64`。检查 JSON 中的 `aa_gate.passed`，脚本结束并不表示校准通过。

[分配脚本](validation/performance-round7-allocation-fixture-2026-09-27.py)使用 `--fixture <results.json> --candidate <最终二进制>`。它需要 `rr-1 rr-64 hash-64` 三个已生成的配置；本次复用中间候选的配置目录，并明确替换为最终二进制。需要已有 Heaptrack preload 库，输出单独的 `allocations.json`，不修改吞吐结果。

| 产物 | SHA-256 |
| --- | --- |
| 第六轮基线 | `b0ab0508c0a79231ee74a93d8374562ec802fc99a40d39bbb15df828018ab629` |
| 第七轮最终版本 | `a5ae6cb44ba537b18e2c4995438aafd51827cdbbdc0f2af87ac06bb5c2ee36e3` |
| Cargo.lock | `a0e1e51c67cb7828e8608875effd2078e2c37881d84f63e76b0ec25574bd0d88` |
| NGINX 上游 | `d7509a23828ead5af789b69f2042a2eae74d25f740ef3c071dbc35c1b0f41304` |

最终二进制的核对副本保存在工作区 `.local/optimization7-20260927/rgnix-round7-linux-arm64`，原生记录在 `/tmp/rgnix-opt7-20260927/`。压测进程均已退出，原有系统 NGINX 保持运行。本轮没有创建提交、GitHub Release 或发布镜像。

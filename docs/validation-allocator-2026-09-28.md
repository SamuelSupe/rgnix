# 分配器与 HTTP/1 缓冲区优化验证

2026-09-28，分支 `experiment/replace-pingora`，基于 `1bd974f`。本轮完成 Linux GNU 可选 jemalloc、实验性 Hyper 读缓冲复用、固定头名预编译和禁用重试时的客户端简化路径。默认数据面仍是 Pingora，默认分配器仍是系统分配器。

**最终组合相对仅启用 jemalloc 的版本，在两 worker、64 并发、1 KiB 响应中观测到吞吐 +3.87%、CPU/请求 −3.21%；双方 A/A 通过。** 这只是小幅、有限场景的证据。16 KiB 最终版本 A/A 未通过，观测吞吐 −1.65%、CPU/请求 +2.42%；不据此认定收益或稳定退化。NGINX 的两个最终对照场景也未通过校准，不能宣布已接近其稳定性能。

## 保留的实现

- `--features jemalloc`：通过锁定的 `tikv-jemallocator 0.6.1` / jemalloc 5.3 为 Linux GNU 二进制设置 Rust 全局分配器，静态链接，不需要 `LD_PRELOAD`。本地 C 库仍使用各自的分配接口，其他目标继续使用系统分配器。集成方式参见 [GlobalAlloc 文档](https://docs.rs/tikv-jemallocator/0.6.1/tikv_jemallocator/)。Dockerfile 新增可选 `CARGO_FEATURES` 构建参数。
- Hyper `rgnix-reuse-buffer`：读缓冲为空、处于初始 8 KiB 自适应档位时，允许先用至少 4 KiB 的剩余空间，避免每个小消息都从仍被头部/body 引用的分配中分离出新缓冲。部分消息、增大的自适应档位和固定大小模式保留原来的预留策略。该 feature 随产品的 `hyper-experimental` 启用。
- Hyper 产品路径使用预构造的标准头名清除 hop-by-hop 头；动态 `Connection` 指定的头仍按原有语义处理。禁用重试时直接等待一次 `try_send_request`，省去重试 future 保存的 URI 和额外 pool-key 克隆；允许重试的调用方保留原路径。

本轮没有改变 worker 调度模型，也没有消除客户端请求交接 channel。缓冲复用可能使小消息之后的较大消息先经历一次较小读取，因此不能把小响应收益外推到所有流式负载。

## 两个独立阶段

阶段 A 对照原 system allocator 版本与仅启用 jemalloc 的版本，依次测量 2、1、4 worker。阶段 B 以同一个 jemalloc 二进制为基线，对照加入其余三项改动的最终二进制，只测量 2 worker。每阶段均加入 NGINX 1.30.5；OpenResty 1.31.1.1 仅作为共同源站。

两阶段共 **144 个正式窗口、137,750,202 请求、零 wrk 错误**。原始窗口全部保留，包括校准失败和尾延迟较高的窗口；不包含预热、直连源站、分配探针和持续负载。不能把两阶段或历史报告的提升百分比相加，也不能用不同阶段的绝对 RPS 计算优化收益。

### 阶段 A：jemalloc 保留为可选项

下面是观测中位数；六个场景中，没有任何一个同时满足 system 和 jemalloc 的 A/A 门槛。

| Worker / 响应 | System req/s | jemalloc req/s | 观测变化 | A/A：system / jemalloc | 最大 PSS：system / jemalloc |
|---|---:|---:|---:|---|---:|
| 1 / 1 KiB | 76,767 | 80,065 | +4.30% | FAIL / PASS | 22.73 / 26.94 MiB |
| 1 / 16 KiB | 66,575 | 66,478 | −0.15% | FAIL / FAIL | 24.67 / 29.22 MiB |
| 2 / 1 KiB | 102,320 | 113,507 | +10.93% | FAIL / FAIL | 24.79 / 29.73 MiB |
| 2 / 16 KiB | 96,733 | 109,291 | +12.98% | PASS / FAIL | 27.72 / 32.11 MiB |
| 4 / 1 KiB | 109,080 | 102,150 | −6.35% | FAIL / FAIL | 26.74 / 33.16 MiB |
| 4 / 16 KiB | 97,158 | 110,610 | +13.85% | FAIL / FAIL | 30.92 / 34.98 MiB |

jemalloc 的峰值 PSS 在这组观测中多约 4～6.4 MiB，四 worker 小响应也没有显示收益，故没有将其设为默认。此前使用 `LD_PRELOAD` 的单窗诊断不等同于本次静态 Rust 分配器集成，不能延用其中约 36% 的 CPU 降幅作为正式结论。

两 worker 的 system 16 KiB 窗口 P99 最高 33.812 ms；NGINX 同场景最高 30.790 ms。这些异常窗口保留在原始记录，波动原因仍未确诊。

### 阶段 B：最终组合与 NGINX

“基线”是阶段 A 的 jemalloc 二进制；“最终”在此基础上增加缓冲、头名及无重试路径改动。RPS、CPU 为六个窗口的中位数；P99、PSS 是窗口统计的最大值，不是合并所有请求后的分位数。

| 响应 / 引擎 | req/s | CPU µs/请求 | 最差窗口 P99 | 峰值 PSS | A/A |
|---|---:|---:|---:|---:|---|
| 1 KiB / 基线 | 122,214 | 15.96 | 1.308 ms | 29.29 MiB | PASS |
| 1 KiB / 最终 | **126,944** | **15.44** | 1.175 ms | 27.98 MiB | PASS |
| 1 KiB / NGINX | 216,363 | 9.16 | 14.525 ms | 18.06 MiB | FAIL |
| 16 KiB / 基线 | 111,396 | 17.59 | 1.525 ms | 32.41 MiB | PASS |
| 16 KiB / 最终 | 109,555 | 18.01 | 1.509 ms | 30.71 MiB | FAIL |
| 16 KiB / NGINX | 129,095 | 15.42 | 3.918 ms | 18.00 MiB | FAIL |

1 KiB 三轮各自取两个窗口中位数后，最终/基线吞吐比为 **1.098、1.039、1.043**；三轮方向一致。但 3.87% 的总体增幅仍小于部分组内波动，10% 校准门槛只是稳定性筛查，不是置信区间或独立重复实验。分配减少的证据比吞吐增幅更直接。

最终版/NGINX 的吞吐中位数比为 58.7% / 84.9%（1 / 16 KiB），**仅供本轮观测**。小响应的 CPU 成本仍明显较高，不能把这次优化描述为消除了与 NGINX 的差距。

| 场景 / 引擎 | 三对 RPS 对称差 | 两组 RPS CV | A/A |
|---|---|---|---|
| 1 KiB / 基线 | 2.20%、0.13%、5.07% | 1.91%、4.28% | PASS |
| 1 KiB / 最终 | 1.32%、3.86%、0.51% | 5.06%、6.93% | PASS |
| 1 KiB / NGINX | 23.72%、7.34%、6.77% | 11.05%、4.43% | FAIL |
| 16 KiB / 基线 | 5.27%、6.83%、6.39% | 6.25%、4.31% | PASS |
| 16 KiB / 最终 | 5.27%、7.60%、14.58% | 4.13%、12.97% | FAIL |
| 16 KiB / NGINX | 3.44%、11.70%、8.50% | 2.52%、6.23% | FAIL |

## 分配机制验证与持续负载

单连接预热 10 次后，分别对基线和最终二进制执行 500 次请求，每次验证状态和完整 1 KiB 响应。Linux AArch64 uprobes 记录静态分配器 `_rjem_malloc/mallocx/calloc/realloc/rallocx` 入口，通过 ELF 符号与返回地址归属调用者；过滤 jemalloc 内部调用以避免重复计数。保留后台分配，不计进程启动阶段；未报告丢失事件。

| 指标 | 基线 | 最终 |
|---|---:|---:|
| 分配/重分配 API 次数 / 请求 | 31.002 | 27.554 |
| 累计申请字节 / 请求 | 19,828.52 | 5,411.72 |
| 8 KiB 分配次数，500 请求 | 1,000 | 133 |
| `BytesMut::reserve_inner` 累计申请字节 | 8,192,000 | 1,089,536 |

累计申请字节下降 **72.71%**；它既不是常驻内存降幅，也不是吞吐增幅。客户端 `request` 调用归属的累计申请字节从 508,000 降到 436,000；本轮未单独测定固定头名修改的收益。

最终版另以同一进程交替运行六个 20 秒的 1 / 16 KiB 窗口：**14,238,746 请求，零错误**。PSS 预热后 27.62 MiB，采样峰值 30.87 MiB，最后空闲 15 秒后 17.52 MiB；没有看到在这 120 秒内持续累积的内存。进程未设置 `LD_PRELOAD`，探针实际捕获了静态 jemalloc 调用。这不替代长期泄漏或生产容量验收。

## 执行验证

- 分配器阶段：Hyper 45、同一二进制的 Pingora 42、完整独立模式 94 项集成检查通过。
- 最终组合：Hyper 45 项集成检查通过，覆盖完整转发、流式响应、超时、POST 不重放、取消后预算释放、热更新旧请求完成及优雅退出。
- Hyper 单元测试 118 通过、6 忽略；hyper-util 61 通过、2 忽略。新增测试在大小消息混合读取后继续持有旧 body 帧，检查后续复用与连接缓冲销毁没有破坏原数据。
- 默认 feature 的 Linux `cargo check --locked`，最终 feature 组合的 Clippy `-D warnings`，格式及 diff 检查均通过。共 179 项依赖单元测试通过、8 项原有忽略；226 次集成检查执行包含不同阶段的重复场景，不是 226 个独立案例。

首次 Hyper 测试命令只启用 client/server/http1，因现有测试辅助 `Compat` 依赖 http2 feature 而编译失败；随后使用 `full,rgnix-full-body,rgnix-reuse-buffer` 完整通过。初次失败及修正后的命令/日志均保留，没有为通过测试改变产品 feature。

## 条件、复现及边界

OrbStack Ubuntu ARM64，Rust 1.90.0、release、thin LTO、单 codegen unit。NGINX 1.30.5 / GCC 15.2.0 / `-O2`。共享宿主机上隔离 loopback network namespace；客户端 CPU 2～5、源站 6～9、前端从 CPU 10 起分配 1/2/4 个虚拟 CPU，affinity 不保证独占物理核心。正式吞吐窗口没有并行编译或 profiler。

64 条持久连接，客户端四线程，源站四 worker；双方 Host `localhost`、上游连接/读/写超时 5 秒、keepalive 60 秒、空闲池每 worker 128。NGINX 关闭代理缓冲和重试，rgnix 流式且禁止重放。访问日志和 OTLP 关闭，rgnix 保留真实指标、路由快照与请求/后端预算。

每窗前端重新启动，预热 3 秒、测量 8 秒，三轮交错且顺序轮换。每轮同一程序执行两窗；这两窗构成 A/A 配对，同一批全部窗口也用于 A/B 中位数，**没有另外独立的 A/A 样本**。预先固定每对 RPS 对称差及两组样本 CV 均不超过 10%，且零错误；不因失败重试或删窗。每进程启动检查状态和完整 body，wrk 正式窗口统计状态/连接/读/写/超时错误，没有逐字节验证全部正式响应。

阶段 B 源站直连为约 568,697 / 497,850 req/s，高于前端测试负载；这不能排除共享宿主机调度影响。延迟在各自达到的吞吐下测量，尚未做相同固定请求速率对照。

```sh
cargo build --locked --release --features hyper-experimental,jemalloc -j 2
./target/release/rgnix serve -c nginx.conf --experimental-hyper

# 在独立 network namespace 内运行；替换两个保留的二进制路径。
python3 scripts/benchmark_compare.py \
  --rgnix /path/to/allocator-only --candidate /path/to/final \
  --nginx /path/to/nginx --openresty /path/to/openresty \
  --engines rgnix rgnix candidate candidate nginx nginx \
  --rgnix-transport hyper --candidate-transport hyper --plain-proxy \
  --cases proxy-1k proxy-16k --workers 2 --concurrency 64 \
  --rounds 3 --seconds 8 --warmup 3 --interleave \
  --client-threads 4 --client-cpus 2,3,4,5 \
  --origin-workers 4 --origin-cpus 6,7,8,9 --server-cpus 10,11 \
  --work-dir /var/tmp/rgnix-comparison-new --output /var/tmp/rgnix-comparison-new.json
```

阶段 A 将基线改为 system 二进制、候选改为 allocator-only，依次使用 2/1/4 worker 和对应数量的前端 CPU。始终使用新的 work-dir。构建、驱动、源码摘要、测试日志、所有 A/A 计算及校准失败均保存在[汇总 JSON](validation/allocator-2026-09-28.json)。驱动源文本是当次执行的档案，含测试机路径；复现需要调整工具、二进制和 fixture 路径。

| 二进制 | SHA-256 |
|---|---|
| system 基线 | `2e61dfd9255e18ba26777138787f2d4cdb8cc65ecebfebead6b59cabdc1ebbc9` |
| allocator-only | `a69905c0d843a1a349085b4222f89cd79213e126ff8d08a092e958845f3ce4b6` |
| 最终组合 | `f16b0d2f35af9143448c6f27b629bd2838b03b77323aa9411eaf26dd5672b976` |
| NGINX | `d7509a23828ead5af789b69f2042a2eae74d25f740ef3c071dbc35c1b0f41304` |

最终源码摘要与被测二进制记录已核对。allocator-only 的 `Cargo.toml` 摘要是在最后源码中排除随后新增的 buffer feature 后重建的，汇总中明确标注；其他生产文件基于 `1bd974f`。system 基线复用此前保留且已有源码核对记录的二进制。

本轮没有验证 Docker 镜像构建、Linux amd64、真实网卡或长期持续负载；没有做 TLS、HTTP/2、RGL、Ingress 或默认 Pingora 的性能资格验证，也没有测最终组合的 1/4 worker 性能。实验性 Hyper 原有功能限制继续适用。

原始数据：[1 worker](validation/allocator-1w-2026-09-28.json)、[2 worker](validation/allocator-2w-2026-09-28.json)、[4 worker](validation/allocator-4w-2026-09-28.json)、[最终组合](validation/buffer-comparison-2026-09-28.json)、[分配基线](validation/allocator-probe-before-2026-09-28.json)、[分配候选](validation/allocator-probe-after-2026-09-28.json)、[持续负载](validation/allocator-soak-2026-09-28.json)。测试机临时文件位于 `/var/tmp/rgnix-allocator-20260928`，不视为永久发布制品。

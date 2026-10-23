# Hyper 内核性能与 NGINX 对照

2026-09-30，分支 `experiment/replace-pingora`，基于 `1bd974f` 的未提交实验改动。默认构建和运行仍为 Pingora。本轮基线是已经补齐 TLS、HTTP/2、RGL、认证及 Ingress/Gateway 能力的 Hyper 产品二进制，不能与此前最小代理或旧 HTTP/1 子集的数字直接相减。

**普通 1 KiB 代理的分配/重分配 API 次数减少 16.16%，累计申请字节减少 42.99%；吞吐对照未通过完整校准，尚未证明达到 NGINX 相近的性能水平。**

本轮进一步减少普通请求的固定开销，并在 OrbStack Ubuntu ARM64 对照 **NGINX 1.28.0（Ubuntu）**。历史测试所用的临时 NGINX/OpenResty 安装已不存在；这里使用已安装的 NGINX，同时作为共同源站，**没有测试 OpenResty 前端**。

## 保留的改动

- 普通 `proxy_pass` 不带 URI 时直接保留 Hyper 已解析的请求 URI，避免复制、重新解析和再次复制路径。带 URI、RGL 改写及 Gateway 路径仍使用原有语义。
- 清除 `Connection` 动态指定的头时，跳过已经统一清除的标准 hop-by-hop 头，避免为常见 `keep-alive` 再分配列表；其他动态头仍清除。
- 102/103 提示响应使用按需分配的有界队列，HTTP/1 在连接内复用队列和无人持有的请求令牌，空队列轮询不加锁。旧句柄保留独立令牌，不能向下一请求发送提示；最终响应前先转发已排队提示，再关闭该请求。HTTP/2 仍按流隔离。最多四条、每条 64 KiB 的原有约束不变。
- 无 Range/条件请求的小文件 GET（最多 64 KiB）在安全打开文件的同一个阻塞任务中完成读取，直接返回完整 body。HEAD、Range、条件请求和大文件保持原有选择及流式路径；未加入静态文件缓存。

第一次队列候选在空轮询路径上使用互斥锁，直接响应出现较差的观测值，未作为最终实现保留。其 90 个完整窗口仍保存在 [中间候选数据](validation/hyper-perf-core-2026-09-30.json)，没有删掉不利窗口。最终实现与中间候选使用不同的不可变二进制及摘要。

## 分配机制验证

Linux AArch64 对静态 jemalloc 的 `_rjem_malloc/mallocx/calloc/realloc/rallocx` 做入口探针。单持久连接预热十次后发送 500 次 1 KiB 代理请求，每次检查状态和完整 body；过滤分配器内部嵌套调用，保留后台分配。探针与正式吞吐窗口分开执行，没有丢失事件。

| 操作计数 | 基线 | 最终 | 变化 |
|---|---:|---:|---:|
| 分配/重分配 API 次数 / 请求 | 36.526 | 30.622 | −16.16% |
| 累计申请字节 / 请求 | 9,904.94 | 5,646.54 | −42.99% |
| 提示响应通道归属申请字节，500 请求 | 2,064,000 | 0 | 普通持久连接复用 |
| 清除 hop-by-hop 头归属申请字节，500 请求 | 64,000 | 0 | 常见固定头不再构造列表 |

这里的零表示预热后的本 fixture 没捕获到该调用者的申请，不代表创建连接、发送真实提示或保留旧句柄不分配。累计申请字节不是常驻内存，也不是吞吐提升百分比。两个探针包含少量后台分配及缓冲区大小差异，不能把每个差额都归给单独改动。

## 最终对照

最终阶段 **90 窗、129,027,527 请求、零 wrk 错误**；加上基线和中间候选，本轮共 204 个正式窗口、275,804,448 请求、零错误。预热、源站直连、探针及持续负载不计入这些数字。

**没有任何场景同时通过基线/最终或最终/NGINX 的完整 A/A 校准。** 最终版 TLS 组自身通过，但对照组未通过。下面仅是观测中位数：不能认定普通代理的稳定收益、稳定退化或 NGINX 对等。

| 场景 | 基线 req/s | 最终 req/s | NGINX req/s | 最终相对基线 | 最终 / NGINX |
|---|---:|---:|---:|---:|---:|
| 直接响应（2 B） | 319,993 | 323,657 | 687,870 | +1.14% | 47.1% |
| 静态文件（16 KiB） | 80,010 | 122,000 | 308,160 | +52.48% | 39.6% |
| HTTP 代理（1 KiB） | 104,314 | 101,152 | 192,622 | -3.03% | 52.5% |
| HTTP 代理（16 KiB） | 79,410 | 85,064 | 117,047 | +7.12% | 72.7% |
| HTTPS 代理（1 KiB，持久连接） | 92,334 | 94,252 | 158,489 | +2.08% | 59.5% |

静态文件改善方向明显，但峰值内存更高；1 KiB 代理没有显示吞吐改善，少分配未在这次窗口中转化为更快的普通代理。其最大同程序配对差为 14.38%，超过观测到的 −3.03% 变化。保留改动依据是可验证的分配/复制减少和行为验收，并未获得稳定吞吐资格。

下表为最终 / NGINX 的 CPU 中位数及窗口最大值；P99 不是合并全部请求后的分位数，且在各自达到的不同吞吐下测量。

| 场景 | CPU µs/请求：最终 / NGINX | 最大窗口 P99 ms：最终 / NGINX | 峰值 PSS MiB：最终 / NGINX |
|---|---:|---:|---:|
| 直接响应（2 B） | 6.07 / 2.88 | 0.825 / 9.099 | 32.18 / 21.69 |
| 静态文件（16 KiB） | 15.78 / 6.44 | 4.572 / 2.144 | 106.64 / 21.68 |
| HTTP 代理（1 KiB） | 19.54 / 10.32 | 1.827 / 1.376 | 35.61 / 22.33 |
| HTTP 代理（16 KiB） | 23.07 / 17.07 | 2.278 / 2.047 | 38.11 / 22.24 |
| HTTPS 代理（1 KiB，持久连接） | 21.06 / 12.54 | 1.692 / 1.431 | 39.65 / 26.47 |

最终静态窗口 PSS 最高 106.64 MiB，基线最高 74.76 MiB；两者都是 17 个线程。不能把累计申请字节减少描述为常驻内存减少，也不能据线程数量排除其他内存问题。独立静态持续负载的补查结果见后文。

| 场景 / 程序 | 最大配对对称差 | 最大组 CV | A/A |
|---|---:|---:|---|
| 直接响应（2 B） / 基线 | 7.70% | 22.27% | FAIL |
| 直接响应（2 B） / 最终 | 2.59% | 23.48% | FAIL |
| 直接响应（2 B） / NGINX | 28.46% | 38.42% | FAIL |
| 静态文件（16 KiB） / 基线 | 19.89% | 16.60% | FAIL |
| 静态文件（16 KiB） / 最终 | 27.25% | 17.68% | FAIL |
| 静态文件（16 KiB） / NGINX | 17.91% | 33.52% | FAIL |
| HTTP 代理（1 KiB） / 基线 | 16.56% | 13.80% | FAIL |
| HTTP 代理（1 KiB） / 最终 | 14.38% | 9.28% | FAIL |
| HTTP 代理（1 KiB） / NGINX | 7.81% | 14.49% | FAIL |
| HTTP 代理（16 KiB） / 基线 | 22.93% | 23.09% | FAIL |
| HTTP 代理（16 KiB） / 最终 | 11.94% | 19.10% | FAIL |
| HTTP 代理（16 KiB） / NGINX | 13.03% | 17.89% | FAIL |
| HTTPS 代理（1 KiB，持久连接） / 基线 | 8.66% | 18.89% | FAIL |
| HTTPS 代理（1 KiB，持久连接） / 最终 | 6.33% | 5.51% | PASS |
| HTTPS 代理（1 KiB，持久连接） / NGINX | 24.36% | 16.01% | FAIL |

## 行为、内存与 CPU 观察

- 最终二进制：HTTP 集成 94、产品能力 115、双 worker 生命周期 52 项全部通过，共 **261 项运行检查**。覆盖 URI/头部语义、完整 body、静态文件及变化、TLS/SNI/HTTPS 上游、HTTP/2/h2c/gRPC/trailers、WebSocket、RGL/认证/租户策略、取消、超时、热更新和优雅退出。
- Hyper 单元测试 **119 通过、6 原有忽略**。本轮增加有界提示队列、最后 sender 关闭唤醒、实际提示唤醒、队列重用及旧句柄隔离的行为回归；不以地址或分配次数作为功能断言。
- Linux release 构建、全 target Clippy `-D warnings`、Python 语法及 diff 检查通过。生产代码没有切换 worker 架构，也没有放宽超时、连接验证或重试限制。
- 静态文件补查：每个二进制同一进程连续三个 20 秒窗口、无预热，基线 5,527,530 请求、最终 7,498,456 请求，零错误；峰值 PSS 74.17 / 90.64 MiB。最终各窗结束为 78.04、37.43、58.65 MiB，空闲 15 秒后基线 / 最终为 25.38 / 24.43 MiB。确认存在较高短期峰值和停止负载后的回落，具体分配器/工作集归因未证明；不排除长期或更高并发的内存问题。该补查不作为额外校准吞吐结果。
- 同一最终进程交替执行六个 20 秒静态文件 / 1 KiB / 16 KiB 窗口，**12,660,056 请求、零错误**。预热后 PSS 34.27 MiB，采样峰值 40.67 MiB，空闲 15 秒后 32.94 MiB。仅是 120 秒观察，不能证明长期无泄漏。
- 基线和 NGINX 各进行独立 20 秒 `cpu-clock` / 499 Hz / DWARF 剖析，约 19,592 / 19,761 样本、零丢失。基线平坦样本中主程序 41.25%、libc 7.13%、vDSO 5.40%；NGINX 主程序 18.81%、libc 9.99%、vDSO 0.07%。程序间调用栈比例不能直接当作吞吐比，最终版没有再采样。
- Tokio 调度器的 45.7% 是包含请求处理的整条栈占比，不能解释为 45.7% 的调度浪费。请求处理、头部解析和计时仍是值得进一步隔离的用户态成本。

## 与 NGINX 仍有的差距

这次分配和 URI 路径改动没有替换 Hyper 的 HTTP/1 客户端请求交接、响应回调、共享调度器和通用头部容器。继续减少这些固定成本需要更大范围的协议内核改动及可重复的对照，不能只依据删除分配就宣称对等。

NGINX 1.28.0 的 [Linux 输出链](https://github.com/nginx/nginx/blob/release-1.28.0/src/os/unix/ngx_linux_sendfile_chain.c)直接组合 `writev` / `sendfile` 并处理写就绪；Hyper 静态文件仍读到用户空间，再经 body 编码输出。本轮小文件预读只省掉一次异步文件交接，没有实现 Linux 零拷贝。零拷贝集成是静态文件的重要后续候选，必须保留 framing、Range、压缩、TLS、取消及实际完成计数。

NGINX 的 [缓存时钟](https://github.com/nginx/nginx/blob/release-1.28.0/src/core/ngx_times.c)和[请求/连接生命周期](https://github.com/nginx/nginx/blob/release-1.28.0/src/http/ngx_http_request.c)也与当前 Rust/Tokio 路径不同。Hyper 已缓存 Date 头的编码，但缓存检查及 I/O 超时更新仍读取当前时钟。低精度统一计时、worker 归属和批量请求存储可以分别做实验；当前没有实现或证明这些方案的收益。已有的固定 worker / 独立池实验未通过校准，不能作为本轮成果。

## 条件、复现及证据

OrbStack Ubuntu ARM64、Linux 7.0.14；Rust release / thin LTO / 单 codegen unit / `hyper-experimental,jemalloc`。NGINX Ubuntu 1.28.0，发行包 `-O2` / LTO，OpenSSL 3.5.3；二进制与完整构建信息保存在原始 JSON。

独立 loopback network namespace。前端 CPU 10～11 两 worker，客户端 CPU 2～5 四线程，源站 CPU 6～9 四 worker；64 条持久连接，上游连接/读/写超时 5 秒，keepalive 60 秒，每 worker 空闲池 128。NGINX 关闭代理缓冲及重试；rgnix 保留真实指标、请求/后端预算和快照，关闭访问日志及 OTLP。宿主机其他工作负载未停止，虚拟 CPU affinity 不保证物理核心独占。

最终比较每轮每场景运行基线、最终、NGINX 各两窗，顺序轮换；三轮、预热 3 秒、测量 8 秒，共 90 窗。A/A 使用同一批两次相同二进制的窗口：每对 RPS 对称差及两列样本 CV 均须不超过 10%，至少三对、零错误。没有为通过门槛而删窗或重试；该门槛不是置信区间。每次启动检查状态和完整 body，wrk 正式窗口统计错误，未逐字节验证所有正式响应。TLS 场景测试持久 HTTPS 的 1 KiB 代理，未测每请求新握手。

```sh
cargo build --locked --release --features hyper-experimental,jemalloc -j2
python3 scripts/benchmark_compare.py \
  --rgnix /path/to/rgnix-before --candidate /path/to/rgnix-final \
  --nginx /usr/sbin/nginx --openresty /usr/sbin/nginx \
  --rgnix-transport hyper --candidate-transport hyper \
  --engines rgnix rgnix candidate candidate nginx nginx \
  --cases return static proxy-1k proxy-16k tls --interleave \
  --workers 2 --origin-workers 4 --client-threads 4 \
  --server-cpus 10,11 --client-cpus 2,3,4,5 --origin-cpus 6,7,8,9 \
  --concurrency 64 --rounds 3 --seconds 8 --warmup 3 \
  --work-dir /var/tmp/rgnix-hyper-comparison --output /var/tmp/rgnix-hyper-comparison.json
```

在已启用 loopback 的独立 network namespace 中执行上述测试。`--openresty` 是历史参数名，本轮传入 NGINX 作为共同源站；选择 `body` 场景仍需要真正的 OpenResty。基线 SHA-256 为 `ed872bb8463280226ea12b013d9dcde0e838d4c4f0b099534c760c16225190ff`，最终为 `282f6a1d9e046eadab96d6476b8fd802a17501683b6e2102e56a8ca2a2ecf459`。

分析可用 `python3 docs/validation/analyze-hyper-performance-2026-09-30.py /path/to/benchmark.json` 重算。完整窗口、分析、源码摘要、检查命令、CPU 摘要和持续负载见[机器记录](validation/hyper-performance-2026-09-30.json)、[基线数据](validation/hyper-perf-baseline-2026-09-30.json)、[最终数据](validation/hyper-perf-final-2026-09-30.json)、[基线探针](validation/hyper-perf-probe-before-2026-09-30.json)及[最终探针](validation/hyper-perf-probe-final-2026-09-30.json)。原始 perf.data 与执行日志保存在本次 OrbStack `/var/tmp/rgnix-hyper-perf-20260930/`；该临时目录不是可移植交付物。

未重跑 amd64、HTTP/2 吞吐、TLS 新握手吞吐、256 并发、Kubernetes 控制器主流程、固定到达速率延迟或生产长期负载。此次没有改控制器代码，先前 Kubernetes 功能验收不能代替本轮内核的上述未测项目。尚未提交或发布到 GitHub，未更改默认数据面。

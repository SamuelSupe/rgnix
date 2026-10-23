# 当前实验性 Hyper 数据面与 NGINX 对比

2026-09-28，测试 rgnix `6c54c95` 的实验性 Hyper HTTP/1 数据面与 NGINX 1.30.5。产品代码没有修改；本轮修正了基准脚本的配置差异，并重新采集数据。默认 Pingora 未参与本次对比。

## 结果：小响应差距仍明显

六对交错窗口中，NGINX 的吞吐均高于 rgnix。以下为每场景三轮的中位数与完整范围，**仅代表本轮观测**；NGINX 的两个场景 A/A 均未通过，不能认证稳定的性能比例。

| 普通代理 | rgnix req/s，中位数〔范围〕 | NGINX req/s，中位数〔范围〕 | rgnix / NGINX 中位数 |
|---|---:|---:|---:|
| 1 KiB | **71,488**〔66,478～80,054〕 | **156,269**〔154,402～158,690〕 | **45.7%** |
| 16 KiB | **63,929**〔55,632～70,055〕 | **79,018**〔78,941～84,146〕 | **80.9%** |

逐轮配对比值分别为 **50.4%、42.5%、46.3%**（1 KiB），以及 **88.7%、81.0%、66.1%**（16 KiB）。1 KiB 观测中位数对应 NGINX 约 2.19 倍吞吐；16 KiB 约 1.24 倍。不能把这些数与先前不同配置、时间窗口的表格相减来计算本次优化收益。

| 场景 / 引擎 | CPU µs/请求，中位数 | P99 中位数 / 最差窗口 | 峰值 PSS，所有对比窗口最大值 |
|---|---:|---:|---:|
| 1 KiB / rgnix | 27.10 | 1.976 / 2.278 ms | 21.67 MiB |
| 1 KiB / NGINX | 12.55 | 2.863 / 6.570 ms | 18.82 MiB |
| 16 KiB / rgnix | 30.35 | 2.594 / 2.760 ms | 24.52 MiB |
| 16 KiB / NGINX | 24.52 | 3.257 / 18.785 ms | 18.82 MiB |

对比阶段，rgnix 的 RPS CV 为 **9.45% / 11.45%**（1 KiB / 16 KiB），NGINX 为 **1.37% / 3.70%**。rgnix 的 16 KiB 对比波动亦超过 10%，即使此前自身 A/A 通过，也不能忽略后续波动。NGINX 较高的尾延迟窗口同样全部保留，不能只引用其较高吞吐。

正式校准加对比共 **36 窗、52,913,655 请求、零 wrk 错误**；其中实际两程序交错对比为 **12 窗、16,839,591 请求**。后者的直连源站基线为 **399,809 / 313,187 req/s**，高于对应前端流量；前端 CPU 中位数约为 **1.94～1.96 核**，客户端和源站各低于其四核预算。这里没有观察到源站或客户端先达到 CPU 上限，但不据此排除宿主机调度影响。

可以确认这次小响应吞吐仍有明显差距，且每请求 CPU 成本更高。无法从这次吞吐测试确定剩余开销具体落在哪个模块，也不能宣称上一轮减少分配已经带来可归因的吞吐提升。

## 测试条件

- OrbStack Ubuntu ARM64，Apple M5 Max 宿主机；独立 loopback network namespace。其他用户工作负载没有停止，虚拟 CPU affinity 不等于独占物理核心。
- 前端均为 **2 worker、64 条持久连接**，CPU 10、11；wrk 四线程使用 CPU 2～5；相同 OpenResty 源站四 worker 使用 CPU 6～9。OpenResty 在这里仅作为源站。
- 普通 HTTP/1 GET，响应分别为 **1 KiB / 16 KiB**。1 KiB 可命中新小响应路径，16 KiB 超出 8 KiB 优化阈值；本次吞吐测试未统计具体命中率。
- 双方使用 Host `localhost`、上游 connect/read/write 超时 5 秒、keepalive 60 秒，空闲连接预算为每 worker 128（Hyper 此配置下共享总量 256）。本次 64 并发低于两者池上限。
- NGINX 明确 `proxy_buffering off`、`proxy_request_buffering off`、`proxy_next_upstream off`。rgnix 流式转发并禁用重试。双方关闭访问日志，rgnix 关闭 OTLP 导出但保留指标、路由快照、请求/后端预算。
- 每个进程启动后先校验两个路径的状态码与完整响应 body。正式压测由 wrk 统计状态、连接、读写、超时错误，没有逐字节检查每个正式请求。
- 每窗预热 **5 秒**、测量 **15 秒**，每场景 **3 轮交错**，下一轮反转引擎与场景顺序。前端每窗重启，源站在同阶段各窗之间保持运行。无构建或 profiler 与本轮正式测试并行。

rgnix 使用 Rust 1.90.0、locked release、thin LTO、单 codegen unit；NGINX 使用 GCC 15.2.0、`-O2`、OpenSSL 3.5.3。这里比较实际产品路径，不能把差值全部归因于 HTTP parser 或语言。

## 基准配置修正

旧脚本为 NGINX 配置 5 秒上游超时，而 rgnix 使用默认 60 秒；NGINX 空闲池是每 worker 512，而 rgnix 默认每 worker 128；上游 Host 也未显式对齐。发现后停止初始采集，修改 `scripts/benchmark_compare.py`，显式设置相同超时、Host 和池预算，再完整重跑。

中止批次保留在 [原始记录](validation/nginx-current-aborted-fixture-2026-09-28.json)，不计入正式结果。这次重跑由配置正确性触发，不是看到校准失败后重试或挑选窗口。历史报告继续保留其当时配置与结论；本轮数据不能直接用于计算相对旧版的优化收益。

## 校准结果

运行前固定规则：rgnix 和 NGINX 各执行每场景 3 对同二进制 A/A；每对 RPS 对称差、两组样本 CV 均须不超过 **10%**，并要求零错误。之后无论校准是否通过，均执行一次预定的完整交错对比；校准失败的场景只能报告观测结果，不能认证相对吞吐或发布容量。

| 引擎 / 场景 | 三对 RPS 对称差 | 两组 CV | 校准 |
|---|---|---|---|
| rgnix / 1 KiB | 0.14%、2.33%、0.78% | 1.62%、1.99% | PASS |
| rgnix / 16 KiB | 3.11%、9.13%、2.25% | 3.54%、0.70% | PASS |
| NGINX / 1 KiB | 11.89%、2.18%、3.49% | 3.29%、7.21% | FAIL |
| NGINX / 16 KiB | 15.58%、1.70%、4.15% | 6.13%、4.30% | FAIL |

NGINX 的校准阶段最大 P99 为 **23.597 ms**，该窗口没有被剔除。两个场景均未获得双方校准通过的资格，波动原因尚未确诊。rgnix 本轮通过也不推翻上一轮不同条件下失败的校准记录。

## 范围与复现

这不是完整产品功能或稳定生产容量验收：未覆盖 TLS、HTTP/2、RGL、Ingress、默认 Pingora、大请求上传、真实网卡、amd64 或长时间浸泡。wrk 为固定并发的闭环负载；双方在各自达到的吞吐下测量延迟，没有做相同固定请求速率的延迟对照。

CPU 成本包括前端进程树；PSS 避免多进程共享页面重复计数。表中的 P99 是各窗口 P99 的中位数/最大值，不是合并所有请求直方图后的 P99。

```sh
python3 scripts/benchmark_compare.py \
  --rgnix /path/to/rgnix --nginx /path/to/nginx --openresty /path/to/openresty \
  --engines rgnix nginx --rgnix-transport hyper --plain-proxy \
  --cases proxy-1k proxy-16k --workers 2 --concurrency 64 \
  --rounds 3 --seconds 15 --warmup 5 --interleave \
  --client-threads 4 --client-cpus 2,3,4,5 \
  --origin-workers 4 --origin-cpus 6,7,8,9 --server-cpus 10,11 \
  --work-dir /tmp/rgnix-comparison-new --output /tmp/rgnix-comparison-new.json
```

相同设置下，rgnix A/A 增加 `--candidate /path/to/rgnix --candidate-transport hyper --engines rgnix candidate`；NGINX A/A 使用 `--engines nginx nginx`。每次使用新的 work-dir，避免覆盖窗口。

rgnix SHA-256：`2e61dfd9255e18ba26777138787f2d4cdb8cc65ecebfebead6b59cabdc1ebbc9`。保留二进制的生产源码摘要与当前 `6c54c95` 已核对；基准脚本单独记录摘要。

NGINX SHA-256：`d7509a23828ead5af789b69f2042a2eae74d25f740ef3c071dbc35c1b0f41304`。

证据：[汇总与驱动](validation/nginx-current-2026-09-28.json)、[rgnix A/A](validation/nginx-current-aa-rgnix-2026-09-28.json)、[NGINX A/A](validation/nginx-current-aa-nginx-2026-09-28.json)、[交错对比全部窗口](validation/nginx-current-comparison-2026-09-28.json)。原始 fixture 和进程日志保留在测试机 `/tmp/rgnix-nginx-compare-current-20260928-aligned`，不是永久制品。

# Hyper 请求状态、客户端与计时器优化

基于 `experiment/replace-pingora` 的 `9ff7cf5`，在 OrbStack Ubuntu ARM64、Rust 1.90.0 上完成四项改动。实验仍限于独立模式的明文 HTTP/1；未迁移 TLS/H2/RGL，也未改变默认运行引擎。

## 实现与代价

- **缩小请求状态**：共享 `Context` 从 2,080 字节降至 936 字节；镜像与 trace 状态改为按需分配。Hyper 先固定 guard 的存储位置，再填充请求数据。原先请求入口的大块复制随之减少。开启镜像、trace 时各增加一次对象分配，普通请求没有这两次分配。
- **减少客户端状态搬移**：`hyper-util` 在连接池取连接、发送 future 和可重试错误之间保留 `Box<Request<B>>`，在原有 Hyper dispatch 边界移出。增加一次请求盒分配，重试策略没有放宽。使用可选的独立路径依赖，只编译此客户端所需的 HTTP/1 能力；reqwest/kube 保留注册表版本。下述客户端结果同时包含请求装箱和 feature 隔离的影响，不能归因于单独一项。上游原始来源、许可证和补丁说明保留在 [vendor/hyper-util](../vendor/hyper-util/RGNIX.md)。
- **按需管理超时**：上游复用下游的惰性 `Deadline`；只有首次 Pending 或有效上传进展才创建/重置期限。读取成功只清除活动标记，不再用一次时间读取重置计时器。空 flush、空写不延长读期限；连接、读写超时、向量写和流式传输保留。
- **发布前准备配置**：固定头名称/值、上游 Host、已知端点 authority、客户端按快照预建；不带 URI 的 `proxy_pass` 直接复用原 `PathAndQuery`，动态变量仍在请求阶段展开。请求不再加锁查找客户端。DNS 刷新引入的新端点按实际选址解析，避免缓存旧地址。旧请求持有原快照；历史记录去掉空闲池，回退时重新准备。

精确与前缀 location 可以具有相同 route ID，因此准备结果以监听地址和快照内 route 对象身份索引，不能只按 ID 覆盖。准备失败发生在发布前，不替换有效版本。

## 运行检查

Clippy `--all-targets -D warnings`、locked release 构建通过。Rust 单元检查 24 项通过，1 项路由微基准按其既有声明忽略。新增两项虚拟时间检查保护真实上传进展延长读期限、读取成功后重新计时，以及空 flush/空写不能延长期限。

真实 socket 集成检查 **Hyper 40 项、Pingora 37 项通过**，原有 **94 项默认引擎验收通过**。新增检查保护同路径精确/前缀路由、固定头删除、准备结果随热更新生效、历史回退重新创建客户端、慢速上传期间延长读取期限；已有大请求、取消、流式响应、POST 不重放、预算释放、OTLP/W3C 和排空检查继续执行。最终数量和输出见下面的机器记录。

## 操作计数

相同 1 KiB 普通代理 fixture、单 worker、CPU 10。libc uprobes：1 条持久连接，10 次预热后 500 个校验响应的请求。Heaptrack：4 条持久连接，2,000 请求减去独立零请求启动控制。两类工具分开运行；数据是操作计数，不是吞吐/延迟或存活内存。

| 每请求指标 | 9ff7cf5 | 本轮 | 变化 |
|---|---:|---:|---:|
| 捕获的 libc 拷贝字节 | 26,485.5 | 15,043.6 | −43.20% |
| memcpy 调用 | 125.90 | 95.99 | −23.75% |
| clock_gettime 调用 | 29.69 | 24.38 | −17.89% |
| 分配次数 | 50.36 | 43.34 | −13.94% |
| 累计申请字节 | 24,459.5 | 21,382.1 | −12.58% |

请求入口捕获的复制从 8,677 降至 3,637 字节/请求；可识别 Hyper 客户端 request/send 栈合计从 10,456 降至 6,184 字节/请求。内联会改变调用点归属，这些归属不是独立 CPU 成本。libc 探针不包含编译器内联复制，并含少量后台活动。新旧各批事件数量与 perf 捕获样本数量核对一致；临时探针已清除。

优化前二进制 SHA-256：`d5f199b3bab4debc10510ef4a36533068d4a015ce03b5f6a5e80924bd5df4170`。

本轮二进制 SHA-256：`89bcd6456bd1a4c83ad6270ab7d9c944c7685ff1e7731b2bbf0334738996d2be`。

## 吞吐资格

无 profiler 的测试单独进行：64/256 连接，各 3 轮交替，每窗预热 2 秒、测量 10 秒。前端 1 worker/CPU 10，客户端 1 线程/CPU 2，上游 1 worker/CPU 6。每个并发档位要求新旧二进制分别通过同二进制 A/A：各配对对称差和两组 RPS CV 均不超过 10%，且零错误。通过后才运行 NGINX A/A 和旧版/新版/NGINX 对照；不挑选窗口或重试直到通过。

本轮四组资格均为 **FAIL**：

| 二进制 / 并发 | 最大配对差 | 两组 RPS CV |
|---|---:|---:|
| 旧版 / 64 | 22.36% | 18.91% / 10.46% |
| 旧版 / 256 | 18.75% | 21.86% / 9.26% |
| 新版 / 64 | 20.39% | 9.97% / 21.50% |
| 新版 / 256 | 20.99% | 12.36% / 18.56% |

24 个窗口共 **11,463,516 请求，零 wrk 错误**；旧版最大 P99 为 38.005 ms，新版为 60.489 ms，不能忽略尾延迟异常。没有继续 NGINX A/A 或新旧/NGINX A/B，也没有跨校准批次计算提速比例。本次波动来源未确诊，不能把任何一个窗口归因于本轮改动。

[旧版全部 A/A 窗口](validation/hyper-prepared-aa-before-2026-09-28.json) · [新版全部 A/A 窗口](validation/hyper-prepared-aa-after-2026-09-28.json) · [完整机器证据](validation/hyper-prepared-2026-09-28.json)。可以确认的是操作开销下降、行为检查通过；吞吐提升和接近 NGINX 的结论仍未成立。

## 复现与范围

```sh
cargo test --locked --features hyper-experimental --lib
cargo clippy --locked --features hyper-experimental --all-targets -- -D warnings
cargo build --locked --release --features hyper-experimental
python3 scripts/hyper_integration.py /path/to/rgnix
python3 scripts/hyper_integration.py /path/to/rgnix --pingora
python3 scripts/integration.py /path/to/rgnix
```

操作计数复用 `scripts/profile_http_primitives.py`，以已有 profile/fixture 和矩阵为输入，分别传入新旧 `--binary`；Heaptrack 驱动和每次执行命令保存在机器记录。吞吐使用 `scripts/benchmark_compare.py --plain-proxy --rgnix-transport hyper --candidate-transport hyper --cases proxy-1k --concurrency 64 256 --interleave`，先让两个标签指向同一个二进制。完整参数和全部窗口保留在 A/A JSON 中。

未做 amd64、真实网卡、长时间浸泡、多 worker 扩展、镜像/trace 开启时的容量对照或 Kubernetes/Gateway 重验。原有空闲池总量隔离和完整生产协议迁移限制仍然存在。本轮没有实现未经测量支持的每 worker 独立连接池、XDP 或 io_uring 传输替换。

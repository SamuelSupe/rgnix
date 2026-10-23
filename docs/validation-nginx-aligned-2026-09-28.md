# NGINX 实现对照后的 Hyper 优化

基于 `experiment/replace-pingora` 的 `bf0b414`，在 OrbStack Ubuntu ARM64、Rust 1.90.0 上完成实现和验证。保留按需捕获请求头、已完整小响应消除 body 通道两项改动；固定 worker/独立池仅保留试验记录，生产源码已撤回该改动。默认 Pingora 数据面没有变化。

## 保留的实现

### 请求上下文按实际需要捕获

配置发布时生成每条路由需要的原始请求头列表：指标使用的 content-type、访问日志启用时的 referer/user-agent，以及限流、并发限制、hash 使用的 header/cookie。请求仍把所有合法头转发给上游，只减少上下文中的额外 HeaderMap 克隆。重复值和非 UTF-8 值保持原有处理规则；变量仍看到修改前的原始请求头。

为避免引入第二套变量解析器，任何请求头、响应头或直接响应模板包含 `$` 时，保守地保留完整原始视图。可信代理没有改变客户端 IP 时，也不再第二次格式化同一个 IP。路由匹配和完成路径的日志、指标、trace、预算均保留。

这借鉴了 NGINX 让请求元数据依附输入缓冲区的做法，但不是全新的零复制 parser：Hyper 本来已通过共享 Bytes 引用大部分头值，本轮减少的是 rgnix 额外捕获的数据结构。

### 已完整小响应直接交付

新增隔离的 `vendor/hyper` 1.11.1 路径依赖，实验客户端也指向这一副本；reqwest/kube 继续使用注册表依赖。原始 manifest、上游 VCS 身份及 MIT 许可均保留，实际协议改动集中在三个 Rust 文件。

`rgnix-full-body` 仅优化客户端响应：Content-Length 为 1～8192 字节，且 body 已全部在解析响应头后的现有读缓冲区时，用原 decoder 完成 framing 和 keepalive 状态转换，再返回单个拥有 Bytes 的 Incoming。无需建立原 body channel。不会额外读取、等待凑齐 body 或预取下一响应。

未完整响应、大响应、chunked/EOF 定界、Expect、升级及声明 trailer 的响应继续原通道。服务端请求体路径不变，产品预算仍持有到下游 socket 写入完成或取消。请求 dispatch 和响应 oneshot 仍存在，因此不能称为 NGINX 式统一 HTTP/1 驱动。新增 Bytes 变体也增加了依赖维护成本；[vendor 契约](../vendor/hyper/RGNIX.md)记录升级时必须保护的边界。

## 分阶段操作计数

Heaptrack 使用四条持久连接、2,000 个完整校验响应的请求，减去独立零请求启动控制；进程直接 exec 并固定 CPU 10，没有把 taskset 启动器当作被测进程。以下数字包括少量后台噪声，累计申请字节不等于 RSS。

| 阶段，1 KiB 响应 | 分配次数/请求 | 累计申请字节/请求 |
|---|---:|---:|
| 原版 bf0b414 | 42.330 | 20,608.3 |
| 仅请求头/IP 优化 | 38.376 | 20,340.2 |
| 再隔离 Hyper，关闭 full-body feature | 38.262 | 20,345.8 |
| 开启 full-body，最终保留版本 | 30.496 | 19,845.4 |
| 固定 worker 试验，已撤回 | 30.499 | 19,821.7 |

最终版本相对原版少约 **28.0% 分配调用**。隔离依赖与 body 改动分别构建，避免把 feature 组合变化算成 body 优化。小幅小数差异属于后台计数噪声，不能据此归因。

额外发送 64 个自定义请求头时，仅请求头优化使分配从 **170.212 降至 102.312 次/请求**，约少 **39.9%**；累计申请从 29,863.3 降至 20,964.8 字节/请求。这里没有测量完整最终版本的 64 头组合，不能把两项百分比相加。

对 16 KiB 响应，full-body 关闭/开启分别为 **38.282 / 38.403 次分配**、**45,590.3 / 45,597.0 累计申请字节/请求**。超过阈值的响应仍走流式通道，没有小响应路径的分配收益。

另用 libc uprobes，单条持久连接、10 次预热、500 次校验请求，统计实际动态调用：

| 指标/请求 | bf0b414 | 最终保留版本 |
|---|---:|---:|
| 捕获的 memcpy 调用 | 90.990 | 80.998 |
| 捕获的复制字节 | 12,715.6 | 12,084.9 |
| clock_gettime 调用 | 24.392 | 24.372 |

复制字节减少约 **5.0%**，时钟调用基本不变。各批事件数与 perf 捕获样本数一致，临时 probes 已清除。探针不包含内联复制，且包含后台活动；上述分配、复制和时钟数据并非同一个 CPU 成本分母。

部分 Heaptrack 操作计数与另一个候选的 release 构建重叠，所以只用于调用计数，不用于延迟/CPU 推断。无 profiler 的吞吐校准没有与构建、集成测试或 profiler 并行。

## 固定 worker 试验为何撤回

试验让每个 worker 拥有单线程 reactor 和客户端池。上下游连接在接入的 reactor 内推进，进程/后端预算仍共享；每组空闲池总容量按 worker 分摊，没有乘大预算。listener 使用复制的监听 FD，没有实现 SO_REUSEPORT 或显式负载均衡。客户端中的 Mutex/Arc/Atomic 仍存在，不能称为无锁实现。

单 worker 操作计数中，时钟读取降至 20.088 次/请求，但复制变成 12,364.9 字节/请求，比仅 body 优化多 280 字节。1、2、4 worker 各通过 45 项产品行为检查，随后按预先固定规则做 A/A：64 连接，每组 3 对交替窗口，每窗预热 3 秒、测量 10 秒；每对 RPS 对称差和两组 CV 均须不超过 10%，并要求零错误。

客户端使用四线程/CPU 2～5，上游四 worker/CPU 6～9，前端使用 CPU 10 起的对应大小集合。这仅固定虚拟 CPU 集合，不为每个线程绑独立 CPU，也不保留宿主机物理核心。

| worker | 三对 RPS 对称差 | 两组 CV | 最大 P99 | 结果 |
|---|---|---|---:|---|
| 1 | 31.27%、22.81%、2.56% | 19.01%、6.51% | 4.330 ms | FAIL |
| 2 | 8.15%、18.84%、8.43% | 11.60%、2.36% | 19.581 ms | FAIL |
| 4 | 21.79%、29.48%、19.34% | 22.14%、17.47% | 5.132 ms | FAIL |

18 个正式窗口共 **18,100,685 请求，零 wrk 错误**。4 worker 的部分窗口仅三个数据面线程消耗超过 0.1 秒 CPU，存在明显 CPU 工作分布不均的现象；尚不能仅凭这些计数确定连接接入倾斜或虚拟机调度各占多少影响。

各 worker 设置在候选自身校准失败后停止，未进入旧版/NGINX 校准或 A/B。失败不能证明固定 worker 比原架构慢，但不足以支持引入新调度和池碎片的代价，因此恢复原共享调度器。原试验补丁、二进制身份及全部窗口留在机器证据中，未挑选表现好的窗口作为扩展曲线。

## 最终保留版本的吞吐资格

撤回 worker 改动后，对保留的请求头/body 二进制重新做独立 A/A，使用上述相同规则，限定 1 worker、64 连接。这里测的是另一份明确摘要的二进制，不是重复之前的失败试验直到通过。

结果仍为 **FAIL**：三对对称差 **2.95%、17.10%、29.49%**，两组 CV **3.75% / 22.25%**。六个正式窗口共 **3,114,818 请求，零 wrk 错误**；RPS 范围 **38,307～60,418**、CPU **16.54～25.94 µs/请求**、最大 P99 **3.914 ms**、峰值 PSS **22.90 MiB**。这些是同一程序在不同窗口的观测值，不能读成优化前后变化。

按预定规则没有继续原版/NGINX 校准或吞吐 A/B，也没有对最终版本重跑 2/4 worker 容量测试。由此只确认分配和捕获的复制调用减少，**不确认吞吐提升、尾延迟改善或接近 NGINX**。短 loopback 测试及共享虚拟机噪声仍不能替代稳定机器上的容量验证；也不能用这组数与过去 NGINX 表格相除得到当前差距。

完整窗口：[固定 worker 1](validation/nginx-aligned-aa-candidate-1w-2026-09-28.json)、[2](validation/nginx-aligned-aa-candidate-2w-2026-09-28.json)、[4](validation/nginx-aligned-aa-candidate-4w-2026-09-28.json)、[最终版本](validation/nginx-aligned-aa-final-candidate-1w-2026-09-28.json)。

## 行为与构建验证

新增测试集中保护两个现实边界：完整小响应的消费/连接复用，以及未完整小响应首片段不能等待尾部。另扩展产品集成，覆盖 64 头转发、原始重复/非 UTF-8 头变量、header/cookie 限流键和固定路由的日志字段。

- vendored Hyper 现有单元套件：**117 通过、6 ignored**，包含新增两个边界测试。
- vendored hyper-util `legacy_client`：**21 通过**，覆盖提前响应、取消、额外响应字节、chunked 复用和升级。
- 最终版本真实 socket 集成：**Hyper 45 项在 1、2、4 worker 分别通过；Pingora 42 项通过**。包括流式上传、慢响应、取消、POST 不重放、共享预算、热更新/回退、OTLP 和优雅退出。
- 最终源码根项目库测试：**24 通过、1 ignored**。`cargo check --locked`、实验 feature 的 Clippy `--all-targets -D warnings`、格式与 diff 检查通过。

上游测试的 `full` features 不代表产品新增 H2 支持。上游既有重复 ignore 警告保留；最初新增单元测试缺少一层 Result 解包、worker 试验访问私有字段的构建错误均已修正。依赖隔离阶段还有一次 manifest 不完整读取失败，之后 locked 构建成功；这些失败没有计入通过数。

没有重跑全部默认引擎 94 项集成，没有 amd64、真实网卡、长时间浸泡或 TLS/H2/RGL/Ingress 迁移验证。实验支持边界见 [Hyper 数据面文档](hyper-experimental.md)。

## 复现与制品

```sh
cargo build --locked --release --features hyper-experimental
cargo clippy --locked --features hyper-experimental --all-targets -- -D warnings
python3 scripts/hyper_integration.py /path/to/rgnix
python3 scripts/hyper_integration.py /path/to/rgnix --pingora
cargo test --manifest-path vendor/hyper/Cargo.toml --features full,rgnix-full-body --lib
cargo test --manifest-path vendor/hyper-util/Cargo.toml --features full,hyper/rgnix-full-body --test legacy_client
```

vendor 测试使用根 lockfile 的副本解析测试依赖，独立测试 lockfile 不纳入提交；产品构建由根 `Cargo.lock` 锁定。消融时只关闭根 Hyper 依赖的 `rgnix-full-body` feature，其余隔离依赖不变。

原版二进制 SHA-256：`f3857b17334587caf19cfe868df3e6c1bab0c894f553862582ed692a67218878`。

最终保留版本 SHA-256：`2e61dfd9255e18ba26777138787f2d4cdb8cc65ecebfebead6b59cabdc1ebbc9`。

[机器证据](validation/nginx-aligned-2026-09-28.json)包含各阶段二进制摘要、保留源码摘要、构建/测试输出、计数驱动、预先定义的校准驱动、撤回的 worker 补丁及窗口文件列表。原始 perf/Heaptrack 数据留在 Linux `/tmp/rgnix-nginx-opt-20260928`，不是永久制品。

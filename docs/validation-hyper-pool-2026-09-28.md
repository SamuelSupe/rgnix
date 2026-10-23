# Hyper 连接复用路径优化

基于 `experiment/replace-pingora` 的 `20a9b4c`，在 OrbStack Ubuntu ARM64、Rust 1.90.0 上验证。改动仅在实验性 `vendor/hyper-util` 客户端；默认 Pingora、reqwest/kube 的注册表依赖没有变更。

## 实现与代价

客户端的不可变配置、connector、协议 builder 和池句柄保存在共享 `Arc<ClientInner>` 中。请求只复制这个句柄，实际建立连接时才复制 connector/builder。

取连接时先 poll 原有 checkout。空闲连接命中后立即返回；未命中时，保留已经注册的 waiter，将原有 checkout/connect 竞争放入按需创建的 boxed future。这样，持久连接请求无需携带握手和竞争状态。空闲过期、不可用连接检查、取消清理、响应 body 所有权和产品禁用自动重试的策略继续沿用原有实现。

每个客户端新增一次共享状态分配，每次冷连接增加一次 future 分配。这是明确的取舍，并不适合被表述为所有请求都减少分配。两项改动作为一个候选整体测量，没有分别做吞吐归因。

## 操作计数

相同 1 KiB 普通代理 fixture，单 worker/CPU 10。libc uprobes 使用一条持久连接、10 次预热和 500 个校验响应的请求。Heaptrack 使用四条下游持久连接、2,000 个校验响应的请求，并减去独立零请求启动控制。工具分开运行，不与无 profiler 的校准并行。

| 每请求指标，持久上游连接 | 20a9b4c | 本轮 | 变化 |
|---|---:|---:|---:|
| 捕获的 libc 复制字节 | 15,043.7 | 12,715.6 | −15.48% |
| memcpy 调用 | 95.99 | 90.99 | −5.21% |
| 分配次数 | 43.36 | 42.37 | 约少 1 次 |
| 累计申请字节 | 21,445.4 | 20,620.4 | −3.85% |
| clock_gettime 调用 | 24.37 | 24.43 | 基本不变 |

客户端 request/try_send_request 调用点捕获的复制合计从 6,184 降至 3,856 字节/请求。内联可能改变调用点归属；探针不包含内联复制，且含少量后台活动。新旧各批事件数分别为 60,184 和 57,712，与 perf 捕获样本数一致。Heaptrack 的字节是累计申请量，不是存活内存或 RSS。

另外将同一上游 fixture 的 `keepalive_requests` 从 1,000,000 改为 1，使每次请求重新建立上游连接，保留下游四条持久连接：分配从 **66.35 增至 67.35 次/请求**，累计申请量从 **46,190.7 增至 46,287.3 字节/请求**。即每请求约多一次分配、97 字节；2,000 次请求的状态码与完整响应均通过校验。这项数据不代表连接建立延迟或高并发短连接容量。

新旧各完成一次 15 秒、199 Hz 的 CPU-clock/DWARF profile，均没有丢失采样或请求错误。采样中内核唤醒、时钟和网络处理仍较突出；百分比分布不能证明绝对 CPU 降幅，也不能把内核成本直接归因于 Hyper。该结果不支持只靠继续缩小复制便能追平 NGINX 的判断。

## 行为与构建检查

- `cargo check --locked --features hyper-experimental`、Clippy `--all-targets -D warnings`、locked release 构建通过。
- 产品真实 socket 集成：**Hyper 40 项、Pingora 37 项通过**。覆盖流式请求/响应、超时、取消、POST 不重放、预算释放、热更新/回退、日志/span 和优雅排空。
- vendored 客户端的现有 `legacy_client` 集成 **21 项通过**，连接池单元 **9 项通过**。包含空闲过期、关闭/失效连接、取消和 waiter 清理、禁用 keepalive、chunked 复用、上传结束前返回响应及升级边界。没有为内部结构变化添加重复测试。
- vendor 测试使用 `full` features，以满足上游测试自身所需的 `server-auto`/H2 支持；这不表示产品 Hyper 数据面支持 H2。

vendor 测试目录使用主项目锁文件副本解析依赖，未改动主项目 `Cargo.lock`。最初离线运行缺少缓存依赖，第一次不完整 feature 集合缺少 `server-auto`；补齐测试环境后上述 30 项实际运行通过。上游单元测试仍有既有重复 `#[ignore]` 警告，产品 Clippy 无警告。

本轮没有重复运行上一轮的全部默认引擎 94 项和根项目单元测试，也未做 amd64、多 worker、真实网卡、长时间浸泡、TLS/H2/RGL/Ingress 迁移验证。实验运行边界仍见 [Hyper 文档](hyper-experimental.md)。

## 吞吐资格

使用独立 loopback namespace，前端/客户端/上游分别固定 CPU 10/2/6；1 worker、64 并发。运行前固定规则：3 对交替窗口，每窗预热 5 秒、测量 15 秒；每一对 RPS 对称差、两组样本 CV 都不得超过 10%，并要求零错误。依次校准旧版、新版、NGINX，任何一步失败即停止，不挑选窗口，不重复直到通过。

旧版同二进制 A/A **FAIL**：三对对称差为 **35.68%、18.92%、8.78%**，两组 RPS CV 为 **11.21% / 19.89%**。六个测量窗口共 **4,707,790 请求，零 wrk 错误**，最大 P99 **7.116 ms**。这里两个引擎标签都指向旧版同一二进制，不能把它们读成新旧比较。

按预定规则没有继续新版/NGINX A/A 或 A/B，也没有运行 256 并发。可以确认持久连接的操作计数下降；不能确认吞吐提升、尾延迟改善或与 NGINX 的差距收窄。当前环境波动原因仍未确诊。完整数据见 [全部窗口](validation/hyper-pool-aa-before-2026-09-28.json) 与 [机器证据](validation/hyper-pool-2026-09-28.json)。

## 复现和身份

旧版 SHA-256：`89bcd6456bd1a4c83ad6270ab7d9c944c7685ff1e7731b2bbf0334738996d2be`。

新版 SHA-256：`f3857b17334587caf19cfe868df3e6c1bab0c894f553862582ed692a67218878`。

```sh
cargo clippy --locked --features hyper-experimental --all-targets -- -D warnings
cargo build --locked --release --features hyper-experimental
python3 scripts/hyper_integration.py /path/to/rgnix
python3 scripts/hyper_integration.py /path/to/rgnix --pingora
cp Cargo.lock vendor/hyper-util/Cargo.lock
cargo test --locked --manifest-path vendor/hyper-util/Cargo.toml --features full --test legacy_client
cargo test --locked --manifest-path vendor/hyper-util/Cargo.toml --features full --lib client::legacy::pool::tests
```

复制/时钟计数使用已有 `scripts/profile_http_primitives.py`，CPU 采样使用 `scripts/profile_http.py`，分别传入新旧二进制。所有 profile 命令、二进制摘要、构建和行为输出、Heaptrack 驱动、校准驱动与源文件摘要保存在机器证据中。原始 perf/heaptrack 数据保留于测试机 `/tmp/rgnix-hyper-pool-20260928`，不是永久制品。

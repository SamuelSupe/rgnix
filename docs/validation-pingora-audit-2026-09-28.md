# Pingora 优化空间：源码审计与独立诊断

2026-09-28。审计没有修改产品代码；以下候选优化尚未实施，不属于 v0.5.0 的性能收益。性能运行使用版本升级前的 round-nine ARM64 二进制，SHA-256 为 `92de0b2c9901d15f5be5b54d04e67daa3ef9f5299f70ee72096b828bf6e9b2d4`。

## 当前二进制的独立诊断

OrbStack Ubuntu ARM64，隔离网络命名空间，普通 HTTP/1.1 keepalive、共同 1 KiB 上游、64 连接、单 worker 绑定 CPU 10；三轮交错执行，每窗预热 2 秒并测量 8 秒。无 profiler 的正式数据如下，CV 使用样本标准差：

| 前端 | 中位 req/s | CPU µs/请求 | RPS CV | P99 中位 ms |
|---|---:|---:|---:|---:|
| rgnix round nine | 60,020 | 16.642 | 10.97% | 1.921 |
| 最小 vendor Pingora | 61,779 | 16.188 | 16.82% | 2.380 |
| NGINX 1.30.5 | 114,027 | 8.760 | 8.84% | 1.256 |

最小示例使用当前修改过的 vendor，省略产品路由、预算及遥测，不是官方未修改 Pingora 或可部署的替代模式。NGINX 每轮均领先，但最小示例与 rgnix 的 2.93% 中位数差异不能视作已证实的功能成本：两者 CV 超过 10%。前置单 worker A/A 最大配对差 5.63%，只通过该限定配置的校准，没有解除此前双 worker 及 P99 异常。

6 个 A/A 与 9 个三方窗口共 8,332,824 请求，零 wrk 错误。单独双 worker CPU 采样并非正式吞吐对照；其最小 vendor 示例中 header/parse 调用链约 9.69%、allocator 调用链约 2.50%，类别包含子调用且可重叠，不能相加为可恢复收益。原始栈统计保留符号未解析比例。

## 与上游逐项比对

固定官方主干 [4487f7b2](https://github.com/cloudflare/pingora/commit/4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19)。8 个本地 Cargo registry 文件与官方 0.9.0 tag 字节一致；主干选定的 HTTP/1 body/client/server/connector 和 HTTP header 文件与该 tag 一致。代理层泛型 downstream session 等代码已有变化，本轮没有测试主干整体性能。[逐文件摘要](validation/pingora-audit-sources-2026-09-28.json)。

| 候选 | 已确认依据 | 实施边界 |
|---|---|---|
| 免预初始化解析数组 | 请求/响应每次解析尝试各初始化 256 个 32 B Header；release 汇编仍有各 8,192 B 存储循环 | 使用现有 httparse 未初始化数组接口，保留 header 上限、解析选项与校验 |
| body 所有权交接 | 1 KiB 响应仍约申请一次 64 KiB BodyReader 缓冲，再复制为 Bytes | 按切片传递预读数据，限制缓冲与滞留内存；不单纯缩小大流 read 大小 |
| 批量 vectored write | 多任务 body 仍 put_slice 拼接到连续输出缓冲 | 保留单任务直接路径、部分写进度、取消、SSE flush 与 TLS 语义 |
| 复用 HTTP 工作区 | 连接池保留 stream，但重新构造 HTTP session | 区分连接缓冲和单请求 framing/timeout/peer 状态，限制容量 |
| 减少 header 交接克隆 | HeaderMap 与大小写映射并存，跨阶段 clone | 保留重复头与原始上游 framing/keepalive 元数据，不能直接用修改后的响应头判断复用 |

解析初始化的 ARM64 指令中，请求 `0xb1bf70`、响应 `0xb195b4` 均以 `0x20` 递增并写入，直到 `0x2000`。这是栈上存储，不是堆分配或 body 复制，热缓存成本需要实测。[解析接口](https://docs.rs/httparse/1.10.1/httparse/struct.ParserConfig.html#method.parse_response_with_uninit_headers)、[当前请求解析](../vendor/pingora-core/src/protocols/http/v1/server.rs)、[响应解析](../vendor/pingora-core/src/protocols/http/v1/client.rs)。

64 KiB 缓冲占此前 heaptrack 累计申请字节 62.39%，**不是 CPU 占比、RSS 或预期吞吐收益**。上游已有下游 owned buffer 入口和部分 chunked vectored write 支持；需优化的是尚未贯通的生命周期与批量交接。[官方 body](https://github.com/cloudflare/pingora/blob/4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19/pingora-core/src/protocols/http/v1/body.rs)。

更大的候选是紧凑 HTTP/1 转发状态机。当前最小 vendor 示例即使 CTX 为单元类型，入口也 malloc/memcpy 7,272 B；该精确大小包含本地补丁影响。两条转发方向由同一父 Future 驱动，不是每请求启动两个独立 Tokio task。完整重构必须保持早响应、背压、取消和调度公平性，目前没有收益验证。

## 本地补丁的单独风险

[TransportPool](../vendor/pingora-core/src/connectors/pool.rs)是我们的修改：单把 Mutex 管理池内全部分组，100 ms sweep 期间持锁检查 socket。它省去每次归还的 watcher task，但可能产生多 worker 争用；目前未证明它导致了先前 P99 异常。后续需要原版 0.9.0、当前 vendor 和候选的 1/2/4/8 worker 对照，不能将该设计归因给官方。

## 证据与范围

[A/A 全部窗口](validation/pingora-audit-aa-2026-09-28.json) · [三方全部窗口](validation/pingora-audit-compare-2026-09-28.json) · [分析与门槛](validation/pingora-audit-analysis-2026-09-28.json) · [CPU 栈统计](validation/pingora-audit-stacks-2026-09-28.json) · [分配归因](validation/pingora-audit-heap-sizes-2026-09-28.json)。

原始 JSON 中的 `.local`/`/tmp` 路径描述当时执行环境，并不表示这些二进制、完整 perf/heaptrack 文件随 GitHub 源码发行。发布的汇总足以核对本文数值；完整重采集仍需原生 Linux 工具与对应构建。没有新增候选 A/B、amd64 性能、真实 NIC、TLS/H2 容量或长期稳定性验收。性能发布资格仍未通过，v0.5.0 仅作为经过功能门禁的预览发布。

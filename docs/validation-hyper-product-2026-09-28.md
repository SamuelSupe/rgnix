# Hyper 产品路径集成与验证（2026-09-28）

本轮在 `experiment/replace-pingora` 分支、独立原型提交 `74e488e` 之后，将明文 HTTP/1 候选路径接入真实 rgnix。默认引擎仍是 Pingora；通过 `--features hyper-experimental` 构建后，可用 `serve --experimental-hyper` 选择候选路径。[支持范围、资源及协议差异](hyper-experimental.md)是本记录的前提。

## 已完成的验证

环境为 OrbStack Ubuntu、Linux ARM64、Rust 1.90.0 / LLVM 20.1.8。release 使用 thin LTO 与 codegen-units=1。候选与默认路径使用同一个特性构建，SHA-256：

```text
21a986afd26f64119b9a9b82b96567b460f10d8a67b7058f3b3ca9bca70d0852
```

| 验证 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --features hyper-experimental --all-targets -- -D warnings` | PASS |
| `cargo test --locked --lib`（默认构建） | 22 PASS、1 个既有路由微基准 ignored |
| `cargo build --locked --release --features hyper-experimental` | PASS |
| Hyper 真实 socket 行为脚本 | 35 PASS |
| 相同脚本的 Pingora 共同契约 | 32 PASS |
| 原有 `scripts/integration.py`，Pingora 模式 | 94 PASS |

[完整检查名称、编译器、构建与源码摘要](validation/hyper-product-checks-2026-09-28.json)。这些套件有重叠，不将总数当成不同功能的数量。

Hyper 行为验证包含 `Connection` 关键字段保护、域名/精确/前缀路由、URI 替换与原始编码、HEAD、定长与 chunked 大小限制、8 MiB 上传校验、不可重放 POST、上游首部/body 读超时、上传写阻塞超时、限速、进程/路由/后端并发预算、keepalive 到期、流水线请求、流式响应与取消、热更新保留旧请求快照、不支持的插件更新保留上一版本、优雅排空，以及 OTLP server/client span、W3C 传播、OTLP 与本地日志关联和错误指标。

共享完成逻辑从原 Pingora logging 回调提取；候选路径在 body 结束且 socket flush 完成时记录结果，取消及错误通过 RAII 释放许可。原有 94 项主流程覆盖 TLS/H2、静态文件、RGL、WebSocket/SSE、请求取消及控制器故障等，用于检查提取共享逻辑没有破坏默认引擎；这些测试通过**不表示**候选已支持这些功能。

验证期间修正了测试 fixture：访问日志语法、预算指标标签、限流状态、注入超时后的被动摘除设置和流式同步屏障。流水线复测中发现默认 Pingora 只发送首条响应就关闭连接；其现有配置明确关闭 pipelining，故流水线断言仅针对 Hyper，其余共同契约继续运行。没有修改 Pingora 的协议行为来凑齐相同检查数量。

本轮没有进行 amd64、TLS/H2/RGL 候选迁移、Ingress/Gateway、长时间浸泡、多 worker 或跨节点测试。大量地址下空闲池的总量隔离、连接建立/复用细分指标仍未对齐。

## 性能校准

使用一个 release 二进制的两个标签进行每种模式的 A/A，随后只有两种模式均通过既定门槛才继续 Pingora/Hyper/NGINX A/B。共同配置为明文 HTTP/1、1 KiB 真实上游响应；访问日志与 OTLP 关闭，产品路由、预算、超时和指标保留。不同模式的调度和池实现也属于此次替换范围。

代理 1 worker / CPU 10；wrk 1 thread / CPU 2；OpenResty 上游 1 worker / CPU 6；私有 network namespace、loopback；64 连接、3 轮交替，每窗预热 2 秒、测量 10 秒。vCPU 绑定不保证独占物理核心。门槛沿用最大对称配对差和各组 RPS 样本 CV 均不超过 10%；不删除异常窗口。

| 模式 | 最大配对差 | 两组 RPS CV | 结果 |
|---|---:|---:|---|
| Pingora | 36.94% | 6.45% / 14.31% | FAIL |
| Hyper | 10.68% | 9.78% / 1.76% | FAIL |

共 12 个测量窗口、7,404,067 个请求、零 wrk 错误，不含预热和上游基线。两种模式未通过门槛，脚本按设计返回 2，**未执行 NGINX A/B**；不能用这些 A/A 样本的均值或中位数算“提升百分比”。原始数据包含每个窗口的 CPU、P99、PSS、负载、配置和调用参数：[Pingora A/A](validation/hyper-product-aa-pingora-2026-09-28.json)、[Hyper A/A](validation/hyper-product-aa-hyper-2026-09-28.json)、[门槛及校准构建身份](validation/hyper-product-calibration-2026-09-28.json)。没有新结论证明产品路径接近 NGINX。

**构建边界：**校准使用 SHA-256 `bbdb885a…a792d86` 的边界补丁前构建。随后复核发现 Hyper 尚未复用现有 `Connection` 关键字段校验；新增共同回归检查并完成补丁后重建。上面的行为表以最终构建为准；最终构建未再做吞吐测量，失败的旧窗口也没有被覆盖。

## 复现校准

先以 `MODE=pingora` 运行，再以 `MODE=hyper` 运行；更换新目录防止覆盖前次证据。使用原生 Linux 文件系统上的二进制与 fixture，并将脚本放在私有网络命名空间内运行：

```sh
python3 scripts/benchmark_compare.py \
  --rgnix /path/to/rgnix --candidate /path/to/rgnix \
  --nginx /path/to/nginx --openresty /path/to/openresty \
  --engines rgnix candidate --plain-proxy --cases proxy-1k \
  --rgnix-transport "$MODE" --candidate-transport "$MODE" \
  --workers 1 --origin-workers 1 --client-threads 1 \
  --server-cpus 10 --client-cpus 2 --origin-cpus 6 \
  --concurrency 64 --rounds 3 --warmup 2 --seconds 10 --interleave \
  --work-dir "/tmp/new-aa-$MODE" --output "/tmp/new-aa-$MODE.json"
```

目前的结果证明受限产品路径可运行，并发现了还需迁移的协议与资源边界。下一步应先对已集成路径做 CPU/分配剖析，区分产品逻辑、连接池和调度开销；可靠的性能结论需要更稳定的隔离测量环境。不会依据最小代理的历史吞吐直接推进全量替换。

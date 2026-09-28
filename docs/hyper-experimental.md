# 实验性 Hyper HTTP/1 数据面

`experiment/replace-pingora` 分支将 Hyper 接到真实 `rgnix` 二进制，不再只测独立的最小代理。默认构建与默认运行仍使用 Pingora；该实验尚不能替换完整的生产数据面。

```sh
cargo build --locked --release --features hyper-experimental
./target/release/rgnix serve -c nginx.conf --experimental-hyper
```

不传运行参数时，同一个二进制走原有 Pingora 数据面，便于排除编译器、依赖版本和构建配置差异。实验仍复用 Pingora 的进程、后台服务、管理接口和错误类型，只替换业务 HTTP/1 的协议处理、上游连接池及流式传输。

## 已接入的产品能力

- 配置解析、不可变请求快照、虚拟主机、精确/前缀路由和 `proxy_pass` URI 语义。
- 直接响应、请求/响应头、变量、可信代理地址解析、IP 访问控制、加权轮询/最少连接/hash 后端选择。
- 进程、路由和后端并发预算、本地限速、请求体大小限制；流式转发，关闭自动重试。
- 上游连接及读写空闲超时、下游 keepalive、请求取消和有界优雅退出。
- 原有请求/路由/上游错误指标、访问日志及轮转、OTLP 日志与 span、W3C 上下文传播。
- SIGHUP 与管理发布复用原有控制面。候选快照在发布前检查传输兼容性；不支持的更新或回退被拒绝，保留上一有效快照。已开始的请求保持原版本。

请求完成逻辑在两种数据面间共享。并发许可与上游租约保持到响应写入 socket，或错误/取消终止，不能在收到上游响应头时提前释放。这里的写入完成指交给本机 TCP socket，不代表远端应用已消费；传输失败时字节指标可能包含已交给编码器但未送达的 body 数据。

## 支持边界与差异

只允许独立文件配置、明文 HTTP/1 客户端和明文 HTTP/1 上游。启动和配置发布会拒绝 TLS、HTTP/2、PROXY protocol、RGL/body 检查、认证、静态文件、压缩、sticky cookie、多租户/发布策略、外部共享限流配置及 Ingress/Gateway 模式。CONNECT、Upgrade 和声明请求 trailer 的请求返回 501。尚不提供 WebSocket 或 trailer 兼容性承诺。

- Hyper 空闲池上限按**监听器、超时配置组合、目标 authority**计算，每组上限是 `upstream_keepalive_pool_size × threads`，不是现有 Pingora 的总量上限。活跃请求仍受进程和后端预算约束；大量端点/超时组合下的空闲连接总量隔离仍待迁移。
- 超时不同的路由使用不同客户端连接池。新配置版本更换池；旧请求继续使用已取得的客户端，不把旧客户端重新写进新版本缓存。
- 下游首部总期限为 60 秒；下游 body 读、响应写空闲期限为 60 秒。完成响应后使用路由的 `keepalive_timeout`。上游使用路由 connect/read/write 超时；底层读空闲期限可能比 keepalive 更早关闭空闲上游连接。
- Hyper 请求 future 同时覆盖上传和等待响应头。无法可靠区分的上游 I/O 超时使用 `upstream_timeout` 错误标签；响应 body 读超时仍使用 `read_timeout`，来源标签为 `upstream`。没有伪造分阶段时间。
- 尚未迁移上游连接建立/复用的细分指标；请求数、字节、上游首部耗时、整体上游耗时、错误和预算指标已经接入。
- Hyper 用 Tokio 共享调度器分配连接任务；Pingora 保留原有调度模式。传输对照包含这项调度差异，不能归因为单一 parser 优化。
- Hyper 顺序处理 HTTP/1 流水线请求；默认 Pingora 配置关闭 pipelining，发送首个响应后关闭连接。本次未修改默认路径的行为。

## 验证与比较

行为脚本覆盖两种运行模式的共同契约，并检查实验模式拒绝不支持的更新：

```sh
python3 scripts/hyper_integration.py /path/to/rgnix
python3 scripts/hyper_integration.py /path/to/rgnix --pingora
python3 scripts/integration.py /path/to/rgnix
```

使用 `scripts/benchmark_compare.py --plain-proxy --cases proxy-1k`，通过 `--rgnix-transport` 与 `--candidate-transport` 选择两种模式；两条程序路径可以指向同一个构建。A/A 时两种标签必须使用相同模式，分别校准 Pingora 和 Hyper。使用相同配置、CPU/并发、窗口长度和共同上游，门槛通过后才解释 A/B；不能用最小代理的结果代替产品路径。

[本轮实际验证记录](validation-hyper-product-2026-09-28.md)与[独立原型的历史结果](validation-hyper-prototype-2026-09-28.md)分开记录。当前未完成 TLS/H2/RGL/认证和完整连接资源隔离的迁移，也没有多 worker、跨节点或生产容量结论。

后续 [CPU 剖析与上下文搬移优化](validation-hyper-profile-2026-09-28.md)将请求状态放在固定堆位置，减少它在异步状态和完成队列中的按值复制。捕获的 libc 拷贝字节下降约 58.6%，代价是每请求增加一次分配；该操作计数不等同于吞吐提升。

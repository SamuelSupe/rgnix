# 实验性 Hyper HTTP 数据面

`experiment/replace-pingora` 分支将 Hyper 接到真实 `rgnix` 二进制，不再只测独立的最小代理。普通 Cargo 构建包含两种内核，当前开发版本默认运行 Hyper；选择和剩余验收项见[切换记录](hyper-default-rollout.md)。

```sh
cargo build --locked --release
./target/release/rgnix check -c nginx.conf --engine hyper
./target/release/rgnix serve -c nginx.conf --engine hyper
```

Linux GNU 上可同时启用 `--features hyper-experimental,jemalloc`。jemalloc 静态链接为 Rust 全局分配器，不需要 `LD_PRELOAD`；本地 C 库仍使用各自的分配接口。该选项也可单独用于 Pingora 构建，其他目标继续使用系统分配器。它目前是显式启用选项，普通构建不变；内存代价和正式对照结果见[分配器验证记录](validation-allocator-2026-09-28.md)。

实验构建还启用小消息读缓冲复用、预构造的 hop-by-hop 头名，以及禁止重试时的直接请求路径；旧 body 帧仍可安全持有，重试策略不变。此前两 worker / 1 KiB 对照相对仅启用 jemalloc 观测到吞吐 +3.87%、CPU/请求 −3.21%，双方 A/A 通过；16 KiB 和 NGINX 对照未通过完整校准，不能据此外推整体性能。

完整内核的[后续优化与 NGINX 对照](validation-hyper-performance-2026-09-30.md)复用 HTTP/1 提示响应队列、保留已解析的普通代理 URI，并合并小文件读取。普通 1 KiB 代理分配/重分配调用从约 36.5 降到 30.6 次/请求，累计申请字节减少约 43%。最终 90 窗零错误，但没有场景同时通过优化前后或 NGINX 对照的完整校准；静态文件峰值内存也更高，不能宣布稳定吞吐收益或性能对等。

使用 `--engine hyper|pingora` 或 `RGNIX_ENGINE` 选择内核，CLI 覆盖同名环境变量。该选择覆盖旧 `--experimental-hyper[=true|false]` 和 `RGNIX_EXPERIMENTAL_HYPER`；没有任何选择时走 Hyper（未编入 Hyper 的精简构建使用 Pingora）。旧参数仍可使用，显式 false 固定选择 Pingora，用于显式回退。Hyper 启动失败不会静默切换内核。`check`、`serve` 使用相同的内核兼容性校验；`check` 不绑定监听器，也不验证上游可达性。`cargo build --no-default-features` 生成 Pingora-only 二进制，显式选择 Hyper 会报告不可用。

同一个二进制可运行两种数据面，便于排除编译器、依赖版本和构建配置差异。Hyper 使用独立的 Tokio 进程宿主负责 worker、信号和有界退出，仍复用 Pingora 的后台服务接口、管理接口和错误类型。业务 HTTP/1、HTTP/2 的协议处理、上游连接池及流式传输由 Hyper 负责；可选 HTTP/3 使用 Quinn/h3 并共享同一请求策略。认证、RGL、租户准入、后端选择和响应策略与 Pingora 共用实现。

## 已接入的产品能力

- 配置解析、不可变请求快照、虚拟主机、精确/前缀路由和 `proxy_pass` URI 语义。
- 直接响应、请求/响应头、变量、可信代理地址解析、IP 访问控制、加权轮询/最少连接/hash 后端选择。
- 进程、路由和后端并发预算、本地限速、请求体大小限制；流式转发，关闭自动重试。
- 上游连接及读写空闲超时、下游 keepalive、请求取消和有界优雅退出。
- RGL/Wasm 请求和响应钩子、前缀/完整 body 检查，认证、租户隔离、灰度和有界镜像。
- TLS/SNI、HTTP/2/h2c、gRPC/trailers、WebSocket、静态文件和压缩。
- 原有请求/路由/上游错误指标、访问日志及轮转、OTLP 日志与 span、W3C 上下文传播。
- SIGHUP 与管理发布复用原有控制面。候选快照在发布前检查传输兼容性；不支持的更新或回退被拒绝，保留上一有效快照。已开始的请求保持原版本。

请求完成逻辑在两种数据面间共享。并发许可与上游租约覆盖响应流生命周期，不能在收到上游响应头时提前释放。HTTP/1 完成记录随 socket flush；HTTP/2 还受协议库流控队列影响。字节指标表示交给传输编码器的数据，不是远端应用消费确认，传输失败时可能包含未送达数据。

## 支持边界与差异

可用于独立文件配置、Ingress 和 Gateway 模式。已接入 HTTPS/SNI 热更新、客户端 HTTP/2、h2c、HTTP/HTTPS 上游、gRPC 双向流及 trailers、WebSocket、PROXY protocol、RGL 请求/响应钩子与 body 检查、JWT/外部认证/mTLS、静态文件、gzip/Brotli、sticky cookie、租户配额、共享限流和发布策略。启用实验构建和运行参数后，三种模式复用同一数据面。

```sh
rgnix ingress --engine hyper --ingress-class rgnix --publish-service namespace/service
rgnix gateway --engine hyper --gateway namespace/name --publish-service namespace/service
# Helm 使用包含 hyper-experimental 特性的镜像：
helm upgrade --install rgnix charts/rgnix --set engine=hyper
```

CONNECT 通过 `rgnix_connect on;` 显式启用，目的地址必须匹配选中的配置后端或端点，不允许任意目标。路由、认证、RGL 和并发预算覆盖隧道生命周期；隧道空闲读写使用路由超时。HTTP/2 和可选 HTTP/3 支持 Extended CONNECT WebSocket，转发到 H1/H2 上游；自动协商上游的 WebSocket 使用独立 H1 池，显式 H2 上游要求支持 Extended CONNECT。明文 HTTP/2 支持 prior knowledge 和 h2c Upgrade；升级前完整接收初始 HTTP/1 body，最多 1 MiB（也受路由大小/空闲限制），然后执行原请求一次并保留 trailers。HTTPS 上的 h2c Upgrade 被拒绝。WebSocket 握手必须没有请求体；有体握手先执行大小限制，再拒绝升级。配置不能覆盖连接 framing 头，也不能把 1xx 配置为最终响应。上游 102/103 可转发到 HTTP/1、HTTP/2 或 HTTP/3 客户端，不执行最终响应钩子；提示队列最多四条、每条首部最多 64 KiB，过量提示丢弃。

- `--hyper-max-connections` 默认 16384，限制所有监听地址合计的已接收 TCP/QUIC 连接，包含 TLS 握手、keepalive 和 WebSocket。超限关闭新连接，管理端口保持独立；请求和租户并发预算继续单独执行。另按来源 IP、监听地址及等待初始请求/握手的连接分层准入，参数见下表。
- 临时 accept 错误（包括描述符、内存和 socket 缓冲耗尽）采用 25 ms 至 1 s 的退避重试，资源恢复后继续接收连接；不可恢复的监听错误仍触发宿主退出。管理端口也需要系统描述符，进程耗尽时不能保证它仍可连接。
- 每个原生服务 reactor 每 250 ms 更新心跳。任一服务未启动或超过 `--hyper-worker-stall-timeout-seconds`（默认 5 秒）未更新，`/healthz` 和 `/readyz` 返回 503；线程恢复后自动恢复。该检查检测调度停顿，不保证所有路由和上游正常。
- 空闲上游预算 `upstream_keepalive_pool_size × threads` 现在在监听器、路由客户端、端点和配置版本间共享。预算满时优先淘汰当前客户端最旧的空闲连接，否则不缓存新连接。HTTP/2 的共享连接占一个池名额，不按 stream 重复计算。
- TLS 上下文随快照发布一并准备，握手从所捕获的证书版本选择并校验证书。仅路由或插件变化时保留相同监听器的上下文、票据和缓存；证书/私钥、CA、客户端认证、SNI 归属或监听配置变化时创建新安全代次，旧票据失效。单域名且不使用 mTLS 的监听器允许 TLS 1.2/1.3 会话恢复；多 SNI 和 mTLS 监听器仍关闭恢复。已有连接上的请求仍执行当前快照的认证规则。
- 超时不同的路由使用不同客户端连接池。传输设置、TLS 上下文、固定请求/响应头和端点 authority 在发布前准备；池在所属 worker 首次使用时创建。发布仅复用后端配置/端点、监听器、连接/读写/keepalive 超时、协议、SNI、CA 及客户端身份全部相同的池；其他变化使用新池。旧请求继续使用原快照。历史记录不持有空闲池，回退可从当前快照复用安全等价的池。DNS 刷新后的新地址按实际选中的端点解析。
- `client_header_timeout`、`client_body_timeout`、`send_timeout` 默认各为 60 秒。HTTP/1 首部使用连接建立时默认虚拟主机的总期限；HTTP/2 的连接前言、分片 frame header 和 HEADERS/CONTINUATION 首部块也受同一总期限约束，适用于 TLS、prior knowledge 和 h2c Upgrade。零星字节及其他活跃 stream 不延长期限。未完成首部块超时关闭整条 H2 连接，因为 CONTINUATION 不能与其他流的 frame 交错；普通 body 读和响应写仍按 stream/路由配置执行。完成响应后使用 `keepalive_timeout`，活跃 H2 stream 不受连接空闲计时器干扰。上游使用独立 connect/read/write 超时；HTTP/1 空闲上游不运行响应读取期限，新请求 flush 后登记新的响应期限。
- HTTP/2 响应头和 DATA 流控分别使用每个 stream 的读写期限；其他 stream 的活动不能延长它们。当前 stream 的上传进展可以延长自己的响应头期限。
- Hyper 请求 future 同时覆盖上传和等待响应头。无法可靠区分的上游 I/O 超时使用 `upstream_timeout` 错误标签；响应 body 读超时仍使用 `read_timeout`，来源标签为 `upstream`。没有伪造分阶段时间。
- `rgnix_upstream_connect_seconds{backend,reused}` 记录连接获取时间和是否复用。新增 `rgnix_hyper_connections`、`rgnix_hyper_connection_rejections_total`、`rgnix_hyper_tls_handshake_errors_total`。标签不包含请求路径或任意域名。
- Hyper 每个 worker 使用独立的单线程 Tokio reactor，连接及其显式 HTTP/1 上游池在该 worker 内推进。Linux 多 worker 使用 SO_REUSEPORT 分配新连接；其他平台共用接受 socket，但本轮只在 Linux 验证。HTTP/2 和 ALPN 自动协商继续共享客户端池以保留跨 worker 多路复用；各 stream 的超时仍独立。进程连接、请求、租户和后端预算，以及空闲上游总预算继续共享。Pingora 保留原有调度模式；对照包含这项 worker 和池归属调整，不能归因为单一 parser 优化。
- 静态文件复用安全打开、条件请求和 Range 选择逻辑。Linux 明文 H1.1 且不启用压缩时使用已安全打开的文件描述符进行 sendfile；每次最多 64 KiB，256 KiB 后让出调度。TLS/H2/H3/压缩继续流式发送。文件截断会关闭不完整响应；指标和许可在传输完成或失败时结算。
- Hyper 顺序处理 HTTP/1 流水线请求；显式选择 Pingora 时配置关闭 pipelining，发送首个响应后关闭连接。

### 连接隔离参数

以下参数只作用于 Hyper 业务监听器。四个分层限额的 `0` 表示自动选择；进程总限额必须为 1～1000000。分层显式限额不得超过 1000000，实际值受进程总预算及适用的 IP/待建预算限制；非零自动值至少为 1。

| CLI 参数 | 自动值/默认值 | Helm `experimentalHyper` 字段 |
|---|---|---|
| `--hyper-max-connections` | 16384 | `maxConnections` |
| `--hyper-max-connections-per-ip` | `min(总连接数 / 4, 1024)` | `maxConnectionsPerIp` |
| `--hyper-max-connections-per-listener` | `总连接数 / 监听地址数` | `maxConnectionsPerListener` |
| `--hyper-max-handshakes` | `min(总连接数 / 4, 256)` | `maxHandshakes` |
| `--hyper-max-handshakes-per-ip` | `min(IP 连接上限 / 2, 16, 待建连接上限)` | `maxHandshakesPerIp` |
| `--hyper-worker-stall-timeout-seconds` | 5 秒，允许 1～3600 秒 | `workerStallTimeoutSeconds` |

TCP 待建预算覆盖 PROXY 解析、TLS 握手和第一份完整请求首部，到达请求处理器时释放；keepalive/隧道继续占连接预算。QUIC 在 TLS 握手完成时释放待建预算，等待 HTTP/3 首部的 stream 继续使用请求预算。同地址的 TCP/QUIC 共用监听限额。来源使用 socket 对端 IP，IPv4-mapped IPv6 归一化，不采用转发头或 PROXY 声明；QUIC 按建立连接时的地址计数。

可信前置代理、NAT 或压测客户端共享 IP 时，应按预期并发提高两个 IP 限额。监听器上限保留地址间容量，未用额度不会自动借给其他地址；原有租户请求预算继续执行，这不是租户级进程/内存隔离。来源计数仅保留仍有连接的 IP，表大小受总连接预算约束。

源码 Docker 构建与未来原生发行包启用 `http3,jemalloc`，同一产物默认使用 Hyper，可显式选择 Pingora。发布门槛覆盖两种模式的产品流程、原生发行二进制的 Hyper/HTTP3 回归，以及相同镜像的两种 Gateway 模式。新增门槛的远端运行仍需对应版本 CI 确认；当前实现与本地范围见[主内核加固记录](validation-hyper-primary-2026-10-01.md)。

## 可选 HTTP/3

```sh
cargo build --locked --release --features http3,jemalloc
rgnix serve -c nginx.conf
# nginx.conf 的 TLS server/http 上下文中加入 http3 on;
# Ingress/Gateway 的监听由资源生成，用此选项为 TLS 监听开启 UDP：
rgnix ingress --hyper-http3 --ingress-class rgnix --publish-service namespace/service
helm upgrade --install rgnix charts/rgnix --set image.repository=YOUR_REPOSITORY --set image.tag=YOUR_TESTED_TAG --set experimentalHyper.http3=true
```

需要包含 `http3` 特性的镜像。UDP 与 TLS TCP 使用相同本地端口，必须是固定非零端口，不能使用 PROXY protocol；改变 UDP 监听集合需要重启。Helm 为 HTTPS 的公开端口同时暴露 UDP。TLS 响应自动提供 `Alt-Svc: h3=":公开端口"; ma=60`，公开端口从合法请求 authority/Host 推导，省略时为 443；已配置的 Alt-Svc 保留，可用于端口映射不同的部署。前置负载均衡必须转发 UDP。

HTTP/3 使用 TLS 1.3、流式上传/响应、头与 trailers、RGL/body 检查、认证、快照和预算。首部限制 64 KiB，最多 128 双向流，QUIC 接收窗口每流 1 MiB/每连接 8 MiB；新连接占用全局连接预算，等待首部的流另受请求预算限制。QUIC 空闲期限为 75 秒；首部解析和 body/send 使用配置期限。为共享监听的 mTLS 主机请求可选客户端证书，握手使用 CA 联集，每个请求再按匹配主机当前 CA 和 required/optional 策略验证。

HTTP/3 宣告 Extended CONNECT，支持 [RFC 9220 WebSocket](https://www.rfc-editor.org/rfc/rfc9220.html) 和受限 TCP CONNECT。隧道通过 64 KiB 有界桥接缓冲流式转发，保留双向半关闭；取消、超时和失败重置对应 QUIC stream，不影响其他请求。CONNECT 不允许 Content-Length；未知扩展协议返回 501。RGL、认证及并发许可继续覆盖整个隧道生命周期。原始 TCP CONNECT 不自动添加 Alt-Svc，避免把目标上游端口误宣告为入口 QUIC 端口。

QUIC TLS 1.3 会话恢复使用每个监听器、每个安全代次独立的 512 条内存会话缓存。路由/插件变化可保留票据；证书、CA、SNI 归属或客户端认证策略变化使旧票据失效。恢复握手仍检查证书有效期和撤销状态；mTLS 恢复保留原客户端身份，每个请求按当前主机策略重新验证。0-RTT 保持关闭，避免重复执行有副作用的请求。

仍不宣告 WebTransport，也没有 HTTP/3 上游或主动迁移策略配置。TCP 多 SNI/mTLS 会话恢复仍受上面的安全限制。跨物理主机、原生 amd64、浏览器 HTTP/3 和长时间压力资格尚未完成；这些边界不能用协议冒烟测试替代。新增协议验证见[补充记录](validation-hyper-protocols-2026-10-01.md)。

## 验证与比较

行为脚本覆盖两种运行模式的共同契约，包括坏插件发布保留上一有效快照：

```sh
python3 scripts/hyper_integration.py /path/to/rgnix
python3 scripts/hyper_integration.py /path/to/rgnix --pingora
python3 scripts/integration.py /path/to/rgnix
```

使用 `scripts/benchmark_compare.py --plain-proxy --cases proxy-1k`，通过 `--rgnix-transport` 与 `--candidate-transport` 选择两种模式；两条程序路径可以指向同一个构建。A/A 时两种标签必须使用相同模式，分别校准 Pingora 和 Hyper。使用相同配置、CPU/并发、窗口长度和共同上游，门槛通过后才解释 A/B；不能用最小代理的结果代替产品路径。

[本轮实际验证记录](validation-hyper-product-2026-09-28.md)与[独立原型的历史结果](validation-hyper-prototype-2026-09-28.md)分开记录。功能接入与验收见[内核补齐验证记录](validation-hyper-kernel-2026-09-28.md)。此前性能记录对应当时的 HTTP/1 子集，不能直接代表补齐功能后的版本；当前没有新的 NGINX 性能对等或生产容量结论。

后续 [CPU 剖析与上下文搬移优化](validation-hyper-profile-2026-09-28.md)将请求状态放在固定堆位置，减少它在异步状态和完成队列中的按值复制。捕获的 libc 拷贝字节下降约 58.6%，代价是每请求增加一次分配；该操作计数不等同于吞吐提升。

后续 [请求状态、客户端与计时器优化](validation-hyper-prepared-2026-09-28.md)进一步缩小共享请求上下文，并将固定配置处理移到发布阶段。实验性客户端使用隔离的 `vendor/hyper-util` 路径依赖；reqwest/kube 仍使用注册表版本。读取成功只清除计时状态，实际上传进展才延长上游读取期限，空 flush 不会延长期限。镜像和 trace 状态仅在使用时分配；开启这些功能时各增加一次小对象分配。

后续 [连接复用路径优化](validation-hyper-pool-2026-09-28.md)共享不可变客户端状态，先取空闲连接，仅在未命中时创建连接竞争状态。持久连接 fixture 中捕获的复制字节再减少约 15.5%，每请求少约一次分配；上游每次关闭连接时则多约一次分配。原有连接过期、取消和禁用重试规则保留，吞吐资格单独记录。

后续 [NGINX 实现对照优化](validation-nginx-aligned-2026-09-28.md)在发布时准备原始请求头的保留列表。固定配置只保留限流/hash、日志和指标实际需要的头；包含变量的配置保留完整原始视图。转发请求头、重复值和变量读取修改前值的语义保持不变。

实验依赖现在同时隔离 `vendor/hyper` 与 `vendor/hyper-util`，不修改 reqwest/kube 的注册表副本。上游 Content-Length 为 1～8192 字节、且 body 已全部在现有读缓冲区时，复用原 decoder 完成 framing 后直接交付 `Bytes`，省去 body 通道。不会为了命中而等待或额外读取；未完整响应、chunked、未知长度、trailer 和升级继续原路径。请求 dispatch 和响应回调通道仍存在。

独立测量中，1 KiB 代理分配从约 42.3 降至 30.5 次/请求；这不是吞吐提升百分比。当时固定 worker/独立池试验未通过校准，已撤下，试验补丁和全部窗口保存在验证记录中。当前 worker 归属方案见下方的新验证记录。

后续[精简执行路径验证](validation-hyper-plain-2026-09-30.md)在发布时准备适用的 HTTP/1.1 固定响应和不带 URI 的普通代理计划，减少完整上下文和逐请求字符串构造。访问日志、trace、认证、限流、租户、RGL、Gateway、灰度、压缩和动态变量仍走完整路径；已配置的功能不会被跳过。保留方案的 1 KiB 代理分配降至约 17.6 次/请求，但吞吐对照未通过完整校准。前台借用上游连接驱动也做了独立试验，修复了负载下的唤醒遗漏后仍没有保留收益依据，已撤下并归档；该阶段没有消除请求通道，也没有切换 worker 模型。

后续[深度剖析与连接期限修复](validation-hyper-deep-2026-10-01.md)缩小上传状态，禁止重放时使用不携带待恢复请求的响应回调，自动 Date 仅在编码时检查。同时修复 HTTP/1 空闲探测错误运行响应读取期限的问题；活动请求的超时和取消规则保留。捕获的复制字节、分配和正式吞吐分别记录，不能将复制下降比例当成吞吐收益。该阶段验证时默认数据面仍为 Pingora；当前开发版本已切为 Hyper。

后续[worker 归属与 NGINX 对照](validation-hyper-workers-2026-10-01.md)保留独立 reactor 和显式 HTTP/1 独立池，HTTP/2 / Auto 继续共享复用客户端。最终 327 项运行检查通过，独立计数中的 futex 调用和上下文切换明显减少。双 worker 的 1 KiB 代理观测吞吐提高约 33% / 23%，但旧版、候选和 NGINX 的校准均失败，候选也有尾延迟尖峰；该数字不是已确认的稳定提升或生产容量。该轮的 mTLS CA 轮换偶发 reset 在旧版也复现；后续已定位为验收客户端的 OpenSSL 线程错误残留，见本轮补齐验证记录。

后续[空闲接收探测与请求 future 试验](validation-hyper-dispatch-2026-10-01.md)保留一字节空闲探测，减少响应仍持有缓冲区时的提前分配；活动读取及异常数据拒绝规则保留。1 KiB 代理独立计数中累计申请字节减少约 44%，复制开销基本不变。去装箱请求 API 增加了复制且没有一致吞吐收益，已撤下。保留方案的 271 项产品运行检查通过；最终 36 窗、65,322,867 请求零错误，但所有组未通过完整校准，观测吞吐变化 -1.36% / +3.92% 不能作为稳定提升。独立 owned 原型的截断响应处理已修复，未接入产品。

后续[固定后端计划与指标句柄缓存](validation-hyper-forward-2026-10-01.md)将固定后端直接放入普通代理计划，并缓存后端指标句柄。500 次完整响应校验中的已生成指标查找调用由每请求 4 次降至 0 次；分配和复制基本持平，273 项运行检查通过。单 worker 产品对照完成 36 窗、34,367,831 请求零错误，但所有组校准失败，吞吐中位数变化 -1.97% / -0.13%，尚未确认收益。双 worker 连接绑定原型另外完成 36 窗，其校准也全部失败，未接入产品。这两种运行条件不同，不能跨矩阵比较绝对吞吐；PGO 和 worker 内指标聚合尚未实施。

后续[请求准备成本与 NGINX 再对照](validation-hyper-closegap-2026-10-01.md)共享监听器对象、借用已小写域名并避免为已知逐跳字段分配名字。分配由每请求约 16.05 次降到 14.05 次，捕获的复制字节减少约 7.4%；372 项运行检查通过。正式 36 窗和针对尾延迟的 18 窗均零错误，但产品同版本校准未通过，同步 CPU 诊断也未确认收益，仍未建立接近 NGINX 的稳定性能结论。PGO [构建流程](pgo.md)已跑通，本次训练未取得明确收益，没有用于保留候选或默认构建；访问日志、预算及其他已配置功能没有被跳过。

本轮进一步补齐协议和传输边界，见[2026-10-01 验证记录](validation-hyper-gaps-2026-10-01.md)。上游 Gateway conformance、跨物理主机、长时间 soak 和稳定性能仍需各自证据；该阶段验证时默认数据面仍为 Pingora；当前开发版本已切为 Hyper。

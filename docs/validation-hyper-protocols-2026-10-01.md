# Hyper 协议补充验证：2026-10-01

本轮补齐 H2 未完成首部块的总期限，以及 H3 CONNECT、WebSocket、中间响应和 TLS 会话恢复。默认数据面仍是 Pingora。本记录没有新的性能测试结果，也不声称达到 NGINX 的吞吐或生产容量。

[机器可读验证结果与协议检查列表](validation/hyper-protocols-2026-10-01.json)。

## 构建与环境

- 分支：`experiment/replace-pingora`；在已有未提交工作上继续修改，没有发布版本。
- OrbStack Ubuntu Linux ARM64，Rust 1.90.0，内核 `7.0.14-orbstack-00380-ga7e0a2dc9535`。
- 构建：`cargo build --locked --features http3,jemalloc`，dev profile，非 release 性能构建。
- 固定二进制：`/var/tmp/rgnix-protocol-gaps-final-20261001`。
- SHA-256：`2665edd0d6cd81fcd4bfa70950355c9d632b1de294ff5533b5c2329c37c159b1`。
- 本地原始记录：`.local/hyper-next-gaps-20261001/`。`source-manifest.json` 记录 294 个 Rust/Cargo 文件，清单 SHA-256 为 `1d1f5721d75a8c8802d521953e8990df20d1eb9d959c19e22bc09563d7e313a2`。

## 实现与实际行为

H2 在传输层观察前言和 frame 边界，保留 Hyper/h2 的协议与 HPACK 校验。HEADERS 到最后一个 CONTINUATION 的总期限不随字节到达而延长；其他活跃 stream 也不能关闭该期限。首部块超时关闭连接，因为未完成 CONTINUATION 会阻塞所有流。完整缓冲内的 frame 不分配计时器。真实 socket 同时验证正常分片、逐字节慢速分片以及 h2c Upgrade 后的同一边界。

H3 宣告 Extended CONNECT，执行受限 TCP CONNECT 和 RFC 9220 WebSocket。使用现有路由、认证、RGL、端点选择、超时及完整生命周期并发许可，桥接缓冲固定为 64 KiB。到 H1、显式 H2 和自动协商上游的 WebSocket 均有实际转发检查。1 MiB 隧道数据超过桥接缓冲仍完整返回；上游半关闭后客户端仍可上传。超时/取消通过 QUIC stream reset 表达，避免把失败转换为正常 FIN；其他请求保持可用。未知有效协议 token 进入路由层返回 501，CONNECT 的 Content-Length 返回 400；原始 CONNECT 不自动添加 Alt-Svc，防止错误宣告目标上游端口。

H3 也转发上游 103/102，先发送中间响应再发送最终响应，不执行中间响应的最终钩子。复用四条/每条 64 KiB 的有界提示队列及路由发送期限。

QUIC TLS 恢复使用每监听器、每发布版本独立的 512 条会话缓存，签发两张票据，禁止 0-RTT。测试使用一张票据验证恢复，使用另一张**未消费**票据验证发布后失效，避免把单次票据的消费误认为配置隔离。mTLS 恢复保留原身份；共享 SNI 的匿名票据不能访问受保护主机，后来添加客户端证书也不能提升该票据的身份。新认证握手可以访问受保护主机，CA 更新后既有恢复连接被拒绝。

`vendor/h3` 来自 crates.io 0.0.8，保留 MIT 许可证；补丁仅扩展 protocol token 的解析/保存，并将相应复制改为克隆。来源和升级检查见 [vendor 文档](../vendor/README.md)。

## 验证结果

| 验证 | 实际结果与范围 |
| --- | --- |
| `scripts/hyper_protocols.py --http3` | **PASS，69 项**，真实 TCP/TLS/H2/QUIC 连接；原始记录 `protocols-final-complete.log` |
| `scripts/hyper_integration.py` | **PASS，57 项**，代理、流式 body、上游断连/超时、trailers、预算及终止；`hyper-complete.log` |
| `scripts/check.sh`，`http3,jemalloc` + Hyper 模式 | **PASS**，包含 101 项核心集成、Ingress API 恢复、32 项 OTLP、20 项文件轮转、115 项产品行为、10 项迁移及 12 项共享限流；`shared-checks-accepted.log` |
| `cargo test --features http3,jemalloc` | **PASS，25 项，1 项原有 ignored** |
| `cargo clippy --features http3,jemalloc --all-targets -- -D warnings` | **PASS**；h3 0.0.8 依赖自身仍打印 9 条上游编译警告 |
| `cargo check --features hyper-experimental` | **PASS**，不启用 HTTP/3 的 Hyper 构建；这是编译验证 |
| 默认 Pingora 的 `scripts/check.sh` | **PASS**，默认编译与运行模式完整回归；`default-checks.log` |
| Hyper 有界中间响应队列单元回归 | **PASS，1 项**，包含容量、旧 sender 隔离和关闭唤醒；`hints-unit.log` |
| 固定 NGINX 1.28.0 行为对照 | **PASS，49 项**；`nginx-parity-complete.log`，非吞吐测试 |

完整集成检查首次在顺序隔离/限流断言失败。上一轮固定二进制也复现了四次 502 的窗口：客户端已收到响应时，服务端尚未完成 flush 和结算。测试现在等待已有 in-flight 指标达到预期后再判断下一次顺序请求，仍会在许可泄漏或未完成结算时失败。没有用重试掩盖代理响应或修改被动隔离策略。

协议客户端采用 aioquic 1.3.0、h2 4.3.0。aioquic 的测试适配保留 GREASE 后 FIN 的处理，并为 1xx 响应恢复“等待最终首部”的解析状态；其原实现把第二段响应首部当作 trailers。此适配只在测试中使用，产品没有关闭 GREASE，也没有跳过协议字段校验；这不是原生浏览器 HTTP/3 兼容认证。

## 剩余边界

WebTransport、HTTP/3 上游和主动迁移策略配置仍未实现；TCP 多 SNI/mTLS 会话恢复仍关闭。0-RTT 有意保持禁用。此次没有重跑 Kubernetes 集群部署、跨物理主机、原生 amd64、浏览器 H3、长期压力或官方 Gateway conformance。此前的集群记录对应其原二进制，不能移用于本轮新构建。

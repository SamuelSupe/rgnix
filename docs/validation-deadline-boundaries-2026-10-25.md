# 超时错误响应、HTTP/3 提示与隧道取消验证（2026-10-25）

本轮以 `experiment/replace-pingora`、HEAD `1bd974f` 加全部现有未提交改动为基线，修复复审确认的三个问题。结构化证据见 [JSON 记录](validation/deadline-boundaries-2026-10-25.json)，冻结程序、基线摘要、失败对照和执行日志保存在 `.local/deadline-boundaries-20261025/`。

## 实现与契约

- 操作总期限已过期时，生成的错误响应另有一次 `send_timeout` 交付预算。该绝对时刻由响应守卫保存，响应体、HTTP/2 流控、TCP/TLS 最终 flush 和 HTTP/3 发送共用它，不因写进展续期。不重启上游工作；可写连接正常交付完整 504，HTTP/2 兄弟请求继续完成。错误响应自身发送受阻仍会取消并归还额度。
- HTTP/3 102/103 提示携带原 request/backendRequest 较早期限；等待发送时取它与独立发送超时的较早值。部分提示首部到期时重置对应流；已完成提示后仍允许正常发送最终 504。
- HTTP/3 CONNECT/WebSocket 桥接复用现有完成通道感知守卫取消。取消会中断接受响应、DATA 或结束操作的等待，丢弃未交付的桥接缓冲并重置对应 QUIC 流；其他流不受影响。正常完成仍等待桥接交付结果。

没有新增依赖、生产配置项或后台任务。产品变动集中在 Hyper 的请求、响应守卫与 HTTP/3 适配器三个源码文件。

## 针对性回归

扩展现有 `scripts/deadline_regressions.py`，继续使用公开运行时夹具和原有 h2/aioquic 依赖；既有协议 CI 已调用该脚本，无须新增工作流。最终套件共 **164 项检查**，包含此前的 41 项交付回归，覆盖 request=150ms、backendRequest=150ms、request=1s/backendRequest=150ms、两者显式关闭四种配置：

- HTTP/1 与 HTTP/2 的 TCP/TLS GET、HEAD：上游首部延迟 400ms，有限期限返回完整 504，关闭期限返回 200；HTTP/2 上较晚发起、尚未超时的兄弟请求成功，同连接后续请求仍可用。
- 阻塞在共享 TCP 输出后的 504：650ms 检查点仍保留待交付额度；随后在独立错误交付预算内取消并释放，客户端恢复读取不能收到成功结束的过期响应。
- HTTP/3 普通 GET、HEAD，以及先发送完整 103 再延迟最终首部：有限期限仍获得完整 504，兄弟流成功。
- HTTP/3 较大 103、原始 CONNECT 和 Extended CONNECT WebSocket：固定每流接收额度 1024 字节，期限到达后只重置受阻流，额度归零；650ms 后恢复流控，没有新增响应数据，也没有成功 FIN。同连接后续请求成功。
- 关闭绝对期限时，提示和隧道保留待交付额度；恢复流控后完整交付，CONNECT/WebSocket 传输超过 16 MiB。另起受阻流验证独立的 2 秒发送超时仍会重置流并归还额度。

最终运行中，150ms 策略的提示/隧道重置约为 **151.6–156.3ms**。原始 CONNECT 在恢复流控前后的已收字节数均为 990 或 993，WebSocket 均为 949，确认到期后没有追加缓冲数据。关闭总期限后两类隧道均完成至少 16 MiB 交付。

修复前冻结程序分别运行错误响应、提示、CONNECT 的定向回归，均在对应缺陷处失败：504 路径发生连接重置；提示和 CONNECT 在绝对期限内没有收到要求的重置。后两项过滤其他断言，以独立抵达各自的失败检查，原始日志和过滤脚本一并保留。

首次测试补充了测试客户端的兼容处理：复用 `hyper_protocols.py` 中已有的 aioquic 1.3 多段响应首部适配器；HTTP/3 HEAD 延迟测试使用空资源，避免 aioquic 未建模 HEAD 的非零 Content-Length 行为。HTTP/1/2 HEAD 仍使用非零资源长度。上述首次失败记录保留，以下为最终验证结果。

## OrbStack 验证

Ubuntu arm64，Rust 1.90.0，锁定依赖，开发构建 opt-level=1，运行特性 `http3,jemalloc`。

| 检查 | 结果 |
|---|---|
| 扩展交付回归 | 164 项通过 |
| 修复前失败对照 | 三个对应缺陷均被捕获 |
| Rust 测试 | 38 通过，1 项既有性能测试忽略 |
| 完整 `scripts/check.sh` | 通过：HTTP、Ingress 恢复、OTLP、日志轮转、产品能力与迁移 |
| 既有 HTTP/2 流控期限回归 | 40 次通过，含零/部分窗口、兄弟流隔离和禁用期限 |
| Hyper 专项集成 | 84 项通过 |
| 扩展协议，含 HTTP/3 | 76 项通过 |
| 显式 Pingora HTTP 集成 | 107 项通过 |
| Clippy，全部 targets，`-D warnings` | 通过 |
| 默认特性编译，不启用 HTTP/3 | 通过 |
| 格式、Python 语法、diff 空白检查 | 通过；这些为静态检查 |

复跑命令，在 OrbStack Ubuntu 的仓库根目录执行：

```sh
export CARGO_TARGET_DIR=/home/livesite/.cache/rgnix-opt-target
export CARGO_PROFILE_DEV_OPT_LEVEL=1
RGNIX_CARGO_FEATURES=http3,jemalloc bash scripts/check.sh
cargo build --locked --features http3,jemalloc --example gateway_deadlines -j 2
/home/livesite/.cache/rgnix-http3-venv/bin/python scripts/deadline_regressions.py "$CARGO_TARGET_DIR/debug/examples/gateway_deadlines" --http3
/home/livesite/.cache/rgnix-http3-venv/bin/python scripts/hyper_protocols.py "$CARGO_TARGET_DIR/debug/rgnix" --http3
cargo clippy --locked --features http3,jemalloc --all-targets -j 2 -- -D warnings
cargo check --locked -j 2
```

## 保留检查与验证边界

1080 个基线文件中仅修改 9 个相关文件，其余 1071 个摘要一致；新增本验证 Markdown/JSON 两个文件。既有删除项 `scripts/global_rate_limit.py`、`src/traffic/global.rs` 保留，未暂存、提交、重置或 stash。

未重跑完整 Kubernetes watch/多副本集群、amd64、发行构建、长时间负载或 XDP；没有生产部署、远端 CI 或性能收益判断。现有 h3 依赖的 9 条警告保持不变。

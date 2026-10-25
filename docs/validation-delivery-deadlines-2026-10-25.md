# 响应最终交付期限修复与验证（2026-10-25）

本轮修复 HTTP/2 最终 flush 和 HTTP/3 发送等待期间的 Gateway 总期限遗漏。基线为 `experiment/replace-pingora`、HEAD `1bd974f` 加任务开始时的全部未提交改动。结构化结果见 [JSON 记录](validation/delivery-deadlines-2026-10-25.json)，冻结执行文件、基线摘要、失败对照及原始日志位于本地 `.local/delivery-deadlines-20261025/`。

## 修复行为

- 完成读取的响应仍保留原 request/backendRequest 的较早期限，直到 TCP/TLS 最终 flush 成功。连接上的待交付队列只保留最早计时器；正常 flush 清除它。未设置总期限时不创建该计时器。
- 若最终 flush 被阻塞，期限到达后记录交付超时、归还请求和后端额度，并中止共享 TCP 连接。关闭时使用零 linger，丢弃尚在 socket 中排队的字节，避免客户端恢复读取后收到成功结束的过期响应。该阶段数据已序列化，无法只撤回其中某一流；同连接其他请求也会随连接取消。普通 HTTP/2 流控等待仍只重置超时流，已重置的响应直接释放 guard，不进入最终 flush 队列。
- 普通 HTTP/3 响应与被拒绝的 CONNECT 响应共用交付函数。最终响应头、DATA、trailers 和结束操作整体受原绝对期限约束，QUIC 发送容量不足也不会延长期限。超时发送 `H3_REQUEST_CANCELLED`，释放额度；其他流和后续请求继续使用同一连接。既有逐次发送超时保留，显式 `0s` 只关闭绝对期限。

## 针对性回归

新增 `examples/gateway_deadlines.rs` 夹具通过公开 API 为编译后的文件路由附加 Gateway 策略，并运行正式 `runtime::serve`。它使 socket 回归不依赖 Kubernetes 控制面，未修改产品 CLI 或增加测试专用的产品配置项。

`scripts/deadline_regressions.py` 已接入现有协议 CI，使用现有 h2/aioquic 依赖和测试工具：

- request=150ms、backendRequest=150ms、request=1s/backendRequest=150ms 和两项显式关闭，四种配置。
- HTTP/2 TCP 与 TLS：小接收缓冲、足量 HTTP/2 流控额度和 16 MiB 大流先阻塞底层写入，再发送 32 字节小流。确认该小流确实到达源站；在客户端仍未读取时检查请求/后端额度归零，随后恢复读取，要求连接终止且小流不能成功结束。关闭期限的控制用例则必须恢复正常交付。每次再用新连接确认监听器可用。
- HTTP/3：将流窗口固定在 1024 字节并持续处理 QUIC 事件，确认已有部分响应、空响应兄弟流成功；总期限到达后只重置受阻流，额度归零，同连接后续请求成功。关闭总期限后恢复额度，要求完整交付 16 MiB；再阻塞新流，确认独立的 2 秒写超时仍会重置流并释放额度。

最终套件共 41 项断言。150ms 策略下 QUIC 重置约在 152–155ms；TCP/TLS 额度在仍未恢复读取的检查点均为 0，恢复读取后过期小流不能以成功 END_STREAM 结束。显式关闭期限时两种传输均恢复交付。

冻结修复前执行文件运行同一测试：HTTP/2 在“最终 flush 到期释放额度”处失败；独立运行 HTTP/3 用例在“发送等待服从总期限并释放额度”处失败。首次正向执行修正了夹具的 Clap 分组命名冲突，以及 aioquic 1.3 在 GREASE 后 FIN 缺少 H3 结束事件的已知测试客户端兼容处理；失败日志保留。下表为最终源码的通过结果。

## OrbStack 执行结果

环境：Ubuntu arm64、Rust 1.90.0，锁定依赖，开发构建 opt-level=1，`http3,jemalloc`。

| 检查 | 结果 |
|---|---|
| 新增响应交付回归 | 连续两轮各 41 项通过，覆盖 TCP、TLS、QUIC、较早期限、禁用与独立写超时 |
| Rust 测试 | 38 通过，1 项既有性能测试忽略 |
| `scripts/check.sh` | 完整通过：HTTP 107、Ingress 恢复 57、OTLP 32、日志轮转 20、产品能力 119、迁移 10 项 |
| 既有 HTTP/2 流控期限回归 | 40 次通过，保留零窗口/部分窗口、逐流隔离与禁用行为 |
| 既有 Gateway 主机冲突及 WebSocket 回归 | 四种期限模式各三次通过，退出后额度归零 |
| Hyper 专项集成 | 84 项通过 |
| 扩展协议，含 HTTP/3 | 76 项通过，包含 CONNECT、mTLS 与恢复路径 |
| 显式 Pingora HTTP 集成 | 107 项通过 |
| clippy，全部 targets，`-D warnings` | 通过 |
| 格式、Python 语法、diff 空白检查 | 通过；这些为静态检查 |

在 OrbStack Ubuntu 的仓库根目录可运行：

```sh
export CARGO_TARGET_DIR=/home/livesite/.cache/rgnix-opt-target
export CARGO_PROFILE_DEV_OPT_LEVEL=1
RGNIX_CARGO_FEATURES=http3,jemalloc bash scripts/check.sh
cargo build --locked --features http3,jemalloc --example gateway_deadlines -j 2
/home/livesite/.cache/rgnix-http3-venv/bin/python scripts/deadline_regressions.py "$CARGO_TARGET_DIR/debug/examples/gateway_deadlines" --http3
cargo clippy --locked --features http3,jemalloc --all-targets -j 2 -- -D warnings
```

## 验证边界与保留检查

未重跑完整 Kubernetes watch/多副本集群、amd64、发行构建、长时间负载或 XDP 验收；没有进行生产部署或性能收益判断。CI 配置已添加回归命令，本轮没有触发远端 CI。vendor 源码未改，h3 依赖原有 9 条警告仍存在。

基线 1076 个非忽略文件中，仅编辑 10 个相关文件，其余 1066 个内容一致。新增专用夹具、回归脚本及本 Markdown/JSON 记录，共 4 个文件；既有删除项保留。Cargo 与 CI 文件也验证了移除本轮新增条目后摘要与基线一致。未暂存、提交、重置或 stash。

# 复审遗漏修复与验证（2026-10-25）

本轮修复复审发现的两项遗漏，以任务开始时的完整工作区为基线，保留既有未提交改动及删除项。分支为 `experiment/replace-pingora`，HEAD `1bd974f`；本记录描述本地源码和 OrbStack 执行结果，不代表发布或生产验收。

结构化结果见 [JSON 记录](validation/review-followups-2026-10-25.json)。冻结执行文件、基线摘要、隔离夹具和原始日志位于本地 `.local/rereview-fixes-20261025/`。

## 修复边界

- HTTP/2 响应发送任务独立检查 Gateway request/backendRequest 的较早绝对期限。等待接收窗口时不再依赖下一次 body 读取；窗口只有部分容量时，仅提交可发送部分，将剩余数据保留在计时器控制的任务内。超时发送该流的 `RST_STREAM(CANCEL)`，归还请求与后端额度，不影响同连接其他流。逐流超时标记使已经读完的小响应也记录为超时，避免将重置误记为交付成功。原有 DATA 写入 idle timeout 的计时阶段保留。
- `/v1/simulate` 复用现有有界 JSON 读取函数，保持并发上限 2、请求体上限 1 MiB、reader/writer 角色及 namespace 范围检查。读取总期限为 5 秒，连续慢传也返回 408 并归还名额；格式错误返回 400，超大或不可用请求体返回 413。

## 针对性回归

`scripts/gateway_e2e.py` 扩展正式源站与标准库 HTTP/2 探针，覆盖 32 字节、1 MiB 响应，初始窗口 0、1 字节及 65535 字节后耗尽。每个请求并发发送另一条空响应流，检查其正常完成。request=150ms、backendRequest=150ms、request=1s/backendRequest=150ms 及显式禁用期限四种模式，共 20 组；隔离运行每组两次，共 40 次全部通过。超时流约在 **151–156ms** 重置，其他流完成；恢复接收窗口后，禁用期限的响应完整交付。

隔离夹具使用正式 `runtime::serve` 和合成 Gateway 策略，执行同一源站与 HTTP/2 探针。它还在客户端连接仍打开时读取 Prometheus，确认请求/后端占用均归零，并核对小响应和大响应的超时完成日志。修复前冻结执行文件运行同一零窗口用例，600ms 后仍无重置，回归用例失败。

`scripts/product_features.py` 在现有具名身份测试中，用 namespace reader 的两个请求占满模拟名额，每 250ms 继续上传少量数据。检查第三个请求返回 429、两个慢传请求返回 408，以及客户端尚未主动关闭时额度归零、全局 reader 的正常模拟恢复为 200；同时覆盖无效 JSON 400 和超过 1 MiB 的 413。修复前执行文件在读取响应时超时，修复后通过。原有越权模拟 403 检查继续通过。

## OrbStack 执行

环境为 Ubuntu arm64、Rust 1.90.0，开发构建 opt-level=1，产品功能配置 `http3,jemalloc`，锁定依赖。

| 检查 | 结果 |
|---|---|
| Rust 测试 | 38 通过，1 项既有性能测试忽略 |
| `scripts/check.sh` | 完整通过：HTTP 107、Ingress 恢复 57、OTLP 32、日志轮转 20、产品能力 119、迁移 10 项 |
| vendor Hyper 测试，含 rgnix 扩展 | 124 通过，6 项既有测试忽略 |
| HTTP/2 总期限定向检查 | 40 次通过，含额度释放、逐流隔离和禁用期限 |
| 既有 Gateway 主机冲突及 WebSocket 隔离回归 | 四模式各三次通过 |
| Hyper 专项集成 | 84 项通过 |
| HTTP/2/3 等扩展协议 | 76 项通过 |
| 显式 Pingora HTTP 集成 | 107 项通过 |
| clippy，全部 targets，`-D warnings` | 通过 |
| 格式、Python 与嵌入探针语法、diff 空白检查 | 通过；这些为静态检查 |

主体命令，在 OrbStack Ubuntu 的仓库根目录执行：

```sh
export CARGO_TARGET_DIR=/home/livesite/.cache/rgnix-opt-target
export CARGO_PROFILE_DEV_OPT_LEVEL=1
RGNIX_CARGO_FEATURES=http3,jemalloc bash scripts/check.sh
cargo clippy --locked --features http3,jemalloc --all-targets -j 2 -- -D warnings
python3 scripts/hyper_integration.py "$CARGO_TARGET_DIR/debug/rgnix"
/home/livesite/.cache/rgnix-http3-venv/bin/python scripts/hyper_protocols.py "$CARGO_TARGET_DIR/debug/rgnix" --http3
RGNIX_ENGINE=pingora python3 scripts/integration.py "$CARGO_TARGET_DIR/debug/rgnix"
```

额外尝试 vendor Hyper 单独关闭 `rgnix-extensions` 的 client/server/http1/http2 测试组合，仍因已有 `send_informational` 类型缺少 feature 条件而编译失败。任务开始时的 vendor 源码副本也以相同两个错误失败，确认不是本轮引入；产品 Cargo 始终启用该扩展。本轮保留这一不受产品构建使用的既有组合问题。h3 依赖的 9 条既有警告仍存在。

## 验证范围与改动保留

未重跑完整 Kubernetes watch/多副本集群套件、amd64、发行构建、长时间负载或 XDP 验收；Gateway 新增探针通过真实 socket 隔离数据面执行。没有进行生产部署或性能收益判断。

任务开始时记录 1074 个非忽略文件摘要；本轮只修改其中 13 个相关文件，其余 1061 个内容一致，没有新增缺失文件。新增本 Markdown 和 JSON 验证记录。既有 `scripts/global_rate_limit.py` 与 `src/traffic/global.rs` 删除状态保留，未暂存、提交、重置或 stash。

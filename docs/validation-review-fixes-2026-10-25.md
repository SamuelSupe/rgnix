# 源码审查问题修复与验证（2026-10-25）

本轮在 `experiment/replace-pingora` 的现有工作区上修复三个问题，保留已有未提交改动及删除项。源码基线为 HEAD `1bd974f` 加任务开始时的工作区内容，不能将此结果视为已发布版本的验收。

结构化结果、执行文件和日志摘要、保留检查及逐轮局部测量见 [JSON 记录](validation/review-fixes-2026-10-25.json)。原始日志和隔离夹具保存在本地 `.local/review-fixes-20261025/`。

## 修复与回归边界

- 公共请求主机检查在路由前拒绝绝对 URI / HTTP/2 authority 与 Host 的主机名、端口冲突。保持缺少 Host 的 HTTP/2/3、IPv6、ASCII 大小写和原始 CONNECT 的既有边界。Rust 回归及真实 HTTP/1/2 socket 检查覆盖冲突拒绝、正常转发和同连接其他流继续成功。HTTP/3 的冲突已由其协议库以 `H3_MESSAGE_ERROR` 拒绝，本轮增加实际流重置和同连接后续请求的检查，没有修改该协议库。
- 普通响应与升级隧道共用同一绝对期限计算。隧道使用原请求和原后端开始时间的较早期限，覆盖升级等待、双向传输及交付等待；取消时释放连接、请求和后端额度。`gateway_e2e.py` 增加持续 ping/pong 的 WebSocket 检查，覆盖 request、backendRequest、较早 backendRequest 和显式禁用期限。
- 灰度窗口增量维护错误数与超阈值数，删除每请求扫描和排序。最近最多 10000 条样本、毫秒精度、nearest-rank p95 严格超标和错误比例达到阈值的语义保留。回归验证 5% 分界、非整分位窗口、fallback/旧阶段排除、容量淘汰、无新请求时过期，以及阶段切换清空计数。

旧普通 HTTP/1 成功夹具包含 Host 省略目标端口的请求，现改为匹配的 authority，并单独验证原冲突返回 400。HTTP/3 夹具补充识别普通请求的流重置，避免把已被协议层拒绝的请求误判为等待 HTTP 响应超时。首次失败日志保留在本地目录；下表为调整后完整通过的执行。

## 本轮实际执行

环境：OrbStack Ubuntu arm64、Rust 1.90.0，locked 开发构建，`http3,jemalloc`，`CARGO_PROFILE_DEV_OPT_LEVEL=1`。

| 检查 | 结果 |
|---|---|
| `cargo fmt --check`、Python 语法编译、`git diff --check` | 通过 |
| Rust 测试 | 38 通过，1 项路由性能测试按原定义忽略 |
| `cargo clippy --locked --features http3,jemalloc --all-targets -j 2 -- -D warnings` | 通过 |
| `scripts/check.sh` | 完整通过，包含 HTTP、模拟 Kubernetes API 恢复、OTLP、日志轮转、产品能力及迁移 |
| HTTP 集成 | Hyper 107 项；同一二进制显式选择 Pingora 后 107 项 |
| Hyper 专项 | 84 项 |
| 扩展协议，含 HTTP/3 | 76 项 |
| Gateway 数据面隔离夹具 | 四种期限模式各重复三次；冲突 400、受保护主机 403、匹配请求 200；隧道退出后请求/后端额度归零 |

隔离夹具使用正式 `runtime::serve` 和合成 Gateway 路由策略，实际运行 `gateway_e2e.py` 中的源站与 WebSocket 探针。150ms 总期限下，持续收发约 157–162ms 后关闭；request=1s / backendRequest=150ms 同样按较早期限关闭。显式禁用总期限时，三个 600ms 观察窗口均持续收发成功。后端并发上限为 1，重复连接和后续普通请求成功，Prometheus 请求/后端占用恢复为 0。

可重复的主体检查命令（在 OrbStack Ubuntu 中，从仓库根目录执行）：

```sh
export CARGO_TARGET_DIR=/home/livesite/.cache/rgnix-opt-target
export CARGO_PROFILE_DEV_OPT_LEVEL=1
RGNIX_CARGO_FEATURES=http3,jemalloc bash scripts/check.sh
cargo clippy --locked --features http3,jemalloc --all-targets -j 2 -- -D warnings
RGNIX_ENGINE=pingora python3 scripts/integration.py "$CARGO_TARGET_DIR/debug/rgnix"
python3 scripts/hyper_integration.py "$CARGO_TARGET_DIR/debug/rgnix"
/home/livesite/.cache/rgnix-http3-venv/bin/python scripts/hyper_protocols.py "$CARGO_TARGET_DIR/debug/rgnix" --http3
```

## 灰度回调局部成本

冻结修复前、修复后执行文件，使用相同 Rust/profile/功能配置；预填 10000 条样本，计时 1000 次 `State::completed`，三轮交替程序顺序。开启 p95 时，中位耗时从 **138.637ms 降为 0.048ms**；关闭 p95 时为 3.237ms / 0.045ms。原始各轮与执行文件 SHA-256 在 JSON 中。

这是单进程回调微测量，说明移除窗口扫描和排序的局部效果；共享 OrbStack 宿主未取得独占硬件或生产吞吐资格，不能外推网关 RPS、CPU 或 P99 改善。

## 未执行范围与改动保留

本轮没有重跑真实 Kubernetes watch/多副本部署、发行构建、amd64、24 小时负载或 XDP 内核验收。`gateway_e2e.py` 的新增逻辑已通过隔离数据面夹具执行，但完整集群套件未重跑。

任务开始时记录 1071 个非忽略文件摘要；本轮仅编辑其中列明的 13 个文件，其余 1058 个内容保持一致。新增回归测试与本验证记录另行列入当前工作区，未进行暂存、提交、重置或 stash。

# v0.1 缺陷修复与验证记录

本页保留 `qa9` 的历史验收；当前 `qa10` 的检查点隔离和 TLS 归属修复见 [最新验证记录](validation-isolation.md)。

日期：2026-09-23。本轮针对产品审查中实际复现的问题修复，并补充运行保护、更新隔离和验收。首轮 `qa5` 的记录保留在 [历史验证记录](validation.md)，不代表当前产物。

## 修复结果

| 问题 | 修复后的行为 | 回归证据 |
|---|---|---|
| 平坦长表达式可通过 CLI 编译，但 SIGHUP 编译导致控制线程栈溢出、整个进程退出 | 构造 AST 时限制真实深度；限制编译遍历、分支和输入规模；CLI/热更新共用有界编译线程；外部 Wasm 在 JIT 前检查结构预算 | 10,000 项运算链被明确拒绝；热更新后进程存活并继续使用旧配置 |
| server 级 `return 403` 被 location 的动作覆盖 | server return 在 location 插件和请求体限制之前执行；同一作用域第一条 return 生效；location return 不被前后出现的 proxy_pass 覆盖 | 与 NGINX 1.28.0 对照，包含重复 return 和两种指令顺序 |
| 无效插件期间重启副本丢失上一有效版本，坏路由仍显示 Ready | 已接受源码与路由定义写入控制器命名空间检查点，绑定 Ingress/插件 UID；watch 确认持久化后才发布；新副本可恢复已服务版本 | 模拟 API 故障以及真实 Kubernetes 中修改插件和路由后全量滚动重启 |
| `keepalive_timeout 0` 被当作无限 keepalive | 0 关闭复用，非零值按 Pingora 秒级精度向上取整 | 实际 socket 对照 NGINX 的连接关闭行为 |
| 上游连接失败生成的 502 丢失 `add_header ... always` | 代理错误响应走统一响应头处理；失败插件的暂存修改不提交 | NGINX 502 对照及响应钩子失败测试 |
| 重复 server_name 选择了后声明的主机 | 相同监听地址/主机名采用第一个声明，证书选择遵守相同顺序 | NGINX 重复主机名对照 |
| 100/103 临时响应可能提前消耗响应插件 | 只在最终响应头运行响应钩子，包含 WebSocket 101；随后释放插件实例额度 | 上游发送 103 后，最终响应仍获得插件头 |

检查点不可写时，新插件及路由继续等待，原有效版本继续服务。首次同步只有无法发布的插件路由时保持 NotReady；其他已接受路由仍可使入口就绪，坏租户不会撤下整个服务。端点、Service、TLS、Class、Ingress 和插件资源删除按当前状态处理，旧检查点不能复活已撤销的资源。

同时完成以下运行改进：

- 请求路径使用域名/路径索引；Ingress 只对所选资源及其依赖计算语义摘要，跳过无关和纯 status/resourceVersion 更新。相同后端每轮只解析一次，重复坏脚本使用有界失败缓存。
- Event、状态回写和检查点持久化与数据面更新分开执行。模拟 Event 写入阻塞时，端点撤销仍在测试的 2 秒门限内完成。
- 默认最多 1,024 个并发请求和 32 个插件实例，超额返回 503，管理端口独立。流式响应发送最终响应头后释放插件实例，请求仍计入并发额度。
- 上游端点连续 3 次传输失败后暂时摘除 10 秒；成功响应复位，客户端取消不计入，HTTP 5xx 不计入。参数可配置，失败请求没有自动重试。
- 增加当前配置内容 SHA-256、配置诊断、检查点、额度拒绝和端点摘除指标；访问/错误日志带路由、后端和版本信息。Helm 增加检查点 RBAC 和节点软拓扑分散约束。

参数和契约见 [部署文档](deployment.md)、[兼容矩阵](compatibility.md) 和 [RGL 文档](rgl.md)。

## 最终产物与环境

最终功能验收使用从 `rgnix:0.1.0-qa9` 镜像提取的 Linux arm64 release 二进制。`rgnix:0.1.0` 指向同一镜像，未推送远端仓库。

| 项目 | 值 |
|---|---|
| 镜像 manifest list | `sha256:7f406ae614305273ad0b8ce2a6ef2aafeac1c7ac98d89401fbced79a7fa0c52f` |
| 二进制 SHA-256 | `2b62c0dce99eb113af62f9974f8554f33f3fe485bae8019013fd8933b908f32e` |
| 二进制大小 | 28,580,280 字节 |
| Cargo.lock SHA-256 | `4d7576f55284c5dee116b54c25b82c0d0c5d220e9c858a75d75ab19142ea11ec` |
| Rust 源树 SHA-256 | `45d83a885e86738bfd44d386d6b437d7afdd5f72573ef043377794e03ed0a97d` |

[产物摘要](validation/fixes-artifact.json) 的源树算法与首轮相同：排序后的 Cargo.toml、Cargo.lock 和 src 下文件依次输入相对路径、NUL、文件字节。文档与测试脚本不计入该摘要。

运行环境为 OrbStack Ubuntu 25.10/aarch64，Rust 开发检查使用 1.90.0，镜像构建使用 Rust 1.98.1/bookworm/OpenSSL。Kubernetes 为 OrbStack 单节点 `v1.35.6+orb1`，仅操作专用 `rgnix-qa-20260923` 命名空间及其验收资源。NGINX 对照固定为 **1.28.0 (Ubuntu)**。

## 实际验收

| 检查 | 结果 | 证据 |
|---|---|---|
| Rust fmt、clippy `-D warnings`、锁定依赖测试 | 通过，5/5 Rust 测试 | [Linux 检查输出](validation/fixes-linux-checks.txt) |
| HTTP、TLS、流式代理、RGL、资源限额与故障隔离 | **43/43** | [逐项结果](validation/fixes-http.json) |
| NGINX 1.28.0 行为对照 | **22/22** | [逐项结果](validation/fixes-nginx.json) |
| 模拟 Kubernetes API 的持久化/恢复/隔离 | **11/11** | [逐项结果](validation/fixes-ingress-recovery.json) |
| Helm lint、模板渲染、Shell 语法 | 通过 | [检查输出](validation/fixes-static-checks.txt) |
| 真实 Kubernetes 生命周期与升级 | **35/35** | [逐项结果及副本状态](validation/fixes-ingress.json)、[完整运行输出](validation/fixes-kubernetes-qa9.txt) |

HTTP、NGINX 对照和模拟 API 测试使用最终镜像二进制，[完整 release 输出](validation/fixes-release-checks.txt) 可交叉核对。HTTP 测试继续覆盖 HTTPS 上游证书/主机名验证、HTTP/2、SSE、WebSocket、客户端取消、超时、POST 断连不重放、静态文件边界以及新旧请求快照隔离。

模拟 API 测试显式阻塞检查点写入，验证首次配置不提前 Ready、更新不提前生效；重启进程验证已接受源码和路由恢复；阻塞 Event 写入验证端点撤销不被阻塞。它还覆盖无检查点冷启动、租户故障隔离、纯状态更新不重建以及删除清理。

真实 Kubernetes 验收在无效插件并修改路由定义后重启全部入口副本，确认原模块和原路由恢复；继续验证端点、Service、插件 ConfigMap、TLS Secret 删除与恢复、TLS 轮换、Lease 切换、地址回写和清除。滚动升级期间 **300 次集群内 Service 请求全部成功**。最终两副本均为 `qa9`、**2/2 Ready**，配置 SHA-256 相同，检查点健康值均为 1。环境保留供检查。

状态地址 `192.0.2.10` 仅用于回写验收，不是云 LB 或公网可达性证明。测试中 port-forward 使用自动分配端口，不占用其他 OrbStack 服务端口；输出中的 `Terminated: 15` 是脚本正常结束自己创建的 port-forward 进程。

## 补充性能基线

以下数据来自本轮 **qa7** release，而非最终 qa9；[基线产物摘要](validation/fixes-baseline-artifact.json) 独立记录其二进制和源码身份。qa7 已包含索引、额度和被动端点隔离；之后补充了 return 语义、响应模块整理及检查点发布门槛。没有把 qa7 的性能数据冒充 qa9 实测。

两工作线程，关闭访问日志；Python HTTP/1.1 keepalive 客户端，每场景三轮、每轮约三秒。代理场景读取真实 NGINX 上游的 16 KiB 正文，另配置 1,000 条路由；额度设置足以容纳测试并发。所有 18 轮均为 **0 请求错误**。

| 场景 | 并发 | 无插件 RPS / p95 | pass 插件 RPS / p95 | 读头/分支/改头插件 RPS / p95 |
|---|---:|---:|---:|---:|
| 本地 2 字节响应 | 8 | 23,293.7 / 0.853 ms | 23,751.8 / 0.816 ms | 23,558.0 / 0.821 ms |
| 16 KiB 真实代理，额外 1,000 条路由 | 32 | 15,328.2 / 5.049 ms | 15,581.2 / 4.974 ms | 15,535.8 / 4.930 ms |

表中 RPS 和 p95 为三轮中位数。代理场景服务 CPU 约 0.55–0.81 核，最大 RSS 约 43.7 MiB。CPU/RSS 的每轮数据见 [本地响应基线](validation/fixes-benchmark-local.json) 和 [真实代理基线](validation/fixes-benchmark-proxy.json)。

这些是共享 OrbStack 上的短时回归数据，客户端、上游及其他工作负载共用宿主资源，场景间小幅波动不能解释为插件加速或固定开销。它们不证明生产容量上限、长期内存稳定性或相对 NGINX 的性能优势。

## 复现命令

```sh
# OrbStack Ubuntu 内，从仓库目录执行
export CARGO_TARGET_DIR=/tmp/rgnix-target
cargo fmt --check
cargo clippy --locked --all-targets -j 2 -- -D warnings
cargo test --locked -j 2
python3 scripts/integration.py .local/rgnix-linux-arm64
python3 scripts/nginx_parity.py .local/rgnix-linux-arm64
python3 scripts/ingress_recovery.py .local/rgnix-linux-arm64

# 对待测 release 二进制生成新的性能数据
python3 scripts/benchmark.py .local/rgnix-linux-arm64
python3 scripts/benchmark.py .local/rgnix-linux-arm64 --proxy --concurrency 32 --routes 1000

# macOS / OrbStack Docker 与 Kubernetes
docker build -t rgnix:0.1.0-qa9 .
RGNIX_IMAGE_TAG=0.1.0-qa9 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

运行二进制应从对应镜像提取并记录 SHA-256。脚本在 `.local/` 生成结果；本次确认的结果保存在 `docs/validation/`。

## 仍需验证的边界

- Linux amd64 的实际构建与运行、多节点故障转移、云 LoadBalancer、集群双栈仍未实测。
- 未完成专用机器上的长期高负载、API Server 长期断连和大规模资源变更测试。相关资源变化仍重建所选快照，不宣称已完成全量增量控制器。
- 编译预算限制已知栈/结构风险，编译仍在同一进程，不等于独立进程级故障隔离。并发额度不限制尚未读完请求头的 TCP 连接总量。
- 上游保护是被动摘除；主动健康探测、完整 Lua/NGINX、Gateway API 和自动重试仍不属于本版范围。

本轮结果证明已复现缺陷的修复及上述回归场景通过，不替代生产规模认证。

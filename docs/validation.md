# v0.1 实际验证记录

本文保留首轮 `qa5` 的历史结果。`qa9` 的缺陷修复见 [上一轮修复记录](validation-fixes.md)，当前 `qa10` 产物和验收见 [最新验证记录](validation-isolation.md)。

日期：2026-09-23。以下是运行证据，不是仅根据代码列出的功能清单。

## 环境与产物

| 项目 | 实际值 |
|---|---|
| Linux 环境 | OrbStack Ubuntu 25.10，aarch64，17 个可见逻辑 CPU，共享宿主机 |
| 内核 | `7.0.14-orbstack-00380-ga7e0a2dc9535` |
| 开发构建 | Rust 1.90.0，`CARGO_TARGET_DIR=/tmp/rgnix-target` |
| 镜像 release 构建 | `rust:1.98-bookworm`，Rust 1.98.1，OpenSSL，Debian bookworm |
| Kubernetes | OrbStack 单节点 `v1.35.6+orb1`，独立 namespace `rgnix-qa-20260923` |
| 对照服务 | NGINX **1.28.0 (Ubuntu)**；脚本拒绝其他版本 |
| 关键依赖 | Pingora 0.9.0、Wasmtime 38.0.4、kube 2.0.1，完整版本见 Cargo.lock |
| 本地镜像 | `rgnix:0.1.0`，验收固定标签 `rgnix:0.1.0-qa5`，Linux arm64 |
| 可执行文件 | `.local/rgnix-linux-arm64`，28,076,280 字节，从最终镜像提取 |

最终镜像 manifest list：

```text
sha256:856da9861f6dea577fde69d4b07e63165bafa4b3396fb35c15269d2e7115782b
```

二进制 SHA-256：

```text
752c5cc35e245693de225c829fc9135286fc720583d2e4d1597dd218c48f2e6f
```

Cargo.lock SHA-256：`c38580e1709df155dbe45b81539954b7c1d0834d1341bd7251682faf5424d1ec`。

Rust 源树摘要：`21285974cb3a5b52a6133457ba9d53643b2df2310887600570469c211ed21ad6`。算法为排序后的 Cargo.toml、Cargo.lock、src 下文件依次输入 SHA-256，每个文件输入相对路径、NUL、文件字节。文档/脚本修改不影响此摘要。

## 验收结果

| 门禁 | 结果 | 证据 |
|---|---|---|
| `cargo fmt --check` | 通过 | 最终源码检查 |
| `cargo test --locked -j 2` | **5/5** | [Rust 输出](validation/rust-checks.txt) |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过，无 warning | 同上 |
| HTTP/插件真实 socket 验收 | **35/35** | [逐项结果](validation/http.json) |
| NGINX 1.28.0 差分 | **16/16** | [逐项结果](validation/nginx-parity.json) |
| Helm lint、模板渲染、Shell 语法 | 通过 | `helm lint` 无失败；仅建议设置图标 |
| Kubernetes 完整资源生命周期 | **31/31**，升级期间 300 次 Service 请求均成功 | [逐项结果](validation/ingress.json) |

Rust 测试保护 NGINX/Kubernetes 路径语义差异、静态文件能力根边界、编译函数/分支/响应钩子、类型与递归拒绝、无限循环 fuel 限额、1 MiB 宿主分配和超出 8 MiB 的 Wasm 内存拒绝。没有为静态指令表或显而易见的内部映射堆叠测试。

HTTP 验收启动真实 Python 上游与 rgnix release 二进制，覆盖：

- GET/HEAD、MIME、索引及重定向、Range/416、ETag/304、If-Range 日期、目录外符号链接及路径逃逸。
- 带 URI / 不带 URI 的代理语义、编码保留、头部继承、IPv6 上游。
- 可信 HTTPS 上游成功，主机名不匹配及不可信证书均 502；客户端 TLS 和 curl 实际 HTTP/2 协商。
- 约 1 MB 请求流、超限 413、SSE 第一条事件及时到达、WebSocket 双向帧、客户端取消、上游读超时。
- POST 上游执行副作用后断连：下游 502，上游副作用计数 **恰好 1**，没有重放。
- RGL 编译 `.wasm` 后独立加载、请求与响应钩子、直接 403、无限循环 500、响应钩子失败后 500 且暂存头不泄漏。
- SIGHUP 后旧请求仍得到旧响应钩子结果、新请求使用新版本；无效更新继续使用旧版本；指标可读。
- Ingress 控制器初始化失败后 healthz/readyz 均为 503。

NGINX 对照比较精确/字符串前缀/默认主机/通配主机、尾斜杠 301、原始与替换 URI（空格、百分号、编码斜杠及问号）、嵌套 include 路径、请求 Host 和响应头整组继承、静态正文及 Range。它不声称覆盖所有 NGINX 指令组合，不比较 Date、Server、ETag 文字、默认错误页、重定向 HTML 等实现差异。

Ingress 脚本在专用命名空间创建、修改、删除自己的资源。覆盖命名/数字 Service 端口、Exact、Prefix 段边界、Class 隔离、defaultBackend、单标签通配、冲突稳定优先级及 Event、编译插件后端选择、坏脚本保留旧模块及路由、端点撤销/恢复、Service 与 ConfigMap 删除/恢复、TLS 轮换/删除/恢复、Lease 选主和领导者终止、地址回写及撤销、滚动升级和 Ingress 删除。最后恢复应用 Ingress 供检查，controller **2/2 Ready**。修正终止配置后，滚动升级期间通过集群内 Service 的 300 次请求全部成功。

状态地址 `192.0.2.10` 是测试写入 Service/status 的保留示例地址；这验证地址复制和清除逻辑，**不是云 LoadBalancer 分配或公网可达性验证**。TLS 测试使用短期自签名证书，测试客户端对入口使用 `-k`；上游证书校验是独立的正反向测试。

## 性能基线

使用最终 release 二进制、两个工作线程、关闭访问日志。Python 标准库 HTTP/1.1 keepalive 客户端并发 8，返回本地 2 字节正文；每场景预热 1 秒，测量 3 轮，每轮约 3 秒。CPU 从服务进程 `/proc/PID/stat` 差值计算，RSS 从 `/proc/PID/status` 读取。完整原始数据见 [benchmark-arm64.json](validation/benchmark-arm64.json)。

| 场景 | RPS 中位数 | p50 中位数 | p95 中位数 | p99 中位数 | 服务 CPU 中位数（核） | 最大 RSS |
|---|---:|---:|---:|---:|---:|---:|
| 无插件 | 24,380.8 | 0.260 ms | 0.805 ms | 1.167 ms | 0.300 | 21.09 MiB |
| 编译插件 `route.pass()` | 23,868.5 | 0.268 ms | 0.817 ms | 1.198 ms | 0.460 | 21.38 MiB |
| 编译插件读头、分支、改头 | 22,422.1 | 0.281 ms | 0.881 ms | 1.325 ms | 0.623 | 21.61 MiB |

9 轮请求错误均为 0。这里是**共享 OrbStack 上的回归基线**：客户端与服务在同一虚拟机，客户端和其他工作负载会影响吞吐，服务 CPU 没有跑满；各场景顺序执行，也未证明长期稳定性。不能据此宣称生产容量上限、插件固定开销比例或超过 NGINX 性能。该负载没有测量真实上游代理的网络/TLS成本；之后应在专用机器上补多并发、长连接、大流量和长期 soak。

## 验收中修复的问题

- Pingora 尝试预算包括首次请求：固定为 1，以断连 POST 副作用计数确认不自动重试。
- 修正 URI 编码和前缀替换、目录缺失处理、include 相对目录以及 If-Range 日期精度。
- kube TLS 与数据面统一为 OpenSSL，修复初始化时 TLS provider 冲突；控制服务失败能使探针失败。
- 用预链接 Wasm 模块复用加载工作，请求仍独立实例化；宿主修改和资源预算通过实际失败路径验证。
- 无效插件期间保留上一有效路由定义，独立应用端点/Service/证书及整个资源的撤销。
- Service 地址清空时，JSON merge patch 必须显式删除旧 ingress 数组。
- 连续滚动升级探测曾发现连接拒绝；Chart 增加 preStop 端点传播等待，以及 maxUnavailable=0 / minReadySeconds。
- 容器构建缓存曾复用旧二进制；构建步骤保留依赖缓存但显式重新构建 rgnix crate。最终验收使用从镜像提取的可执行文件。

## 可重复命令

```sh
# OrbStack Ubuntu 内
export CARGO_TARGET_DIR=/tmp/rgnix-target
cargo test --locked -j 2
cargo clippy --locked --all-targets -j 2 -- -D warnings
python3 scripts/integration.py .local/rgnix-linux-arm64
python3 scripts/nginx_parity.py .local/rgnix-linux-arm64
python3 scripts/benchmark.py .local/rgnix-linux-arm64

# macOS 的 OrbStack Docker / kubectl / Helm
docker build -t rgnix:0.1.0 .
RGNIX_IMAGE_TAG=0.1.0 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

运行后 JSON 写入 `.local/`；本次经过确认的结果拷贝在 `docs/validation/`，[产物摘要](validation/artifact.json) 记录对应镜像、源码和二进制。验收环境保留供检查，不自动清理或影响已有 namespace。运行中的 Pod 使用验收标签；正式标签指向同一镜像，未推送远端仓库。

## 尚未验证

- Linux amd64 的构建/运行：已提供双架构 Docker/CI 配置，本机实测只有 arm64。
- 多节点或云托管 Kubernetes、云 LB、集群 IPv6/双栈：代码解析 IPv6 EndpointSlice，独立模式 IPv6 上游已跑通，当前 Kubernetes 测试为 IPv4。
- 大量 Ingress/端点变化、API Server 长期断连、长时间高负载/内存 soak、复杂证书链与全部 SNI 组合。
- 自动证书签发、Gateway API、完整 NGINX/Lua 行为不属于本版范围。

这些结果支持可部署首版及后续回归，不等于完成生产规模认证。

# Hyper 内核补齐验证 — 2026-09-28

本次将现有产品策略接入实验性 Hyper 传输，保持默认 Pingora。验证的是当前工作树，尚未提交或发布。之前的分配器、缓冲复用和 NGINX 性能报告对应旧实现，不能作为本轮功能扩展后的性能证明。

## 实现范围

- 抽取认证、插件调用、命名空间准入、后端选择、请求头和静态文件响应选择，与 Pingora 共用业务语义。
- OpenSSL TLS/SNI 动态证书、CA/SNI/客户端证书隔离、PROXY v1/v2、HTTP/2 与 h2c、gRPC 双向流、WebSocket、SSE。
- RGL/Wasm 请求与响应钩子、完整和前缀 body 检查；预读后转发完整原始数据，不重放请求。
- JWT/JWKS、外部认证、mTLS、租户配额、共享限流、灰度与镜像、Ingress/Gateway 控制面。
- 静态文件的安全目录访问、HEAD/Range/条件请求、gzip/Brotli、日志/OTLP/指标、快照发布与回退。
- 所有监听器共享下游连接上限；上游空闲连接预算跨客户端和快照版本共享；真实连接复用指标。
- HTTP/2 每流响应头、响应体和发送流控期限；HTTP/1/2 的 102/103 不提前执行最终响应钩子。

## 运行中发现并修复

1. 静态文件和带 trailers 的响应在正常结束时被误记为取消：补充精确剩余长度和 trailer 完成状态。
2. WebSocket 带体握手可能绕过请求体大小检查：升级前执行限制；非空握手体拒绝升级。
3. 排空期间拒绝原连接上的下一请求：对齐既有行为，完成请求并发送 Connection: close，保留已建立的 WebSocket 至截止时间。
4. HTTP/2 的 :authority 使用端点 IP，与配置 Host 不一致：在连接和池选择后单独应用 authority，严格 H2 后端可以接收配置域名。
5. H2 同连接的活动掩盖另一流的超时，以及 RST_STREAM 覆盖超时原因：增加流级期限并保留 504 分类。
6. 通用套件对合法的 HTTP 请求头大小写敏感：仅修正 fixture 查找方式，保留重复值和隔离行为断言。

## 验证环境和结果

以下均为实际完成的运行结果，完整检查名称、日志摘要哈希和产物身份保存在[机器可读记录](validation/hyper-kernel-2026-09-28.json)。

| 验证 | 结果 | 范围 |
|---|---:|---|
| Hyper HTTP 集成 | 94 通过 | 路由、流式请求、取消、超时、WebSocket、热更新和排空 |
| Hyper 产品行为 | 115 通过 | 认证、RGL/body、静态文件、压缩、TLS/H2/gRPC、租户和发布策略 |
| Hyper 传输专项 | 52 通过 | 同连接 H2 超时隔离、trailers、连接总预算、102/103、错误分类与生命周期 |
| Kubernetes API 恢复 fixture | 57 通过 | kube-rs watch、端点撤销、删除、检查点和 Lease；这行不是实际集群测试 |
| 共享 Redis 限流 | 12 通过 | 跨进程配额、协调器中断和恢复 |
| OTLP 日志和链路 | 32 通过 | 上下文传播、导出队列、错误处理和退出刷新 |
| 本地日志轮转 | 20 通过 | 内置轮转、重开、真实 logrotate、队列拥塞 |
| 迁移工具 | 10 通过 | 生成配置加载、差异检查和拒绝静默丢失策略 |
| 同一二进制默认 Pingora | 94 + 115 通过 | HTTP 和产品行为对照，未设置实验开关 |
| 实际 Kubernetes Ingress | 46 通过 | 路由、端点、TLS 撤销与轮换、选主、删除、双副本滚动升级 |
| 实际 Kubernetes Gateway | 115 通过 | HTTPRoute/GRPCRoute、ReferenceGrant、BackendTLSPolicy、配额、镜像、发布和准入 |
| Rust 单元测试 | 24 通过，1 忽略 | 根项目启用 Hyper 特性 |
| vendored Hyper 单元测试 | 118 通过，6 忽略 | 库测试，启用完整特性 |
| vendored hyper-util 连接池测试 | 26 通过，2 忽略 | `client::legacy` 范围；另 11 项被过滤 |
| 构建和静态检查 | 通过 | release、两种构建的 Clippy、Rust 格式、Python/Shell 语法、Helm lint、diff 检查 |

Hyper 独立运行与 API fixture 共 392 项检查；Pingora 对照共 209 项。实际 Ingress 滚动升级期间的 300 次请求全部成功。Gateway 在 100 条额外路由下运行 60 秒，包含 HTTP、TLS、RGL、body、日志与 tracing，期间发布插件并替换 Pod：**10,137 次请求，0 失败**。该负载每个客户端限制最多 40 次/秒，只证明本次有界连续性，不是吞吐基准或容量上限。

Linux 构建与原生行为测试使用 OrbStack Ubuntu aarch64。现有 OrbStack Kubernetes 集群最初因 Service 地址池耗尽而无法部署；未清理其他工作负载，删除本任务新建的失败命名空间后，在 OrbStack Docker 中建立独立 Kind v1.34.0 集群（一个控制节点、两个工作节点）。两个控制器副本确实调度在不同工作节点，但这些节点共享同一台宿主机。默认 Kubernetes context 未改动；测试集群和命名空间保留用于检查。

最终 release 启用 `hyper-experimental,jemalloc`，二进制 SHA256 为 `ed872bb8463280226ea12b013d9dcde0e838d4c4f0b099534c760c16225190ff`。本地测试镜像为 `rgnix:hyper-kernel-20260928`，将该 ARM64 二进制放入已有 Ubuntu 24.04 测试镜像；已在运行中的 Gateway Pod 内核对二进制哈希及 `--experimental-hyper` 参数。镜像 ID、基线提交、工作树源码摘要见机器记录。这不是已发布镜像，也不是本轮生产 Dockerfile 构建验证。

验证还修正了两个 fixture 假设：通用 gRPC 超时检查不再要求 Pingora 必须复用同一连接，Hyper 专项仍显式检查同连接流隔离；安全策略检查等待实际数据面响应，避免管理就绪先于 Pingora listener 绑定的启动窗口。最终套件均重新运行通过。

未在本轮执行原生 amd64、跨物理节点故障、长时间容量、完整 Gateway conformance 或新的 NGINX 性能比较。新增 GitHub Actions Hyper 作业尚未在远端执行。默认仍为 Pingora，Hyper 继续显式启用；CONNECT、HTTP/2 Extended CONNECT、h2c Upgrade、HTTP/3 以及 Hyper 静态文件 sendfile 优化均不属于本次补齐范围。

## 重现

```sh
cargo build --locked --release --features hyper-experimental,jemalloc
RGNIX_CARGO_FEATURES=hyper-experimental RGNIX_EXPERIMENTAL_HYPER=true bash scripts/check.sh
python3 scripts/hyper_integration.py target/debug/rgnix
cargo test --manifest-path vendor/hyper-util/Cargo.toml --lib --features client-legacy,server-auto,http1,http2,tokio client::legacy
```

实际 Kubernetes 验收使用已载入测试镜像并安装 Gateway API v1.6.1 standard CRD 的独立集群：

```sh
export KUBECONFIG="$PWD/.local/kernel-fill-20260928/kubeconfig"
RGNIX_IMAGE_REPOSITORY=rgnix RGNIX_IMAGE_TAG=hyper-kernel-20260928 \
  RGNIX_EXPERIMENTAL_HYPER=true bash scripts/ingress-e2e.sh \
  rgnix-hyper-ingress-20260928 kind-rgnix-hyper-20260928
python3 scripts/gateway_e2e.py --context kind-rgnix-hyper-20260928 \
  --namespace rgnix-hyper-kernel-20260928 --image rgnix:hyper-kernel-20260928 \
  --experimental-hyper --require-multiple-nodes --soak-seconds 60 --scale-routes 100 \
  --output .local/kernel-fill-20260928/gateway.json
```

Helm 使用包含该 Cargo feature 的镜像，并设置 `experimentalHyper.enabled=true`。原生 Docker 构建可使用 `--build-arg CARGO_FEATURES=hyper-experimental,jemalloc`。开启方式、资源边界及传输差异见 [Hyper 文档](hyper-experimental.md)。

# OTLP span 与 W3C 传播验证 · 2026-09-25

本次针对 v0.2.0 之后的工作区源码，包含此前运维指标扩展，未改写 GitHub v0.2.0 附件。基线提交 `538ef3d59bae45766b0b2c3aa0fdf176949ac899`。实现及接入方式见[链路指南](tracing.md)，[机器可读结果](validation/tracing-2026-09-25.json)保存完整检查名称、实际 trace/span ID 和镜像标识。

## 构建与行为回归

OrbStack Ubuntu / Rust 1.90.0，执行锁定依赖的 `cargo build/test/clippy --locked -j 2`；Clippy 使用 `--all-targets -- -D warnings`，`cargo fmt --check` 通过。

| 验证 | 结果 |
|---|---:|
| Rust 单元测试 | 7/7 |
| HTTP、流式传输、插件与预算 `scripts/integration.py` | 82/82 |
| 产品及链路行为 `scripts/product_features.py` | 111/111 |
| OTLP 传输、故障及退出 `scripts/otlp_integration.py` | 32/32 |
| 本地文件轮转 `scripts/log_rotation.py` | 20/20 |
| 标准 Collector + HTTPS + 双副本 Ingress | 6/6 |
| 双副本鉴权、镜像与日志关联 | 8/8 |

共 266 项行为检查通过。OTLP 故障套件覆盖共用 exporter 传输层，主要以 logs 为载荷；traces 的真实 protobuf 接收另由双代理与标准 Collector 验证。没有将此前 Ingress 恢复和指标专项的历史检查重复计入本轮。

新增链路断言在已有产品套件中验证：

- 通过两个真实代理端口，一次请求产生同 trace 的四个相连 span，业务后端收到最后一个 client span ID；每跳日志关联自己的 server span。
- `tracestate` 同时进入下游头和 OTLP span；多头按顺序合并，重复 key 被丢弃，超长 state 按完整成员截断。
- 父采样优先于本地 ratio；未采样请求仍传播上下文、产生关联日志，不导出 span；关闭本代理 traces 时保留传入采样标记。
- future version、重复 parent、version ff、零 ID、大小写错误、version 00 多余字段按规定继续或重建上下文，非法 parent 不携带孤立 state。
- 配置中的传播头修改不会使最终转发 ID 与已记录 span 脱节；鉴权有独立 client span。
- parent remote flags、起止时间和日志原生 ID 对应；HTTP 401 对 client 为 ERROR、server 为 UNSET；gRPC 非零状态两个 span 均为 ERROR。
- combined/JSON 本地访问日志带 trace、server、parent、主后端 ID 和采样字段；请求 query 中的合成私密值未进入 span。

## 标准 Collector 与 Kubernetes

在专用命名空间 `rgnix-tracing-qa-20260925` 通过 Helm 部署两个控制器，限定同 namespace watch/RBAC。Collector Contrib **0.123.0** 开启 OTLP/HTTP logs/traces 接收，使用测试私有 CA、HTTPS 和 Bearer 认证。两信号各自配置 CA 与认证 Secret。父级 sampled=1、控制器 ratio=0，确认父级决策生效。

复现标准 Collector 阶段：

```sh
RGNIX_IMAGE_TAG=tracing-20260925 \
RGNIX_OTLP_EVIDENCE_DIR=.local/tracing-collector \
bash scripts/otlp-collector-e2e.sh rgnix-tracing-qa-example orbstack
```

该脚本要求镜像已构建，创建或复用带 `rgnix-qa=true` 的专用 namespace，生成 1 天有效的测试证书。它检查每个 Pod 的业务响应、Collector 接受计数、Kubernetes 资源属性、server/client span、父 ID、state 及关联日志。

随后在同一测试 namespace 增加 HTTP 回显服务，配置外部鉴权及 100% 镜像。逐个 Pod 发送采样请求，并同时比对回显服务收到的三个 HTTP 请求、Collector 实际解码结果和控制器 JSON stdout：

| Pod | Trace ID | Server span | Auth / Proxy / Mirror 子 span |
|---|---|---|---|
| `rgnix-otlp-7956d6b7ff-dznsv` | `41f92f3577b34da6a3ce929d0e0e4736` | `f2a41545ad5a428d` | `b719290c7f03257e` / `834dcbdbedc4b546` / `720ff4f833f7b1ec` |
| `rgnix-otlp-7956d6b7ff-srrrc` | `41f92f3577b34da6a3ce929d0e0e4737` | `818173ba20792ded` | `73bce54caf653e0b` / `ae037e1e3d565b4f` / `a01c5efe0174e9ec` |

两组 server 的远程父 ID 均为 `01f067aa0ba902b7`，三个 client 都以同组 server 为父。三个下游 `traceparent` 分别与对应 client span ID 一致，`tracestate` 均为 `qa=mirror`。OTLP 日志原生 span ID 和本地 JSON `span_id` 对应同组 server，`upstream_span_id` 对应主业务 client。

镜像阶段的一次性运行脚本及原始解码输出保存在本机 `.local/tracing-mirror-kubernetes.py`、`.local/tracing-collector/`；上表和机器可读结果是本次运行留档，未将这些数据描述为覆盖所有 Kubernetes 生命周期行为。

## 构建标识及边界

- Linux arm64 debug 二进制 SHA256：`0a0239bdb541973ffcf406a46b3557ce483ccae4f2197e06482f2df46b8dab64`。
- Dockerfile 使用 Rust 1.98 / Debian bookworm 构建 `rgnix:tracing-20260925`，镜像 ID：`sha256:5ad4c957c55f4c9aa4f0c1f97b785eda7f5663da533846b7ca23976bef3a5755`；两个 Pod 的 imageID 对应此摘要。
- 镜像内二进制 SHA256：`c9ea33a2dddc7ace6d49c9a999452b10cdbeb274aa886731fca65cbb675fe41f`。
- 二进制相关源码 SHA256：`67779247b88f85151c9bd5eaf6746089758c467ec50906ded9c6476c4c00543b`。算法为 Cargo.toml、Cargo.lock、src/、vendor/ 下文件按 Python Path 排序，逐项拼接相对路径、NUL、文件内容、NUL 后计算 SHA256。

没有第三方 SaaS 凭据，未验证具体平台的索引、查询或保留；验证的是标准 Collector 的实际接收与 protobuf 解码。未运行 amd64、trace 性能基线或长期断网测试。测试 namespace 保留，未修改其他业务 namespace；这份留档中的合成证书不用于生产。

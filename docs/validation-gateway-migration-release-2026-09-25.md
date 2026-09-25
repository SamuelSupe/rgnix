# Gateway、迁移与发行流程验证 · 2026-09-25

本次验证 v0.2.0 之后的工作区源码，基线提交为 `538ef3d59bae45766b0b2c3aa0fdf176949ac899`，包含此前尚未发布的指标和链路改动。没有修改现有 GitHub v0.2.0 附件，也没有创建新版本或向 GHCR 推送镜像。

实现范围见 [Gateway API](gateway-api.md)、[迁移工具](migration.md)和[发行流程](releases.md)。[机器可读记录](validation/gateway-migration-release-2026-09-25.json)包含检查名称、构建摘要及实际 Pod 标识。

## 本地运行与回归

Linux 测试运行于 OrbStack Ubuntu arm64，Rust 1.90.0、Python 3.13.7。业务镜像使用 Rust 1.98 / Debian bookworm 构建，避免将 Ubuntu 的新 glibc 依赖带入 Debian 运行镜像。依赖使用 Cargo.lock；格式检查、Clippy `--all-targets -- -D warnings` 和构建均通过。

| 验证 | 通过 |
| --- | ---: |
| Rust 单元测试 | 9/9 |
| HTTP、TLS、流式传输、插件与预算 | 82/82 |
| 产品策略、认证、模拟器与链路 | 111/111 |
| Ingress API 故障及恢复 | 57/57 |
| OTLP 传输、故障与退出 | 32/32 |
| 本地文件轮转 | 20/20 |
| 迁移命令 | 10/10 |
| 真实 Kubernetes Gateway 行为 | 36/36 |

HTTP、产品和文件日志套件在本轮集成过程中运行；最终协议发布修复后重跑了 Rust、迁移、Ingress 和 OTLP，最终镜像重跑了 Gateway 套件。未将此前 NGINX 对照、治理压测或标准 Collector 专项结果计入本轮。

迁移检查实际覆盖：显式确认进程模型差异、跨目录候选配置的默认 root、候选加载、请求比较发现行为变化、汇总不支持指令、命名端口解析、多文档 YAML、认证注解不丢失、通配域名不扩权，以及默认后端冲突阻断。

## Kubernetes 验收

测试使用 OrbStack Kubernetes `v1.35.6+orb1`、Gateway API **v1.6.1 standard CRDs**、Helm 4.2.0。CRD 下载文件 SHA256 为 `24d931f22abd8e40c973264319ead7cfa09d0fb7716b7ab1ee2ff174cb063a73`，与官方 release asset 元数据核对一致。

专用 namespace 为 `rgnix-gateway-qa-20260925` 与 `rgnix-gateway-qa-20260925-peer`。验证内容包括：

- HTTPRoute 的路径段、方法、请求头、重复头首值、查询参数匹配，URI 改写、字面量头修改和重定向。
- 90/10 Service 权重分配；跨 namespace Service 的 ReferenceGrant 授权与撤销。
- RGL 选择已声明后端；编译失败保留旧插件时，端点撤销、恢复和路由删除仍生效。
- allowedRoutes 的 Same、All、标签选择器；管理身份不能通过改变模拟请求的方法跨越 namespace 权限。
- 跨 namespace TLS Secret 授权、真实 SNI 握手、证书热轮换，以及删除后的握手拒绝。
- GRPCRoute 的 HTTP/2 unary、双向流与 trailers；仅修改 Service appProtocol 即可更新后端协议；不支持的 HTTPS 协议不会降级为明文。
- 两副本滚动重启并恢复就绪。该检查验证 Deployment 可用状态，不等同于持续负载下的零错误升级证明。

结束时两个 Gateway Pod 均 ready、重启计数为 0，并运行相同镜像摘要。HTTP/TLS 请求主要通过一个 Pod 的 port-forward 验证，gRPC 请求通过 Service 访问；没有将其描述为逐 Pod 全矩阵验证。测试资源保留供检查；其中包含故意拒绝的配置和已删除证书的 listener，不作为生产示例。

复现命令见 [Gateway 验证说明](gateway-api.md#reproduce-behavioral-validation)。脚本只允许复用带有本测试归属标签的 namespace。

## 发行流程验证

- Actionlint 1.7.7 检查 CI 和 release workflow 通过。
- Helm lint、Gateway 模式渲染、复用外部 GatewayClass 与 namespace watch 参数渲染通过；成功打包本地 chart 候选。
- `scripts/release_version.py` 核对 Cargo/Chart 版本通过。发布工作流在推送镜像前拒绝覆盖已存在的 GitHub release。
- 使用最终 Debian 构建的 arm64 二进制，按 `Dockerfile.release` 重新组装运行镜像；`--version` 与 `gateway --help` 执行成功。
- 工作流已配置原生 amd64/arm64 构建、Kind Gateway 验收、镜像 SBOM/provenance、签名、OCI chart、校验和与 GitHub artifact attestations。跨架构运行镜像组装配置了 QEMU；这不替代原生二进制测试。

当前 Cargo/Chart 版本仍为 0.2.0，本地 chart 和二进制属于开发候选，不能用来覆盖已有发布。正式发布前必须同步更新版本，并执行新的远端发行流程。

## 最终产物标识与边界

- 数据面镜像：`rgnix:gateway-release-qa-20260925`。
- 镜像摘要：`sha256:bd62cf33518e2cd48209271dda1fa3f83919b38154ab001f6989f440e991d072`；两个 Pod 的 imageID 对应该摘要。
- 镜像内二进制 SHA256：`c5b7c94ab0864a391289f29fbf645d3928fb29555f5480b3bbca61879bb612d5`。
- Linux debug 二进制 SHA256：`b9696002935e2c698fbfa216a9d5c3c51e407faac0f09c33bd0b0fe5773c4ec7`。
- 二进制源码 SHA256：`71c74b26e2001aa2d9dfcb44c051100a38d604c5e3785f8cf79cd3de8752167c`。对 Cargo.toml、Cargo.lock、src 下 Rust 文件和 vendor 下全部文件按相对路径排序，依次拼接路径、NUL、内容、NUL 后计算 SHA256。

尚未执行上游 Gateway API conformance 套件、远端 Actions、原生 amd64 验收、GHCR/OCI 发布、公开签名或 attestations 验证，也未验证多节点、云 LoadBalancer 和长期性能。工作流配置不能作为这些步骤已经成功的证据。

Gateway 当前为绑定单个 Gateway 的预置数据面，不动态创建任意 Gateway 的基础设施；不支持的资源/字段见兼容说明。Gateway 插件的上一有效版本仅保存在进程内，重启前需恢复有效源文件，不能沿用 Ingress 持久化恢复的保证。

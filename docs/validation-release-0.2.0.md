# v0.2.0 发布验证 — 2026-09-25

本次发布包含请求体路由、OTLP 日志/链路、本地日志轮转、入口策略、认证、调度、多租户治理及分阶段灰度。中英文 README、安装文档、Chart 与 Cargo 版本统一到 0.2.0。

## 发布产物与来源

- Git 标签：`v0.2.0`；[Release](https://github.com/SamuelSupe/rgnix/releases/tag/v0.2.0)。
- `rgnix-0.2.0-linux-arm64.tar.gz`：Linux arm64 二进制、示例、文档、Chart 与许可证。
- `rgnix-0.2.0.tgz`：可单独安装的 Helm Chart；容器镜像仍需自行构建和推送。
- `SHA256SUMS`：上述两个下载文件的 SHA-256。
- 二进制 SHA-256：`c7627936d9b3482e9642f8c09eddec101268d2313aa8ee292847c84d25e1e886`。
- 生产源码摘要：`45c6c7800856977d137a6b880eeabc94ee36aea766b10ab4b4177526604a4dac`。摘要定义、镜像、Chart 和 Pod 身份见 [artifact](validation/release-0.2.0-artifact.json)。

二进制从仓库 Dockerfile 构建的 `rgnix:0.2.0` 提取，Rust 1.98 / Debian bookworm，运行镜像 UID/GID 为 10101。ELF 实际依赖 libgcc_s、libm、libc，最高 glibc 符号版本为 2.34；没有动态 libssl/libcrypto 依赖。下载包不适用于 macOS 或 Alpine/musl。

对照治理 qa4，生产文件仅修改 Cargo.toml/Cargo.lock 的包版本和 Cargo 元数据；`src` 与 `vendor` 逐字节一致。撤销这些元数据改动能复现 qa4 的原摘要。此前运行记录保留当时的镜像和工作区状态，不能与本次发布检查混为一轮。

## 本次实际运行

以下网络回归全部使用最终 `rgnix 0.2.0` release 二进制，在 OrbStack Ubuntu Linux arm64 执行：

| 验证项 | 结果 | 证据 |
|---|---|---|
| HTTP/TLS、流式请求、WebSocket、SSE、插件 | 79/79 | [清单](validation/release-0.2.0-http.json) |
| 认证、调度、诊断、历史、具名身份及策略热更新 | 89/89 | [清单](validation/release-0.2.0-features.json) |
| 模拟 Kubernetes API 故障与恢复 | 53/53 | [清单](validation/release-0.2.0-recovery.json) |
| NGINX 1.28.0 对照 | 46/46 | [清单](validation/release-0.2.0-nginx.json) |
| OTLP 投递、故障与退出 | 32/32 | [清单](validation/release-0.2.0-otlp.json) |
| 文件轮转与外部 logrotate | 20/20 | [清单](validation/release-0.2.0-log-rotation.json) |
| 真实双副本升级、灰度状态、准入及 Class 撤销/恢复 | 9/9 | [清单](validation/release-0.2.0-upgrade.json)、[输出](validation/release-0.2.0-upgrade.txt) |
| Rust 测试、fmt、Clippy | 7/7；fmt 与 `-D warnings` 通过 | Rust 1.90.0；[输出](validation/release-0.2.0-rust-checks.txt) |
| Helm | lint、默认与治理选项模板渲染通过 | [lint](validation/release-0.2.0-helm.txt) |

真实集群使用已有专用 namespace `rgnix-governance-final-20260925`，先校验 `rgnix-qa=true` 标签再升级。两个 Pod 均为 Ready，`/proc/1/exe` 摘要与下载二进制一致，版本均为 `rgnix 0.2.0`。探针沿用[治理升级脚本](validation/governance-upgrade-probe.py)，只替换镜像/tag 为 0.2.0 和结果输出位置；探针摘要记录在 artifact。集群为 OrbStack 单节点 v1.35.6+orb1，kubectl 1.33 的版本偏差仍存在，实际 API 调用与断言成功。

本次没有重跑此前 46 项真实集群基础生命周期、65 项完整治理场景和 300 次滚动请求。它们的具体 QA 产物和代码增量关系见[治理阶段记录](validation-governance.md)。最终升级探针覆盖本次版本化产物的双副本部署及关键持久化边界。

## 复现与边界

使用下载包中的二进制，配合本仓库的 `scripts/integration.py`、`ingress_recovery.py`、`otlp_integration.py`、`log_rotation.py`、`product_features.py` 和 `nginx_parity.py`。这些脚本使用本地临时服务；依赖见[开发指南](../CONTRIBUTING.md)。NGINX 对照要求固定 1.28.0。真实集群脚本会变更专用测试资源，应先阅读脚本并使用 QA namespace。

本次不声明 amd64 运行、多节点、云 LoadBalancer、长期压力或恶意租户容量极限通过。配额按进程计数；外部指标异常阻止阶段推进，自动回退由本地错误率/p95 规则触发。未发布 registry 镜像。GitHub Actions 结果应查看具体提交的 workflow，不能从这些本地结果推断。

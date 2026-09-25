# 产品 P1 修复验证 · 2026-09-25

本轮实现 Gateway 策略与持久化恢复、跨副本请求速率配额，以及外部业务指标自动回退。源码、README、Helm 和配置文档已更新，尚未提交 Git 或发布 GitHub；已发布的 v0.2.0 不包含这些改动。[机器可读记录](validation/p1-product-2026-09-25.json)保存检查名称、源代码摘要和实际 Pod 标识。

## 修复范围

| P1 | 实现与验证 |
| --- | --- |
| Gateway 策略缺口 | JWT、外部鉴权、速率限制、90/10 分流、镜像、阶段审批与回退；只允许选择声明且授权的 Service；BackendTLSPolicy 验证 CA/SNI，信任变化隔离连接池 |
| Gateway 重启丢失有效插件 | 控制器 namespace ConfigMap 检查点记录源码、路由和注解，并绑定 Gateway、Route、插件 ConfigMap UID；无效插件期间逐个新副本验证恢复；端点、权限和对象撤销继续生效 |
| 多副本各算一份速率配额 | 可选 Redis 原子令牌桶共享 route 和 namespace 请求速率；闭合、放行、本地降级三种故障策略；配置热更新、键容量和查询并发限制、指标与诊断 |
| 外部指标只能阻止推进 | 连续失败次数与持续时间门槛触发 fallback；区分超标和 provider 不可用；校验 UID、完整策略和 resourceVersion 后写回回退 revision，所有副本及重启后生效 |

同时修复了两个边界：重建同名 ConfigMap 后清除旧检查点；JWT Secret 撤销后保留已授权匹配的拒绝响应，避免落入同域名的公开前缀路由。实际 HTTP 用例覆盖这两种情形。

## 实际运行

环境：OrbStack Linux arm64，原生 Rust 1.90.0，Redis 8.0.2；镜像由 Rust 1.98 / Debian bookworm 构建。Kubernetes `v1.35.6+orb1`，Gateway API v1.6.1 standard CRDs，Helm 4.2.0。

| 检查 | 结果 |
| --- | ---: |
| Rust 单元与边界测试 | 12/12 |
| HTTP 行为回归 | 82/82 |
| Ingress 检查点与资源恢复回归 | 57/57 |
| OTLP 回归 | 32/32 |
| 文件日志与轮转 | 20/20 |
| 产品行为、协议、认证与管理接口 | 111/111 |
| 迁移工具 | 10/10 |
| 双进程共享限流与故障恢复 | 12/12 |
| 真实 Kubernetes Gateway | 73/73 |
| 真实 Kubernetes Ingress 策略与治理 | 70/70 |
| Ingress 基础、Lease、TLS 与滚动升级 | 46/46 |

`cargo fmt`、`cargo clippy --all-targets --locked -j 2 -- -D warnings`、Helm lint、带共享限流 Secret 的 scoped Gateway Chart 渲染、Python 语法与 `git diff --check` 均通过。

完整 `scripts/check.sh` 在最后的 Gateway 鉴权拒绝保护前通过；增加该保护后重跑 Rust/Clippy/build，并用最终镜像完整重跑 Gateway 与 Ingress 策略套件。Ingress 基础套件使用前一镜像 `rgnix:p1-verified-20260925`，其后生产代码变化仅为 Gateway 策略失效时的拒绝路由。没有把所有检查描述为同一二进制上的一次运行。

Gateway 检查覆盖逐副本插件恢复、ConfigMap UID 变化、检查点删除、JWT Secret 撤销、鉴权身份头、路由共享速率、命名空间共享速率、租户隔离、审批/重启/回退、HTTP 200 下的业务指标失败、后端 CA 轮换/撤销/主机名、前端 TLS、gRPC 流和滚动更新。Ingress 也验证了业务指标触发回退及逐副本重启恢复，原有具名身份、审计管理、准入和真实 OpenTelemetry Collector 行为通过回归。

测试夹具补齐了重跑时 TLS 服务重载新证书、业务测试域名授权和异步发布等待；记录中的结果来自修正后的成功运行。

## 最终产物

- 本地镜像：`rgnix:p1-complete-20260925`
- 镜像索引：`sha256:632a5276f4d9a4a9a7641cee1d3ceb691d8d1c30b87a33feb0e5efeeb4c160b0`
- 镜像内二进制：`f3976ee667f787fe876f42d46d5a0d8b5e1b1b895238d7461abb2386529aa428`
- Rust 源码与依赖清单摘要：`4a2319389e6fe275fb981ebffb5fd6bbb22776fcd783099d8f7f6396fc0acb89`，计算范围见 JSON。

测试主 namespace 为 `rgnix-p1-qa-20260925`、`rgnix-p1-ingress-20260925`；各自的 `-peer` / `-tenant` namespace 也由夹具创建。资源保留供检查，包含故意无效或撤销的配置，不能直接作为生产示例。结束时两个部署共四个控制器 Pod 均就绪。

复现核心命令：

```sh
CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh
python3 scripts/gateway_e2e.py --namespace YOUR_GATEWAY_QA_NAMESPACE \
  --image rgnix:YOUR_TAG --output .local/gateway-results.json
RGNIX_IMAGE_TAG=YOUR_TAG bash scripts/ingress-e2e.sh YOUR_INGRESS_QA_NAMESPACE orbstack
RGNIX_IMAGE_TAG=YOUR_TAG python3 scripts/product_kubernetes.py YOUR_INGRESS_QA_NAMESPACE orbstack
```

Linux 测试需要 `redis-server` / `redis-cli`；CI 依赖安装已补齐。共享限流测试自行启动隔离 Redis 进程并清理，不依赖系统 Redis 服务。

## 使用与边界

配置见 [Gateway](gateway-api.md)、[共享限流](shared-rate-limits.md)、[业务指标与发布治理](governance.md)。共享速率需要所有副本连接同一协调器和 scope；本地并发、CPU/内存保护继续按进程执行。Gateway 检查点异步持久化，尚未持久化的更新不能保证崩溃恢复。

本轮没有执行原生 amd64 运行验证、上游 Gateway conformance 套件、多节点故障或长时间压力测试，也未验证 Redis TLS 服务端、Sentinel/Cluster 自动故障转移。没有远端 Actions、签名镜像推送或 GitHub Release 发布。

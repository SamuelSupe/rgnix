# 运维指标扩展验证 · 2026-09-25

本次针对 v0.2.0 之后的工作区源码验证，未改写 GitHub v0.2.0 发布附件。基线提交 `538ef3d59bae45766b0b2c3aa0fdf176949ac899`。

## 构建与回归

在 OrbStack Ubuntu 执行 Rust 1.90.0 `cargo build/test/clippy --locked -j 2`，Clippy 使用 `--all-targets -- -D warnings`；`cargo fmt --check` 通过。Linux arm64 Debug 二进制 SHA256 为 `95c701b36ab45b270f2b87d6eb13d07a0f804899409ab6e5c64b8212e210efb7`。

| 验证 | 结果 |
|---|---:|
| Rust 单元测试 | 7/7 |
| HTTP/流式传输/插件/预算 `scripts/integration.py` | 82/82 |
| 产品行为 `scripts/product_features.py` | 95/95 |
| Kubernetes API 恢复夹具 `scripts/ingress_recovery.py` | 57/57 |
| OTLP `scripts/otlp_integration.py` | 32/32 |
| 日志轮转 `scripts/log_rotation.py` | 20/20 |

新增行为断言覆盖：请求体前缀预读后完整转发只计一次输入字节，进行中请求及插件/后端租约占用、结束释放、主动健康检查状态、上游连接复用和响应头/完成计数、Linux RSS/FD/线程、配置更新时间、删除后端 Gauge 消失但历史 Counter 保留、watch 错误与重新同步、Lease 争用和成功选主。

使用 `prom/prometheus:v3.5.0` 的 `promtool check metrics` 分别检查独立服务与真实 Ingress 的输出，通过；`promtool check rules examples/prometheus-alerts.yaml` 验证 9 条规则通过。Helm lint 通过，开启/关闭 metrics Service、启用带 selector labels 的 ServiceMonitor 均完成模板校验；关闭 Service 却启用 ServiceMonitor 会明确报错。

独立服务样本为 62 个指标族、811 个样本、66,158 字节；带租户及灰度门禁的 Ingress 样本为 69 个指标族、255 个样本、26,238 字节。这是夹具的实际暴露量，不是总指标上限或性能基线。

## 真实 Kubernetes 与 Prometheus

在 OrbStack `rgnix-metrics-qa-20260925` 独立命名空间部署两个控制器副本、HTTP 上游和 Prometheus，使用限定 namespace 的 watch/RBAC。验证完成 9/9：

1. Helm 创建独立 ClusterIP metrics Service，业务 Service 不增加管理端口。
2. 请求阻塞期间命名空间占用为 1、配额为 2，进程在途为 1；完成后释放为 0。
3. 灰度审批等待状态及外部门禁通过/检查时间可见。
4. 12 次并发抓取均保留完整的后端及租户状态序列。
5. Prometheus Kubernetes 服务发现分别抓取两个 Pod，两个 instance 的 `up` 均为 1。
6. 9 条运维告警在 Prometheus 中加载且评估健康。
7. 上游缩容至 0 后，抓取到的可用端点总数归零，请求返回 503。
8. 上游恢复至 1 副本后，两控制器的两个配置后端均恢复一个可用端点，请求返回 200。
9. 删除 Ingress 后，两个副本的后端及 rollout Gauge 序列从 Prometheus 即时查询中消失。

测试通过现有 Dockerfile 的 Rust 1.98 构建 Linux arm64 release 镜像 `rgnix:metrics-20260925`。镜像 ID 为 `sha256:f3174b8c353b9f020a77e16eb0296ebb68e115f7a53d22c642f9e102a8738925`，两个 Pod 的 imageID 均对应此摘要。镜像内二进制 SHA256 为 `1fb2f0f6e8beac2a6cc04d70f3944199f01527ca225e0a4a7b09e21c763b023f`。

二进制相关源码摘要为 `9d28f76e58f5c87bbf561515a2c4baa911f5eb674548ad52b53ac5f2012822e3`：将 Cargo.toml、Cargo.lock、src/、vendor/ 下文件按 Python Path 排序，逐项拼接相对路径、NUL、文件内容、NUL 后计算 SHA256。[机器可读结果](validation/metrics-2026-09-25.json)保留检查名称和构建标识。

## 验证边界

- 本机未安装 Prometheus Operator：ServiceMonitor 经过 Helm 渲染校验，未验证 Operator 的实际发现链路；双副本抓取使用真正的 Prometheus Kubernetes 服务发现完成。
- 未进行 Linux amd64、非 Linux 进程采集验证，也未进行高基数或长时压力基准。
- 字节统计在请求结束入账，连接获取直方图只记录成功获取；watch 最近事件时间不是存活心跳，完整语义见[指标文档](metrics.md)。
- 专用 QA 命名空间保留；测试 Ingress 已删除，上游已恢复。未修改其他测试或业务命名空间。

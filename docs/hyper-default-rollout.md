# Hyper 默认内核与验收状态

当前开发版本包含 Hyper 和 Pingora，按用户要求默认运行 Hyper，Chart 的默认选择也为 Hyper。显式选择 Pingora 可回退；`--no-default-features` 构建只包含 Pingora，默认使用 Pingora。此次默认切换不等于全部发行资格通过：24 小时长测尚未通过，串行负载复现共享 Redis 限流路径的 503，根因仍需定位。下表继续作为验收要求，历史 debug 测试及短时间 benchmark 不能代替这些证据。

| 门槛 | 必须取得的证据 |
|---|---|
| 内核选择与回退 | CLI、环境变量、Helm 一致；两种内核真实启动；旧 true/false 参数兼容；`check` 与 `serve` 拒绝相同的不兼容配置 |
| 发行产物 | 原生 Linux amd64/arm64 在 Bookworm 构建；最终 release 二进制跑完整行为及 Hyper 协议检查；镜像内二进制摘要与已测产物一致；两种内核在最终镜像中启动 |
| Kubernetes | 最终镜像跑 Ingress/Gateway 主流程；24 小时混合负载，持续发布插件、轮换 TLS 和替换 Pod；检查请求错误、内存、CPU、描述符、连接、许可和尾延迟趋势 |
| 性能资格 | 同一个发行二进制运行 Hyper/Pingora，交错比较；分别做同内核 A/A 校准。记录差异与波动，校准失败不宣称稳定提升或 NGINX 性能持平 |

CI 的 `artifact` 作业在两种原生架构上运行 `scripts/validate_artifact.sh`；`image` 作业把该产物装入 Bookworm 镜像，再运行 `scripts/image_smoke.py`。镜像摘要检查防止测试的二进制与部署的二进制不同。Release 作业同样测试最终 release 二进制。

## 运维选择

```sh
rgnix check -c nginx.conf
rgnix serve -c nginx.conf
rgnix serve -c nginx.conf --engine pingora
helm upgrade --install rgnix charts/rgnix --set image.repository=YOUR_REPOSITORY --set image.tag=YOUR_TESTED_TAG
helm upgrade rgnix charts/rgnix --reuse-values --set engine=pingora
```

Chart 的 `engine: null` 选择 Hyper；旧 `experimentalHyper.enabled: true/false` 仍固定选择对应内核，`engine` 非空时优先。Chart 同时设置新旧环境变量。Hyper 部署必须使用当前开发版本构建的镜像；已经发布的 v0.5.0 没有 Hyper，仍使用 v0.5.0 镜像时必须设置 `experimentalHyper.enabled=false`，并保持 `engine=null`，以免向旧二进制传递新 CLI 参数。切换开发镜像后，可用 `engine=pingora` 回退。现有 v0.5.0 二进制、镜像与 OCI Chart 不因源码默认值变更而改变。

`rgnix_engine_info{engine="hyper"|"pingora"} 1` 表示实际运行内核。回退需要重新启动或滚动部署；HTTP/3、CONNECT 等 Hyper 专属配置需要同时调整，配置不兼容时明确失败。

连接治理使用 socket 来源 IP。共享代理或 NAT 集中流量时，管理员应按可信来源流量调大 `--hyper-max-connections-per-ip`、`--hyper-max-handshakes-per-ip`，保留进程和监听器的总预算。性能脚本针对同一个可信 loopback 压测来源显式调整这两个上限，并将完整启动参数写入报告；产品默认保护不因此改变。

## 长时间验证

在专用命名空间运行：

```sh
python3 scripts/gateway_e2e.py --context YOUR_CONTEXT \
  --namespace rgnix-hyper-default-soak --image YOUR_TESTED_IMAGE \
  --engine hyper --require-multiple-nodes \
  --soak-seconds 86400 --soak-event-seconds 600 --scale-routes 100 \
  --output .local/hyper-default-soak.json
```

测试每 30 秒把各 Pod 的指标和实际 image ID 写入 `.samples.jsonl`；负载进程把请求数、错误和尾延迟写入 `.load.jsonl`。六个 worker 分别运行 HTTP、TLS 和 RGL 前缀 body 路由，合计最高 240 请求/秒；日志与 span 使用 OTLP 输出。每十分钟发布插件，首次及每六次发布同时轮换 TLS、替换一个就绪 Pod。重连不会重放失败的请求；任何请求错误导致该负载检查失败。

吞吐对照与稳定性测试串行运行，避免压测抢占同一宿主机资源影响限流依赖。负载在首个请求错误后结束，并保留完整错误记录；修复或调整验证环境后，新的 24 小时计时与旧失败记录分别保存。

延迟窗口最多保留每 worker 最近 3000 个成功请求，不能称为整个 24 小时的精确 p99。一份副本持续运行整个测试，禁止消失或重启；仅替换另一副本，避免重启掩盖长期增长。趋势须按 Pod 生命周期检查：RSS、描述符、连接或占用许可持续增长需要调查。该负载不代表生产容量，也不包含 24 小时 HTTP/2、HTTP/3 或跨物理主机故障验证；短时协议检查单独记录。

Rust 与 Chart 默认值已按用户明确要求切换。默认变化后的产物须重新验证默认启动和 Pingora 回退；旧二进制与镜像的证据不能作为新产物的证明。运行中或失败的 24 小时测试不能记为通过，未完成资格保持公开记录。

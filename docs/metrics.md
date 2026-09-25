# 运维指标

v0.3.0 扩展了管理端口的 Prometheus `/metrics`，并提供 Helm metrics Service、ServiceMonitor 与告警规则。

## 接入

独立服务默认在 `127.0.0.1:9090` 提供 `/metrics`，可通过 `--admin` 修改。下面是与服务同机运行的 Prometheus 配置；规则文件使用仓库的 [prometheus-alerts.yaml](../examples/prometheus-alerts.yaml)：

```yaml
global:
  scrape_interval: 30s
  evaluation_interval: 30s
rule_files:
  - /etc/prometheus/rgnix-alerts.yaml
scrape_configs:
  - job_name: rgnix
    static_configs:
      - targets: ["127.0.0.1:9090"]
```

Helm 默认创建 `<release>-metrics` ClusterIP Service，9090 端口指向 Pod 的管理端口。业务 LoadBalancer Service 仍只提供 HTTP/HTTPS。已有 Prometheus Operator 时启用：

```sh
helm upgrade --install rgnix charts/rgnix -n rgnix --create-namespace \
  --set image.repository=YOUR_REGISTRY/rgnix --set image.tag=YOUR_BUILD \
  --set metrics.serviceMonitor.enabled=true \
  --set metrics.serviceMonitor.labels.release=YOUR_PROMETHEUS_RELEASE
```

`labels` 必须匹配 Prometheus 的 `serviceMonitorSelector`，Prometheus 的 `serviceMonitorNamespaceSelector` 也必须选中此命名空间。默认抓取间隔 30s、超时 10s，可设置 `metrics.serviceMonitor.interval/scrapeTimeout`；超时不得大于间隔。需要预先安装 Operator 和 ServiceMonitor CRD，Chart 不负责安装它们。详见 [ServiceMonitor API](https://prometheus-operator.dev/docs/api-reference/api/#servicemonitor)。

没有 Operator 时，可在 Prometheus 使用 Kubernetes 服务发现抓取每个 Pod：

```yaml
scrape_configs:
  - job_name: rgnix
    kubernetes_sd_configs:
      - role: endpoints
        namespaces:
          names: [rgnix]
    relabel_configs:
      - source_labels: [__meta_kubernetes_service_label_app_kubernetes_io_name, __meta_kubernetes_service_label_app_kubernetes_io_component, __meta_kubernetes_endpoint_port_name]
        action: keep
        regex: rgnix;metrics;metrics
      - source_labels: [__meta_kubernetes_namespace]
        target_label: namespace
      - source_labels: [__meta_kubernetes_pod_name]
        target_label: pod
```

Prometheus 的 ServiceAccount 需要对应命名空间的 Service、Endpoints、Pod `get/list/watch` 权限。不能只把多副本 Service 的 ClusterIP 当成一个静态目标，否则轮询会混合不同 Pod 的计数器。可设置 `metrics.enabled=false` 关闭 metrics Service；这不关闭进程内 `/metrics`。

指标与健康接口无需 token；`/v1/*` 仍按管理员身份鉴权。metrics Service 指向同一管理监听端口，应仅允许管理和采集网络访问。通过 OpenTelemetry Collector 的 Prometheus receiver 也可以把这些指标转发到第三方；内置 OTLP exporter 目前只推送日志和 traces，不直接推送 metrics。

## 指标目录

Counter 在进程重启时清零，使用 `rate()` 或 `increase()`；Gauge 是当前值。直方图提供 `_bucket`、`_sum`、`_count`，多副本分位数先聚合 bucket 再计算。以下目录包含新增和已有的主要运维指标；可选功能的序列在功能启用或发生事件后出现。

| 范围 | 指标 | 含义与标签 |
|---|---|---|
| 实例 | `rgnix_build_info{version,mode,arch,os}` | 恒为 1；`mode` 为 standalone/ingress |
| 可用性 | `rgnix_healthy`、`rgnix_ready` | 与 `/healthz`、`/readyz` 对应，1 正常/就绪；目标无法抓取请使用 Prometheus 的 `up` |
| 流量 | `rgnix_requests_total{status}`、`rgnix_request_seconds` | 完成请求数、完整请求耗时 |
| 路由 | `rgnix_route_requests_total{route,status_class}`、`rgnix_route_request_seconds{route}` | 按配置路由统计；不使用客户端原始路径 |
| gRPC | `rgnix_grpc_requests_total{route,grpc_status}` | RPC 完成状态 |
| 字节 | `rgnix_request_body_bytes_total`、`rgnix_response_body_bytes_total` | 下游实际读取/发送的 body 字节，在请求结束时入账；不含 HTTP 头、TLS 开销；响应压缩后计数 |
| 失败 | `rgnix_request_failures_total{source,reason}` | 代理错误，包含响应头发送后的失败；`source` 为 upstream/downstream/internal；原因采用固定分类，见下文 |
| 上游结果 | `rgnix_backend_requests_total{backend,result}`、`rgnix_upstream_errors_total` | 配置后端请求结果、上游错误总数 |
| 连接获取 | `rgnix_upstream_connect_seconds{backend,reused}` | 成功拿到上游连接的耗时，包括建连/TLS 或连接池复用；`reused=true/false`，失败连接不进入此直方图 |
| 响应头 | `rgnix_upstream_header_seconds{backend}` | 从选定上游到最终响应头的时间，含上传请求体；忽略 100/103 等临时响应，101 计一次 |
| 上游耗时 | `rgnix_upstream_request_seconds{backend}` | 从选定上游到整次请求结束，包含流式传输、下游背压和失败 |
| 端点 | `rgnix_backend_endpoints{backend,state}` | 当前端点总量、可选数、被动摘除数、主动检查未就绪数；`state=total/eligible/ejected/unready`，后两类可重叠 |
| 后端占用 | `rgnix_backend_inflight{backend}`、`rgnix_backend_inflight_limit{backend}` | 持有后端租约的请求数、后端并发上限；上限 0 表示不限 |
| DNS | `rgnix_backend_dns_age_seconds{backend}` | 域名上游上次成功 DNS 解析距今秒数，含初始配置解析；IP 上游不产生该序列 |
| 故障隔离 | `rgnix_upstream_ejections_total` | 连续传输失败引起的端点摘除次数 |
| 进程预算 | `rgnix_budget_in_use{budget}`、`rgnix_budget_limit{budget}` | 当前占用和容量，`budget=inflight/plugin/mirror/simulation`；请求槽不等于 TCP 连接数 |
| 拒绝 | `rgnix_rejected_requests_total{budget}` | 请求、插件、路由速率/并发限制导致的拒绝 |
| 租户预算 | `rgnix_namespace_budget_in_use{tenant_namespace,resource}`、`rgnix_namespace_budget_limit{tenant_namespace,resource}` | 命名空间 request/plugin/auth/mirror 占用及配额，按进程计数，包含仍被运行快照保留的租户状态 |
| 共享限流 | `rgnix_global_rate_limit_total{result}`、`rgnix_global_rate_limit_seconds{result}` | 共享准入、429、容量 503 及三种故障降级结果；固定标签，详见[跨副本限流](shared-rate-limits.md) |
| 租户拒绝 | `rgnix_namespace_rejections_total{namespace,resource}` | 命名空间 request/rate/plugin/auth/mirror 配额拒绝；保持原有 namespace 标签，遇抓取目标同名标签时由 Prometheus 改为 exported_namespace |
| 限流器 | `rgnix_limiter_entries{tenant_namespace}`、`rgnix_limiter_capacity{tenant_namespace}` | 已分配的速率/并发限流键数与容量；独立模式为 `_standalone` |
| 插件 | `rgnix_plugin_calls_total`、`rgnix_plugin_errors_total`、`rgnix_plugin_seconds` | 钩子执行次数、trap/非法决策数、执行耗时；请求阶段含实例化 |
| Body 路由 | `rgnix_body_inspection_seconds{mode,result}`、`rgnix_body_inspected_bytes_total{mode}` | full/prefix 预读的耗时、结果和成功提供给插件的字节数；`result=complete/truncated/timeout/too_large/error` |
| 快照 | `rgnix_config_version`、`rgnix_config_info{sha256}`、`rgnix_config_resources{kind}` | 本进程版本、当前内容摘要、监听/host/路由/后端/证书/脚本路由数量 |
| 更新 | `rgnix_reloads_total`、`rgnix_reload_errors_total`、`rgnix_config_last_success_timestamp_seconds` | 发布数、拒绝更新数、最近发布 Unix 时间；独立启动也设置成功时间 |
| 更新耗时 | `rgnix_config_update_seconds{source,result}` | SIGHUP 文件更新或 Ingress 重建与发布耗时；`source=file/ingress`，`result=success/error`；不含管理员回退和离线 preflight |
| 配置诊断 | `rgnix_config_diagnostics` | 当前 Ingress 诊断数量；保留旧路由并发布有效部分属于成功发布，仍需观察该值和 reload_errors |
| 控制策略 | `rgnix_control_reloads_total`、`rgnix_control_reload_errors_total` | 管理员/租户等控制文件更新成功/失败 |
| 恢复 | `rgnix_checkpoint_healthy`、`rgnix_checkpoint_errors_total` | 最近持久化状态和累计失败 |
| Watch | `rgnix_ingress_watch_streams{kind,state}` | 配置的、完成初始同步的、最近事件正常的 watch 数；`state=configured/synchronized/healthy`，多命名空间时可以大于 1 |
| Watch 事件 | `rgnix_ingress_watch_events_total{kind,event}`、`rgnix_ingress_watch_errors_total{kind}` | apply/delete/init/init_apply/init_done 事件数、watch 错误数 |
| Watch 时间 | `rgnix_ingress_watch_last_event_timestamp_seconds{kind}` | 最近成功事件时间，**不是心跳**；安静的集群可以长期没有事件 |
| 控制器资源 | `rgnix_ingress_cached_resources{kind}`、`rgnix_ingress_selected_resources` | 依赖过滤前的 watch 缓存资源数、选定 class 的 Ingress 数 |
| 选主 | `rgnix_ingress_leader`、`rgnix_ingress_lease_attempts_total{result}` | 最近确认的本地 Lease 是否仍在有效期内；结果 acquired/contended/error/timeout；用于观测，不替代 Lease 授权检查 |
| 状态回写 | `rgnix_report_errors_total` | Event/status API 失败或超时 |
| 发布流程 | `rgnix_rollout_stage{owner}`、`rgnix_rollout_state{owner,state}` | 当前 Ingress / Gateway Route 的 0 起始阶段（Gateway owner 加资源 kind 前缀），以及 paused/promoted/rolled_back/approval_pending 标志 |
| 发布回退 | `rgnix_traffic_rollbacks_total`、`rgnix_mirror_requests_total{result}` | 自动回退次数、镜像结果 |
| 外部指标门禁 | `rgnix_metric_gate_passed{gate}`、`rgnix_metric_gate_checked_timestamp_seconds{gate}` | 当前控制配置下的门禁结果、上次检查时间；未检查为 0，通过结果超过 60 秒不再算通过 |
| 证书 | `rgnix_certificate_expiry_timestamp_seconds{listener,host}`、`rgnix_certificate_valid{listener,host}` | TLS 证书到期时间、时间及域名校验结果 |
| 标签容量 | `rgnix_metric_labels`、`rgnix_metric_label_limit`、`rgnix_metric_label_overflow_total{kind}`、`rgnix_metrics_omitted{kind}` | 历史标签使用量/上限、合并到 overflow 的观测次数、当前状态省略对象数 |
| Linux 进程 | `process_cpu_seconds_total`、`process_resident_memory_bytes`、`process_virtual_memory_bytes`、`process_open_fds`、`process_max_fds`、`process_threads`、`process_start_time_seconds` | 由 `/proc` 采集本进程 CPU 累计秒、RSS/虚拟内存、FD、线程数及启动时间；非 Linux 不提供 |
| 日志/OTLP | `rgnix_*logs_dropped_total`、`rgnix_log_*`、`rgnix_otlp_*` | 队列、轮转、导出重试、失败、丢弃；完整字段见[文件日志](log-rotation.md#缓冲终止与观测)、[OTLP 日志](otlp.md)和[链路](tracing.md) |

`reason` 固定为 `connect_timeout/tls_timeout/read_timeout/write_timeout/read/write/closed/request_timeout/body_limit/http_status/other`。它描述代理错误类别；业务返回的 4xx/5xx 应查看 HTTP 状态指标。客户端断开后可能仍已发送 200 响应头，不能仅依靠 HTTP 状态发现这类失败。

所有延迟与请求计数都以请求/阶段完成为界；SSE/WebSocket 长连接未结束时看预算占用，不能把其未入账的流量当成零流量。进程 CPU/RSS 不代表容器资源配额，容器限制、OOM、重启次数仍使用 kube-state-metrics/cAdvisor 等集群指标。

## 基数和抓取行为

路由/后端/命名空间历史计数标签共享 2048 个配置身份上限，超出合并为 `_overflow` 并增加溢出计数。路径、查询参数、客户端 IP、请求头值、body、token 和错误字符串均不作为指标标签。配置中的 host/namespace/owner 等仍会出现在对应运维指标中。

当前后端、租户、发布流程指标各最多暴露 2048 个对象，超出数量记录到 `rgnix_metrics_omitted`；证书延续现有 2048 个条目上限；外部门禁受配置的 128 项上限约束。当前状态在每次抓取时读取一个快照，删除的后端/发布对象立即消失，历史 Counter 保留至进程重启。租户状态可能随旧请求或发布历史保留。不同指标族之间不是事务快照，负载变化时允许微小时间差。

采集不发起 DNS、健康检查或 Kubernetes API 请求，也不改变路由、端点健康状态或发布阶段。并行抓取使用各自的当前状态样本，避免相互清空 GaugeVec。基数限制控制单个进程内存；Prometheus 长期存储还应配置目标标签、保留周期和采样预算，参见 [Prometheus instrumentation practices](https://prometheus.io/docs/practices/instrumentation/)。

`backend` 是配置解析后的键，例如 `app`、`http://app` 或包含 TLS 策略摘要的键；多个配置别名可能共享同一后端状态。按键关联请求与健康指标，避免把所有别名的端点数或在途数相加作为物理总量。仅配置命名空间配额的租户才产生 namespace budget 指标。

## 查询与告警

```promql
# 每秒完成请求数
sum(rate(rgnix_requests_total[5m]))

# 按路由聚合多个副本的 P95 秒数
histogram_quantile(0.95, sum by (le, route) (rate(rgnix_route_request_seconds_bucket[5m])))

# 每个 Pod/进程的在途请求占用比例
rgnix_budget_in_use{budget="inflight"} / rgnix_budget_limit{budget="inflight"}

# 每个后端成功连接获取中的连接池复用比例
sum by (job, instance, backend) (rate(rgnix_upstream_connect_seconds_count{reused="true"}[5m]))
/ sum by (job, instance, backend) (rate(rgnix_upstream_connect_seconds_count[5m]))

# 每秒 CPU 秒数，1 表示平均占用一个 CPU 核
rate(process_cpu_seconds_total{job="rgnix"}[5m])

# 最早到期的证书还剩多少天
(min(rgnix_certificate_expiry_timestamp_seconds) - time()) / 86400
```

[告警规则](../examples/prometheus-alerts.yaml)覆盖未就绪、5xx 比例、预算压力、空后端、更新拒绝、watch 故障、证书到期、日志丢弃和指标省略。阈值是起点，需按 SLO、发布窗口和证书周期调整；另配置 `up{job="rgnix"} == 0` 等采集不可达告警。不要以“最后 watch 事件很旧”单独判定控制器故障。

```sh
promtool check rules examples/prometheus-alerts.yaml
curl -fsS http://127.0.0.1:9090/metrics | promtool check metrics
```

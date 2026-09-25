# OTLP 访问日志

v0.2.0 起提供此能力。`serve` 与 `ingress` 可以将访问日志发送到支持 **OTLP/HTTP protobuf** 的第三方平台或 OpenTelemetry Collector。默认关闭；原有文件/stdout 日志继续输出。

## 独立服务

```sh
export OTEL_SERVICE_NAME=rgnix-edge
export OTEL_RESOURCE_ATTRIBUTES='deployment.environment.name=production,service.namespace=edge'
export OTEL_EXPORTER_OTLP_LOGS_ENDPOINT='https://YOUR_PLATFORM/v1/logs'
# 按平台要求设置认证方式，从环境或 Secret 注入。
export OTEL_EXPORTER_OTLP_LOGS_HEADERS='Authorization=Bearer%20YOUR_TOKEN'

rgnix serve -c nginx.conf
```

也可使用 `--otlp-logs-endpoint http://127.0.0.1:4318/v1/logs`。这是**完整日志 URL**，按原样使用，支持平台自定义路径和查询参数。CLI 优先于环境变量；错误输出不打印端点 URL、认证头或采集端返回正文。

若设置通用 `OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4318/tenant`，自动追加 `/v1/logs`，最终为 `/tenant/v1/logs`。日志专用 endpoint 优先。没有 endpoint 时不创建导出线程。

日志专用 `OTEL_EXPORTER_OTLP_LOGS_HEADERS`、`_PROTOCOL`、`_TIMEOUT`、`_CERTIFICATE` 分别覆盖对应通用 `OTEL_EXPORTER_OTLP_*`，不合并两套 headers。Headers 使用逗号分隔 `key=value`；空格、逗号、等号可按 `%20`、`%2C`、`%3D` 转义。支持 Bearer、Basic 或平台 API Key 等普通 HTTP 认证头；禁止覆盖 Host、Content-Length 和传输编码等字段。

HTTPS 默认验证系统 CA 和主机名；使用私有 CA 时设置 `OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE=/path/ca-bundle.pem`，将 PEM 证书加入信任。没有跳过验证的开关，也不自动跟随重定向。导出客户端直接连接 endpoint，不读取 HTTP_PROXY/HTTPS_PROXY；如需网络代理、gRPC、压缩、mTLS、厂商专用认证或多目的地输出，可通过 Collector 转发。

## Kubernetes / Helm

```yaml
# values-otlp.yaml
otlpLogs:
  endpoint: https://YOUR_PLATFORM/v1/logs
  serviceName: rgnix-ingress
  resourceAttributes: deployment.environment.name=production,service.namespace=edge
  headersSecret:
    name: otlp-auth
    key: headers
  # 可选：同命名空间 ConfigMap 中的 PEM CA。
  caConfigMap:
    name: ""
    key: ca.crt
```

在控制器命名空间准备 Secret `otlp-auth`，其 `headers` 键内容是上述 headers 字符串。可通过 Secret 管理系统创建，避免将 token 放在 Helm values 或命令参数中。然后安装自己构建的新镜像：

```sh
helm upgrade --install rgnix charts/rgnix \
  --namespace rgnix-system --create-namespace \
  --set image.repository=YOUR_REGISTRY/rgnix \
  --set image.tag=YOUR_BUILD_TAG \
  -f values-otlp.yaml
```

Chart 自动附加 `k8s.namespace.name` 与 `k8s.pod.name`。每个副本导出自己处理的请求，Lease 选主不影响导出。自定义部署也可以直接设置下表环境变量。exporter 参数、认证、CA、Secret 环境变量变化需要重启 Pod；独立模式 SIGHUP 热更新路由、`access_log` 开关及[本地轮转策略](log-rotation.md)，不重建 OTLP exporter。

## 字段与开关

在请求结束时生成 OTLP LogRecord，Resource 包含 `service.name`（默认 `rgnix`）、`service.version` 和自定义属性，InstrumentationScope 为 `rgnix.access`。时间为请求完成时的 Unix 纳秒；Body 固定为 `HTTP access`。

| 日志属性 | 含义 |
|---|---|
| `http.request.method` | 原请求方法 |
| `url.path` / `url.scheme` | 原请求路径，不含 query；HTTP/HTTPS |
| `server.address` / `client.address` | 请求主机名、直连客户端 IP；不信任 X-Forwarded-For |
| `network.protocol.name` / `.version` | HTTP 和客户端协议版本 |
| `http.response.status_code` / `.body.size` | 状态码、已发送响应体字节数 |
| `rgnix.request.duration_ms` | 从请求上下文建立到完成的毫秒耗时 |
| `rgnix.route.id` | 命中的配置路由标识 |
| `rgnix.upstream.name` / `.address` | 选定后端与实际连接地址；本地响应不包含 |
| `rgnix.config.version` / `.sha256` | 该请求持有的配置版本与摘要 |
| `rgnix.error.source` | 有传输/处理错误时为 upstream/downstream/internal |
| `rgnix.parent_span_id` / `rgnix.upstream.span_id` | 开启 traces 后的父 span 与主后端 client span ID；没有对应 span 时省略 |

成功请求 INFO、HTTP 4xx WARN、HTTP 5xx 或处理错误 ERROR。导出与本地访问日志相同的请求，包括路由前拒绝和未匹配请求（使用 http 级策略）；独立管理端口不导出。`access_log off` 同时关闭该路由的本地与 OTLP 日志，并保留原有继承规则。只需要 OTLP 时，独立模式可使用 `access_log /dev/null;`。

有效的 W3C traceparent 提供 trace_id/span_id 关联；显式启用 OTLP traces 后，日志关联代理 server span，后端请求带 client span 上下文及有效的 tracestate。无有效父级时创建新根 trace。LogRecord 的原生 trace_id/span_id/flags 支持平台日志与链路关联。trace 导出拥有独立采样和有界队列，访问日志 off 不关闭 traces，见[链路与日志关联](tracing.md)。

OTLP 日志不导出请求/响应 Body、query、任意 HTTP headers、Cookie、Authorization、Referer 或 User-Agent。路径本身和 IP 仍可能包含业务数据。字符串按 UTF-8 边界截断：路径最多 4096 字节，其余字段 1024 字节。截断只影响日志。

## 缓冲、失败与终止

| 环境变量 | 默认值 | 范围/说明 |
|---|---:|---|
| `OTEL_SERVICE_NAME` | rgnix | 最多 1024 字节，覆盖资源中的 service.name |
| `OTEL_RESOURCE_ATTRIBUTES` | 空 | 逗号分隔 key=value；最多 32 项、输入总长 16 KiB，值最多 1024 字节 |
| `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` | http/protobuf | 其他协议明确拒绝启动 |
| `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | 5000 | 一批导出的**总预算**，含重试等待，1–30000 毫秒 |
| `OTEL_BLRP_MAX_QUEUE_SIZE` | 2048 | 1–16384 条；溢出丢弃新记录 |
| `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` | 256 | 1–512 条，不得超过队列容量 |
| `OTEL_BLRP_SCHEDULE_DELAY` | 1000 | 一批从首条开始的最大等待，1–60000 毫秒 |
| `OTEL_LOGS_EXPORTER` | otlp（有 endpoint 时） | none 关闭 |
| `OTEL_SDK_DISABLED` | false | true 关闭 OTLP 日志和 traces 导出 |

日志与 traces exporter 可分别或同时启用，使用独立队列。指标通过 Prometheus `/metrics` 暴露。支持上述环境变量子集，不实现所有 OpenTelemetry SDK 配置。

请求线程只构造有上限的记录并尝试入队，不等待网络。独立线程批量发送；单条最多 16 KiB 编码负载、单批最多 1 MiB，接收响应最多 64 KiB。队列容量限制记录数，Rust 对象开销另计。Collector 不可用不会影响健康/就绪状态。

网络错误及 HTTP 429/502/503/504 最多重试两次，使用指数退避与随机抖动，并遵守 Retry-After 和总预算。其他状态、非法响应、部分成功不重试；部分拒收数量单独计入丢弃。认证失败等永久错误丢弃当前批次，后续批次仍尝试发送。确认响应丢失时可能产生重复日志。

SIGTERM 先让 Pingora 排空请求，再给导出队列最多 **5 秒** 刷新时间；超时记录剩余丢弃数。SIGKILL、进程崩溃、队列满或持续导出故障可丢失日志。本功能是内存缓冲的尽力投递，不是持久化审计日志；如需可靠存储或长时间断网缓冲，使用具备持久队列的 Collector。

启用时 `/metrics` 提供以下无请求路径标签的指标：

| 指标 | 含义 |
|---|---|
| `rgnix_otlp_logs_exported_total` | 采集端确认接受的记录数 |
| `rgnix_otlp_logs_dropped_total{reason}` | queue_full / export_failed / remote_rejected / shutdown / record_too_large |
| `rgnix_otlp_logs_export_errors_total` | 失败尝试和总预算超时 |
| `rgnix_otlp_logs_retries_total` | 实际重试次数 |
| `rgnix_otlp_logs_partial_success_total` | 部分成功或带警告的响应数 |
| `rgnix_otlp_logs_pending` | 队列与正在发送的记录总数 |

## 接入检查

平台 endpoint 必须接收 OTLP **logs**，不能用 traces/metrics URL 或普通 JSON 日志接口代替。先确认 `rgnix_otlp_logs_exported_total` 增长，再在平台按 `service.name` 搜索。这个计数表示接收方确认接受；平台的后续处理、保留和索引仍由平台决定。

协议依据：[OTLP HTTP 请求与响应](https://opentelemetry.io/docs/specs/otlp/#otlphttp)、[Exporter 环境变量和 URL 规则](https://opentelemetry.io/docs/specs/otel/protocol/exporter/)。实际验证与边界见 [OTLP 验证记录](validation-otlp.md)。

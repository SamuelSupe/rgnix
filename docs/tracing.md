# 分布式链路与日志关联

`serve` 与 `ingress` 均可同时导出 OTLP logs 和 traces。请求产生 server span，实际后端调用产生 client span；外部鉴权和已发出的流量镜像各有独立 client span。接收的 W3C `traceparent` 决定 trace ID 和父 span，转发请求携带本次 client span 的 `traceparent`，有效的 `tracestate` 同时传递。

本页描述 v0.3.0。该版本在基础 traces 上补充 `tracestate`、严格头校验、鉴权/镜像子 span 和扩展日志关联字段。

## 同时开启日志与 span

```sh
export OTEL_SERVICE_NAME=rgnix-edge
export OTEL_RESOURCE_ATTRIBUTES='deployment.environment.name=production,service.namespace=edge'

rgnix serve -c nginx.conf \
  --otlp-logs-endpoint http://collector:4318/v1/logs \
  --otlp-traces-endpoint http://collector:4318/v1/traces \
  --trace-sample-ratio 0.1
```

两个 endpoint 分别接收官方 `ExportLogsServiceRequest` 和 `ExportTraceServiceRequest` protobuf 消息。使用 **OTLP/HTTP protobuf 推送**；`/metrics` 仍用于 Prometheus 抓取。配置值是接收方的完整 URL，rgnix 本身不开放 OTLP 接收端口。需要 gRPC 或厂商协议时，通过 Collector 转发。

“同时导出”采用独立、有界的后台队列，不在业务请求内同步等待观测平台，也不保证 logs/traces 同时到达。日志与 span 通过 ID 关联。

| 配置 | 默认值与语义 |
|---|---|
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `--otlp-traces-endpoint` | 完整 traces URL；CLI 优先 |
| `RGNIX_TRACE_SAMPLE_RATIO` / `--trace-sample-ratio` | 0.1；范围 0..1，仅决定没有有效父上下文的根请求 |
| `OTEL_TRACES_EXPORTER` | 显式 traces endpoint 时启用；`none` 禁用；`otlp` 可搭配通用 endpoint 启用 |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | 通用基地址，启用 traces 后追加 `/v1/traces`；单独设置不会开启 traces |
| `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | `key=value` 逗号分隔；例如 `Authorization=Bearer%20TOKEN` |
| `OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE` | 额外信任的 PEM CA 文件；仍验证主机名 |
| `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | `http/protobuf`；其他协议拒绝启动 |
| `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` | 5000 ms；范围 1..30000，是一批导出含重试等待的总预算 |
| `OTEL_BSP_MAX_QUEUE_SIZE` | 2048；范围 1..16384，按 span 数量计算 |
| `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` | 256；范围 1..512，不超过队列容量 |
| `OTEL_BSP_SCHEDULE_DELAY` | 1000 ms；范围 1..60000，从首条 span 开始等待 |
| `OTEL_SDK_DISABLED=true` | 关闭 OTLP logs 和 traces；本地日志仍遵循配置 |

TRACES 专用的 HEADERS/CERTIFICATE/PROTOCOL/TIMEOUT 分别覆盖对应通用 OTLP 变量，headers 不合并。`OTEL_SERVICE_NAME`、`OTEL_RESOURCE_ATTRIBUTES` 同时作用于 logs/traces。认证、TLS、重试和持久缓冲边界与[日志 exporter](otlp.md)相同；修改 exporter 参数需要重启，SIGHUP 不重建 exporter。

## 父子关系与传播

```text
调用方 span
└─ rgnix server span                 ← 本次访问日志 span_id
   ├─ auth client span              → 外部鉴权服务
   ├─ proxy client span             → 业务后端或下一级 rgnix
   │  └─ 下一级 server span
   └─ mirror client span            → 镜像后端
```

- 同一个请求链使用相同 trace ID，每次已启用的 server/client span 有独立、非零 span ID。鉴权重试每次产生独立子 span。镜像是异步调用，可能晚于 server span 结束。
- 有效父级的 sampled 位优先：传入 `01` 时即使本地 ratio 为 0 也采样；传入 `00` 时即使本地 ratio 为 1 也不导出 span。未采样请求仍创建和传递上下文，访问日志仍可记录其 ID 和 `trace_sampled=false`。
- 缺失或非法 `traceparent`、重复 `traceparent`、零 ID、非法十六进制会创建新根上下文，并清除孤立 `tracestate`。version `00` 必须为规定长度；未来版本按兼容规则读取已知字段，向下游输出 version `00`。
- 多个 `tracestate` 头按顺序合并；非法成员、重复 key 或超过 32 个成员会舍弃 state，但保留合法 parent。传播上限为 512 字节，超长时先去除大于 128 字节的完整成员，再从尾部移除完整成员，不截断 vendor 值。
- 开启 traces 时，在配置/RGL 请求头修改之后注入最终上下文，防止 span 与实际下游请求脱节。关闭 traces 时不创建代理 span，主代理请求保留现有转发行为；有效传入 parent 仍可用于访问日志关联。
- 不注入响应 `traceparent`，不自动生成或解析 `baggage`、B3、Jaeger 私有传播头。既有普通头转发规则仍适用。

例如向代理发送：

```sh
curl -H 'Host: example.test' \
  -H 'traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' \
  -H 'tracestate: example=release7' \
  http://127.0.0.1:8080/
```

接收平台中的 server span 应继续 trace `4bf92f3577b34da6a3ce929d0e0e4736`，父 ID 为 `00f067aa0ba902b7`。后端收到同一 trace ID、新的 client span ID 和 `example=release7`。未启用 instrumentation 的业务后端也能收到这些头，但它需要自己的 SDK 才会继续生成业务 span。

## 日志内容

本地 combined/JSON 访问日志记录以下关联字段，可配合已有[文件轮转](log-rotation.md)：

| 字段 | 含义 |
|---|---|
| `trace_id` | 32 位小写十六进制 trace ID |
| `span_id` | 本代理 server span ID；未启用 traces 时为有效传入 parent ID |
| `parent_span_id` | 本代理 server span 的父 ID；根请求或未启用时为 `-` |
| `upstream_span_id` | 主后端 client span ID；没有主后端 span 时为 `-` |
| `trace_sampled` | 是否采样，JSON 布尔值 |

```nginx
http {
    access_log /var/log/rgnix/access.log json;
    rgnix_log_rotation size=100m interval=1d keep=7 gzip=on;
    # 添加实际的 upstream/server/location 配置，并预先创建可写目录。
}
```

如果设置了 `rgnix_log_fields` / Ingress `rgnix.io/log-fields` 白名单，需要将所需的新字段加入名单。请求失败的错误日志也携带 `trace_id` 和 `span_id`。

OTLP LogRecord 使用原生 `trace_id`、`span_id` 和采样 flags，另有 `rgnix.parent_span_id`、`rgnix.upstream.span_id` 属性。`access_log off` 关闭访问日志，独立开启的 traces 仍继续导出。完整 span 通过 OTLP traces 发送，本地访问日志保存上述关联字段。

## Span 字段与运行边界

span 包含纳秒起止时间、kind、父 ID、`trace_state`、父上下文 remote flags、status，以及请求方法、响应状态、配置路由/后端标识。server 的 `http.route` 和 span 名称使用配置中的路由模式，client 的 `rgnix.upstream.role` 为 `proxy`、`auth` 或 `mirror`。未知 HTTP 方法记为 `_OTHER` 并保留 `http.request.method_original`。

HTTP 5xx 和传输错误标为 ERROR；HTTP 4xx 对 server 为 UNSET、对实际收到该状态的 client 为 ERROR。gRPC 非零状态标为 ERROR。错误使用有限类别或状态码的 `error.type`，不导出底层错误文本。健康请求为 UNSET。管理端口、后台健康探测、Kubernetes 控制器 API 请求不产生本次业务 span。

span 不采集 Body、query、Cookie、Authorization 或任意业务请求头。`tracestate` 是明确传播并导出的例外；应由调用方避免在 vendor state 中放入敏感数据。长连接 span 在请求结束时导出，进行中的 SSE/WebSocket 不会提前出现完整 span。

单条编码最多 16 KiB，单批最多 1 MiB，响应最多 64 KiB；队列满丢弃新 span，导出故障不阻塞业务。关闭时每个 exporter 最多等待 5 秒刷新。队列不持久化，两个信号可能独立丢失；如需磁盘缓冲，由 Collector 承担。

`/metrics` 暴露 `rgnix_otlp_traces_exported_total`、`dropped_total{reason}`、`export_errors_total`、`retries_total`、`partial_success_total`、`pending`（后五项同样带 `rgnix_otlp_traces_` 前缀）。先检查接收计数，再按 trace ID 在平台查询。具体 SaaS 平台的索引、查询和保留策略由平台配置决定。

## Ingress / Helm

```yaml
otlpLogs:
  endpoint: https://collector.observability.svc:4318/v1/logs
  serviceName: rgnix-ingress
  resourceAttributes: deployment.environment.name=production
  headersSecret:
    name: otlp-auth
    key: headers
  caConfigMap:
    name: otlp-ca
    key: ca.crt
otlpTraces:
  endpoint: https://collector.observability.svc:4318/v1/traces
  sampleRatio: 0.1
  headersSecret:
    name: otlp-auth
    key: headers
  caConfigMap:
    name: otlp-ca
    key: ca.crt
```

Secret/ConfigMap 位于控制器命名空间。使用系统信任的证书或不需要认证时，省略对应条目。两个信号共用 `otlpLogs.serviceName/resourceAttributes`，并自动附带 Pod 和 namespace 资源属性。两个副本独立导出，与 Lease 是否当选无关。JSON stdout 访问日志可设置 Ingress 注解 `rgnix.io/log-format: json`。

协议参考：[W3C Trace Context](https://www.w3.org/TR/trace-context/)、[OTLP](https://opentelemetry.io/docs/specs/otlp/)、[HTTP span 状态约定](https://opentelemetry.io/docs/specs/semconv/http/http-spans/)。实际验证见[链路验证记录](validation-tracing-2026-09-25.md)。

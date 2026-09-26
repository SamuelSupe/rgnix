# 多租户、灰度与变更管理

本轮能力复用现有不可变快照和流式代理。完整字段、默认值及边界见[参数参考](platform-policies.md)。

## 修复的产品问题

- HTTPS 主动健康检查与业务请求使用一致的 SNI、CA、客户端证书；不同 TLS 策略有独立的健康状态与连接池。
- 从响应头和 trailers 提取 `grpc-status`，在 `rgnix_grpc_requests_total` 中区分 RPC 结果；错误会标记 server/client span。
- Ingress 外部认证使用就绪端点池，故障时在总认证超时内最多尝试三个端点。业务请求仍不自动重试。
- IP、路由限流在认证前执行；依赖请求头、Cookie、JWT claim 的限流在认证后执行。
- `proxy_ssl_certificate` 与 `proxy_ssl_certificate_key` 支持上游 mTLS；Ingress 使用同命名空间的 `rgnix.io/upstream-client-secret`。
- 本地访问日志默认脱敏常见凭证参数，包含 URI 和 Referer 中的编码参数名；支持 JSON、字段选择、关闭 query/Referer/客户端地址。

```nginx
access_log /var/log/rgnix/access.jsonl json;
rgnix_log_query off;
rgnix_log_client off;
rgnix_log_referer off;
rgnix_log_redact token access_token api_key password secret;
rgnix_log_fields timestamp method uri status bytes route backend trace_id;
```

`rgnix_log_redact` 替换默认敏感参数列表；`rgnix_log_fields` 只影响 JSON 字段集合。字段选择不是自动发现任意敏感信息。OTLP 日志继续不输出 query、请求体和凭证头；关闭客户端地址也对 OTLP 生效。

## 管理员命名空间配额

通过 `--tenant-policy-file` 加载 JSON，示例为 [tenant-policy.json](../examples/tenant-policy.json)。Helm 使用 `tenancy.policyConfigMap.name/key` 挂载文件。应用 Ingress 注解不能覆盖该文件；策略文件每秒检查，完整校验成功后自动生效。域名授权和限定 namespace 的 watch/RBAC 见[治理操作指南](governance.md)。

管理员可以限制：

- 配置数量：Ingress、路由、后端引用，以及插件权限与源码大小。
- 编译预算：`max_compilations_per_minute`（默认 60，范围 1..600），按副本限制该命名空间的新脚本编译；缓存命中不扣额度。
- 应用参数：请求体大小和超时上限；应用配置无限请求体时仍受管理员上限约束。
- 运行资源：请求、插件、外部认证、镜像的并发预算，命名空间总请求速率，以及限流键表容量。

每个命名空间独立计数和分配限流表。耗尽一个命名空间的预算不会占用其他命名空间的独立额度。拒绝的配置产生 `NamespaceQuota` / `NamespaceDenied` Event，`/v1/routes` 展示生效上限和活动数量。

省略 `default` 时，只接纳 `namespaces` 显式列出的命名空间。显式命名空间条目替换 default 条目，缺失字段使用内置默认值。请求和插件并发上限必须小于进程总上限。可选 [Redis 协调器](shared-rate-limits.md) 支持跨副本请求速率预算；并发及资源预算仍按进程计数，需要独立 CPU/内存或更强信任隔离时，应使用不同控制器部署、IngressClass 和 Kubernetes 资源限制。

Kubernetes 插件 JIT 在独立后台工作线程执行，不持有编译缓存锁。FIFO 队列最多 128 项，每个命名空间最多 2 项排队或执行；更新受限时沿用有效插件，并继续发布当前端点、证书及删除。预检最多等待编译结果 3 秒，繁忙时需要重试。源码/指令预算仍适用；这些是进程内编译调度与额度，不是操作系统级的 CPU/内存硬隔离。

编译完成会主动唤醒控制器。新副本在已有持久化插件仍等待编译恢复时保持未就绪，避免把流量引向尚未恢复的路由；运行中的副本不会因新候选脚本排队而整体退出服务。Ingress 将正常排队单独报告为 `PendingCompilation`，不会作为 `InvalidPlugin` 事件统计。

分阶段发布的推进使用 Pod Lease 汇总当前控制器副本的候选样本，按资源 UID、策略摘要、阶段与开始时间隔离，不能复用上一阶段或重建资源的统计。每 5 秒刷新观测；缺失、未就绪、过期或阶段不一致的副本会暂停推进。观测有效年龄最多为 20 秒和策略 window_seconds 中较小者；窗口短于上报间隔时可能等待下一次新鲜观测。单 Pod 最多上报 1024 个分阶段发布、192 KiB，超过容量的发布不能自动推进。错误率和 p95 阈值从合并计数判断，不平均各副本的百分位。局部自动回退继续即时保护流量，阶段推进仅由 leader 持久化。分阶段发布自动启用此观察，不依赖 reportReplicas 开关；Chart 始终授予必要的 Pod/Lease 读取权限。

## 声明式灰度、镜像和回退

[Ingress 示例](../examples/ingress-rollout.yaml) 使用 `rgnix.io/traffic-policy`：

```json
{
  "revision": "checkout-v2",
  "backends": [
    { "service": "checkout-stable:http", "weight": 90 },
    { "service": "checkout-canary:http", "weight": 10 }
  ],
  "mirror": {
    "service": "checkout-shadow:http",
    "percent": 5,
    "max_body_bytes": 65536,
    "timeout_ms": 500
  },
  "rollback": {
    "fallback": "checkout-stable:http",
    "min_requests": 100,
    "error_percent": 5,
    "window_seconds": 60,
    "max_p95_ms": 500
  }
}
```

Service 必须在同一命名空间，端口可以用名称或数字。普通权重在各副本本地调度，可配置 `cohort` 保持跨副本分组稳定。RGL 显式 `route.proxy` 可覆盖正常分流；一旦回退，稳定后端优先于脚本选择。被选后端故障不会触发业务请求重放。分阶段权重、审批、暂停/恢复及外部指标门禁见[治理操作指南](governance.md)。

镜像发送真实的第二份请求，适用于能接收副本请求的影子服务。主请求流式传输，同时在限额内收集镜像体；只有完整请求体不超限时才发送。超限、WebSocket 和 gRPC 流会跳过镜像，不发送残缺请求。镜像有独立并发和超时预算，不等待镜像结果才响应主请求；状态见 `rgnix_mirror_requests_total`。

自动回退只评估非 fallback 目标的完成请求：上游传输故障、HTTP 5xx、非零或缺失的 gRPC 状态计为失败；客户端取消和镜像不参与。满足最小样本量后，错误率达到阈值或 p95 超过阈值，就锁定到稳定后端。每个副本最多保存时间窗内的 10,000 个样本。

触发副本立即切换新请求，并回写 `rgnix.io/rolled-back-revision`；其他副本通过 watch 同步，新副本也恢复该状态。Kubernetes API 不可用时，其他副本的同步会延迟，本地回退仍保持。更新为新的 `revision` 才重新开启发布。Event、管理诊断和 `rgnix_traffic_rollbacks_total` 提供回退记录。

## 管理权限、模拟和持久化

- `--admin-token-file`：写入角色，可执行回退。
- `--admin-read-token-file`：只读角色，可查看诊断和进行隔离模拟，回退返回 403。
- `--admin-users-file`：具名身份、reader/writer 角色和 namespace 范围。身份与旧令牌文件均自动热更新；移除身份会撤销后续请求权限。
- `--admin-audit-file`：JSON 操作审计，默认 stderr，不记录令牌或请求体；审计输出不可用时，管理操作在执行前失败。
- `--history-dir`：独立模式持久化历史，保存当前版本和最多八个历史版本。

`POST /v1/simulate` 与 CLI `simulate` 支持真实 IP/CIDR、证书链、JWT、认证身份头、请求体限制、认证前后限流和 RGL 判断，使用独立预算，不调用业务后端。`repeat` 可构造请求批次，`hold_permits` 模拟并发持有。外部认证必须提供 `external_auth` fixture，缺失时返回 424；只有 CLI 显式指定 `--live-auth` 才访问真实认证服务。

模拟结果还包含最终后端、URI、脱敏后的转发头、灰度状态和镜像资格。`rgnix diff`、`POST /v1/validate` 和 `/v1/validate-ingress` 用于候选变更预检；可选 TLS Admission Webhook 在 Kubernetes 持久化之前执行相同的 Ingress 语义校验。

```sh
rgnix serve -c /etc/rgnix/nginx.conf \
  --history-dir /var/lib/rgnix/history \
  --admin-token-file /run/secrets/admin-write \
  --admin-read-token-file /run/secrets/admin-read \
  --admin-audit-file /var/log/rgnix/audit.jsonl
```

历史包含展开后的配置、插件、本地 JWKS、证书和私钥；目录权限为 0700，文件为 0600，原子替换后 fsync。静态网站内容和远程 JWKS 不归档。单个版本最多 4 MiB 配置和 16 MiB 本地依赖；历史集合最多 8 MiB 配置和 32 MiB 解码后依赖，超过预算会淘汰较早版本。该目录应作为密钥材料保护和备份。

重启恢复最后提交的版本；SIGHUP 显式重新导入源文件。`POST /v1/rollback/VERSION` 校验并发布一个新的递增版本，不改写管理员源文件。Ingress 继续通过修改 Kubernetes 源资源回退，保留 Service、端点和 Secret 撤销语义。

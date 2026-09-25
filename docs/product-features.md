# 入口策略、认证、调度与诊断

这些功能同时使用现有不可变路由快照、编译式 RGL 和流式代理。独立模式通过 SIGHUP 发布配置；Ingress 的应用策略和脚本一起保存有效版本。默认不启用认证、限流、主动健康检查、响应压缩或 trace 导出；自动请求重试仍关闭。

## 可信代理与访问控制

```nginx
set_real_ip_from 10.0.0.0/8;
set_real_ip_from 2001:db8:100::/48;
real_ip_header X-Forwarded-For;
real_ip_recursive on;
allow 203.0.113.0/24;
deny all;
```

以上指令支持 http/server/location，并继承到内层。当前层出现 `set_real_ip_from` 时替换整组可信地址；出现 allow/deny 时替换整组访问规则。allow/deny 按声明顺序首次匹配，未匹配默认允许；使用 `deny all` 实现白名单。单组最多 256 条。

默认没有可信代理，`real_ip_header` 默认 `X-Real-IP`，recursive 默认 off。只有 socket 对端属于可信 CIDR 才接受转发地址。支持 X-Real-IP、X-Forwarded-For、Forwarded 的 `for=` 字段及 `proxy_protocol`；也可指定一个包含 IP 地址的普通头。recursive on 从右向左跳过可信节点，选择最近的不可信地址。格式错误、超过 8 KiB 或 64 跳的整条链被忽略，使用 socket 对端。

`$remote_addr` 和 `req.remote_addr()` 使用解析后的地址，`$realip_remote_addr` 保留 socket 地址。`$scheme` 仅接受可信 socket 对端提供的单值 `X-Forwarded-Proto: http|https`。Forwarded 的 proto 参数不用于 scheme。访问日志、IP 限流和 CIDR 规则使用相同身份。

```nginx
server {
    listen 0.0.0.0:8443 ssl proxy_protocol;
    set_real_ip_from 10.20.0.0/16;
    real_ip_header proxy_protocol;
    ssl_certificate server.crt;
    ssl_certificate_key server.key;
}
```

PROXY v1/v2 在 HTTP/TLS 之前读取，必须显式启用且声明可信发送方。支持 TCP IPv4/IPv6，UNKNOWN/LOCAL 保留 socket 地址；不接受 UDP/Unix 地址。头读取总超时 2s，v1 最长 108 字节，v2 负载最多 512 字节；TLV 消费但不提供身份或 SSL 权限。开启后该监听上的每条连接都必须带 PROXY 头。同地址 server 的 PROXY 开关及可信列表必须一致；修改需要重启。

Ingress 可信代理属于控制器级配置，租户不能通过注解扩大信任范围：

```sh
rgnix ingress --ingress-class rgnix --publish-service rgnix-system/rgnix \
  --trusted-proxy 10.20.0.0/16 --real-ip-header x-forwarded-for --real-ip-recursive
```

启用 PROXY 时增加 `--proxy-protocol --real-ip-header proxy_protocol`。Helm 使用 `forwarding.*`；需要保留直连客户端地址时可配置 `service.externalTrafficPolicy: Local`，具体效果取决于集群负载均衡器。

## 流量预算

```nginx
rgnix_limit_rate 100 burst=200 key=ip;
rgnix_limit_conn 20 key=header:x-tenant;
```

可选 [Redis 协调器](shared-rate-limits.md) 将速率限制扩展到多个副本；并发限制仍按进程执行。两条指令支持 H/S/L、覆盖继承，`off` 禁用。rate 是每秒请求数，使用令牌桶，burst 默认等于 rate。conn 限制同时进行的请求/流，不是 TCP socket 数。key 支持 `route`、`ip`、`header:NAME`、`cookie:NAME`、`jwt:CLAIM`，默认 ip。缺失 key 归入同一空值桶。速率、burst 和并发值均为 1..1,000,000。

配额按路由、策略和 key 隔离，**每个进程独立计数**；两个副本不是一个分布式配额。使用 header/cookie 作为租户标识时，标识的真实性由应用或外部认证决定；JWT key 使用已验证的 claim。认证在预算与插件前执行，插件修改不会追溯改变预算 key。

速率超额返回 429；并发/状态容量超额返回 503，并带 `Retry-After: 1`。不排队。状态表最多 16,384 项，只淘汰空闲至少 60s 且没有活动请求的项；流结束或客户端取消会释放并发许可。全局 `--max-inflight` 与 `--max-plugin-instances` 继续生效。

## 上游协议、调度和健康

```nginx
upstream application {
    server 10.0.0.11:8080 weight=3;
    server 10.0.0.12:8080;
    least_conn;
    rgnix_max_inflight 200;
}
location / {
    proxy_pass https://application;
    proxy_http_version 2;
    proxy_ssl_name backend.example.com;
    proxy_ssl_trusted_certificate backend-ca.pem;
}
```

`proxy_http_version 1.1|2|auto` 支持 H/S/L，默认 1.1。2 为严格 HTTP/2；明文上游使用 prior-knowledge h2c，TLS 上游通过 ALPN。auto 在 TLS 上协商 H2/H1，明文使用 H1。gRPC 使用 2，传递消息流、状态 trailer 和双向流；不自动转换 HTTP/JSON 到 gRPC。

`proxy_ssl_name` 是固定验证名/SNI，无变量；缺省取 URL 主机/命名 upstream 名。`proxy_ssl_trusted_certificate` 为 PEM CA 文件，覆盖继承，加载时解析。证书和主机名验证始终开启，`proxy_ssl_verify` 和 `proxy_ssl_server_name` 只接受 on。连接池按 CA 和协议隔离，CA 变化不会复用旧信任连接。

| upstream 指令 | 行为 |
|---|---|
| 缺省 / `rgnix_balance round_robin` | 加权轮询，保留固定地址声明顺序 |
| `least_conn` / `rgnix_balance least_conn` | 按权重比较活动请求数，平局轮转 |
| `ip_hash` | 对解析后的客户端 IP 做稳定哈希 |
| `rgnix_balance hash KEY` | 加权 rendezvous 哈希；KEY 同流量预算选择器 |
| `rgnix_balance sticky COOKIE` | 自动签发 32 位随机十六进制 Cookie，后续稳定选择后端；没有服务端会话表 |
| `rgnix_max_inflight N` | 后端总活动请求预算，默认 0 不单独限制，最大 1,000,000；超额 503 |
| `rgnix_health_check PATH [interval=10s] [timeout=1s] [status=200]` | GET 探测；interval 1s..1h、timeout 1ms..30s，期望状态 200..399 |

黏性 Cookie 使用 Path=/、Max-Age=86400、HttpOnly、SameSite=Lax；HTTPS 增加 Secure。不同服务应使用不同 Cookie 名。后端摘除时哈希选择另一个健康节点，恢复后可能返回原节点；不是业务会话复制或永久后端绑定。

主动检查默认关闭；启用后新节点在首次成功探测前不可选，之后按每次结果摘除/恢复。探测沿用路由的协议、SNI、CA 和客户端证书；不同 TLS 设置使用独立健康状态。探测为 HTTP GET，不是 gRPC Health Checking RPC。被动传输故障隔离同时生效，当前业务请求不重试。

域名在初次加载时使用系统解析，运行期间在控制面按 DNS TTL 刷新 A/AAAA；刷新间隔限制为 1s..1h。固定 IP 不做 DNS 请求。每次查询最多等待 2s，失败后约 5s 重试，最多保留上次成功答案 60s，随后撤下过期地址。地址替换不取消已经选定旧地址的请求。Kubernetes Service 后端完全使用 EndpointSlice，不依赖这套 DNS 刷新。

## JWT、外部认证及客户端证书

```nginx
rgnix_jwt jwks=keys.json issuer=https://issuer.example.com audience=my-api algorithm=RS256;
# jwks 也可为受系统 CA 信任的 HTTPS URL
rgnix_auth_request http://auth.internal/check timeout=2s headers=x-user,x-tenant;
```

H/S/L 覆盖继承，`off` 禁用。JWT 接受 Bearer token，固定算法 RS256/ES256，验证签名、issuer、audience、exp、nbf，允许 5s 时钟偏差。拒绝缺失/重复 Authorization、未知 kid、错误算法和过期令牌。令牌最多 16 KiB；JWKS 最多 1 MiB/64 个 key，RSA 限定约 2048..8192 位。普通文件在 SIGHUP 重载；远程 JWKS 约每 30s 刷新，单次总超时 3s，禁止重定向和环境代理，刷新失败保留旧 key 最多 5 分钟，之后认证失败。没有按请求 kid 动态联网获取密钥的能力。

`req.claim("tenant")` 返回已验证的标量 claim；缺失或对象/数组为 nil，数值/布尔转为字符串。最多 64 个 claim，单项值 1 KiB，计入插件宿主预算。认证拒绝 401，RGL 可据 claim 决定业务路由或进一步返回 403。

外部认证使用 GET，不发送请求体。发送 Authorization/Cookie 及由代理生成的 X-Original-Method、X-Original-URI、X-Original-Host、X-Real-IP。2xx 允许，401/403 拒绝，其他响应/超时/断连返回 503。响应体不参与决策。只转发配置列出的最多 16 个 `x-` 身份响应头，禁止转发地址头；先移除客户端伪造的同名头，单值最多 4 KiB。不跟随重定向，不做自动重试。Ingress 外部认证只允许同命名空间 Service 引用。

```nginx
ssl_client_certificate client-ca.pem;
ssl_verify_client on; # optional 或 off
```

客户端证书设置只在 H/S，默认 off。on 要求证书，optional 允许匿名但验证已提供证书。每个 SNI 主机可配置不同 CA。新握手使用当前 CA，每个新请求也按当前路由 CA 校验证书链，因此 CA 撤销会影响已复用 TLS 连接上的后续请求；已经处理中的请求保持原快照。HTTP 明文请求无法满足 on。

## 网站静态内容与压缩

```nginx
location /app/ {
    alias /srv/frontend/;
    try_files $uri $uri/ /index.html;
    gzip on;
    brotli on;
}
```

alias 只在 location，固定路径替换匹配前缀，支持目录和精确文件；仍使用目录能力限制，不能通过符号链接逃逸。root 会取消继承的 alias。`try_files` 支持 2..8 个 `$uri`、`$uri/` 或固定绝对路径候选，最后一项为固定路径或 `=400..599`。fallback 在当前 root/alias 内读取，不重新匹配 location，不支持命名路由、query、任意变量或内部重定向循环。这是用于静态资源和 SPA 的 NGINX 子集。

gzip/brotli 支持 H/S/L，默认 off。`gzip_comp_level` / `brotli_comp_level` 1..9，on 默认分别 1/4；当前实现设置 level 会启用相应算法。`gzip_min_length` 默认 1024，`gzip_types` 对两种算法共同生效，始终包含 text/html；默认另含 text/plain、text/css、application/javascript、application/json、image/svg+xml。

只变换 200、允许 MIME、未压缩、满足最小已知长度的响应。HEAD、Range/206、请求或响应的 no-transform、gRPC、SSE 不压缩；即使显式把 gRPC/SSE 加入 MIME 表也不会变换。协商遵守质量权重、q=0 和通配排除。未知长度允许流式压缩；调整 Content-Length/ETag/Vary，保持背压。不增加整响应缓存或磁盘缓冲。

## 诊断与回退

```sh
rgnix dump -c nginx.conf
rgnix explain -c nginx.conf --listener 127.0.0.1:8080 --host app.example.com --path /api/x
rgnix simulate -c nginx.conf --request request.json
rgnix serve -c nginx.conf --admin-token-file admin.token
```

simulate 接受 listener、method、host、path、headers、client、body，以及客户端证书、外部认证 fixture、repeat 和 hold_permits。它执行身份、认证、限流和 RGL 判断，使用隔离预算，不访问业务上游。CLI --live-auth 可以显式访问认证服务；管理接口只使用 fixture。详见[平台策略](platform-policies.zh-CN.md)。

`--admin-token-file`/`RGNIX_ADMIN_TOKEN_FILE` 开启以下受 Bearer token 保护的接口，token 为 32..4096 个可打印非空白字符，修改 token 文件自动热更新。`--admin-users-file` 支持具名角色与 namespace 范围，见[治理操作指南](governance.md)：

| 方法、路径 | 用途 |
|---|---|
| GET `/v1/config` | 生效配置摘要、路由、后端、证书可用性 |
| GET `/v1/routes`、`/v1/backends` | 路由设置、实时端点健康与活动数 |
| GET `/v1/explain?listener=...&host=...&path=...` | 规范化路径及选中的路由，不执行插件 |
| GET `/v1/history` | 当前/前 8 个本进程版本和最近 16 次文件更新错误 |
| POST `/v1/simulate` | 认证、限流和插件模拟，不消耗线上预算 |
| POST `/v1/rollback/VERSION` | 独立模式发布保留版本，生成新的单调递增版本号 |

诊断屏蔽配置中的请求/响应头值和认证 URL query，不输出私钥/令牌；响应最多 8 MiB。健康、就绪、metrics 保持原有独立端口行为。管理 HTTP 端口应只在受控网络或端口转发中访问。

默认回退保存在内存；--history-dir 启用包含证书、插件等依赖的持久化版本，重启恢复最后提交版本。下一次 SIGHUP 仍加载当前文件。Ingress 版本由 Kubernetes 源资源管理，接口返回 409 并要求修改源资源；不会把历史快照里的已删除 Service、端点或 Secret 恢复回来。

## Ingress 应用注解

注解前缀均为 `rgnix.io/`，作用于该 Ingress 的所有路径。独立应用应使用独立 Ingress。

| 名称 | 值 / 默认 |
|---|---|
| `client-max-body-size` | size，默认 1m，0 不限制，最大 1 TiB（用字节或 k/m/g 表示） |
| `proxy-connect-timeout`、`proxy-read-timeout`、`proxy-send-timeout` | 默认 60s，正数且最多 24h |
| `keepalive-timeout` | 默认 75s，允许 0，最多 24h |
| `access-log` | on/off，默认 on；同时控制本地访问日志与 OTLP logs |
| `limit-rate`、`limit-conn` | 与上面的指令参数相同；默认关闭 |
| `allow-cidrs` | 逗号分隔 CIDR，未列入的地址拒绝 |
| `backend-protocol` | HTTP（默认）、HTTPS、GRPC、GRPCS |
| `upstream-http-version` | 1.1/2/auto；GRPC(S) 必须为 2 |
| `upstream-server-name`、`upstream-ca-secret` | 验证名称，以及同 namespace Secret 的 ca.crt |
| `balance` | round_robin、least_conn、`hash KEY`、`sticky COOKIE` |
| `backend-max-inflight`、`health-path` | 后端预算、GET 探测路径；缺省无单独预算/无主动探测 |
| `compression` | off、gzip、br、`gzip br` |
| `jwt-secret`、`jwt-issuer`、`jwt-audience`、`jwt-algorithm` | 同 namespace Secret 的 jwks.json；算法默认 RS256 |
| `auth-service`、`auth-timeout`、`auth-response-headers` | `name:port/path`（命名/数字端口）、默认 3s、逗号分隔身份头 |
| `client-ca-secret`、`verify-client` | 同 namespace Secret 的 ca.crt；默认 on，可设 optional |
| `script`、`request-body`、`request-body-timeout` | 保留已有 RGL 和请求体读取能力 |

策略会生成有效版本检查点，即使没有自定义插件。策略非法时保留对应 Ingress 上次有效设置，新副本可从检查点恢复；Secret/Service/端点撤销仍实时生效，认证或 CA 依赖不可用时禁用相关路由。检查点只保存源配置，不保存 Secret 内容。未知 `rgnix.io/` 注解拒绝，避免静默忽略拼写错误。

## 可观测性

`rgnix_route_requests_total{route,status_class}`、`rgnix_route_request_seconds{route}`、`rgnix_backend_requests_total{backend,result}` 使用配置中的路由/后端标识。所有历史标签总数最多 2048，超出归入 `_overflow`；不使用实际路径、任意 Host、用户或租户值。访问日志覆盖路由匹配前拒绝的请求和未匹配请求，使用 http 级访问日志策略。相关 RGL、预算、配置更新及文件/OTLP 队列指标保持有效。

启用代理 trace：

```sh
rgnix serve -c nginx.conf \
  --otlp-logs-endpoint https://collector.example.com/v1/logs \
  --otlp-traces-endpoint https://collector.example.com/v1/traces \
  --trace-sample-ratio 0.1
```

trace 支持 `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT/HEADERS/CERTIFICATE/TIMEOUT/PROTOCOL`；信号变量优先于通用 OTLP 变量。通过完整 traces endpoint 或 `OTEL_TRACES_EXPORTER=otlp` 显式开启，`OTEL_TRACES_EXPORTER=none` 禁用。使用 `OTEL_BSP_MAX_QUEUE_SIZE/MAX_EXPORT_BATCH_SIZE/SCHEDULE_DELAY` 配置有界批量导出，默认和访问日志队列相同，独立队列/失败计数，不阻塞请求。只有 http/protobuf；共享访问日志导出的 TLS、重试及部分拒绝处理，退出最多等待 5s。

父级 W3C traceparent 的采样位优先；无父级时按 ratio（0..1，默认 0.1）决定采样。每个请求包含 server span，主后端、外部鉴权及流量镜像各有独立 client span。向后端注入对应 client span 的 traceparent 和有效 tracestate，支持跨代理继续父子链。本地日志记录 trace_id/span_id、parent_span_id、upstream_span_id 和 trace_sampled；OTLP 日志以原生字段关联 server span。未启用 traces 时仍可关联有效传入上下文。trace 不采集 Body、query、Cookie、Authorization 或任意业务头；传播头校验、采样及日志语义见[链路指南](tracing.md)。

`rgnix_otlp_traces_exported_total/dropped_total/export_errors_total/retries_total/partial_success_total/pending` 与 logs 指标对应。访问日志 off 不关闭单独启用的 trace。Helm 提供 `otlpTraces.*`、`admin.tokenSecret` 和可信代理配置。

源码已加入预置数据面的 [Gateway API](gateway-api.md)、[迁移工具](migration.md)和[正式发行流水线](releases.md)。Gateway 预览支持 GatewayClass、Gateway、HTTPRoute、GRPCRoute 和 ReferenceGrant；尚未获得上游 conformance 认证，具体不支持的字段和进程内插件历史限制见兼容说明。

后续范围仍包括响应缓存、HTTP/3、完整 Lua/NGINX 兼容、正则和嵌套 location、rewrite/map/if、分布式限流，以及 Gateway 基础设施自动置备和其余策略能力。

命名空间配额、Service 灰度、镜像、自动回退、管理角色和日志字段策略参见[平台策略及变更管理](platform-policies.zh-CN.md)。

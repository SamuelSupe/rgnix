# 域名授权、发布流程与管理治理

这些能力包含在 v0.2.0 预览版中；实现阶段的实际运行范围见[治理验证记录](validation-governance.md)。

## 命名空间与域名边界

`--tenant-policy-file` 的管理员策略可以增加 `domains`：

```json
{
  "default": {"max_inflight":64,"max_plugins":4},
  "namespaces": {},
  "domains": {
    "checkout.example.test": ["payments"],
    "*.shop.example.test": ["shop"],
    "shared.example.test": ["payments","shop"],
    "": ["platform"]
  }
}
```

存在 `domains` 时，域名必须获得显式授权。空对象拒绝所有域名；空字符串授权无 host 路由及 defaultBackend。通配符只覆盖一层标签；相互覆盖的授权必须使用相同 namespace 集合。授权检查覆盖普通路径、精确路径和 TLS 名称，内层路径不能绕过。授权多个 namespace 才允许共享域名，具体路由冲突仍按创建时间、namespace、名称决定。

省略 `domains` 保留先到先得模式：最早接纳的 namespace 拥有域名，其他 namespace 无法用更长路径抢占；此模式不能防止域名首次被抢先登记。生产多租户部署应配置管理员域名授权。撤销授权会撤销相应路由，即使该 Ingress 同时存在无效插件更新。

Helm `watchNamespaces: [payments, shop]` 同时生成限定范围的 Role/RoleBinding 并设置重复的 `--watch-namespace` 参数。列表为空时保持集群范围。发布 Service 和检查点所在 namespace 额外需要 Service/ConfigMap watch；IngressClass 是集群资源。namespace 必须先存在，变更 watch 范围需要滚动更新。

可选 [Redis 协调器](shared-rate-limits.md) 将请求速率与命名空间总速率按 scope 跨副本计数；并发等资源配额仍按控制器进程计数。CPU、内存和全局预算属于进程共享资源；需要硬隔离时，使用独立 Deployment、IngressClass、namespace watch 列表与 Kubernetes requests/limits。

## 具名管理身份与热更新

`--admin-users-file /etc/rgnix/users.json` 或 Helm `admin.usersSecret.name/key`：

```json
{"users":[
  {"name":"payments-observer","role":"reader","namespaces":["payments"],"token_sha256":"REPLACE_WITH_64_HEX_SHA256"},
  {"name":"platform-operator","role":"writer","token_sha256":"REPLACE_WITH_ANOTHER_64_HEX_SHA256"}
]}
```

生成至少 32 字节的随机 bearer token，只将 SHA-256 写入文件；调用时使用原 token。身份名称和 token 摘要都必须唯一。最多 1024 个身份，文件上限 1 MiB。`namespaces` 省略代表全局权限，空列表代表无 namespace 权限。

reader 可查看本 namespace 的路由、后端、证书，进行解释、模拟及 Ingress 预检。writer 额外可控制其 namespace 内的灰度发布。独立模式文件预检与历史回退要求全局 writer；历史列表要求全局权限。旧的 `--admin-token-file` 和 `--admin-read-token-file` 保留全局 writer/reader 语义，可与具名身份并用。

令牌文件、身份文件、租户策略和指标 provider 文件每秒检查一次。作为一组完整校验后原子替换；错误文件保留整组上一有效版本，缺失文件不代表撤销。撤销身份应提交合法的 `{"users":[]}` 或删除对应用户。投影 Secret/ConfigMap 要等待 Kubernetes 更新挂载文件；更新后无需重启。正在执行的请求保留已接纳权限，后续请求使用新权限。

`/v1/config` 返回当前 `controls_sha256`、调用身份，以及本身份可见发布所用 `metric_gates` 的通过状态和检查年龄；不暴露 provider URL 或凭证。`rgnix_control_reloads_total` / `rgnix_control_reload_errors_total` 记录更新结果。操作审计包含具名 actor、操作、版本及结果，不记录 token、请求体或 provider 凭证。审计写入失败时不开始管理变更。`/healthz`、`/readyz`、`/metrics` 仍依赖管理端口的网络隔离，不按用户过滤。

## 分阶段灰度与稳定分组

`rgnix.io/traffic-policy` 增加 `cohort`、`steps`、`metric_gates`：

```json
{
  "revision":"checkout-v3",
  "backends":[{"service":"stable:http","weight":90},{"service":"candidate:http","weight":10}],
  "cohort":"cookie:release-user",
  "rollback":{"fallback":"stable:http","min_requests":100,"error_percent":5,"window_seconds":60,"max_p95_ms":500},
  "steps":[
    {"weights":{"stable:http":90,"candidate:http":10},"duration_seconds":60,"min_requests":100,"approval":true},
    {"weights":{"stable:http":50,"candidate:http":50},"duration_seconds":120,"min_requests":100,"approval":true},
    {"weights":{"stable:http":0,"candidate:http":100},"duration_seconds":120,"min_requests":100,"approval":false}
  ],
  "metric_gates":["checkout-error-ratio"]
}
```

每阶段权重必须覆盖 backends 中所有 Service，允许 0，总和必须大于 0。最多 16 阶段；阶段时长 0..86400 秒，`min_requests` 默认 20，上限 10000，允许 0（无流量样本门槛）。阶段发布要求配置 rollback。普通分流使用轮询；cohort 支持 `header:NAME`、`cookie:NAME`、`jwt:CLAIM`、`ip`，值缺失时使用客户端 IP。revision、分组值和固定后端顺序共同确定分组，同阶段不同副本选择一致。

RGL 可覆盖正常流量选择；已锁定的回退优先于 RGL，无法继续强制发往坏候选后端。镜像保持独立，镜像故障不触发主流量回退。

Lease 领导者约每 5 秒检查阶段时长、健康样本、审批和外部指标。所有条件通过后持久化 `rgnix.io/rollout-state`，各副本由 watch 采用权重。最终阶段完成观察后标记 `promoted`。阶段状态、暂停及审批跨重启恢复；请求样本是每副本的内存窗口，重启后重新累积。健康样本只包含该阶段完成的非 fallback 请求；跨副本业务指标应通过外部 provider 查询聚合结果。

```http
POST /v1/rollouts
Authorization: Bearer TOKEN
Content-Type: application/json

{"owner":"payments/checkout","revision":"checkout-v3","operation":"approve","stage":0}
```

操作支持 `pause`、`resume`、`approve`、`rollback`。审批必须带当前 stage，过期 revision、UID 或 stage 返回 409。暂停保留当前权重；恢复重新开始本阶段观察时钟。回退永久锁定该 revision，使用新 revision 才重新开启。自动与手动更新使用 resourceVersion，冲突后重新读取资源，不覆盖并发更新。

## 外部指标门禁

管理员通过 `--rollout-metrics-file` 或 Helm `rolloutMetrics.secret.name/key` 配置有名 provider。应用只能引用名称，不能指定任意 URL：

```json
{"gates":{"checkout-error-ratio":{
  "url":"https://prometheus.example.test/api/v1/query?query=checkout_error_ratio",
  "pointer":"/data/result/0/value/1",
  "max":0.01,
  "bearer_token":"ADMIN_MANAGED_CREDENTIAL"
}}}
```

支持 HTTP(S) JSON 数值或数值字符串，JSON pointer 定位数据，`min`/`max` 为包含边界。上限 128 个 provider、每发布 8 个 gate。后台每约 5 秒检查，单次 2 秒、响应 64 KiB、并发 8；HTTPS 使用系统信任，不跟随重定向、不使用环境代理。缺失、超时、解析失败或超过 60 秒的旧结果均阻止阶段推进，不当成通过。指标通过时仍需满足本地健康、时间与审批条件。默认外部门禁只阻止推进。当前源码可在 traffic-policy 中显式配置 `metric_rollback`，让业务指标连续失败触发稳定后端回退，即使候选请求持续返回 HTTP 200。该扩展尚未包含在已发布 v0.2.0 中。

```json
{
  "metric_gates": ["checkout-error-ratio"],
  "metric_rollback": {"consecutive_failures":3,"failure_seconds":15,"unavailable":"pause"}
}
```

要求已配置 rollback fallback；可以配合 steps，也可以单独保护普通分流。失败次数（1..120）与持续秒数（0..3600）必须同时满足；只计不同的、新鲜的观测。成功观测、阶段或控制配置变化会清空失败序列。`unavailable: pause` 为默认值，指标服务超时/错误只阻止推进；显式 `rollback` 则将这些不可用观测按失败计算。未知 provider 或过期结果不构成连续失败证据。单个 gate 满足门槛即回退。回退先作用于本地请求，再以 UID、完整策略和 resourceVersion 校验持久化 `rgnix.io/rolled-back-revision`，其他副本通过 watch 收敛，跨重启恢复；新 revision 才启动新发布。短暂传播窗口内副本可能不同步。Gateway 模式同样适用，管理命令可用 kind 区分同名资源。

## 候选变更预检与 Kubernetes 准入

`rgnix diff -c candidate.conf --against current.conf` 编译两份配置，报告新增、删除、修改的路由/后端、证书变化及监听参数是否要求重启。

`POST /v1/validate` 接受 `{"config_path":"/etc/rgnix/candidate.conf","expected_version":12}`，在独立模式编译候选，不发布。文件由全局 writer 指定本机路径；基准版本发生变化返回 409，应重试预检。`POST /v1/validate-ingress` 接受完整 Ingress JSON，用当前 Service、端点、Secret、插件、配额与域名策略构建隔离候选，返回 valid/errors/warnings/diff，不改写任何资源。

模拟器的 `outbound` 展示最终 Service/后端、URI、脱敏转发头、当前灰度状态、镜像资格与确定性的样本选择；`io_executed:false` 表明未调用业务上游。它不模拟网络成功率或远端响应，也不改变真实轮询游标、限流计数与灰度样本。

可选 Helm 准入配置：

```yaml
watchNamespaces: [payments, shop]
admission:
  enabled: true
  tlsSecret: rgnix-admission-tls
  caBundle: BASE64_ENCODED_CA_PEM
  failurePolicy: Fail
```

TLS Secret 的 SAN 必须覆盖 `RELEASE-admission.NAMESPACE.svc`。Chart 安装独立的 9443 TLS listener、Service 和 ValidatingWebhookConfiguration，不通过该端口开放管理 API。CREATE/UPDATE 与 `kubectl apply --dry-run=server` 复用同一套预检。健康时忽略无关 IngressClass；限定 watch 时自动设置 namespaceSelector。准入启用后，发布状态注解只允许控制器 ServiceAccount 更新，应用通过受审计的 rollout API 操作。DELETE 不被阻挡。首次部署需要等待 watch 就绪；默认 Fail 在 webhook 不可用时拒绝匹配 namespace 内的 Ingress 变更，包括其他 Class，应据此选择部署范围。准入证书更新需滚动控制器。

## 业务 TLS 与 HTTPS 跳转

业务证书加载时检查叶证书有效期、私钥匹配及声明的 server_name/TLS hostname，支持单标签通配符；过期或不匹配证书拒绝加载。运行时过期证书不再用于新握手。Ingress Secret 更新继续热发布；新无效证书不能通过沿用旧授权绕过撤销语义。

`/v1/config` 证书诊断包含有效期、名称匹配及未来 14 天内到期标记。Prometheus 提供 `rgnix_certificate_expiry_timestamp_seconds{listener,host}` 与 `rgnix_certificate_valid{listener,host}`；host 来自配置，最多 2048 个证书项。证书申请续期继续由 cert-manager 等外部组件负责。

`rgnix.io/ssl-redirect: "true"` 返回 308，保留路径与 query；`rgnix.io/https-port` 可指定目标端口，默认 443。跳转发生在客户端证书和应用认证之前，CIDR 限制仍先执行。可信上游代理报告的 HTTPS scheme 可避免外部 TLS 终止造成重定向循环；必须同时正确配置可信代理范围。

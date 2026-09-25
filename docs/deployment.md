# 部署与运行

## 独立服务

```sh
rgnix check -c /etc/rgnix/nginx.conf
rgnix serve -c /etc/rgnix/nginx.conf --admin 127.0.0.1:9090 --threads 2
# 更新文件后向上述进程发信号
kill -HUP PID
kill -TERM PID
```

SIGHUP 在控制线程重读 include、证书和脚本，全部完成后发布快照。坏配置增加失败计数并保留原版本；监听地址、TLS/HTTP2 选项变化拒绝热更新。线程数来自 CLI，需要重启。SIGTERM 停止接收新请求并优雅终止，配置的 5 秒宽限期及最长 25 秒排空不适合无限期维持 WebSocket；客户端应支持重连。

相对 root、include、证书、脚本和日志路径均基于主配置文件目录。挂载示例目录只读时无需其他数据卷；日志默认写 stdout/stderr。使用文件日志时预先创建可写目录；显式错误日志在加载配置时检查可打开，访问日志按需打开，两者通过有界后台队列写入。`rgnix_log_rotation` 提供大小/UTC 时间轮转、保留份数及 gzip；SIGUSR1 重开文件，可配合外部 logrotate。独立模式成功的 SIGHUP 也应用轮转策略并重开文件，失败保留旧配置。见[本地日志与轮转](log-rotation.md)。

## 镜像

`Dockerfile` 使用 Rust 1.98 / Debian bookworm 多阶段构建，运行阶段非 root 用户 UID/GID 10101。Rust 依赖使用 `--locked`。有企业 TLS 根证书的构建环境可使用 BuildKit secret，不关闭证书校验：

```sh
docker build --secret id=build_ca,src=/path/to/ca-bundle.pem -t rgnix:0.2.0 .
```

双架构 OCI 镜像构建（需要 buildx 以及本机或远端相应架构 builder）：

```sh
docker buildx build --platform linux/amd64,linux/arm64 \
  -t YOUR_REGISTRY/rgnix:0.2.0 \
  --output type=oci,dest=rgnix-0.2.0.oci.tar .
```

本地单架构加载使用 `docker build` 或 buildx `--load`。CI 配置双架构构建但不推送。构建缓存分架构；显式重新构建项目 crate，防止源文件 mtime 导致旧二进制被误复用。

## Kubernetes Ingress

```sh
helm upgrade --install rgnix charts/rgnix \
  --namespace rgnix-system --create-namespace \
  --set image.repository=YOUR_REGISTRY/rgnix \
  --set image.tag=0.2.0
kubectl -n rgnix-system rollout status deployment/rgnix
```

Chart 默认：2 个副本、LoadBalancer Service 的 80→8080 / 443→8443、管理端口 9090 不由业务 Service 暴露；只读 rootfs、drop ALL capabilities、seccomp RuntimeDefault；请求 100m CPU/128Mi、限制 2 CPU/512Mi。滚动更新 maxUnavailable=0、maxSurge=1、minReadySeconds=2；preStop 等待 5 秒让端点撤销传播后再接收 SIGTERM，terminationGracePeriodSeconds 为 60，覆盖请求、OTLP logs/traces 和本地日志排空预算；PDB 至少 1 个可用副本。无需 CRD。

Chart 按节点提供软拓扑分散约束，多节点时优先分散副本；单节点仍可运行，但不具备节点故障容错能力。

`--ingress-class rgnix` 只接受同名 IngressClass，且其 controller 必须为 `rgnix.io/ingress-controller`。仅当此 Class 标记为默认时才接收无 ingressClassName 的资源。默认不读取旧式 `kubernetes.io/ingress.class` 注解，不兼容 ingress-nginx 的其他注解。

准备应用 Service 和 TLS Secret，再应用 [Ingress 示例](../examples/ingress.yaml)。示例需要同命名空间 `app:http`、`canary:http` 两个 Service 及 `app-tls` Secret；这些应用资源不会由 controller 自动创建。

```sh
kubectl -n YOUR_APP_NAMESPACE create secret tls app-tls \
  --cert=fullchain.pem --key=key.pem
kubectl -n YOUR_APP_NAMESPACE apply -f examples/ingress.yaml
```

TLS Secret 不会自动创建，也不自动签发证书或把 HTTP 跳转到 HTTPS。可以由 cert-manager 等外部系统更新。新完整 TLS 握手读取当前 SNI 证书，已有 TLS 连接继续完成；删除或无效证书不会退回其他租户证书。

同一 TLS 域名按 Ingress 的创建时间、命名空间和名称确定归属，后续声明产生 `TLSConflict` Event。所属 Secret 缺失、损坏或未指定时保留该域名归属，新完整握手失败，也不回退到其他 Ingress 的同名或通配证书。移除拥有者的 TLS 域名声明、删除该 Ingress 或改变其 Class 后释放归属，后续声明才可接管；其他域名仍可使用自己的证书。

### 路由和后端

- 支持明确域名、空 host、单标签通配域名、defaultBackend、Exact、Prefix；ImplementationSpecific 定义为 Prefix。
- Prefix 按路径段匹配，`/api` 匹配 `/api/v1`，不匹配 `/apix`。同 host 中最长路径优先，同长度 Exact 优先。
- defaultBackend 是独立的兜底项；同一或其他 Ingress 的显式规则均先于它匹配，包括无 host 的 `Prefix /`。删除显式规则后重新使用兜底；多个 defaultBackend 之间仍按创建时间、namespace、name 决定优先级并报告冲突。
- 仅 Service 后端，支持命名/数字 Service 端口。EndpointSlice 的端口按 Service 端口名对应，ready 不为 false 且 terminating 不为 true 才可选。解析 IPv4/IPv6，忽略 FQDN 地址类型。
- EndpointSlice 若带有 `apiVersion: v1`、`kind: Service` 的 ownerReference，其名称和 UID 必须与当前 Service 一致，避免同名 Service 重建时重新使用旧切片。只修改 ownerReference 也触发快照更新。没有 Service ownerReference 的手工切片继续按同命名空间及 service-name 标签关联；这是本产品增加的归属约束，不能替代对 EndpointSlice 写权限的控制。
- 不支持 ExternalName、resource backend、UDP/SCTP。无端点返回 503；后端默认 HTTP，`rgnix.io/backend-protocol` 可选 HTTPS/GRPC/GRPCS，私有 CA 来自同命名空间 Secret。
- 冲突的 host/path/pathType 按创建时间、namespace、name 升序，较早者获胜；发布 RouteConflict Event。通配域名仅匹配一层 DNS 标签。
- Ingress 转发 Host 为客户端主机，X-Forwarded-For/X-Real-IP 为可信代理策略解析的客户端 IP，X-Forwarded-Proto 为有效协议；默认不信任传入地址头。外部 LB 信任链和 PROXY v1/v2 由控制器 `forwarding.*` 配置，租户不能扩大可信 CIDR。

每个 Ingress 可独立配置请求大小、连接/读写超时、keepalive、访问日志、限流/并发、认证、客户端证书、压缩和后端调度，详见[策略注解表](product-features.md#ingress-应用注解)及[策略示例](../examples/ingress-policies.yaml)。

### 插件及更新失败

```yaml
metadata:
  annotations:
    rgnix.io/script: routes/main.rgl
    # 可选；默认关闭请求体读取
    rgnix.io/request-body: "prefix 4k"
    rgnix.io/request-body-timeout: "2s"
```

ConfigMap 必须与 Ingress 同命名空间，源码存放在 `data` 指定键中。选择后端使用 `route.proxy("SERVICE:PORT")`，仅允许该 Ingress 已声明的 Service。Ingress 注解目前加载 RGL 源码；独立模式可以加载 `.rgl` 或 `.wasm`。

请求体读取策略与脚本一起写入检查点并原子更新；无效读取策略也保留上一有效组合。`full SIZE` 用于完整 JSON 判断，`prefix SIZE` 只截断插件视图，仍转发完整 Body。详见[按请求体路由](request-body.md)。

六类 watcher 分别维护 Ingress、IngressClass、Service、EndpointSlice、TLS Secret、ConfigMap 缓存；首次同步后才 ready。事件短暂合并后在控制面编译和构建快照。文件配置是整份原子替换；Ingress 插件失败按资源隔离，保留上一有效模块及路由定义，其中插件可选后端仍限于该有效定义。端点、Service、Class 选择、TLS 及 Ingress 删除独立按当前资源构建，避免脚本错误冻结资源撤销。删除插件 ConfigMap 禁用对应路由；没有历史模块的坏插件路由返回 503。恢复有效插件后再发布新的路由定义。

API Server 暂时断开时 watcher 重连并保留最后观察到的状态，不能承诺在断网期间获知删除。控制服务初始化失败或异常终止会使 healthz/readyz 返回 503；短时 watch 重试保持进程存活。

每个使用插件或应用策略的 Ingress 对应一个 `rgnix-state-*` ConfigMap，位于 publish-service 的命名空间。检查点包含版本化源码、策略和路由定义，绑定 Ingress UID、插件 ConfigMap UID（如存在）和 IngressClass；不保存 Secret 内容或 Wasmtime 原生机器码。无自定义插件时使用内置 pass 源码记录策略，数据面不额外挂载插件。写入使用 resourceVersion 并核对当前源资源，Ingress 删除或插件撤销后清理。内容必须能够编码在 900 KiB 检查点内。持久化异步执行，但插件、策略和路由只有在 watch 确认该检查点后才能发布；写入失败继续使用上一有效版本，增加指标并重试。首次发布等待期间保持 NotReady。部署更新前应确认 `rgnix_checkpoint_healthy=1` 且没有 PendingCheckpoint 诊断。

检查点写入和清理各自最多并发 8 项，每项限时 5 秒；超大检查点、单项 API 拒绝或卡住只使该项失败，其他项继续完成。待持久化内容变化时切换到最新批次，不等待旧批次超时；纯端点或状态变化不会反复重启相同的持久化工作。检查点列表读取本身失败时整轮无法继续，控制器记录失败并重试，已发布数据面保持运行。

新副本遇到坏脚本时重新编译上一有效检查点；没有可恢复版本的路由返回 503。首次同步若仅有无法发布的插件路由，`/readyz` 保持 503；存在其他已接受路由时仍可就绪，避免一个租户的错误撤下整个入口。已启动的副本保留有效版本，新出现的坏路由仅使该路由退化。插件 ConfigMap 删除或 UID 改变不会恢复已撤销的插件。不要手动编辑检查点；控制器命名空间的写权限属于集群管理边界。

只对选定 Ingress 及其当前/上一有效路由依赖计算输入摘要；无关资源、status/resourceVersion 变化不触发发布。相同后端每轮只解析一次，相同坏插件缓存失败结果。域名和路径匹配使用索引。相关资源变化仍会重建所选路由快照；超大集群的增量构建和容量上限尚未认证。Event、Lease/status 和检查点写入与数据快照更新分开执行，并设置超时，API 写阻塞不阻塞端点撤销。

Event 仅在 API 确认创建成功后去重，按 Ingress UID、原因和诊断内容区分。失败增加 `rgnix_report_errors_total`，约 10 秒后的周期报告会重试，无需编辑资源；成功记录在本进程内保留，诊断消失后移除。进程重启或请求超时但服务端实际已写入时，可能产生重复 Event。状态回写的非冲突失败同样计数并重试，404/409 按资源删除或并发变更处理。

每轮报告中，Event 创建与 Lease/状态回写并发执行，Event 分支最多 5 秒。状态分支先完成选主和 publish-service 读取，两步各限时 5 秒；随后最多并发更新 8 个 Ingress，每项 GET 与 PATCH 合计限时 5 秒。某一项被拒绝或超时不会取消其他项，失败分别计数并在后续报告重试。

状态批次运行期间每 10 秒续租，避免大量慢资源导致租约过期；失去领导权、续租失败或进程停止会取消剩余请求。已经由 API Server 接受的写入不能撤销。长批次可能推迟下一轮读取新的地址目标，但不阻塞数据面快照更新。回写前重新核对 Ingress UID 和 Class，并使用当前 resourceVersion，避免覆盖已删除、重建或转交其他控制器的资源。

### Lease 与地址回写

两个副本都维护数据面，Lease 用于状态回写与灰度阶段推进。Lease 位于 publish-service 的命名空间，名为 `CLASS-leader`，30 秒租期；身份默认 POD_NAME。领导者把 `--publish-service namespace/name` 的 `status.loadBalancer` 地址复制到所选 Ingress，撤销地址时清空旧值。状态更新使用 resourceVersion，资源删除和冲突不覆盖新版本。灰度回退在触发副本立即生效，不等待选主。

在本地 kubeconfig 下也可运行：

```sh
rgnix ingress --ingress-class rgnix \
  --publish-service rgnix-system/rgnix \
  --identity dev-controller \
  --http-listen 127.0.0.1:8080 --https-listen 127.0.0.1:8443 \
  --admin 127.0.0.1:9090
```

默认 RBAC 包含全命名空间资源 watch、Secret 读取、Ingress/status patch、Event create，以及 publish namespace 内 Lease 操作。设置 Helm `watchNamespaces` 可同时限定 watch 范围和 namespaced Role/RoleBinding；IngressClass 仍需集群读取权限。仅设置 IngressClass 不会缩小 RBAC。保护 ServiceAccount、管理员策略文件及插件 ConfigMap，详见[治理操作指南](governance.md)。

更新 Chart 时同时更新 RBAC：publish namespace 的 Role 还需要 ConfigMap get/list/watch/create/update/delete 和 Service get/list/watch 权限，用于检查点恢复及发布地址读取。

## 资源与上游故障保护

`serve` 和 `ingress` 均支持以下参数；修改需要重启：

| CLI / Helm 值 | 默认值 | 行为 |
|---|---|---|
| `--max-inflight` / `maxInflight` | 1,024 | 请求头读取后限制并发请求数，含流式请求；超额返回 503，管理接口独立 |
| `--max-plugin-instances` / `maxPluginInstances` | 32 | 限制同时存活的请求插件实例；不排队，超额返回 503 |
| `--upstream-max-fails` / `upstreamMaxFails` | 3 | 同一端点连续传输错误达到阈值后暂时摘除；0 关闭 |
| `--upstream-fail-timeout-secs` / `upstreamFailTimeoutSeconds` | 10 | 摘除时长，之后重新参与加权轮询 |

健康状态按端点统计；正常完成的响应清除连续失败计数，客户端取消不计为上游失败，HTTP 5xx 本身不触发摘除。相同后端配置在快照更新间保留健康状态。所有端点不可选时返回 503，失败的当前请求不会自动重试。以上为默认被动故障隔离；可另外启用主动 HTTP 健康检查。Ingress 始终检查 EndpointSlice 就绪状态。默认被动策略与 NGINX max_fails 参数不同。

并发上限应与容器内存、请求体和插件复杂度一起调整；它不限制尚未完成请求头的空闲 TCP 连接总数，也不是生产容量保证。

## 管理与指标

访问日志可选通过 OTLP/HTTP protobuf 输出到第三方平台或 Collector，独立服务与 Ingress 共用 exporter；认证、Helm Secret、自定义 CA、缓冲与指标见 [OTLP 访问日志](otlp.md)。

独立模式默认只在 `127.0.0.1:9090` 开放管理接口；Ingress Pod 内监听 `0.0.0.0:9090`，业务 Service 不暴露。健康和指标接口保持无认证，使用网络策略/端口隔离。配置 `--admin-token-file` 或 Helm `admin.tokenSecret` 后可访问受 Bearer token 保护的 `/v1/config`、routes/backends/history/explain；独立模式还可回退保留版本。见[诊断与回退](product-features.md#诊断与回退)。

| 接口/指标 | 含义 |
|---|---|
| `/healthz` | 200 正常；控制器失败 503 |
| `/readyz` | 200 已有可用快照；启动同步和退出时 503 |
| `rgnix_requests_total{status}` | 完成请求计数，只有有限状态码标签 |
| `rgnix_request_seconds` | 请求时延直方图 |
| `rgnix_route_requests_total` / `rgnix_route_request_seconds` | 按配置路由统计请求和时延；历史标签容量有限 |
| `rgnix_backend_requests_total` | 按配置后端统计结果 |
| `rgnix_plugin_calls_total` / `rgnix_plugin_seconds` | 插件求值次数/耗时；请求阶段包含实例化，响应阶段包含缺省空钩子调用 |
| `rgnix_plugin_errors_total` | 插件 trap、非法决策或后端越权 |
| `rgnix_upstream_errors_total` | 上游错误 |
| `rgnix_config_version` | 本进程当前快照版本，不是跨副本全局编号 |
| `rgnix_reloads_total` / `rgnix_reload_errors_total` | 发布数、更新失败数；每次重建的插件失败会计数 |
| `rgnix_access_logs_dropped_total` / `rgnix_error_logs_dropped_total` | 本地访问/显式错误日志队列溢出或文件打开/写入失败 |
| `rgnix_log_rotations_total` / `rgnix_log_reopens_total` | 文件轮转/重开次数 |
| `rgnix_log_io_errors_total{operation}` | 日志文件 I/O 失败；完整队列和退出指标见[轮转文档](log-rotation.md) |
| `rgnix_config_info{sha256}` | 当前数据面配置内容摘要，每个进程仅暴露当前一条；用于副本一致性比较 |
| `rgnix_config_diagnostics` | 当前 Ingress 配置诊断数量 |
| `rgnix_checkpoint_healthy` / `rgnix_checkpoint_errors_total` | 最近检查点持久化是否成功、累计失败次数 |
| `rgnix_report_errors_total` | Kubernetes Event/status 写入失败或超时 |
| `rgnix_rejected_requests_total{budget}` | 请求/插件实例额度耗尽，标签仅 inflight/plugin |
| `rgnix_upstream_ejections_total` | 因连续传输失败暂时摘除的端点次数 |

指标只使用配置路由/后端标识，不使用实际请求路径、Host、租户等任意请求值；历史路由/后端标签总数最多 2048。进程 CPU/RSS 使用容器监控或 `/proc` 获取。`otlpTraces.*` 可独立启用 W3C 上下文传播、server/client spans 和访问日志关联，详见[可观测性](product-features.md#可观测性)。

## OrbStack 验收

Linux 构建/验证在 OrbStack 中执行，避免用 macOS 编译结果代表 Linux 服务：

`scripts/check.sh` 还需要系统安装 `logrotate`、`python3-grpcio`、`python3-brotli`，用于外部轮转、gRPC 双向流及 Brotli 解码验收。

```sh
orb -m ubuntu bash -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh'
docker build -t rgnix:0.2.0 .
RGNIX_IMAGE_TAG=0.2.0 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
RGNIX_IMAGE_TAG=0.2.0 python3 scripts/product_kubernetes.py rgnix-qa-example orbstack
```

脚本只接受专用命名空间，并要求现有 namespace 带 `rgnix-qa=true`；使用专属 IngressClass，不修改其他 controller 或工作负载。它会创建两个 NGINX 后端、插件、临时证书，执行删除/恢复和滚动升级。结束后保留环境便于检查。QA 的 LoadBalancerClass 为 `rgnix.io/acceptance`、禁用 NodePort，通过 port-forward 和集群内 Service 验证；地址 `192.0.2.10` 仅是 status 回写测试数据，不是真实公网 LB。

随后运行 product_kubernetes.py 会在同一 QA 命名空间创建 HTTP/TLS 业务 fixture、认证资源和真实 OpenTelemetry Collector，并升级测试控制器以验证 Helm 参数、策略恢复、Secret/Service 撤销和 traces。它不适用于业务命名空间。运行镜像需要集群预先可用；应用 fixture 使用 python:3.13-alpine，Collector 使用 0.123.0。

手动清理时先卸载 Helm 以清理 ClusterRole/IngressClass，再删除测试 namespace：

```sh
helm uninstall rgnix-qa -n rgnix-qa-example --kube-context orbstack
kubectl --context orbstack delete namespace rgnix-qa-example
```

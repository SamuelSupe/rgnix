# 多副本配置发布状态

此能力从 **v0.4.0 Preview** 提供。适用于 Ingress 和 Gateway；独立模式不启用。

## 启用与观测

使用 v0.4.0+ 镜像，并设置 Helm `reportReplicas: true`，或向控制器传入 `--report-replicas`。分阶段灰度也复用该报告，并按需自动启用。Chart 始终提供控制器 namespace 的 Pod get/list 和 Lease get/list/create/update 权限；不需要 Pod 写权限。`shutdown.enabled` 需显式开启，使用旧 v0.3.0 镜像时保持关闭。

控制器通过 `--publish-service namespace/name` 的 selector 查找副本，`POD_NAME` 必须对应当前 Pod。每个 Pod 使用独立 Lease 写入配置摘要、控制配置摘要、已观测输入摘要、发布时间、就绪和拒绝状态。Lease 绑定 Pod UID，由 Kubernetes 随 Pod 回收。不同 Service、IngressClass 或 Gateway 绑定使用不同报告分组。不要让发布 Service 同时选中无关工作负载。

```sh
curl --fail -H "Authorization: Bearer $TOKEN" http://127.0.0.1:9090/v1/fleet
```

`GET /v1/fleet` 仅允许全局 reader/writer，具名 namespace 身份不可查看其他租户所在副本的配置状态。返回 `target`、`replicas`、`converged`、`observed_at` 和观测年龄。每个副本包含 Pod/节点、发布时间、拒绝数量和一个最多 512 字符的 `rejection` 原因；完整诊断继续查看相应资源状态、Event 与该实例日志。`version` 是进程内序号，跨副本应比较 SHA-256 摘要。

| state | 含义 |
|---|---|
| converged | 已就绪，输入已处理，活动快照与控制配置一致，且没有拒绝项 |
| drifted | 副本的输入、活动配置或控制配置与查询实例不同 |
| rejected | 某个配置/依赖被拒绝，可能仍在服务上一有效插件 |
| reconciling | 最新输入尚未完成处理 |
| unready | Pod 或本地数据面未就绪 |
| stale | 心跳超过 20 秒或时钟异常 |
| missing | 已发现 Pod，但没有有效的同 UID 报告 |

状态按健康优先级分类；例如一个尚未就绪的 Pod 首先显示 unready。每约 5 秒上报和读取，单轮 API 访问最多 8 秒。观测超过 20 秒也不会返回 `converged: true`。每组最多检查 256 个匹配 selector 的 Pod（包括 API 返回的终止记录）、1024 个存留 Lease；达到上限或 API 分页未取全时报告失败，不以部分列表声称收敛。API/RBAC 故障只使发布门禁失败，不中断已有业务流量。

## 发布脚本门禁

发布变更后，从一个已确认采用目标配置的实例 `/v1/config.sha256` 或 `/v1/fleet.target.active_sha256` 取得预期摘要，再等待所有发现的活动副本一致：

```sh
rgnix wait --admin-url http://127.0.0.1:9090 \
  --token-file /run/secrets/rgnix-admin-token \
  --expected-sha256 EXPECTED_64_HEX_SHA256 \
  --replicas 2 --timeout-seconds 90
```

成功输出状态 JSON、退出 0。配置拒绝、摘要不符、副本不足、陈旧心跳或超时均不能通过；鉴权/连接错误直接退出非零。令牌不放在命令行参数中。HTTPS 验证服务端证书，不跟随重定向或使用环境代理。

必须显式确认目标摘要对应刚提交的变更；直接读取任意旧副本的摘要不能证明新版本发布成功。`--replicas` 是最少数量，不会忽略超出的活动 Pod。终止中或已终止 Pod 不参与比较，其已接纳请求仍按原快照排空。报告依据 Service 发现，不能替代 Deployment 期望副本数；设置最少副本数可避免只剩一个存活实例时误判成功。

这是一致性观测与发布等待门禁，**不是跨副本原子提交协议**。watch 传播期间仍可能存在多个版本。外部依赖变化、控制文件投影和无效配置会延迟或阻止收敛。坏配置仍保留既有恢复语义，端点和资源撤销继续生效。

## 告警

[指标目录](metrics.md)包含固定状态标签的 `rgnix_fleet_replicas`、收敛、观测时间及失败计数；[告警示例](../examples/prometheus-alerts.yaml)包含持续不收敛与心跳过期规则。每个 Pod 单独抓取指标，不能轮询一个 ClusterIP 后将不同进程的计数器当作同一实例。

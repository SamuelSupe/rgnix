# 跨副本速率限制

v0.3.0 预览版支持可选 Redis 协调器。启用后，独立服务、Ingress 和 Gateway 的路由速率限制，以及管理员命名空间 `requests_per_second` / `burst`，在同一 `scope` 内共享令牌桶。未启用时保持本地计数。

```json
{
  "url": "rediss://USER:PASSWORD@redis.example.com:6379/0",
  "scope": "production-edge",
  "failure_mode": "closed",
  "timeout_ms": 100,
  "max_inflight": 256
}
```

把实际凭证写入受限文件或 Kubernetes Secret，不要提交到仓库：

```sh
rgnix serve -c nginx.conf --global-rate-limit-file /etc/rgnix/shared-rate.json
kubectl -n edge create secret generic shared-rate --from-file=config.json=/etc/rgnix/shared-rate.json
helm upgrade --install edge charts/rgnix -n edge --set globalRateLimit.secret.name=shared-rate
```

Ingress / Gateway 命令也接受 `--global-rate-limit-file`。Helm Secret 默认 key 为 `config.json`，通过完整目录投影实现热更新。文件作为管理控制配置的一部分校验、原子替换；无效更新保留上一版本。`/v1/config.shared_rate_limit` 显示是否启用、scope、超时和故障模式，不返回 Redis URL 或凭证。

| 故障模式 | 协调器超时、断连或本地查询并发已满 |
| --- | --- |
| `closed`（默认） | 返回 503 |
| `open` | 本次跳过共享速率检查，其他认证、并发和资源限制继续执行 |
| `local` | 使用该进程的本地令牌桶；故障期间不再保证跨副本总速率 |

令牌不足返回 429；共享键容量耗尽始终返回 503。默认查询超时 100ms、最多 256 个并行查询，可分别配置 1..2000ms、1..4096。只在配置了速率限制的请求阶段访问 Redis。命名空间总限流先于认证，路由 IP/route 限流在认证前，header/cookie/JWT claim 限流在认证后。

共享状态按 `scope + namespace + route + rate policy + identity` 隔离，命名空间总预算不包含 route。多个副本必须使用相同 scope 和一致路由配置；不同部署应使用不同 scope。修改速率参数或 scope 会创建新桶。独立模式以配置的服务器序号与 location 标识路由，各副本应保持一致。正常共享准入不再扣减本地速率桶。

Redis 在一次 Lua 操作内使用服务端时间检查、扣减和维护过期；请求标识只以摘要存储。新建桶受每 namespace 的 `max_limiter_keys` 限制，独立模式上限 16384；空闲键至少保留 60 秒，之后可回收。降低容量不会强制驱逐现有活跃桶，恢复到新容量前拒绝创建新桶。键使用同一 hash tag，但当前客户端连接单一 Redis 端点，不提供 Sentinel/Cluster 拓扑发现。需要 `TIME`、`EVAL`、HASH、ZSET、`PEXPIRE` 权限；不要使用会主动淘汰限流状态的 Redis eviction 策略。Redis 数据丢失或被清空会重置桶，计费级持久额度应使用专门账本。

`rediss` 使用系统 CA 和主机名验证，不支持 `#insecure`。不提供自动业务请求重试；不重试结果不明的 Redis 扣减，避免重复计数。失效连接会丢弃，后续请求重新连接，恢复过程中仍可能收到一次 503。Redis 本身的 HA、备份和容量由部署者管理。

CPU/内存、并发请求、插件、鉴权和镜像并发继续按进程隔离。模拟器使用独立本地桶，返回 `rate_limit_scope: isolated_simulation` 和 `shared_quota_checked: false`，不访问或消耗生产共享额度。

指标：`rgnix_global_rate_limit_total{result}` 和 `rgnix_global_rate_limit_seconds{result}`。固定 result 为 `allowed`、`limited`、`capacity`、`unavailable_closed`、`unavailable_open`、`unavailable_local`，不使用任意用户标识作标签。

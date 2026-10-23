# 本地限流与旧 Redis 配置迁移

当前开发版本的路由速率限制和命名空间总速率限制均按进程执行，不需要 Redis。每个副本独立持有令牌桶；速率不足返回 429，并发和资源保护继续执行。

路由继续使用 `rgnix_limit_rate` / `rgnix_limit_conn`，Ingress 和 Gateway 继续使用相应 policy annotation。命名空间总速率继续由管理员 tenant policy 的 `requests_per_second` / `burst` 配置。详见[产品能力](product-features.md)和[平台策略](platform-policies.zh-CN.md)。

有 N 个副本时，部署总速率可能达到 N 倍的每副本限额，burst 也分别计算。扩缩容会改变总额度，进程重启会重新初始化本地桶。要求跨副本严格共享额度时，应在部署入口统一执行该策略；本版本不提供分布式额度协调。

从使用 Redis 协调器的版本升级时：

1. 移除 `--global-rate-limit-file`。新二进制会拒绝这个旧参数。
2. 移除 Helm 的 `globalRateLimit` 配置及其专用 Secret。Chart 会拒绝仍指定协调器 Secret 的旧 values，避免静默改变限流范围。
3. 按副本数重新分配路由和命名空间限额；旧 scope、dependency failure mode、Redis timeout 和共享键容量不再参与请求处理。
4. 更新监控：共享协调器的 `rgnix_global_rate_limit_*` 指标不再提供；使用现有请求拒绝、命名空间拒绝和限流器容量指标。`/v1/config.rate_limit_scope` 为 `process`。

请求模拟继续使用独立的本地桶，不消耗实际请求额度。历史发行和验证记录描述各自当时的 Redis 行为，不代表当前开发版本仍依赖 Redis，也不代表新版本已完成长测。

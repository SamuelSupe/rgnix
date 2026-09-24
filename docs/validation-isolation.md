# Ingress 检查点与 TLS 归属修复

日期：2026-09-24，Asia/Singapore。本轮修复两个已复现的 Ingress 隔离问题。上一轮 `qa9` 的完整记录见 [历史修复记录](validation-fixes.md)。

## 问题与修复

| 问题 | 旧镜像 qa9 的实际结果 | 修复后的行为 |
|---|---|---|
| 单个检查点中断整批持久化 | 超过 900 KiB 的检查点使整批提前退出，正常租户的新路由持续返回 503 | 每项写入/清理独立限时 5 秒，最多并发 8 项；单项失败继续处理其他项，失败汇总到日志和指标 |
| 证书删除后回退到其他租户 | 优先 Ingress 的 Secret 删除后，TLS 握手仍成功，拿到另一命名空间序列号 31 的证书 | 先按稳定优先级确定域名归属；证书缺失或无效时保留归属并拒绝该域名的新完整握手，不回退到同名或通配证书 |

旧镜像的失败输出保留在 [检查点复现](validation/isolation-checkpoint-before.txt) 和 [TLS 复现](validation/isolation-tls-before.txt)。这些是有意执行的新回归用例所产生的预期失败记录。

持久化任务同时处理新配置到达的情况：待保存内容变化时取消过时批次并读取最新内容；相同内容不反复中断工作，单个卡住的旧写入不会让新租户等到旧批次超时。仍保留当前源资源校验、resourceVersion 条件更新、删除前置条件以及 watch 确认后才发布的约束。检查点列表读取整体失败时仍需等待 API 恢复，已发布数据面继续运行。

TLS 归属按照 Ingress 创建时间、命名空间和名称排序。冲突产生 `TLSConflict` Event；证书恢复后继续使用原拥有者的证书。只有移除其域名声明、删除拥有者 Ingress 或改变 Class 才释放归属。配置内容摘要包含“有归属但证书不可用”的状态，副本可以比较该状态。

修改集中在检查点持久化、Ingress 构建、TLS 快照模型以及相应的验收脚本，没有修改 RGL ABI 或代理请求处理。运行契约已更新至 [部署文档](deployment.md)。

## 验证

Linux 编译与运行均在 OrbStack arm64 执行。最终运行验证使用从 `rgnix:0.1.0-qa10` 提取的 release 二进制，镜像由锁定依赖构建。

| 验证 | 结果 | 证据 |
|---|---|---|
| Rust fmt、clippy `-D warnings`、单元测试 | 通过，5/5 | [检查输出](validation/isolation-linux-checks.txt) |
| HTTP、TLS、代理、插件完整回归 | 43/43 | [结果](validation/isolation-http.json) |
| NGINX 1.28.0 行为对照 | 22/22 | [结果](validation/isolation-nginx.json) |
| 模拟 Kubernetes API 与真实 TLS 握手 | 19/19 | [结果](validation/isolation-recovery.json) |
| 真实 Kubernetes 生命周期 | 37/37；升级期间 300 次请求全部成功 | [结果及副本状态](validation/isolation-ingress.json)、[完整输出](validation/isolation-kubernetes.txt) |

模拟 API 测试分别注入超大检查点、403 拒绝和 8 秒阻塞；正常租户均在 3 秒测试门限内发布。TLS 测试验证跨命名空间的同名及通配声明、原证书删除、无效 PEM、有效替换、其他域名继续握手，以及拥有者删除后的合法交接。新增用例扩展现有行为验收脚本，没有为内部字段或静态映射新增测试。

真实集群使用已有专用命名空间 `rgnix-qa-20260923`，验证 TLS 冲突 Event、存在同名及通配候选时的 Secret 撤销、恢复后实际证书序列号，以及完整资源生命周期和滚动升级。结束后两个副本均使用 `qa10`、2/2 Ready，配置摘要一致，检查点健康值均为 1。测试临时增加的冲突 Ingress 和备用 Secret 已移除，原验收环境保留。状态地址测试仍使用保留示例地址，不代表公网 LB 验证。

Helm lint、模板渲染和 Shell 语法检查也通过。[完整 release 输出](validation/isolation-release-checks.txt) 记录 HTTP、NGINX 及模拟 API 验收；日志中的 `Terminated: 15` 是脚本正常结束自己创建的 port-forward。

## 产物

- 镜像：`rgnix:0.1.0-qa10`，本地 `rgnix:0.1.0` 指向同一产物，未推送远端。
- 镜像 manifest list：`sha256:fd48cc205e30b95e0ad88f68b560e964c12f22d569da430e1c2d79d4c340a34c`。
- 二进制 SHA-256：`590dfa7bbcf3555e712a529e69e150fbab25dbf4179b6ff739f8232ab1e11191`。
- 源树 SHA-256：`e6d5f04b1ff0cacfb90ceb9de4bdd16f565e7963fe55f48818e857000bee1ab5`。

完整身份及摘要算法沿用前轮，见 [产物记录](validation/isolation-artifact.json)。

```sh
# OrbStack Ubuntu 内，从仓库目录执行
export CARGO_TARGET_DIR=/tmp/rgnix-target
cargo fmt --check
cargo clippy --locked --all-targets -j 2 -- -D warnings
cargo test --locked -j 2
python3 scripts/ingress_recovery.py .local/rgnix-linux-arm64
python3 scripts/integration.py .local/rgnix-linux-arm64
python3 scripts/nginx_parity.py .local/rgnix-linux-arm64

# macOS / OrbStack Docker 和专用验收命名空间
docker build -t rgnix:0.1.0-qa10 .
RGNIX_IMAGE_TAG=0.1.0-qa10 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

本轮没有重新测量性能，也没有完成 amd64、多节点、云 LoadBalancer、长期压力或大规模资源变更认证。检查点测试证明单项失败隔离，不能推导任意资源规模下的固定更新时间。编译仍在同一进程，请求并发额度仍在读取请求头之后生效；这两项架构边界未在本轮改变。

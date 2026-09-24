# 连接复用、特殊文件与 Event 故障隔离修复

日期：2026-09-24，Asia/Singapore。本轮修复审查中在 qa11 上实际复现的三个问题，原始结果见 [修复前证据](validation/transport-before.json)。上一轮记录见 [WebSocket 与 URI 修复](validation-protocol.md)。

## 问题与修复

| 问题 | qa11 的实际结果 | 修复后的实现 |
|---|---|---|
| HTTP 请求分帧异常时重新开启连接复用 | 同时带 Transfer-Encoding 和 Content-Length 的代理请求返回 200、keep-alive，同一连接上的下一次请求仍返回 200 | 配置只调整 Pingora 已允许复用的连接，保留解析器的关闭决定；正常 chunked 请求继续复用 |
| FIFO 阻塞静态文件线程池 | 20 个 FIFO 请求导致普通文件请求超时，readyz 仍为 200；接入 FIFO 写端后普通文件恢复 | 普通路径与 index 都先拒绝特殊文件，用 O_NONBLOCK/O_NOCTTY 打开，再检查实际打开的文件类型；继续使用 cap-std 的 root 限制 |
| Event API 延迟阻止续租和地址回写 | Event 延迟 8 秒时，观察 22 秒没有新增 Lease 写入或 status 写入，Ingress 地址停留在旧值 | Event 与 Lease/状态回写并发执行，各自限时 5 秒，失败分别计数；续租先于地址回写 |

连接修复同时恢复 HTTP/1.0 未声明 keep-alive 时的默认关闭行为。原始复现确认了连接关闭防护回归，没有证明可利用的请求走私攻击。[RFC 9112 的相关要求](https://www.rfc-editor.org/rfc/rfc9112.html#section-6.1) 允许服务器拒绝含两种长度声明的请求，或按 Transfer-Encoding 处理，但处理后必须关闭连接。

静态文件的预检查不能单独阻止文件替换竞态，因此打开操作保留非阻塞标志，打开后的类型检查仍生效。root 内的合法相对符号链接仍可访问，逃逸 root 的链接继续返回 403。`libc` 成为直接依赖以使用平台定义的打开标志，沿用锁文件已有的 0.2.189，没有升级其他依赖。

Event 去重仍只记录已确认成功的写入；后续写入超时不会清除之前的确认。约 10 秒的周期报告会重试失败项。Event 的超时不会取消 Lease/状态回写。这里隔离的是 Event API 故障；Kubernetes API 整体不可用时仍需要等待恢复。

运行契约见 [兼容矩阵](compatibility.md) 与 [部署文档](deployment.md)。

## 验证

Linux 编译和运行在 OrbStack Ubuntu arm64 执行。下表的 HTTP、NGINX 和模拟 API 验收使用从最终 `rgnix:0.1.0-qa12` 镜像提取的 release 二进制；真实集群运行同一镜像。调试二进制也通过了对应检查。

| 验证 | 结果 | 证据 |
|---|---|---|
| Rust fmt、clippy `-D warnings`、单元测试 | 通过，5/5 | [检查输出](validation/transport-linux-checks.txt) |
| HTTP、TLS、代理、插件回归 | 53/53 | [结果](validation/transport-http.json) |
| NGINX 1.28.0 行为对照 | 29/29 | [结果](validation/transport-nginx.json) |
| 模拟 Kubernetes API 与真实 TLS 握手 | 27/27 | [结果](validation/transport-recovery.json) |
| 真实 Kubernetes 生命周期 | 37/37；升级期间 300 次请求全部成功 | [结果及副本状态](validation/transport-ingress.json)、[完整输出](validation/transport-kubernetes.txt) |

新增回归扩展已有验收脚本，覆盖实际缺陷及正常行为：

- 向真实代理连接发送同时带 Transfer-Encoding 和 Content-Length 的请求，验证上游收到正确的 chunked 内容、冲突 Content-Length 不被转发、响应后收到连接 EOF。正常 chunked 请求仍能在原连接发送第二次请求；HTTP/1.0 未声明 keep-alive 时响应后关闭。
- 并发发起 20 个 FIFO 或 FIFO index 请求及一个普通文件请求，验证特殊文件全部返回 403，普通文件正常返回 200。root 内合法相对符号链接及越界链接分别保留 200/403 行为。
- 模拟 Event API 延迟 8 秒，同时更改 publish Service 地址，验证 Ingress 地址仍从 `192.0.2.10` 更新到 `192.0.2.11`，后续 Lease 写入继续发生，超时错误指标增加。保留 Event 403 重试、成功去重以及端点撤销不受报告阻塞的验证。

完整 release 输出见 [验收记录](validation/transport-release-checks.txt)。Helm lint、模板渲染、Shell 语法及 Python 语法检查通过。Kubernetes 日志中的 `Terminated: 15` 是脚本正常关闭自己创建的 port-forward。

真实集群使用已有专用命名空间 `rgnix-qa-20260923`。结束时两个副本均为 `qa12`、2/2 Ready，配置摘要一致，检查点健康值均为 1。测试临时创建的 TLS 冲突资源已删除，原验收环境保留。地址回写使用保留示例地址，不代表公网 LoadBalancer 验证。

## 产物与复现

- 镜像：`rgnix:0.1.0-qa12`，本地 `rgnix:0.1.0` 指向同一产物，未推送远端。
- manifest list：`sha256:eb9bbe53b304ae025ac2255f9e271b87cf764f0f284e6ac2e127dcc55e126141`。
- 二进制 SHA-256：`5b24e32f41357556a98bdde74ee90f0eaa3960bea01a644f3d23e7a68bfc4068`。
- 源树 SHA-256：`0f396e02916a9615da6a0d7a0650f067be1f82c251951090b1e229d4e346daa9`。

[产物记录](validation/transport-artifact.json) 同时包含 Cargo.lock 和验收脚本摘要。源树摘要按相对路径排序，对 Cargo.toml、Cargo.lock 和 src 下的文件依次输入路径、NUL、文件原始内容。

```sh
# macOS / OrbStack Docker，从仓库目录执行
docker build -t rgnix:0.1.0-qa12 .
mkdir -p .local
artifact_container=$(docker create rgnix:0.1.0-qa12)
docker cp "$artifact_container":/usr/local/bin/rgnix .local/rgnix-linux-arm64
docker rm "$artifact_container"

# OrbStack Ubuntu 内，从同一仓库目录执行
export CARGO_TARGET_DIR=/tmp/rgnix-target
cargo fmt --check
cargo clippy --locked --all-targets -j 2 -- -D warnings
cargo test --locked -j 2
python3 scripts/integration.py .local/rgnix-linux-arm64
python3 scripts/nginx_parity.py .local/rgnix-linux-arm64
python3 scripts/ingress_recovery.py .local/rgnix-linux-arm64

# macOS / OrbStack Kubernetes，在专用验收命名空间执行
RGNIX_IMAGE_TAG=0.1.0-qa12 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

本轮没有重新测量性能，也没有完成 amd64、多节点、云 LoadBalancer 或长期压力验证。特殊文件回归覆盖 FIFO 及其目录索引，未注入真实设备节点或底层文件系统永久 I/O 阻塞。Event 延迟故障使用模拟 API 注入；真实 Kubernetes 验证覆盖正常选主、地址更新、主副本退出和滚动升级。

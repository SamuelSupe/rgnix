# 脚本后端、状态隔离与 HTTP 语义修复

日期：2026-09-24 至 2026-09-25，Asia/Singapore。本轮修复 qa13 审查中实际复现的六项缺陷。原始观察见 [修复前证据](validation/semantics-before.json)，上一轮记录见 [路由修复](validation-routing.md)。

## 问题与修复

| 问题 | qa13 的实际结果 | 修复后的行为 |
|---|---|---|
| 脚本选择 HTTPS upstream 降为明文 | 同一 upstream 使用 `route.pass()` 时通过 TLS，使用 `route.proxy("localhost")` 时却发送包含测试 Authorization 头的明文 HTTP | 所有 location 加载后，将名称绑定到该 upstream 使用的协议；脚本选择 HTTPS 后端仍验证证书及主机名 |
| 两个同类 Action 无法区分 | 先创建 app、alternate 两个 Proxy，再返回第一个值，实际流量进入 alternate | 每次请求钩子最多调用一次路由决策 API，重复调用返回 500，不提交该钩子的暂存修改 |
| 一个慢 Ingress 阻塞其他状态回写 | 首个 Ingress GET 卡住后，每轮报告都在处理其他命名空间之前超时 | 每项 GET 与 PATCH 合计限时 5 秒，最多并发 8 项；单项错误不取消其他项，长批次每 10 秒续租 |
| 自动补斜杠使用错误的 location 配置 | `/slash` 的 301 使用根 location 的响应头 | 使用 `/slash/` 对应代理 location 的响应头、keepalive 和超时设置 |
| If-Range 接受较新的不同日期 | 不等于 Last-Modified 的日期仍得到 206 和部分内容 | 日期必须精确等于 Last-Modified（秒精度）；较旧、较新或无效日期返回完整文件 |
| `$host` 没有 server_name 兜底 | HTTP/1.0 无 Host 时，`proxy_set_header Host $host` 删除上游 Host | 使用所选 server 的第一个 server_name；`$http_host` 继续表示原始 Host 头 |

脚本后端名称绑定与声明顺序无关。未被 proxy_pass 引用的 upstream 默认使用 HTTP；含插件的配置若把同一个 upstream 名同时用于 HTTP 和 HTTPS，会在加载时报告歧义，应为两种协议使用不同名称。没有插件的混合协议配置继续有效。

路由决策保持 `rgnix_v1` ABI 的 0/1/2 返回值，未引入新的 Action 编码。插件应先完成分支判断，再调用一次 pass、proxy 或 reply；不能先计算多个候选 Action。普通函数可返回唯一决策，未调用决策 API 时仍可隐式 pass。

Ingress 状态批次失去领导权、续租失败或进程停止时取消剩余请求。每次回写前重新核对 UID、Class 和 resourceVersion。已被 API Server 接受的写入不能撤销；大量慢资源仍可能推迟下一轮地址目标的读取，但不会阻塞数据面资源更新。

新增回归扩展已有测试，覆盖实际观察到的缺陷：

- 命名 HTTPS upstream 的脚本选择、证书验证，以及混合协议配置的加载规则。
- 两次 Proxy、两次 Reply 后返回先前 Action 的失败行为，以及 HTTP 请求得到 500。
- 17 个慢 Ingress 分三批超时，另一命名空间仍得到新地址；批次期间 Lease 持续续租；403 不阻塞其他资源，权限恢复后状态追平。
- NGINX 1.28.0 对照补斜杠响应头与连接关闭、If-Range 相等及不等日期、HTTP/1.0 无 Host 的 `$host` 展开。

状态 API 的延迟和拒绝在模拟 Kubernetes API 中精确注入；真实集群验收使用正常 API，覆盖资源更新、选主和滚动升级。

## 验证

Linux 编译和运行使用 OrbStack Ubuntu arm64。下表的 HTTP、NGINX 和模拟 API 验收使用从最终 `rgnix:0.1.0-qa14` 镜像提取的 release 二进制；真实集群运行同一镜像。调试二进制也通过了对应检查。

| 验证 | 结果 | 证据 |
|---|---|---|
| Rust fmt、clippy `-D warnings`、单元测试 | 通过，5/5 | [检查输出](validation/semantics-linux-checks.txt) |
| HTTP、TLS、代理、插件回归 | 59/59 | [结果](validation/semantics-http.json) |
| NGINX 1.28.0 行为对照 | 46/46 | [结果](validation/semantics-nginx.json) |
| 模拟 Kubernetes API 与真实 TLS 握手 | 39/39 | [结果](validation/semantics-recovery.json) |
| 真实 Kubernetes 生命周期 | 41/41；升级期间 300 次请求全部成功 | [结果及副本状态](validation/semantics-ingress.json)、[完整输出](validation/semantics-kubernetes.txt) |

完整 release 输出见 [验收记录](validation/semantics-release-checks.txt)。Helm lint、模板渲染、Shell 语法及 Python 语法检查通过。首次 Docker 镜像导出遇到内部 lease 失效，源码摘要核对后重试构建成功。

真实集群使用已有专用命名空间 `rgnix-qa-20260923`。结束时两个副本均使用 qa14 的镜像摘要、2/2 Ready，配置摘要一致，检查点健康值均为 1。Pod spec 指定 qa14；OrbStack 容器状态显示同摘要的 `rgnix:0.1.0` 别名，产物记录同时保存两者。验收环境保留，脚本关闭自身创建的 port-forward。地址回写使用保留示例地址，不代表公网 LoadBalancer 验证。

## 产物与复现

- 镜像：`rgnix:0.1.0-qa14`，本地 `rgnix:0.1.0` 指向同一产物，未推送远端。
- manifest list：`sha256:97e6717c5bc9c3937100e46cd20e385ecc4f29cbb2def68454ca68ed85c2f44b`。
- 二进制 SHA-256：`ced3d28cb3025408b1fa3d7ce3cb9f409da100dab89be5cc82b2ebfc797783cb`。
- 源树 SHA-256：`1250ccd37cc4b04b026b2599c20032a4fddb5acde040914a5056056151611944`。

[产物记录](validation/semantics-artifact.json) 同时包含 Cargo.lock 和验收脚本摘要。本轮未修改依赖或锁文件。源树摘要按相对路径排序，对 Cargo.toml、Cargo.lock 和 src 下的文件依次输入路径、NUL、文件原始内容。

```sh
# macOS / OrbStack Docker，从仓库目录执行
docker build -t rgnix:0.1.0-qa14 .
mkdir -p .local
artifact_container=$(docker create rgnix:0.1.0-qa14)
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
RGNIX_IMAGE_TAG=0.1.0-qa14 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

本轮未重新测量性能，也未完成 amd64、多节点、云 LoadBalancer 或长期压力验证。状态 API 的多资源故障注入在模拟 API 中完成，未对真实集群 API Server 注入延迟或拒绝。

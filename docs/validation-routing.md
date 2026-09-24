# 默认后端、请求语义与 EndpointSlice 归属修复

日期：2026-09-24，Asia/Singapore。本轮修复 qa12 审查中实际复现的四项缺陷，并增加 EndpointSlice 归属约束。原始观察见 [修复前证据](validation/routing-before.json)，上一轮记录见 [传输修复](validation-transport.md)。

## 问题与修复

| 问题 | qa12 的实际结果 | 修复后的行为 |
|---|---|---|
| defaultBackend 覆盖显式规则 | 同一 Ingress 同时定义 defaultBackend 和无 host 的 `Prefix /` 时，根路径与子路径都进入默认后端，并产生错误的路由冲突 | 默认后端使用独立兜底项，显式路径全部未命中后才选择它；同一或其他 Ingress 的根规则均优先 |
| `req.set_query` 连带改变路径 | `/a%2Fb?old=1` 只改 query 后变成 `/a/b?new=1`；重复斜杠和点段也被规范化 | 未显式改写路径且 proxy_pass 不带 URI 时，只替换 query，保留原始路径字节 |
| HTTP/1.1 Host 校验不足 | 缺失、空值或 `bad/path` 仍进入默认后端，返回 200；NGINX 返回 400 | 路由前校验 Host 的存在、唯一性和 authority 格式，异常返回 400 |
| 上游读超时状态码错误 | 超时返回 502，NGINX 返回 504 | 响应头发送前的上游连接、TLS 握手及读写超时映射为 504；其他上游传输错误仍为 502 |
| 同名 Service 重建后混入旧切片 | 新 Service UID 创建后仍使用归属旧 UID 的 EndpointSlice；同时存在新旧切片时流量分配到两者 | 带 Service ownerReference 的切片必须匹配当前 Service 名称及 UID；没有当前有效端点时返回 503 |

EndpointSlice 的 Service ownerReference 校验是本产品增加的归属约束，不能将 Kubernetes 基于 service-name 标签的关联规则表述为强制 UID 校验。没有 Service ownerReference 的手工切片仍可使用。资源摘要包含 ownerReference，因此仅修改归属、地址不变也会触发更新；它不能替代 EndpointSlice 写权限控制。

默认后端之间仍按创建时间、namespace、name 选择稳定优先级并报告冲突。带 host 的规则、无 host 的显式规则及默认兜底按既定顺序匹配。删除显式规则后恢复默认后端。

HTTP/1.0 可以省略 Host，HTTP/2 可使用 `:authority`，合法但未配置的主机仍进入默认虚拟主机。固定版本 Pingora 已有更严格的绝对 URI 校验：URI authority 与 Host 不一致时返回 400，而 NGINX 使用 URI 中的主机。本轮保留此行为，见 [兼容矩阵](compatibility.md)。查询修改与 `req.set_path` 或 proxy_pass URI 同时存在时，显式路径改写和 URI 替换继续生效。已经发出响应头的流式响应不能再改为 504，仍以终止传输处理。

## 验证

Linux 编译和运行使用 OrbStack Ubuntu arm64。下表的 HTTP、NGINX 和模拟 API 验收使用从最终 `rgnix:0.1.0-qa13` 镜像提取的 release 二进制；真实集群运行同一镜像。调试二进制也通过了对应检查。

| 验证 | 结果 | 证据 |
|---|---|---|
| Rust fmt、clippy `-D warnings`、单元测试 | 通过，5/5 | [检查输出](validation/routing-linux-checks.txt) |
| HTTP、TLS、代理、插件回归 | 55/55 | [结果](validation/routing-http.json) |
| NGINX 1.28.0 行为对照 | 40/40 | [结果](validation/routing-nginx.json) |
| 模拟 Kubernetes API 与真实 TLS 握手 | 35/35 | [结果](validation/routing-recovery.json) |
| 真实 Kubernetes 生命周期 | 41/41；升级期间 300 次请求全部成功 | [结果及副本状态](validation/routing-ingress.json)、[完整输出](validation/routing-kubernetes.txt) |

完整 release 输出见 [验收记录](validation/routing-release-checks.txt)。Helm lint、模板渲染、Shell 语法及 Python 语法检查通过。Kubernetes 日志中的 `Terminated: 15` 是脚本正常关闭自己创建的 port-forward。

真实集群使用已有专用命名空间 `rgnix-qa-20260923`。结束时两个副本均为 `qa13`、2/2 Ready，配置摘要一致，检查点健康值均为 1。测试临时创建的兜底、catchall 和 TLS 冲突资源已删除，原验收环境保留。地址回写使用保留示例地址，不代表公网 LoadBalancer 验证。

新增场景扩展已有验收脚本，覆盖以下可观察行为：

- 同一 Ingress 的默认后端与无 host 根规则共存；另一个 Ingress 的根规则覆盖更早创建的默认后端；显式路径未命中后继续使用兜底。
- 仅替换或清空 query 时保留编码斜杠、编码字符、连续斜杠、点段及编码问号；显式路径改写和 proxy_pass URI 替换仍有效。
- 原始 HTTP 请求中缺失、空值、非法值、重复 Host，以及绝对 URI 的 Host 校验；HTTP/1.0、带端口和 IPv6 Host 保持可用。延迟上游触发的读超时与 NGINX 均返回 504。
- Service UID 变化后撤销旧切片，只有新切片参与转发；只修改 ownerReference 时重新发布；名称不匹配的 Service owner 被拒绝；移除 Service owner 后手工切片仍可用。

EndpointSlice 归属变更的精确顺序通过模拟 API 注入，避免依赖真实集群垃圾回收的时序。

## 产物与复现

- 镜像：`rgnix:0.1.0-qa13`，本地 `rgnix:0.1.0` 指向同一产物，未推送远端。
- manifest list：`sha256:7fefed79dceb1e430a5fe109517623a30eec6b80ffcf16768bcd6834e418ed82`。
- 二进制 SHA-256：`491edc6b26e6d1ff814a5b4e00657dc837628cb5804875801ebabe636ecd7f35`。
- 源树 SHA-256：`a56ff850f72bd88abd8c27bed699dca101ecb85b4a31ad4f04f2cd0540e6300f`。

[产物记录](validation/routing-artifact.json) 同时包含 Cargo.lock 和验收脚本摘要。本轮未修改依赖或锁文件。源树摘要按相对路径排序，对 Cargo.toml、Cargo.lock 和 src 下的文件依次输入路径、NUL、文件原始内容。

```sh
# macOS / OrbStack Docker，从仓库目录执行
docker build -t rgnix:0.1.0-qa13 .
mkdir -p .local
artifact_container=$(docker create rgnix:0.1.0-qa13)
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
RGNIX_IMAGE_TAG=0.1.0-qa13 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

本轮没有重新测量性能，也没有完成 amd64、多节点、云 LoadBalancer 或长期压力验证。上游超时状态码的运行对照覆盖响应头前的读超时；连接、TLS 握手和写超时使用相同错误映射，但本轮未分别注入这些超时。

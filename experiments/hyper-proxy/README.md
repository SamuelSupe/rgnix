# Hyper 传输内核实验

独立 Cargo 项目，评估 HTTP/1 代理是否值得从 Pingora 迁移到 Hyper。它不是 rgnix 的可部署替代实现，也不会改变 `rgnix serve`、Ingress 或 Gateway 的默认行为。

[实际实验记录与原始数据](../../docs/validation-hyper-prototype-2026-09-28.md)记录了行为验证、吞吐观测及失败的 A/A 校准。当前不能宣称已稳定接近 NGINX。

## 编译与验证

在 Linux 上使用 Rust 1.90，release profile 与主项目一致；依赖由本目录的 `Cargo.lock` 锁定。

```sh
cargo build --release --locked --manifest-path experiments/hyper-proxy/Cargo.toml
python3 experiments/hyper-proxy/verify.py experiments/hyper-proxy/target/release/rgnix-hyper-prototype
experiments/hyper-proxy/target/release/rgnix-hyper-prototype \
  --listen 127.0.0.1:8080 --upstream 127.0.0.1:9000 --workers 1
```

原型固定转发到一个上游，保留原始 path/query，设置 `Host: localhost`，用于与 `examples/minimal_proxy.rs` 同场景比较。支持 HTTP/1 请求与响应流、HEAD、连接复用、逐跳头清理，以及多值响应头。Hyper 连接池最多保留 512 个空闲上游连接，空闲期限 60 秒。连接超时及上游响应头总期限均为 5 秒，下游头期限 60 秒；关闭自动重试。

**当前缺少** TLS、HTTP/2、WebSocket、trailer 契约、NGINX 配置、路由策略、RGL、认证、租户与请求预算、请求体大小上限、body 空闲读写超时、访问日志、指标、追踪和证书热更新。CONNECT / Upgrade 显式返回 501。退出时最多等待现有连接 5 秒，然后中断剩余连接；不等同于产品的优雅终止契约。请勿暴露到生产网络。

`verify.py` 使用真实 TCP HTTP/1 上游，验证 URI、逐跳头、多值 Cookie、连接复用、HEAD、8 MiB 定长/分块上传、双向流式行为及 POST 失败不重放；它不是完整 HTTP 协议符合性测试。

## 比较方法

现有 `scripts/benchmark_compare.py` 增加 `--hyper` 与 `--hyper-control`：两者接受同一二进制以做 A/A。最小代理仅参与 `proxy-1k` 和 `proxy-16k`，不会把缺少静态服务、脚本或 TLS 的场景误算成性能提升。

先在每组 worker/并发参数下，用相同二进制交替跑 A/A；随后交替测完整 rgnix、当前 vendor 的最小 Pingora、Hyper 原型与固定版本 NGINX。Rust 二进制必须使用相同工具链和 release profile。记录 RPS、CPU/request、P99、RSS/PSS、错误数、原始输出及二进制摘要。

即使原型接近 NGINX，也只能证明替代传输路径有潜力。完整产品需要迁回策略及超时，再使用相同配置验证；不能直接将省略产品功能后的差异计为收益。

## 迁移边界

| 现有模块 | 迁移工作 |
|---|---|
| `config`、`routing`、RGL 编译/执行、后端选择、租户策略 | 多数逻辑可保留；将输入输出连接到新请求生命周期 |
| `proxy/mod.rs`、`proxy/body.rs`、`proxy/responses.rs` | 重接请求/响应 hook、前缀读取后回放、限流 permit、后端 lease、直接响应和静态流；取消或报错也必须释放资源和记账 |
| `runtime.rs`、证书、`upstream.rs` | 监听器、OpenSSL/SNI/ALPN、mTLS 校验、连接池按信任配置隔离、证书撤销后禁止复用旧连接 |
| `compression.rs`、`proxy_protocol.rs` | 重接压缩流、PROXY protocol；保留 SSE/gRPC、Range、no-transform 等边界 |
| 管理接口、admission、后台 controller | 将 Pingora 服务生命周期迁到 Tokio；保留审计、watch、持久化及 drain 行为 |
| 日志、指标、trace、健康检查 | 建立连接、首字节、body 完成、异常、取消事件的准确映射，不能因响应头已发出就提前完成请求记账 |

合理顺序是先证明普通代理的收益，再迁移限定 HTTP/1 路由并运行现有验收，最后处理 TLS/H2/gRPC/WebSocket 和控制面。当前阶段不增加通用传输抽象或双内核生产选项。

API 依据：[Hyper 服务端](https://hyper.rs/guides/1/server/hello-world/)、[客户端连接](https://hyper.rs/guides/1/client/basic/)、[hyper-util 0.1.20 连接池与重试](https://docs.rs/hyper-util/0.1.20/hyper_util/client/legacy/struct.Builder.html)。

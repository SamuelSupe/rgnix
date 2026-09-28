# 替换 Pingora：Hyper HTTP/1 原型实验

从 v0.5.0 的 `b1220b83decd1289ecca3584d1a08f75559bb186` 建立 `experiment/replace-pingora` 分支。新增 [独立 Hyper 原型](../experiments/hyper-proxy/README.md)，正式 rgnix 数据面未更换。

**当前判断：值得继续做产品集成实验，但尚未证明替换后能稳定接近 NGINX。** 普通 HTTP/1 转发原型可以运行，探索样本中吞吐高于最小 Pingora；同二进制校准失败，且原型缺少产品策略、TLS/H2 与完整超时契约，不能据此承诺完整 rgnix 的收益。

## 实现与行为检查

原型使用 Hyper 1.11.1、hyper-util 0.1.20 和 Tokio；独立锁文件，不引入正式产品依赖。使用真实 HTTP 解析与流式 body，连接池保留 HTTP 连接状态，关闭请求重试，没有缓存响应或硬编码基准响应。

OrbStack Linux ARM64 原生运行通过 7 组行为检查：10 次 keepalive 请求复用同一上游连接；原始 path/query、重复 Cookie 与逐跳头；HEAD；8 MiB 定长上传；8 MiB 分块上传在客户端发送完前到达上游；流式/分块响应；POST 在上游收完 body 后断连只发送一次并返回 502；不支持的 Upgrade 显式返回 501。若干检查合并在同组，完整列表见 [验证记录](validation/hyper-prototype-verify-2026-09-28.json)。最终代码通过 `cargo fmt --check`、release 编译和 Clippy `-D warnings`。

这不是完整产品验收：未验证慢读者内存上界、客户端取消后的上游及时释放、早期响应、trailer、TLS/H2/gRPC/WebSocket、证书撤销、RGL、租户隔离、更新/排空或 Ingress/Gateway。没有运行完整产品回归套件；该分支没有改动生产请求路径。

## 首轮探索对照

环境：共享 OrbStack Ubuntu ARM64，宿主 Apple M5 Max；私有网络命名空间和 loopback、tmpfs 文件。Rust 三个程序都由 Rust 1.90.0 / LLVM 20.1.8、thin LTO、codegen-units=1、同级 release 优化编译。完整 rgnix 和最小 Pingora 从同一基线源码构建；最小 Pingora 使用本项目 vendor 补丁。NGINX 固定 1.30.5，OpenResty 1.31.1.1 仅作共同上游。

代理 1 worker / CPU 10；wrk 4 threads / CPU 2–5；共同上游 4 workers / CPU 6–9；64 连接，3 轮交替，每窗预热 2 秒、测量 8 秒。vCPU affinity 不等于保留物理核心。访问日志和 OTLP 关闭，完整 rgnix 保留产品预算和指标；两种最小代理不具备这些功能。Hyper 的连接/响应头期限与 Pingora 的读写空闲超时也不等价。

下面仅列观测中位数。**A/A 未通过，所有行均不构成稳定性能结论。**

| 场景 | 程序 | 请求/秒 | CPU μs/请求 | P99 ms | RPS CV |
|---|---|---:|---:|---:|---:|
| 1 KiB 代理 | 完整 rgnix | 40,529 | 24.67 | 3.581 | 28.05% |
| 1 KiB 代理 | 最小 vendor Pingora | 58,061 | 17.18 | 2.531 | 19.00% |
| 1 KiB 代理 | Hyper 原型 | 91,792 | 10.77 | 2.496 | 29.84% |
| 1 KiB 代理 | NGINX | 119,419 | 8.37 | 1.536 | 18.44% |
| 16 KiB 代理 | 完整 rgnix | 42,778 | 23.38 | 3.062 | 6.94% |
| 16 KiB 代理 | 最小 vendor Pingora | 52,552 | 19.03 | 2.486 | 8.67% |
| 16 KiB 代理 | Hyper 原型 | 87,292 | 11.44 | 1.568 | 31.84% |
| 16 KiB 代理 | NGINX | 58,465 | 17.04 | 2.231 | 6.78% |

不能将 16 KiB 中位数解读为超过 NGINX：Hyper 三窗分别约 87,292 / 87,813 / 46,770，请求/秒大幅变化。1 KiB 的 Hyper/NGINX 中位数比例约 77%，首轮约 90%，说明挑某一窗口会产生误导。NGINX 一个窗口出现 12.332 ms P99，表中 P99 中位数也不能掩盖它。

前置 A/A 的相同二进制分为两个标签、轮换顺序，沿用最大对称配对差和组内 CV 均不超过 10% 的门槛。每轮配对差为 `abs(a-b) / ((a+b)/2) × 100%`，CV 使用样本标准差除以均值：

| 程序 | 最大配对差 | 两组 CV | 结果 |
|---|---:|---|---|
| rgnix | 40.92% | 23.33% / 8.53% | FAIL |
| Hyper | 49.43% | 16.60% / 13.75% | FAIL |

首轮 12 个 A/A 窗口共 5,860,065 请求；24 个探索对照窗口共 12,856,459 请求，均无 wrk 错误。计数不含预热、preflight 和上游基线。A/A 失败后保留的对照仅用于探索，不用于归因或发布门槛。

初次启动上游 fixture 放在用户 home 下，NGINX worker 无权穿过权限为 750 的父目录，返回 403；移到专用 `/tmp` 后解决。这次启动失败没有产生测量窗口，也没有放宽用户目录权限。

## 降低负载后的复测

将 wrk 和上游各减为 1 worker，分别绑定 CPU 2 和 6；代理保持 1 worker / CPU 10、64 连接。每窗预热 3 秒、测量 15 秒，3 轮；两个 rgnix 标签使用相同二进制，两个 Hyper 标签使用相同最终构建，按 `rgnix candidate hyper hyper-control` 开始轮换。该配置与首轮不同，不跨组比较 RPS。

| 程序 | 最大配对差 | 两组 CV | 结果 |
|---|---:|---|---|
| rgnix | 17.36% | 12.13% / 2.37% | FAIL |
| Hyper | 28.06% | 9.07% / 6.93% | FAIL |

这次仍未通过既定 10% 门槛，复测脚本在门槛处返回 2，**未继续运行第二次四方对照**。没有剔除首轮、改变门槛或将当前主机的波动归咎于某个未经证实的原因。原始记录见 [低负载 A/A](validation/hyper-prototype-aa-refined-2026-09-28.json)，统计合并到 [门槛计算](validation/hyper-prototype-analysis-2026-09-28.json)。多 worker、其他并发和专属物理机上的结论仍未验证。

复测 12 窗共 15,575,102 请求、零 wrk 错误；连同首轮，两次校准与探索对照合计 48 窗、34,291,626 请求、零 wrk 错误。无错误不代表校准通过，也不替代协议和产品完整性验证。

## 复现

构建基线 `rgnix` 和 `minimal_proxy`，以及原型，使用同一工具链；程序路径以实际构建结果替换。以下共同参数对应首轮：

```sh
python3 scripts/benchmark_compare.py \
  --rgnix /path/to/rgnix --pingora /path/to/minimal_proxy \
  --hyper /path/to/rgnix-hyper-prototype \
  --nginx /path/to/nginx --openresty /path/to/openresty \
  --engines rgnix pingora hyper nginx \
  --rounds 3 --seconds 8 --warmup 2 --concurrency 64 \
  --cases proxy-1k proxy-16k --interleave \
  --workers 1 --origin-workers 4 --client-threads 4 \
  --server-cpus 10 --client-cpus 2,3,4,5 --origin-cpus 6,7,8,9 \
  --work-dir /tmp/new-hyper-fixture --output /tmp/hyper-compare.json
```

正式解释对照结果前必须先校准：传入 `--candidate` 指向同一 rgnix 二进制、`--hyper-control` 指向同一 Hyper 二进制，选择 `--engines rgnix hyper candidate hyper-control --cases proxy-1k`，使用新的 fixture 和 output 路径。未通过不发布收益结论。完整调用、配置、版本、摘要和各窗口输出保存在原始 JSON 中。

## 产品迁移判断

Hyper 可以承担替代 HTTP 协议内核的角色，本原型证明了基本流式代理路径可落地。但“删掉 Pingora 依赖”不是当前工作量的主体：`proxy/mod.rs` 有请求/响应、body、错误和 logging 生命周期耦合；`runtime.rs`、压缩、PROXY protocol、管理服务及上游证书对象也依赖 Pingora。

配置模型、路由匹配、后端选择、RGL 编译与多数策略可以保留。下一步应将这些真实产品逻辑接入限定 HTTP/1 路径，保证请求快照、前缀 body 回放、permit/lease 释放、日志与 trace 完成时机；随后补齐完整超时、TLS 信任域连接池、H2/gRPC/WebSocket 与更新排空，再进行同功能比较。详细模块边界在 [原型说明](../experiments/hyper-proxy/README.md#迁移边界)。

当前不建议立即删除生产 Pingora，也不引入自研 HTTP parser 或 epoll 服务器。候选核心带来的收益与省略策略/超时的收益尚未分离，多 worker 扩展性也尚未测量。

原始证据：[A/A](validation/hyper-prototype-aa-2026-09-28.json)、[探索对照](validation/hyper-prototype-compare-2026-09-28.json)、[门槛计算](validation/hyper-prototype-analysis-2026-09-28.json)、[构建身份](validation/hyper-prototype-build-2026-09-28.json)。首轮使用格式化前的原型构建；最终格式化后重新编译，复测使用新的摘要，两者没有功能差异，摘要分别记录。

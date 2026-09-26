# RGL XDP 验证记录 — 2026-09-26

本轮验证基于 v0.3.0 后的当前工作区实现；未发布新的 GitHub 版本。Linux 二进制 SHA-256 与逐项结果保存在 [XDP 运行记录](validation/xdp-2026-09-26.json)。

## 实际运行环境

- OrbStack Ubuntu，Linux `7.0.14-orbstack-00380-ga7e0a2dc9535`，aarch64。
- Rust 1.90；Ubuntu Clang 20.1.8，`bpfel`、`-mcpu=v3`。
- 真实 Linux BPF verifier、`BPF_PROG_TEST_RUN`、BPF link create/update。
- 内核配置确认 `CONFIG_BPF_JIT=y`、`CONFIG_BPF_JIT_ALWAYS_ON=y`，实际测试启用 JIT。
- 流量验证使用专用 veth 和网络命名空间，完成后删除；未向现有物理网卡、CNI 或业务节点加载程序。

## 结果

| 验证 | 结果 |
| --- | --- |
| `cargo test --locked -j 2` | 12 项通过 |
| `scripts/integration.py` | 原有 HTTP/RGL 集成 82 项通过 |
| `scripts/xdp_integration.py` | 37 项通过，详见 JSON |
| `cargo clippy --locked --all-targets -j 2 -- -D warnings` | 通过 |
| `cargo fmt --check`、`git diff --check` | 通过 |
| XDP DaemonSet `kubectl create --dry-run=client` | 客户端清单校验通过；未实际部署 |

XDP 测试覆盖 RGL 类型/能力拒绝、失败编译不覆盖已有产物、HTTP 插件不能混入 XDP 入口、IPv4/IPv6 CIDR、TCP/UDP、SYN/SYN-ACK、IPv4 options、IPv6 扩展头、VLAN、分片及畸形/截断报文、固定窗口包预算与计数。

真实 veth 流量验证覆盖 generic/native 两种模式、现有附着独占保护、坏对象及内核校验失败保留旧策略、原子更新、相同产物更新不重置版本、RGL 源码直接更新、SIGTERM/SIGKILL 自动卸载，以及接口删除后进程退出。

主代理测试进程用 `setpriv` 限制为 `BPF`、`NET_ADMIN`、`PERFMON`，并启用 `no_new_privs`。仅 BPF/NET_ADMIN 的配置在本内核上因指针操作校验被拒绝，已据此修正示例权限。没有使用 SYS_ADMIN 运行代理；测试工具创建网络命名空间仍由测试 VM 的 root 执行。

## 尚未验证

- 物理网卡 native XDP 的吞吐、P99、CPU 收益，以及硬件 offload。
- Linux amd64 的实际内核执行、其他内核/Clang 版本及生产多节点部署。
- DaemonSet 实际滚动升级、集群准入策略及定制 seccomp 配置。
- 与 Cilium/libxdp 的程序链共存；当前会明确拒绝占用已有 XDP 的接口。

这是一轮功能与生命周期验证，不构成性能提升百分比或生产容量承诺。原子 SIGHUP 不存在卸载间隙；进程重启/退出会移除过滤器，恢复普通网络路径。

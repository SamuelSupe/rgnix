# 本地日志轮转验证

> 阶段历史记录：下文保留验证当时的版本、镜像和发布状态。后续 v0.2.0 发布包的实际验收与摘要见[发布验证](validation-release-0.2.0.md)。

日期：2026-09-25，Asia/Singapore。新增访问/错误日志文件轮转、gzip、保留策略及 USR1 重开；[配置与运行边界](log-rotation.md)。当前是未提交源码改动，未推送 GitHub 或替换已发布的 v0.1.0 下载包。

## 实际结果

| 检查 | 结果 |
|---|---|
| Rust 测试、fmt、Clippy all-targets | 6/6，格式及 `-D warnings` 通过；[完整输出](validation/log-rotation-linux-checks.txt) |
| HTTP/TLS/代理/插件回归 | 79/79；[记录](validation/log-rotation-http.json) |
| 模拟 Kubernetes API/恢复 | 45/45；[记录](validation/log-rotation-recovery.json) |
| OTLP 网络/故障回归 | 32/32，含本地日志共存和终止刷新；[记录](validation/log-rotation-otlp.json) |
| 新增轮转验收 | debug 20/20；镜像提取的 release 二进制 20/20；[debug](validation/log-rotation-files.json)、[release](validation/log-rotation-image-files.json) |
| 非 root 容器实际运行 | 4/4：挂载卷写入/轮转、USR1、SIGTERM、gzip 解码；[记录](validation/log-rotation-container.json) |
| Helm 与示例 | `helm lint charts/rgnix` 通过；创建示例日志目录后，`rgnix check -c examples/logging.conf` 通过 |

debug 与脚本运行环境为 OrbStack Ubuntu Linux arm64、Rust 1.90.0、logrotate 3.22.0。release 镜像使用仓库 Dockerfile 的 `rust:1.98-bookworm` 构建，镜像名 `rgnix:0.1.0-rotation-qa`。构建使用已有 BuildKit `build_ca` secret，未关闭证书校验。

容器验收实际使用镜像默认 UID/GID **10101:10101** 和独立可写 Docker volume；结束后删除本轮临时容器及卷。没有修改此前 Body/OTLP 验收的 Kubernetes 命名空间。镜像、二进制、源码、锁文件及脚本摘要见 [log-rotation-artifact.json](validation/log-rotation-artifact.json)。

## 覆盖的行为

- 8 个并发客户端产生 120 条访问记录，跨多个 gzip 归档核对每条出现一次，记录未在文件间拆分。
- 大小阈值、单条超长记录、保留文件权限、时间段切换；修改保留数量和关闭策略后行为更新。
- 无效轮转策略的 SIGHUP 被拒绝，原有策略继续生效；清理不删除无关文件或符号链接目标。
- 访问/错误日志使用同一路径时共用 writer；退出时队列中的记录写完。
- 目录变为不可写时轮转失败，仍向旧文件追加并增加 I/O 错误指标；恢复目录权限后再次轮转。
- USR1 同时重开访问与错误文件；有效 SIGHUP 也重开。路径被目录占用时保留旧句柄，修复后再次 USR1 恢复。
- 真实 logrotate 完成两轮 rename/create/USR1 和延迟压缩，检查 `.2.gz`、`.1` 和当前文件的记录归属。
- 向不被读取的 FIFO 写日志，4300 个 HTTP 请求仍返回 200；队列有界、满时丢弃并计数。释放 FIFO 后，排队期间收到的 USR1 仍生效。
- stdout/stderr 句柄指向权限为 000 的文件时，配置检查、日志写入和 USR1 仍使用已继承的可写句柄。该场景最初复现了重新打开 `/dev/stderr` 导致 `Permission denied` 的兼容问题，修复后纳入上述 20 项检查。

## 复现

```sh
# Ubuntu 中预先安装构建依赖和 logrotate
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target bash scripts/check.sh'
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && CARGO_TARGET_DIR=/tmp/rgnix-target cargo clippy --locked --all-targets -- -D warnings'
helm lint charts/rgnix

# 单独运行文件轮转验收，可传入 debug 或 release 二进制
orb -m ubuntu sh -lc 'cd /PATH/TO/rgnix && python3 scripts/log_rotation.py /tmp/rgnix-target/debug/rgnix'
docker build -t rgnix:0.1.0-rotation-qa .
```

轮转脚本需要使用普通用户运行，才能实际验证目录权限失败；使用临时目录、动态端口和自身子进程，结束后清理。需附加企业构建 CA 时按[部署文档](deployment.md)传入 BuildKit secret。

## 未验证范围

本轮没有重跑真实 Kubernetes 生命周期或 NGINX 差分；Ingress 默认仍写 stdout/stderr，没有新增日志 PVC 或 Ingress 文件路径参数。没有执行 amd64、NFS/其他特殊文件系统、ENOSPC、断电/进程崩溃恢复、systemd 定时调度或长时间吞吐/磁盘容量测试。所记录的故障恢复针对目录权限和路径占用，不代表所有存储故障都不会丢日志。

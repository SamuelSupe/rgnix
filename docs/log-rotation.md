# 本地日志与轮转

v0.2.0 起提供此能力。独立服务通过 `access_log`、`error_log` 指定本地文件，通过 `rgnix_log_rotation` 开启内置轮转；也支持外部 logrotate 重命名后发送 **SIGUSR1** 重开文件。本地访问日志可以与 [OTLP 导出](otlp.md) 同时启用。

## 内置轮转

```nginx
events {}
http {
    access_log /var/log/rgnix/access.log combined;
    error_log /var/log/rgnix/error.log warn;
    rgnix_log_rotation size=100m interval=1d keep=7 gzip=on;

    server {
        listen 8080;
        location / { return 200 "ok"; }
    }
}
```

先创建日志目录并授予运行用户写权限。仓库提供[可运行示例](../examples/logging.conf)：从仓库根目录执行 `mkdir -p .local/logs`，再执行 `rgnix serve -c examples/logging.conf`。

| 配置 | 契约 |
|---|---|
| 上下文 | 仅 `http`；统一作用于本进程所有显式配置的普通访问日志、错误日志文件 |
| 默认 | `rgnix_log_rotation off;`，仍正常追加日志；开启时必须设置 size 或 interval |
| `size=100m` | 在追加下一条记录将超过阈值时轮转；单位为字节、k/m/g（1024 进制，不区分大小写），范围 1 字节..1 TiB |
| `interval=1d` | 按 Unix 时间对齐的 UTC 时间段轮转；单位 s/m/h/d，无单位为秒，范围 1 秒..365 天；例如 1d 在 UTC 午夜之后有新记录时轮转 |
| 两种阈值 | 可单独配置，也可组合；任意一种到达即触发 |
| `keep=7` | 每个文件保留最近 7 份历史归档，不包含当前文件；范围 1..1000，默认 7；下次轮转时执行清理 |
| `gzip=on` | 历史归档压缩为 `.gz`；默认 off；当前文件始终是可追加的文本文件 |

空文件不轮转；一条记录不会被拆到两个文件，因此单条大于 size 的记录会使文件超过阈值。时间轮转由下一条记录触发，空闲时不会生成空归档。首次打开非空文件时，以其修改时间判断所属时间段。

历史文件使用 `access.log.rgnix.00000000000000000001[.gz]` 形式，序号延续已有归档。保留策略只管理对应文件的 `.rgnix.` + 20 位数字 + 可选 `.gz` 后缀，其他文件与符号链接不清理。该后缀是 rgnix 保留的归档命名空间。

访问与错误日志写到同一路径时共用一个后台 writer，避免两套轮转相互覆盖。每个实际文件应只由一个 rgnix 进程、一个一致的配置路径管理；不要让多个 Pod 共享同一日志文件，也不要为同一文件使用不同路径别名。stdout/stderr、设备、FIFO 及配置为符号链接的路径不会被自动轮转。

轮转时保留原有文件权限位；新文件由当前运行用户创建，不做 chown。目录需要创建、替换和删除文件的权限。先保存旧文件，再原子替换当前路径；创建/替换失败时继续使用旧文件并记录错误。压缩失败会保留未压缩归档。错误指标应被监控，因为持续文件系统错误会使大小及保留数量暂时超出策略。

## 热更新与外部 logrotate

- **SIGHUP**：独立模式验证新配置，成功后应用轮转策略并重开文件；无效配置保留上一有效版本。
- **SIGUSR1**：仅重开已缓存的日志文件，不解析配置或重启监听。重开失败保留旧句柄继续写；修复路径后再次发送即可恢复。
- 重开在后台处理，可能存在短暂延迟；外部压缩应使用 `delaycompress`，让刚改名的文件在下一轮再压缩。

使用外部 logrotate 时，对相同文件关闭内置轮转：

```nginx
rgnix_log_rotation off;
```

例如服务由名为 `rgnix.service` 的 systemd unit 管理时，可按[外部轮转示例](../examples/logrotate.conf)配置：

```text
/var/log/rgnix/*.log {
    daily
    rotate 7
    missingok
    notifempty
    compress
    delaycompress
    sharedscripts
    postrotate
        /bin/systemctl kill --kill-whom=main --signal=USR1 rgnix.service
    endscript
}
```

示例让 rgnix 在重开时创建新文件，目录必须可写；若使用 logrotate 的 `create`，请明确设置实际运行用户/组。服务名按部署修改。rgnix 不自行创建 PID 文件；其他进程管理器应向其记录的 rgnix 主进程 PID 发送 USR1。此仓库不安装 systemd unit 或 logrotate 定时任务，需要部署方配置调度。

协议参考：[NGINX 日志重开](https://nginx.org/en/docs/control.html#logs)、[logrotate 手册](https://github.com/logrotate/logrotate/blob/main/logrotate.8.in)。

## 缓冲、终止与观测

访问日志和显式 `error_log` 共用 **4096 条**记录的有界队列。文件写入、轮转、压缩和清理在一个后台线程执行；队列满时丢弃新记录，不等待磁盘。压缩大文件或慢盘可能导致队列填满。未显式配置的错误日志仍由默认 logger 写 stderr。

优雅终止先排空请求和 OTLP，再给本地队列最多 **5 秒**排空时间。文件写入不逐条 fsync；SIGKILL、进程崩溃、队列满或磁盘故障可能丢失日志，因此不提供持久化审计保证。Helm 默认终止宽限为 **55 秒**，覆盖 preStop、请求排空与两条日志队列的刷新预算。

| 指标 | 含义 |
|---|---|
| `rgnix_log_rotations_total` | 完成的本地文件轮转数 |
| `rgnix_log_reopens_total` | 成功重开的文件数 |
| `rgnix_log_io_errors_total{operation}` | open/write/rotate/reopen/compress/cleanup 失败数 |
| `rgnix_access_logs_dropped_total` | 本地访问日志队列满或打开/写入失败丢弃数 |
| `rgnix_error_logs_dropped_total` | 显式错误日志队列满或打开/写入失败丢弃数 |
| `rgnix_file_logs_pending` | 排队或正在写入的记录数 |
| `rgnix_file_logs_shutdown_pending_total` | 退出排空期限到达时仍未完成的记录数 |

指标没有日志路径标签。文件 I/O 错误会限频写到进程 stderr，避免把故障日志递归写回同一队列。OTLP 使用独立队列和指标，本地文件故障不停止 OTLP 导出。

Ingress 模式及 Helm 仍默认输出 stdout/stderr，由容器运行时负责轮转；本次未添加 Ingress 文件路径参数或日志 PVC。独立模式在容器内写文件时，需要挂载可写目录，镜像运行用户为 UID/GID 10101。

实际测试和未验证边界见[轮转验证记录](validation-log-rotation.md)。

# NGINX 配置兼容矩阵

本表描述 rgnix 当前源码的实际实现。H/S/L 分别表示 `http`、`server`、`location`；“继承”指内层未声明时采用外层值。未知指令、不支持的参数或上下文直接失败，不静默忽略。词法和上下文错误包含源文件、行、列；插件/证书等加载错误还包含相关路径。

## 结构与路由

| 指令 | 上下文、参数 | 语义及限制 |
|---|---|---|
| `include` | 任意配置层，1 个路径/glob | 按路径排序展开；相对主配置目录；无匹配 glob 允许，缺失的普通文件报错；深度最多 32，拒绝环；单文件最多 4 MiB |
| `events {}` | main，无参数 | 仅允许空块；无 worker_connections 等进程/事件参数 |
| `http {}` | main，无参数 | 必须有且仅有一个，至少一个 server |
| `server {}` | H，无参数 | 一个或多个；不支持 stream/mail |
| `upstream NAME {}` | H，1 个名称 | server 列表必须非空；支持 least_conn/ip_hash 和 rgnix 调度扩展；不支持 zone/keepalive |
| upstream `server ADDRESS [weight=N]` | upstream，1–2 个参数 | 默认端口 80、权重 1；权重 1..65535；IPv4/IPv6，域名按 TTL 刷新；不支持 backup/down/max_fails 等 |
| `listen PORT\|IP:PORT [ssl] [default_server] [proxy_protocol]` | S | 默认 `0.0.0.0:80`；IPv6 使用 `[::]:8080`；PROXY 须声明可信 CIDR；不支持主机名、Unix socket、reuseport、旧式 `listen ... http2` 等 |
| `server_name NAME...` | S | 精确、前导 `*.` 通配名、`_`；大小写不敏感；NGINX 通配名可跨多个标签；无 regex、尾部通配或 `.example.com` 特殊语义 |
| `location PATH {}` / `location = PATH {}` | S | PATH 必须以 `/` 开头；精确优先，其次最长字符串前缀；无嵌套、正则、`^~`、命名 location |

同地址 server 的 TLS 和 http2 选项必须一致。默认虚拟主机为该监听的第一个 server，或显式 `default_server`；重复 default_server 报错。同监听上重复的 server_name 保留先定义的主机。Host 精确匹配优先于最长通配后缀。未命中 location 时使用 server 的隐式 `/` 路由。

请求匹配使用 UTF-8、百分号解码、合并重复 `/`、消除 `.` 和 `..` 后的路径；无效编码、NUL、反斜线及逃逸根的路径返回 400。v0.1 不支持 NGINX 接受的任意非 UTF-8 路径字节。

HTTP/1.1 必须有且仅有一个合法、非空的 Host；缺失、重复或非法值在路由前返回 400，绝对 URI 请求也会校验 Host。HTTP/2 支持 `:authority`，HTTP/1.0 可以省略 Host。合法但未配置的主机名仍使用默认虚拟主机。Pingora 会拒绝 Host 与请求目标 authority 不一致的请求；NGINX 对绝对 URI 使用目标主机并忽略不同的 Host，此处保留 Pingora 更严格的校验。

## 代理

| 指令 | 上下文、参数 | 默认值、继承与差异 |
|---|---|---|
| `proxy_pass URL` | L，1 个固定 http/https URL | 支持命名 upstream 与地址；URI 可选；拒绝变量、用户名密码、fragment 和 URL query；查询修改用 RGL `req.set_query` |
| `proxy_set_header NAME VALUE` | H/S/L，2 参数 | 当前层出现任意一条则替换整组继承；空值删除。默认 Host 为上游 authority；Pingora 自动连接复用，与 NGINX 默认 `Connection: close` 不同 |
| `proxy_connect_timeout TIME` | H/S/L，1 参数 | 60s，继承 |
| `proxy_read_timeout TIME` | H/S/L，1 参数 | 60s，继承；也用作已匹配请求的下游读超时 |
| `proxy_send_timeout TIME` | H/S/L，1 参数 | 60s，继承；也用作下游写超时 |
| `proxy_http_version 1.1\|2\|auto` | H/S/L，1 参数 | 默认 1.1；2 支持 TLS ALPN H2 和明文 prior-knowledge h2c；auto 仅 TLS 协商 H2/H1 |
| `proxy_ssl_name NAME` | H/S/L，1 参数 | 固定 SNI/验证名；默认 URL 主机或 upstream 名，无变量 |
| `proxy_ssl_trusted_certificate FILE` | H/S/L，1 参数 | 自定义 PEM CA，继承；默认系统信任根；连接池按 CA 和协议隔离 |
| `proxy_ssl_verify on` / `proxy_ssl_server_name on` | H/S/L，1 参数 | 始终开启，off 拒绝 |
| `client_max_body_size SIZE` | H/S/L，1 参数 | 1 MiB，继承；0 为不限制；支持字节/k/m/g（大小写均可）；限制 HTTP 请求体，不限制已完成升级的 WebSocket 累计流量 |
| `keepalive_timeout TIME` | H/S/L，1 参数 | 75s，继承；0 关闭下游 keepalive；仅调整协议允许复用的连接，不重新开启解析器已禁止的复用；应用到上游空闲连接；不支持第二个 header_timeout 参数；下游非整秒值向上取整 |

TIME 为非负整数，可带 `ms`、`s`、`m`；无单位按秒。`proxy_pass https://group` 的默认 SNI/验证名为 group 名，可用 `proxy_ssl_name` 覆盖。直接 URL 默认使用 URL 主机名验证。CA 和主机名验证始终开启；可以配置 CA 文件或使用 OpenSSL 的 `SSL_CERT_FILE`。

在响应头尚未发送时，上游连接、TLS 握手和读写超时返回 504；其他上游传输错误返回 502。已经开始的流式响应无法改写状态码，仍以终止传输处理。

URI 示例（location `/api/`，请求 `/api/a%20b?x=1`）：

| 配置/插件 | 上游 URI |
|---|---|
| `proxy_pass http://app;` | `/api/a%20b?x=1`，保留原始转义 |
| `proxy_pass http://app/base/;` | `/base/a%20b?x=1`，替换匹配的规范化路径前缀 |
| `proxy_pass http://app/base;` | `/basea%20b?x=1`，不补 `/` |
| `req.set_path("/new path")` | `/new%20path?x=1`，覆盖最终路径，不重匹配 location |
| `req.set_query("q=2")` | 未显式改写路径且 proxy_pass 不带 URI 时，保留原始路径转义、重复斜杠和点段；使用 `q=2`，空字符串清空 query |

以 `/` 结尾的代理 location 对对应无尾斜杠路径返回 301，并使用该 location 的响应头及 keepalive 配置；精确 location 可覆盖该行为。相对 Location、重定向响应体、Server 头与 NGINX 不要求逐字节相同。差分验收比较状态、Location 的路径/query、正文及相关头。

请求/响应流式转发，无磁盘缓冲或响应缓存。WebSocket 保留升级所需头；SSE 无整包聚合。客户端 TLS ALPN 支持 HTTP/2，上游可用 H1/H2/h2c，支持 gRPC 双向流和 trailers；无 HTTP/3。默认加权轮询，也支持最少活动请求、哈希、黏性 Cookie、主动 HTTP 健康检查及被动故障隔离。失败请求最多尝试一个上游；已发送的部分流无法回滚。

HTTP/1.1 请求同时带 Transfer-Encoding 和 Content-Length 时，沿用 Pingora 的分帧校验和连接关闭决定：接受的 chunked 请求移除冲突 Content-Length，响应后关闭连接。正常 chunked 请求可继续复用；HTTP/1.0 未显式请求 keep-alive 时关闭连接。

## 静态文件、响应与日志

| 指令 | 上下文、参数 | 默认值、继承与差异 |
|---|---|---|
| `root PATH` | H/S/L，1 个固定目录 | 主配置目录下的 `html`，继承；不支持变量；显式 root 清除继承 alias |
| `alias PATH` | L，1 个固定路径 | 用目录或精确文件替换匹配前缀，保持目录能力边界 |
| `try_files CANDIDATE... FALLBACK` | H/S/L，2–8 参数 | `$uri`、`$uri/`、固定绝对路径候选；末项固定路径或 `=400..599`；不做内部 location 重匹配 |
| `gzip on\|off` / `brotli on\|off` | H/S/L | 默认 off；流式压缩，遵守 Accept-Encoding 质量值；完整限制见入口策略文档 |
| `gzip_comp_level N` / `brotli_comp_level N` | H/S/L，1 参数 | 1..9；显式 level 启用算法；on 默认分别 1/4，与 NGINX 的独立启用语义不同 |
| `gzip_min_length SIZE` / `gzip_types MIME...` | H/S/L | 默认 1024 字节；类型整组覆盖且包含 text/html；共同应用于 gzip/Brotli |
| `index NAME...` | H/S/L，非空文件名列表 | `index.html`，整组替换；不支持带 `/` 的索引 URI、内部重定向或 autoindex 文件列表 |
| `types { MIME EXT...; }` | H/S/L | 默认 html/css/js；内层整表替换；允许空表。建议 include 显式 MIME 表 |
| `default_type MIME` | H/S/L，1 参数 | `application/octet-stream`，继承；NGINX 默认是 `text/plain` |
| `return STATUS [TEXT]` | S/L，1–2 参数 | STATUS 200..599；同层首个 return 终止处理，不被 proxy_pass 覆盖；server 级先于 location 匹配、请求体大小检查及插件执行，使用 server 的响应头；文本支持变量；3xx 非空文本作为 Location；204/304 无正文；不支持 444 和省略状态的 URL 简写 |
| `add_header NAME VALUE [always]` | H/S/L，2–3 参数 | 当前层出现任意一条则替换继承组；否则继承；always 包含代理自身产生的错误响应；无 always 时仅作用于 200/201/204/206/301/302/303/304/307/308 |
| `access_log PATH [combined]` / `access_log off` | H/S/L | 默认 `/dev/stdout`，继承；UTC combined 后追加 route/backend/upstream/config 字段，无自定义 log_format 或 NGINX buffer/gzip 参数；后台有界队列，满或文件 I/O 失败时丢弃并计数 |
| `error_log PATH [LEVEL]` | **仅 H** | 未配置时 stderr，`RUST_LOG` 控制；显式 LEVEL 默认 error，可取 error/warn/info/debug；不支持分层 error_log 和 syslog |
| `rgnix_log_rotation off` 或 `size=N interval=TIME keep=N gzip=on\|off` | **仅 H** | 默认 off；开启时 size/interval 至少一个，keep 默认 7、gzip 默认 off；全局作用于普通文件日志，支持 SIGHUP 更新和 USR1 重开；rgnix 扩展，[完整语义](log-rotation.md) |

静态文件仅 GET/HEAD，支持 MIME、ETag、Last-Modified、If-None-Match、If-Modified-Since、If-Range 和单个字节 Range；多段 Range 忽略后返回完整文件。无索引目录返回 403，缺失文件返回 404。不提供目录内容列表。文件通过 `cap-std` 的目录能力打开，符号链接不能逃出 root；相较 NGINX 默认行为更严格。

`If-Range` 使用日期时，必须与文件的 Last-Modified 精确相等（秒精度）才发送范围响应；较新、较旧或无效的日期均返回完整文件。它与 If-Modified-Since 的日期比较规则不同。

只读取普通文件，FIFO、设备和套接字等特殊文件返回 403，目录 index 使用同样规则。打开前检查类型，并使用非阻塞打开防止文件在检查后被替换为 FIFO；打开后仍检查实际类型。root 内指向普通文件的相对符号链接可以正常访问。

静态路径仅进行一次百分号解码；文件名中的字面 `%` 不再解释为转义。目录及代理前缀补斜杠的 301 响应会重新编码路径，保留查询字符串中的原始转义；脚本修改静态路径或 query 时，目录重定向使用修改后的值。

ETag 包含长度和高精度修改时间，与 NGINX ETag 的文字格式不同。日志时间统一 UTC，combined 用户名字段固定 `-`；JWT claim 不自动写入此字段。数据面未匹配及路由前拒绝请求也写访问日志，管理端口请求除外。可选 OTLP logs/traces 和本地轮转见相关文档。

## TLS、插件与变量

| 指令 | 上下文、参数 | 默认值、继承与差异 |
|---|---|---|
| `ssl_certificate FILE` | H/S | PEM 证书链，叶证书在前；继承，和私钥成对；每个 server 一套，支持多个 SNI server |
| `ssl_certificate_key FILE` | H/S | PEM 私钥，继承；加载时检查与证书匹配 |
| `http2 on\|off` | H/S | 默认 off；仅 TLS 监听启用 ALPN H2 |
| `ssl_client_certificate FILE` | H/S | PEM 客户端 CA，继承；热更新并对已有 TLS 连接上的后续请求重新验证 |
| `ssl_verify_client on\|optional\|off` | H/S | 默认 off；on 要求已验证客户端证书，optional 验证已提供证书 |
| `rgnix_script FILE.rgl\|FILE.wasm` | S/L | 默认无，内层覆盖继承；一条路由一个插件；不是 NGINX 指令 |
| `rgnix_request_body off\|full SIZE\|prefix SIZE` | H/S/L | 默认 off，内层覆盖；SIZE 为 1 字节..256 KiB，仅挂载插件的路由生效；full 超限 413，prefix 截断判断视图而完整转发；不是 NGINX 指令 |
| `rgnix_request_body_timeout TIME` | H/S/L | 默认 5s，独立继承；大于 0 且最多 60s，预读总超时返回 408；不是 NGINX 指令 |

最低 TLS 1.2。不支持 ssl_protocols、密码套件配置、OCSP、动态证书路径变量。SNI 证书在新握手时读取当前快照；客户端 CA 策略按每个新请求的快照校验。Ingress 无匹配证书时拒绝握手。

脚本选择 upstream 名时沿用该组在 `proxy_pass` 中使用的 HTTP/HTTPS 协议，HTTPS 仍验证证书及主机名；没有引用的组默认 HTTP。含插件的配置不能让同一组同时使用两种协议，需使用不同 upstream 名，详见 [RGL 后端选择规则](rgl.md)。

变量仅可用于 `proxy_set_header`、`add_header`、`return`：

| 变量 | 值 |
|---|---|
| `$host` / `$http_host` | 规范化请求主机名；缺失时回退到所选 server 的第一个 server_name / 原始 Host 头，缺失为空 |
| `$scheme` | `http` 或 `https` |
| `$request_uri` | 原始路径和 query |
| `$uri` / `$args` | 规范化/插件修改后的路径、query |
| `$request_method` / `$remote_addr` | 请求方法、按可信代理策略解析的客户端 IP；默认使用直接连接 IP |
| `$realip_remote_addr` | 原始 socket 对端 IP |
| `$proxy_host` | 代理请求头展开时的上游 authority |
| `$proxy_add_x_forwarded_for` | 原始 X-Forwarded-For 加解析后的客户端 IP |
| `$http_NAME` | 大小写不敏感请求头，`_` 转成 `-`；不存在为空字符串 |

不支持 `${...}`、变量赋值或动态 proxy_pass。无 worker/master/daemon/pid 指令，工作线程通过 CLI 配置。无 `rewrite/map/if`、缓存、stream、mail 或第三方模块。

可信代理指令 `set_real_ip_from`、`real_ip_header`、`real_ip_recursive`、CIDR `allow/deny` 支持 H/S/L；JWT、外部认证、限流也在 H/S/L 覆盖继承。`least_conn`、`ip_hash`、`rgnix_balance`、`rgnix_max_inflight`、`rgnix_health_check` 在 upstream。各参数范围、默认值、组继承规则、NGINX 差异及 Ingress 对应注解详见[入口策略、认证、调度与诊断](product-features.md)。

参考：[NGINX proxy_pass](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_pass)、[HTTP core](https://nginx.org/en/docs/http/ngx_http_core_module.html)。行为对照固定 NGINX 1.28.0；具体证据见 [最新验证记录](validation-semantics.md)。

## 平台策略增量

支持 `proxy_ssl_certificate` / `proxy_ssl_certificate_key`、`access_log PATH json`，以及 `rgnix_log_query/client/referer/redact/fields`。原有 combined 输出保留，但默认脱敏敏感 query 参数。上游 TLS 校验保持开启。命名空间治理、灰度、镜像、自动回退和管理历史是 rgnix 扩展，配置见[平台策略](platform-policies.md)。

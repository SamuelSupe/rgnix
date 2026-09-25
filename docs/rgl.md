# RGL v1 语言与宿主 ABI

RGL 是面向路由的静态类型小语言，语法接近 Lua。源文件后缀 `.rgl`。词法分析和递归下降/Pratt 解析生成 AST；类型推导后由 `wasm-encoder` 生成真实 Wasm。Wasmtime 使用 Cranelift 在加载时编译机器码。请求路径上没有源码解析、AST 解释或模块编译。

编译模块按内容 SHA-256 在进程内复用；机器码缓存不跨进程持久化。Ingress 将上一有效源码和路由定义保存到控制器命名空间，重启后重新编译恢复。配置发布前校验 Wasm、导入白名单、内存、实例化和入口签名。`.wasm` 与 `.rgl` 使用相同隔离限制；不加载原生序列化代码缓存。最多缓存 128 个编译失败摘要，避免端点变化反复编译相同的坏插件。

## 语法与类型

```lua
-- 行注释；函数参数和返回类型从调用点推导
function allowed(header)
    return header ~= nil and str.starts_with(header, "Bearer ")
end

function on_request()
    local token = req.header("authorization")
    if not allowed(token) then
        return resp.reply(401, "missing bearer token")
    elseif req.method() == "POST" then
        req.set_header("x-method", "write")
    else
        req.set_header("x-method", "read")
    end
    local count = 0
    while count < 3 do
        count = count + 1
    end
    return route.pass()
end
```

支持普通顶层函数、`local NAME = EXPR`、赋值、`if/elseif/else/end`、`while/do/end`、函数调用和 `return`。作用域随函数/分支/循环块划分；允许局部变量遮蔽。源文件最多 256 KiB、65,536 个 token；块嵌套和函数调用深度最多 64，单个 if 最多 64 个分支。表达式 AST 和编译遍历的组合深度最多 128，平坦运算链同样受限。CLI 与热更新使用相同的 8 MiB 编译线程栈。入口及其可达函数会进行类型检查和编译。

| 类型/运算 | 语义 |
|---|---|
| 整数 | 有符号 i64；十进制；`+ - * / %` 与比较。加减乘按 Wasm 二进制补码回绕；整数除法朝零截断；除零和除法溢出 trap |
| 布尔 | `true/false`；`and/or/not`，短路求值；条件必须是布尔，不使用 Lua 的隐式真假转换 |
| 字符串 | UTF-8，单/双引号，`\n \r \t \\ \" \'` 转义；`..` 拼接；无自动数字转字符串 |
| 可空字符串 | 缺失请求/响应头、不可用的请求体文本或 JSON 字符串字段为 `nil`，可用 `== nil`、`~= nil` 检查；`local x = nil` 无法推导具体类型，拒绝；非空字符串变量可赋 nil |
| Action | `route.pass/proxy` 或 `resp.reply` 的结果，只用于请求阶段路由决策 |
| Void | 无返回值函数；`on_response` 必须无返回值 |

函数为单态：同一个函数的不同调用点必须具有相同参数类型。普通有返回值的函数要求每条路径返回同一类型；`on_request` 可隐式返回 pass。无浮点数、动态 table、数组、闭包、全局可变变量、模块系统、递归、协程、for/break 或 Lua 标准库。

## 请求阶段 API

路由匹配后调用 `on_request()`。以下 `string?` 表示可空字符串。

| API | 返回值/作用 |
|---|---|
| `req.method()` | 方法字符串 |
| `req.path()` | 已解码、规范化的请求路径，或本钩子暂存的新路径 |
| `req.query()` | 原始或修改后的 query，不带 `?` |
| `req.host()` | 请求 authority 的小写主机部分，不带端口 |
| `req.remote_addr()` | 可信代理策略解析的客户端 IP；默认不信任转发头 |
| `req.header(name)` | `string?`，名称大小写不敏感；可读本钩子已暂存的修改 |
| `req.body()` / `req.body_len()` | 请求体可见视图的 UTF-8 字符串（未开启或非 UTF-8 为 nil）/ 字节数 |
| `req.body_complete()` / `req.body_truncated()` | 视图已确认完整 / 可能省略尾部；关闭时均为 false |
| `req.body_contains(pattern)` | 对可见字节搜索，支持非 UTF-8 请求体；模式最多 8 KiB |
| `req.json_string(pointer)` | 完整 JSON 的字符串字段，缺失/非字符串/无效或截断 JSON 为 nil |
| `req.json_int(pointer, fallback)` | 完整 JSON 中可表示为 i64 的整数；缺失、非整数或无效/截断 JSON 返回 fallback，不自动转换字符串或浮点数 |
| `req.json_bool(pointer, fallback)` | 完整 JSON 布尔字段，否则返回布尔 fallback |
| `req.arg(name)` | 首个匹配的 query 参数，按 form 规则解码（含 `+`）；缺失为 nil，读取插件修改后的 query |
| `req.cookie(name)` | 首个同名 Cookie 的原始值，缺失为 nil；读取插件修改后的 Cookie 头 |
| `req.claim(name)` | JWT 认证后已验证的标量 claim，缺失为 nil；数值/布尔转字符串，对象/数组不暴露 |
| `req.set_path(path)` | 已解码的绝对路径，最多 8 KiB，不带 query；字面 `%` 保留，转发和目录重定向时按路径编码；不重新匹配 location |
| `req.set_query(query)` | 原始 query，最多 8 KiB；调用者负责 query 转义，空字符串清空；未改写路径且 proxy_pass 不带 URI 时，保留原始路径字节，不重新规范化 |
| `req.set_header(name, value)` | 修改请求头，值最多 8 KiB，要求合法 HTTP 头 |
| `req.remove_header(name)` | 删除请求头 |
| `route.pass()` | Action 0，使用匹配路由原动作 |
| `route.proxy(backend)` | Action 1，选择允许的后端及其传输协议，名称最多 512 字节；保留原始 URI，除非显式改写 |
| `resp.reply(status, body)` | Action 2，直接响应；status 为 200..599，body 最多 64 KiB；默认 `text/plain; charset=utf-8` |

应先完成条件判断，再执行 `return route.pass()`、`return route.proxy("app")` 或 `return resp.reply(403, "denied")`。每次请求钩子最多调用一次决策 API；重复调用使整个钩子失败并返回 500，即使两次动作类型相同。ABI v1 的 0/1/2 只表示动作类别，不能区分两个 Proxy 或 Reply，因此不支持先计算多个候选 Action 再返回其中一个。不调用决策 API 时可以隐式 pass；调用后必须返回与之对应的动作。

独立模式可选配置中的 upstream 名。名称在所有 location 加载后绑定到 `proxy_pass` 使用的协议，不依赖声明顺序；仅通过 HTTPS 引用的 upstream 在脚本选择后仍使用 TLS，并验证证书及主机名。未被 `proxy_pass` 引用的 upstream 默认使用 HTTP。配置中存在插件时，同一 upstream 名同时被 HTTP 和 HTTPS 引用会导致加载失败，应为两种协议配置不同名称。Ingress 模式只能选择**当前 Ingress 声明**的 `Service:port`，例如 `app:http` 或 `app:80`，且只能在同命名空间。

插件不能修改 Connection、Transfer-Encoding、Content-Length、Upgrade、Trailer、TE、Keep-Alive、Proxy-Connection；它们属于传输协议状态。不能绕过配置任意创建网络后端。头部表示为字符串视图，重复同名头只暴露一个值，非文本值不暴露；传输仍由 Pingora 保留原始 HTTP 头语义。

## 响应阶段 API

可选 `on_response()` 在上游/静态/直接响应的响应头写出前调用，与请求阶段使用同一个请求独立的 Wasm 实例。不能在此阶段改变请求、选择后端或重新发响应。

| API | 返回值/作用 |
|---|---|
| `resp.status()` | 原响应状态整数 |
| `resp.header(name)` | `string?`，含配置 add_header 以及本钩子暂存的修改 |
| `resp.set_header(name, value)` | 修改响应头，限制同请求头 |
| `resp.remove_header(name)` | 删除响应头 |

请求体读取需显式开启 `rgnix_request_body full SIZE` 或 `prefix SIZE`，最多 256 KiB；默认关闭。预读发生在请求钩子前；响应钩子可读取同一视图，不继续读取客户端。截断只影响插件视图，完整请求体仍原样转发。不开放请求体修改和响应体读取/修改。详见[请求体路由、限制与 Ingress 注解](request-body.md)。协议解析失败或在路由/插件之前生成的错误响应不保证运行响应钩子。

字符串函数：`str.concat(a,b)`、`str.eq(a,b)`（允许 nil 比较）、`str.starts_with(a,b)`、`str.contains(a,b)`、`str.len(a)`（UTF-8 字节数）、`str.lower(a)`（ASCII 小写）、`str.hash(a)`（SHA-256 前 8 字节的大端整数，清除符号位，跨进程稳定）。除了相等比较，nil 传给字符串操作会 trap，先做非空判断。hash 不是签名或身份验证；认证由 [JWT/外部认证策略](product-features.md#jwt外部认证及客户端证书)完成。

typed JSON getter 同样要求完整有效 JSON，截断前缀不能当成完整文档；query/cookie/hash 扫描按输入长度扣 fuel。示例见 [typed-body.rgl](../examples/typed-body.rgl)，可用 `rgnix simulate` 离线执行路由和插件。

## 隔离与原子提交

| 限制 | 默认值 |
|---|---|
| 每个钩子 Wasm fuel | 100,000；每次宿主调用额外扣 100 |
| 请求体访问 | 默认关闭；扫描/JSON 查询额外按每 32 字节每次遍历扣 1 fuel；JSON 临时预算和视图计入宿主额度 |
| Wasm 线性内存 | 8 MiB，最多 1 个 memory，禁止 table |
| Wasm 栈 | 256 KiB |
| 宿主数据 | 每请求累计 1 MiB，包含请求/响应元数据、句柄字符串和暂存修改 |
| 单字面量 / 拼接结果 / 直接响应体 | 64 KiB |
| Wasm 输入 | 2 MiB |
| Wasm 编译预算 | 最多 1,024 个类型/函数/全局值、64 个导入；每个签名最多 64 个参数、1 个结果；单函数代码最多 64 KiB、4,096 个局部变量，模块局部变量总量最多 16,384；最多 50,000 条指令、128 层控制结构 |
| 进程插件实例预算 | 默认 32；`--max-plugin-instances` 调整，超额请求返回 503 |

每个请求创建独立 Store 和实例；没有跨请求可变状态。`on_response` 重新给予 fuel，但共用该请求的宿主数据预算。宿主分配按累计量计费，重复覆盖同一个头不会绕过预算。实例化 start 函数也受 fuel/内存限制，start 的路由修改不会带入请求。

响应钩子只处理最终响应（包含 WebSocket 101），不会被 100/103 等临时响应提前消耗。最终响应头处理后释放实例和实例额度，长时间流式传输继续受进程请求并发上限约束。配置的 server 级 return 在路由插件之前完成；需要插件处理的直接响应应放在 location 中。

宿主修改先暂存，只有整个钩子成功后提交。非法头、越界内存、nil 错用、超限、Wasm trap 或后端越权返回 500；失败钩子的部分修改不转发。流式请求已发送到上游后的外部副作用无法撤销，且系统从不自动重放该请求。响应钩子失败不能撤销先前已成功完成的请求钩子或上游工作。

配置编译或实例化失败时不发布候选快照。独立模式保留整个旧快照。Ingress 的插件语法更新失败时保留该 Ingress 的上一有效模块及路由定义（包含其已声明后端），但这些后端的 Service/端点、Ingress 删除、Class 选择和 TLS 变化仍实时生效；删除 ConfigMap 会禁用对应路由，删除 TLS Secret 会撤销该主机的新 TLS 握手。更新恢复有效后再发布新的路由定义。

## `rgnix_v1` ABI

Wasm 仅能导入 `rgnix_v1` 模块中上表的函数，加上编译器私用的 `literal`。没有 WASI，也没有文件、网络、时间或进程 API。引入其他模块、错误签名或额外 memory/table 会在加载阶段失败。

- 所有宿主函数的参数和返回值均为 Wasm `i64`；Void 返回 0。
- 必須导出 `memory` 和 `on_request: () -> i64`，可导出 `on_response: () -> i64`。
- 字符串以请求内的正整数句柄传递；0 表示 nil。句柄不能跨请求使用。
- `literal(pointer: i64, byte_length: i64) -> i64` 从 memory 拷贝 UTF-8 字节并返回字符串句柄；范围和预算在宿主检查。
- Bool 为 0/1；Int 为 i64；请求入口返回 0/1/2，必须与宿主暂存的 Pass/Proxy/Reply 决策一致。
- 编译产物包含 `rgnix.abi` 自定义段、内容 `1`；兼容性以版本化导入名和函数签名为准，不依赖该提示段。

可移植 `.wasm` 与 CPU 无关，启动加载时编译成本单独发生；不输出或加载 Wasmtime 的不安全反序列化原生缓存。[Wasmtime Module 文档](https://docs.wasmtime.dev/api/wasmtime/struct.Module.html)。

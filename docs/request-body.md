# 按请求体路由

**v0.2.0 起支持请求体路由**，可使用该版本下载包或从源码构建。

在匹配 location 后、执行 `on_request()` 和选择上游前，按配置读取请求体。功能默认关闭；只在挂载了插件的路由上生效。支持 POST 以及其他带请求体的方法。插件只能读取，不能修改请求体。

## 完整 JSON

```nginx
location /api/ {
    proxy_pass http://app;
    rgnix_script body-json.rgl;
    rgnix_request_body full 64k;
    rgnix_request_body_timeout 2s;
}
```

```lua
function on_request()
    if req.json_string("/tenant") == "vip" then
        return route.proxy("canary")
    end
    return route.pass()
end
```

`full 64k` 等待完整请求体，超过 64 KiB 返回 **413**，不连接上游。JSON Pointer 支持嵌套对象、数组下标、`~0` 和 `~1` 转义，例如 `/items/0/model`。只返回字符串字段；不存在、类型不是字符串、无效 JSON 或无效 Pointer 返回 `nil`。JSON 对象重复键采用最后一个值。

不依赖 Content-Type 自动决定解析方式，也不解压 Content-Encoding，不提供 multipart/form-data 或表单字段解析。按原始 HTTP 实体字节处理，chunked 的分块边界不包含在视图中。需要时可用 `req.header("content-type")` 做业务校验。

## 前缀与截断

```nginx
location /ingest/ {
    proxy_pass http://app;
    client_max_body_size 20m;
    rgnix_script body-prefix.rgl;
    rgnix_request_body prefix 4k;
    rgnix_request_body_timeout 2s;
}
```

```lua
function on_request()
    if req.body_contains("route=vip;") then
        return route.proxy("canary")
    end
    return route.pass()
end
```

`prefix 4k` 最多向插件暴露前 4096 字节，读够后立即执行插件，无需等待整个上传。未达到上限而请求体已结束时，暴露完整内容。`req.body_contains` 按字节搜索 UTF-8 模式串，二进制数据和截断的 UTF-8 字符不会导致搜索失败。超出前缀的字节不参与判断，跨越截断点的模式也不匹配。

**截断只影响判断视图，上游收到完整、原样、顺序不变的请求体。** 预读字节由 Pingora 先发送一次，剩余部分继续流式转发；不会自动重试。`client_max_body_size` 仍约束整个请求，不会因为前缀模式而放宽。上游已经收到部分流后才发现后续超限时，终止传输，已产生的上游副作用不能撤回。

`req.body_complete()` 表示已确认读取了完整请求体；`req.body_truncated()` 表示已启用检查但视图可能省略后续内容。对恰好在上限处暂停的 chunked/HTTP2 流，若还未观察到结束标记，保守视为截断，不多等一个字节或结束帧。关闭检查时两个值都为 false。

**截断视图上的 `req.json_string()` 返回 nil**，即使前缀看起来已经包含一个完整 JSON 对象。需要按局部内容判断时使用 `req.body_contains` 或 `req.body`；需要可靠的 JSON 字段语义时使用完整读取模式。

## API 与限制

| API | 契约 |
|---|---|
| `req.body()` | 可空 UTF-8 字符串。未启用或视图不是合法 UTF-8 时为 nil；已读取的空请求体为 `""`。截断切断多字节字符也会返回 nil |
| `req.body_len()` | 视图的字节数；未启用时为 0 |
| `req.body_complete()` | 是否检查且确认包含完整请求体 |
| `req.body_truncated()` | 是否检查且未确认完整 |
| `req.body_contains(pattern)` | 在可见字节内搜索，pattern 最多 8 KiB；未启用时 false，已启用时空模式匹配 |
| `req.json_string(pointer)` | 完整 JSON 的字符串字段；Pointer 最多 1024 字节和 32 层，超出时插件失败 |

`rgnix_request_body off;` / `full SIZE;` / `prefix SIZE;` 支持 H/S/L，内层覆盖，默认 off。SIZE 必须显式提供，范围 **1 字节到 256 KiB**，支持字节和 k/m/g 单位。`rgnix_request_body_timeout TIME;` 独立继承，默认 **5s**，必须大于零且不超过 60s；这是整个预读阶段的总时间，不会被持续发送少量数据无限延长，超时返回 **408**。

读取前先占用插件并发额度，额度耗尽返回 503；读取和解析受限制，不落盘。预读转发缓冲最多 SIZE + 64 KiB（容纳跨过前缀边界的一次传输读取），插件视图始终不超过 SIZE。插件视图、字符串副本和 JSON 解码临时预算计入每请求 1 MiB 宿主额度；扫描和 JSON 查询另扣与输入长度相关的 fuel。JSON 查询遍历借用的子树，不构建整棵动态对象。反复读取/解析可能耗尽额度并返回 500，应保存并复用返回值。`on_response()` 可读取同一视图，不会继续读取客户端。

`Expect: 100-continue` 在预读前得到确认，已处理的 Expect 不继续发送给上游。HTTP/1.1、chunked、客户端 HTTP/2 均使用相同策略。请求在预读期间持有原配置快照，热更新仅影响后续请求。

## Kubernetes Ingress

```yaml
metadata:
  annotations:
    rgnix.io/script: routes/main.rgl
    rgnix.io/request-body: "prefix 4k"
    rgnix.io/request-body-timeout: "2s"
```

也可设置 `"full 64k"` 或 `"off"`；限制及默认值与文件配置一致。脚本使用 `route.proxy("canary:http")`，且该 Service 必须出现在当前 Ingress 的后端声明中。

读取模式和超时与源码、路由定义一起保存检查点，再原子发布。只更新注解也触发更新。脚本或读取策略无效时保留上一有效组合，新副本也能恢复；EndpointSlice 撤销及 Ingress 删除仍立即生效。

完整独立配置见 [body-routing.conf](../examples/body-routing.conf)。

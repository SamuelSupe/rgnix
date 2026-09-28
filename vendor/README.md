# Pingora 0.9.0 vendor patches

`pingora-core` is based on crates.io 0.9.0, with the bounded patches below.
The original Apache-2.0 license and notices are retained. Registry metadata and
the nested Cargo.lock are omitted; the root Cargo.lock pins the build.

## HTTP/1 transient state and lazy timers

`pingora-proxy` joins both directions of an HTTP/1 exchange in one future.
Its two task pipes now borrow fixed-capacity queues from that future instead
of allocating general multi-producer channels. Capacity remains four tasks per
direction; a read still reserves space before consuming a body chunk. A mutex
preserves `Send` when the parent future migrates, and cancellation releases
reservations and wakes the other half. Custom-message and HTTP/2 channels are
unchanged. Successful async sends and receives consume Tokio's cooperative
budget, preserving its yield behavior when operations stay continuously ready.
Normal full-queue backpressure no longer allocates an error object.
Retain bounded exchange, reservation cancellation, early response, large upload,
WebSocket and drain tests when updating this patch.

HTTP/1 parsed header offsets use inline storage for sixteen fields and grow on
the heap for larger messages. Response task batches use inline storage sized for
the pipe and one saved task. Header limits, duplicate values, original casing,
framing checks and filter order are unchanged. Existing Vec-based public methods
remain available; the HTTP/1 writer additionally accepts an exact-size iterator.
Retain large-header, duplicate-header and keepalive isolation checks.

`pingora-timeout` is based on crates.io 0.9.0 under its original Apache-2.0
license. A fast timeout checks its timer-thread watchdog when the wrapped I/O
first returns Pending, instead of reading the clock even for immediately ready
I/O. The callback already returns a pinned boxed timer, so the timeout no longer
boxes it a second time. Deadline rounding, the long-duration Tokio fallback and
watchdog recovery are preserved. Retain ready/pending/expired timer, fallback and
watchdog tests; remove these patches when equivalent upstream behavior exists.

## Request inspection replay

`enable_retry_buffering_with_limit` is added in `protocols/http/server.rs`,
`protocols/http/v1/server.rs` and `protocols/http/v2/server.rs`.

Pingora's fixed 64 KiB replay buffer is insufficient for bounded request-body
inspection: even a small prefix can consume a transport chunk that crosses
that limit. The additive API lets rgnix allocate a bounded replay buffer before
inspection, and refuses to reset an existing buffer. Pingora then forwards the
pre-read bytes once before continuing its normal streaming path. Automatic
upstream retries remain disabled.

The rgnix limit is 256 KiB for inspection plus 64 KiB for one transport read.
An unexpectedly larger read fails closed before selecting an upstream. Keep
the full-body/hash, fragmented/chunked upload, HTTP/2 and no-retry integration
checks when upgrading Pingora. Remove this patch when upstream provides an
equivalent public API.

`pingora-proxy/src/proxy_h1.rs` and `proxy_h2.rs` only enable an automatic
retry buffer when `max_retries > 1` (the budget includes the first attempt).
Explicit inspection buffers are preserved: their pre-read bytes must still be
forwarded on the initial attempt. Ordinary single-attempt uploads no longer
retain an unused retry copy. Keep full-body/hash, HTTP/2 and no-replay checks
when upgrading this guard.

## Request state and idle connections

The additive `HttpServerApp::process_new_http_boxed` hook transfers the existing
session allocation through dispatch; its default preserves the original API.
`finish_boxed` completes writes and drains in place before extracting a reusable
stream. The proxy keeps its session and application context boxed across async
calls, avoiding repeated copies of their large state machines.

HTTP/1 transport pooling now reuses bounded peer-group queues instead of
allocating a watcher task, mutex and notifications on every return. One weakly
owned maintenance task per connector checks idle sockets every 100 ms. Checkout
also checks the exact deadline, socket peer, EOF and unsolicited data before
reuse; expired sockets cannot be borrowed between sweeps. Capacity remains a
global connection cap, with oldest connections evicted from the least recently
used peer group. Empty group indexes are bounded and retained for reuse; index
churn reclaims an empty group instead of displacing live idle connections.
Checkout evaluates expiry after acquiring the pool lock. rgnix
continues to include worker ID and TLS trust policy in the pool key. HTTP/2's
multiplexed pool is unchanged. Keep pool-capacity/expiry, upstream EOF, hostile
idle data, trust rotation and cancellation checks when updating these patches.

## Linux file responses

The additive HTTP/1 `write_file_body` operation uses sendfile only for fixed
lengths on plain TCP/Unix sockets, after flushing buffered headers. It preserves
write timeouts and payload byte accounting, handles partial writes, and treats
an early EOF as failure. Unsupported transport/framing returns false without
writing body bytes; unsupported kernel/filesystem operations may fall back only
before the first body byte. TLS, HTTP/2 and compression retain buffered writes.
This API bypasses body filters and must only be used when the caller has ruled
out transformations. As with conventional sendfile, cold file data can require
kernel filesystem I/O; it is not an asynchronous disk-I/O guarantee.

## Listener reactor ownership

`services/listening.rs` starts each accept loop on the Tokio runtime that built
its listener. In no-steal mode, picking a different runtime can leave the loop
polling a stopped reactor during immediate shutdown. Failed accepts also yield
so an immediately failing socket cannot starve shutdown. Accepted connections
still distribute across workers. Keep the repeated multi-listener SIGINT check
when upgrading this patch.

## PROXY protocol before HTTP or TLS

`protocols/digest.rs` adds a separate `proxy_protocol_addr` OnceCell to the
socket digest. It never overwrites the kernel peer used to verify trusted CIDRs.
`listeners/mod.rs` runs the existing pre-TLS callback before choosing plaintext
or TLS, so explicitly configured PROXY listeners work with both protocols.
The bounded parser and trust policy live in rgnix, not in this dependency.
Retain v1/v2 IPv4/IPv6 and PROXY-before-TLS request tests during upgrades.

## Complete Brotli streams

`protocols/http/compression/brotli.rs` finishes CompressorWriter with
`into_inner()` on the final input; `flush()` alone omits the stream terminator.
The writer is held in an Option so it can be consumed once. The regression
checks complete decompression instead of hard-coding compressed bytes. Actual
HTTP Brotli decoding is also covered by `scripts/product_features.py`.

Pingora 0.9.0 ignores Accept-Encoding quality weights. rgnix negotiates its
configured algorithms before calling that API; this does not need a vendor edit.

## Absolute request and backend deadlines

`pingora-proxy` is pinned to crates.io 0.9.0 with two additive `ProxyHttp`
hooks: `request_deadline` and `backend_request_timeout`. Their defaults are
unset, preserving callers without deadline policies. The proxy applies the
request deadline during request filtering and wraps each upstream future with
the earlier request/backend deadline. Completion includes downstream streaming
and backpressure. On timeout the transport future is dropped; it must not enter
the reusable connection pool. Existing error handling, logging and request
context cleanup still run, and rgnix's one-attempt budget remains unchanged.

This is needed for HTTPRoute timeouts: per-read inactivity timers alone cannot
bound a continuously trickling response. Keep Gateway slow-header/trickle/body
checks and HTTP/1.1, HTTP/2, cancellation, WebSocket and no-replay regression
checks when upgrading. Remove this patch when an equivalent upstream API exists.
The original license and notices are retained, with registry metadata and the
nested lockfile omitted as for pingora-core.
# Application drain before runtime shutdown

The additive `Server::set_graceful_shutdown_check` hook lets rgnix wait for its
in-flight request permits before stopping Tokio runtimes. Tokio's
`shutdown_timeout` immediately cancels async work and only waits for blocking
tasks, so using that timeout alone truncated gRPC streams after the initial
grace period. The check and runtime cleanup share the same total timeout.
Keep the live Gateway gRPC stream termination and HTTP keepalive rollout checks
when upgrading this patch.

## Experimental Hyper client

`hyper-util/` is an optional direct path dependency used only by `hyper-experimental`.
It is not a global crates.io override: reqwest and kube retain their registry copy.
The HTTP/1-only client keeps requests boxed through checkout and retry state, then
moves the request into the existing Hyper dispatcher. Automatic retries remain off.
See [patch provenance and tradeoffs](hyper-util/RGNIX.md) and the
[measured validation](../docs/validation-hyper-prepared-2026-09-28.md).

`hyper/` is also an isolated optional path dependency, based on crates.io 1.11.1.
With `rgnix-full-body`, a complete fixed-length client response of at most 8 KiB
already in the read buffer uses a single `Bytes` body instead of a channel.
It uses the original decoder, never waits for more bytes to qualify, and keeps
all other bodies on the existing streaming path. See its [patch contract](hyper/RGNIX.md)
and [allocation, framing and streaming checks](../docs/validation-nginx-aligned-2026-09-28.md).

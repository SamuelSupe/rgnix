# Pingora 0.9.0 vendor patches

`pingora-core` is based on crates.io 0.9.0, with the bounded patches below.
The original Apache-2.0 license and notices are retained. Registry metadata and
the nested Cargo.lock are omitted; the root Cargo.lock pins the build.

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

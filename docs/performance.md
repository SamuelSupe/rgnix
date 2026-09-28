# Performance status and reproduction

v0.5.0 includes the implementation from optimization rounds one through nine. Historical reports describe their test-time source and binary identities; statements such as "unpublished" refer to the report date, before this release. Those diagnostic binaries are not the final Rust 1.98 release archives. Publishing this preview does not certify stable capacity or erase failed calibration.

## Current evidence

The [round-nine comparison](validation-performance-round9-2026-09-27.md) measured plain HTTP/1, two workers, 64 connections, three alternating 8-second windows after warmup, on shared OrbStack Linux ARM64. Access logging/OTLP output was disabled; product policies and metrics remained active.

| Workload | rgnix median req/s | NGINX 1.30.5 median req/s |
|---|---:|---:|
| 2-byte direct response | 265,012 | 721,865 |
| 1 KiB proxy | 103,596 | 194,876 |
| 16 KiB static file | 143,690 | 376,704 |
| RGL header routing / NGINX map | 91,519 | 250,873 |

These are observations, not guaranteed rates. The 64-connection identical-binary A/A comparison exceeded the 10% gate at 12.23%. A 256-connection window had 40.911 ms P99 despite passing its throughput gate. Virtual CPU affinity does not reserve physical cores. RGL/Wasm and NGINX map have different execution/isolation costs; static data was on tmpfs. HTTPS, cold disk and real NIC capacity cannot be inferred.

A [later one-worker diagnostic](validation-pingora-audit-2026-09-28.md) retained the gap in all three rounds; the modest difference between rgnix and its minimal vendor-based proxy was below a reliable attribution threshold. A successful scoped one-worker A/A check does not resolve the earlier multi-worker or tail-latency qualification.

## Implemented versus proposed

Implemented: backend selection and Gateway indexing, lazy request/header state, request metric handles, RGL constants and request-only instance lifetimes, guarded sendfile, bounded idle-connection groups, smaller repeated session transfers, borrowed HTTP/1 task queues and lazy timer checks. See [vendor patch contracts](../vendor/README.md), [deployment budgets](deployment.md) and [Wasm compatibility](rgl.md).

Not implemented: uninitialized parser scratch arrays, upstream body ownership transfer, batch vectored response writes, reusable HTTP session workspaces and a compact ordinary HTTP/1 state machine. The [Pingora audit](validation-pingora-audit-2026-09-28.md) identifies source/assembly evidence and risks; it contains no measured candidate gain. The custom pool's shared lock also needs separate multi-worker investigation.

Fewer allocations are not equivalent to lower CPU or RSS. Round nine reduced proxy allocation calls about 11.4% against round eight but increased captured libc copy bytes about 6.7%; its proxy throughput observation changed only 0.9%. Samples from allocation, profiler and uninstrumented runs are not interchangeable cost budgets.

## Reproduce and qualify

Use [scripts/benchmark_compare.py](../scripts/benchmark_compare.py) and its `--help` on an isolated Linux test machine. Build with locked dependencies, record compiler and binary hashes, pin client/proxy/origin CPU sets, verify response correctness and keep equivalent feature settings. The JSON records include the actual invocation, configuration, warmup, durations and all windows.

Run interleaved same-binary A/A at every target worker/concurrency setting before interpreting A/B. Preserve rejected/outlier windows; report CPU per request, P99, errors and memory alongside throughput. Separate allocation/perf instrumentation from formal throughput. Include large/chunked uploads, slow readers, cancellation, early responses, TLS/H2, SSE/WebSocket, timeout, no-replay and reload/drain checks when changing transport code. `examples/minimal_proxy.rs` is diagnostic only and omits product policies.

## Historical records

- [Released v0.4.0 versus NGINX and OpenResty](validation-nginx-openresty-2026-09-27.md): the full three-frontend comparison. Later runs used OpenResty as an origin only.
- [Round 1](validation-performance-2026-09-27.md), [2](validation-performance-round2-2026-09-27.md), [3](validation-performance-round3-2026-09-27.md): application fixed costs and qualification limits.
- [Round 4](validation-performance-round4-2026-09-27.md): Gateway route index; a route-matching microbenchmark is separate from HTTP capacity.
- [Round 5](validation-performance-round5-2026-09-27.md), [6](validation-performance-round6-2026-09-27.md), [7](validation-performance-round7-2026-09-27.md): multi-endpoint selection and pool experiments.
- [NGINX rerun](validation-nginx-round7-2026-09-27.md), [round 8](validation-performance-round8-2026-09-27.md), [round 9](validation-performance-round9-2026-09-27.md): remaining frontend gap, copy/allocation tradeoffs and tail limits.
- [2026-09-28 Pingora audit](validation-pingora-audit-2026-09-28.md): latest diagnosis and unimplemented candidates.

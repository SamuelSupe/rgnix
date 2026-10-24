# Performance status and reproduction

v0.5.0 includes the implementation from optimization rounds one through nine. Historical reports describe their test-time source and binary identities; statements such as "unpublished" refer to the report date, before this release. Those diagnostic binaries are not the final Rust 1.98 release archives. Publishing this preview does not certify stable capacity or erase failed calibration.

## Current evidence

The current development engine defaults to Hyper. The [2026-10-23 fixed-cost comparison](validation-hyper-fixed-costs-2026-10-23.md) compares main `a110737` with a fixed single-IP backend selection optimization and Redis removal, using matching compiler/features and fresh calibration. Redis removal changes quotas to per-process limits; see the [migration requirements](shared-rate-limits.md). Qualification limits and all measurements are recorded in that report.

The subsequent [instruction-count optimization](validation-hyper-instructions-2026-10-23.md) compares main `7f20b00` with request-local reuse of a prepared-route index. Matched Cachegrind controls pass the 1% A/A gate: single-worker user-space instructions fall 1.90% for ordinary 1 KiB proxy requests and 0.89% with 64 extra fields. These are instruction results, not throughput or P99 gains. An optional reactor/reference wakeup probe supports separate tail-latency diagnosis.

A fresh [Hyper/Pingora comparison of main `971afcd`](validation-hyper-pingora-main-2026-10-23.md) uses the same release ELF for both engines: two workers, 1/16 KiB responses, 64/256 connections, 48 alternating windows and 56,516,904 error-free requests. All four throughput and P99 repeatability checks fail on the shared OrbStack host. The observed median differences are diagnostic observations rather than qualified stable performance ratios.

The [focused 1 KiB / 64-connection tail-latency study](validation-hyper-tail-2026-10-24.md) distinguishes wrk's corrected histogram from raw response latency and evaluates more frequent native-reactor I/O checks. The before/after P99 observation is 2.404 to 2.105 ms with passing P99 repeatability; throughput observes -6.4% and fails calibration. The separate Hyper/Pingora comparison also fails calibration. The report preserves the throughput tradeoff, complete histograms, diagnostic controls and functional validation rather than claiming a stable production gain.

A fresh [Hyper candidate `6343436` / NGINX 1.28.0 comparison](validation-hyper-nginx-2026-10-24.md) measures the same two-worker plain HTTP/1, 1 KiB path at 64/256 connections. All 24 windows complete 105,442,084 requests without wrk errors. Observed median throughput is 104,665 / 149,903 req/s at 64 connections and 134,177 / 186,198 req/s at 256 (Hyper / NGINX). Median window P99 is 1.959 / 2.083 ms and 4.094 / 2.944 ms, respectively. Both programs fail throughput and P99 repeatability in both settings; these are observations, not certified ratios or evidence of better Hyper tails at 64 connections. The local candidate is separate from published main, and historical NGINX 1.30.5 results are not mixed into this comparison.

The [targeted Hyper/NGINX tuning study](validation-hyper-nginx-tuning-2026-10-24.md) retains local candidate `9a104b5` as an experiment. Header-timer reuse removes one allocation per warmed keepalive request, and a reactor interval of 32 halves measured epoll calls. The complete 36-window, 151,175,687-request comparison observes throughput changes of -0.7% / -5.3% at 64/256 connections, and P99 changes of approximately zero / +5.9%. Both comparisons fail throughput and P99 repeatability; no stable end-to-end gain is established. An initial NGINX shared-listener cohort used only one worker, so its five windows remain separate and the full matrix was rerun with matched per-worker HTTP listeners. Listener alignment is not a Hyper optimization gain. The report preserves all windows, independent operation counts, 419 runtime checks and the local-only publication status.

## Earlier measurements

The [round-nine comparison](validation-performance-round9-2026-09-27.md) measured plain HTTP/1, two workers, 64 connections, three alternating 8-second windows after warmup, on shared OrbStack Linux ARM64. Access logging/OTLP output was disabled; product policies and metrics remained active.

| Workload | rgnix median req/s | NGINX 1.30.5 median req/s |
|---|---:|---:|
| 2-byte direct response | 265,012 | 721,865 |
| 1 KiB proxy | 103,596 | 194,876 |
| 16 KiB static file | 143,690 | 376,704 |
| RGL header routing / NGINX map | 91,519 | 250,873 |

These are observations, not guaranteed rates. The 64-connection identical-binary A/A comparison exceeded the 10% gate at 12.23%. A 256-connection window had 40.911 ms P99 despite passing its throughput gate. Virtual CPU affinity does not reserve physical cores. RGL/Wasm and NGINX map have different execution/isolation costs; static data was on tmpfs. HTTPS, cold disk and real NIC capacity cannot be inferred.

A [later one-worker diagnostic](validation-pingora-audit-2026-09-28.md) retained the gap in all three rounds; the modest difference between rgnix and its minimal vendor-based proxy was below a reliable attribution threshold. A successful scoped one-worker A/A check does not resolve the earlier multi-worker or tail-latency qualification.

The experimental branch also includes a [standalone Hyper transport feasibility study](validation-hyper-prototype-2026-09-28.md). It preserves streaming and connection reuse but omits product features. Its exploratory results and failed calibration are not a performance claim for rgnix. Pingora was the production engine at the time of that study.

The next [product integration experiment](hyper-experimental.md) enables Hyper inside the real binary for a bounded plain HTTP/1 subset, preserving routing, budgets, telemetry and configuration snapshots. `benchmark_compare.py --plain-proxy` can compare the same binary with `--rgnix-transport pingora --candidate-transport hyper`; calibrate each mode against itself first. [Actual checks and calibration](validation-hyper-product-2026-09-28.md) remain separate from the minimal-proxy results.

A subsequent [CPU and copy profile](validation-hyper-profile-2026-09-28.md) found repeated moves of the integrated request context. Keeping that context in one heap allocation reduced captured libc copy bytes from about 64 KB to 26.5 KB per request, at the cost of one extra allocation. This is an operation-count result, not a throughput or NGINX-parity claim.

The next [prepared-request optimization](validation-hyper-prepared-2026-09-28.md) reduces captured copies to about 15.0 KB/request, allocation calls to 43.3/request, and clock reads to 24.4/request in the same diagnostic fixture. It prepares headers/clients before publication and shrinks cold request state. These operation counts do not establish a throughput gain; the report retains the new calibration results separately.

The subsequent [idle-pool optimization](validation-hyper-pool-2026-09-28.md) shares immutable client state and constructs connection-race state only after an idle checkout misses. Captured libc copies fall from 15.0 to 12.7 KB/request, with about one fewer allocation on keepalive requests. A new upstream connection per request instead costs about one extra allocation; throughput calibration remains a separate requirement.

The next [NGINX-aligned experiment](validation-nginx-aligned-2026-09-28.md) retains only required original request headers on fixed routes and eliminates the body channel for small, already-buffered upstream responses. Allocation calls fall from 42.3 to 30.5/request for the 1 KiB fixture; 64 extra headers expose a separate 170.2 to 102.3/request reduction from selective capture alone. Incomplete responses remain streaming. The fixed-worker experiment was withdrawn after failed calibration; its patch and measurements remain in the report. These operation counts do not establish throughput improvement or NGINX parity.

A [fresh current-branch NGINX comparison](validation-nginx-current-2026-09-28.md) aligns upstream Host, timeouts and idle-pool budgets in the harness. With two workers and 64 connections, three-round medians were **71,488 vs 156,269 req/s** for 1 KiB and **63,929 vs 79,018 req/s** for 16 KiB (experimental product Hyper vs NGINX 1.30.5). Both NGINX A/A cases failed the 10% gate, and the subsequent rgnix 16 KiB comparison had 11.45% CV. These are retained observations, not certified ratios or proof of optimization gains. All 36 calibration/comparison windows completed 52.9 million requests with zero wrk errors; the report includes CPU, P99, PSS and every window.

## Implemented versus proposed

Implemented: backend selection and Gateway indexing, lazy request/header state, request metric handles, RGL constants and request-only instance lifetimes, guarded sendfile, bounded idle-connection groups, smaller repeated session transfers, borrowed HTTP/1 task queues and lazy timer checks. See [vendor patch contracts](../vendor/README.md), [deployment budgets](deployment.md) and [Wasm compatibility](rgl.md).

Not implemented: uninitialized parser scratch arrays, upstream body ownership transfer, batch vectored response writes, reusable HTTP session workspaces and a compact ordinary HTTP/1 state machine. The [Pingora audit](validation-pingora-audit-2026-09-28.md) identifies source/assembly evidence and risks; it contains no measured candidate gain. The custom pool's shared lock also needs separate multi-worker investigation.

Fewer allocations are not equivalent to lower CPU or RSS. Round nine reduced proxy allocation calls about 11.4% against round eight but increased captured libc copy bytes about 6.7%; its proxy throughput observation changed only 0.9%. Samples from allocation, profiler and uninstrumented runs are not interchangeable cost budgets.

## Reproduce and qualify

For single-worker user-space fixed costs, use `scripts/profile_http_instructions.py --binary /absolute/path/rgnix --work-dir /native/linux/new-directory` with Valgrind, OpenSSL and `/usr/sbin/nginx` installed on Linux. Add `--candidate /absolute/path/candidate` for a comparison, and `--extra-headers 64` for header-width sensitivity. Each sample subtracts a matching startup/warmup/shutdown run from a run with 2,000 extra validated 1 KiB responses, using Cachegrind's flat instruction counts. Three rounds retain six samples per binary; require every A/A pair and the group CV to stay within 1%. Signed function deltas and failed controls are preserved. This measures neither kernel work nor shared-cache contention, and cannot establish throughput, P99 or multi-worker capacity. Keep the optional wakeup probe disabled during these runs.

Use [scripts/benchmark_compare.py](../scripts/benchmark_compare.py) and its `--help` on an isolated Linux test machine. Build with locked dependencies, record compiler and binary hashes, pin client/proxy/origin CPU sets, verify response correctness and keep equivalent feature settings. The JSON records include the actual invocation, configuration, warmup, durations and all windows.

Multi-worker NGINX/OpenResty plain HTTP frontends now use `listen ... reuseport` to match rgnix's per-worker listeners. A shared-listener diagnostic put an entire 64-connection cohort on one NGINX worker despite configuring two. Check actual per-worker CPU records as well as the configured worker count, retain affected windows separately, and rerun the complete comparison after changing this fixture. Do not count listener alignment as a Hyper code gain or mix earlier shared-listener ratios into the new matrix.

For separate CPU/syscall diagnostics of the same plain-proxy fixture, use `scripts/profile_http.py --matrix /native/linux/result.json --binary /absolute/path/rgnix --engines hyper nginx --concurrency 256 --seconds 15 --work-dir /native/linux/new-profile`. The default concurrency remains 64. `--sudo-perf` uses passwordless sudo for perf alone when tracepoints/kernel samples require it; proxy and origin processes retain the calling user. Match the benchmark's single-IP admission and upstream-pool settings. Instrumented windows do not establish throughput gains.

Run interleaved same-binary A/A at every target worker/concurrency setting before interpreting A/B. Preserve rejected/outlier windows; report CPU per request, P99, errors and memory alongside throughput. Separate allocation/perf instrumentation from formal throughput. Include large/chunked uploads, slow readers, cancellation, early responses, TLS/H2, SSE/WebSocket, timeout, no-replay and reload/drain checks when changing transport code. `examples/minimal_proxy.rs` is diagnostic only and omits product policies.

## Historical records

- [Released v0.4.0 versus NGINX and OpenResty](validation-nginx-openresty-2026-09-27.md): the full three-frontend comparison. Later runs used OpenResty as an origin only.
- [Round 1](validation-performance-2026-09-27.md), [2](validation-performance-round2-2026-09-27.md), [3](validation-performance-round3-2026-09-27.md): application fixed costs and qualification limits.
- [Round 4](validation-performance-round4-2026-09-27.md): Gateway route index; a route-matching microbenchmark is separate from HTTP capacity.
- [Round 5](validation-performance-round5-2026-09-27.md), [6](validation-performance-round6-2026-09-27.md), [7](validation-performance-round7-2026-09-27.md): multi-endpoint selection and pool experiments.
- [NGINX rerun](validation-nginx-round7-2026-09-27.md), [round 8](validation-performance-round8-2026-09-27.md), [round 9](validation-performance-round9-2026-09-27.md): remaining frontend gap, copy/allocation tradeoffs and tail limits.
- [2026-09-28 Pingora audit](validation-pingora-audit-2026-09-28.md): latest diagnosis and unimplemented candidates.

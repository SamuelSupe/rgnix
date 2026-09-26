# Performance validation — 2026-09-26

This records the current unreleased working tree on OrbStack Linux arm64. It is a local comparison, not a production capacity claim. Earlier Python/debug measurements remain historical evidence and are not used as the optimization baseline.

## Environment and method

- Linux `7.0.14-orbstack-00380-ga7e0a2dc9535`, aarch64; Rust 1.90, Wasmtime 38.0.4, Clang 20.1.8, wrk `debian/4.1.0-4build2`.
- Both binaries use `cargo build --release --locked -j 2`, thin LTO and one codegen unit. Baseline means the working tree at the start of this performance task, including the preceding unreleased XDP implementation.
- Before: SHA-256 `8b95ee184a038807e9be8828f2444f0c900ef8fc861176a82841ed515c3ab16b`.
- After: SHA-256 `c6049300dd4561446c84d90dba15960469d87c8496e2a5141702a05855ccd7d5`.
- HTTP: two server workers on VM CPUs 0–1; two wrk threads on CPUs 2–3; 64 persistent HTTP/1.1 connections. The local NGINX upstream uses CPUs 4–5. CPU sets separate the task's processes, not the physical host or unrelated VM workloads.
- Each case warms up for one second, then runs for eight seconds, three rounds. Binary and case order reverse in alternate rounds. Tables report medians; raw JSON retains every round, CPU time, RSS, errors and wrk output. CPU microseconds/request uses the server process's user + system CPU time, not elapsed request latency.
- Local responses are two bytes; upstream responses are 16 KiB. Access logs, tracing, authentication, compression and configured rate limits are disabled in these capacity workloads; normal request metrics remain enabled.
- Both proxy comparison attempts overlapped another project's build/test activity on the shared VM. They are excluded from performance conclusions; the second attempt is retained as [excluded raw evidence](validation/performance-proxy-excluded-2026-09-26.json), including its qualification field. No unrelated workload was stopped. This environment did not provide a stable proxy-capacity window.

## Changes

CPU-clock sampling identified repeated Wasm mapping/teardown on the RGL path. In the initial branch profile, `unmap_vmas`, `tlb_finish_mmu` and `free_pgtables` together accounted for about 10.6% of samples, with Wasmtime instance destruction in their call stacks. The VM supports software CPU-clock sampling; hardware cycles/instructions counters were unavailable.

In the optimized branch profile those three symbols no longer reached the 0.5% reporting threshold. [Profile summaries](validation/performance-profile-2026-09-26.json) retain the sampling parameters, binary hashes and all symbols above that threshold. Profiling runs are separate from the throughput comparison.

Runtime modes now use Wasmtime's pooling allocator sized to the existing plugin admission limit, plus three control-plane slots. Each execution still owns a fresh Store, host state and instance. Memory is reset by Wasmtime before reuse; fuel, 8 MiB linear-memory limits, 1 MiB host limits and tenant permits remain active. The first 64 KiB per used pool slot may stay resident on Linux to reduce page faults. Pooling can therefore trade bounded resident memory for CPU time, especially with many concurrent proxy requests. Configuration-only commands retain on-demand allocation.

XDP port/protocol scope checks stop at the configured list length and first match. Non-expiring address-set hits and misses avoid an unnecessary monotonic clock read. The benchmark also uncovered a Clang diagnostic rejecting a standalone boolean predicate such as `if pkt.is_udp() then`; generated condition expressions now handle that valid RGL form.

## Local HTTP results

[Raw three-round comparison](validation/performance-http-2026-09-26.json), with no competing build/test observed during this comparison:

| Workload | Before req/s | After req/s | Throughput change | Before → after P99, ms | Before → after CPU µs/request |
| --- | ---: | ---: | ---: | ---: | ---: |
| No plugin | 185,753 | 184,845 | −0.5% | 0.563 → 0.654 | 10.54 → 10.57 |
| `route.pass()` plugin | 117,336 | 136,365 | +16.2% | 0.863 → 0.748 | 16.84 → 14.35 |
| Header branch + request-header edit | 78,960 | 118,257 | +49.8% | 1.329 → 0.977 | 25.07 → 16.72 |

All 18 measured runs completed without HTTP/socket errors. The branch saves approximately one third of server CPU time per request; it still executes the branch and host header operations. These percentages describe this workload and concurrency. Local-response median RSS stayed around 33–34 MiB.

The no-plugin path has no throughput benefit and showed a higher P99 in this initial comparison. A later [plain-response control run](validation/performance-plain-control-excluded-2026-09-26.json), three rounds of ten seconds, also encountered background activity: latency swung substantially for both binaries, so it is excluded from conclusions. No stable no-plugin tail-latency improvement or regression is established; a quiet host is needed to resolve it. Proxy performance likewise remains unqualified by this run. The excluded samples are not averaged into the local-response table or treated as a product failure.

## XDP kernel results

[Paired kernel runs](validation/performance-xdp-kernel-2026-09-26.json): seven rounds per binary/case, 100,000 repetitions per run, VM CPU 0. Each run also checks action, named rule packets and bytes. Values below are median kernel-reported means in ns, rounded to integer ns by the kernel API.

| Policy/configuration | Before ns | After ns |
| --- | ---: | ---: |
| Pass / UDP drop | 16 / 16 | 16 / 16 |
| One configured port and protocol | 37 | 16 |
| Full scope lists, match first entry | 29 | 16 |
| Full scope lists, match last entry | 27 | 29 |
| Single-port scope miss | 27 | 17 |
| Address-set miss / non-expiring hit | 20 / 21 | 19 / 20 |
| Per-source bucket plus global ceiling | 38 | 39 |

The useful measured gain is the common short/early-match scope path (37 → 16 ns, approximately 57% less time). The full-list last-match case costs 2 ns more; the optimization does not accelerate every configuration. The 1 ns set/limiter differences are near the API's reporting resolution and are not grounds for a broad performance claim. Common ports/protocols can be placed earlier in scope lists without changing match semantics.

## Live XDP mixed traffic

The [native veth run](validation/performance-xdp-veth-2026-09-26.json) completed all 18 windows (three rounds of none/pass/filter, with and without UDP traffic), totaling **15,543,059 HTTP requests with zero errors**. The sender offered approximately 99,955–99,972 UDP packets/s in the filtering phases, and the rule counter recorded 599,936 drops in each six-second flood. HTTP measurement lasts five seconds; the sender runs one extra second to cover client setup, so packet counters cover a longer interval than the HTTP latency sample. Counter deltas exclude warmup.

Observed HTTP medians were about 167k–174k req/s across all six groups. These samples verify filtering and continued HTTP service under mixed traffic; they do not show a stable HTTP acceleration from XDP. They ran on the same shared host and are not promoted to an isolated capacity result. System softirq counters describe the whole VM. The harness removed its processes, veth pair, network namespace and BPF pins after completion.

## Executed regressions

[Results and binary identity](validation/performance-regression-2026-09-26.json):

| Gate | Result |
| --- | --- |
| Rust unit/boundary tests | 13 passed |
| HTTP/TLS/body/reload integration | 82 passed |
| Product/authentication/governance/metrics | 111 passed |
| OTLP integration | 32 passed |
| XDP real-kernel/network/libxdp | 61 passed |
| Clippy all targets, warnings denied | PASS |
| Rust formatting, Python syntax and diff whitespace | PASS |

The new pooling regression mutates guest memory and globals, grows memory, holds concurrent instances, alternates modules and traps before reuse. It verifies fresh initial data/global values/memory size, response-hook state continuity, bounded allocation and the control-plane spare capacity. Existing tests continue to check host allocation and oversized Wasm rejection, fuel exhaustion, request bodies, failed reload retention and old requests completing across reloads.

An initial product run exposed a race in its health-metrics assertion: the transport-specific pool had completed its probe before the named pool's metrics changed. The test now waits for the named pool's unready count to converge, then performs the original total/eligible/unready assertions. Production health-check behavior was not changed. The full rerun passed.

## Reproduction

Save the release binary before changing source, then build the candidate. Do not benchmark during builds or other load tests. Adjust CPU lists to the machine.

```sh
python3 scripts/benchmark.py ./rgnix-before --compare ./rgnix-after \
  --client wrk --concurrency 64 --seconds 8 --rounds 3 \
  --server-cpus 0,1 --client-cpus 2,3 --output http.json

python3 scripts/benchmark.py ./rgnix-before --compare ./rgnix-after --proxy \
  --client wrk --concurrency 64 --seconds 8 --rounds 3 \
  --server-cpus 0,1 --client-cpus 2,3 --origin-cpus 4,5 --output proxy.json

sudo python3 scripts/xdp_kernel_benchmark.py ./rgnix-before --compare ./rgnix-after \
  --cpu 0 --rounds 7 --output xdp-kernel.json

sudo python3 scripts/xdp_benchmark.py ./rgnix-after --client wrk \
  --seconds 5 --rounds 3 --flood-pps 100000 \
  --server-cpus 0,1 --client-cpus 2,3 --flood-cpus 4 --output xdp-veth.json
```

The [wrk scripting API](https://github.com/wg/wrk/blob/master/SCRIPTING) supplies latency and duration in microseconds. The [Wasmtime instantiation guidance](https://docs.wasmtime.dev/examples-fast-instantiation.html) describes pooling and `InstancePre`; rgnix already used `InstancePre` before this change. [Kernel program test runs](https://docs.kernel.org/bpf/bpf_prog_run.html) execute the actual BPF program with synthetic input and return its verdict; they are distinct from the live veth test.

## Limits

These are short, closed-loop HTTP/1.1 measurements on a shared virtualized host. They do not establish open-loop SLO capacity, physical-NIC PPS, multi-queue contention, long-duration memory stability, native amd64 performance, TLS/HTTP2 capacity, high-cardinality limiter capacity or production Kubernetes/CNI performance. The XDP microbenchmark repeats one warm frame/key with fresh maps in each run and validates the rule packet/byte counters; kernel-reported ns must not be converted into promised wire throughput.

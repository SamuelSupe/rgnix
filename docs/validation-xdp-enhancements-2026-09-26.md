# XDP ABI 2 validation — 2026-09-26

This records executed checks for the current working source, not a GitHub release. The preceding [initial XDP validation](validation-xdp-2026-09-26.md) remains a historical ABI 1 result.

## Environment and identity

- OrbStack Linux `7.0.14-orbstack-00380-ga7e0a2dc9535`, aarch64, Rust 1.90, Clang 20.1.8, Aya 0.14.0, xdp-tools/libxdp 1.5.6.
- Current Linux debug binary SHA-256: `4c62d2bfe60645a635b8be151c4d2c9587e53b65d4db03fdd88d25cab94170ef`.
- The Kubernetes QA image copied this same binary into the existing Ubuntu 24.04 runtime base with iproute2 and CA certificates. It was local-only, named `rgnix:xdp-qa-20260926`; no release image was published.
- Kubernetes: OrbStack `v1.35.6+orb1`, one node. The test used a newly created `rgxqa926`/`rgpqa926` veth pair and namespace `rgnix-xdp-qa-20260926`. No node uplink/CNI interface was modified.

## Executed gates

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --locked --all-targets -j 2 -- -D warnings` | PASS |
| `cargo test --locked -j 2` | 12 passed |
| Existing HTTP integration | 82 passed |
| Existing OTLP transport/failure integration | 32 passed |
| XDP kernel/network integration, including `--libxdp` | 60 passed |
| Real Helm/ConfigMap/rolling restart/cleanup checks | 6 passed |
| Helm lint, disabled and enabled XDP values | PASS |
| `git diff --check` | PASS |

[XDP results](validation/xdp-enhancements-2026-09-26.json) cover observation, destination/protocol/port scope, parser/fragment actions, per-source/subnet/port packet and byte budgets, mandatory global ceilings, IPv4/IPv6 dynamic sets, expiry and overlapping prefixes, source diagnostics and repeatable object digests. The live network checks cover automatic projection/file updates, bad desired configuration and readiness, content-addressed rollback, consumed budget preservation, sampled OTLP protobuf events, persistent SIGKILL/restart takeover, explicit detach and interface removal.

The kernel accepted the documented restricted capability set (`BPF`, `NET_ADMIN`, `PERFMON`, `no_new_privs`). Restart takeover was changed to use pinned owned program FDs after a real test showed that global program enumeration required additional privileges; the repaired path passed with the restricted set. Program identity is now staged before other pinned maps to support recovery after partial loading.

The libxdp checks attached two independent components on the dedicated native veth. rgnix drops were enforced, component-only removal preserved the peer, and `XDP_PASS` continued into a peer that dropped the packet. A managed agent in generic mode refused the foreign native dispatcher. This qualifies the explicit immutable `--dispatcher` artifact path on this environment; it does not certify arbitrary CNI programs or a managed hot-reload integration with Cilium.

[Kubernetes results](validation/xdp-kubernetes-2026-09-26.json): the chart's verifier init container and node agent ran with the supplied security context. ConfigMap projection changed observation to enforcement without HUP; an invalid object hash retained generation 2 and failed the convergence exec probe; a corrected projection recovered it. DaemonSet rolling restart returned generation 1 with the same semantic policy digest while taking over the persistent link. Explicit detach removed XDP before deleting the dedicated veth and namespace. Node selection used the existing hostname label, without relabeling nodes. History persistence/rollback was tested with real files in the Linux network gate; the Kubernetes QA used `history: false` to avoid creating node history files.

## Controlled local performance baseline

Command:

```sh
sudo python3 scripts/xdp_benchmark.py /tmp/rgnix-target/debug/rgnix \
  --seconds 3 --rounds 3 --flood-pps 100000 --output xdp-benchmark.json
```

The final run started after compilation, container construction and other task tests completed. Each cell is the median of three runs. Four Python HTTP clients used persistent connections against the real rgnix `return 200 "ok"` endpoint with access logging disabled. A separate paced sender offered UDP to port 9 on the dedicated veth. Order alternated between rounds. `none` has no XDP; `pass` parses and accounts but permits traffic; `filter` drops UDP/9 and permits HTTP. Kernel program counters confirmed roughly 300,000 UDP drops per three-second attack phase.

| Mode | UDP load | Offered PPS | HTTP req/s | HTTP p99 ms | Server CPU (one core = 100%) | RSS MiB | HTTP errors |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| none | none | 0 | 23,848 | 0.487 | 124.1% | 44.3 | 0 |
| none | 100k target | 99,892 | 23,520 | 0.498 | 123.7% | 44.5 | 0 |
| pass | none | 0 | 23,293 | 0.496 | 123.7% | 44.2 | 0 |
| pass | 100k target | 99,915 | 23,115 | 0.507 | 122.7% | 44.2 | 0 |
| filter | none | 0 | 23,259 | 0.492 | 123.3% | 44.1 | 0 |
| filter | 100k target | 99,919 | 23,419 | 0.486 | 124.5% | 44.2 | 0 |

[Raw and summarized measurements](validation/xdp-benchmark-2026-09-26.json) also include process memory, kernel-reported per-frame execution time and system softirq counters. The kernel micro-run (`BPF_PROG_TEST_RUN`, 100,000 repetitions) reported 14 ns for pass and 15 ns for filtering; these synthetic kernel values are not end-to-end NIC latency.

**Interpretation:** this short, client-driven virtual-link test does not demonstrate a stable HTTP throughput improvement. At approximately 100k offered UDP PPS, all modes stayed near 23k HTTP requests/s with zero errors. Filtering removed the unwanted packets before the stack, but the remaining small throughput/p99 differences are not grounds for a production speedup claim. CPU numbers describe the HTTP process; softirq counters cover the entire VM and cannot isolate this interface. An earlier unpaced exploratory run overlapped image setup and is intentionally excluded from the published performance baseline.

## Remaining qualification boundaries

- Physical NIC/native driver throughput, hardware offload, and multi-buffer/jumbo behavior are not validated here.
- No native amd64 kernel execution, broad kernel/Clang-version matrix or multi-node capacity/rollout certification was available in this environment. CI includes a native Linux kernel gate and image build matrix, but those remote jobs were not executed in this turn.
- Kubernetes testing covered one node and a dedicated veth, not the actual uplink, production CNI traffic, or all cluster policies.
- libxdp artifacts have immutable embedded configuration. Dynamic address sets, managed OTLP consumption, preserved reload budgets and automatic publication are provided by the exclusive agent path; existing arbitrary CNI attachments are never overwritten.
- Sustained saturation, adversarial high-cardinality LRU churn, NIC queue affinity and long-running leak/soak measurements still need a controlled hardware testbed.

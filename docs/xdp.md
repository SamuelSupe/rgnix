# RGL XDP packet policies

Available in **v0.4.0 Preview** on Linux amd64/arm64. Current objects use **XDP ABI 2**. Recompile older objects before deploying this version.

`on_xdp()` is an administrator-owned packet hook compiled through RGL → C/Clang → eBPF → Linux verifier/JIT. Packets do not enter Wasm or userspace. A separate node agent owns one interface; HTTP services and tenant plugins retain their existing unprivileged execution model. XDP sees wire IPs/ports, not HTTP headers, TLS contents, POST bodies or tenant identity.

## Start in observation mode

```sh
cargo build --release --locked
rgnix xdp compile examples/edge-managed.rgl -o edge.o
sudo rgnix xdp doctor --interface eth0
sudo rgnix xdp check edge.o --config examples/xdp-policy.json --kernel
sudo rgnix xdp run edge.o --interface eth0 --mode native \
  --config examples/xdp-policy.json --history-dir /var/lib/rgnix-xdp
rgnix xdp status --ready
```

The example starts with `observe: true`, TCP ports 80/443, and an empty dynamic blocklist. Inspect `would_drop` counters and events before changing observation to false. Use the actual **pre-NAT wire ports and destination addresses**: these may be NodePorts or a load balancer address. Changes to a projected ConfigMap, JSON configuration, source or object are watched by content every second. `--watch-interval 0` disables polling; SIGHUP always retries a load.

Requirements: little-endian Linux, kernel 5.12+ with BPF links/atomics, writable **bpffs** for shared state, and `CAP_BPF`, `CAP_NET_ADMIN`, `CAP_PERFMON`. `doctor` reports actual interface/XDP information, BTF, mount state and process capabilities without attaching. It cannot prove native-driver support. Select `native` or `generic` explicitly; no fallback silently changes mode. `check --kernel` loads/verifies without attachment. Clang is needed only for `.rgl`; precompiled `.o` files work with the normal runtime image built from this source.

An administrator must mount bpffs if the node does not already provide it. The agent does not mount filesystems or need `SYS_ADMIN`. A shared `/run/rgnix-xdp` holds exclusive interface locks. In containers, both bpffs and that lock directory must refer to the host; see the Helm template. This prevents another pod from taking over an actively managed persistent link.

## Packet language

```lua
function on_xdp()
    if pkt.src_in_set("blocked") then
        return xdp.drop("blocklist")
    end
    if pkt.tcp_syn() and not xdp.allow("client-syn", "src_ip", 100, 200) then
        return xdp.drop("syn-rate")
    end
    return xdp.pass("accepted")
end
```

| API | Contract |
| --- | --- |
| `pkt.src_in("CIDR")`, `pkt.dst_in("CIDR")` | Literal IPv4/IPv6 network membership. Wrong address family is false. |
| `pkt.src_in_set("name")`, `pkt.dst_in_set("name")` | Membership in a bounded administrator-configured dynamic address set. |
| `pkt.ip_version()` | 0, 4 or 6. |
| `pkt.protocol()`, `pkt.is_tcp()`, `pkt.is_udp()` | IP protocol/resolved IPv6 next header. No inner tunnel decoding. |
| `pkt.src_port()`, `pkt.dst_port()`, `pkt.has_ports()` | Host-order ports for complete unfragmented TCP/UDP headers; otherwise ports are zero. |
| `pkt.tcp_syn()` | TCP SYN set and ACK clear, with parsed ports. |
| `pkt.fragmented()` | IPv4 MF/nonzero offset or IPv6 fragment header, including atomic fragments. |
| `pkt.len()` | Visible Ethernet bytes, including headers; not HTTP body bytes. |
| `xdp.allow("id", "key", rate, burst)` | Consume one token and return whether admitted. |
| `xdp.allow_bytes("id", "key", rate, burst)` | Consume packet length, measured in bytes. |
| `xdp.pass("rule")`, `xdp.drop("rule")` | Final verdict with optional stable rule name. Implicit completion passes. |
| `xdp.allow_rate(PPS)` | Compatibility API: one-second shared fixed window per call site; new policies should use named token buckets. |

Keys: `global`, `src_ip`, `src_subnet` (IPv4 /24, IPv6 /64), `dst_port`, `src_ip_dst_port`. Packet/byte rate and burst are literal integers from 1 through 1,000,000,000. Rate budgets are per interface, shared exactly across CPUs through atomic timestamps. They are not cluster-wide or per tenant. A false check does not itself drop: the RGL code decides its verdict. Short circuit evaluation determines which budgets are consumed; consuming an earlier budget is not rolled back if a later check fails.

Named buckets use conservative GCRA token accounting. A burst costs tokens immediately; time replenishes them up to the burst capacity. Nanosecond rounding can slightly under-admit high rates. Eight failed CAS attempts deny conservatively and increment a separate contention metric. A packet larger than a byte bucket's burst is rejected.

Global named buckets use a bounded 256-entry hash. Keyed buckets use a **65,536-entry LRU map**; cold entries are reclaimed and an idle/new bucket starts with a full burst. LRU replacement can reset an individual source's debt, so any policy containing keyed checks also enforces an **independent non-LRU global ceiling** before RGL, using `ceiling_pps`/`ceiling_burst` (defaults 1,000,000/10,000). The ceiling counts all in-scope parsed packets, regardless of which branch uses a keyed limit. Map creation failures deny rather than bypass. Metrics expose occupancy, capacity, insertions, budget denials and contention; insertions include replacements and are not claimed as exact eviction counts.

The identity of a named budget includes its name, key, rate, burst and byte/packet unit. Changing unrelated code, rule order, address lists, or observation mode retains that budget. Changing budget parameters intentionally creates a new budget. Pinned maps retain named budgets on restart within the same boot. Old compatibility `allow_rate` windows reset on changed policies.

Supported statements: `local`, assignment, `if/elseif/else`, comparisons and boolean short-circuiting. No loops, helpers, recursion, arbitrary memory, network redirect, packet rewriting or HTTP APIs. Bounds: source 64 KiB, 1,024 AST nodes, depth 32, 64 locals, 64 named budgets, 128 named verdicts, 32 address sets. Rule names are unique 1–64-character ASCII identifiers (`A-Z a-z 0-9 _ . -`). Unnamed verdicts get source-line identifiers; explicit names remain stable when lines move. Type errors report RGL line/column; Clang emits BTF line information referencing `policy.rgl` for kernel diagnostics. The verifier remains the final authority.

## Configuration and parser decisions

JSON version 1 rejects unknown fields. [Example](../examples/xdp-policy.json):

| Field | Meaning |
| --- | --- |
| `policy` | Optional object/source path relative to this JSON file; otherwise the CLI source is used. |
| `policy_sha256` | Optional required SHA-256 of the compiled object, checked before load. This is integrity checking, not a signature or authorization boundary. |
| `revision` | Operator label, up to 128 characters; status also reports content digests. |
| `observe` | Evaluate the complete policy and budgets, but turn drops into passes. Record `would_drop`. |
| `scope.destinations`, `scope.ports`, `scope.protocols` | OR within each list, AND across lists. Empty list means unrestricted. Bounds 32 CIDRs, 32 ports, 16 numeric IP protocols (6 TCP, 17 UDP). |
| `malformed`, `unsupported` | `pass` or `drop`; default `drop`. |
| `fragments` | `pass`, `drop`, or `policy` (default). |
| `ceiling_pps`, `ceiling_burst` | Mandatory positive global ceiling for policies using keyed buckets. |
| `event_sample_every`, `event_max_per_second` | Sample 1/N drop decisions, then cap all samples at a global token budget. N=0 disables; cap defaults to 100/s, maximum 1,000/s. |
| `sets` | Map of set names to `{ "cidr": "...", "expires_at": UNIX_SECONDS }` entries. Expiry 0/omitted means permanent. |

Scope is evaluated **before malformed/unsupported/fragment actions and RGL**. Out-of-scope frames always pass. If a truncated header or fragment cannot provide a requested scope field (for example a TCP port), it cannot match and passes. Use an IP/protocol scope without a port restriction when explicitly filtering fragments. Parsing does not infer missing bytes. ARP/non-IP traffic can reach an unrestricted policy with IP version zero.

Parser support: Ethernet, two VLAN tags, IPv4 options, up to eight IPv6 extension headers. Excess VLAN/extension nesting is unsupported; truncated/inconsistent lengths are malformed. IPv6 jumbograms are unsupported by this parser. Checksums are left to the ordinary network stack. Source IP can be a NAT/LB address and is not a trusted client/tenant identity.

Address sets share a bounded 16,384-entry LPM trie; scope adds at most 32 entries. Duplicate normalized CIDRs within a set are rejected. Absolute expiry is converted to monotonic time on load, so reload does not extend bans; later wall-clock adjustments do not change an already loaded deadline. An expired specific prefix does not hide an active covering prefix. New lists are installed into a fresh map and published with the program in one atomic link update; readers never see a half-populated list. Source/object compilation is reused when only JSON changes. Expired entries stop matching in kernel without a watcher round trip; their storage is reclaimed when the next snapshot replaces the trie.

## Publication, rollback and persistent enforcement

Compilation, integrity validation, map population and kernel verification finish before atomic `BPF_LINK_UPDATE`. Any failure retains the last working filter. `/status` reports desired/applied input digests, the semantic policy revision, object digest, operator revision, generation, last error, history error and rule counters. `/readyz` returns 503 while desired configuration is invalid or unapplied; `/healthz` stays healthy while the last good filter is running. `rgnix xdp status --ready` and `--health` are suitable exec probes.

The read-only admin listener defaults to `127.0.0.1:9191`. Restrict network exposure if changing it; it contains network policy diagnostics. There are no unauthenticated remote write endpoints. Updates/rollback use filesystem permissions or Kubernetes ConfigMap RBAC.

`--history-dir DIR` saves content-addressed objects and JSON with fsync/atomic file replacement. It retains 20 published configurations and their objects in this dedicated directory. History failures are visible in status/metrics. Revert using a policy revision from `/status`:

```sh
rgnix xdp rollback --history-dir /var/lib/rgnix-xdp \
  --revision <64-character-policy-revision> --config /etc/rgnix-xdp/policy.json
rgnix xdp status --ready
```

History survives process restarts. The rollback command publishes the saved configuration; the running watcher still verifies it before applying. For Kubernetes, generate the rollback JSON into a writable staging file and update the ConfigMap through the normal administrator/GitOps workflow; projected volumes are read-only.

Default lifecycle: unpinned link, automatically removed on exit/crash. There is a restart gap. **`--persist` is optional**: pin the link, its current program, identity and budget maps so SIGTERM/SIGKILL retain enforcement. Startup validates interface, network namespace, mode, ABI, map ownership and the pinned program before a compare-and-replace takeover. It never steals another running instance's interface lock. Program pins are staged before replacement so interruption cannot leave an unrecoverable link/program mismatch. This covers process/container restarts, not host reboot or an interface disappearing.

For emergency removal, first stop the service/DaemonSet from restarting, then in the same host network namespace and with the same bpffs/lock mounts:

```sh
sudo rgnix xdp detach --interface eth0 --mode native
```

`detach` checks ownership, refuses an active agent, and only removes this agent's pins/link. Uninstalling a persistent agent does **not** remove enforcement automatically; include explicit detach in the node maintenance procedure. No program on another interface is removed.

## Metrics and OTLP packet events

`/metrics` has low-cardinality global and named-rule series:

- `rgnix_xdp_packets_total{action="pass|drop"}`: actual decisions.
- `rgnix_xdp_rule_packets_total` / `rule_bytes_total` with bounded `rule` and `action="pass|drop|would_drop"` labels.
- `malformed_total`, `would_drop_total`, `scope_bypass_total`.
- `rate_denied_total`, `rate_contention_total`, `rate_state_errors_total`, `rate_insertions_total`, `rate_entries` (30-second sample), `rate_capacity`.
- `config_generation`, `reload_failures_total`, `config_converged`, `last_apply_timestamp_seconds`, `observe`, `history_errors`.
- `events_lost_total`, plus the existing `rgnix_otlp_logs_*` export/queue/retry metrics.

Global counters accumulate across reloads; surviving named rule counters do too. Counters reset on agent restart; budget maps can persist. Cutover counter sampling is approximate. Removed rule labels are retired rather than accumulating forever. Source IPs never become metric labels.

Enable `event_sample_every` and an OTLP/HTTP protobuf endpoint:

```sh
rgnix xdp run edge.o --interface eth0 --config policy.json \
  --otlp-logs-endpoint https://collector.example/v1/logs
```

Existing OTEL headers, CA, resource attributes, batching and bounded retry/queue options apply. Records use instrumentation scope `rgnix.xdp` and event `rgnix.xdp.drop`, with rule, reason, policy digest, source/destination IP/port, protocol, bytes and observed/enforced action. A 256 KiB kernel ring, per-second sample cap and bounded exporter isolate packet processing from an unavailable collector. Events contain network identifiers; leave sampling disabled when those should not leave the node. These are packet events, **not HTTP access logs or fabricated spans**. No packet payload is exported.

## CNI and libxdp interoperability

`rgnix xdp run` owns an exclusive BPF link and refuses existing CNI/XDP programs. It does not migrate Cilium or assume every CNI uses a compatible dispatcher.

For an explicitly **libxdp-compatible** interface, an administrator can instead compile an immutable BTF component:

```sh
rgnix xdp compile examples/edge.rgl --dispatcher -o edge-dispatcher.o
sudo xdp-loader load --mode native --prio 50 --actions XDP_PASS eth0 edge-dispatcher.o
sudo xdp-loader status eth0
# Remove only the component ID reported for rgnix_xdp, not the whole dispatcher:
sudo xdp-loader unload eth0 --id <rgnix-component-id>
```

`--config policy.json` can embed scope, observation mode and limiter settings at compile time. Dispatcher artifacts reject dynamic sets and sampled events, whose lifecycle requires the managed agent. They support literal CIDRs and named rate buckets. They cannot be loaded into `rgnix xdp run`, and external loader updates do not promise the managed agent's state preservation/atomic publication. Configure these immutable components through the CNI/libxdp owner's deployment procedure. This separate artifact avoids silently replacing a CNI, or promising unsupported live CNI mutation. Default `XDP_PASS` chain actions ensure accepted packets continue through other components; do not enable chaining on `XDP_DROP` if drops must remain final.

## Kubernetes rollout

The optional Helm `xdp.enabled` deploys a separate DaemonSet with minimal capabilities, no API token, host bpffs/locks, optional node-local history and convergence-aware probes. Set `xdp.image.tag: "0.4.0"`, `xdp.policyConfigMap`, interface and node selector. It is disabled by default and never changes the HTTP Deployment's privileges. The ConfigMap must contain the compiled object and JSON, mounted as a whole volume so projected updates are visible.

Use disjoint node labels such as `rgnix.io/xdp-ring=canary|stable` and separate Helm releases/ConfigMaps. Start canaries in observe mode; inspect rule counters and node `/status`, enforce on canaries, then promote the same immutable artifact digest. `maxUnavailable: 1`, `minReadySeconds: 10` and `/readyz` convergence stop progression on rejected updates. A persistent link can cover binary restart intervals; plan explicit detach before node/interface/CNI removal. Node-local history is not replicated across nodes.

See [the standalone DaemonSet example](../examples/xdp-daemonset.yaml). Seccomp must permit BPF operations; the supplied profile is unconfined because common default profiles deny them. Narrow it for the target runtime if required. A policy SHA in JSON prevents a temporarily mismatched object/config projection from publishing; the previous filter remains active until both match.

## Replay and verification

See the [executed ABI 2 validation and performance record](validation-xdp-enhancements-2026-09-26.md).

```sh
sudo rgnix xdp test edge.o --config policy.json --packet ethernet.bin --repeat 1000
sudo rgnix xdp replay edge.o --config policy.json --pcap capture.pcap
sudo python3 scripts/xdp_integration.py target/debug/rgnix --libxdp --output xdp-validation.json
cargo build --release --locked
sudo python3 scripts/xdp_benchmark.py target/release/rgnix --client wrk \
  --server-cpus 0,1 --client-cpus 2,3 --flood-cpus 4 --output xdp-benchmark.json
sudo python3 scripts/xdp_kernel_benchmark.py ./rgnix-before --compare target/release/rgnix \
  --cpu 0 --rounds 7 --output xdp-kernel-comparison.json
```

Replay accepts classic Ethernet PCAP (micro/nanosecond, either byte order), at most 64 MiB/10,000 complete frames. It reports actual kernel verdict, rule and execution time per frame, sharing budgets across frames. It replays immediately using live monotonic time, not capture timestamps; this is a policy simulator, not an offline rate/time model. Snaplen-truncated frames and pcapng are rejected.

Integration uses only its own veth/network namespace. The benchmark compares no XDP, pass-only and filtering, with and without a UDP flood, while measuring real rgnix HTTP throughput/p99, process CPU/RSS, system softirq time, offered PPS and XDP counters. Virtual-interface results are a repeatable local baseline, not physical-NIC capacity or a promised speedup. Physical NIC, amd64 kernel, production CNI and multi-node rollout remain separate qualification targets.

Use CPU lists available on the test VM, and install `wrk` before selecting the native client. The kernel comparison alternates binaries and validates verdict, rule packet count and byte count in every run; its ns values describe a repeated warm frame with fresh maps per run, not wire latency. See the [performance comparison using release builds](validation-performance-2026-09-26.md) for the measured optimization and its limits.

References: [Linux BPF maps](https://docs.kernel.org/bpf/map_hash.html), [Aya 0.14](https://docs.rs/aya/0.14.0/aya/), [libxdp dispatcher contract](https://github.com/xdp-project/xdp-tools/blob/main/lib/libxdp/README.org).

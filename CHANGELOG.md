# Changelog

## 0.4.0 — 2026-09-26 · Preview

Optional RGL XDP packet policies, Gateway publication controls and resilient lifecycle handling. See the [release notes](docs/releases/v0.4.0.md) for installation, upgrade requirements and validation boundaries.

- Restrict admission calls by IngressClass/Gateway ownership, hot-reload validated admission certificates, and support overlapping CA rotation and cert-manager CA injection.
- Aggregate stage-qualified candidate samples across controller replicas before advancing a rollout; missing/stale observations pause progress.
- Move Kubernetes plugin compilation into a bounded queue with per-namespace limits and administrator compile-rate budgets, leaving endpoint and certificate withdrawals independent of JIT work; wake reconciliation after compilation and wait for accepted plugin recovery before initial readiness.
- Add configurable shutdown budgets, a preStop drain marker, readiness withdrawal and HTTP/1.1 Connection close before termination; keep active asynchronous requests alive within the shutdown deadline. Extend Gateway checks with persistent connections, follower-only rollout samples, admission outages, certificate rotation and gRPC stream completion during Pod termination.

- Add HTTPRoute RequestMirror with percent/fraction sampling, ReferenceGrant/BackendTLS validation and bounded complete-body forwarding; add absolute request/backendRequest deadlines that also stop trickling streams without request replay.
- Add Gateway/HTTPRoute/GRPCRoute isolated candidate preflight and TLS admission, including strict candidate-plugin validation, namespace authorization and protected rollout state.
- Add staged admission registration so Gateway installation can start its TLS validator before registering the fail-closed webhook.
- Add opt-in Ingress/Gateway replica publication reports, fixed-label metrics/alerts, authenticated `/v1/fleet` and `rgnix wait` expected-digest/minimum-replica gates.
- Add opt-in revision-aware hard spreading across multiple nodes; scope business/admission Services and PDBs to HTTP controller Pods so same-release XDP agents cannot pollute discovery or availability counts.
- Extend release acceptance with three-node kind configuration, configurable route scale, mixed HTTP/TLS/RGL/body/OTLP load, publication during in-flight requests and Pod replacement. See the [production gate](docs/production-readiness.md) and [executed scope](docs/validation-product-2026-09-26.md); full Gateway conformance is not claimed.

- Pool Wasmtime resources within the runtime plugin budget while keeping fresh request instances; short-circuit XDP scope scans and avoid clock reads for non-expiring address sets. Add native wrk, paired binary and verified kernel benchmarks with an [executed performance record](docs/validation-performance-2026-09-26.md).
- Fix standalone boolean XDP predicates rejected by Clang; make the product health-metrics regression wait for the named pool's probe convergence.
- Add XDP ABI 2: scoped observation/enforcement, named per-source/subnet/port token buckets and byte budgets, independent global ceilings, bounded LRU state, expiring LPM address sets and stable rule counters.
- Add sampled OTLP packet events using the existing bounded exporter; automatic content-based configuration/object watch, desired/applied status, readiness convergence, integrity checks and durable 20-revision rollback.
- Add opt-in persistent link/program/map ownership and restart takeover, explicit safe detach, shared host locks, PCAP explanation, source diagnostics, `xdp doctor/status`, immutable libxdp dispatcher artifacts and an optional Helm DaemonSet.
- Add isolated functional, dispatcher interoperability and real HTTP/UDP-load benchmark gates. Hardware NIC and multi-node performance qualification remain separate from virtual-interface evidence.

- Add administrator-owned `on_xdp()` RGL packet policies compiled through Clang to real eBPF: IPv4/IPv6 CIDRs, TCP/UDP ports, SYN checks, bounded parsing and atomic packet-rate budgets.
- Add `rgnix xdp compile/check/run/test`, exclusive native/generic BPF-link attachment, atomic SIGHUP replacement with last-good retention, automatic detach, health endpoints and Prometheus counters.
- Provide an isolated Linux kernel/veth regression gate, a separate node DaemonSet example, and explicit packet-language, privilege, CNI coexistence and lifecycle boundaries.

## 0.3.0 — 2026-09-25 · Preview

Gateway API, shared request-rate quotas, durable recovery and business-metric rollback. This preview adds native Linux amd64/arm64 release archives, a versioned GHCR image and an OCI Helm chart. See the [release notes](docs/releases/v0.3.0.md) for installation, upgrade boundaries and validation.

### Gateway and governance

- Persist Gateway last-good plugins in controller-namespace checkpoints, binding Gateway/Route/ConfigMap UIDs and preserving live revocation checks; remove checkpoints for deleted or recreated sources.
- Apply JWT, external auth, rate limits, mirrors and staged rollout controls to Gateway routes; support BackendTLSPolicy trust/hostname validation with connection-pool isolation. Failed authentication policy dependencies cannot fall through to a public route.
- Add optional Redis-coordinated route and namespace request rates, bounded admission queries, explicit closed/open/local failure modes, hot Secret configuration and metrics.
- Add sustained external business-metric rollback with configurable treatment of unavailable providers, durable rollback annotations and replica/restart recovery.

- Add a pre-provisioned Gateway API mode with HTTPRoute/GRPCRoute matching, Service weights, request/response header modifiers, redirects/rewrites, ReferenceGrant enforcement, namespace attachment rules and live TLS/EndpointSlice updates. Conformance certification and infrastructure provisioning are not claimed.
- Add conservative NGINX assessment/candidate generation, Ingress-to-Gateway conversion and offline request-sample comparison commands.
### Observability

- Add upstream phase latency and connection reuse, request-body traffic and inspection, backend and tenant budget usage, controller watch/Lease state, rollout gates and Linux CPU/RSS/FD metrics with bounded labels.
- Add a dedicated Helm metrics Service, optional ServiceMonitor and Prometheus alert examples.
- Validate and propagate W3C traceparent/tracestate, create distinct backend/auth/mirror child spans, and correlate local and OTLP access logs with the server span.

### Distribution

- Add native amd64/arm64 release gates, Kubernetes Gateway regression checks, GHCR multi-platform images, SBOM/provenance, keyless image signatures, OCI charts and attested release archives.
- Keep version metadata, chart image defaults, release notes and checksums aligned; retain earlier release assets unchanged.

## 0.2.0 — 2026-09-25 · Preview

Request-body routing, observability, traffic policies, multi-tenant governance and staged releases. The release includes a Linux arm64 binary archive, a Helm chart and SHA-256 checksums. See the [release validation](docs/validation-release-0.2.0.md) for exact artifact identity and test coverage. No public registry image is included.

### Tenant and release governance

- Enforce rollout fallback even when RGL explicitly chooses another backend.
- Add administrator domain grants and cross-namespace hostname ownership, with namespace-scoped watches and Helm RBAC.
- Add named management identities and namespace permissions; atomically reload credentials, tenant policy and metric providers.
- Add staged rollout weights, stable cohorts, pause/resume/approval, persisted progress and external JSON metric gates.
- Add candidate configuration diffs, Ingress preflight and an optional TLS admission webhook sharing runtime validation.
- Show final outbound plans in isolated simulations; validate business certificate names and validity, export expiry metrics and support Ingress HTTPS redirects.

### Platform policies and change management

- Fix HTTPS health-probe TLS profiles, gRPC error metrics/spans, external-auth failover, and admission ordering.
- Add upstream mTLS and local JSON access logs with configurable query redaction and field selection.
- Add administrator namespace quotas, isolated admission/plugin/auth/mirror budgets and limiter tables.
- Add Ingress Service weights, bounded asynchronous mirrors and durable metric-triggered rollback shared through Kubernetes watches.
- Add read/write management roles, JSON audit, authentication/rate-limit simulation and persistent standalone configuration history with archived dependencies.
- Preserve source diagnostics when validating archived configuration. Keep business request retries disabled.


### Routing, transport and observability

- Per-Ingress body/timeouts/keepalive/logging, authentication, compression and traffic policies, including durable recovery without a custom plugin and live dependency revocation.
- Trusted real-IP chains, PROXY v1/v2, CIDR ACLs, route/key rate and concurrency limits, and backend request budgets.
- HTTP/2 and h2c upstreams, gRPC duplex streaming/trailers, verified Ingress HTTPS/private CA and CA-isolated connection pools.
- Active HTTP health checks, least connections, stable weighted hashing, managed affinity cookies and DNS TTL refresh.
- JWT/JWKS verification and remote key rotation, external authorization with vetted identity headers, and SNI client certificate policies revalidated after CA rotation.
- Typed JSON, query, cookie, stable hash and verified claim RGL helpers; static alias/SPA fallback and negotiated streaming gzip/Brotli.
- Authenticated runtime diagnostics, effective configuration, route explanation, offline simulation, file snapshot history/rollback, bounded route/backend metrics and OTLP server/client traces with log correlation.
- Local access/error log rotation by size or UTC interval, retained gzip archives, live policy reload, USR1 reopen for external logrotate, bounded background writing and filesystem/drop metrics.
- OTLP/HTTP protobuf access-log export for standalone and Ingress, with authentication headers, resource attributes, verified HTTPS/custom CA, bounded batching/retries, drop metrics and shutdown flushing.
- Opt-in request body routing: bounded full-body inspection, prefix inspection, raw byte search and JSON Pointer string lookup. The full original body is still forwarded once.
- Configurable total inspection timeout, resource budgets, HTTP/2 and `100-continue` support; streaming remains the default.
- Ingress body-policy annotations are checkpointed and published with their plugin; invalid changes retain the accepted combination while endpoint and resource withdrawals continue.
- Additive bounded replay-buffer API in the vendored Pingora core 0.9.0; see [patch provenance](vendor/README.md).

## 0.1.0 — 2026-09-25 · Preview

First public preview of rgnix, a programmable Rust HTTP server and Kubernetes Ingress controller.

### Included

- Pingora/OpenSSL HTTP and HTTPS proxying, client HTTP/2, WebSocket, SSE, streaming bodies and static files.
- An explicit NGINX HTTP configuration subset with diagnostics for unsupported directives.
- RGL compilation to portable Wasm and Wasmtime/Cranelift machine code at load time, request/response hooks and bounded per-request instances.
- Atomic file reloads, SNI certificate updates, health/readiness endpoints, metrics and graceful termination.
- Standard Ingress reconciliation, EndpointSlice discovery, ConfigMap plugins, TLS Secrets, durable accepted configuration and Lease-based status reporting.
- A two-replica Helm chart and Linux amd64/arm64 image build configuration.

### Latest corrections

- Script-selected HTTPS upstream aliases retain TLS and certificate validation; mixed-protocol aliases are rejected when plugins are present.
- A request hook cannot issue multiple routing decisions and accidentally select a different same-kind action.
- Ingress status operations have independent timeouts and bounded concurrency; long batches renew their Lease.
- Automatic trailing-slash redirects use the matching location's settings.
- Date-based If-Range requires exact Last-Modified equality.
- `$host` falls back to the selected server's primary name when the request has no authority.

Earlier corrections and their evidence are linked from the [latest validation record](docs/validation-semantics.md).

### Validation and artifacts

The Linux arm64 binary is the verified qa14 artifact: 5 Rust tests, 59 HTTP checks, 46 NGINX 1.28.0 comparisons, 39 mock API recovery scenarios, and 41 real Kubernetes lifecycle checks passed. A two-replica rolling upgrade completed 300 successful requests. Artifact identity is recorded in [semantics-artifact.json](docs/validation/semantics-artifact.json).

Release downloads include a Linux arm64 archive, its runtime requirements, examples, and SHA-256 checksums. No amd64 runtime acceptance, multi-node/cloud load-balancer validation, or long-duration production load test is claimed. No public container image is published with this preview.

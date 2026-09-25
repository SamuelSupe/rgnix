# Changelog

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

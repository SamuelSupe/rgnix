# Changelog

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

<p align="center">
  <img src="docs/assets/readme-hero.svg" alt="rgnix — Rust HTTP server with compiled routing, Ingress and Gateway API" width="100%">
</p>

<p align="center"><strong>NGINX-style configuration. Lua-style routing. Compiled execution.</strong></p>

<p align="center">
  <a href="https://github.com/SamuelSupe/rgnix/actions/workflows/ci.yml"><img src="https://github.com/SamuelSupe/rgnix/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/SamuelSupe/rgnix/releases"><img src="https://img.shields.io/github/v/release/SamuelSupe/rgnix?include_prereleases&amp;color=ea580c" alt="Latest preview release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-0f766e" alt="Apache 2.0"></a>
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/Rust-2024_edition-334155" alt="Rust 2024 edition"></a>
</p>

<p align="center">
  <b>English</b> · <a href="README.zh-CN.md">简体中文</a><br>
  <a href="#quick-start">Quick start</a> · <a href="#programmable-routing">Routing</a> · <a href="#kubernetes-ingress">Kubernetes</a> · <a href="#logs-and-observability">Observability</a> · <a href="#validation">Validation</a>
</p>

**rgnix** is a Rust HTTP server, reverse proxy, and Kubernetes Ingress/Gateway API controller in one binary. [Pingora](https://github.com/cloudflare/pingora) and OpenSSL handle transport. **RGL**, a small Lua-style language, compiles to WebAssembly and then to native code through Wasmtime/Cranelift when configuration is loaded.

> **v0.3.0 Preview** adds Gateway API, durable plugin recovery, shared request-rate quotas, business-metric rollback, richer metrics and W3C tracing. [Download](https://github.com/SamuelSupe/rgnix/releases/tag/v0.3.0) · [Release notes](docs/releases/v0.3.0.md) · [Changelog](CHANGELOG.md). Review the [validation scope](#validation) and [NGINX compatibility matrix](docs/compatibility.md) before deployment.

## What you get

**New in 0.3:** [Gateway API](docs/gateway-api.md), [shared rate limits](docs/shared-rate-limits.md), [migration tools](docs/migration.md), and native Linux amd64/arm64 distribution. Gateway uses a pre-provisioned data plane; upstream conformance certification is not claimed.

| Area | Capabilities |
| :--- | :--- |
| **HTTP & proxy** | HTTP/1.1, HTTPS, client/upstream HTTP/2, h2c, gRPC duplex streams and trailers, WebSocket, SSE, streaming bodies |
| **Configuration** | A documented NGINX subset, strict diagnostics, header inheritance, URI replacement, atomic reloads |
| **Compiled routing** | Typed RGL, request/response hooks, JSON/query/cookie/claim helpers, bounded full-body or prefix inspection |
| **Traffic & security** | Trusted real IP and PROXY v1/v2, CIDR ACLs, JWT/JWKS, external auth, client/upstream mTLS, rate and concurrency limits |
| **Backends** | Weighted round robin, least connections, weighted hashing, affinity cookies, active/passive health checks, DNS TTL refresh |
| **Static & TLS** | Root/alias, index, conditional and single-range requests, restricted SPA fallback, gzip/Brotli, SNI certificate reload and expiry metrics |
| **Kubernetes** | Standard Ingress with durable recovery; Gateway/HTTPRoute/GRPCRoute preview, Service weights, ReferenceGrant, live EndpointSlice and TLS Secret updates |
| **Tenant governance** | Administrator quotas and domain grants, namespace-scoped watches/RBAC, named reader/writer identities and audit logs |
| **Release management** | Service weights, bounded mirrors, stable cohorts, staged canaries, approvals, metric gates, automatic error/latency and business-metric rollback |
| **Operations** | OTLP logs/traces, local log rotation, Prometheus metrics, simulation, candidate diff/preflight, optional admission webhook, persistent standalone rollback |

## Quick start

### Linux amd64 or arm64

Download the matching archive and **SHA256SUMS** from [v0.3.0](https://github.com/SamuelSupe/rgnix/releases/tag/v0.3.0). For amd64 (use `arch=arm64` on AArch64):

```sh
arch=amd64
archive="rgnix-0.3.0-linux-$arch.tar.gz"
curl -fLO "https://github.com/SamuelSupe/rgnix/releases/download/v0.3.0/$archive"
curl -fLO https://github.com/SamuelSupe/rgnix/releases/download/v0.3.0/SHA256SUMS
grep " $archive\$" SHA256SUMS | sha256sum -c -
tar -xzf "$archive"
cd "rgnix-0.3.0-linux-$arch"
./rgnix check -c examples/nginx.conf
./rgnix serve -c examples/nginx.conf
```

In another terminal, run `curl http://localhost:8080/health`. The static home page works immediately; `/api/` expects application upstreams on ports **9001–9003**. Read the [Linux runtime requirements](docs/install-binary.md). Relative paths resolve against the main configuration file's directory.

### Container or source

The non-root image contains native binaries for **linux/amd64 and linux/arm64**:

```sh
git clone --branch v0.3.0 https://github.com/SamuelSupe/rgnix.git
cd rgnix
docker run --rm -p 8080:8080 -v "$PWD/examples:/etc/rgnix:ro" \
  ghcr.io/samuelsupe/rgnix:0.3.0 serve -c /etc/rgnix/nginx.conf
```

To build from source, use **Rust 1.90+** on Linux (`Cargo.lock` pins dependencies; Docker and CI use Rust 1.98.0):

```sh
# Debian / Ubuntu; install Rust separately.
sudo apt-get update
sudo apt-get install -y build-essential cmake pkg-config libssl-dev
cargo build --release --locked
./target/release/rgnix serve -c examples/nginx.conf
```

[Image signatures, checksums and attestations](docs/releases.md) · [Deployment instructions](docs/deployment.md).

## Programmable routing

Attach `.rgl` source or compiled `.wasm` with `rgnix_script`:

```nginx
events {}
http {
    upstream app { server 127.0.0.1:9001; }
    upstream canary { server 127.0.0.1:9002; }
    server {
        listen 8080;
        server_name app.example.com;
        location /api/ {
            proxy_pass http://app/;
            rgnix_script routes.rgl;
        }
    }
}
```

```lua
-- routes.rgl
function on_request()
    if req.header("x-canary") == "1" then
        return route.proxy("canary")
    end
    return route.pass()
end

function on_response()
    resp.set_header("x-proxy", "rgnix")
end
```

`route.pass()` uses the matched action and `proxy_pass` URI replacement. `route.proxy()` selects an allowed backend and preserves the request URI. `req.set_path()` sets the final path without rematching the location. A latched rollout rollback takes precedence over a script's backend choice.

Each request has its own Wasm instance, with defaults of **100,000 fuel per hook**, **8 MiB Wasm memory** and **1 MiB cumulative host data**. Host changes commit only after a successful hook. No WASI, filesystem, network or process APIs are exposed. RGL is an independent language; it does not run arbitrary Lua modules. [Language and API reference](docs/rgl.md).

### Route by POST body, including a truncated prefix

| Inspection policy | Example decision | Forwarding |
| :--- | :--- | :--- |
| `rgnix_request_body full 64k;` | `req.json_string("/tenant") == "vip"` | Buffer within the configured limit, then forward the complete body |
| `rgnix_request_body prefix 4k;` | `req.body_contains("route=vip;")` | Inspect only the first 4096 bytes, then forward those bytes and the remaining stream |

Truncation affects only the plugin's decision input. **The original request body is forwarded in full, once.** Inspection is opt-in, bounded by size/time/concurrency limits, and available in both standalone and Ingress modes. [Body API](docs/request-body.md) · [Runnable example](examples/body-routing.conf).

## Kubernetes Ingress

Install the versioned OCI chart:

```sh
helm upgrade --install rgnix oci://ghcr.io/samuelsupe/rgnix/charts/rgnix \
  --version 0.3.0 --namespace rgnix-system --create-namespace
kubectl -n rgnix-system rollout status deployment/rgnix
```

The chart defaults to **two replicas** and includes an IngressClass, RBAC, Service, probes, PDB and graceful termination. No custom CRD is required. Prepare application Services and TLS Secrets before applying the [Ingress example](examples/ingress.yaml).

- Exact/wildcard hosts, default backends, named/numeric Service ports, and Kubernetes `Exact`/`Prefix` paths; `ImplementationSpecific` uses Prefix semantics.
- Ready, non-terminating IPv4/IPv6 endpoints; an empty backend returns 503. HTTPS/private CA and HTTP/2 policies are available through [Ingress annotations](examples/ingress-policies.yaml).
- Same-namespace ConfigMap plugins via `rgnix.io/script: routes/main.rgl`; scripts may select only that Ingress's declared backends.
- Durable accepted configuration in controller-namespace ConfigMaps. Invalid plugin updates retain the last accepted version while resource deletion and endpoint withdrawal still take effect.

### Gateway API

Install the pinned standard CRDs, then select Helm `mode=gateway`. Each Deployment serves one explicitly bound Gateway; HTTPRoute and GRPCRoute share the same proxy and plugin runtime. JWT/external auth, BackendTLSPolicy, Service weights, mirrors and rollout controls are available within the [documented field and policy boundaries](docs/gateway-api.md).

Accepted plugin source and route policies persist in UID-bound ConfigMap checkpoints. New replicas can restore an accepted version while current source is invalid; permissions, endpoints, Secrets and resource deletion remain live. Checkpoint writes are asynchronous. [Runnable Gateway example](examples/gateway.yaml) · [Migration assessment](docs/migration.md).

### Tenant boundaries and staged releases

Administrators can restrict watched namespaces, authorize domains, and cap configuration size, routes, requests, plugins, auth and mirrors per namespace. Named reader/writer identities constrain management access by namespace; credentials and policy files reload atomically. [Governance guide](docs/governance.md) · [Quota and change-management guide](docs/platform-policies.md).

Configure a **90/10 Service split** with an annotation:

```yaml
metadata:
  annotations:
    rgnix.io/traffic-policy: >-
      {"revision":"checkout-v2",
       "backends":[{"service":"checkout-stable:http","weight":90},
                   {"service":"checkout-canary:http","weight":10}]}
```

The [complete rollout example](examples/ingress-rollout.yaml) adds stable cohorts, mirrors, stages, approvals and error/p95 rollback. External JSON metric gates block promotion when unhealthy or unavailable; `metric_rollback` can also trigger a durable fallback after sustained failures, alongside local error/latency rollback. Progress persists across controller restarts. File diff, candidate preflight and an optional TLS admission webhook validate changes before publication.

**Request rates can be shared across replicas** with the optional [Redis coordinator](docs/shared-rate-limits.md); concurrency and resource budgets remain per process. For hard CPU/memory isolation, use separate controller Deployments/IngressClasses, scoped watches and Kubernetes resource limits.

## Logs and observability

Export access logs and spans together to an OpenTelemetry Collector or another OTLP/HTTP protobuf endpoint:

```sh
OTEL_SERVICE_NAME=rgnix-edge rgnix serve -c examples/nginx.conf \
  --otlp-logs-endpoint http://collector:4318/v1/logs \
  --otlp-traces-endpoint http://collector:4318/v1/traces \
  --trace-sample-ratio 0.1
```

OTLP supports authentication headers, HTTPS/private CAs, bounded asynchronous queues, drop metrics and shutdown flushing. W3C `traceparent` and `tracestate` continue across proxies; backend, auth and mirror calls receive distinct child spans. Local and OTLP access logs correlate with the server span. Ingress uses the same exporter and supports credentials from a Helm Secret. [Log export](docs/otlp.md) · [Tracing, propagation and sampling](docs/tracing.md).

See the [runtime tracing validation](docs/validation-tracing-2026-09-25.md) for propagation and log-correlation evidence.

Local logs can rotate by size or UTC interval with retention and gzip:

```nginx
http {
    access_log /var/log/rgnix/access.log;
    error_log /var/log/rgnix/error.log warn;
    rgnix_log_rotation size=100m interval=1d keep=7 gzip=on;
    # Add server blocks here; create a writable log directory first.
}
```

File and OTLP logging can run together. **SIGUSR1** reopens files for external logrotate. [Rotation guide](docs/log-rotation.md) · [Example](examples/logging.conf).

The separate management listener exposes `/healthz`, `/readyz` and `/metrics`. These endpoints are unauthenticated and should remain on a protected management network. Token-protected `/v1/*` APIs expose configuration, routing, simulation, release controls and history. [Operations and metrics](docs/deployment.md).

Operational metrics cover upstream latency phases and connection reuse, body traffic/inspection, live backend and tenant budgets, watch/Lease state, rollout gates, and Linux CPU/RSS/FD metrics. Helm includes a dedicated metrics Service and optional ServiceMonitor. See the [metric catalog and scrape setup](docs/metrics.md) and [Prometheus alerts](examples/prometheus-alerts.yaml).

## CLI and architecture

```text
rgnix serve -c nginx.conf [--admin 127.0.0.1:9090] [--threads 2]
rgnix check -c nginx.conf
rgnix compile routes.rgl -o routes.wasm
rgnix dump -c nginx.conf
rgnix diff -c candidate.conf --against nginx.conf
rgnix explain -c nginx.conf --host example.com --path /api
rgnix simulate -c nginx.conf --request request.json
rgnix ingress --ingress-class rgnix --publish-service namespace/service
rgnix gateway --gateway namespace/name --publish-service namespace/service
rgnix migrate nginx -c nginx.conf
rgnix migrate ingress -f ingress-and-services.yaml -o gateway.yaml
rgnix migrate compare --before old.conf --after new.conf --requests requests.json
```

`check` validates configuration, DNS, certificates and plugins without opening listeners. `compile` produces portable Wasm. **SIGHUP** reloads standalone routes, scripts, certificates and log policy; **SIGTERM** drains requests. Listener and worker-thread changes require restart.

```mermaid
flowchart LR
    Files[Configuration + RGL] --> Build[Validate and compile]
    K8s[Kubernetes watches] --> Build
    Build --> Snapshot[Immutable runtime snapshot]
    Request[HTTP request] --> Router[Host and path matching]
    Snapshot --> Router
    Router --> Policy[Auth, budgets and request hook]
    Policy --> Proxy[Pingora proxy]
    Policy --> Local[Static file or direct response]
    Proxy --> Response[Response hook and telemetry]
    Local --> Response
```

Compilation occurs on the control plane. Snapshot publication is atomic; in-flight requests retain their original version. Simulation shows the outbound plan without sending a business request or changing live counters.

## Validation

The [release workflow](https://github.com/SamuelSupe/rgnix/actions/workflows/release.yml) gates publication on native **amd64 and arm64** Rust/behavior checks, Clippy and release builds, followed by the amd64 Kubernetes Gateway/TLS/gRPC harness. See [release notes and verification](docs/releases/v0.3.0.md).

Pre-release acceptance on **OrbStack Linux arm64** recorded:

| Suite | Passed |
| :--- | ---: |
| Kubernetes Gateway / Ingress policies | **73 / 70** |
| Cross-process shared rates | **12** |
| HTTP / standalone product behaviors | **82 / 111** |
| Controller recovery / migration | **57 / 10** |
| OTLP / local log rotation | **32 / 20** |
| Rust unit and boundary tests | **12** |

The [P1 validation record](docs/validation-p1-product-2026-09-25.md) identifies the tested binaries and the order of the final Gateway guard regression. Earlier [tracing](docs/validation-tracing-2026-09-25.md), [metrics](docs/validation-metrics-2026-09-25.md) and [v0.2.0 release](docs/validation-release-0.2.0.md) records remain separate historical evidence.

**Outside acceptance scope:** upstream Gateway conformance certification, multi-node failure, cloud load balancers, long-duration load, adversarial tenant capacity, and Redis Sentinel/Cluster failover. Historical performance measurements are not a capacity guarantee.

## Scope and documentation

rgnix implements an explicit NGINX subset. It does not implement full Lua/NGINX compatibility, regex/nested locations, `rewrite/map/if`, response caching, HTTP/3, ingress-nginx annotations or automatic business-request retries. Gateway API has a separately documented [support boundary](docs/gateway-api.md). Unsupported configuration fails with diagnostics.

| Guide | Contents |
| :--- | :--- |
| [Compatibility](docs/compatibility.md) | Directives, inheritance, variables and intentional differences |
| [RGL](docs/rgl.md) · [Body routing](docs/request-body.md) | Language, host APIs, ABI, sandbox and inspection limits |
| [Traffic policies](docs/product-features.md) | Authentication, load balancing, static serving, compression and traces |
| [Governance](docs/governance.md) · [Platform policies](docs/platform-policies.md) | Domains, quotas, roles, staged releases, preflight, admission and rollback |
| [Operations](docs/deployment.md) | Helm, TLS, recovery, metrics and shutdown |
| [Gateway API](docs/gateway-api.md) · [Migration](docs/migration.md) | Supported resources, field boundaries and migration candidates |
| [Release engineering](docs/releases.md) · [Security](SECURITY.md) | Build gates, signed artifacts, versioning and reporting |
| [OTLP](docs/otlp.md) · [File logs](docs/log-rotation.md) | Export, rotation and delivery limits |
| [Validation](docs/validation-p1-product-2026-09-25.md) · [Contributing](CONTRIBUTING.md) | Evidence, reproduction and development checks |

Most detailed references and validation reports are currently in Chinese. [简体中文 README](README.zh-CN.md).

## License

[Apache License 2.0](LICENSE). The bounded Pingora modifications and upstream provenance are documented in [vendor/README.md](vendor/README.md).

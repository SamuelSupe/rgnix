# Production policies and change management

These capabilities extend the existing Pingora data plane and immutable configuration snapshots. All new policies are opt-in except query credential redaction and correct gRPC status reporting.

## Namespace governance

Run the Ingress controller with `--tenant-policy-file /etc/rgnix/tenancy/policy.json`. Only the controller administrator mounts this file; Ingress annotations cannot change it. Policy files are checked every second and atomically reloaded after validation. An omitted `default` denies namespaces without an explicit entry. Each explicit namespace entry replaces the default entry, with omitted fields filled from the built-in defaults. Domain authorization, scoped watches, named identities, staged rollouts and admission are covered in the [governance guide](governance.md).

```json
{
  "default": {
    "max_ingresses": 32,
    "max_routes": 128,
    "max_backends": 64,
    "max_body_bytes": 16777216,
    "max_timeout_seconds": 60,
    "max_script_bytes": 262144,
    "max_inflight": 64,
    "max_plugins": 4,
    "max_auth": 16,
    "max_mirrors": 4,
    "max_limiter_keys": 2048,
    "requests_per_second": 1000,
    "burst": 1000,
    "allow_scripts": true,
    "allow_mirroring": false
  },
  "namespaces": {
    "payments": { "max_inflight": 128, "max_plugins": 8, "allow_mirroring": true }
  }
}
```

Counts apply across all accepted Ingress resources in a namespace, ordered by creation time, namespace and name. Over-quota resources are not installed and receive a `NamespaceQuota` Event. Unlisted namespaces receive `NamespaceDenied` when no default is configured. Request body and timeout settings are capped, including application `client-max-body-size: "0"`; diagnostics show the effective values. Plugin permission and size ceilings also apply to retained configurations.

Each namespace has its own request, plugin, external-auth and mirror concurrency budgets, a pre-authentication aggregate token bucket, and a bounded route limiter table. Per-route policies still apply inside those budgets. Namespace exhaustion does not consume another namespace's limiter keys or concurrency allowance. Request-rate buckets can be shared using the optional [Redis coordinator](shared-rate-limits.md). Concurrency and resource budgets remain per controller process; these are containment limits, not a distributed billing ledger. Global budgets remain the final process ceiling. Namespace request/plugin budgets must be smaller than their corresponding global budgets. For stronger CPU/memory or trust isolation, use separate controller deployments and IngressClasses with Kubernetes resource limits.

Helm reads `tenancy.policyConfigMap.name` / `key` (default key `policy.json`). Do not grant application service accounts write access to this ConfigMap or the controller Deployment. Inspect quotas and active request/plugin/auth/mirror counts under `/v1/routes`; rejection counters are `rgnix_namespace_rejections_total`.

## Declarative traffic splitting, mirrors and rollback

`rgnix.io/traffic-policy` is a JSON annotation applying to that Ingress's proxy routes. All Service references are in the Ingress namespace. Named and numeric Service ports work. These references also become explicitly allowed script backends; resource watches and endpoint withdrawals cover them.

```yaml
metadata:
  annotations:
    rgnix.io/traffic-policy: >-
      {"revision":"checkout-v2",
       "backends":[{"service":"checkout-stable:http","weight":90},
                   {"service":"checkout-canary:http","weight":10}],
       "mirror":{"service":"checkout-shadow:http","percent":5,
                 "max_body_bytes":65536,"timeout_ms":500},
       "rollback":{"fallback":"checkout-stable:http","min_requests":100,
                   "error_percent":5,"window_seconds":60,"max_p95_ms":500}}
```

Weights are relative and selected per process; an optional `cohort` key makes assignment stable across replicas. A failed chosen backend returns its error; the business request is never automatically replayed. A script's explicit `route.proxy` takes precedence over ordinary weighted selection, but a latched rollback overrides that script selection. Existing endpoint load balancing remains responsible for choosing a replica within the selected Service.

Mirroring is opt-in and sends a second real request to the configured shadow Service, including the final request headers and body. Use a shadow Service designed to accept these copies. Primary responses do not wait for mirrors. Mirror response contents are discarded. The request body is copied while the primary request streams; the mirror is sent only when the complete body is within the configured cap. Oversized bodies are **skipped**, never sent as truncated requests. WebSocket and gRPC streams are skipped. A process has at most 16 outstanding mirrors, with additional namespace limits; timeout, size and busy-budget outcomes appear in `rgnix_mirror_requests_total`. Mirrors include `x-rgnix-mirror: true` and do not count towards rollout health.

Rollback evaluates completed **non-fallback target** requests: upstream transport errors, HTTP 5xx and nonzero/missing gRPC status are failures. Client cancellations are excluded. The controller retains at most 10,000 samples within the specified time window. At `min_requests`, an error ratio at or above `error_percent`, or p95 above `max_p95_ms`, latches the revision to its fallback. Thresholds are evaluated locally; a replica that observes the threshold immediately switches new requests and writes `rgnix.io/rolled-back-revision` back to the Ingress. Other replicas converge through their watch, and new replicas restore that latch. Kubernetes API outages delay propagation to other replicas; they do not undo the local latch. The chart grants Ingress patch permission for this operation. Diagnostics show the latch and sample count; `TrafficRolledBack` Events and `rgnix_traffic_rollbacks_total` record rollback.

Use a **new `revision`** to re-arm a rollout after fixing the candidate. Do not reuse an old revision for a new release. Changing endpoint membership does not reset samples or reactivate a failed revision. No Gateway API or CRD is required.

## Management roles, simulation and durable history

- `--admin-token-file`: write role, including rollback; compatible with the previous admin token option.
- `--admin-read-token-file`: read diagnostics and run isolated simulations; rollback returns 403.
- `--admin-users-file`: JSON named identities with `reader`/`writer` roles, optional namespace scopes and SHA-256 bearer-token digests. Omitted scopes grant global access; an empty list grants none. Credentials and tenant policy files reload automatically; invalid replacements retain the last valid bundle.
- `--admin-audit-file`: append JSON audit records (0600 on creation). Defaults to stderr. Records contain the role, operation, version, result and timestamp, without tokens or request bodies. An unavailable audit sink rejects management actions before execution. File rotation can use external `copytruncate`; keep audit files in administrator-controlled storage.
- Helm supports `admin.tokenSecret`, `admin.readTokenSecret` and `admin.usersSecret`, each with `name` and `key`.

`POST /v1/simulate` accepts the same JSON as `rgnix simulate -c nginx.conf --request request.json`. It evaluates trusted client IP, CIDR rules, client certificate chain validation, JWT verification, auth identity headers, pre/post-auth rate and concurrency budgets, request body limits and RGL. It never consumes live data-plane limiter buckets or calls the business upstream. `repeat` (1–1000) runs a batch; `hold_permits: true` models concurrent requests held for that batch. Use `client_certificate` for a PEM certificate chain fixture; this checks trust, not TLS possession of a private key.

External auth requires an explicit fixture, for example `"external_auth":{"status":200,"headers":{"x-user":"alice"}}`. Without a fixture it reports status 424 rather than assuming success. The CLI's `--live-auth` explicitly invokes the configured authentication service. The management endpoint always uses fixtures. At most two management simulations run concurrently.

Simulation also reports the final backend, URI, redacted forwarding headers, rollout progress and mirror eligibility without changing live selection state. `rgnix diff`, `/v1/validate` and `/v1/validate-ingress` compile isolated candidate configurations. An optional TLS admission webhook applies the same Ingress validation before Kubernetes persistence. Staged rollout weights, approvals, pause/resume and administrator-defined external JSON metric gates are documented in the [governance guide](governance.md).

For standalone mode, enable durable history:

```sh
rgnix serve -c /etc/rgnix/nginx.conf \
  --history-dir /var/lib/rgnix/history \
  --admin-token-file /run/secrets/admin-write \
  --admin-read-token-file /run/secrets/admin-read \
  --admin-audit-file /var/log/rgnix/audit.jsonl
```

The journal stores the active version plus up to eight previous versions, including expanded configuration, local JWKS, certificates/private keys and plugin bytes. The directory is 0700, the journal is 0600, writes use atomic replacement and fsync, and a process lock prevents two instances from sharing it. Static website contents and remote JWKS are not archived. Each configuration bundle is limited to 4 MiB of configuration and 16 MiB of local assets. The retained set is capped at 8 MiB of configuration and 32 MiB of decoded assets; older versions are evicted when either budget is reached. Assets use compact base64 in the journal. Protect and back up this directory as secret material.

`POST /v1/rollback/VERSION` validates the retained version and publishes a new monotonically increasing version. `/v1/history` includes `durable_versions`. On restart, the most recently committed version is restored from the journal; `SIGHUP` explicitly imports the current source files. A rollback does not rewrite administrator source files. Ingress rollback continues to use Kubernetes source resources, preserving endpoint and Secret withdrawals.

## Reliability and privacy fixes

- Standalone active HTTPS probes use the same SNI, CA and client identity as their route. Different TLS policies get separate health state and connection pools.
- `proxy_ssl_certificate` and `proxy_ssl_certificate_key` enable upstream mTLS. Ingress uses `rgnix.io/upstream-client-secret` (`tls.crt`, `tls.key`); rotation replaces identity and deletion disables the route. Server certificate and hostname checks remain mandatory.
- Ingress external auth balances its ready EndpointSlice addresses, briefly excludes failed addresses, and can try up to three addresses within the single configured auth timeout. Only authentication GETs may fail over; business requests still have no automatic retry.
- IP/route limits are enforced before JWT and external auth. Header/cookie/claim limits run after authentication, so trusted auth identity headers remain usable. Namespace aggregate limits precede both.
- gRPC metrics use `rgnix_grpc_requests_total{route,grpc_status}` with bounded configured-route labels. Nonzero or missing gRPC status marks both server and client OTLP spans as errors, including trailers-only responses. HTTP status metrics remain unchanged.
- Local access logs redact common query credentials by default, including percent-encoded keys, in both URI and Referer. `access_log PATH json;` enables JSON. `rgnix_log_query on|off;`, `rgnix_log_client on|off;`, `rgnix_log_referer on|off;`, `rgnix_log_redact KEY...;`, and `rgnix_log_fields FIELD...;` provide field control. `log-format`, `log-query`, `log-client`, `log-referer`, `log-redact` and `log-fields` annotations provide the same controls (comma-separated lists). Client suppression also applies to OTLP logs. OTLP continues to omit query strings, credentials and bodies. Redaction is a configured key policy, not automatic discovery of arbitrary secrets in paths or values.

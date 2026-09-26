# Gateway API

Gateway support is included in **v0.3.0 Preview**, targeting the **v1.6.1 standard CRDs**. The upstream conformance suite has not been certified; the implementation must not be advertised as a conformant Gateway API implementation.

**v0.4.0 Preview** adds standard RequestMirror, HTTPRoute timeouts, Gateway preflight/admission and optional replica publication reporting, with scoped admission calls, certificate reload and configurable draining.

## Deployment model

Each rgnix deployment serves one explicitly bound `namespace/name` Gateway. Helm provisions its Deployment and Service; the controller does not dynamically create infrastructure for arbitrary Gateway objects. Replicas independently watch resources and publish immutable snapshots. A Lease selects the status writer.

```sh
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.1/standard-install.yaml
docker build -t YOUR_REGISTRY/rgnix:gateway-preview .
docker push YOUR_REGISTRY/rgnix:gateway-preview
helm upgrade --install edge charts/rgnix -n edge --create-namespace \
  --set mode=gateway --set gateway.className=edge \
  --set image.repository=YOUR_REGISTRY/rgnix --set image.tag=gateway-preview
```

The chart creates an `edge/edge` Gateway with an HTTP listener on port 80. Apply an HTTPRoute in `edge` referencing that Gateway and an existing Service, or configure `gateway.listeners[].allowedRoutes` to authorize application namespaces. Set `gateway.createClass=false` to reuse a GatewayClass managed outside this Helm release. Existing Ingress deployments keep `mode=ingress` and need no Gateway CRDs.

The equivalent process command is:

```sh
rgnix gateway --gateway edge/edge --publish-service edge/edge \
  --http-listen 0.0.0.0:8080 --https-listen 0.0.0.0:8443 \
  --http-port 80 --https-port 443 --watch-namespace edge --watch-namespace app
```

External listener ports must match `--http-port` / `--https-port`; socket addresses are fixed for the process lifetime. The chart derives the external ports from `service.httpPort` and `service.httpsPort`. When changing them, also update `gateway.listeners`.

## Supported contract

| Resource / behavior | Current implementation |
| --- | --- |
| GatewayClass | `controllerName: rgnix.io/gateway-controller`; bound classes receive Accepted status; parametersRef is rejected |
| Gateway | HTTP and terminating HTTPS; multiple hostnames on the two configured ports; hostname-specific listener isolation |
| Listener authorization | Same, All and namespace label Selector; allowed route kinds; parentRef sectionName and port |
| HTTPRoute | Exact and segment PathPrefix, method, exact header/query matches; conditions within a match are AND, matches are OR |
| Route precedence | Specific hostname, exact/longer path, method, header/query specificity, creation time, namespace/name and rule order |
| GRPCRoute | Exact service/method and header matching; HTTP/2 and h2c, unary and bidirectional streams, trailers |
| Backend references | Numeric Service ports, ready/non-terminating IPv4/IPv6 EndpointSlice addresses; Service UID ownership checks |
| Traffic split | Relative Service weights independent of each Service's endpoint count; zero weight receives no traffic |
| HTTP filters | RequestHeaderModifier, ResponseHeaderModifier, RequestRedirect, URLRewrite and one RequestMirror; validated literal header values, full/prefix path replacement |
| Timeouts | HTTPRoute request and backendRequest transaction deadlines, including continuously streaming responses; zero disables that deadline |
| Change validation | Isolated Gateway/HTTPRoute/GRPCRoute preflight and optional TLS validating admission webhook |
| Publication | Optional per-Pod Lease reports and expected-digest/minimum-replica CLI gate |
| Cross-namespace references | ReferenceGrant for Service and TLS Secret references, reevaluated on deletion or modification |
| TLS | One Secret per HTTPS listener, multiple SNI certificates across listeners; certificate/key validation and live replacement/revocation |
| Backend TLS | BackendTLSPolicy with Service / named-port targets, hostname verification, same-namespace CA ConfigMap/Secret or System trust |
| Route policy | Existing JWT, external auth, rate/concurrency, upstream and logging annotations; declarative split, mirror, staged approval and rollback |
| Status | GatewayClass Accepted; Gateway/listener Accepted, Programmed, ResolvedRefs; route parent Accepted and ResolvedRefs; observedGeneration and stable transition timestamps |
| RGL | `rgnix.io/script: configmap/main.rgl`, request-body inspection annotations, max-body annotation; aliases `service:port` and `namespace/service:port` for that rule's authorized backends |
| Governance | Namespace watch restrictions, administrator quota/domain policies, request/plugin permits, body/time limits and existing global budgets |

Gateway wildcards can cover multiple DNS labels. An exact hostname listener with no matching route does not fall back to a broader listener. The upstream Host is preserved unless URLRewrite changes it. Request header operations run before plugin header edits; trace propagation remains owned by the existing tracing layer. Framing, hop-by-hop and Host header modifier operations are rejected; use URLRewrite for hostname changes.

Invalid or unauthorized backend references retain their configured weight and return 500 for that share of traffic. A valid Service with no ready endpoints returns 503. There is no automatic request retry. Redirects retain query strings; prefix replacement respects path segment boundaries.

Accepted plugin source, route specification and policy annotations are checkpointed asynchronously into controller-namespace ConfigMaps. Fresh replicas can rebuild the last accepted source when the live plugin fails compilation. Checkpoints bind the Gateway UID, route kind/UID and source ConfigMap UID; a missing or recreated source cannot inherit deleted code. Current quotas, domain/parent authorization, ReferenceGrants, Secrets and endpoints are always rechecked. An unavailable authentication dependency keeps its authorized route match as a 500 response, preventing fallback to a broader public route. Route/source deletion removes stale checkpoints. Monitor `rgnix_checkpoint_healthy` / `rgnix_checkpoint_errors_total` and permit controller-namespace ConfigMap CRUD; persistence is asynchronous and a change not yet checkpointed is not crash-durable. Back up this namespace along with the Gateway resources.

Administrator `max_ingresses` counts HTTPRoute/GRPCRoute objects in this mode; `max_routes` counts their expanded host/listener/match entries. `max_backends` counts distinct Service-port references. Explicit administrator domain grants also apply. Gateway allowedRoutes controls cross-namespace attachment; it is not inferred from an IngressClass.

## Standard mirror and timeouts

```yaml
rules:
  - matches:
      - path: {type: PathPrefix, value: /checkout}
    backendRefs:
      - {name: checkout, port: 80}
    timeouts:
      request: 5s
      backendRequest: 2s
    filters:
      - type: RequestMirror
        requestMirror:
          backendRef: {name: checkout-shadow, port: 80}
          percent: 10
```

Use `fraction: {numerator: 1, denominator: 10}` instead of `percent` for a rational sample; the default denominator is 100. Omitting both mirrors every eligible request; zero mirrors none. One mirror is supported per rule. The reference uses the same Service, ReferenceGrant and BackendTLSPolicy validation as primary traffic, consumes namespace backend quota, and requires `allow_mirroring` when an administrator tenant policy is configured. An invalid reference sets `ResolvedRefs=False` and disables the mirror while preserving valid primary traffic. It does not authorize RGL to select that Service as a primary backend.

The mirror preserves the forwarded path, Host, edited headers and complete request body. It is asynchronous, has a 64 KiB body cap and a 500 ms deadline, and shares the global/namespace mirror permits. Oversized bodies, WebSocket and gRPC are skipped and counted. It cannot be combined with an annotation-based traffic-policy mirror. Mirror failure never retries or fails the primary request. These bounds mean arbitrary large streaming requests are not guaranteed to be mirrored.

`request` starts after request headers have been read, before authentication, body inspection and request plugins. `backendRequest` starts when dispatching to the selected upstream, including connection establishment, request upload and response streaming. The earlier deadline wins. The deadline is absolute: a trickling response does not reset it. Expiry before response headers produces 504; after headers have been sent it terminates the stream. No business request is replayed. This is separate from idle connect/read/write timeouts and does not bound reading the initial client headers.

Durations use Gateway API units `h`, `m`, `s`, `ms`. `backendRequest` cannot exceed a finite `request`; explicitly disabling backendRequest while request is finite is rejected. `0s` disables that absolute deadline; independent I/O and administrator limits remain. Omitted deadlines are unset. Administrator `max_timeout_seconds` caps configured finite deadlines as well as existing I/O limits; it does not create an absolute deadline when none is configured. Long-lived SSE/WebSocket routes should choose budgets deliberately. GRPCRoute `timeouts` are rejected because this field is not part of its supported schema.

## Gateway preflight and admission

`POST /v1/validate-gateway` accepts a complete v1 Gateway, HTTPRoute or GRPCRoute object and returns `valid`, `errors`, `warnings`, `base_version` and `diff`. It runs the controller compiler against an isolated copy of the current cached Services, grants, Secrets, scripts and policies. It never publishes or writes Kubernetes resources. A bad candidate plugin cannot hide behind last-good recovery. A namespace reader/writer may preview routes in its scope; previewing Gateway changes requires a global writer because listeners affect other namespaces.

```sh
curl --fail-with-body -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' --data-binary @route.json \
  http://127.0.0.1:9090/v1/validate-gateway
```

Enable Helm `admission.enabled` with `tlsSecret` and `caBundle` as for Ingress. In Gateway mode the chart registers `/admission/gateway` for Gateway, HTTPRoute and GRPCRoute CREATE/UPDATE operations in the watch namespaces and the controller namespace. Kubernetes CEL matchConditions filter both new and old ownership before contacting the webhook, so outages do not block unrelated Gateways or routes. Admission certificate/key files reload atomically without restarting Pods; rotate CA trust with overlapping old/new bundles as described in [governance](governance.md). Controller-owned rollout annotations remain protected. DELETE remains effective. Preflight is a point-in-time validation, not a reservation or atomic multi-resource transaction. Monitor both admission and controller outcomes when dependencies change after validation.

Bootstrap first enablement in two phases: start the TLS server with `admission.register=false`, wait for the Deployment, then register the webhook. Otherwise Helm can register a fail-closed webhook before its Service is ready and block creation/update of its own Gateway. Put the certificate settings and `admission.enabled: true` in `admission-values.yaml`:

```sh
helm upgrade --install edge charts/rgnix -n edge --create-namespace \
  -f gateway-values.yaml -f admission-values.yaml \
  --set admission.register=false --wait --timeout 180s
helm upgrade edge charts/rgnix -n edge --reuse-values \
  --set admission.register=true --wait --timeout 180s
```

`register` defaults to true for compatibility. Setting it false on an existing registered installation removes the webhook; this switch is for initial bootstrap, not routine certificate rotation. The two-phase flow never uses `failurePolicy: Ignore`. Helm continues to own the webhook so uninstall removes it normally.

See [replica publication](publication.md) for `GET /v1/fleet`, `rgnix wait` and failure states, and [production acceptance](production-readiness.md) for executed scope and repeatable gates.

## BackendTLSPolicy

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata:
  name: checkout-tls
  namespace: app
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: checkout
      sectionName: https
  validation:
    hostname: checkout.internal.example.com
    caCertificateRefs:
      - group: ""
        kind: ConfigMap
        name: checkout-ca
```

The CA object contains `ca.crt`. One Service target per policy is supported; omit sectionName for all its ports. Use `wellKnownCACertificates: System` instead of CA refs for public/system trust. Cross-namespace CA refs, subjectAltNames overrides and implementation-specific options are rejected. At most eight CA refs and a 1 MiB combined bundle are accepted. An invalid attached policy fails closed; an older policy wins conflicts for the same target, and a port-specific policy takes precedence over a Service-wide policy. TLS settings override route CA/SNI annotations for that backend; route client identity and protocol selection remain available. CA/SNI changes separate connection pools. Removing a policy restores the route's declared transport, so retain `appProtocol: https` when plaintext must never be allowed.

## Boundaries

No Gateway infrastructure provisioning, addresses override, parametersRef, ListenerSet, TCP/UDP/TLSRoute, frontend TLS client-certificate validation, HTTPRoute retries/session persistence, regular-expression matches, backend-level filters, multiple RequestMirror filters or ExtensionRef. Gateway backends use cleartext HTTP/1.1 by default; GRPCRoute and Services declaring `appProtocol: kubernetes.io/h2c` use h2c. A Service declaring `appProtocol: https` requires a usable TLS policy or explicit HTTPS backend annotation; it cannot silently fall back to plaintext. Unsupported fields and filters receive status diagnostics.

The same [application annotations](product-features.md#ingress-应用注解) now apply to HTTPRoute and GRPCRoute, except frontend client-certificate annotations (rejected explicitly). JWT and external authentication resolve same-namespace dependencies; secret/endpoint withdrawal remains effective during plugin recovery. `auth-service` uses the existing HTTP auth-Service contract. With `traffic-policy`, every target and mirror must appear as a same-namespace numeric-port Service backendRef in every affected rule. A zero-weight ref may declare a mirror-only Service; traffic-policy weights govern the primary split. Missing or unauthorized rollout references fail the rule closed. Mirroring uses the existing bounded HTTP mirror implementation; gRPC/WebSocket and oversized streaming bodies are skipped, with counters. Standard RequestMirror uses the same bounded transport; it does not require a duplicate primary backendRef.

Rollout progress and rollback revision persist on the owning Route. `POST /v1/rollouts` accepts an optional `kind: HTTPRoute|GRPCRoute|Ingress` to disambiguate namespace/name; existing unambiguous commands remain compatible. Reader/writer roles, namespace scope, auditing, approval, pause/resume, stable cohorts and [external metric rollback](governance.md#外部指标门禁) apply. Shared request rates and namespace quotas are available through the optional [Redis coordinator](shared-rate-limits.md). Gateway mode has its own preflight endpoint and webhook described above. Authenticated request simulation uses the active Gateway match and forwarding rules, with deterministic weight samples.

Controller watch/cache/Lease metrics retain their existing `rgnix_ingress_*` names for compatibility; the `kind` label identifies Gateway resources. Configuration diagnostics and update duration also cover Gateway mode. `rgnix_ingress_selected_resources` specifically describes Ingress selection.

## Reproduce behavioral validation

```sh
docker build -t rgnix:gateway-qa .
docker build -f scripts/fixtures/gateway-grpc.Dockerfile -t rgnix:gateway-grpc-fixture .
python3 scripts/gateway_e2e.py --context orbstack \
  --namespace rgnix-gateway-qa --image rgnix:gateway-qa \
  --output gateway-results.json
```

The test owns the explicitly named namespace and a `-peer` namespace, and retains them for inspection. Use dedicated names. Release CI runs the same behavioral checks on Kind before distribution. This harness is not the upstream Gateway API conformance suite.

See the [current-source validation record](validation-gateway-hardening-2026-09-26.md) for the final artifact's complete 115-check run, persistent-connection traffic during Pod replacement, gRPC drain and admission rotation. The [earlier product record](validation-product-2026-09-26.md) and [2026-09-25 record](validation-gateway-migration-release-2026-09-25.md) retain their original evidence and boundaries separately.

Specification: [Gateway API v1.6.1](https://github.com/kubernetes-sigs/gateway-api/releases/tag/v1.6.1), [implementer guidance](https://gateway-api.sigs.k8s.io/guides/implementers-guide/).

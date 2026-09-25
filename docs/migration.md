# Migration tools

All commands are offline preparation commands. They do not apply Kubernetes resources, replace a running server or prove traffic equivalence with an existing NGINX deployment. Output candidates are written only when there are no blocking findings; existing output files are never overwritten. JSON reports go to stdout, failures return a nonzero exit code.

## NGINX assessment and candidate

```sh
rgnix migrate nginx -c /etc/nginx/nginx.conf > report.json
rgnix migrate nginx -c /etc/nginx/nginx.conf \
  --accept-process-differences -o /tmp/rgnix.conf > report.json
rgnix check -c /tmp/rgnix.conf
```

The scanner expands includes, inventories directives and records source locations. It reports unsupported directives together, rather than stopping at the first unknown name. The final candidate is validated by the actual rgnix parser/compiler. Unsupported argument forms can still be reported by that final validation.

`--accept-process-differences` explicitly allows dropping NGINX process/event directives such as user, worker_processes and worker_connections. Set rgnix threads, container limits and OS file-descriptor limits separately. The report continues to list each dropped directive. It does not discard unsupported routing, security or caching directives.

The candidate expands includes, rewrites file paths to the original main configuration directory and makes the original default `html` root explicit. It remains dependent on those files. This is not a self-contained deployment bundle. Review proxy_pass URI replacement, header inheritance and file behavior against [the compatibility matrix](compatibility.md).

## Standard Ingress to Gateway API

```sh
kubectl -n app get ingress,service -o yaml > ingress-and-services.yaml
rgnix migrate ingress -f ingress-and-services.yaml \
  --gateway-class edge --gateway-name edge --namespace app \
  -o gateway-candidate.yaml > migration-report.json
```

Input accepts multi-document YAML, JSON objects and Kubernetes Lists. Named Service ports require matching Service manifests in the input. Numeric ports do not require access to the cluster. The output contains a GatewayClass, one Gateway per input namespace and HTTPRoutes; referenced Services, Secrets and plugin ConfigMaps remain external dependencies.

Unknown annotations block conversion, including ingress-nginx authentication/rewrite annotations: their behavior must not disappear silently. Ingress wildcard hosts block conversion because Gateway wildcards can authorize deeper subdomains. ImplementationSpecific becomes rgnix's segment-prefix interpretation and produces a review finding. A legacy ingress.class annotation is removed because the target Gateway binding replaces class selection.

This first converter does not integrate ingress2gateway or implement its provider-specific annotation translations. It produces conservative candidates for rgnix's supported scope. Conflicting source routes and overlapping default/hostless fallback rules block conversion because creation order and fallback priority cannot be copied reliably to new resources. Generated route names include a source-identity digest to prevent truncation collisions.

Deploy a bound Gateway data plane for every generated Gateway. Do not direct traffic to it until application Services, TLS and authorization dependencies are ready. See [Gateway deployment](gateway-api.md).

## Request sample comparison

```json
[
  {"host":"app.example.test","path":"/api/items?q=1"},
  {"method":"POST","host":"app.example.test","path":"/api/items","headers":{"content-type":"application/json"},"body":"{\"tenant\":\"canary\"}"}
]
```

```sh
rgnix migrate compare --before old-rgnix.conf --after candidate.conf \
  --requests requests.json > comparison.json
```

Both files must be valid rgnix configurations. Comparison uses the existing authentication, rate admission and RGL simulator, including authentication fixtures. It compares normalized results and excludes generated route identifiers. Private outbound header values remain redacted and therefore are not suitable for proving equality of those values. Upstream response behavior, timing, live external authentication and real NGINX semantics require runtime differential tests. Request fixture files are limited to 4 MiB and 1,000 cases; individual simulation limits still apply.

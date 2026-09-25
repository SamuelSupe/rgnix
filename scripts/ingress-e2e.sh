#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ns="${1:?usage: ingress-e2e.sh dedicated-namespace [context]}"
context="${2:-orbstack}"
k=(kubectl --context "$context" -n "$ns")
release=rgnix-qa
class="$ns"
work="$(mktemp -d /tmp/rgnix-ingress.XXXXXX)"
forward_pid=""
cleanup() {
  if [[ -n "$forward_pid" ]]; then kill "$forward_pid" 2>/dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT
if kubectl --context "$context" get namespace "$ns" >/dev/null 2>&1; then
  [[ "$(kubectl --context "$context" get namespace "$ns" -o jsonpath='{.metadata.labels.rgnix-qa}')" == true ]] || { echo "Namespace is not owned by this acceptance script" >&2; exit 1; }
else
  kubectl --context "$context" create namespace "$ns"
  kubectl --context "$context" label namespace "$ns" rgnix-qa=true
fi
helm upgrade --install "$release" charts/rgnix --kube-context "$context" -n "$ns" \
  --set ingressClass="$class" --set service.type=LoadBalancer --set image.pullPolicy=Never \
  --set service.loadBalancerClass=rgnix.io/acceptance --set service.allocateLoadBalancerNodePorts=false \
  --set image.tag="${RGNIX_IMAGE_TAG:-0.2.0}" --wait --timeout 180s
"${k[@]}" delete ingress fallback wildcard conflicting --ignore-not-found
for color in blue green; do
  cat <<EOF | "${k[@]}" apply -f -
apiVersion: v1
kind: ConfigMap
metadata:
  name: $color
data:
  default.conf: |
    server { listen 8080; location / { return 200 "$color"; } }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $color
spec:
  replicas: 1
  selector:
    matchLabels: {app: $color}
  template:
    metadata:
      labels: {app: $color}
    spec:
      containers:
        - name: nginx
          image: nginx:1.28.0-bookworm
          imagePullPolicy: IfNotPresent
          ports: [{name: http, containerPort: 8080}]
          readinessProbe:
            httpGet: {path: /, port: http}
          volumeMounts: [{name: config, mountPath: /etc/nginx/conf.d}]
          resources:
            requests: {cpu: 10m, memory: 16Mi}
            limits: {cpu: 200m, memory: 64Mi}
      volumes:
        - name: config
          configMap: {name: $color}
---
apiVersion: v1
kind: Service
metadata:
  name: $color
spec:
  selector: {app: $color}
  ports: [{name: http, port: 80, targetPort: http}]
EOF
done
"${k[@]}" rollout status deployment/blue --timeout=120s
"${k[@]}" rollout status deployment/green --timeout=120s
plugin() {
  cat <<'EOF' > "$work/main.rgl"
function on_request()
    if req.header("x-canary") == "1" then return route.proxy("green:http") end
    return route.pass()
end
function on_response()
    resp.set_header("x-plugin", "active")
end
EOF
  "${k[@]}" create configmap routes --from-file=main.rgl="$work/main.rgl" --dry-run=client -o yaml | "${k[@]}" apply -f -
}
certificate() {
  local serial="$1"
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=qa.example.test \
    -addext subjectAltName=DNS:qa.example.test -set_serial "$serial" \
    -keyout "$work/key.pem" -out "$work/cert.pem" >/dev/null 2>&1
  "${k[@]}" create secret tls qa-tls --cert="$work/cert.pem" --key="$work/key.pem" --dry-run=client -o yaml | "${k[@]}" apply -f -
}
plugin
certificate 1
cat <<EOF > "$work/ingresses.yaml"
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: app
  annotations:
    rgnix.io/script: routes/main.rgl
spec:
  ingressClassName: $class
  tls:
    - hosts: [qa.example.test]
      secretName: qa-tls
  rules:
    - host: qa.example.test
      http:
        paths:
          - path: /api
            pathType: Prefix
            backend: {service: {name: blue, port: {name: http}}}
          - path: /exact
            pathType: Exact
            backend: {service: {name: blue, port: {number: 80}}}
          - path: /canary
            pathType: Prefix
            backend: {service: {name: green, port: {name: http}}}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: ignored
spec:
  ingressClassName: a-different-controller
  rules:
    - host: ignored.example.test
      http:
        paths:
          - path: /
            pathType: Prefix
            backend: {service: {name: blue, port: {name: http}}}
EOF
"${k[@]}" apply -f "$work/ingresses.yaml"
start_forward() {
  if [[ -n "$forward_pid" ]]; then kill "$forward_pid" 2>/dev/null || true; fi
  "${k[@]}" port-forward --address 127.0.0.1 deployment/"$release" :8080 :8443 :9090 > "$work/forward.log" 2>&1 &
  forward_pid=$!
  for _ in $(seq 1 100); do
    http_forward="$(awk '$1=="Forwarding" && $5==8080 {split($3,a,":"); print a[2]}' "$work/forward.log")"
    https_forward="$(awk '$1=="Forwarding" && $5==8443 {split($3,a,":"); print a[2]}' "$work/forward.log")"
    admin_forward="$(awk '$1=="Forwarding" && $5==9090 {split($3,a,":"); print a[2]}' "$work/forward.log")"
    if [[ -n "$http_forward" && -n "$https_forward" && -n "$admin_forward" ]] && curl --noproxy '*' -fsS "http://127.0.0.1:${admin_forward}/readyz" >/dev/null 2>&1; then return; fi
    sleep .1
  done
  cat "$work/forward.log" >&2; return 1
}
expect() {
  local label="$1" expected="$2"; shift 2
  local actual=""
  for _ in $(seq 1 120); do
    actual="$(curl --noproxy '*' -sS --max-time 2 "$@" -H 'Host: qa.example.test' 2>/dev/null || true)"
    if [[ "$actual" == "$expected" ]]; then printf 'PASS %s\n' "$label"; return; fi
    sleep .25
  done
  printf 'FAIL %s: expected %s; got %s\n' "$label" "$expected" "$actual" >&2; return 1
}
start_forward
cat <<'EOF' > "$work/main.rgl"
function on_request()
    if req.json_string("/tenant") == "vip" or req.body_contains("route=vip;") then
        return route.proxy("green:http")
    end
    return route.pass()
end
EOF
"${k[@]}" create configmap routes --from-file=main.rgl="$work/main.rgl" --dry-run=client -o yaml | "${k[@]}" apply -f -
"${k[@]}" annotate ingress app rgnix.io/request-body='full 32k' rgnix.io/request-body-timeout=2s --overwrite
expect 'JSON body selects a declared Service' green --data-binary '{"tenant":"vip","padding":"before-prefix-update"}' http://127.0.0.1:${http_forward}/api
expect 'JSON body uses the default Service for other values' blue --data-binary '{"tenant":"standard"}' http://127.0.0.1:${http_forward}/api
"${k[@]}" annotate ingress app rgnix.io/request-body='prefix 16' --overwrite
# Observe the new policy before sending a body that the previous full policy rejects.
expect 'Bytes after the prefix cannot select a Service' blue --data-binary '0123456789abcdefroute=vip;' http://127.0.0.1:${http_forward}/api
{ printf 'route=vip;'; head -c 200000 /dev/zero; } > "$work/upload.bin"
expect 'Prefix annotation routes a large POST body' green --data-binary "@$work/upload.bin" http://127.0.0.1:${http_forward}/api
"${k[@]}" annotate ingress app rgnix.io/request-body='prefix 0' --overwrite
for _ in $(seq 1 100); do
  "${k[@]}" get events --field-selector involvedObject.name=app,reason=InvalidPlugin -o jsonpath='{range .items[*]}{.message}{"\n"}{end}' | grep -q 'request body inspection size' && break
  sleep .1
done
"${k[@]}" get events --field-selector involvedObject.name=app,reason=InvalidPlugin -o jsonpath='{range .items[*]}{.message}{"\n"}{end}' | grep -q 'request body inspection size'
expect 'Rejected body policy retains accepted prefix routing' green --data-binary "@$work/upload.bin" http://127.0.0.1:${http_forward}/api
plugin
"${k[@]}" annotate ingress app rgnix.io/request-body- rgnix.io/request-body-timeout-
expect 'Prefix path and named Service port' blue http://127.0.0.1:${http_forward}/api/v1
expect 'Prefix segment boundary' 404 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/apix
expect 'Exact path and numeric Service port' blue http://127.0.0.1:${http_forward}/exact
expect 'Exact path boundary' 404 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/exact/child
expect 'IngressClass isolation' 404 -H 'Host: ignored.example.test' -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/
cat <<EOF | "${k[@]}" apply -f -
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: fallback}
spec:
  ingressClassName: $class
  defaultBackend: {service: {name: green, port: {name: http}}}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: wildcard}
spec:
  ingressClassName: $class
  rules:
    - host: '*.wild.test'
      http:
        paths:
          - path: /
            pathType: Prefix
            backend: {service: {name: blue, port: {name: http}}}
EOF
expect 'Default backend handles unmatched paths on a known host' green http://127.0.0.1:${http_forward}/unmatched
expect 'Single-label wildcard host' blue -H 'Host: a.wild.test' http://127.0.0.1:${http_forward}/
expect 'Wildcard does not match multiple labels' green -H 'Host: a.b.wild.test' http://127.0.0.1:${http_forward}/
"${k[@]}" patch ingress fallback --type merge -p '{"spec":{"rules":[{"http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"blue","port":{"name":"http"}}}}]}}]}}'
expect 'Hostless root rule overrides its Ingress default backend' blue -H 'Host: unmatched.test' http://127.0.0.1:${http_forward}/child
"${k[@]}" patch ingress fallback --type merge -p '{"spec":{"rules":null}}'
expect 'Default backend resumes after its explicit rule is removed' green -H 'Host: unmatched.test' http://127.0.0.1:${http_forward}/child
cat <<EOF | "${k[@]}" apply -f -
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: catchall}
spec:
  ingressClassName: $class
  rules:
    - http:
        paths:
          - path: /
            pathType: Prefix
            backend: {service: {name: blue, port: {name: http}}}
EOF
expect 'Hostless rule overrides an earlier default-only Ingress' blue -H 'Host: unmatched.test' http://127.0.0.1:${http_forward}/child
expect 'Named host route wins over a hostless catchall' green http://127.0.0.1:${http_forward}/canary
"${k[@]}" delete ingress fallback wildcard catchall
cat <<EOF | "${k[@]}" apply -f -
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: conflicting}
spec:
  ingressClassName: $class
  rules:
    - host: qa.example.test
      http:
        paths:
          - path: /api
            pathType: Prefix
            backend: {service: {name: green, port: {name: http}}}
EOF
expect 'Earlier Ingress wins conflicting route' blue http://127.0.0.1:${http_forward}/api
for _ in $(seq 1 100); do
  [[ -n "$("${k[@]}" get events --field-selector involvedObject.name=conflicting,reason=RouteConflict -o name)" ]] && break
  sleep .2
done
[[ -n "$("${k[@]}" get events --field-selector involvedObject.name=conflicting,reason=RouteConflict -o name)" ]]
echo 'PASS route conflict publishes Kubernetes Event'
"${k[@]}" delete ingress conflicting
expect 'Compiled plugin selects declared canary' green -H 'x-canary: 1' http://127.0.0.1:${http_forward}/api
expect 'TLS Secret serves SNI host' blue -k --resolve qa.example.test:${https_forward}:127.0.0.1 https://qa.example.test:${https_forward}/api
serial() { openssl s_client -connect 127.0.0.1:${https_forward} -servername qa.example.test </dev/null 2>/dev/null | openssl x509 -noout -serial 2>/dev/null; }
[[ "$(serial)" == serial=01 ]]
certificate 2
for _ in $(seq 1 100); do [[ "$(serial)" == serial=02 ]] && break; sleep .2; done
[[ "$(serial)" == serial=02 ]]
echo 'PASS TLS certificate rotation'
for _ in $(seq 1 100); do
  checkpoint_count="$("${k[@]}" get configmaps -l rgnix.io/checkpoint-class -o name | wc -l | tr -d ' ')"
  [[ "$checkpoint_count" -gt 0 ]] && break
  sleep .2
done
[[ "$checkpoint_count" -gt 0 ]]
echo 'PASS accepted plugin checkpoint persisted'
"${k[@]}" patch configmap routes --type merge -p '{"data":{"main.rgl":"invalid source"}}'
expect 'Invalid plugin retains last compiled module' green -H 'x-canary: 1' http://127.0.0.1:${http_forward}/api
"${k[@]}" patch ingress app --type json -p '[{"op":"replace","path":"/spec/rules/0/http/paths/0/path","value":"/changed"}]'
sleep 1
expect 'Invalid plugin retains last accepted route definition' blue http://127.0.0.1:${http_forward}/api
expect 'Rejected route definition is not published' 404 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/changed
"${k[@]}" rollout restart deployment/"$release"
"${k[@]}" rollout status deployment/"$release" --timeout=150s
start_forward
expect 'New replicas restore accepted plugin after invalid update' green -H 'x-canary: 1' http://127.0.0.1:${http_forward}/api
expect 'New replicas restore accepted routing definition' 404 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/changed
"${k[@]}" patch ingress app --type json -p '[{"op":"replace","path":"/spec/rules/0/http/paths/0/path","value":"/api"}]'
"${k[@]}" scale deployment blue --replicas=0
expect 'Endpoint removal applies during invalid plugin update' 503 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/api
"${k[@]}" scale deployment blue --replicas=1
"${k[@]}" rollout status deployment/blue --timeout=120s
expect 'Endpoint addition restores backend' blue http://127.0.0.1:${http_forward}/api
"${k[@]}" delete service blue
expect 'Service deletion withdraws its endpoints' 503 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/api
cat <<EOF | "${k[@]}" apply -f -
apiVersion: v1
kind: Service
metadata: {name: blue}
spec:
  selector: {app: blue}
  ports: [{name: http, port: 80, targetPort: http}]
EOF
expect 'Service recreation restores endpoints' blue http://127.0.0.1:${http_forward}/api
"${k[@]}" delete configmap routes
expect 'Deleted plugin disables routes' 503 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/api
plugin
expect 'Plugin recreation restores route' blue http://127.0.0.1:${http_forward}/api
"${k[@]}" create secret tls qa-tls-fallback --cert="$work/cert.pem" --key="$work/key.pem" --dry-run=client -o yaml | "${k[@]}" apply -f -
cat <<EOF | "${k[@]}" apply -f -
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: tls-conflicting}
spec:
  ingressClassName: $class
  tls:
    - hosts: [qa.example.test, '*.example.test']
      secretName: qa-tls-fallback
  rules:
    - host: spare.example.test
      http:
        paths:
          - path: /api
            pathType: Prefix
            backend: {service: {name: green, port: {name: http}}}
EOF
for _ in $(seq 1 100); do
  [[ -n "$("${k[@]}" get events --field-selector involvedObject.name=tls-conflicting,reason=TLSConflict -o name)" ]] && break
  sleep .2
done
[[ -n "$("${k[@]}" get events --field-selector involvedObject.name=tls-conflicting,reason=TLSConflict -o name)" ]]
echo 'PASS conflicting TLS owner publishes Kubernetes Event'
"${k[@]}" delete secret qa-tls
expect 'Deleted TLS Secret blocks conflicting and wildcard fallback' 000 -k -o /dev/null -w '%{http_code}' --resolve qa.example.test:${https_forward}:127.0.0.1 https://qa.example.test:${https_forward}/api
certificate 3
expect 'TLS Secret recreation' blue -k --resolve qa.example.test:${https_forward}:127.0.0.1 https://qa.example.test:${https_forward}/api
[[ "$(serial)" == serial=03 ]]
echo 'PASS recovered TLS owner serves its replacement certificate'
"${k[@]}" delete ingress tls-conflicting
"${k[@]}" delete secret qa-tls-fallback
"${k[@]}" patch service "$release" --subresource=status --type merge -p '{"status":{"loadBalancer":{"ingress":[{"ip":"192.0.2.10"}]}}}'
for _ in $(seq 1 300); do
  [[ "$("${k[@]}" get ingress app -o jsonpath='{.status.loadBalancer.ingress[0].ip}')" == 192.0.2.10 ]] && break
  sleep .2
done
[[ "$("${k[@]}" get ingress app -o jsonpath='{.status.loadBalancer.ingress[0].ip}')" == 192.0.2.10 ]]
echo 'PASS elected leader publishes Service address'
"${k[@]}" patch service "$release" --subresource=status --type merge -p '{"status":{"loadBalancer":{"ingress":null}}}'
for _ in $(seq 1 100); do
  [[ -z "$("${k[@]}" get ingress app -o jsonpath='{.status.loadBalancer.ingress}')" ]] && break
  sleep .2
done
[[ -z "$("${k[@]}" get ingress app -o jsonpath='{.status.loadBalancer.ingress}')" ]]
echo 'PASS withdrawn Service address clears Ingress status'
# The earlier rollout can remove the old holder before its Lease expires.
for _ in $(seq 1 240); do
  leader="$("${k[@]}" get lease "$class-leader" -o jsonpath='{.spec.holderIdentity}')"
  leader_state="$("${k[@]}" get pod "$leader" -o jsonpath='{.status.phase}:{.metadata.deletionTimestamp}:{.metadata.labels.app\.kubernetes\.io/name}' 2>/dev/null || true)"
  [[ "$leader_state" == 'Running::rgnix' ]] && break
  sleep .25
done
[[ "$leader_state" == 'Running::rgnix' ]]
"${k[@]}" delete pod "$leader" --wait=false
for _ in $(seq 1 180); do
  next_leader="$("${k[@]}" get lease "$class-leader" -o jsonpath='{.spec.holderIdentity}')"
  [[ -n "$next_leader" && "$next_leader" != "$leader" ]] && break
  sleep .25
done
[[ -n "$next_leader" && "$next_leader" != "$leader" ]]
echo 'PASS Lease leadership transfers after leader termination'
"${k[@]}" exec deployment/blue -- sh -c '
  for n in $(seq 1 300); do
    response=$(curl -fsS --max-time 2 -H "Host: qa.example.test" http://rgnix-qa/api) || exit 1
    [ "$response" = blue ] || exit 1
    sleep .1
  done
' > "$work/rolling-probe.log" 2>&1 &
probe_pid=$!
"${k[@]}" rollout restart deployment/"$release"
"${k[@]}" rollout status deployment/"$release" --timeout=150s
if wait "$probe_pid"; then echo 'PASS 300 Service requests during rolling upgrade'; else cat "$work/rolling-probe.log" >&2; exit 1; fi
start_forward
expect 'Two-replica rolling upgrade' blue http://127.0.0.1:${http_forward}/api
for _ in $(seq 1 40); do
  hashes=""
  for pod_ip in $("${k[@]}" get pods -l app.kubernetes.io/name=rgnix -o jsonpath='{.items[*].status.podIP}'); do
    hash="$("${k[@]}" exec deployment/blue -- curl -fsS --max-time 3 "http://${pod_ip}:9090/metrics" | awk '/^rgnix_config_info\{/ {print $0}')"
    hashes="${hashes}${hash}"$'\n'
  done
  [[ "$(printf '%s' "$hashes" | awk 'NF' | sort -u | wc -l | tr -d ' ')" == 1 ]] && break
  sleep .25
done
[[ -n "$hash" && "$(printf '%s' "$hashes" | awk 'NF' | sort -u | wc -l | tr -d ' ')" == 1 ]]
echo 'PASS replicas publish matching configuration fingerprints'
"${k[@]}" delete ingress app
expect 'Ingress deletion removes routing' 404 -o /dev/null -w '%{http_code}' http://127.0.0.1:${http_forward}/api
"${k[@]}" apply -f "$work/ingresses.yaml"
expect 'Restored inspection environment' blue http://127.0.0.1:${http_forward}/api
echo "Ingress acceptance completed in namespace $ns (resources retained for inspection)."

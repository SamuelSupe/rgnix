#!/usr/bin/env bash
# A real Collector verifies decoding, authentication and the Ingress Helm wiring.
set -euo pipefail
cd "$(dirname "$0")/.."
ns="${1:?usage: otlp-collector-e2e.sh dedicated-namespace [context]}"
context="${2:-orbstack}"
k=(kubectl --context "$context" -n "$ns")
work="$(mktemp -d /tmp/rgnix-otlp.XXXXXX)"
forward_pid=""
cleanup() {
  if [[ -n "$forward_pid" ]]; then kill "$forward_pid" 2>/dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT
if kubectl --context "$context" get namespace "$ns" >/dev/null 2>&1; then
  [[ "$(kubectl --context "$context" get namespace "$ns" -o jsonpath='{.metadata.labels.rgnix-qa}')" == true ]] || { echo "Namespace is not owned by acceptance tests" >&2; exit 1; }
else
  kubectl --context "$context" create namespace "$ns"
  kubectl --context "$context" label namespace "$ns" rgnix-qa=true
fi

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=collector \
  -addext "subjectAltName=DNS:collector.$ns.svc" \
  -keyout "$work/tls.key" -out "$work/tls.crt" >/dev/null 2>&1
"${k[@]}" create secret tls collector-tls --cert="$work/tls.crt" --key="$work/tls.key" --dry-run=client -o yaml | "${k[@]}" apply -f -
"${k[@]}" create configmap otlp-ca --from-file=ca.crt="$work/tls.crt" --dry-run=client -o yaml | "${k[@]}" apply -f -
"${k[@]}" create secret generic otlp-auth --from-literal=headers='Authorization=Bearer%20rgnix-synthetic-test-token' --dry-run=client -o yaml | "${k[@]}" apply -f -
cat > "$work/collector.yaml" <<'EOF'
extensions:
  bearertokenauth:
    token: rgnix-synthetic-test-token
  health_check:
    endpoint: 0.0.0.0:13133
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
        auth:
          authenticator: bearertokenauth
        tls:
          cert_file: /tls/tls.crt
          key_file: /tls/tls.key
exporters:
  debug:
    verbosity: detailed
    sampling_initial: 100
    sampling_thereafter: 1
service:
  extensions: [bearertokenauth, health_check]
  pipelines:
    logs:
      receivers: [otlp]
      exporters: [debug]
    traces:
      receivers: [otlp]
      exporters: [debug]
EOF
"${k[@]}" create configmap collector-config --from-file=config.yaml="$work/collector.yaml" --dry-run=client -o yaml | "${k[@]}" apply -f -
cat <<'EOF' | "${k[@]}" apply -f -
apiVersion: apps/v1
kind: Deployment
metadata:
  name: collector
spec:
  replicas: 1
  selector:
    matchLabels: {app: collector}
  template:
    metadata:
      labels: {app: collector}
    spec:
      containers:
        - name: collector
          image: otel/opentelemetry-collector-contrib:0.123.0
          args: [--config=/config/config.yaml]
          readinessProbe:
            httpGet: {path: /, port: 13133}
          resources:
            requests: {cpu: 50m, memory: 64Mi}
            limits: {cpu: "1", memory: 256Mi}
          volumeMounts:
            - {name: config, mountPath: /config, readOnly: true}
            - {name: tls, mountPath: /tls, readOnly: true}
      volumes:
        - name: config
          configMap: {name: collector-config}
        - name: tls
          secret: {secretName: collector-tls}
---
apiVersion: v1
kind: Service
metadata:
  name: collector
spec:
  selector: {app: collector}
  ports:
    - {name: otlp, port: 4318, targetPort: 4318}
    - {name: health, port: 13133, targetPort: 13133}
EOF
"${k[@]}" rollout restart deployment/collector
"${k[@]}" rollout status deployment/collector --timeout=120s
helm upgrade --install rgnix-otlp charts/rgnix --kube-context "$context" -n "$ns" \
  --set ingressClass="$ns" --set service.type=ClusterIP --set image.pullPolicy=Never \
  --set "watchNamespaces[0]=$ns" \
  --set image.tag="${RGNIX_IMAGE_TAG:-0.3.0}" \
  --set otlpLogs.endpoint="https://collector.$ns.svc:4318/v1/logs" \
  --set otlpLogs.serviceName=rgnix-ingress-qa \
  --set otlpLogs.headersSecret.name=otlp-auth --set otlpLogs.caConfigMap.name=otlp-ca \
  --set otlpTraces.endpoint="https://collector.$ns.svc:4318/v1/traces" \
  --set otlpTraces.sampleRatio=0 \
  --set otlpTraces.headersSecret.name=otlp-auth --set otlpTraces.caConfigMap.name=otlp-ca \
  --wait --timeout=120s
# Repeated runs rotate the test CA; exporters load their trust bundle at startup.
"${k[@]}" rollout restart deployment/rgnix-otlp
"${k[@]}" rollout status deployment/rgnix-otlp --timeout=120s
cat <<EOF | "${k[@]}" apply -f -
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: access-log-test
spec:
  ingressClassName: $ns
  rules:
    - host: access.example.test
      http:
        paths:
          - path: /
            pathType: Exact
            backend:
              service:
                name: collector
                port: {name: health}
EOF

pods="$("${k[@]}" get pods -l app.kubernetes.io/instance=rgnix-otlp -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.metadata.deletionTimestamp}{"\n"}{end}' | awk 'NF == 1 {print $1}')"
[[ "$(wc -w <<< "$pods" | tr -d ' ')" == 2 ]] || { echo "Expected two controller replicas" >&2; exit 1; }
for pod in $pods; do
  "${k[@]}" port-forward "pod/$pod" :8080 :9090 > "$work/forward.log" 2>&1 &
  forward_pid=$!
  for _ in {1..80}; do
    if grep -q '127.0.0.1:.* -> 9090' "$work/forward.log"; then break; fi
    sleep 0.1
  done
  http_port="$(sed -n 's/.*127\.0\.0\.1:\([0-9]*\) -> 8080/\1/p' "$work/forward.log" | head -1)"
  admin_port="$(sed -n 's/.*127\.0\.0\.1:\([0-9]*\) -> 9090/\1/p' "$work/forward.log" | head -1)"
  [[ -n "$http_port" && -n "$admin_port" ]]
  for _ in {1..80}; do
    code="$(curl -sS -o "$work/body" -w '%{http_code}' -H 'Host: access.example.test' \
      -H 'traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' \
      -H 'tracestate: qa=collector' "http://127.0.0.1:$http_port/?credential=query-private")"
    if [[ "$code" == 200 ]]; then break; fi
    sleep 0.1
  done
  [[ "$code" == 200 ]] || { cat "$work/body"; exit 1; }
  echo "PASS Ingress request on $pod"
  for _ in {1..80}; do
    curl -fsS "http://127.0.0.1:$admin_port/metrics" > "$work/metrics"
    if grep -Eq '^rgnix_otlp_logs_exported_total [1-9][0-9]*$' "$work/metrics" && \
      grep -Eq '^rgnix_otlp_traces_exported_total ([2-9]|[1-9][0-9]+)$' "$work/metrics"; then break; fi
    sleep 0.1
  done
  grep -Eq '^rgnix_otlp_logs_exported_total [1-9][0-9]*$' "$work/metrics"
  grep -Eq '^rgnix_otlp_traces_exported_total ([2-9]|[1-9][0-9]+)$' "$work/metrics"
  echo "PASS Collector acknowledged authenticated HTTPS logs and traces from $pod"
  kill "$forward_pid" 2>/dev/null || true
  wait "$forward_pid" 2>/dev/null || true
  forward_pid=""
done
"${k[@]}" logs deployment/collector --tail=5000 > "$work/decoded.log"
for field in 'service.name: Str(rgnix-ingress-qa)' "k8s.namespace.name: Str($ns)" 'http.response.status_code: Int(200)' 'Body: Str(HTTP access)' 'rgnix.upstream.address:' 'rgnix.request.duration_ms: Double(' 'rgnix.config.sha256:'; do
  grep -Fq "$field" "$work/decoded.log" || { cat "$work/decoded.log"; echo "Missing field: $field" >&2; exit 1; }
done
for pod in $pods; do grep -Fq "k8s.pod.name: Str($pod)" "$work/decoded.log"; done
! grep -Fq 'query-private' "$work/decoded.log"
echo 'PASS Real Collector decoded resource, HTTP, backend, duration and config fields from both replicas; query excluded'
sed -E 's/[[:blank:]]+:/:/g' "$work/decoded.log" > "$work/trace-fields.log"
for field in 'Trace ID: 4bf92f3577b34da6a3ce929d0e0e4736' 'Parent ID: 00f067aa0ba902b7' 'TraceState: qa=collector' 'Kind: Server' 'Kind: Client' 'rgnix.parent_span_id: Str(00f067aa0ba902b7)' 'rgnix.upstream.span_id:'; do
  grep -Fq "$field" "$work/trace-fields.log" || { cat "$work/decoded.log"; echo "Missing trace field: $field" >&2; exit 1; }
done
echo 'PASS Real Collector decoded server/client spans, W3C parent/state and correlated access logs'
if [[ -n "${RGNIX_OTLP_EVIDENCE_DIR:-}" ]]; then
  mkdir -p "$RGNIX_OTLP_EVIDENCE_DIR"
  cp "$work/decoded.log" "$RGNIX_OTLP_EVIDENCE_DIR/otlp-collector-decoded.txt"
  "${k[@]}" get pods -o wide > "$RGNIX_OTLP_EVIDENCE_DIR/otlp-pods.txt"
fi
echo "PASS OTLP Ingress acceptance; namespace $ns retained for inspection"

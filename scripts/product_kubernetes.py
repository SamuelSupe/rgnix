#!/usr/bin/env python3
"""Live policy acceptance in a dedicated, retained OrbStack QA namespace."""
import concurrent.futures
import gzip
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import tempfile
import time

from integration import check, RESULTS
from product_features import certificates, jwt


BACKEND = '''import http.server,json,ssl,sys,time
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version="HTTP/1.1"
    def log_message(self,*args): pass
    def do_POST(self): self.do_GET()
    def do_GET(self):
        body=self.rfile.read(int(self.headers.get("content-length","0")))
        if self.path=="/slow": time.sleep(.6)
        status=200
        if self.path=="/authorize": status=200 if self.headers.get("authorization")=="Bearer approved" else 401
        data=json.dumps({"body_size":len(body),"headers":dict(self.headers),"path":self.path}).encode()
        if self.path=="/large": data=b"application response "*1000
        self.send_response(status)
        self.send_header("Content-Type","text/plain")
        self.send_header("Content-Length",str(len(data)))
        if self.path=="/authorize": self.send_header("x-tenant","verified")
        self.end_headers()
        try: self.wfile.write(data)
        except (BrokenPipeError,ConnectionResetError): pass
server=http.server.ThreadingHTTPServer(("0.0.0.0",int(sys.argv[1])),Handler)
if len(sys.argv)>2:
    context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain("/tls/tls.crt","/tls/tls.key")
    server.socket=context.wrap_socket(server.socket,server_side=True)
server.serve_forever()
'''


def main():
    namespace = sys.argv[1]
    context = sys.argv[2] if len(sys.argv) > 2 else "orbstack"
    tag = os.environ.get("RGNIX_IMAGE_TAG", "0.4.0")
    repository = os.environ.get("RGNIX_IMAGE_REPOSITORY", "rgnix")
    kube = ["kubectl", "--context", context, "-n", namespace]
    forwards = []

    def run(*args, data=None):
        result = subprocess.run(args, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        if result.returncode:
            raise RuntimeError(f"{args[:4]}: {result.stderr}")
        return result.stdout

    def k(*args, **kwargs):
        return run(*kube, *args, **kwargs)

    def apply(kind, name, **fields):
        version = {"Deployment": "apps/v1", "Ingress": "networking.k8s.io/v1"}.get(kind, "v1")
        k("apply", "-f", "-", data=json.dumps({"apiVersion": version, "kind": kind, "metadata": {"name": name, "namespace": namespace}, **fields}))

    def secret(name, **files):
        args = [f"--from-file={key}={value}" for key, value in files.items()]
        k("apply", "-f", "-", data=k("create", "secret", "generic", name, *args, "--dry-run=client", "-o", "json"))

    def deployment(name, containers, volumes):
        apply("Deployment", name, spec={
            "replicas": 1,
            "selector": {"matchLabels": {"app": name}},
            "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {"containers": containers, "volumes": volumes},
            },
        })

    def ingress(name, annotations, backend_port="http", tls=False):
        host = "example.test" if tls else name + ".product.test"
        spec = {
            "ingressClassName": namespace,
            "rules": [{"host": host, "http": {"paths": [{
                "path": "/", "pathType": "Prefix",
                "backend": {"service": {"name": "policy-app", "port": {"name": backend_port}}},
            }]}}],
        }
        if tls:
            spec["tls"] = [{"hosts": [host], "secretName": "policy-tls"}]
        apply("Ingress", name, metadata={"name": name, "namespace": namespace, "annotations": {"rgnix.io/" + key: value for key, value in annotations.items()}}, spec=spec)

    def wait(fn, expected, timeout=45):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                last = fn()
                if last == expected:
                    return last
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(.2)
        raise AssertionError(f"expected {expected!r}; last response {last!r}")

    def forward(pod, root):
        output = open(root / f"{pod}.forward", "w+")
        process = subprocess.Popen([*kube, "port-forward", "--address", "127.0.0.1", "pod/" + pod, ":8080", ":8443", ":9090"], stdout=output, stderr=output)
        forwards.append((process, output))
        deadline = time.monotonic() + 15
        ports = {}
        while time.monotonic() < deadline:
            for line in (root / f"{pod}.forward").read_text().splitlines():
                if line.startswith("Forwarding from"):
                    ports[int(line.split()[-1])] = int(line.split()[2].rsplit(":", 1)[1])
            if len(ports) == 3:
                return ports
            time.sleep(.1)
        raise AssertionError("port-forward did not become ready")

    def pods():
        return [item["metadata"]["name"] for item in json.loads(k("get", "pods", "-l", "app.kubernetes.io/instance=rgnix-qa", "-o", "json"))["items"] if not item["metadata"].get("deletionTimestamp")]

    with tempfile.TemporaryDirectory(prefix="rgnix-policy-k8s-") as temp:
        root = Path(temp)
        try:
            labels = json.loads(k("get", "namespace", namespace, "-o", "json"))["metadata"].get("labels", {})
            if labels.get("rgnix-qa") != "true":
                raise RuntimeError("run ingress-e2e.sh first in a dedicated rgnix-qa=true namespace")
            k("delete","ingress","staged","business","canary","forced-fallback","--ignore-not-found")
            certificates(root)
            secret("policy-tls", **{"tls.crt": root / "server.crt", "tls.key": root / "server.key"})
            secret("policy-ca", **{"ca.crt": root / "ca.crt"})
            secret("policy-jwt", **{"jwks.json": root / "jwks.json"})
            token = "policy-admin-" + os.urandom(32).hex()
            (root / "token").write_text(token)
            secret("policy-admin", token=root / "token")
            apply("ConfigMap", "policy-backend", data={"server.py": BACKEND})
            containers = []
            for name, port in [("http", 8080), ("https", 8443)]:
                containers.append({
                    "name": name, "image": "python:3.13-alpine",
                    "args": ["python3", "/app/server.py", str(port), *(["tls"] if name == "https" else [])],
                    "ports": [{"name": name, "containerPort": port}],
                    "volumeMounts": [
                        {"name": "script", "mountPath": "/app", "readOnly": True},
                        {"name": "tls", "mountPath": "/tls", "readOnly": True},
                    ],
                    "resources": {
                        "requests": {"cpu": "10m", "memory": "16Mi"},
                        "limits": {"cpu": "200m", "memory": "96Mi"},
                    },
                    "readinessProbe": {"tcpSocket": {"port": port}, "periodSeconds": 1},
                })
            deployment("policy-backend", containers, [
                {"name": "script", "configMap": {"name": "policy-backend"}},
                {"name": "tls", "secret": {"secretName": "policy-tls"}},
            ])
            def service(name):
                apply("Service", name, spec={
                    "selector": {"app": "policy-backend"},
                    "ports": [
                        {"name": "http", "port": 80, "targetPort": 8080},
                        {"name": "https", "port": 443, "targetPort": 8443},
                    ],
                })
            service("policy-app")
            service("policy-auth")
            collector = '''receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  debug:
    verbosity: detailed
service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [debug]
'''
            apply("ConfigMap", "policy-collector", data={"config.yaml": collector})
            deployment("policy-collector", [{
                "name": "collector", "image": "otel/opentelemetry-collector-contrib:0.123.0",
                "args": ["--config=/config/config.yaml"], "ports": [{"containerPort": 4318}],
                "volumeMounts": [{"name": "config", "mountPath": "/config"}],
                "resources": {
                    "requests": {"cpu": "10m", "memory": "32Mi"},
                    "limits": {"cpu": "500m", "memory": "128Mi"},
                },
            }], [{"name": "config", "configMap": {"name": "policy-collector"}}])
            apply("Service", "policy-collector", spec={"selector": {"app": "policy-collector"}, "ports": [{"port": 4318, "targetPort": 4318}]})
            k("rollout", "restart", "deployment/policy-backend")
            k("rollout", "status", "deployment/policy-backend", "--timeout=120s")
            k("rollout", "restart", "deployment/policy-collector")
            k("rollout", "status", "deployment/policy-collector", "--timeout=120s")
            apply("ConfigMap", "namespace-policy", data={"policy.json":json.dumps({"default":{"max_inflight":2,"max_plugins":1,"max_auth":1,"max_mirrors":1,"max_body_bytes":3*1024*1024,"max_limiter_keys":16,"allow_mirroring":True,"requests_per_second":10000,"burst":10000},"namespaces":{namespace+"-tenant":{"max_ingresses":1,"max_routes":1,"max_inflight":2,"max_plugins":1,"max_auth":1,"max_mirrors":1}}})})
            values = {
                "ingressClass": namespace, "image": {"repository": repository, "tag": tag, "pullPolicy": "Never"},
                "reportReplicas": True,
                "shutdown": {"enabled": True},
                "service": {
                    "type": "LoadBalancer", "loadBalancerClass": "rgnix.io/acceptance",
                    "allocateLoadBalancerNodePorts": False,
                },
                "admin": {"tokenSecret": {"name": "policy-admin"}},
                "tenancy":{"policyConfigMap":{"name":"namespace-policy"}},
                "forwarding": {"trustedProxies": ["127.0.0.0/8"], "recursive": True},
                "otlpTraces": {
                    "endpoint": f"http://policy-collector.{namespace}.svc:4318/v1/traces",
                    "sampleRatio": 1,
                },
            }
            (root / "values.json").write_text(json.dumps(values))
            run("helm", "upgrade", "--install", "rgnix-qa", "charts/rgnix", "--kube-context", context, "-n", namespace, "-f", str(root / "values.json"), "--wait", "--timeout", "180s")
            # Start each acceptance run from fresh controller processes and counters.
            k("rollout", "restart", "deployment/rgnix-qa")
            k("rollout", "status", "deployment/rgnix-qa", "--timeout=180s")
            ports = forward(pods()[0], root)

            def request(name, path="/", method="GET", body=None, headers=None, mtls=False, admin=False):
                if mtls:
                    ctx = ssl.create_default_context(cafile=str(root / "ca.crt"))
                    ctx.load_cert_chain(root / "client.crt", root / "client.key")
                    connection = http.client.HTTPConnection("127.0.0.1", ports[8443], timeout=4)
                    connection.sock = ctx.wrap_socket(socket.create_connection(("127.0.0.1", ports[8443]), timeout=4), server_hostname="example.test")
                    host = "example.test"
                else:
                    connection = http.client.HTTPConnection("127.0.0.1", ports[9090 if admin else 8080], timeout=4)
                    host = name + ".product.test"
                try:
                    connection.request(method, path, body, {"Host": host, **(headers or {})})
                    response = connection.getresponse()
                    return response.status, dict((key.lower(), value) for key, value in response.getheaders()), response.read()
                finally:
                    connection.close()

            ingress("policy", {"client-max-body-size": "4m", "proxy-read-timeout": "200ms", "access-log": "off"})
            wait(lambda: request("policy")[0], 200)
            check("Kubernetes policy allows a 2 MiB POST without a plugin", request("policy", method="POST", body=b"x" * (2 * 1024 * 1024))[0] == 200)
            check("Kubernetes per-Ingress upstream timeout is enforced", request("policy", "/slow")[0] == 504)
            identity = json.loads(request("policy", headers={"x-forwarded-for": "203.0.113.9", "x-forwarded-proto": "https"})[2])["headers"]
            identity = {name.lower(): value for name, value in identity.items()}
            if identity.get("x-real-ip") != "203.0.113.9" or identity.get("x-forwarded-proto") != "https":
                raise AssertionError(f"unexpected forwarded identity: {identity}")
            check("Helm trusted proxy settings control forwarded identity", identity.get("x-real-ip") == "203.0.113.9" and identity.get("x-forwarded-proto") == "https")
            check("Helm admin token enables protected diagnostics", request("", "/v1/config", admin=True)[0] == 401 and request("", "/v1/config", admin=True, headers={"Authorization": "Bearer " + token})[0] == 200)
            wait(lambda: json.loads(request("", "/v1/fleet", admin=True, headers={"Authorization": "Bearer " + token})[2]).get("converged"), True)
            fleet = json.loads(request("", "/v1/fleet", admin=True, headers={"Authorization": "Bearer " + token})[2])
            check("Ingress replicas report the same accepted configuration", len(fleet["replicas"]) == 2 and fleet["target"]["active_sha256"])
            check("Ingress rejects snapshot rollback in favor of source resources", request("", "/v1/rollback/1", method="POST", admin=True, headers={"Authorization": "Bearer " + token})[0] == 409)
            ingress("rate", {"limit-rate": "1 burst=2 key=header:x-tenant"})
            def rate_limited():
                key = os.urandom(8).hex()
                return [request("rate", headers={"x-tenant": key})[0] for _ in range(3)]
            wait(rate_limited, [200, 200, 429])
            check("Ingress rate annotation enforces an isolated request bucket", True)
            ingress("compressed", {"compression": "gzip br"})
            wait(lambda: request("compressed")[0], 200)
            compressed = request("compressed", "/large", headers={"Accept-Encoding": "gzip"})
            check("Ingress compression streams a valid gzip representation", compressed[1].get("content-encoding") == "gzip" and gzip.decompress(compressed[2]) == b"application response " * 1000)
            ingress("secure", {"backend-protocol": "HTTPS", "upstream-server-name": "example.test", "upstream-ca-secret": "policy-ca"}, "https")
            wait(lambda: request("secure")[0], 200)
            check("Ingress HTTPS verifies its Service certificate with a Secret CA", True)
            secret("policy-ca", **{"ca.crt": root / "other-ca.crt"})
            wait(lambda: request("secure")[0] in (502, 503), True)
            check("Ingress CA rotation rejects pooled connections under old trust", True)
            secret("policy-ca", **{"ca.crt": root / "ca.crt"})
            wait(lambda: request("secure")[0], 200)
            k("delete", "secret", "policy-ca")
            wait(lambda: request("secure")[0], 503)
            check("Ingress CA Secret deletion withdraws the protected route", True)
            secret("policy-ca", **{"ca.crt": root / "ca.crt"})
            wait(lambda: request("secure")[0], 200)
            check("Ingress CA Secret restoration restores verified forwarding", True)
            ingress("jwt", {"jwt-secret": "policy-jwt", "jwt-issuer": "qa", "jwt-audience": "api"})
            bearer = {"Authorization": "Bearer " + jwt(root, {"iss": "qa", "aud": "api", "exp": int(time.time()) + 300})}
            wait(lambda: request("jwt", headers=bearer)[0], 200)
            check("Ingress JWT Secret authenticates and rejects anonymous callers", request("jwt")[0] == 401)
            k("delete", "secret", "policy-jwt")
            wait(lambda: request("jwt", headers=bearer)[0], 503)
            check("JWT Secret withdrawal takes effect while its policy is retained", True)
            secret("policy-jwt", **{"jwks.json": root / "jwks.json"})
            wait(lambda: request("jwt", headers=bearer)[0], 200)
            ingress("auth", {"auth-service": "policy-auth:http/authorize", "auth-response-headers": "x-tenant"})
            approved = {"Authorization": "Bearer approved", "x-tenant": "forged"}
            wait(lambda: request("auth", headers=approved)[0], 200)
            vetted = {name.lower(): value for name, value in json.loads(request("auth", headers=approved)[2])["headers"].items()}
            check("Ingress external auth replaces an untrusted identity header", vetted.get("x-tenant") == "verified" and request("auth")[0] == 401)
            k("delete", "service", "policy-auth")
            wait(lambda: request("auth", headers=approved)[0], 503)
            check("External auth Service revocation fails closed", True)
            service("policy-auth")
            wait(lambda: request("auth", headers=approved)[0], 200)
            ingress("mtls", {"client-ca-secret": "policy-ca", "verify-client": "on"}, tls=True)
            wait(lambda: request("mtls", mtls=True)[0], 200)
            check("Ingress client CA Secret enables mutual TLS", True)
            k("annotate", "ingress", "policy", "rgnix.io/client-max-body-size=invalid", "--overwrite")
            wait(lambda: bool(json.loads(k("get", "events", "--field-selector", "involvedObject.name=policy,reason=InvalidPlugin", "-o", "json"))["items"]), True)
            check("Invalid policy retains its accepted body limit without a script", request("policy", method="POST", body=b"x" * (2 * 1024 * 1024))[0] == 200)
            k("rollout", "restart", "deployment/rgnix-qa")
            k("rollout", "status", "deployment/rgnix-qa", "--timeout=180s")
            for pod in pods():
                ports = forward(pod, root)
                wait(lambda: request("policy")[0], 200)
                check("Restarted replica restores policy-only checkpoint: " + pod, request("policy", method="POST", body=b"x" * (2 * 1024 * 1024))[0] == 200)
            k("patch", "service", "policy-app", "--type=merge", "-p", '{"spec":{"selector":{"app":"withdrawn"}}}')
            wait(lambda: request("policy")[0], 503)
            check("Invalid policy cannot freeze Kubernetes endpoint withdrawal", True)
            service("policy-app")
            wait(lambda: request("policy")[0], 200)
            k("delete", "ingress", "policy")
            wait(lambda: request("policy")[0], 404)
            check("Deleting a policy-only Ingress removes its recovered route", True)

            platform_code = """import http.server,json,sys,time
role=sys.argv[2]
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*args):pass
    def do_POST(self):self.do_GET()
    def do_GET(self):
        data=self.rfile.read(int(self.headers.get('content-length','0')))
        if self.path!='/stats':
            self.server.count+=1
            self.server.last={'path':self.path,'body':data.decode(errors='replace'),'headers':dict(self.headers)}
        if self.path=='/slow':time.sleep(1)
        if self.path=='/stall' and role=='mirror':time.sleep(2)
        if self.path=='/latency' and role=='candidate':time.sleep(.25)
        status=503 if role=='candidate' and self.path=='/fail' else 200
        body=json.dumps({'role':role,'count':self.server.count,'last':self.server.last}).encode()
        self.send_response(status);self.send_header('Content-Length',str(len(body)));self.send_header('x-auth-role',role);self.end_headers()
        try:self.wfile.write(body)
        except (BrokenPipeError,ConnectionResetError):pass
server=http.server.ThreadingHTTPServer(('0.0.0.0',int(sys.argv[1])),Handler)
server.count=0;server.last={};server.serve_forever()
"""
            apply("ConfigMap","platform-backend",data={"server.py":platform_code})
            deployment("platform-backend",[{"name":role,"image":"python:3.13-alpine","args":["python3","/app/server.py",str(port),role],"volumeMounts":[{"name":"script","mountPath":"/app"}],"resources":{"requests":{"cpu":"10m","memory":"16Mi"},"limits":{"cpu":"200m","memory":"96Mi"}}} for role,port in [("stable",8080),("candidate",8081),("mirror",8082)]],[{"name":"script","configMap":{"name":"platform-backend"}}])
            k("rollout","restart","deployment/platform-backend")
            k("rollout","status","deployment/platform-backend","--timeout=120s")
            backend_pod=next(p for p in json.loads(k("get","pods","-l","app=platform-backend","-o","json"))["items"] if not p["metadata"].get("deletionTimestamp"))
            backend_ip=backend_pod["status"]["podIP"]
            for role,port in [("stable",8080),("candidate",8081),("mirror",8082)]:
                apply("Service",role,spec={"selector":{"app":"platform-backend"},"ports":[{"name":"http","port":80,"targetPort":port}]})
            admin_auth={"Authorization":"Bearer "+token}
            traffic={"revision":"weights-v1","backends":[{"service":"stable:http","weight":90},{"service":"candidate:http","weight":10}]}
            ingress("canary",{"traffic-policy":json.dumps(traffic)})
            wait(lambda:"weights-v1" in request("","/v1/routes",admin=True,headers=admin_auth)[2].decode(),True)
            wait(lambda:request("canary")[0],200)
            replies=[json.loads(request("canary")[2])["role"] for _ in range(100)]
            check("Declarative Service traffic split follows 90/10 weights",replies.count("stable")==90 and replies.count("candidate")==10)
            traffic["revision"]="mirror-v1"
            traffic["mirror"]={"service":"mirror:http","percent":100,"max_body_bytes":1024,"timeout_ms":250}
            ingress("canary",{"traffic-policy":json.dumps(traffic)})
            admin_auth={"Authorization":"Bearer "+token}
            wait(lambda:'mirror-v1' in request("","/v1/routes",admin=True,headers=admin_auth)[2].decode(),True)
            # Read mirror state through a separate route, without adding mirroring to it.
            def backend_route(name,service,ns=namespace):
                value={"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":name,"namespace":ns},"spec":{"ingressClassName":namespace,"rules":[{"host":name+".product.test","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":service,"port":{"name":"http"}}}}]}}]}}
                run("kubectl","--context",context,"apply","-f","-",data=json.dumps(value))
            backend_route("mirror-state","mirror")
            wait(lambda:request("mirror-state","/stats")[0],200)
            before=json.loads(request("mirror-state","/stats")[2])["count"]
            response=request("canary","/mirror-body",method="POST",body=b"complete payload")
            wait(lambda:json.loads(request("mirror-state","/stats")[2])["count"],before+1)
            mirrored=json.loads(request("mirror-state","/stats")[2])["last"]
            check("Mirror sends the complete bounded body independently",response[0]==200 and mirrored["body"]=="complete payload" and mirrored["headers"].get("x-rgnix-mirror")=="true")
            request("canary","/too-large",method="POST",body=b"x"*2048)
            time.sleep(.3)
            check("Oversized mirror bodies are skipped without truncating primary requests",json.loads(request("mirror-state","/stats")[2])["count"]==before+1)
            start=time.monotonic();response=request("canary","/stall");elapsed=time.monotonic()-start
            check("Slow mirrors do not delay primary responses",response[0]==200 and elapsed<.8)
            traffic={"revision":"rollback-v1","backends":[{"service":"stable:http","weight":90},{"service":"candidate:http","weight":10}],"rollback":{"fallback":"stable:http","min_requests":3,"error_percent":50,"window_seconds":60}}
            ingress("canary",{"traffic-policy":json.dumps(traffic)})
            wait(lambda:'rollback-v1' in request("","/v1/routes",admin=True,headers=admin_auth)[2].decode(),True)
            for _ in range(105):request("canary","/fail")
            wait(lambda:json.loads(k("get","ingress","canary","-o","json"))["metadata"]["annotations"].get("rgnix.io/rolled-back-revision"),"rollback-v1")
            check("Candidate errors trigger durable Ingress rollback metadata",True)
            for pod in pods():
                ports=forward(pod,root)
                wait(lambda:all(request("canary","/fail")[0]==200 for _ in range(15)),True)
            check("Both replicas adopt the persisted fallback",True)
            k("rollout","restart","deployment/rgnix-qa");k("rollout","status","deployment/rgnix-qa","--timeout=180s")
            ports=forward(pods()[0],root)
            wait(lambda:request("canary","/fail")[0],200)
            check("Automatic rollback survives controller restart",all(json.loads(request("canary","/fail")[2])["role"]=="stable" for _ in range(20)))
            # A new revision is an explicit re-arm, independent of resource watch churn.
            prior_hash=json.loads(request("","/v1/config",admin=True,headers=admin_auth)[2])["sha256"]
            traffic["revision"]="latency-v2";traffic["backends"][0]["weight"]=0;traffic["backends"][1]["weight"]=100
            traffic["rollback"]["max_p95_ms"]=100
            ingress("canary",{"traffic-policy":json.dumps(traffic)})
            wait(lambda:'latency-v2' in request("","/v1/routes",admin=True,headers=admin_auth)[2].decode(),True)
            check("Configuration fingerprint includes traffic weights and rollback policy",json.loads(request("","/v1/config",admin=True,headers=admin_auth)[2])["sha256"]!=prior_hash)
            for _ in range(3):request("canary","/latency")
            wait(lambda:json.loads(k("get","ingress","canary","-o","json"))["metadata"]["annotations"].get("rgnix.io/rolled-back-revision"),"latency-v2")
            check("Configured p95 latency threshold also rolls back a new revision",True)
            # One auth address fails while another remains ready in EndpointSlice.
            apply("Service","auth-ha",spec={"ports":[{"name":"http","port":80}]})
            for name,port in [("auth-dead",9),("auth-live",8080)]:
                run("kubectl","--context",context,"apply","-f","-",data=json.dumps({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":name,"namespace":namespace,"labels":{"kubernetes.io/service-name":"auth-ha"}},"addressType":"IPv4","ports":[{"name":"http","port":port,"protocol":"TCP"}],"endpoints":[{"addresses":[backend_ip],"conditions":{"ready":True}}]}))
            ingress("auth-ha",{"auth-service":"auth-ha:http/authorize","auth-response-headers":"x-auth-role"})
            wait(lambda:request("auth-ha")[0],200)
            check("External auth fails over across ready EndpointSlice addresses",all(request("auth-ha")[0]==200 for _ in range(8)))
            ingress("ceiling",{"client-max-body-size":"0","proxy-read-timeout":"86400s"})
            wait(lambda:request("ceiling")[0],200)
            check("Administrator body ceiling overrides an unlimited application policy",request("ceiling",method="POST",headers={"Content-Length":str(3*1024*1024+1)})[0]==413)
            routes=json.loads(request("","/v1/routes",admin=True,headers=admin_auth)[2])
            ceiling=next(route for host in routes for route in host["routes"] if "ceiling:" in route["id"])
            check("Administrator timeout ceiling is reflected in effective configuration",ceiling["settings"]["read_timeout"]["secs"]==60)
            tenant=namespace+"-tenant"
            run("kubectl","--context",context,"apply","-f","-",data=json.dumps({"apiVersion":"v1","kind":"Namespace","metadata":{"name":tenant,"labels":{"rgnix-qa":"true"}}}))
            run("kubectl","--context",context,"apply","-f","-",data=json.dumps({"apiVersion":"v1","kind":"Service","metadata":{"name":"stable","namespace":tenant},"spec":{"ports":[{"name":"http","port":80}]}}))
            run("kubectl","--context",context,"apply","-f","-",data=json.dumps({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"stable","namespace":tenant,"labels":{"kubernetes.io/service-name":"stable"}},"addressType":"IPv4","ports":[{"name":"http","port":8080,"protocol":"TCP"}],"endpoints":[{"addresses":[backend_ip],"conditions":{"ready":True}}]}))
            backend_route("isolated","stable",tenant);backend_route("held","stable")
            wait(lambda:request("isolated")[0],200);wait(lambda:request("held")[0],200)
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                held=[pool.submit(request,"held","/slow") for _ in range(2)]
                time.sleep(.2)
                check("Namespace inflight exhaustion leaves another namespace available",request("held")[0]==503 and request("isolated")[0]==200)
                check("Namespace permits release after the held requests finish",all(f.result()[0]==200 for f in held))
            backend_route("over-quota","stable",tenant)
            time.sleep(1)
            check("Namespace configuration quota rejects additional Ingress routes",request("over-quota")[0]==404 and request("isolated")[0]==200)
            ingress("key-budget",{"limit-rate":"100 burst=100 key=header:x-key"})
            wait(lambda:request("key-budget")[0],200)
            codes=[request("key-budget",headers={"x-key":str(i)})[0] for i in range(25)]
            check("Limiter key exhaustion is isolated by namespace",503 in codes and request("isolated")[0]==200)

            # Governance runs against the same real resources, after verifying the restrictive quotas.
            tenant_policy={"default":{"max_inflight":32,"max_plugins":8,"max_auth":8,"max_mirrors":8,"max_limiter_keys":128,"allow_mirroring":True,"requests_per_second":10000,"burst":10000},"namespaces":{}}
            controls_before=json.loads(request("","/v1/config",admin=True,headers=admin_auth)[2])["controls_sha256"]
            controller_pods=pods()
            apply("ConfigMap","namespace-policy",data={"policy.json":json.dumps(tenant_policy)})
            wait(lambda:json.loads(request("","/v1/config",admin=True,headers=admin_auth)[2])["controls_sha256"]!=controls_before,True,180)
            wait(lambda:request("over-quota")[0],200)
            check("Namespace policy projection reloads quotas without replacing controller pods",pods()==controller_pods)

            apply("ConfigMap","forced-route",data={"main.rgl":'function on_request() return route.proxy("candidate:http") end'})
            forced={"revision":"forced-v1","backends":[{"service":"stable:http","weight":0},{"service":"candidate:http","weight":100}],"rollback":{"fallback":"stable:http","min_requests":3,"error_percent":50,"window_seconds":60}}
            ingress("forced-fallback",{"traffic-policy":json.dumps(forced),"script":"forced-route/main.rgl"})
            wait(lambda:"forced-v1" in request("","/v1/routes",admin=True,headers=admin_auth)[2].decode(),True)
            def forced_candidate_ready():
                status, _, body = request("forced-fallback")
                return status == 200 and json.loads(body).get("role") == "candidate"
            wait(forced_candidate_ready, True)
            for _ in range(10):request("forced-fallback","/fail")
            wait(lambda:json.loads(k("get","ingress","forced-fallback","-o","json"))["metadata"]["annotations"].get("rgnix.io/rolled-back-revision"),"forced-v1")
            for pod in pods():
                ports=forward(pod,root)
                wait(lambda:request("forced-fallback","/fail")[0],200)
                check("Persisted fallback overrides an explicit RGL backend on replica " + pod,all(json.loads(request("forced-fallback","/fail")[2])["role"]=="stable" for _ in range(5)))

            backend_route("owned","stable")
            wait(lambda:request("owned")[0],200)
            attack={"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"domain-claim","namespace":tenant},"spec":{"ingressClassName":namespace,"rules":[{"host":"owned.product.test","http":{"paths":[{"path":"/private","pathType":"Prefix","backend":{"service":{"name":"stable","port":{"name":"http"}}}}]}}]}}
            run("kubectl","--context",context,"apply","-f","-",data=json.dumps(attack))
            wait(lambda:bool(json.loads(run("kubectl","--context",context,"-n",tenant,"get","events","--field-selector","involvedObject.name=domain-claim,reason=DomainDenied","-o","json"))["items"]),True)
            check("A different namespace cannot acquire a longer path on an owned hostname",json.loads(request("owned","/private")[2])["role"]=="stable")

            domains={}
            for ns in [namespace,tenant]:
                for obj in json.loads(run("kubectl","--context",context,"-n",ns,"get","ingress","-o","json"))["items"]:
                    if obj["spec"].get("ingressClassName")!=namespace or obj["metadata"]["name"]=="domain-claim":continue
                    for rule in obj["spec"].get("rules",[]):domains.setdefault(rule.get("host",""),[ns])
                    for tls in obj["spec"].get("tls",[]):
                        for host in tls.get("hosts",[]):domains.setdefault(host,[ns])
                    if obj["spec"].get("defaultBackend"):domains.setdefault("",[ns])
            domains.update({"business.product.test":[namespace],"staged.product.test":[namespace],"preflight.product.test":[namespace],"granted.product.test":[tenant]})
            tenant_policy["domains"]=domains
            apply("ConfigMap","namespace-policy",data={"policy.json":json.dumps(tenant_policy)})
            read_token="tenant-reader-"+os.urandom(32).hex()
            write_token="tenant-writer-"+os.urandom(32).hex()
            users={"users":[{"name":"tenant-reader","role":"reader","namespaces":[tenant],"token_sha256":hashlib.sha256(read_token.encode()).hexdigest()},{"name":"tenant-operator","role":"writer","namespaces":[tenant],"token_sha256":hashlib.sha256(write_token.encode()).hexdigest()}]}
            (root/"users.json").write_text(json.dumps(users));secret("governance-users",**{"users.json":root/"users.json"})
            metrics={"gates":{"qa-gate":{"url":f"http://stable.{namespace}.svc/stats","pointer":"/count","max":0}}}
            metrics["gates"]["business-fail"]={"url":f"http://stable.{namespace}.svc/stats","pointer":"/count","max":-1}
            (root/"metrics.json").write_text(json.dumps(metrics));secret("governance-metrics",**{"metrics.json":root/"metrics.json"})
            admission_host=f"rgnix-qa-admission.{namespace}.svc"
            run("openssl","req","-new","-newkey","rsa:2048","-nodes","-subj","/CN="+admission_host,"-keyout",str(root/"admission.key"),"-out",str(root/"admission.csr"))
            (root/"admission.ext").write_text("subjectAltName=DNS:"+admission_host+"\nextendedKeyUsage=serverAuth\n")
            run("openssl","x509","-req","-in",str(root/"admission.csr"),"-CA",str(root/"ca.crt"),"-CAkey",str(root/"ca.key"),"-CAcreateserial","-days","1","-extfile",str(root/"admission.ext"),"-out",str(root/"admission.crt"))
            secret("governance-admission",**{"tls.crt":root/"admission.crt","tls.key":root/"admission.key"})
            values["watchNamespaces"]=[namespace,tenant]
            values["admin"]["usersSecret"]={"name":"governance-users"}
            values["rolloutMetrics"]={"secret":{"name":"governance-metrics"}}
            values["admission"]={"enabled":True,"tlsSecret":"governance-admission","caBundle":base64.b64encode((root/"ca.crt").read_bytes()).decode()}
            (root/"values.json").write_text(json.dumps(values))
            run("helm","upgrade","rgnix-qa","charts/rgnix","--kube-context",context,"-n",namespace,"-f",str(root/"values.json"),"--wait","--timeout","180s")
            ports=forward(pods()[0],root)
            wait(lambda:request("isolated")[0],200)
            account=f"system:serviceaccount:{namespace}:rgnix-qa"
            yes=run("kubectl","--context",context,"auth","can-i","list","secrets","-n",tenant,"--as",account).strip()
            no=subprocess.run(["kubectl","--context",context,"auth","can-i","list","secrets","-n","default","--as",account],capture_output=True,text=True)
            check("Scoped Helm RBAC permits selected namespaces and denies unrelated Secrets",yes=="yes" and no.returncode==1 and no.stdout.strip()=="no")
            scoped={"Authorization":"Bearer "+read_token}
            visible=json.loads(request("","/v1/routes",admin=True,headers=scoped)[2])
            check("Named namespace identity sees only its own effective routes",bool(visible) and all(r["id"].startswith(tenant+"/") for h in visible for r in h["routes"]))
            command={"owner":namespace+"/forced-fallback","revision":"forced-v1","operation":"rollback"}
            check("Namespace writer cannot operate another tenant rollout",request("","/v1/rollouts",method="POST",body=json.dumps(command),admin=True,headers={"Authorization":"Bearer "+write_token})[0]==403)

            granted=json.loads(json.dumps(attack));granted["metadata"]["name"]="granted";granted["spec"]["rules"][0]["host"]="granted.product.test";granted["spec"]["rules"][0]["http"]["paths"][0]["path"]="/"
            run("kubectl","--context",context,"apply","-f","-",data=json.dumps(granted))
            wait(lambda:request("granted")[0],200)
            check("Explicit administrator domain grants allow the authorized namespace",True)
            denied_claim=json.loads(json.dumps(attack));denied_claim["metadata"]["name"]="new-domain-claim"
            denied=subprocess.run(["kubectl","--context",context,"apply","--dry-run=server","-f","-"],input=json.dumps(denied_claim),capture_output=True,text=True)
            check("Admission rejects a cross-namespace domain claim before persistence",denied.returncode!=0 and "DomainDenied" in denied.stderr)
            candidate=json.loads(json.dumps(granted));candidate["metadata"]={"name":"preflight","namespace":namespace};candidate["spec"]["rules"][0]["host"]="preflight.product.test"
            status,_,body=request("","/v1/validate-ingress",method="POST",body=json.dumps(candidate),admin=True,headers=admin_auth)
            check("Ingress preflight returns a candidate diff without publishing",status==200 and json.loads(body)["valid"] and json.loads(body)["diff"]["added"] and request("preflight")[0]==404)
            run("kubectl","--context",context,"apply","--dry-run=server","-f","-",data=json.dumps(candidate))
            candidate["metadata"]["annotations"]={"rgnix.io/client-max-body-size":"invalid"}
            rejected=subprocess.run(["kubectl","--context",context,"apply","--dry-run=server","-f","-"],input=json.dumps(candidate),capture_output=True,text=True)
            check("API server dry-run accepts valid policies and rejects invalid ones",rejected.returncode!=0 and "InvalidPlugin" in rejected.stderr)
            tampered=json.loads(json.dumps(candidate));tampered["metadata"]["annotations"]={"rgnix.io/rollout-state":"{}"}
            denied=subprocess.run(["kubectl","--context",context,"apply","--dry-run=server","-f","-"],input=json.dumps(tampered),capture_output=True,text=True)
            check("Admission protects controller-owned rollout progress from tenant edits",denied.returncode!=0 and "controller-owned" in denied.stderr)

            staged={"revision":"stages-v1","backends":[{"service":"stable:http","weight":50},{"service":"candidate:http","weight":50}],"cohort":"cookie:release","rollback":{"fallback":"stable:http","min_requests":3,"error_percent":50,"window_seconds":60},"steps":[{"weights":{"stable:http":50,"candidate:http":50},"duration_seconds":1,"min_requests":0,"approval":True},{"weights":{"stable:http":0,"candidate:http":100},"duration_seconds":1,"min_requests":0,"approval":True}],"metric_gates":["qa-gate"]}
            ingress("staged",{"traffic-policy":json.dumps(staged)})
            def progress():
                value=json.loads(k("get","ingress","staged","-o","json"))["metadata"]["annotations"].get("rgnix.io/rollout-state","{}")
                return json.loads(value)
            wait(lambda:progress().get("started_at",0)>0,True)
            cohorts=[]
            for pod in pods():
                ports=forward(pod,root)
                wait(lambda:request("staged")[0],200)
                roles=[json.loads(request("staged",headers={"Cookie":"release=user-42"})[2])["role"] for _ in range(8)]
                cohorts.append(roles)
            check("Stable cohort routing chooses the same Service across replicas",len(set(sum(cohorts,[])))==1)
            def action(operation,stage=None,headers=admin_auth):
                data={"owner":namespace+"/staged","revision":"stages-v1","operation":operation}
                if stage is not None:data["stage"]=stage
                return request("","/v1/rollouts",method="POST",body=json.dumps(data),admin=True,headers=headers)[0]
            check("Stage approval rejects stale stage numbers",action("approve",9)==409)
            check("Stage can be paused and approved through the audited writer API",action("pause")==200 and action("approve",0)==200)
            wait(lambda:progress().get("paused"),True)
            time.sleep(6)
            check("Paused rollout preserves its current weights",progress()["stage"]==0)
            check("Resume accepts the current persisted revision",action("resume")==200)
            time.sleep(6)
            check("Failing external metric gate prevents stage promotion",progress()["stage"]==0)
            metric_status=json.loads(request("","/v1/config",admin=True,headers=admin_auth)[2])["metric_gates"]["qa-gate"]
            check("Metric diagnostics explain a fresh failed gate without exposing its endpoint",metric_status["fresh"] and not metric_status["passed"] and "http" not in json.dumps(metric_status))
            metrics["gates"]["qa-gate"].pop("max");metrics["gates"]["qa-gate"]["min"]=0
            (root/"metrics.json").write_text(json.dumps(metrics));secret("governance-metrics",**{"metrics.json":root/"metrics.json"})
            wait(lambda:progress().get("stage"),1,180)
            check("Hot metric provider update permits approved healthy stage advancement",True)
            for pod in pods():
                ports=forward(pod,root)
                wait(lambda:all(json.loads(request("staged")[2])["role"]=="candidate" for _ in range(4)),True)
            check("Both replicas adopt the persisted stage weights",True)
            k("rollout","restart","deployment/rgnix-qa");k("rollout","status","deployment/rgnix-qa","--timeout=180s")
            ports=forward(pods()[0],root)
            wait(lambda:request("staged")[0],200)
            check("Stage and pending approval survive controller restart",progress()["stage"]==1 and not progress()["promoted"] and progress()["approved_stage"] is None)
            check("Final stage accepts explicit approval",action("approve",1)==200)
            wait(lambda:progress().get("promoted"),True)
            check("Successful final observation marks the rollout promoted",True)
            simulation={"host":"staged.product.test","path":"/preview?q=1","headers":{"Cookie":"release=user-42"}}
            status,_,body=request("","/v1/simulate",method="POST",body=json.dumps(simulation),admin=True,headers=admin_auth)
            plan=json.loads(body).get("outbound",{})
            check("Ingress simulation exposes effective rollout backend and final URI",status==200 and "candidate" in plan.get("backend","") and plan.get("uri")=="/preview?q=1" and plan.get("io_executed") is False)

            business=json.loads(json.dumps(staged));business.pop("steps");business.pop("cohort")
            business.update(revision="business-v1",metric_gates=["business-fail"],metric_rollback={"consecutive_failures":2,"failure_seconds":4})
            business["backends"]=[{"service":"stable:http","weight":0},{"service":"candidate:http","weight":100}]
            ingress("business",{"traffic-policy":json.dumps(business)})
            def business_role():
                status,_,body=request("business")
                return json.loads(body).get("role") if status==200 else None
            wait(business_role,"candidate")
            wait(lambda:json.loads(k("get","ingress","business","-o","json"))["metadata"]["annotations"].get("rgnix.io/rolled-back-revision"),"business-v1",60)
            check("Business metric failures roll back HTTP-successful Ingress candidates",business_role()=="stable")
            k("rollout","restart","deployment/rgnix-qa");k("rollout","status","deployment/rgnix-qa","--timeout=180s")
            for pod in pods():
                ports=forward(pod,root)
                wait(business_role,"stable")
            check("External-metric rollback persists across all Ingress replica restarts",True)

            ingress("mtls",{"client-ca-secret":"policy-ca","verify-client":"on","ssl-redirect":"true"},tls=True)
            wait(lambda:request("", "/secure?q=1",headers={"Host":"example.test"})[0],308)
            redirect=request("","/secure?q=1",headers={"Host":"example.test"})
            check("Ingress HTTPS redirect preserves the path and query before authentication",redirect[1].get("location")=="https://example.test/secure?q=1" and request("mtls",mtls=True)[0]==200)
            controller_pods=pods()
            users["users"]=[];(root/"users.json").write_text(json.dumps(users));secret("governance-users",**{"users.json":root/"users.json"})
            wait(lambda:request("","/v1/routes",admin=True,headers=scoped)[0],401,180)
            check("Projected identity revocation takes effect without pod restart",pods()==controller_pods)

            original_class=json.loads(k("get","ingressclass",namespace,"-o","json"))
            assert original_class["metadata"]["annotations"]["meta.helm.sh/release-namespace"]==namespace
            original_class["metadata"]={key:value for key,value in original_class["metadata"].items() if key in ("name","labels","annotations")}
            foreign_class=json.loads(json.dumps(original_class));foreign_class["spec"]["controller"]="other.example/controller"
            try:
                k("delete","ingressclass",namespace)
                run("kubectl","--context",context,"create","-f","-",data=json.dumps(foreign_class))
                wait(lambda:"belongs to a different controller" in request("","/v1/validate-ingress",method="POST",body=json.dumps(candidate),admin=True,headers=admin_auth)[2].decode(),True)
                check("Management preflight rejects a Class reassigned to another controller",True)
                wait(lambda:subprocess.run(["kubectl","--context",context,"apply","--dry-run=server","-f","-"],input=json.dumps(candidate),capture_output=True,text=True).returncode,0)
                check("Admission does not impose rgnix policies on a foreign controller Class",True)
            finally:
                k("delete","ingressclass",namespace,"--ignore-not-found")
                run("kubectl","--context",context,"apply","-f","-",data=json.dumps(original_class))
            wait(lambda:request("staged")[0],200)
            check("Restoring the Class restores persisted stage routing",json.loads(request("staged")[2])["role"]=="candidate")

            wait(lambda: "Span #" in k("logs", "deployment/policy-collector", "--tail=1000"), True)
            decoded = k("logs", "deployment/policy-collector", "--tail=1000")
            check("Trace-only Helm export is decoded by a real OpenTelemetry Collector", "Kind           : Server" in decoded and "Kind           : Client" in decoded)
            Path(".local/product-collector-decoded.txt").write_text(decoded)
            Path(".local/product-kubernetes-pods.txt").write_text(k("get", "pods", "-o", "wide"))
        finally:
            for process, output in forwards:
                process.terminate()
                process.wait(timeout=5)
                output.close()
    result = {"namespace": namespace, "context": context, "image": repository + ":" + tag, "passed": len(RESULTS), "checks": RESULTS}
    Path(".local/product-kubernetes.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

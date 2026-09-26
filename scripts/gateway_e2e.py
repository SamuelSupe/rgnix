#!/usr/bin/env python3
"""Gateway API behavioral checks against an explicitly selected Kubernetes namespace."""
import argparse
import collections
import contextlib
import base64
import hashlib
import http.client
import json
import pathlib
import socket
import ssl
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

from product_features import certificates, jwt

parser = argparse.ArgumentParser()
parser.add_argument("--context", default="orbstack")
parser.add_argument("--namespace", required=True)
parser.add_argument("--image", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--soak-seconds", type=int, default=0)
parser.add_argument("--scale-routes", type=int, default=100)
parser.add_argument("--require-multiple-nodes", action="store_true")
parser.add_argument("--grpc-image", default="rgnix:gateway-grpc-fixture")
args = parser.parse_args()
assert 0 <= args.soak_seconds <= 86400 and 1 <= args.scale_routes <= 2000
ns, peer = args.namespace, args.namespace + "-peer"
root = pathlib.Path(__file__).resolve().parents[1]
checks = []
kubectl = ["kubectl", "--context", args.context]


def command(*argv, data=None):
    result = subprocess.run(argv, input=data, text=True, capture_output=True, timeout=120)
    if result.returncode:
        raise RuntimeError(result.stderr or result.stdout)
    return result.stdout


def apply(*objects):
    return command(*kubectl, "apply", "-f", "-", data=json.dumps({"apiVersion": "v1", "kind": "List", "items": objects}))


def get(kind, name, namespace=ns):
    return json.loads(command(*kubectl, "-n", namespace, "get", kind, name, "-o", "json"))


def wait(label, check, seconds=50):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                checks.append({"name": label, "passed": True})
                print("PASS", label, flush=True)
                return value
        except Exception as error:
            last = str(error)
        time.sleep(0.5)
    raise AssertionError(f"{label}: {last}")


def route(name, rules, hosts=None, namespace=ns, annotations=None):
    return {"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": {"name": name, "namespace": namespace, "annotations": annotations or {}},
            "spec": {"parentRefs": [{"name": "gateway", "namespace": ns}], "hostnames": hosts or ["api.example.test"], "rules": rules}}


def rule(path="/", backend="a", **extra):
    return {"matches": [{"path": {"type": "PathPrefix", "value": path}}], "backendRefs": [{"name": backend, "port": 80}], **extra}


def condition(kind, name, type_, value, namespace=ns):
    item = get(kind, name, namespace)
    status = item.get("status", {})
    conditions = status.get("conditions", [])
    if kind.endswith("route"):
        conditions = [c for p in status.get("parents", []) for c in p.get("conditions", [])]
    if kind == "backendtlspolicy":
        conditions = [c for a in status.get("ancestors", []) for c in a.get("conditions", [])]
    return any(c.get("type") == type_ and c.get("status") == value and c.get("observedGeneration") == item["metadata"]["generation"] for c in conditions)


origin = '''import http.server,json,os,ssl,time
class Handler(http.server.BaseHTTPRequestHandler):
 protocol_version="HTTP/1.1"
 mirrors=0
 last_mirror=None
 def do_GET(self):
  body=self.rfile.read(int(self.headers.get('Content-Length',0)))
  if self.headers.get('x-rgnix-mirror') == 'true':
   Handler.mirrors += 1; Handler.last_mirror={'path':self.path,'host':self.headers.get('Host'),'body':body.decode(errors='replace')}
  if self.path in ('/v1/logs','/v1/traces'):
   self.send_response(200);self.send_header('Content-Length','0');self.end_headers();return
  if self.headers.get('x-delay'): time.sleep(min(5,float(self.headers['x-delay'])))
  if self.path.startswith('/slow'): time.sleep(0.4)
  if self.path.startswith('/trickle'):
   self.send_response(200);self.send_header('Content-Length','20');self.end_headers()
   try:
    for _ in range(20): self.wfile.write(b'x');self.wfile.flush();time.sleep(0.05)
   except (BrokenPipeError,ConnectionResetError): pass
   return
  if self.path == '/authorize':
   self.send_response(200 if self.headers.get('Authorization')=='Bearer approved' else 401);self.send_header('x-tenant','verified');self.send_header('Content-Length','0');self.end_headers();return
  out=json.dumps({'value':0,'mirrors':Handler.mirrors,'last_mirror':Handler.last_mirror,'backend':os.getenv('BACKEND','a'),'path':self.path,'headers':dict(self.headers),'body':body.decode(errors='replace')}).encode()
  self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('X-Origin','yes');self.send_header('Content-Length',str(len(out)));self.end_headers();self.wfile.write(out)
 do_POST=do_GET
 def log_message(self,*args): pass
server=http.server.ThreadingHTTPServer(('0.0.0.0',8080),Handler)
if os.getenv('TLS'):
 context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain('/tls/tls.crt','/tls/tls.key');server.socket=context.wrap_socket(server.socket,server_side=True)
server.serve_forever()
'''
grpc_origin = '''import grpc
from concurrent.futures import ThreadPoolExecutor
server=grpc.server(ThreadPoolExecutor(4))
def unary(request,context):
 context.set_trailing_metadata((('x-trailer','received'),))
 return request
server.add_generic_rpc_handlers((grpc.method_handlers_generic_handler('qa.Echo',{'Unary':grpc.unary_unary_rpc_method_handler(unary,request_deserializer=lambda v:v,response_serializer=lambda v:v),'Stream':grpc.stream_stream_rpc_method_handler(lambda requests,context:requests,request_deserializer=lambda v:v,response_serializer=lambda v:v)}),))
server.add_insecure_port('[::]:50051');server.start();server.wait_for_termination()
'''

for name in [ns, peer]:
    existing = command(*kubectl, "get", "namespace", name, "--ignore-not-found", "-o", "json")
    if existing and json.loads(existing).get("metadata", {}).get("labels", {}).get("gateway-qa") != ns:
        raise RuntimeError(f"refusing to reuse namespace {name} not owned by this fixture")
apply(*[{"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name, "labels": {"gateway-qa": ns}}} for name in [ns, peer]])
command(*kubectl, "-n", peer, "delete", "referencegrant", "allow", "tls", "--ignore-not-found")
for namespace in [ns, peer]:
    command(*kubectl,"-n",namespace,"delete","httproutes,grpcroutes,backendtlspolicies","--all","--ignore-not-found")
    apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "origin", "namespace": namespace}, "data": {"server.py": origin, "grpc_origin.py": grpc_origin}})
    for name in ["a", "b"]:
        apply({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "namespace": namespace}, "spec": {"replicas": 1, "selector": {"matchLabels": {"qa-origin": name}}, "template": {"metadata": {"labels": {"qa-origin": name}}, "spec": {"containers": [{"name": "origin", "image": "python:3.13-alpine", "command": ["python", "/app/server.py"], "env": [{"name": "BACKEND", "value": name}], "ports": [{"containerPort": 8080}], "volumeMounts": [{"name": "origin", "mountPath": "/app"}]}], "volumes": [{"name": "origin", "configMap": {"name": "origin"}}]}}}},
              {"apiVersion": "v1", "kind": "Service", "metadata": {"name": name, "namespace": namespace}, "spec": {"selector": {"qa-origin": name}, "ports": [{"name": "http", "port": 80, "targetPort": 8080}]}})

with tempfile.TemporaryDirectory() as directory:
    directory = pathlib.Path(directory)
    repository, tag = args.image.rsplit(":", 1)
    token = "gateway-qa-reader-" + ns
    writer = "gateway-qa-writer-" + ns
    users = {"users": [{"name": "namespace-reader", "role": "reader", "namespaces": [ns], "token_sha256": hashlib.sha256(token.encode()).hexdigest()}]}
    operator = "gateway-qa-operator-" + ns
    users["users"].append({"name":"operator","role":"writer","token_sha256":hashlib.sha256(operator.encode()).hexdigest()})
    users["users"].append({"name":"namespace-writer","role":"writer","namespaces":[ns,peer],"token_sha256":hashlib.sha256(writer.encode()).hexdigest()})
    apply({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "admin-users", "namespace": ns}, "stringData": {"users.json": json.dumps(users)}})
    values = {"mode": "gateway", "gateway": {"name": "gateway", "className": ns, "listeners": [{"name": "http", "protocol": "HTTP", "port": 80, "allowedRoutes": {"namespaces": {"from": "All"}}}]}, "image": {"repository": repository, "tag": tag}, "replicaCount": 2, "watchNamespaces": [ns, peer], "service": {"type": "ClusterIP"}}
    values["otlpLogs"] = {"endpoint":f"http://a.{ns}.svc/v1/logs"}
    values["otlpTraces"] = {"endpoint":f"http://a.{ns}.svc/v1/traces", "sampleRatio":1}
    values["reportReplicas"] = True
    values["shutdown"] = {"enabled": True}
    values["requireMultipleNodes"] = args.require_multiple_nodes
    values["upstreamMaxFails"] = 0
    cli_token = "gateway-qa-wait-" + ns
    apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"wait-token","namespace":ns},"stringData":{"token":cli_token}})
    values["admin"] = {"usersSecret": {"name": "admin-users", "key": "users.json"}, "tokenSecret":{"name":"wait-token","key":"token"}}
    apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"release-metrics","namespace":ns},"stringData":{"metrics.json":json.dumps({"gates":{"orders":{"url":f"http://a.{ns}.svc/metric","pointer":"/value","max":-1}}})}})
    values["rolloutMetrics"]={"secret":{"name":"release-metrics","key":"metrics.json"}}
    apply({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"redis","namespace":ns},"spec":{"replicas":1,"selector":{"matchLabels":{"qa-origin":"redis"}},"template":{"metadata":{"labels":{"qa-origin":"redis"}},"spec":{"containers":[{"name":"redis","image":"redis:8.0.2-alpine","args":["--save","","--appendonly","no"],"ports":[{"containerPort":6379}]}]}}}},
          {"apiVersion":"v1","kind":"Service","metadata":{"name":"redis","namespace":ns},"spec":{"selector":{"qa-origin":"redis"},"ports":[{"port":6379}]}})
    command(*kubectl,"-n",ns,"rollout","status","deployment/redis","--timeout=90s")
    apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"shared-rate","namespace":ns},"stringData":{"config.json":json.dumps({"url":f"redis://redis.{ns}.svc:6379","scope":ns,"timeout_ms":200})}})
    namespace_policy={"default":{"allow_mirroring":True,"requests_per_second":10000,"burst":10000}}
    apply({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"namespace-policy","namespace":ns},"data":{"policy.json":json.dumps(namespace_policy)}})
    values["globalRateLimit"]={"secret":{"name":"shared-rate","key":"config.json"}}
    values["tenancy"]={"policyConfigMap":{"name":"namespace-policy","key":"policy.json"}}
    (directory / "values.json").write_text(json.dumps(values))
    # Previous runs intentionally mutate listener ownership; reset only this fixture's Gateway.
    command(*kubectl, "-n", ns, "delete", "gateway", "gateway", "--ignore-not-found")
    command("helm", "--kube-context", args.context, "upgrade", "--install", "gateway", str(root / "charts/rgnix"), "-n", ns, "-f", str(directory / "values.json"), "--wait", "--timeout", "110s")
    command(*kubectl, "-n", ns, "rollout", "restart", "deployment/gateway")
    command(*kubectl, "-n", ns, "rollout", "status", "deployment/gateway", "--timeout=110s")
    sock = socket.socket(); sock.bind(("127.0.0.1", 0)); port = sock.getsockname()[1]; sock.close()
    sock = socket.socket(); sock.bind(("127.0.0.1", 0)); tls_port = sock.getsockname()[1]; sock.close()
    forward_log = (directory / "forward.log").open("w")
    sock = socket.socket(); sock.bind(("127.0.0.1", 0)); admin_port = sock.getsockname()[1]; sock.close()
    forward = subprocess.Popen([*kubectl, "-n", ns, "port-forward", "deployment/gateway", f"{port}:8080", f"{tls_port}:8443", f"{admin_port}:9090"], stdout=forward_log, stderr=forward_log)

    def request(path="/", host="api.example.test", headers=None, method="GET", body=None, target_port=None):
        headers = {"Host": host, **(headers or {})}
        req = urllib.request.Request(f"http://127.0.0.1:{target_port or port}{path}", headers=headers, method=method, data=body)
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, *args): return None
        try:
            response = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect).open(req, timeout=3)
        except urllib.error.HTTPError as error:
            response = error
        data = response.read()
        try: data = json.loads(data)
        except ValueError: pass
        return response.status, dict(response.headers), data

    def reconnect(restart=True):
        global forward
        if forward.poll() is None:
            forward.terminate(); forward.wait(timeout=5)
        if restart:
            command(*kubectl, "-n", ns, "rollout", "restart", "deployment/gateway")
            command(*kubectl, "-n", ns, "rollout", "status", "deployment/gateway", "--timeout=110s")
        forward = subprocess.Popen([*kubectl, "-n", ns, "port-forward", "deployment/gateway", f"{port}:8080", f"{tls_port}:8443", f"{admin_port}:9090"], stdout=forward_log, stderr=forward_log)

    def admin(path, data=None, target_port=None, credential=None):
        req=urllib.request.Request(f"http://127.0.0.1:{target_port or admin_port}{path}", data=None if data is None else json.dumps(data).encode(), headers={"Authorization":f"Bearer {credential or writer}","Content-Type":"application/json"})
        try: response=urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req,timeout=5)
        except urllib.error.HTTPError as error: response=error
        return response.status,json.loads(response.read())

    @contextlib.contextmanager
    def replicas():
        forwards=[]; ports=[]
        try:
            pods=json.loads(command(*kubectl,"-n",ns,"get","pods","-l","app.kubernetes.io/instance=gateway","-o","json"))["items"]
            for pod in pods:
                if pod["metadata"].get("deletionTimestamp"): continue
                pair=[]
                for _ in range(2):
                    sock=socket.socket();sock.bind(("127.0.0.1",0));pair.append(sock.getsockname()[1]);sock.close()
                forwards.append(subprocess.Popen([*kubectl,"-n",ns,"port-forward","pod/"+pod["metadata"]["name"],f"{pair[0]}:8080",f"{pair[1]}:9090"],stdout=forward_log,stderr=forward_log))
                pair.append(pod["metadata"]["name"])
                ports.append(pair)
            assert len(ports)==2
            deadline = time.monotonic() + 10
            for pair in ports:
                for local_port in pair[:2]:
                    while True:
                        try:
                            with socket.create_connection(("127.0.0.1", local_port), timeout=.5):
                                break
                        except OSError:
                            if time.monotonic() >= deadline or any(p.poll() is not None for p in forwards):
                                raise RuntimeError("replica port-forward did not start")
                            time.sleep(.05)
            yield ports
        finally:
            for process in forwards:
                if process.poll() is None: process.terminate()
                process.wait(timeout=5)

    failure = None
    try:
        wait("Gateway status reflects published generation", lambda: condition("gateway", "gateway", "Programmed", "True"))
        apply(route("base", [rule()]))
        wait("Service backend and Host preservation", lambda: request()[2].get("headers", {}).get("Host") == "api.example.test")
        wait("HTTPRoute status", lambda: condition("httproute", "base", "ResolvedRefs", "True"))
        wait("All replicas acknowledge the same accepted configuration", lambda: admin("/v1/fleet", credential=operator)[1].get("converged"))
        if args.require_multiple_nodes:
            pods = json.loads(command(*kubectl, "-n", ns, "get", "pods", "-l", "app.kubernetes.io/instance=gateway", "-o", "json"))["items"]
            nodes = sorted({pod["spec"]["nodeName"] for pod in pods if not pod["metadata"].get("deletionTimestamp")})
            assert len(nodes) >= 2, f"controllers must run on different nodes: {nodes}"
            checks.append({"name":"Controller replicas run on distinct Kubernetes nodes","passed":True,"nodes":nodes})
        assert admin("/v1/fleet")[0] == 403
        checks.append({"name":"Namespace-scoped identity cannot inspect peer configurations", "passed":True})
        observed_after = int(time.time()) + 1
        apply({"apiVersion":"v1","kind":"Pod","metadata":{"name":"node-agent-selector-fixture","namespace":ns,"labels":{"app.kubernetes.io/name":"rgnix-xdp","app.kubernetes.io/instance":"gateway"}},"spec":{"containers":[{"name":"agent","image":"python:3.13-alpine","command":["sleep","300"]}]}})
        try:
            def controller_only():
                view=admin("/v1/fleet",credential=operator)[1]
                return view.get("observed_at",0)>=observed_after and view.get("converged") and len(view["replicas"])==2
            wait("Replica discovery excludes same-release XDP node agents",controller_only)
        finally:
            command(*kubectl,"-n",ns,"delete","pod","node-agent-selector-fixture","--ignore-not-found","--wait=false")
        lease_role = get("role", "gateway-leader")
        lease_rule = next(index for index, item in enumerate(lease_role["rules"]) if "leases" in item["resources"])
        verbs = lease_role["rules"][lease_rule]["verbs"]
        def lease_verbs(value):
            command(*kubectl, "-n", ns, "patch", "role", "gateway-leader", "--type=json", "-p", json.dumps([{"op":"replace","path":f"/rules/{lease_rule}/verbs","value":value}]))
        try:
            lease_verbs([verb for verb in verbs if verb != "list"])
            wait("Replica observation fails closed when Lease reads are denied", lambda: admin("/v1/fleet", credential=operator)[1].get("error") and not admin("/v1/fleet", credential=operator)[1].get("converged"))
            assert request()[0] == 200
            checks.append({"name":"Replica observation failure leaves accepted data-plane traffic available","passed":True})
        finally:
            lease_verbs(verbs)
        wait("Replica observation recovers after RBAC restoration", lambda: admin("/v1/fleet", credential=operator)[1].get("converged"))
        for name, budgets in [("request-deadline", {"request":"150ms"}), ("backend-deadline", {"backendRequest":"150ms"}), ("disabled-deadline", {"request":"0s", "backendRequest":"0s"})]:
            apply(route(name, [rule(timeouts=budgets)], hosts=[name+".example.test"]))
            expected = 200 if name == "disabled-deadline" else 504
            wait(name+" bounds delayed upstream headers", lambda: request("/slow", host=name+".example.test")[0] == expected)
        def total_stream_timeout():
            # Port-forward can delay EOF after the gateway has already closed the stream.
            probe = '''import http.client,json,time
connection=http.client.HTTPConnection("gateway",80,timeout=3)
started=time.monotonic()
connection.request("GET","/trickle",headers={"Host":"backend-deadline.example.test"})
response=connection.getresponse()
try:
 response.read()
 raise AssertionError("stream completed beyond its deadline")
except http.client.IncompleteRead as error:
 print(json.dumps({"status":response.status,"bytes":len(error.partial),"seconds":time.monotonic()-started}))
finally: connection.close()
'''
            measured = json.loads(command(*kubectl,"-n",ns,"exec","deployment/a","--","python","-c",probe))
            return measured["status"] == 200 and 0 < measured["bytes"] < 20 and measured["seconds"] < 0.8
        wait("Backend deadline cancels a continuously streaming response", total_stream_timeout)
        candidate = route("preview", [rule("/preview", timeouts={"request":"1s","backendRequest":"100ms"})])
        before = admin("/v1/config")[1]["version"]
        code, preview = admin("/v1/validate-gateway", candidate)
        assert code == 200 and preview["valid"] and get("httproute", "base")["metadata"]["name"] == "base", preview
        candidate["spec"]["rules"][0]["timeouts"]["backendRequest"] = "2s"
        code, preview = admin("/v1/validate-gateway", candidate)
        assert code == 200 and not preview["valid"], preview
        assert admin("/v1/config")[1]["version"] == before
        checks.append({"name":"Gateway preflight validates duration relationships without publishing", "passed":True})
        apply(route("standard-mirror-count", [rule("/standard-mirror-count", "b")]))
        mirror_filter = {"type":"RequestMirror", "requestMirror":{"backendRef":{"name":"b","port":80},"fraction":{"numerator":1,"denominator":1}}}
        mirror_route = route("standard-mirror", [rule("/standard-mirror", filters=[mirror_filter])])
        apply(mirror_route)
        wait("Standard RequestMirror resolves its own backend reference", lambda: condition("httproute","standard-mirror","ResolvedRefs","True"))
        payload = b"mirror-complete-body"
        assert request("/standard-mirror?x=1", method="POST", body=payload)[0] == 200
        wait("Standard mirror preserves complete body, URI and Host", lambda: request("/standard-mirror-count")[2].get("last_mirror") == {"path":"/standard-mirror?x=1", "host":"api.example.test", "body":payload.decode()})
        mirror_filter["requestMirror"]["backendRef"]["namespace"] = peer
        apply(mirror_route)
        wait("Unauthorized mirror is disabled while primary requests continue", lambda: condition("httproute","standard-mirror","ResolvedRefs","False") and request("/standard-mirror")[0] == 200)
        def rejected_publication():
            view = admin("/v1/fleet", credential=operator)[1]
            return not view.get("converged") and view.get("target", {}).get("rejection")
        wait("Rejected references report a cause and prevent fleet publication acknowledgement", rejected_publication)
        mirror_filter["requestMirror"]["backendRef"].pop("namespace")
        mirror_filter["requestMirror"].pop("fraction")
        mirror_filter["requestMirror"]["percent"] = 0
        apply(mirror_route)
        wait("Zero-percent standard mirror is accepted", lambda: condition("httproute","standard-mirror","ResolvedRefs","True"))
        with replicas() as pairs:
            for pair in pairs:
                wait("Replica adopts zero-percent mirror", lambda: any(r.get("settings",{}).get("gateway",{}).get("mirror",{}).get("numerator") == 0 for h in admin("/v1/config",target_port=pair[1])[1]["routes"] for r in h["routes"] if "standard-mirror:gateway" in r["id"]))
        count = request("/standard-mirror-count")[2]["mirrors"]
        for _ in range(5): assert request("/standard-mirror")[0] == 200
        time.sleep(0.6)
        assert request("/standard-mirror-count")[2]["mirrors"] == count
        checks.append({"name":"Zero-percent mirror sends no shadow requests", "passed":True})
        apply(route("matches", [{"matches": [{"path": {"type": "Exact", "value": "/special"}, "method": "POST", "headers": [{"name": "X-Canary", "value": "1"}], "queryParams": [{"name": "version", "value": "2"}]}], "backendRefs": [{"name": "b", "port": 80}]}]))
        wait("Method, header and query match", lambda: request("/special?version=2", headers={"X-Canary": "1"}, method="POST")[2].get("backend") == "b")
        wait("Predicate mismatch falls through", lambda: request("/special?version=3", headers={"X-Canary": "1"}, method="POST")[2].get("backend") == "a")
        def duplicate_headers():
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
            connection.putrequest("POST", "/special?version=2", skip_host=True)
            for name, value in [("Host", "api.example.test"), ("Content-Length", "0"), ("X-Canary", "1"), ("X-Canary", "2")]: connection.putheader(name, value)
            connection.endheaders()
            response = connection.getresponse(); data = json.loads(response.read()); connection.close()
            return data.get("backend") == "b"
        wait("Duplicate header matching uses the first value", duplicate_headers)
        apply(route("prefix", [rule("/api", "b")]))
        wait("PathPrefix matches path segments", lambda: request("/api/v1")[2].get("backend") == "b" and request("/apix")[2].get("backend") == "a")
        apply(route("rewrite", [rule("/old", filters=[{"type": "URLRewrite", "urlRewrite": {"hostname": "backend.test", "path": {"type": "ReplacePrefixMatch", "replacePrefixMatch": "/new"}}}, {"type": "RequestHeaderModifier", "requestHeaderModifier": {"set": [{"name": "X-Literal", "value": "$host"}], "remove": ["X-Remove"]}}, {"type": "ResponseHeaderModifier", "responseHeaderModifier": {"set": [{"name": "X-Gateway", "value": "yes"}], "remove": ["X-Origin"]}}])]))
        def rewritten():
            code, headers, data = request("/old/item?q=1", headers={"X-Remove": "secret"})
            return code == 200 and data["path"] == "/new/item?q=1" and data["headers"].get("Host") == "backend.test" and data["headers"].get("X-Literal") == "$host" and "X-Remove" not in data["headers"] and headers.get("X-Gateway") == "yes" and "X-Origin" not in headers
        wait("Rewrite and literal request/response header modifiers", rewritten)
        apply(route("redirect", [{"matches": [{"path": {"type": "PathPrefix", "value": "/redirect"}}], "filters": [{"type": "RequestRedirect", "requestRedirect": {"scheme": "https", "statusCode": 301, "path": {"type": "ReplacePrefixMatch", "replacePrefixMatch": "/secure"}}}]}]))
        wait("Redirect preserves suffix/query and scheme default port", lambda: request("/redirect/a?q=1")[0] == 301 and request("/redirect/a?q=1")[1].get("Location") == "https://api.example.test/secure/a?q=1")
        apply(route("weighted", [{"matches": [{"path": {"type": "Exact", "value": "/weighted"}}], "backendRefs": [{"name": "a", "port": 80, "weight": 9}, {"name": "b", "port": 80, "weight": 1}]}]))
        wait("Weighted route accepted", lambda: condition("httproute", "weighted", "Accepted", "True"))
        counts = collections.Counter(request("/weighted")[2]["backend"] for _ in range(300))
        assert 8 <= counts["b"] <= 65, counts
        checks.append({"name": "Service weights independent of endpoint counts", "passed": True, "counts": dict(counts)})
        cross = route("cross", [{"matches": [{"path": {"type": "Exact", "value": "/cross"}}], "backendRefs": [{"name": "a", "namespace": peer, "port": 80}]}])
        apply(cross)
        wait("Cross-namespace backend denied without ReferenceGrant", lambda: request("/cross")[0] == 500 and condition("httproute", "cross", "ResolvedRefs", "False"))
        grant = {"apiVersion": "gateway.networking.k8s.io/v1", "kind": "ReferenceGrant", "metadata": {"name": "allow", "namespace": peer}, "spec": {"from": [{"group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "namespace": ns}], "to": [{"group": "", "kind": "Service", "name": "a"}]}}
        apply(grant)
        wait("ReferenceGrant permits Service", lambda: request("/cross")[0] == 200 and condition("httproute", "cross", "ResolvedRefs", "True"))
        command(*kubectl, "-n", peer, "delete", "referencegrant", "allow")
        wait("ReferenceGrant withdrawal revokes traffic", lambda: request("/cross")[0] == 500)
        apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "plugin", "namespace": ns}, "data": {"main.rgl": 'function on_request() return route.proxy("b:80") end'}})
        scripted = route("script", [rule("/script", backendRefs=[{"name": "a", "port": 80}, {"name": "b", "port": 80}])], annotations={"rgnix.io/script": "plugin/main.rgl"})
        apply(scripted)
        wait("RGL selects an explicitly declared backend", lambda: request("/script")[2].get("backend") == "b")
        def saved_script():
            maps=json.loads(command(*kubectl,"-n",ns,"get","configmaps","-l","rgnix.io/gateway-checkpoint","-o","json"))["items"]
            return any(json.loads(m.get("data",{}).get("accepted.json","{}" )).get("name")=="script" for m in maps)
        wait("Gateway accepted plugin is durably checkpointed", saved_script)
        apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "plugin", "namespace": ns}, "data": {"main.rgl": 'invalid plugin source'}})
        wait("Invalid plugin retains previous valid code", lambda: condition("httproute", "script", "rgnix.io/PluginReady", "False") and request("/script")[2].get("backend") == "b")
        wait("Gateway preflight cannot hide an invalid candidate behind its checkpoint", lambda: admin("/v1/validate-gateway", scripted)[1].get("valid") is False)
        reconnect()
        with replicas() as pairs:
            for index,pair in enumerate(pairs):
                assert request("/script",target_port=pair[0])[2].get("backend") == "b", "ready replica has not restored its accepted plugin"
                checks.append({"name":f"New replica {index+1} restores checkpoint before readiness while source is invalid","passed":True})
        command(*kubectl, "-n", ns, "scale", "deployment/b", "--replicas=0")
        wait("Endpoint withdrawal still applies during plugin failure", lambda: request("/script")[0] == 503)
        command(*kubectl, "-n", ns, "scale", "deployment/b", "--replicas=1")
        wait("Endpoint restoration", lambda: request("/script")[0] == 200)
        command(*kubectl,"-n",ns,"delete","configmap","plugin")
        apply({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"plugin","namespace":ns},"data":{"main.rgl":"invalid replacement"}})
        wait("Recreated ConfigMap cannot inherit old checkpoint code", lambda: condition("httproute","script","Accepted","False") and request("/script")[2].get("backend")=="a")
        wait("Revoked checkpoint is pruned", lambda: not saved_script())
        command(*kubectl, "-n", ns, "delete", "httproute", "script")
        wait("Route deletion takes effect", lambda: request("/script")[2].get("backend") == "a")
        apply(route("unsupported", [{"matches": [{"path": {"type": "RegularExpression", "value": "/.*"}}], "backendRefs": [{"name": "a", "port": 80}]}]))
        wait("Unsupported expressions reported in status", lambda: condition("httproute", "unsupported", "Accepted", "False"))
        apply(route("foreign", [rule()], hosts=["foreign.example.test"], namespace=peer))
        wait("AllowedRoutes admits another namespace", lambda: request(host="foreign.example.test")[0] == 200)
        command(*kubectl, "-n", ns, "patch", "gateway", "gateway", "--type=json", "-p", json.dumps([{"op": "replace", "path": "/spec/listeners/0/allowedRoutes/namespaces", "value": {"from": "Same"}}]))
        wait("AllowedRoutes revocation removes foreign routes", lambda: request(host="foreign.example.test")[0] == 404)
        selector = {"from": "Selector", "selector": {"matchLabels": {"gateway-qa": ns}}}
        command(*kubectl, "-n", ns, "patch", "gateway", "gateway", "--type=json", "-p", json.dumps([{"op": "replace", "path": "/spec/listeners/0/allowedRoutes/namespaces", "value": selector}]))
        wait("Namespace selector admits matching labels", lambda: request(host="foreign.example.test")[0] == 200)
        # A reader must authorize the same method/header/query route that simulation executes.
        apply(route("scoped-get", [{"matches": [{"method": "GET"}], "backendRefs": [{"name": "a", "port": 80}]}], hosts=["scope.example.test"]))
        apply(route("scoped-post", [{"matches": [{"method": "POST"}], "backendRefs": [{"name": "b", "port": 80}]}], hosts=["scope.example.test"], namespace=peer))
        wait("Cross-namespace method routing", lambda: request(host="scope.example.test", method="POST")[2].get("backend") == "b")
        def simulate(method):
            fixture = json.dumps({"host": "scope.example.test", "path": "/", "method": method}).encode()
            req = urllib.request.Request(f"http://127.0.0.1:{admin_port}/v1/simulate", data=fixture, headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
            try:
                response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=3)
            except urllib.error.HTTPError as error:
                return error.code
            result = json.loads(response.read())
            return response.status if "outbound" in result else 0
        wait("Namespace reader simulates an authorized Gateway route", lambda: simulate("GET") == 200)
        wait("Simulation cannot cross tenant boundaries by changing method", lambda: simulate("POST") == 403)

        security = directory / "security"
        security.mkdir()
        certificates(security)
        apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"jwt","namespace":ns},"stringData":{"jwks.json":(security/"jwks.json").read_text()}})
        apply(route("jwt",[rule("/protected")],hosts=["api.example.test"],annotations={"rgnix.io/jwt-secret":"jwt","rgnix.io/jwt-issuer":"qa","rgnix.io/jwt-audience":"api"}))
        wait("Gateway JWT rejects missing credentials",lambda:request("/protected")[0]==401)
        bearer=jwt(security,{"iss":"qa","aud":"api","exp":int(time.time())+600})
        wait("Gateway JWT validates the declared issuer and audience",lambda:request("/protected",headers={"Authorization":"Bearer "+bearer})[0]==200)
        command(*kubectl,"-n",ns,"delete","secret","jwt")
        wait("JWT Secret withdrawal cannot fall through to a public prefix",lambda:condition("httproute","jwt","Accepted","False") and request("/protected",headers={"Authorization":"Bearer "+bearer})[0]==500)
        apply(route("auth",[rule()],hosts=["auth.example.test"],annotations={"rgnix.io/auth-service":"a:80/authorize","rgnix.io/auth-response-headers":"x-tenant"}))
        wait("Gateway external auth denies unapproved requests",lambda:request(host="auth.example.test")[0]==401)
        wait("Gateway external auth replaces spoofed identity headers",lambda:request(host="auth.example.test",headers={"Authorization":"Bearer approved","x-tenant":"forged"})[2].get("headers",{}).get("x-tenant")=="verified")
        apply(route("rate",[rule()],hosts=["rate.example.test"],annotations={"rgnix.io/limit-rate":"1 burst=1 key=header:x-client"}))
        wait("Gateway rate policy accepted",lambda:condition("httproute","rate","Accepted","True"))
        with replicas() as pairs:
            for pair in pairs: wait("Replica serves shared-rate policy",lambda:admin("/v1/config",target_port=pair[1])[0]==200)
            statuses=[request(host="rate.example.test",headers={"x-client":"across-pods"},target_port=p[0])[0] for p in pairs]
            assert statuses==[200,429],statuses
            checks.append({"name":"Gateway rate budget is shared across replicas","passed":True})
        apply(route("mirror-count",[rule("/mirror-count","b")]))
        traffic={"revision":"gateway-staged","backends":[{"service":"a:80","weight":90},{"service":"b:80","weight":10}],
                 "mirror":{"service":"b:80","percent":100,"max_body_bytes":1024,"timeout_ms":500},
                 "rollback":{"fallback":"a:80","min_requests":100,"error_percent":50,"window_seconds":60},
                 "steps":[{"weights":{"a:80":90,"b:80":10},"duration_seconds":0,"min_requests":6,"approval":True},{"weights":{"a:80":0,"b:80":100},"duration_seconds":0,"min_requests":0,"approval":True}]}
        release=route("release",[rule("/release",backendRefs=[{"name":"a","port":80},{"name":"b","port":80}])],annotations={"rgnix.io/traffic-policy":json.dumps(traffic)})
        apply(release)
        def progress(): return json.loads(get("httproute","release")["metadata"]["annotations"].get("rgnix.io/rollout-state","{}"))
        wait("Gateway persists the initial rollout stage",lambda:progress().get("started_at",0)>0)
        with replicas() as pairs:
            leader_name = get("lease", "gateway-gw-"+hashlib.sha256(b"gateway").hexdigest()[:12])["spec"]["holderIdentity"]
            follower = next(pair for pair in pairs if pair[2] != leader_name)
            wait("Follower has the staged route", lambda: request("/release",target_port=follower[0])[0] == 200)
            counts=collections.Counter(request("/release",target_port=follower[0])[2]["backend"] for _ in range(100))
        assert counts=={"a":90,"b":10},counts
        checks.append({"name":"Gateway staged 90/10 Service split","passed":True,"counts":dict(counts)})
        wait("Gateway mirror reaches its declared Service",lambda:request("/mirror-count")[2].get("mirrors",0)>0)
        assert admin("/v1/rollouts",{"kind":"HTTPRoute","owner":ns+"/release","revision":traffic["revision"],"operation":"approve","stage":0})[0]==200
        wait("Gateway approval advances using candidate samples received only by a non-leader",lambda:progress().get("stage")==1)
        wait("Gateway stage weights change request routing",lambda:all(request("/release")[2].get("backend")=="b" for _ in range(5)))
        reconnect()
        wait("Gateway stage and approval survive both replicas restarting",lambda:request("/release")[2].get("backend")=="b" and progress().get("stage")==1 and progress().get("approved_stage") is None)
        assert admin("/v1/rollouts",{"kind":"HTTPRoute","owner":ns+"/release","revision":traffic["revision"],"operation":"rollback"})[0]==200
        wait("Gateway explicit rollback routes to its stable backend",lambda:all(request("/release")[2].get("backend")=="a" for _ in range(5)))
        business=json.loads(json.dumps(traffic)); business.pop("steps"); business.pop("mirror")
        business.update(revision="business-failure",metric_gates=["orders"],metric_rollback={"consecutive_failures":2,"failure_seconds":4})
        business["backends"]=[{"service":"a:80","weight":0},{"service":"b:80","weight":100}]
        apply(route("business",[rule("/business",backendRefs=[{"name":"a","port":80},{"name":"b","port":80}])],annotations={"rgnix.io/traffic-policy":json.dumps(business)}))
        wait("Business rollout initially uses HTTP-successful candidate",lambda:request("/business")[2].get("backend")=="b")
        wait("Sustained external business failure persists automatic rollback",lambda:get("httproute","business")["metadata"]["annotations"].get("rgnix.io/rolled-back-revision")=="business-failure",seconds=60)
        reconnect()
        with replicas() as pairs:
            for index,pair in enumerate(pairs):
                wait(f"Business rollback survives restart on replica {index+1}",lambda:request("/business",target_port=pair[0])[2].get("backend")=="a")
        invalid=json.loads(json.dumps(release)); invalid["metadata"]["name"]="undeclared"; invalid["spec"]["rules"][0]["backendRefs"]=[{"name":"a","port":80}]
        apply(invalid)
        wait("Gateway rollout cannot select an undeclared Service",lambda:condition("httproute","undeclared","Accepted","False"))

        apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"backend-cert","namespace":ns},"type":"kubernetes.io/tls","data":{"tls.crt":base64.b64encode((security/"server.crt").read_bytes()).decode(),"tls.key":base64.b64encode((security/"server.key").read_bytes()).decode()}},
              {"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"backend-ca","namespace":ns},"data":{"ca.crt":(security/"ca.crt").read_text()}},
              {"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"backend-tls","namespace":ns},"spec":{"replicas":1,"selector":{"matchLabels":{"qa-origin":"backend-tls"}},"template":{"metadata":{"labels":{"qa-origin":"backend-tls"}},"spec":{"containers":[{"name":"origin","image":"python:3.13-alpine","command":["python","/app/server.py"],"env":[{"name":"TLS","value":"true"}],"volumeMounts":[{"name":"origin","mountPath":"/app"},{"name":"tls","mountPath":"/tls"}]}],"volumes":[{"name":"origin","configMap":{"name":"origin"}},{"name":"tls","secret":{"secretName":"backend-cert"}}]}}}},
              {"apiVersion":"v1","kind":"Service","metadata":{"name":"backend-tls","namespace":ns},"spec":{"selector":{"qa-origin":"backend-tls"},"ports":[{"name":"https","port":443,"targetPort":8080,"appProtocol":"https"}]}})
        command(*kubectl,"-n",ns,"rollout","restart","deployment/backend-tls")
        command(*kubectl,"-n",ns,"rollout","status","deployment/backend-tls","--timeout=90s")
        apply(route("backend-tls",[rule("/backend-tls",backendRefs=[{"name":"backend-tls","port":443}])]))
        wait("HTTPS Service requires a usable TLS transport policy",lambda:request("/backend-tls")[0]==500)
        tls_policy={"apiVersion":"gateway.networking.k8s.io/v1","kind":"BackendTLSPolicy","metadata":{"name":"backend","namespace":ns},"spec":{"targetRefs":[{"group":"","kind":"Service","name":"backend-tls","sectionName":"https"}],"validation":{"hostname":"example.test","caCertificateRefs":[{"group":"","kind":"ConfigMap","name":"backend-ca"}]}}}
        apply(tls_policy)
        wait("BackendTLSPolicy validates private CA and hostname",lambda:request("/backend-tls")[0]==200 and condition("backendtlspolicy","backend","Accepted","True"))
        tls_policy["spec"]["validation"]["hostname"]="wrong.example.test";apply(tls_policy)
        wait("BackendTLS hostname mismatch rejects upstream connection",lambda:request("/backend-tls")[0]==502)
        tls_policy["spec"]["validation"]["hostname"]="example.test";apply(tls_policy)
        wait("Corrected BackendTLS hostname restores traffic",lambda:request("/backend-tls")[0]==200)
        apply({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"backend-ca","namespace":ns},"data":{"ca.crt":(security/"other-ca.crt").read_text()}})
        wait("Backend CA rotation invalidates previously trusted upstream pools",lambda:request("/backend-tls")[0]==502)
        apply({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"backend-ca","namespace":ns},"data":{"ca.crt":(security/"ca.crt").read_text()}})
        wait("Restored backend CA restores traffic",lambda:request("/backend-tls")[0]==200)
        command(*kubectl,"-n",ns,"delete","configmap","backend-ca")
        wait("Backend CA withdrawal fails closed with policy status",lambda:request("/backend-tls")[0]==500 and condition("backendtlspolicy","backend","Accepted","False"))

        def certificate(serial):
            key, cert = directory / f"{serial}.key", directory / f"{serial}.crt"
            command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(key), "-out", str(cert), "-days", "2", "-subj", "/CN=tls.example.test", "-addext", "subjectAltName=DNS:tls.example.test", "-set_serial", str(serial))
            apply({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "tls", "namespace": peer}, "type": "kubernetes.io/tls", "data": {"tls.crt": base64.b64encode(cert.read_bytes()).decode(), "tls.key": base64.b64encode(key.read_bytes()).decode()}})
            return hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text())).hexdigest()

        digest = certificate(1)
        listener = {"name": "https", "hostname": "tls.example.test", "port": 443, "protocol": "HTTPS", "tls": {"mode": "Terminate", "certificateRefs": [{"name": "tls", "namespace": peer}]}}
        command(*kubectl, "-n", ns, "patch", "gateway", "gateway", "--type=json", "-p", json.dumps([{"op": "add", "path": "/spec/listeners/-", "value": listener}]))
        apply(route("tls", [rule()], hosts=["tls.example.test"]))

        def tls_request():
            context = ssl._create_unverified_context()
            with socket.create_connection(("127.0.0.1", tls_port), timeout=3) as connection:
                with context.wrap_socket(connection, server_hostname="tls.example.test") as stream:
                    fingerprint = hashlib.sha256(stream.getpeercert(binary_form=True)).hexdigest()
                    client = http.client.HTTPConnection("tls.example.test")
                    client.sock = stream
                    client.request("GET", "/", headers={"Host": "tls.example.test"})
                    response = client.getresponse(); response.read()
                    return response.status, fingerprint

        def tls_revoked():
            try: tls_request(); return False
            except (ssl.SSLError, ConnectionError, OSError): return True

        wait("Cross-namespace TLS Secret requires ReferenceGrant", tls_revoked)
        apply({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "ReferenceGrant", "metadata": {"name": "tls", "namespace": peer}, "spec": {"from": [{"group": "gateway.networking.k8s.io", "kind": "Gateway", "namespace": ns}], "to": [{"group": "", "kind": "Secret", "name": "tls"}]}})
        wait("HTTPS SNI certificate and route", lambda: tls_request() == (200, digest))
        digest = certificate(2)
        wait("TLS Secret rotates without restarting the data plane", lambda: tls_request() == (200, digest))
        command(*kubectl, "-n", peer, "delete", "secret", "tls")
        wait("TLS Secret deletion revokes the SNI certificate", tls_revoked)

        apply({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "grpc", "namespace": ns}, "spec": {"replicas": 1, "selector": {"matchLabels": {"qa-origin": "grpc"}}, "template": {"metadata": {"labels": {"qa-origin": "grpc"}}, "spec": {"containers": [{"name": "origin", "image": args.grpc_image, "ports": [{"containerPort": 50051}], "volumeMounts": [{"name": "origin", "mountPath": "/app"}]}], "volumes": [{"name": "origin", "configMap": {"name": "origin"}}]}}}},
              {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "grpc", "namespace": ns}, "spec": {"selector": {"qa-origin": "grpc"}, "ports": [{"port": 50051, "targetPort": 50051}]}},
              {"apiVersion": "gateway.networking.k8s.io/v1", "kind": "GRPCRoute", "metadata": {"name": "grpc", "namespace": ns}, "spec": {"parentRefs": [{"name": "gateway"}], "hostnames": ["grpc.example.test"], "rules": [{"matches": [{"method": {"service": "qa.Echo"}}], "backendRefs": [{"name": "grpc", "port": 50051}]}]}})
        command(*kubectl, "-n", ns, "rollout", "status", "deployment/grpc", "--timeout=90s")
        wait("GRPCRoute status", lambda: condition("grpcroute", "grpc", "Accepted", "True"))
        client = "import grpc; c=grpc.insecure_channel('gateway:80',options=(('grpc.default_authority','grpc.example.test'),)); value,call=c.unary_unary('/qa.Echo/Unary').with_call(b'hello',timeout=5); assert value==b'hello' and ('x-trailer','received') in call.trailing_metadata(); assert list(c.stream_stream('/qa.Echo/Stream')(iter([b'a',b'b']),timeout=5))==[b'a',b'b']; print('ok')"
        wait("gRPC HTTP/2 unary, trailers and bidirectional stream", lambda: "ok" in command(*kubectl, "-n", ns, "exec", "deployment/grpc", "--", "python3", "-c", client))
        stream_pod=next(p for p in json.loads(command(*kubectl,"-n",ns,"get","pods","-l","app.kubernetes.io/name=rgnix,app.kubernetes.io/instance=gateway","-o","json"))["items"] if not p["metadata"].get("deletionTimestamp"))
        stream_probe="import grpc,time; c=grpc.insecure_channel("+repr(stream_pod["status"]["podIP"]+":8080")+",options=(('grpc.default_authority','grpc.example.test'),));\ndef messages():\n for i in range(22):\n  yield str(i).encode(); time.sleep(1)\nvalues=[]\nfor value in c.stream_stream('/qa.Echo/Stream')(messages(),timeout=35):\n values.append(value)\n if len(values)==1: print('stream active',flush=True)\nassert values==[str(i).encode() for i in range(22)];print('stream drained',flush=True)"
        with (directory/"grpc-drain.log").open("w+") as output:
            stream_process=subprocess.Popen([*kubectl,"-n",ns,"exec","deployment/grpc","--","python3","-c",stream_probe],stdout=output,stderr=subprocess.STDOUT)
            wait("gRPC stream is established before Pod termination", lambda:"stream active" in (directory/"grpc-drain.log").read_text(),seconds=15)
            command(*kubectl,"-n",ns,"delete","pod",stream_pod["metadata"]["name"],"--wait=false")
            status=stream_process.wait(timeout=40)
            output.seek(0); detail=output.read()
            assert status==0 and "stream drained" in detail, detail
        checks.append({"name":"In-flight gRPC HTTP/2 stream finishes after its controller Pod begins terminating","passed":True})
        command(*kubectl,"-n",ns,"rollout","status","deployment/gateway","--timeout=110s")
        reconnect(restart=False)
        wait("Replacement controller restores readiness after gRPC drain", lambda:request()[0]==200)
        command(*kubectl, "-n", ns, "patch", "service", "grpc", "--type=json", "-p", json.dumps([{"op": "add", "path": "/spec/ports/0/appProtocol", "value": "http"}]))
        apply(route("grpc-http", [{"backendRefs": [{"name": "grpc", "port": 50051}]}], hosts=["http-grpc.example.test"]))
        http_client = client.replace("grpc.example.test", "http-grpc.example.test")
        wait("HTTPRoute initially uses the declared HTTP/1 backend protocol", lambda: request(host="http-grpc.example.test")[0] in (502, 503))
        command(*kubectl, "-n", ns, "patch", "service", "grpc", "--type=json", "-p", json.dumps([{"op": "replace", "path": "/spec/ports/0/appProtocol", "value": "kubernetes.io/h2c"}]))
        wait("HTTPRoute honors Service h2c application protocol", lambda: "ok" in command(*kubectl, "-n", ns, "exec", "deployment/grpc", "--", "python3", "-c", http_client))
        command(*kubectl, "-n", ns, "patch", "service", "grpc", "--type=json", "-p", json.dumps([{"op": "replace", "path": "/spec/ports/0/appProtocol", "value": "https"}]))
        wait("Unsupported HTTPS backend cannot be downgraded to plaintext", lambda: condition("httproute", "grpc-http", "ResolvedRefs", "False") and request(host="http-grpc.example.test")[0] == 500)
        command(*kubectl, "-n", ns, "patch", "service", "grpc", "--type=json", "-p", json.dumps([{"op": "replace", "path": "/spec/ports/0/appProtocol", "value": "kubernetes.io/h2c"}]))
        namespace_policy["namespaces"]={peer:{"requests_per_second":1,"burst":2}}
        apply({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"namespace-policy","namespace":ns},"data":{"policy.json":json.dumps(namespace_policy)}})
        def quota_loaded(admin_port):
            return any(r.get("tenant",{}).get("namespace")==peer and r["tenant"]["quota"]["burst"]==2 for h in admin("/v1/config",target_port=admin_port)[1]["routes"] for r in h["routes"])
        with replicas() as pairs:
            for index,pair in enumerate(pairs):
                wait(f"Replica {index+1} loads administrator namespace quota",lambda:quota_loaded(pair[1]),seconds=180)
            statuses=[request(host="foreign.example.test",target_port=pairs[n%2][0])[0] for n in range(3)]
            assert statuses==[200,200,429],statuses
            checks.append({"name":"Namespace total request quota is shared across replicas","passed":True})
            assert request(target_port=pairs[0][0])[0]==200
            checks.append({"name":"Exhausted tenant does not consume another namespace budget","passed":True})
        command(*kubectl, "-n", ns, "rollout", "restart", "deployment/gateway")
        command(*kubectl, "-n", ns, "rollout", "status", "deployment/gateway", "--timeout=90s")
        checks.append({"name": "Two-replica rolling restart", "passed": True})
        # A clean fixture makes rejected resources from earlier negative checks irrelevant to the publication gate.
        for namespace in [ns, peer]:
            command(*kubectl, "-n", namespace, "delete", "httproutes,grpcroutes,backendtlspolicies", "--all", "--ignore-not-found")
        namespace_policy = {"default":{"allow_mirroring":True,"max_ingresses":3000,"max_routes":10000,"max_backends":4096,"requests_per_second":10000,"burst":10000}}
        apply({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"namespace-policy","namespace":ns},"data":{"policy.json":json.dumps(namespace_policy)}})
        digest = certificate(3)
        command(*kubectl, "-n", ns, "patch", "gateway", "gateway", "--type=merge", "-p", json.dumps({"spec":{"listeners":[values["gateway"]["listeners"][0],listener]}}))
        apply(route("base",[rule()]), route("tls",[rule()],hosts=["tls.example.test"]))
        reconnect()
        wait("Clean deployment converges after a rolling restart",lambda:admin("/v1/fleet",credential=operator)[1].get("converged"),seconds=180)

        expected_hash = admin("/v1/fleet",credential=operator)[1]["target"]["active_sha256"]
        result = command(*kubectl,"-n",ns,"exec","deployment/gateway","--","rgnix","wait","--admin-url","http://127.0.0.1:9090","--token-file","/etc/rgnix/admin/token","--expected-sha256",expected_hash,"--replicas","2","--timeout-seconds","30")
        assert json.loads(result)["converged"]
        try:
            command(*kubectl,"-n",ns,"exec","deployment/gateway","--","rgnix","wait","--admin-url","http://127.0.0.1:9090","--token-file","/etc/rgnix/admin/token","--expected-sha256","0"*64,"--timeout-seconds","1")
            raise AssertionError("unexpected digest passed the publication gate")
        except RuntimeError as error: assert "did not converge" in str(error),error
        checks.append({"name":"CLI publication gate requires the expected digest and replica count","passed":True})

        if args.soak_seconds:
            import concurrent.futures
            def plugin(version):
                return 'function on_request() req.set_header("x-release", "'+version+'") return route.pass() end function on_response() resp.set_header("x-release", "'+version+'") end'
            def publish_plugin(version):
                apply({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"production-plugin","namespace":ns},"data":{"main.rgl":plugin(version)}})
            publish_plugin("old")
            apply(route("production-plugin",[rule("/plugin")],annotations={"rgnix.io/script":"production-plugin/main.rgl","rgnix.io/request-body":"prefix 4k"}))
            apply(*[route(f"scale-{index}",[rule(f"/scale/{index}")]) for index in range(args.scale_routes)])
            wait("Scale configuration converges on every replica",lambda:admin("/v1/fleet",credential=operator)[1].get("converged") and request("/plugin")[2].get("headers",{}).get("x-release")=="old",seconds=180)
            with concurrent.futures.ThreadPoolExecutor(1) as pool:
                old = pool.submit(request,"/plugin",headers={"x-delay":"2"})
                time.sleep(0.3)
                publish_plugin("new")
                wait("New requests use the newly published plugin",lambda:request("/plugin")[1].get("x-release")=="new")
                assert old.result()[1].get("x-release")=="old"
            checks.append({"name":"In-flight requests retain their plugin snapshot across publication", "passed":True})
            load_script = r'''import concurrent.futures,http.client,json,socket,ssl,time
from collections import deque
host = "gateway.NAMESPACE.svc"
stop = time.monotonic()+SECONDS
payload = b"body-routing-soak"*256

def load(worker):
 samples=deque(maxlen=10000); errors=[]; completed=0; failed=0
 client=http.client.HTTPConnection(host,80,timeout=3)
 while time.monotonic()<stop:
  started=time.monotonic()
  try:
   if worker%3==0 and client.sock is None:
    raw=socket.create_connection((host,443),timeout=3)
    client.sock=ssl._create_unverified_context().wrap_socket(raw,server_hostname="tls.example.test")
   client.request("POST" if worker%3==2 else "GET", "/plugin" if worker%3 else "/", body=payload if worker%3==2 else None,headers={"Host":"tls.example.test" if worker%3==0 else "api.example.test"})
   response=client.getresponse(); body=response.read()
   assert response.status==200, f"HTTP {response.status}: {body[:120]!r}"
   data=json.loads(body)
   assert worker%3!=2 or data["body"]==payload.decode(), "request body changed"
   completed+=1; samples.append(time.monotonic()-started)
  except Exception as error:
   failed+=1
   if len(errors)<20: errors.append(str(error))
   client.close()
  time.sleep(max(0, 0.025-(time.monotonic()-started)))
 client.close()
 return {"requests":completed,"failed_requests":failed,"errors":errors,"samples":list(samples)}
with concurrent.futures.ThreadPoolExecutor(6) as pool: results=list(pool.map(load,range(6)))
samples=sorted(v for result in results for v in result.pop("samples"))
print(json.dumps({"workers":results,"requests":sum(r["requests"] for r in results),"failed_requests":sum(r["failed_requests"] for r in results),"retained_latency_samples":len(samples),"p99_ms":samples[min(len(samples)-1,int(len(samples)*0.99))]*1000 if samples else None}),flush=True)
assert all(r["failed_requests"]==0 and r["requests"]>0 for r in results)
'''.replace("NAMESPACE",ns).replace("SECONDS",str(args.soak_seconds))
            output_file=directory/"soak.log"
            with output_file.open("w+") as output:
                load=subprocess.Popen([*kubectl,"-n",ns,"exec","-i","deployment/a","--","python","-"],stdin=subprocess.PIPE,stdout=output,stderr=subprocess.STDOUT,text=True)
                load.stdin.write(load_script);load.stdin.close()
                time.sleep(min(3,args.soak_seconds/4))
                publish_plugin("soak")
                pods=json.loads(command(*kubectl,"-n",ns,"get","pods","-l","app.kubernetes.io/instance=gateway","-o","json"))["items"]
                command(*kubectl,"-n",ns,"delete","pod",pods[0]["metadata"]["name"],"--wait=false")
                status=load.wait(timeout=args.soak_seconds+30)
                output.seek(0); data=output.read()
                measurements=[json.loads(line) for line in data.splitlines() if line.startswith('{')]
                soak=measurements[-1] if measurements else {"error":data[-4096:]}
                checks.append({"name":"Keepalive HTTP/TLS/RGL/body/logs/traces load survives publication and Pod replacement","passed":status==0,"seconds":args.soak_seconds,"scale_routes":args.scale_routes,"connection_mode":"HTTP/1.1 keepalive; reconnect on Connection: close, no request replay; at most 40 requests/s per worker","measurement":soak})
                assert status==0,data
            reconnect()
            wait("Replacement replica acknowledges the accepted configuration",lambda:admin("/v1/fleet",credential=operator)[1].get("converged"),seconds=180)

        admission_key,admission_cert=directory/"admission.key",directory/"admission.crt"
        command("openssl","req","-x509","-newkey","rsa:2048","-nodes","-keyout",str(admission_key),"-out",str(admission_cert),"-days","2","-subj",f"/CN=gateway-admission.{ns}.svc","-addext",f"subjectAltName=DNS:gateway-admission.{ns}.svc")
        apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"admission-tls","namespace":ns},"type":"kubernetes.io/tls","stringData":{"tls.crt":admission_cert.read_text(),"tls.key":admission_key.read_text()}})
        admission_values=directory/"admission-values.json"
        admission_values.write_text(json.dumps({"admission":{"enabled":True,"register":False,"tlsSecret":"admission-tls","caBundle":base64.b64encode(admission_cert.read_bytes()).decode()}}))
        command("helm","upgrade","gateway",str(root/"charts/rgnix"),"--kube-context",args.context,"-n",ns,"--reuse-values","-f",str(admission_values))
        command(*kubectl,"-n",ns,"rollout","status","deployment/gateway","--timeout=110s")
        command("helm","upgrade","gateway",str(root/"charts/rgnix"),"--kube-context",args.context,"-n",ns,"--reuse-values","--set","admission.register=true","--wait","--timeout","110s")
        valid=route("admission-check",[rule("/admission-check")])
        wait("Gateway admission accepts server-side dry-run",lambda:command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(valid)))
        invalid=route("admission-invalid",[rule("/invalid")],annotations={"rgnix.io/unknown-policy":"deny"})
        try: command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(invalid));raise AssertionError("invalid policy was admitted")
        except RuntimeError as error: assert "denied the request" in str(error),str(error)
        checks.append({"name":"Gateway admission rejects unsupported policy before persistence","passed":True})
        def admission_ready():
            forbidden=route("admission-forged",[rule("/forged")],annotations={"rgnix.io/rolled-back-revision":"fake"})
            try: command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(forbidden)); return False
            except RuntimeError as error: return "controller-owned" in str(error)
        wait("Gateway admission protects controller-owned rollout state",admission_ready)
        unbound=route("unbound-forged",[rule()],annotations={"rgnix.io/rolled-back-revision":"fake"})
        unbound["spec"]["parentRefs"]=[{"name":"another-gateway"}]
        apply(unbound)
        unbound["spec"]["parentRefs"]=[{"name":"gateway"}]
        try: command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(unbound));raise AssertionError("forged state entered through ownership change")
        except RuntimeError as error: assert "controller-owned" in str(error),str(error)
        checks.append({"name":"Changing parentRefs cannot import forged controller rollout state","passed":True})
        command(*kubectl,"-n",ns,"delete","httproute","unbound-forged")

        webhook_name = ns+"-gateway"
        admission_service = get("service", "gateway-admission")
        selector = admission_service["spec"]["selector"]
        try:
            command(*kubectl,"-n",ns,"patch","service","gateway-admission","--type=json","-p",json.dumps([
                {"op":"replace","path":"/spec/selector","value":{"qa-no-pods":"true"}}]))
            def owned_unavailable():
                try: command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(valid)); return False
                except RuntimeError as error: return "failed calling webhook" in str(error)
            wait("Owned resources fail closed while admission is unavailable",owned_unavailable)
            foreign = route("foreign-admission", [rule()])
            foreign["spec"]["parentRefs"]=[{"name":"another-gateway"}]
            command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(foreign))
            foreign_gateway={"apiVersion":"gateway.networking.k8s.io/v1","kind":"Gateway","metadata":{"name":"another-gateway","namespace":ns},"spec":{"gatewayClassName":ns,"listeners":[{"name":"http","protocol":"HTTP","port":80}]}}
            command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(foreign_gateway))
            foreign_grpc={"apiVersion":"gateway.networking.k8s.io/v1","kind":"GRPCRoute","metadata":{"name":"foreign-grpc","namespace":ns},"spec":{"parentRefs":[{"name":"another-gateway"}],"rules":[{"backendRefs":[{"name":"grpc","port":50051}]}]}}
            command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(foreign_grpc))
            checks.append({"name":"Admission outage does not block another Gateway or its HTTPRoute/GRPCRoute","passed":True})
        finally:
            command(*kubectl,"-n",ns,"patch","service","gateway-admission","--type=json","-p",json.dumps([
                {"op":"replace","path":"/spec/selector","value":selector}]))
        wait("Admission recovers after its Service endpoints return",lambda:command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(valid)))

        controllers=json.loads(command(*kubectl,"-n",ns,"get","pods","-l","app.kubernetes.io/name=rgnix,app.kubernetes.io/instance=gateway","-o","json"))["items"]
        controllers=[pod for pod in controllers if not pod["metadata"].get("deletionTimestamp")]
        assert len(controllers)==2
        controller_uids={pod["metadata"]["uid"] for pod in controllers}
        addresses=[pod["status"]["podIP"] for pod in controllers]
        rotated_key,rotated_cert=directory/"rotated.key",directory/"rotated.crt"
        command("openssl","req","-x509","-newkey","rsa:2048","-nodes","-keyout",str(rotated_key),"-out",str(rotated_cert),"-days","2","-subj",f"/CN=gateway-admission.{ns}.svc","-addext",f"subjectAltName=DNS:gateway-admission.{ns}.svc")
        old_pem,new_pem=admission_cert.read_text(),rotated_cert.read_text()
        def trust(pem):
            command(*kubectl,"patch","validatingwebhookconfiguration",webhook_name,"--type=json","-p",json.dumps([
                {"op":"replace","path":"/webhooks/0/clientConfig/caBundle","value":base64.b64encode(pem.encode()).decode()}]))
        def serving_certificate(pem):
            expected=hashlib.sha256(ssl.PEM_cert_to_DER_cert(pem)).hexdigest()
            probe="import hashlib,socket,ssl; context=ssl.create_default_context(cadata="+repr(old_pem+new_pem)+"); "
            probe+="\nfor address in "+repr(addresses)+":\n with context.wrap_socket(socket.create_connection((address,9443),timeout=3),server_hostname="+repr(f"gateway-admission.{ns}.svc")+") as connection:\n  assert hashlib.sha256(connection.getpeercert(binary_form=True)).hexdigest()=="+repr(expected)+"\nprint('ok')"
            return "ok" in command(*kubectl,"-n",ns,"exec","deployment/a","--","python","-c",probe)
        trust(old_pem+new_pem)
        apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"admission-tls","namespace":ns},"type":"kubernetes.io/tls","stringData":{"tls.crt":new_pem,"tls.key":rotated_key.read_text()}})
        wait("Admission TLS rotates on every replica without restart",lambda:serving_certificate(new_pem),seconds=180)
        current=json.loads(command(*kubectl,"-n",ns,"get","pods","-l","app.kubernetes.io/name=rgnix,app.kubernetes.io/instance=gateway","-o","json"))["items"]
        assert {pod["metadata"]["uid"] for pod in current if not pod["metadata"].get("deletionTimestamp")}==controller_uids
        trust(new_pem)
        wait("Admission succeeds after removing the old CA",lambda:command(*kubectl,"apply","--dry-run=server","-f","-",data=json.dumps(valid)))
        apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"admission-tls","namespace":ns},"type":"kubernetes.io/tls","stringData":{"tls.crt":old_pem,"tls.key":rotated_key.read_text()}})
        def rejected_certificate():
            probe = "import urllib.request\nfor address in " + repr(addresses) + ":\n"
            probe += " metrics=urllib.request.urlopen('http://'+address+':9090/metrics').read().decode()\n"
            probe += " failures=next(float(line.split()[1]) for line in metrics.splitlines() if line.startswith('rgnix_admission_certificate_reload_errors_total '))\n"
            probe += " assert failures >= 1\nprint('ok')"
            return "ok" in command(*kubectl,"-n",ns,"exec","deployment/a","--","python","-c",probe) and serving_certificate(new_pem)
        wait("Invalid admission certificate update retains the accepted certificate",rejected_certificate,seconds=180)
        apply({"apiVersion":"v1","kind":"Secret","metadata":{"name":"admission-tls","namespace":ns},"type":"kubernetes.io/tls","stringData":{"tls.crt":new_pem,"tls.key":rotated_key.read_text()}})

    except BaseException as error:
        failure={"type":type(error).__name__,"message":str(error)[-4096:]}
        raise
    finally:
        forward.terminate()
        forward.wait(timeout=5)
        forward_log.close()
        pathlib.Path(args.output).write_text(json.dumps({"complete":failure is None,"failure":failure,"namespace":ns,"peer_namespace":peer,"image":args.image,"soak_seconds":args.soak_seconds,"scale_routes":args.scale_routes,"checks":checks,"scope":"Gateway, HTTPRoute and GRPCRoute live Kubernetes checks; upstream conformance certification is not claimed"}, indent=2) + "\n")
print(f"PASS {len(checks)} checks; namespace retained for inspection")

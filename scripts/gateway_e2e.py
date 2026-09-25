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
parser.add_argument("--grpc-image", default="rgnix:gateway-grpc-fixture")
args = parser.parse_args()
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


origin = '''import http.server,json,os,ssl
class Handler(http.server.BaseHTTPRequestHandler):
 protocol_version="HTTP/1.1"
 mirrors=0
 def do_GET(self):
  body=self.rfile.read(int(self.headers.get('Content-Length',0)))
  if self.headers.get('x-rgnix-mirror') == 'true': Handler.mirrors += 1
  if self.path == '/authorize':
   self.send_response(200 if self.headers.get('Authorization')=='Bearer approved' else 401);self.send_header('x-tenant','verified');self.send_header('Content-Length','0');self.end_headers();return
  out=json.dumps({'value':0,'mirrors':Handler.mirrors,'backend':os.getenv('BACKEND','a'),'path':self.path,'headers':dict(self.headers),'body':body.decode(errors='replace')}).encode()
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
    users["users"].append({"name":"namespace-writer","role":"writer","namespaces":[ns,peer],"token_sha256":hashlib.sha256(writer.encode()).hexdigest()})
    apply({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "admin-users", "namespace": ns}, "stringData": {"users.json": json.dumps(users)}})
    values = {"mode": "gateway", "gateway": {"name": "gateway", "className": ns, "listeners": [{"name": "http", "protocol": "HTTP", "port": 80, "allowedRoutes": {"namespaces": {"from": "All"}}}]}, "image": {"repository": repository, "tag": tag}, "replicaCount": 2, "watchNamespaces": [ns, peer], "service": {"type": "ClusterIP"}}
    values["admin"] = {"usersSecret": {"name": "admin-users", "key": "users.json"}}
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

    def reconnect():
        global forward
        if forward.poll() is None:
            forward.terminate(); forward.wait(timeout=5)
        command(*kubectl, "-n", ns, "rollout", "restart", "deployment/gateway")
        command(*kubectl, "-n", ns, "rollout", "status", "deployment/gateway", "--timeout=110s")
        forward = subprocess.Popen([*kubectl, "-n", ns, "port-forward", "deployment/gateway", f"{port}:8080", f"{tls_port}:8443", f"{admin_port}:9090"], stdout=forward_log, stderr=forward_log)

    def admin(path, data=None, target_port=None):
        req=urllib.request.Request(f"http://127.0.0.1:{target_port or admin_port}{path}", data=None if data is None else json.dumps(data).encode(), headers={"Authorization":f"Bearer {writer}","Content-Type":"application/json"})
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
                ports.append(pair)
            assert len(ports)==2
            yield ports
        finally:
            for process in forwards:
                if process.poll() is None: process.terminate()
                process.wait(timeout=5)

    try:
        wait("Gateway status reflects published generation", lambda: condition("gateway", "gateway", "Programmed", "True"))
        apply(route("base", [rule()]))
        wait("Service backend and Host preservation", lambda: request()[2].get("headers", {}).get("Host") == "api.example.test")
        wait("HTTPRoute status", lambda: condition("httproute", "base", "ResolvedRefs", "True"))
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
        reconnect()
        with replicas() as pairs:
            for index,pair in enumerate(pairs):
                wait(f"New replica {index+1} restores checkpoint while source is invalid", lambda: request("/script",target_port=pair[0])[2].get("backend") == "b")
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
                 "steps":[{"weights":{"a:80":90,"b:80":10},"duration_seconds":0,"min_requests":0,"approval":True},{"weights":{"a:80":0,"b:80":100},"duration_seconds":0,"min_requests":0,"approval":True}]}
        release=route("release",[rule("/release",backendRefs=[{"name":"a","port":80},{"name":"b","port":80}])],annotations={"rgnix.io/traffic-policy":json.dumps(traffic)})
        apply(release)
        def progress(): return json.loads(get("httproute","release")["metadata"]["annotations"].get("rgnix.io/rollout-state","{}"))
        wait("Gateway persists the initial rollout stage",lambda:progress().get("started_at",0)>0)
        counts=collections.Counter(request("/release")[2]["backend"] for _ in range(100))
        assert counts=={"a":90,"b":10},counts
        checks.append({"name":"Gateway staged 90/10 Service split","passed":True,"counts":dict(counts)})
        wait("Gateway mirror reaches its declared Service",lambda:request("/mirror-count")[2].get("mirrors",0)>0)
        assert admin("/v1/rollouts",{"kind":"HTTPRoute","owner":ns+"/release","revision":traffic["revision"],"operation":"approve","stage":0})[0]==200
        wait("Gateway approval advances persisted stage",lambda:progress().get("stage")==1)
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
    finally:
        forward.terminate()
        forward.wait(timeout=5)
        forward_log.close()
        pathlib.Path(args.output).write_text(json.dumps({"namespace": ns, "peer_namespace": peer, "image": args.image, "checks": checks, "scope": "Gateway, HTTPRoute and GRPCRoute live Kubernetes checks; upstream conformance certification is not claimed"}, indent=2) + "\n")
print(f"PASS {len(checks)} checks; namespace retained for inspection")

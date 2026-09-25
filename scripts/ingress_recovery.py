#!/usr/bin/env python3
"""Exercise checkpoint recovery, readiness and API stalls against a local Kubernetes API fixture."""
import base64
import copy
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import re
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import urlsplit, parse_qs
from integration import Upstream, certificate, free_port, request, wait_for, metric_value


def main():
    binary = str(Path(sys.argv[1]).resolve())
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    alternate = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    alternate.daemon_threads = True
    threading.Thread(target=alternate.serve_forever, daemon=True).start()
    versions = {"ingresses": ("networking.k8s.io/v1", "Ingress"),
                "ingressclasses": ("networking.k8s.io/v1", "IngressClass"),
                "services": ("v1", "Service"), "secrets": ("v1", "Secret"),
                "configmaps": ("v1", "ConfigMap"),
                "endpointslices": ("discovery.k8s.io/v1", "EndpointSlice")}
    objects = {kind: {} for kind in versions}
    listeners = []
    lock = threading.RLock()
    stopped = threading.Event()
    report_started = threading.Event()
    checkpoint_started = threading.Event()
    checkpoint_release = threading.Event()
    checkpoint_faults = {}
    report_delay = 0
    reject_events = False
    event_attempts = []
    leader_identity = "other"
    reject_status = False
    status_attempts = 0
    lease_writes = []
    status_get_faults = {}
    status_get_started = []
    revision = 0
    results = []

    def publish(kind, name, spec, namespace="qa", event="MODIFIED"):
        nonlocal revision
        with lock:
            revision += 1
            previous = objects[kind].get((namespace, name))
            value = copy.deepcopy(spec)
            version, singular = versions[kind]
            value.update(apiVersion=version, kind=singular)
            metadata = value.setdefault("metadata", {})
            metadata.update(name=name, namespace=namespace, resourceVersion=str(revision),
                            uid=metadata.get("uid", previous["metadata"]["uid"] if previous else f"uid-{revision}"))
            metadata.setdefault("creationTimestamp", "2026-09-23T00:00:00Z")
            if event == "DELETED":
                objects[kind].pop((namespace, name), None)
            else:
                objects[kind][(namespace, name)] = value
            for observed, pending in listeners:
                if observed == kind:
                    pending.put({"type": event, "object": copy.deepcopy(value)})
            return value

    script = 'function on_request() return route.pass() end function on_response() resp.set_header("x-version", "old") end'
    publish("ingressclasses", "rgnix", {"spec": {"controller": "rgnix.io/ingress-controller"}}, namespace="")
    plugin = publish("configmaps", "routes", {"data": {"main.rgl": script}})
    ingress = publish("ingresses", "app", {"metadata": {"annotations": {"rgnix.io/script": "routes/main.rgl"}},
        "spec": {"ingressClassName": "rgnix", "rules": [{"host": "example.test", "http": {"paths": [
            {"path": "/api", "pathType": "Prefix", "backend": {"service": {"name": "app", "port": {"name": "http"}}}}]}}]}})
    publish("services", "app", {"spec": {"ports": [{"name": "http", "port": 80, "targetPort": "http"}]}})
    endpoints = publish("endpointslices", "app-1", {"metadata": {"labels": {"kubernetes.io/service-name": "app"}},
        "addressType": "IPv4", "ports": [{"name": "http", "port": upstream.server_port, "protocol": "TCP"}],
        "endpoints": [{"addresses": ["127.0.0.1"], "conditions": {"ready": True}}]})

    class API(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def send_json(self, value, status=200):
            data = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            try:
                self.wfile.write(data)
            except OSError:
                pass

        def missing(self):
            self.send_json({"apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "NotFound", "message": "not found", "code": 404}, 404)

        def target(self):
            parts = urlsplit(self.path).path.split("/")
            kind = next((p for p in parts if p in versions), None)
            namespace = parts[parts.index("namespaces") + 1] if "namespaces" in parts else None
            name = parts[parts.index(kind) + 1] if kind and parts[-1] != kind else None
            return kind, namespace, name

        def do_GET(self):
            if "/leases/" in self.path:
                self.send_json({"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": "rgnix-leader", "resourceVersion": "1"},
                                "spec": {"holderIdentity": leader_identity, "renewTime": "2030-01-01T00:00:00Z", "leaseDurationSeconds": 30}})
                return
            kind, namespace, name = self.target()
            if not kind:
                self.missing()
                return
            query = parse_qs(urlsplit(self.path).query)
            if query.get("watch") == ["true"]:
                pending = queue.Queue()
                with lock:
                    listeners.append((kind, pending))
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                self.wfile.flush()
                try:
                    while not stopped.is_set():
                        try:
                            data = json.dumps(pending.get(timeout=.2)).encode() + b"\n"
                        except queue.Empty:
                            continue
                        self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
                        self.wfile.flush()
                except OSError:
                    pass
                finally:
                    with lock:
                        listeners.remove((kind, pending))
                return
            if kind == "ingresses" and name:
                fault = status_get_faults.get((namespace, name))
                if fault:
                    status_get_started.append(time.monotonic())
                    if fault == "stall":
                        stopped.wait(8)
                    else:
                        self.send_json({"apiVersion": "v1", "kind": "Status", "reason": "Forbidden", "message": "injected Ingress read failure", "code": 403}, 403)
                        return
            with lock:
                if name:
                    value = copy.deepcopy(objects[kind].get((namespace or "", name)))
                else:
                    items = [copy.deepcopy(v) for (ns, _), v in objects[kind].items() if namespace is None or ns == namespace]
                    selector = query.get("labelSelector", [""])[0]
                    if "=" in selector:
                        key, expected = selector.split("=", 1)
                        items = [v for v in items if v["metadata"].get("labels", {}).get(key) == expected]
                    version, singular = versions[kind]
                    value = {"apiVersion": version, "kind": singular + "List", "metadata": {"resourceVersion": str(revision)}, "items": items}
            self.send_json(value) if value else self.missing()

        def mutate(self, method):
            nonlocal status_attempts
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
            if urlsplit(self.path).path.endswith("/events"):
                status = 403 if reject_events and body["reason"] == "InvalidPlugin" else 201
                with lock:
                    event_attempts.append((body["reason"], status))
                if report_delay:
                    report_started.set()
                    stopped.wait(report_delay)
                if status == 403:
                    self.send_json({"apiVersion": "v1", "kind": "Status", "reason": "Forbidden", "message": "injected Event failure", "code": 403}, 403)
                else:
                    self.send_json(body, 201)
                return
            if "/leases/" in self.path:
                lease_writes.append(time.monotonic())
                self.send_json(body)
                return
            if urlsplit(self.path).path.endswith("/status"):
                with lock:
                    status_attempts += 1
                if reject_status:
                    self.send_json({"apiVersion": "v1", "kind": "Status", "reason": "Forbidden", "message": "injected status failure", "code": 403}, 403)
                else:
                    kind, namespace, name = self.target()
                    current = copy.deepcopy(objects[kind][(namespace, name)])
                    current["status"] = body["status"]
                    self.send_json(publish(kind, name, current, namespace))
                return
            kind, namespace, name = self.target()
            if kind != "configmaps":
                self.missing()
                return
            name = name or body["metadata"]["name"]
            if name.startswith("rgnix-state-") and method != "DELETE":
                checkpoint_started.set()
                checkpoint_release.wait(3)
                fault = checkpoint_faults.get(name)
                if fault:
                    if fault == "stall": stopped.wait(8)
                    self.send_json({"apiVersion": "v1", "kind": "Status", "reason": "Forbidden",
                                    "message": "injected checkpoint failure", "code": 403}, 403)
                    return
            with lock:
                previous = objects[kind].get((namespace, name))
                if method == "PUT" and previous and previous["metadata"]["resourceVersion"] != body["metadata"].get("resourceVersion"):
                    self.send_json({"apiVersion": "v1", "kind": "Status", "reason": "Conflict", "message": "conflict", "code": 409}, 409)
                    return
                if method == "DELETE":
                    if previous:
                        publish(kind, name, previous, namespace, "DELETED")
                    self.send_json({"apiVersion": "v1", "kind": "Status", "status": "Success"})
                else:
                    self.send_json(publish(kind, name, body, namespace), 201 if method == "POST" else 200)

        def do_POST(self): self.mutate("POST")
        def do_PUT(self): self.mutate("PUT")
        def do_PATCH(self): self.mutate("PATCH")
        def do_DELETE(self): self.mutate("DELETE")

    api = http.server.ThreadingHTTPServer(("127.0.0.1", 0), API)
    api.daemon_threads = True
    threading.Thread(target=api.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="rgnix-recovery-") as temp:
        directory = Path(temp)
        config = directory / "kubeconfig.json"
        config.write_text(json.dumps({"apiVersion": "v1", "kind": "Config", "clusters": [{"name": "mock", "cluster": {"server": f"http://127.0.0.1:{api.server_port}"}}],
            "contexts": [{"name": "mock", "context": {"cluster": "mock", "user": "review"}}], "current-context": "mock", "users": [{"name": "review", "user": {}}]}))
        processes, logs = [], []
        admin_token = os.urandom(32).hex()
        (directory / "admin.token").write_text(admin_token)

        def start(name, ready=200):
            port, tls, admin = free_port(), free_port(), free_port()
            log = (directory / f"{name}.log").open("w")
            logs.append(log)
            process = subprocess.Popen([binary, "ingress", "--identity", name, "--publish-service", "system/publish",
                "--http-listen", f"127.0.0.1:{port}", "--https-listen", f"127.0.0.1:{tls}", "--admin", f"127.0.0.1:{admin}",
                "--admin-token-file", str(directory / "admin.token")],
                stdout=log, stderr=log, env={**os.environ, "KUBECONFIG": str(config)})
            processes.append(process)
            wait_for(lambda: request(admin, "/readyz")[0], ready)
            wait_for(lambda: metric(admin, "rgnix_config_version") > 0)
            return port, admin, tls

        def metric(admin, name):
            match = re.search(r"^" + name + r" ([0-9.]+)$", request(admin, "/metrics")[2].decode(), re.M)
            return float(match[1]) if match else 0

        def check(label, value):
            assert value, label
            results.append(label)
            print("PASS", label, flush=True)

        try:
            first, admin, tls = start("first", ready=503)
            assert checkpoint_started.wait(3)
            check("initial configuration is not ready before durable acknowledgement", request(admin, "/readyz")[0] == 503 and request(first, "/api")[0] == 503)
            checkpoint_release.set()
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            wait_for(lambda: request(first, "/api")[1].get("x-version"), "old")
            wait_for(lambda: any(ns == "system" for ns, _ in objects["configmaps"]))
            check("accepted configuration is persisted outside the tenant namespace", True)
            metrics = request(admin, "/metrics")[2].decode()
            Path(".local/metrics-controller.prom").write_text(metrics)
            check("controller metrics expose synchronized watches and cached resources", all(metric_value(metrics, "rgnix_ingress_watch_streams", kind=kind, state=state) == 1 for _, kind in versions.values() for state in ("configured", "synchronized", "healthy")) and metric_value(metrics, "rgnix_ingress_cached_resources", kind="Ingress") == 1 and metric_value(metrics, "rgnix_ingress_selected_resources") == 1)
            wait_for(lambda: metric_value(request(admin, "/metrics")[2].decode(), "rgnix_ingress_lease_attempts_total", result="contended") > 0)
            check("follower metrics report Lease contention without claiming leadership", metric(admin, "rgnix_ingress_leader") == 0)
            with lock:
                for observed, pending in listeners:
                    if observed == "ingresses":
                        pending.put({"type": "ERROR", "object": {"apiVersion": "v1", "kind": "Status", "status": "Failure", "code": 410, "reason": "Expired", "message": "metrics recovery fixture"}})
            wait_for(lambda: metric_value(request(admin, "/metrics")[2].decode(), "rgnix_ingress_watch_errors_total", kind="Ingress") > 0)
            wait_for(lambda: metric_value(request(admin, "/metrics")[2].decode(), "rgnix_ingress_watch_events_total", kind="Ingress", event="init_done") > 1)
            check("watch failure and resynchronization are observable without losing ready routes", request(first, "/api")[0] == 200 and metric_value(request(admin, "/metrics")[2].decode(), "rgnix_ingress_watch_streams", kind="Ingress", state="healthy") == 1)
            checkpoint_release.clear()
            checkpoint_started.clear()
            script = script.replace('"old"', '"new"')
            plugin["data"]["main.rgl"] = script
            publish("configmaps", "routes", plugin)
            assert checkpoint_started.wait(3)
            check("pending checkpoint retains the previously served plugin", request(first, "/api")[1].get("x-version") == "old" and request(admin, "/readyz")[0] == 200)
            checkpoint_release.set()
            wait_for(lambda: request(first, "/api")[1].get("x-version"), "new")
            before_reports = metric(admin, "rgnix_report_errors_total")
            reject_events = True
            plugin["data"]["main.rgl"] = "invalid source"
            publish("configmaps", "routes", plugin)
            wait_for(lambda: metric(admin, "rgnix_reload_errors_total") > 0)
            wait_for(lambda: metric(admin, "rgnix_report_errors_total") > before_reports)
            check("Event API rejection is counted without withdrawing the accepted route", request(first, "/api")[1].get("x-version") == "new" and request(admin, "/readyz")[0] == 200)
            failed = event_attempts.count(("InvalidPlugin", 403))
            wait_for(lambda: event_attempts.count(("InvalidPlugin", 403)) > failed, timeout=12)
            check("unchanged failed Event is retried on the periodic report", True)
            reject_events = False
            wait_for(lambda: event_attempts.count(("InvalidPlugin", 201)) == 1, timeout=12)
            check("Event is delivered after permission recovery without a resource edit", True)
            time.sleep(10.5)
            check("successfully published Event is deduplicated", event_attempts.count(("InvalidPlugin", 201)) == 1)
            publisher = publish("services", "publish", {"spec": {"ports": [{"port": 80}]}, "status": {"loadBalancer": {"ingress": [{"ip": "192.0.2.10"}]}}}, namespace="system")
            before_reports = metric(admin, "rgnix_report_errors_total")
            leader_identity = "first"
            reject_status = True
            wait_for(lambda: status_attempts > 0, timeout=12)
            wait_for(lambda: metric(admin, "rgnix_report_errors_total") > before_reports)
            check("Ingress status API rejection is counted", True)
            reject_status = False
            wait_for(lambda: objects["ingresses"][("qa", "app")].get("status", {}).get("loadBalancer", {}).get("ingress") == [{"ip": "192.0.2.10"}], timeout=12)
            check("Ingress status is retried after permission recovery", True)
            metrics = request(admin, "/metrics")[2].decode()
            check("successful Lease claims are observable on the leader", metric_value(metrics, "rgnix_ingress_leader") == 1 and metric_value(metrics, "rgnix_ingress_lease_attempts_total", result="acquired") > 0)
            report_delay = 8
            report_started.clear()
            plugin["data"]["main.rgl"] = "?"
            publish("configmaps", "routes", plugin)
            assert report_started.wait(5)
            before_leases = len(lease_writes)
            before_reports = metric(admin, "rgnix_report_errors_total")
            publisher["status"]["loadBalancer"]["ingress"] = [{"ip": "192.0.2.11"}]
            publish("services", "publish", publisher, namespace="system")
            wait_for(lambda: objects["ingresses"][("qa", "app")].get("status", {}).get("loadBalancer", {}).get("ingress") == [{"ip": "192.0.2.11"}], timeout=12)
            check("stalled Events cannot block Ingress address updates", True)
            wait_for(lambda: len(lease_writes) > before_leases, timeout=12)
            wait_for(lambda: metric(admin, "rgnix_report_errors_total") > before_reports)
            check("Lease renews while Event timeouts remain observable", request(first, "/api")[0] == 200 and request(admin, "/readyz")[0] == 200)
            report_delay = 0
            status_only = copy.deepcopy(ingress)
            status_only["metadata"] = {}
            status_only["spec"]["rules"][0]["host"] = "status.other.test"
            status_only = publish("ingresses", "status-only", status_only, namespace="z-tenant")
            blocked = []
            for n in range(17):
                candidate = copy.deepcopy(ingress)
                candidate["metadata"] = {}
                candidate["spec"]["rules"][0]["host"] = f"blocked-{n}.test"
                name = f"a-stalled-{n:02d}"
                blocked.append((name, publish("ingresses", name, candidate)))

            def status_address(ns, name):
                return objects["ingresses"][(ns, name)].get("status", {}).get("loadBalancer", {}).get("ingress")

            wait_for(lambda: status_address("z-tenant", "status-only"), [{"ip": "192.0.2.11"}], timeout=15)
            wait_for(lambda: all(status_address("qa", name) == [{"ip": "192.0.2.11"}] for name, _ in blocked), timeout=15)
            for name, _ in blocked:
                status_get_faults[("qa", name)] = "stall"
            status_get_started.clear()
            publisher["status"]["loadBalancer"]["ingress"] = [{"ip": "192.0.2.12"}]
            publish("services", "publish", publisher, namespace="system")
            wait_for(lambda: len(status_get_started) > 0, timeout=12)
            batch_started = status_get_started[0]
            wait_for(lambda: status_address("z-tenant", "status-only"), [{"ip": "192.0.2.12"}], timeout=20)
            check("stalled Ingress reads cannot starve another namespace status", status_address("qa", blocked[0][0]) == [{"ip": "192.0.2.11"}])
            check("status batches renew the Lease while progressing past stalled entries", any(batch_started < renewed < time.monotonic() for renewed in lease_writes))
            status_get_faults.clear()
            for name, candidate in blocked:
                publish("ingresses", name, candidate, event="DELETED")
            status_get_faults[("qa", "app")] = "reject"
            publisher["status"]["loadBalancer"]["ingress"] = [{"ip": "192.0.2.13"}]
            publish("services", "publish", publisher, namespace="system")
            before_reports = metric(admin, "rgnix_report_errors_total")
            wait_for(lambda: status_address("z-tenant", "status-only"), [{"ip": "192.0.2.13"}], timeout=20)
            wait_for(lambda: metric(admin, "rgnix_report_errors_total") > before_reports)
            check("an Ingress read rejection does not cancel other status updates", True)
            status_get_faults.clear()
            wait_for(lambda: status_address("qa", "app"), [{"ip": "192.0.2.13"}], timeout=15)
            check("failed Ingress status catches up after API recovery", True)
            publish("ingresses", "status-only", status_only, namespace="z-tenant", event="DELETED")
            leader_identity = "other"
            ingress["spec"]["rules"][0]["http"]["paths"][0]["path"] = "/changed"
            publish("ingresses", "app", ingress)
            second, admin2, _ = start("second")
            check("fresh replica restores accepted plugin and route", request(second, "/api")[1].get("x-version") == "new" and request(second, "/changed")[0] == 404)
            time.sleep(.5)
            before = metric(admin2, "rgnix_config_version")
            publish("configmaps", "unrelated", {"data": {"ignored": "1"}}, namespace="elsewhere")
            ingress["status"] = {"loadBalancer": {"ingress": [{"ip": "192.0.2.1"}]}}
            publish("ingresses", "app", ingress)
            time.sleep(.5)
            check("unrelated and status-only updates do not publish snapshots", before == metric(admin2, "rgnix_config_version"))
            report_delay = 8
            report_started.clear()
            plugin["data"]["main.rgl"] = "@"
            publish("configmaps", "routes", plugin)
            assert report_started.wait(5)
            endpoints["endpoints"][0]["conditions"]["ready"] = False
            started = time.monotonic()
            publish("endpointslices", "app-1", endpoints)
            wait_for(lambda: request(second, "/api")[0], 503)
            check("endpoint withdrawal is independent of stalled Event writes", time.monotonic() - started < 2)
            report_delay = 0
            endpoints["endpoints"][0]["conditions"]["ready"] = True
            publish("endpointslices", "app-1", endpoints)
            wait_for(lambda: request(second, "/api")[0], 200)
            for (ns, name), value in list(objects["configmaps"].items()):
                if ns == "system": publish("configmaps", name, value, ns, "DELETED")
            third, admin3, _ = start("without-checkpoint", ready=503)
            check("fresh replica without a usable checkpoint stays unready", request(admin3, "/readyz")[0] == 503 and request(admin3, "/healthz")[0] == 200 and request(third, "/changed")[0] == 503)
            check("existing replicas continue serving accepted configuration", request(first, "/api")[0] == 200 and request(admin, "/readyz")[0] == 200)
            healthy = copy.deepcopy(ingress)
            healthy["metadata"] = {}
            healthy["spec"]["rules"][0]["http"]["paths"][0]["path"] = "/healthy"
            healthy = publish("ingresses", "healthy", healthy)
            wait_for(lambda: request(admin3, "/readyz")[0], 200)
            check("a rejected tenant does not withdraw another accepted route", request(third, "/healthy")[0] == 200 and request(third, "/changed")[0] == 503)
            publish("ingresses", "healthy", healthy, event="DELETED")
            plugin["data"]["main.rgl"] = script
            publish("configmaps", "routes", plugin)
            wait_for(lambda: request(admin3, "/readyz")[0], 200)
            wait_for(lambda: request(third, "/changed")[0], 200)
            check("valid replacement unblocks initial readiness", request(third, "/changed")[0] == 200)
            publish("ingresses", "app", ingress, event="DELETED")
            wait_for(lambda: request(third, "/changed")[0], 404)
            wait_for(lambda: not any(ns == "system" for ns, _ in objects["configmaps"]))
            check("Ingress deletion removes routing and its checkpoint", True)

            def tenant(host, uid):
                value = copy.deepcopy(ingress)
                value["metadata"] = {"uid": uid, "annotations": {"rgnix.io/script": "routes/main.rgl"}}
                value["spec"]["rules"][0]["host"] = host
                value["spec"]["rules"][0]["http"]["paths"][0]["path"] = "/api"
                return value

            for index, fault in enumerate(("oversize", "reject", "stall"), 1):
                bad = tenant("bad.example.test", f"00000000-0000-0000-0000-{index:012d}")
                good = tenant("healthy.example.test", f"ffffffff-ffff-ffff-ffff-{index:012d}")
                key = "rgnix-state-" + hashlib.sha256(f"rgnix/{bad['metadata']['uid']}".encode()).hexdigest()[:32]
                if fault == "oversize":
                    route = bad["spec"]["rules"][0]["http"]["paths"][0]
                    bad["spec"]["rules"][0]["http"]["paths"] = [
                        {**route, "path": "/api/" + "a" * 850 + str(n)} for n in range(1024)]
                    assert 900 * 1024 < len(json.dumps(bad)) < 1500 * 1024
                else:
                    checkpoint_faults[key] = fault
                publish("ingresses", "bad", bad)
                publish("ingresses", "healthy", good)
                wait_for(lambda: request(first, "/api", headers={"Host": "healthy.example.test"})[1].get("x-version"), "new", timeout=3)
                check(f"{fault} checkpoint cannot block another tenant publication", True)
                publish("ingresses", "bad", bad, event="DELETED")
                publish("ingresses", "healthy", good, event="DELETED")
                checkpoint_faults.pop(key, None)
                wait_for(lambda: not any(ns == "system" for ns, _ in objects["configmaps"]))

            def tls_secret(serial):
                certificate(directory, serial, ("tls.example.test", "*.example.test"))
                return {"type": "kubernetes.io/tls", "data": {
                    "tls.crt": base64.b64encode((directory / "cert.pem").read_bytes()).decode(),
                    "tls.key": base64.b64encode((directory / "key.pem").read_bytes()).decode()}}

            def peer(host):
                try:
                    with socket.create_connection(("127.0.0.1", tls), timeout=2) as connection:
                        with ssl._create_unverified_context().wrap_socket(connection, server_hostname=host) as secured:
                            return secured.getpeercert(binary_form=True)
                except ssl.SSLError:
                    return None

            primary = publish("secrets", "primary", tls_secret(30))
            primary_der = ssl.PEM_cert_to_DER_cert(base64.b64decode(primary["data"]["tls.crt"]).decode())
            secondary = publish("secrets", "secondary", tls_secret(31), namespace="other")
            secondary_der = ssl.PEM_cert_to_DER_cert(base64.b64decode(secondary["data"]["tls.crt"]).decode())
            owner = tenant("tls.example.test", "tls-owner")
            owner["metadata"] = {"creationTimestamp": "2026-09-22T00:00:00Z"}
            owner["spec"]["tls"] = [{"hosts": ["tls.example.test"], "secretName": "primary"}]
            owner = publish("ingresses", "tls-owner", owner)
            rival = copy.deepcopy(owner)
            rival["metadata"] = {"creationTimestamp": "2026-09-23T00:00:00Z"}
            rival["spec"]["tls"] = [{"hosts": ["tls.example.test", "*.example.test"], "secretName": "secondary"}]
            publish("ingresses", "tls-rival", rival, namespace="other")
            wait_for(lambda: peer("tls.example.test"), primary_der)
            wait_for(lambda: peer("other.example.test"), None)
            check("earlier namespace owns its TLS hostname and rejects an overlapping wildcard claim", True)
            publish("secrets", "primary", primary, event="DELETED")
            wait_for(lambda: peer("tls.example.test"), None)
            check("withdrawn TLS claim cannot fall back to an unauthorized namespace certificate", peer("other.example.test") is None)
            invalid = copy.deepcopy(primary)
            invalid["data"]["tls.crt"] = base64.b64encode(b"invalid PEM").decode()
            before = metric(admin, "rgnix_config_version")
            publish("secrets", "primary", invalid)
            wait_for(lambda: metric(admin, "rgnix_config_version") > before)
            check("invalid certificate retains exclusive TLS ownership", peer("tls.example.test") is None)
            publish("secrets", "primary", primary)
            wait_for(lambda: peer("tls.example.test"), primary_der)
            check("valid replacement restores the owned certificate", True)
            publish("ingresses", "tls-owner", owner, event="DELETED")
            wait_for(lambda: peer("tls.example.test"), secondary_der)
            check("deleting the owning Ingress releases TLS ownership", True)

            def backend(name):
                return {"service": {"name": name, "port": {"name": "http"}}}

            def routed_port(host, path="/"):
                status, _, body = request(first, path, headers={"Host": host})
                return json.loads(body)["port"] if status == 200 else None

            service_spec = {"ports": [{"name": "http", "port": 80, "targetPort": "http"}]}
            publish("services", "alternate", {"spec": service_spec})
            publish("endpointslices", "alternate-1", {"metadata": {"labels": {"kubernetes.io/service-name": "alternate"}},
                "addressType": "IPv4", "ports": [{"name": "http", "port": alternate.server_port, "protocol": "TCP"}],
                "endpoints": [{"addresses": ["127.0.0.1"], "conditions": {"ready": True}}]})
            rule = {"http": {"paths": [{"path": "/", "pathType": "Prefix", "backend": backend("alternate")}]}}
            fallback = publish("ingresses", "fallback", {"metadata": {"creationTimestamp": "2026-09-22T00:00:00Z"},
                "spec": {"ingressClassName": "rgnix", "defaultBackend": backend("app"), "rules": [rule]}})
            wait_for(lambda: routed_port("fallback.test"), alternate.server_port)
            check("hostless root rule takes precedence over its default backend", routed_port("fallback.test", "/child") == alternate.server_port)
            del fallback["spec"]["rules"]
            fallback = publish("ingresses", "fallback", fallback)
            wait_for(lambda: routed_port("fallback.test", "/child"), upstream.server_port)
            explicit = publish("ingresses", "explicit", {"spec": {"ingressClassName": "rgnix", "rules": [rule]}})
            wait_for(lambda: routed_port("fallback.test", "/child"), alternate.server_port)
            check("explicit root rule overrides an older Ingress default backend", routed_port("fallback.test", "/child") == alternate.server_port)
            explicit["spec"]["rules"][0]["http"]["paths"][0]["path"] = "/known"
            publish("ingresses", "explicit", explicit)
            wait_for(lambda: routed_port("fallback.test", "/unknown"), upstream.server_port)
            check("default backend applies only after explicit paths miss", routed_port("fallback.test", "/known") == alternate.server_port)
            publish("ingresses", "fallback", fallback, event="DELETED")
            publish("ingresses", "explicit", explicit, event="DELETED")

            owned_service = publish("services", "owned", {"spec": service_spec})
            owner_reference = {"apiVersion": "v1", "kind": "Service", "name": "owned", "uid": owned_service["metadata"]["uid"], "controller": True}
            owned_slice = publish("endpointslices", "owned-1", {"metadata": {"labels": {"kubernetes.io/service-name": "owned"}, "ownerReferences": [owner_reference]},
                "addressType": "IPv4", "ports": [{"name": "http", "port": upstream.server_port, "protocol": "TCP"}],
                "endpoints": [{"addresses": ["127.0.0.1"], "conditions": {"ready": True}}]})
            publish("ingresses", "owned", {"spec": {"ingressClassName": "rgnix", "rules": [{"host": "owned.test", "http": {"paths": [
                {"path": "/", "pathType": "Prefix", "backend": backend("owned")}]}}]}})
            wait_for(lambda: routed_port("owned.test"), upstream.server_port)
            publish("services", "owned", owned_service, event="DELETED")
            wait_for(lambda: request(first, "/", headers={"Host": "owned.test"})[0], 503)
            before = metric(admin, "rgnix_config_version")
            replacement = publish("services", "owned", {"spec": service_spec})
            assert replacement["metadata"]["uid"] != owner_reference["uid"]
            wait_for(lambda: metric(admin, "rgnix_config_version") > before)
            check("recreated Service rejects slices owned by its previous UID", request(first, "/", headers={"Host": "owned.test"})[0] == 503)
            owned_slice["metadata"]["ownerReferences"][0]["uid"] = replacement["metadata"]["uid"]
            owned_slice = publish("endpointslices", "owned-1", owned_slice)
            wait_for(lambda: routed_port("owned.test"), upstream.server_port)
            check("owner-only EndpointSlice updates restore a matching Service", True)
            owned_slice["metadata"]["ownerReferences"][0]["uid"] = owner_reference["uid"]
            owned_slice = publish("endpointslices", "owned-1", owned_slice)
            wait_for(lambda: request(first, "/", headers={"Host": "owned.test"})[0], 503)
            replacement_slice = copy.deepcopy(owned_slice)
            replacement_slice["metadata"]["ownerReferences"][0]["uid"] = replacement["metadata"]["uid"]
            replacement_slice["ports"][0]["port"] = alternate.server_port
            replacement_slice = publish("endpointslices", "owned-new", replacement_slice)
            wait_for(lambda: routed_port("owned.test"), alternate.server_port)
            check("stale and current slices never mix after Service recreation", all(routed_port("owned.test") == alternate.server_port for _ in range(6)))
            replacement_slice["metadata"]["ownerReferences"][0]["name"] = "different-service"
            replacement_slice = publish("endpointslices", "owned-new", replacement_slice)
            wait_for(lambda: request(first, "/", headers={"Host": "owned.test"})[0], 503)
            check("Service owner name must agree with the slice label", True)
            del replacement_slice["metadata"]["ownerReferences"]
            publish("endpointslices", "owned-new", replacement_slice)
            wait_for(lambda: routed_port("owned.test"), alternate.server_port)
            check("manually managed slices without a Service owner remain supported", True)
            publish("services", "body-canary", {"spec": {"ports": [{"name": "http", "port": 80, "targetPort": "http"}]}})
            body_endpoints = copy.deepcopy(endpoints)
            body_endpoints["metadata"] = {"labels": {"kubernetes.io/service-name": "body-canary"}}
            body_endpoints["ports"][0]["port"] = alternate.server_port
            body_endpoints["endpoints"][0]["conditions"]["ready"] = True
            body_endpoints = publish("endpointslices", "body-canary", body_endpoints)
            body_script = '''function on_request()
if req.json_string("/tenant") == "vip" or req.body_contains("route=vip;") then return route.proxy("body-canary:http") end
return route.pass() end'''
            body_plugin = publish("configmaps", "body-routes", {"data": {"main.rgl": body_script}})
            body_ingress = publish("ingresses", "body", {"metadata": {"annotations": {
                "rgnix.io/script": "body-routes/main.rgl", "rgnix.io/request-body": "full 64k", "rgnix.io/request-body-timeout": "1s"}},
                "spec": {"ingressClassName": "rgnix", "rules": [{"host": "body.test", "http": {"paths": [
                    {"path": "/", "pathType": "Prefix", "backend": {"service": {"name": "app", "port": {"name": "http"}}}},
                    {"path": "/canary", "pathType": "Prefix", "backend": {"service": {"name": "body-canary", "port": {"name": "http"}}}}]}}]}})
            def body_route(port=first, data=b'{"tenant":"vip"}'):
                status, _, response = request(port, "/", "POST", {"Host": "body.test"}, data)
                if status != 200:
                    return status
                value = json.loads(response)
                assert value["size"] == len(data) and value["sha256"] == hashlib.sha256(data).hexdigest()
                return value["port"]
            wait_for(body_route, alternate.server_port)
            check("Ingress JSON body annotation routes to a declared Service and preserves bytes", True)
            body_ingress["metadata"]["annotations"]["rgnix.io/request-body"] = "prefix 16"
            publish("ingresses", "body", body_ingress)
            large_body = b"route=vip;" + b"x" * 200000
            wait_for(lambda: body_route(data=large_body), alternate.server_port)
            check("annotation-only prefix update streams the full upload", True)
            body_ingress["metadata"]["annotations"]["rgnix.io/request-body"] = "prefix 0"
            before_errors = metric(admin, "rgnix_reload_errors_total")
            publish("ingresses", "body", body_ingress)
            wait_for(lambda: metric(admin, "rgnix_reload_errors_total") > before_errors)
            check("invalid body policy retains the accepted policy and plugin", body_route(data=large_body) == alternate.server_port)
            body_replica, _, _ = start("body-recovery")
            check("fresh replica restores the body policy with its accepted plugin", body_route(body_replica, large_body) == alternate.server_port)
            body_plugin["data"]["main.rgl"] = "broken plugin"
            publish("configmaps", "body-routes", body_plugin)
            body_endpoints["endpoints"][0]["conditions"]["ready"] = False
            publish("endpointslices", "body-canary", body_endpoints)
            wait_for(lambda: body_route(data=large_body), 503)
            check("bad plugin and body policy cannot freeze endpoint withdrawal", True)
            publish("ingresses", "body", body_ingress, event="DELETED")
            wait_for(lambda: body_route(data=large_body), 404)
            check("body-inspected Ingress deletion takes effect despite invalid updates", True)
            publish("services", "policy-app", {"spec": {"ports": [{"name": "http", "port": 80, "targetPort": "http"}]}})
            policy_slice = publish("endpointslices", "policy-app", {"metadata": {"labels": {"kubernetes.io/service-name": "policy-app"}},
                "addressType": "IPv4", "ports": [{"name": "http", "port": upstream.server_port, "protocol": "TCP"}],
                "endpoints": [{"addresses": ["127.0.0.1"], "conditions": {"ready": True}}]})
            policy_ingress = publish("ingresses", "policy", {"metadata": {"annotations": {
                "rgnix.io/client-max-body-size": "4m", "rgnix.io/proxy-read-timeout": "100ms", "rgnix.io/access-log": "off"}},
                "spec": {"ingressClassName": "rgnix", "rules": [{"host": "policy.test", "http": {"paths": [
                    {"path": "/", "pathType": "Prefix", "backend": {"service": {"name": "policy-app", "port": {"number": 80}}}}]}}]}})
            wait_for(lambda: request(first, "/", headers={"Host": "policy.test"})[0], 200)
            upload = b"x" * (2 * 1024 * 1024)
            check("Ingress body size policy works without a routing plugin", request(first, "/", "POST", {"Host": "policy.test"}, upload)[0] == 200)
            check("Ingress route timeout is configurable", request(first, "/slow", headers={"Host": "policy.test"})[0] == 504)
            policy_ingress["metadata"]["annotations"]["rgnix.io/client-max-body-size"] = "invalid"
            before_errors = metric(admin, "rgnix_reload_errors_total")
            publish("ingresses", "policy", policy_ingress)
            wait_for(lambda: metric(admin, "rgnix_reload_errors_total") > before_errors)
            check("invalid policy without a plugin retains its accepted configuration", request(first, "/", "POST", {"Host": "policy.test"}, upload)[0] == 200)
            policy_replica, _, _ = start("policy-recovery")
            check("new replica restores a policy-only checkpoint", request(policy_replica, "/", "POST", {"Host": "policy.test"}, upload)[0] == 200)
            policy_slice["endpoints"][0]["conditions"]["ready"] = False
            publish("endpointslices", "policy-app", policy_slice)
            wait_for(lambda: request(first, "/", headers={"Host": "policy.test"})[0], 503)
            check("invalid policy cannot freeze live endpoint withdrawal", True)
            publish("ingresses", "policy", policy_ingress, event="DELETED")
            wait_for(lambda: request(first, "/", headers={"Host": "policy.test"})[0], 404)
            check("deleting policy-only Ingress removes its route", True)
            candidate = {"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"preflight","namespace":"qa"},
                "spec":{"ingressClassName":"rgnix","rules":[{"host":"preflight.test","http":{"paths":[{"path":"/","pathType":"Prefix","backend":backend("policy-app")}]}}]}}
            auth = {"Authorization":"Bearer " + admin_token}
            preview = request(admin, "/v1/validate-ingress", "POST", auth, json.dumps(candidate))
            check("Ingress preflight warns about unavailable endpoints without publishing a candidate", preview[0] == 200 and json.loads(preview[2])["valid"] and request(first, "/", headers={"Host":"preflight.test"})[0] == 404)
            before = metric(admin, "rgnix_config_version")
            publish("ingressclasses", "rgnix", {"spec":{"controller":"other.example/controller"}}, namespace="")
            wait_for(lambda: metric(admin, "rgnix_config_version") > before)
            check("Ingress preflight rejects a class reassigned to another controller", request(admin, "/v1/validate-ingress", "POST", auth, json.dumps(candidate))[0] == 400)
        except BaseException:
            for path in directory.glob("*.log"):
                print(path.name, path.read_text()[-7000:], file=sys.stderr)
            raise
        finally:
            stopped.set()
            checkpoint_release.set()
            for process in processes: process.terminate()
            for process in processes: process.wait(timeout=35)
            for log in logs: log.close()
            api.shutdown()
            upstream.shutdown()
            alternate.shutdown()
    output = Path(".local/ingress-recovery.json")
    output.parent.mkdir(exist_ok=True)
    output.write_text(json.dumps({"passed": len(results), "checks": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()

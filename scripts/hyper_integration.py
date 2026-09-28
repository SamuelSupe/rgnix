#!/usr/bin/env python3
"""Shared HTTP/1 policy and lifecycle checks for the experimental product transport."""
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

from integration import Upstream, check, free_port, metric_value, request, wait_for, RESULTS
from product_features import wire_fields


class Collector(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        self.server.records.append((self.path, self.rfile.read(int(self.headers["Content-Length"]))))
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()


class Origin(Upstream):
    release = threading.Event()
    entered = threading.Event()
    attempts = 0

    def do_GET(self):
        if self.path == "/no-read":
            time.sleep(1)
            self.close_connection = True
            return
        if self.path in ["/hold", "/stall"]:
            self.send_response(200)
            self.send_header("Content-Length", "8195" if self.path == "/hold" else "6")
            self.end_headers()
            self.entered.set()
            self.wfile.write(b"x" * 8192 if self.path == "/hold" else b"one")
            self.wfile.flush()
            if self.path == "/hold":
                self.release.wait(5)
            else:
                time.sleep(0.5)
            try:
                self.wfile.write(b"two")
            except (BrokenPipeError, ConnectionResetError):
                pass
        else:
            if self.path == "/commit":
                type(self).attempts += 1
            super().do_GET()


def main():
    binary = str(Path(sys.argv[1]).resolve())
    hyper = "--pingora" not in sys.argv[2:]
    origin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    collector = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Collector)
    collector.records = []
    threading.Thread(target=collector.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="rgnix-hyper-product-") as temp:
        directory = Path(temp)
        port, admin = free_port(), free_port()
        access = directory / "access.jsonl"
        config = directory / "nginx.conf"
        def configuration(marker="old", extra=""):
            return f'''events {{}} http {{
access_log {access} json; keepalive_timeout 1s;
upstream app {{ server 127.0.0.1:{origin.server_port}; rgnix_max_inflight 2; }}
upstream tight {{ server 127.0.0.1:{origin.server_port}; rgnix_max_inflight 1; }}
server {{ listen 127.0.0.1:{port}; server_name example.test;
proxy_read_timeout 2s; proxy_send_timeout 2s;
add_header X-Snapshot {marker} always;
location = /exact {{ return 200 exact; }}
location = /head {{ return 200 abcdef; }}
location = /same {{ return 200 exact-same; }}
location /same {{ proxy_pass http://app; proxy_set_header X-Prepared {marker}; proxy_set_header X-Remove ""; }}
location /rewrite/ {{ proxy_pass http://app/base/; proxy_set_header X-Seen $request_uri; }}
location /raw/ {{ proxy_pass http://app; }}
location /small {{ client_max_body_size 16; proxy_pass http://app; }}
location /large {{ client_max_body_size 16m; proxy_pass http://app; }}
location /slow {{ proxy_read_timeout 100ms; proxy_pass http://app; }}
location /upload-progress {{ proxy_read_timeout 250ms; proxy_pass http://app; }}
location /stall {{ proxy_read_timeout 100ms; proxy_pass http://app; }}
location /limited {{ rgnix_limit_rate 1 burst=1 key=route; return 200 limited; }}
location /hold {{ rgnix_limit_conn 1 key=route; proxy_pass http://app; }}
location /budget-hold {{ proxy_pass http://app/hold; }}
location /backend-hold {{ proxy_pass http://tight/hold; }}
location /no-read {{ client_max_body_size 64m; proxy_send_timeout 100ms; proxy_pass http://app; }}
location / {{ proxy_pass http://app; }}
{extra}
}}
server {{ listen 127.0.0.1:{port}; server_name other.test; location / {{ return 200 other; }} }}
}}'''
        token = directory / "admin.token"
        token.write_text("hyper-integration-private-token-20260928")
        auth = {"Authorization": "Bearer " + token.read_text()}
        config.write_text(configuration())
        log = (directory / "process.log").open("w+")
        env = {k: v for k, v in os.environ.items() if not k.startswith("OTEL_")}
        env.update({"OTEL_BSP_SCHEDULE_DELAY": "40", "OTEL_BLRP_SCHEDULE_DELAY": "40"})
        command = [binary, "serve", "-c", str(config), "--admin", f"127.0.0.1:{admin}",
                   "--admin-token-file", str(token),
                   "--threads", "1", "--upstream-max-fails", "0", "--max-inflight", "2", "--max-plugin-instances", "1",
                   "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "3",
                   "--otlp-logs-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/logs",
                   "--otlp-traces-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/traces",
                   "--trace-sample-ratio", "0"]
        if hyper:
            command.append("--experimental-hyper")
        process = subprocess.Popen(command, stdout=log, stderr=log, env=env)
        metrics = lambda: request(admin, "/metrics")[2].decode()
        budget = lambda: metric_value(metrics(), "rgnix_budget_in_use", budget="inflight")
        def held(path):
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", path, headers={"Host": "example.test"})
            response = conn.getresponse()
            assert response.status == 200 and response.read(8192) == b"x" * 8192
            return conn, response

        def exported(path):
            records = []
            for route, payload in collector.records:
                if route == path:
                    for resource in wire_fields(payload).get(1, []):
                        for scope in wire_fields(resource).get(2, []):
                            records.extend(wire_fields(record) for record in wire_fields(scope).get(2, []))
            return records
        try:
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            initial_version = json.loads(request(admin, "/v1/config", headers=auth)[2])["version"]
            check("Connection cannot strip routing or framing headers", all(request(port, "/exact", headers={"Connection": name})[0] == 400 for name in ["host", "content-length", "transfer-encoding"]))
            status, headers, data = request(port, "/exact")
            check("exact route and configured response header", status == 200 and headers["x-snapshot"] == "old" and data == b"exact")
            status, _, data = request(port, "/same/child", headers={"X-Remove": "discard"})
            prepared_headers = {k.lower(): v for k, v in json.loads(data)["headers"].items()}
            check("prepared exact and prefix routes with the same path stay distinct",
                  request(port, "/same")[2] == b"exact-same" and status == 200
                  and prepared_headers["x-prepared"] == "old" and "x-remove" not in prepared_headers)
            check("virtual host selects its route", request(port, "/", headers={"Host": "other.test"})[2] == b"other")
            response = request(port, "/rewrite/a%20b?q=x")
            value = json.loads(response[2]); headers = {k.lower(): v for k, v in value["headers"].items()}
            check("URI replacement preserves query and variable expansion", value["path"] == "/base/a%20b?q=x" and headers["x-seen"] == "/rewrite/a%20b?q=x")
            check("proxy_pass without URI preserves raw target", json.loads(request(port, "/raw/a%2Fb?q=%2F")[2])["path"] == "/raw/a%2Fb?q=%2F")
            check("prefix trailing slash redirect", request(port, "/rewrite")[0:2][0] == 301 and request(port, "/rewrite")[1]["location"] == "/rewrite/")
            status, headers, data = request(port, "/head", "HEAD")
            check("HEAD preserves response length without body", status == 200 and headers["content-length"] == "6" and not data)
            check("fixed-length body limit", request(port, "/small", "POST", body=b"x" * 17)[0] == 413)
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("POST", "/small", body=iter([b"x" * 17]), headers={"Host": "example.test"}, encode_chunked=True)
            response = conn.getresponse(); response.read(); conn.close()
            check("chunked body limit", response.status == 413)
            data = bytes(range(256)) * 32768
            status, _, result = request(port, "/large", "POST", body=data)
            check("8 MiB streaming POST is intact", status == 200 and json.loads(result)["sha256"] == hashlib.sha256(data).hexdigest())
            status, _, result = request(port, "/", headers={"traceparent": "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"})
            propagated = {k.lower(): v for k,v in json.loads(result)["headers"].items()}["traceparent"]
            wait_for(lambda: len(exported("/v1/traces")), 2)
            spans = exported("/v1/traces")
            server = next(span for span in spans if span[6] == [2])
            client = next(span for span in spans if span[6] == [3])
            check("OTLP exports linked server and client spans with the inherited parent", server[1] == client[1] == [bytes.fromhex("0123456789abcdef" * 2)] and server[4] == [bytes.fromhex("0123456789abcdef")] and client[4] == server[2])
            check("W3C propagation uses the exported client span", status == 200 and propagated == f"00-0123456789abcdef0123456789abcdef-{client[2][0].hex()}-01")
            wait_for(lambda: any(r.get(9) == server[1] and r.get(10) == server[2] for r in exported("/v1/logs")), True)
            check("OTLP access logs correlate with the server span", True)
            def paced_upload():
                for _ in range(8):
                    yield b"x" * 256
                    time.sleep(0.08)
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("POST", "/upload-progress", body=paced_upload(), headers={"Host": "example.test"}, encode_chunked=True)
            response = conn.getresponse(); payload = response.read(); conn.close()
            check("ongoing upload progress extends the upstream read deadline", response.status == 200 and json.loads(payload)["sha256"] == hashlib.sha256(b"x" * 2048).hexdigest())
            check("upstream header timeout", request(port, "/slow")[0] == 504)
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
            conn.request("GET", "/stall", headers={"Host": "example.test"})
            response = conn.getresponse()
            failed = False
            try:
                response.read()
            except http.client.IncompleteRead:
                failed = True
            conn.close()
            check("upstream timeout after headers terminates incomplete stream", failed)
            check("route rate limiter", [request(port, "/limited")[0] for _ in range(2)] == [200,429])
            check("POST is never replayed after origin commit disconnect", request(port, "/commit", "POST", body=b"once")[0] == 502 and Origin.attempts == 1)
            wait_for(budget, 0)

            completed = metric_value(metrics(), "rgnix_requests_total", status="200")
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
            for _ in range(5):
                conn.request("GET", "/exact", headers={"Host": "example.test"})
                assert conn.getresponse().read() == b"exact"
            idle = conn.sock
            check("keepalive expires an idle connection", idle.recv(1) == b"")
            conn.close()
            check("socket completion and guard drop count each response once", metric_value(metrics(), "rgnix_requests_total", status="200") == completed + 5)
            if hyper:
                completed = metric_value(metrics(), "rgnix_requests_total", status="200")
                pipelined = socket.create_connection(("127.0.0.1", port), timeout=3)
                pipelined.sendall(b"GET /exact HTTP/1.1\r\nHost: example.test\r\n\r\nGET /head HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                data = bytearray()
                while chunk := pipelined.recv(4096):
                    data.extend(chunk)
                pipelined.close()
                wait_for(budget, 0)
                check("pipelined responses finish once and release permits", data.count(b"HTTP/1.1 200") == 2 and b"exact" in data and b"abcdef" in data and metric_value(metrics(), "rgnix_requests_total", status="200") == completed + 2)

            Origin.release.clear()
            held_requests = [held("/budget-hold") for _ in range(2)]
            check("process budget rejects a third active request", budget() == 2 and request(port, "/exact")[0] == 503)
            Origin.release.set()
            for conn, response in held_requests:
                response.read(); conn.close()
            wait_for(budget, 0)
            Origin.release.clear()
            conn, response = held("/backend-hold")
            check("backend budget rejects excess work without blocking local responses", request(port, "/backend-hold")[0] == 503 and request(port, "/exact")[0] == 200)
            Origin.release.set(); response.read(); conn.close()
            wait_for(budget, 0)

            upload = socket.create_connection(("127.0.0.1", port), timeout=3)
            upload.sendall(b"POST /no-read HTTP/1.1\r\nHost: example.test\r\nContent-Length: 67108864\r\n\r\n")
            def send_body():
                try:
                    for _ in range(1024):
                        upload.sendall(b"x" * 65536)
                except OSError:
                    pass
            sender = threading.Thread(target=send_body, daemon=True)
            sender.start()
            response = http.client.HTTPResponse(upload); response.begin(); response.read()
            check("upstream write stall returns a gateway timeout", response.status == 504)
            upload.close(); sender.join(timeout=4)
            wait_for(budget, 0)

            Origin.entered.clear(); Origin.release.clear()
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/hold", headers={"Host": "example.test"})
            response = conn.getresponse()
            check("response streams before origin completes", response.read(8192) == b"x" * 8192 and Origin.entered.is_set())
            check("request budget remains held after response headers", budget() == 1)
            check("route concurrency permit remains held", request(port, "/hold")[0] == 503)
            config.write_text(configuration("new")); process.send_signal(signal.SIGHUP)
            wait_for(lambda: request(port, "/exact")[1].get("x-snapshot"), "new")
            prepared_headers = {k.lower(): v for k, v in json.loads(request(port, "/same/child")[2])["headers"].items()}
            check("reload publishes prepared request headers with the new snapshot", prepared_headers["x-prepared"] == "new")
            check("old request retains snapshot across reload", response.getheader("x-snapshot") == "old")
            Origin.release.set(); check("old stream completes after reload", response.read() == b"two"); conn.close()
            wait_for(budget, 0)

            Origin.entered.clear(); Origin.release.clear()
            aborted = socket.create_connection(("127.0.0.1", port), timeout=3)
            aborted.sendall(b"GET /hold HTTP/1.1\r\nHost: example.test\r\n\r\n")
            wait_for(lambda: Origin.entered.is_set(), True)
            aborted.recv(4096); aborted.shutdown(socket.SHUT_RDWR); aborted.close()
            wait_for(budget, 0)
            check("client cancellation releases request and backend budgets", metric_value(metrics(), "rgnix_backend_inflight", backend="http://app") == 0)
            Origin.release.set()

            if hyper:
                (directory / "route.rgl").write_text("function on_request() return route.pass() end")
                config.write_text(configuration("bad", f"location /plugin {{ rgnix_script {directory}/route.rgl; proxy_pass http://app; }}"))
                errors = metric_value(metrics(), "rgnix_reload_errors_total")
                process.send_signal(signal.SIGHUP)
                wait_for(lambda: metric_value(metrics(), "rgnix_reload_errors_total"), errors + 1)
                check("unsupported reload preserves active valid snapshot", request(port, "/exact")[1]["x-snapshot"] == "new")
                check("unsupported Upgrade fails explicitly", request(port, "/", headers={"Connection":"upgrade", "Upgrade":"websocket"})[0] == 501)
            status, _, _ = request(admin, f"/v1/rollback/{initial_version}", "POST", auth)
            prepared_headers = {k.lower(): v for k, v in json.loads(request(port, "/same/child")[2])["headers"].items()}
            check("rollback rebuilds prepared clients and headers from retained history",
                  status == 200 and request(port, "/exact")[1]["x-snapshot"] == "old"
                  and prepared_headers["x-prepared"] == "old")
            wait_for(budget, 0)
            wait_for(lambda: access.exists() and '"uri":"/hold"' in access.read_text(), True)
            records = [json.loads(line) for line in access.read_text().splitlines() if line.startswith("{")]
            check("completion logs include route/config and W3C identifiers", any(r.get("trace_id") == "0123456789abcdef0123456789abcdef" and r["route"] != "-" and r["config"] != "-" for r in records))
            check("request and upstream failure metrics are recorded", metric_value(metrics(), "rgnix_upstream_errors_total") >= 3 and 'rgnix_route_requests_total{' in metrics())
            check("all request and backend permits return to zero", budget() == 0 and metric_value(metrics(), "rgnix_backend_inflight", backend="http://app") == 0)
            Origin.release.clear()
            conn, response = held("/budget-hold")
            process.send_signal(signal.SIGTERM)
            time.sleep(0.2)
            Origin.release.set()
            check("graceful shutdown lets an admitted stream finish", response.read() == b"two")
            conn.close()
            check("graceful shutdown exits within the drain deadline", process.wait(timeout=6) == 0)
        except BaseException:
            log.flush(); log.seek(0); print(log.read(), file=sys.stderr)
            raise
        finally:
            Origin.release.set()
            process.terminate()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill(); process.wait()
            log.close()
            origin.shutdown(); origin.server_close()
            collector.shutdown(); collector.server_close()
    print(json.dumps({"transport": "hyper" if hyper else "pingora", "passed": len(RESULTS), "checks": RESULTS}, indent=2))


if __name__ == "__main__":
    main()

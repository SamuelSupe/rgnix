#!/usr/bin/env python3
"""Shared HTTP/1 policy and lifecycle checks for the experimental product transport."""
import argparse
import concurrent.futures
import contextlib
import ctypes
import resource
import email.utils
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import socketserver
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

    def do_POST(self):
        if self.path != "/trailers-upload":
            return super().do_POST()
        data, trailers = bytearray(), []
        while True:
            size = int(self.rfile.readline().strip(), 16)
            if not size:
                break
            data.extend(self.rfile.read(size))
            assert self.rfile.read(2) == b"\r\n"
        while (line := self.rfile.readline()) != b"\r\n":
            trailers.append(line.strip().lower().decode())
        payload = json.dumps({"body": data.decode(), "trailers": trailers}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path == "/idle-reuse":
            body = str(self.client_address[1]).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path == "/trailers":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Trailer", "X-End")
            self.end_headers()
            self.wfile.write(b"3\r\none\r\n0\r\nX-End: complete\r\n\r\n")
            return
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


class H2Origin(socketserver.BaseRequestHandler):
    peers = set()
    upload = threading.Event()

    def handle(self):
        import h2.config, h2.connection, h2.events, h2.settings
        connection = h2.connection.H2Connection(h2.config.H2Configuration(client_side=False, header_encoding="utf-8"))
        connection.initiate_connection()
        connection.update_settings({h2.settings.SettingCodes.INITIAL_WINDOW_SIZE: 1024})
        self.request.sendall(connection.data_to_send())
        try:
            while data := self.request.recv(65536):
                for event in connection.receive_data(data):
                    if isinstance(event, h2.events.RequestReceived):
                        self.peers.add(self.client_address)
                        if dict(event.headers)[":path"].startswith(("/h2stall", "/h2readstall")):
                            self.upload.set()
                        else:
                            connection.send_headers(event.stream_id, [(":status", "200"), ("content-length", "0")], end_stream=True)
                    # Intentionally withhold WINDOW_UPDATE for uploads.
                self.request.sendall(connection.data_to_send())
        except (ConnectionError, OSError):
            pass


def exercise_native_limits(binary, directory):
    port, second, admin = [free_port() for _ in range(3)]
    config = directory / "admission.conf"
    config.write_text(f"http {{ access_log off; client_header_timeout 30s; server {{ listen 127.0.0.1:{port}; return 200 first; }} server {{ listen 127.0.0.1:{second}; return 200 second; }} }}")
    log = (directory / "admission.log").open("w")
    command = [binary, "serve", "-c", str(config), "--admin", f"127.0.0.1:{admin}", "--experimental-hyper", "--threads", "1", "--hyper-max-connections", "8", "--hyper-max-connections-per-ip", "3", "--hyper-max-connections-per-listener", "4", "--hyper-max-handshakes", "4", "--hyper-max-handshakes-per-ip", "2", "--hyper-worker-stall-timeout-seconds", "1", "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "1"]
    process = subprocess.Popen(command, stdout=log, stderr=log)
    held = []
    metrics = lambda: request(admin, "/metrics")[2].decode()
    def connect(address=port, source="127.0.0.1", complete=False):
        client = socket.socket()
        client.settimeout(2)
        client.bind((source, 0))
        client.connect(("127.0.0.1", address))
        client.sendall(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n" if complete else b"G")
        if complete:
            response = http.client.HTTPResponse(client); response.begin(); response.read()
            assert response.status == 200
        held.append(client)
        return client
    def rejected(source="127.0.0.1", address=port):
        client = socket.socket(); client.settimeout(2); client.bind((source, 0))
        try:
            client.connect(("127.0.0.1", address)); client.sendall(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
            return not client.recv(1)
        except (ConnectionResetError, BrokenPipeError): return True
        finally: client.close()
    def release():
        for client in held: client.close()
        held.clear()
        wait_for(lambda: metric_value(metrics(), "rgnix_hyper_connections"), 0)
    try:
        wait_for(lambda: request(admin, "/readyz")[0], 200)
        connect(); connect()
        wait_for(lambda: metric_value(metrics(), "rgnix_hyper_pending_connections"), 2)
        check("pending requests are bounded per source before route parsing", rejected() and metric_value(metrics(), "rgnix_hyper_admission_rejections_total", reason="handshake_ip") >= 1)
        connect(source="127.0.0.2", complete=True)
        check("one slow source leaves connection capacity for another source", connect(second, source="127.0.0.2", complete=True) is not None)
        release()
        for _ in range(3): connect(complete=True)
        wait_for(lambda: metric_value(metrics(), "rgnix_hyper_pending_connections"), 0)
        check("completed headers release pending admission but retain IP connection limits", rejected() and metric_value(metrics(), "rgnix_hyper_admission_rejections_total", reason="ip") >= 1)
        connect(source="127.0.0.2", complete=True)
        check("listener admission cannot consume another listener's reservation", rejected(source="127.0.0.3") and connect(second, source="127.0.0.3", complete=True) is not None and metric_value(metrics(), "rgnix_hyper_admission_rejections_total", reason="listener") >= 1)
        release()
        connect(); connect(); connect(source="127.0.0.2"); connect(source="127.0.0.2")
        wait_for(lambda: metric_value(metrics(), "rgnix_hyper_pending_connections"), 4)
        check("pending admission is shared across listeners", rejected(source="127.0.0.3", address=second) and metric_value(metrics(), "rgnix_hyper_admission_rejections_total", reason="handshake") >= 1)
        release()
        check("cancelled initial requests return all admission permits", metric_value(metrics(), "rgnix_hyper_pending_connections") == 0 and request(port)[0] == 200)
        # Stop only a data reactor, leaving the management and supervisor reactors running.
        ptrace = ctypes.CDLL(None, use_errno=True).ptrace
        ptrace.argtypes = [ctypes.c_ulong, ctypes.c_ulong, ctypes.c_void_p, ctypes.c_void_p]
        ptrace.restype = ctypes.c_long
        tid = next(int(task.name) for task in Path(f"/proc/{process.pid}/task").iterdir() if (task / "comm").read_text().startswith("BG hyper-"))
        def trace(op):
            if ptrace(op, tid, None, None) == -1: raise OSError(ctypes.get_errno(), "ptrace")
        trace(0x4206)
        try:
            trace(0x4207)
            wait_for(lambda: bool(os.waitpid(tid, os.WNOHANG | 0x40000000)[0]), True)
            wait_for(lambda: request(admin, "/healthz")[0], 503)
            check("stalled data reactors fail health and readiness while admin stays available", request(admin, "/readyz")[0] == 503 and any(line.startswith('rgnix_runtime_service_healthy{') and line.endswith(' 0') for line in metrics().splitlines()))
        finally: trace(17)
        wait_for(lambda: request(admin, "/readyz")[0], 200)
        check("resumed data reactors recover without restarting the process", request(port)[0] == 200 and process.poll() is None)
    finally:
        for client in held: client.close()
        process.terminate(); process.wait(timeout=5); log.close()
    def limit_fds(): resource.setrlimit(resource.RLIMIT_NOFILE, (64, 64))
    log = (directory / "accept-resource.log").open("w")
    command[command.index("--hyper-max-connections") + 1] = "256"
    for name in ["--hyper-max-connections-per-ip", "--hyper-max-connections-per-listener", "--hyper-max-handshakes", "--hyper-max-handshakes-per-ip"]:
        command[command.index(name) + 1] = "256"
    process = subprocess.Popen(command, stdout=log, stderr=log, preexec_fn=limit_fds)
    held = []
    try:
        wait_for(lambda: request(admin, "/readyz")[0], 200)
        for _ in range(48):
            client = socket.create_connection(("127.0.0.1", port), timeout=2)
            client.sendall(b"G"); held.append(client)
        time.sleep(.2)
        check("file descriptor exhaustion retains the native server process", process.poll() is None and "Too many open files" in (directory / "accept-resource.log").read_text())
        for client in held: client.close()
        held.clear()
        wait_for(lambda: request(port)[0], 200)
        check("listener recovers after file descriptors become available", process.poll() is None and metric_value(metrics(), "rgnix_hyper_accept_errors_total", reason="resources") >= 1)
    finally:
        for client in held: client.close()
        process.terminate(); process.wait(timeout=5); log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--pingora", action="store_true")
    parser.add_argument("--threads", type=int, default=1)
    parser.add_argument("--ephemeral-listener", action="store_true")
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    hyper = not args.pingora
    origin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    h2origin = socketserver.ThreadingTCPServer(("127.0.0.1", 0), H2Origin)
    h2origin.daemon_threads = True
    threading.Thread(target=h2origin.serve_forever, daemon=True).start()
    collector = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Collector)
    collector.records = []
    threading.Thread(target=collector.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="rgnix-hyper-product-") as temp:
        directory = Path(temp)
        port, admin, auxiliary = free_port(), free_port(), free_port()
        auxiliary_config = 0 if args.ephemeral_listener else auxiliary
        access = directory / "access.jsonl"
        config = directory / "nginx.conf"
        def configuration(marker="old", extra=""):
            return f'''events {{}} http {{
access_log {access} json; keepalive_timeout 1s;
upstream app {{ server 127.0.0.1:{origin.server_port}; rgnix_max_inflight 2; }}
upstream tight {{ server 127.0.0.1:{origin.server_port}; rgnix_max_inflight 1; }}
upstream multiplex {{ server 127.0.0.1:{h2origin.server_address[1]}; }}
server {{ listen 127.0.0.1:{port}; server_name example.test;
proxy_read_timeout 2s; proxy_send_timeout 2s;
add_header X-Snapshot {marker} always;
location = /exact {{ return 200 exact; }}
location = /head {{ return 200 abcdef; }}
location = /date {{ keepalive_timeout 5s; return 200 date; }}
location = /custom-date {{ add_header Date "Sun, 06 Nov 1994 08:49:37 GMT"; return 200 date; }}
location = /same {{ return 200 exact-same; }}
location /same {{ proxy_pass http://app; proxy_set_header X-Prepared {marker}; proxy_set_header X-Remove ""; }}
location /rewrite/ {{ proxy_pass http://app/base/; proxy_set_header X-Seen $request_uri; }}
location /raw/ {{ proxy_pass http://app; }}
location /projected {{ access_log off; proxy_pass http://app; }}
location /capture {{ proxy_pass http://app;
    proxy_set_header X-Original changed;
    proxy_set_header X-Captured "$http_x_original|$proxy_add_x_forwarded_for";
    add_header X-Original $http_x_original always;
}}
location /header-limit {{ access_log off; rgnix_limit_rate 1 burst=1 key=header:x-tenant; return 200 ok; }}
location /cookie-limit {{ access_log off; rgnix_limit_rate 1 burst=1 key=cookie:tenant; return 200 ok; }}
location /small {{ client_max_body_size 16; proxy_pass http://app; }}
location /large {{ client_max_body_size 16m; proxy_pass http://app; }}
location /slow {{ proxy_read_timeout 100ms; proxy_pass http://app; }}
location /idle-reuse {{ keepalive_timeout 5s; proxy_read_timeout 100ms; proxy_pass http://app; }}
location /upload-progress {{ proxy_read_timeout 250ms; proxy_pass http://app; }}
location /stall {{ proxy_read_timeout 100ms; proxy_pass http://app; }}
location /limited {{ rgnix_limit_rate 1 burst=1 key=route; return 200 limited; }}
location /hold {{ rgnix_limit_conn 1 key=route; proxy_pass http://app; }}
location /budget-hold {{ proxy_pass http://app/hold; }}
location /backend-hold {{ proxy_pass http://tight/hold; }}
location /no-read {{ client_max_body_size 64m; proxy_send_timeout 100ms; proxy_pass http://app; }}
location /h2 {{ proxy_http_version 2; client_max_body_size 4m; proxy_send_timeout 150ms; proxy_read_timeout 250ms; proxy_pass http://multiplex; }}
location / {{ proxy_pass http://app; }}
{extra}
}}
server {{ listen 127.0.0.1:{port}; server_name other.test; location / {{ return 200 other; }} }}
server {{ listen 127.0.0.1:{auxiliary_config}; http2 on; location / {{ return 200 auxiliary; }} }}
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
                   "--threads", str(args.threads), "--upstream-max-fails", "0", "--max-inflight", "2", "--max-plugin-instances", "1",
                   "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "3",
                   "--otlp-logs-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/logs",
                   "--otlp-traces-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/traces",
                   "--trace-sample-ratio", "0"]
        if hyper:
            command.extend(["--experimental-hyper", "--hyper-max-connections", "4", "--hyper-max-connections-per-ip", "4", "--hyper-max-connections-per-listener", "4", "--hyper-max-handshakes", "4", "--hyper-max-handshakes-per-ip", "4"])
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
            if hyper:
                duplicate = command.copy()
                duplicate[duplicate.index("--admin") + 1] = f"127.0.0.1:{free_port()}"
                conflict = subprocess.run(duplicate, capture_output=True, env=env, timeout=10)
                check("a second server cannot join the worker listener port",
                      conflict.returncode != 0 and request(admin, "/healthz")[0] == 200)
            if args.ephemeral_listener:
                inodes = set()
                for fd in Path(f"/proc/{process.pid}/fd").iterdir():
                    try:
                        target = str(fd.readlink())
                    except FileNotFoundError:
                        continue
                    if target.startswith("socket:["):
                        inodes.add(target[8:-1])
                ports = {int(parts[1].split(":")[1], 16)
                         for line in Path(f"/proc/{process.pid}/net/tcp").read_text().splitlines()[1:]
                         if (parts := line.split())[3] == "0A" and parts[9] in inodes}
                ephemeral = ports - {port, admin}
                check("all workers share one port allocated by listen 0", len(ephemeral) == 1)
                auxiliary = ephemeral.pop()
            if hyper:
                active = lambda: metric_value(metrics(), "rgnix_hyper_connections")
                wait_for(active, 0)
                idle = [socket.create_connection(("127.0.0.1", p), timeout=3) for p in [port, auxiliary, port, auxiliary]]
                try:
                    wait_for(active, 4)
                    with socket.create_connection(("127.0.0.1", port), timeout=3) as rejected:
                        try:
                            rejected.sendall(b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n")
                            closed = not rejected.recv(1)
                        except (ConnectionResetError, BrokenPipeError):
                            closed = True
                    check("idle connections share a hard budget across listeners without blocking admin", closed and metric_value(metrics(), "rgnix_hyper_connection_rejections_total") >= 1 and request(admin, "/healthz")[0] == 200)
                finally:
                    for connection in idle: connection.close()
                wait_for(active, 0)
                check("closing idle connections releases connection admission", request(auxiliary)[2] == b"auxiliary")
                h2 = subprocess.run(["curl", "--noproxy", "*", "-fsS", "--http2-prior-knowledge", f"http://127.0.0.1:{auxiliary}/"], capture_output=True)
                check("plaintext listener accepts HTTP/2 prior knowledge", h2.returncode == 0 and h2.stdout == b"auxiliary")
                with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                    client.sendall(b"GET /trailers HTTP/1.1\r\nHost: example.test\r\nTE: trailers\r\nConnection: close\r\n\r\n")
                    wire = client.makefile("rb").read().lower()
                    check("HTTP/1 response trailers survive streamed forwarding", b"trailer: x-end\r\n" in wire and b"0\r\nx-end: complete\r\n\r\n" in wire)
                with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                    client.sendall(b"POST /trailers-upload HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\nTrailer: X-Checksum\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\nX-Checksum: checked\r\n\r\n")
                    response = http.client.HTTPResponse(client); response.begin()
                    received = json.loads(response.read())
                    check("HTTP/1 request trailers survive streamed forwarding", received == {"body": "abc", "trailers": ["x-checksum: checked"]})
                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                    blocked = pool.submit(request, port, "/h2stall", "POST", None, b"x" * (512 * 1024))
                    assert H2Origin.upload.wait(3)
                    for _ in range(5):
                        assert request(port, "/h2pulse")[0] == 200
                        time.sleep(0.04)
                    check("HTTP/2 flow-control timeout is isolated from concurrent stream traffic", blocked.result(timeout=3)[0] == 504 and len(H2Origin.peers) == 1)
                    blocked = pool.submit(request, port, "/h2readstall")
                    for _ in range(10):
                        assert request(port, "/h2pulse")[0] == 200
                        time.sleep(0.04)
                    check("HTTP/2 response header timeout expires on a busy multiplexed connection", blocked.done() and blocked.result(timeout=3)[0] == 504 and len(H2Origin.peers) == 1)
            initial_version = json.loads(request(admin, "/v1/config", headers=auth)[2])["version"]
            check("Connection cannot strip routing or framing headers", all(request(port, "/exact", headers={"Connection": name})[0] == 400 for name in ["host", "content-length", "transfer-encoding"]))
            status, headers, data = request(port, "/exact", headers={
                "User-Agent": "selective-agent", "Referer": "https://example.test/selective",
            })
            check("exact route and configured response header", status == 200 and headers["x-snapshot"] == "old" and data == b"exact")
            with contextlib.closing(http.client.HTTPConnection("127.0.0.1", port, timeout=3)) as connection:
                dates = []
                for attempt in range(2):
                    if attempt:
                        time.sleep(1.1)
                    connection.request("GET", "/date", headers={"Host": "example.test"})
                    response = connection.getresponse()
                    assert response.status == 200 and response.read() == b"date"
                    dates.append(email.utils.parsedate_to_datetime(response.getheader("Date")).timestamp())
                check("generated Date advances on a persistent connection", dates[1] > dates[0] and abs(time.time() - dates[1]) < 2)
            check("configured Date is preserved", request(port, "/custom-date")[1]["date"] == "Sun, 06 Nov 1994 08:49:37 GMT")
            with contextlib.closing(http.client.HTTPConnection("127.0.0.1", port, timeout=3)) as connection:
                peers = []
                for attempt in range(3):
                    if attempt:
                        time.sleep(0.25)
                    connection.request("GET", "/idle-reuse", headers={"Host": "example.test"})
                    response = connection.getresponse()
                    assert response.status == 200
                    peers.append(response.read())
                check("HTTP/1 idle upstream outlives response read timeout and stays reusable", len(set(peers)) == 1)
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
            many = {f"X-Extra-{i}": f"value-{i}" for i in range(64)}
            forwarded = json.loads(request(port, "/projected", headers=many)[2])["headers"]
            forwarded = {k.lower(): v for k, v in forwarded.items()}
            check("selective context capture preserves all forwarded headers", all(forwarded[k.lower()] == v for k,v in many.items()))
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.putrequest("GET", "/projected", skip_host=True)
            for name, value in [("Host", "example.test"), ("Connection", "KeEp-AlIvE, X-Hop"),
                                ("Connection", "X-Other-Hop"), ("Keep-Alive", "timeout=30"),
                                ("Proxy-Connection", "keep-alive"), ("X-Hop", "remove"),
                                ("X-Other-Hop", "remove"), ("X-End-To-End", "keep")]:
                conn.putheader(name, value)
            conn.endheaders()
            response = conn.getresponse()
            forwarded = {k.lower(): v for k, v in json.loads(response.read())["headers"].items()}
            conn.close()
            check("all Connection fields remove nominated headers while preserving end-to-end headers",
                  response.status == 200 and forwarded.get("x-end-to-end") == "keep"
                  and not any(name in forwarded for name in ["x-hop", "x-other-hop", "keep-alive", "proxy-connection"]))
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.putrequest("GET", "/capture", skip_host=True)
            for name, value in [("Host", "example.test"), ("X-Original", "first"),
                                ("X-Original", "last"), ("X-Original", b"\xff"),
                                ("X-Forwarded-For", "prior"), ("User-Agent", "capture-agent"),
                                ("Referer", "https://example.test/original")]:
                conn.putheader(name, value)
            conn.endheaders()
            response = conn.getresponse()
            original = response.getheader("X-Original")
            forwarded = {k.lower(): v for k,v in json.loads(response.read())["headers"].items()}
            conn.close()
            check("variables retain original duplicate and UTF-8 header semantics", original == "last" and forwarded["x-original"] == "changed" and forwarded["x-captured"] == "last|prior, 127.0.0.1")
            for path, key, values in [("/header-limit", "X-Tenant", ["a", "b", "a"]),
                                      ("/cookie-limit", "Cookie", ["tenant=a", "tenant=b", "tenant=a"])]:
                check(f"selective context retains {key} rate keys", [request(port, path, headers={key: v})[0] for v in values] == [200, 200, 429])
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
            rejected_backend = request(port, "/backend-hold")[0]
            wait_for(budget, 1)
            check("backend budget rejects excess work without blocking local responses", rejected_backend == 503 and request(port, "/exact")[0] == 200)
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
            if hyper:
                wait_for(budget, 1)
                reuse_connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                reuse_connection.request("GET", "/idle-reuse", headers={"Host": "example.test"})
                reuse_peer = reuse_connection.getresponse().read()
                wait_for(budget, 1)
            config.write_text(configuration("new")); process.send_signal(signal.SIGHUP)
            wait_for(lambda: request(port, "/exact")[1].get("x-snapshot"), "new")
            wait_for(budget, 1)
            prepared_headers = {k.lower(): v for k, v in json.loads(request(port, "/same/child")[2])["headers"].items()}
            check("reload publishes prepared request headers with the new snapshot", prepared_headers["x-prepared"] == "new")
            if hyper:
                wait_for(budget, 1)
                reuse_connection.request("GET", "/idle-reuse", headers={"Host": "example.test"})
                check("unchanged upstream pools survive a route-only publication", reuse_connection.getresponse().read() == reuse_peer)
                reuse_connection.close()
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
                (directory / "route.rgl").write_text("function on_request() invalid syntax end")
                config.write_text(configuration("bad", f"location /plugin {{ rgnix_script {directory}/route.rgl; proxy_pass http://app; }}"))
                errors = metric_value(metrics(), "rgnix_reload_errors_total")
                process.send_signal(signal.SIGHUP)
                wait_for(lambda: metric_value(metrics(), "rgnix_reload_errors_total"), errors + 1)
                check("invalid plugin reload preserves active valid snapshot", request(port, "/exact")[1]["x-snapshot"] == "new")
                check("unsupported Upgrade fails explicitly", request(port, "/", headers={"Connection":"upgrade", "Upgrade":"unknown-protocol"})[0] == 501)
            status, _, _ = request(admin, f"/v1/rollback/{initial_version}", "POST", auth)
            prepared_headers = {k.lower(): v for k, v in json.loads(request(port, "/same/child")[2])["headers"].items()}
            check("rollback rebuilds prepared clients and headers from retained history",
                  status == 200 and request(port, "/exact")[1]["x-snapshot"] == "old"
                  and prepared_headers["x-prepared"] == "old")
            wait_for(budget, 0)
            wait_for(lambda: access.exists() and '"uri":"/hold"' in access.read_text(), True)
            records = [json.loads(line) for line in access.read_text().splitlines() if line.startswith("{")]
            check("selective context capture retains access log request metadata", any(r.get("user_agent") == "selective-agent" and r.get("referer") == "https://example.test/selective" for r in records))
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
            h2origin.shutdown(); h2origin.server_close()
            collector.shutdown(); collector.server_close()
    if hyper and sys.platform.startswith("linux"):
        with tempfile.TemporaryDirectory(prefix="rgnix-native-limits-") as directory:
            exercise_native_limits(binary, Path(directory))
    print(json.dumps({"transport": "hyper" if hyper else "pingora", "threads": args.threads, "passed": len(RESULTS), "checks": RESULTS}, indent=2))


if __name__ == "__main__":
    main()

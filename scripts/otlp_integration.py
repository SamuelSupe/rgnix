#!/usr/bin/env python3
"""OTLP transport/failure tests against real HTTP/TLS sockets, using only the stdlib."""
import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

RESULTS = []
BINARY = str(Path(sys.argv[1]).resolve())


def check(name, condition):
    if not condition:
        raise AssertionError(name)
    RESULTS.append(name)
    print("PASS", name, flush=True)


def wait_for(predicate, timeout=5):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(0.025)
    raise AssertionError("condition not reached before deadline")


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Receiver(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        payload = self.rfile.read(int(self.headers["Content-Length"]))
        self.server.received.append((self.path, {k.lower(): v for k, v in self.headers.items()}, payload, time.monotonic()))
        index = len(self.server.received) - 1
        response = self.server.responses[min(index, len(self.server.responses) - 1)]
        if response.get("disconnect"):
            self.close_connection = True
            return
        time.sleep(response.get("delay", 0))
        body = response.get("body", b"")
        self.send_response(response.get("status", 200))
        self.send_header("Content-Type", response.get("type", "application/x-protobuf"))
        self.send_header("Content-Length", str(len(body)))
        for key, value in response.get("headers", {}).items():
            self.send_header(key, value)
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass


@contextlib.contextmanager
def receiver(responses=None, certificate=None, handler=Receiver):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    server.received = []
    server.entered = threading.Event()
    server.responses = responses or [{}]
    if certificate:
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(*certificate)
        server.socket = tls.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


class Process:
    def __init__(self, root, env, args=(), upstream=None):
        self.port, self.admin = port(), port()
        self.config = root / f"{self.port}.conf"
        self.access = root / f"{self.port}.access"
        action = f"proxy_pass http://127.0.0.1:{upstream};" if upstream else 'return 200 "ok";'
        self.config.write_text(f'''events {{}} http {{ access_log {self.access}; server {{
            listen 127.0.0.1:{self.port}; server_name app.test;
            location / {{ {action} }}
            location = /off {{ access_log off; return 200 "off"; }}
            location = /missing {{ return 404 "missing"; }}
            location = /broken {{ proxy_pass http://127.0.0.1:1; }}
        }} }}''')
        self.output = open(root / f"{self.port}.stderr", "w+")
        clean_env = {k: v for k, v in os.environ.items() if not k.startswith("OTEL_")}
        clean_env.update(env)
        self.process = subprocess.Popen([
            BINARY, "serve", "-c", str(self.config), "--admin", f"127.0.0.1:{self.admin}", *args,
        ], env=clean_env, stdout=self.output, stderr=self.output)

    def request(self, path="/ok", method="GET", body=None, headers=None, admin=False):
        conn = http.client.HTTPConnection("127.0.0.1", self.admin if admin else self.port, timeout=3)
        try:
            conn.request(method, path, body=body, headers=headers or {"Host": "app.test"})
            response = conn.getresponse()
            return response.status, response.read()
        finally:
            conn.close()

    def ready(self):
        try:
            return self.request("/readyz", admin=True)[0] == 200
        except (OSError, http.client.HTTPException):
            if self.process.poll() is not None:
                self.output.seek(0)
                raise AssertionError(self.output.read())
            return False

    def metric(self, name):
        text = self.request("/metrics", admin=True)[1].decode()
        return sum(float(line.rsplit(" ", 1)[1]) for line in text.splitlines()
                   if line.startswith(name + " ") or line.startswith(name + "{"))

    def stop(self, graceful=False):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM if graceful else signal.SIGINT)
            self.process.wait(timeout=15)


@contextlib.contextmanager
def running(root, server=None, env=None, args=(), upstream=None):
    defaults = {"OTEL_BLRP_SCHEDULE_DELAY": "40", "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT": "1500"}
    if server:
        defaults["OTEL_EXPORTER_OTLP_LOGS_ENDPOINT"] = f"http://127.0.0.1:{server.server_port}/v1/logs"
    defaults.update(env or {})
    proc = Process(root, defaults, args, upstream)
    try:
        wait_for(proc.ready)
        yield proc
    finally:
        try:
            proc.stop()
        finally:
            if proc.process.poll() is None:
                proc.process.kill()
                proc.process.wait()
            proc.output.close()


with tempfile.TemporaryDirectory(prefix="rgnix-otlp-") as directory:
    root = Path(directory)
    with receiver() as sink:
        endpoint = f"http://127.0.0.1:{sink.server_port}"
        with running(root, env={
            "OTEL_EXPORTER_OTLP_ENDPOINT": endpoint + "/tenant/",
            "OTEL_EXPORTER_OTLP_HEADERS": "authorization=ignored",
            "OTEL_EXPORTER_OTLP_LOGS_HEADERS": "authorization=Bearer%20test-token,x-tenant=team%2Ca%3Db",
            "OTEL_SERVICE_NAME": "rgnix-otlp-test",
            "OTEL_RESOURCE_ATTRIBUTES": "service.name=overridden,deployment.environment.name=qa",
        }) as proc:
            check("application response is unchanged", proc.request("/ok?secret=query-private", "POST", "body-private", {
                "Host": "app.test", "Authorization": "Bearer request-private", "Cookie": "cookie-private",
            }) == (200, b"ok"))
            proc.request("/off")
            proc.request("/missing")
            proc.request("/broken")
            wait_for(lambda: proc.metric("rgnix_otlp_logs_exported_total") == 3)
            payload = b"".join(item[2] for item in sink.received)
            check("standard endpoint suffix and protobuf content type", all(
                p == "/tenant/v1/logs" and h["content-type"] == "application/x-protobuf" for p, h, _, _ in sink.received))
            check("signal auth headers override generic and decode escaped values", all(
                h.get("authorization") == "Bearer test-token"
                and h.get("x-tenant") == "team,a=b" for _, h, _, _ in sink.received))
            check("structured access and resource attributes are exported", all(s in payload for s in [
                b"http.response.status_code", b"rgnix.request.duration_ms", b"rgnix.config.sha256",
                b"rgnix-otlp-test", b"deployment.environment.name", b"rgnix.upstream.address"]))
            check("query body auth cookies and disabled route stay out of OTLP", all(s not in payload for s in [
                b"query-private", b"body-private", b"request-private", b"cookie-private", b"/off", b"overridden"]))
            wait_for(lambda: proc.access.exists() and "/broken" in proc.access.read_text())
            check("local access logging remains enabled with credential redaction", "/ok?secret=[REDACTED]" in proc.access.read_text() and "query-private" not in proc.access.read_text())
            proc.config.write_text(proc.config.read_text().replace(f"access_log {proc.access};", "access_log off;"))
            proc.process.send_signal(signal.SIGHUP)
            wait_for(lambda: proc.metric("rgnix_config_version") == 2)
            proc.request("/after-reload")
            time.sleep(0.15)
            check("SIGHUP access_log off also disables OTLP", proc.metric("rgnix_otlp_logs_exported_total") == 3)

    for response, name, retries in [
        ({"status": 503}, "transient 503 retries identical batch", 1),
        ({"disconnect": True}, "disconnected exporter retries identical batch", 1),
        ({"status": 400}, "permanent 400 is not retried", 0),
        ({"status": 401}, "authentication failure is not retried", 0),
        ({"status": 500}, "non-retryable 500 is not retried", 0),
        ({"status": 307, "headers": {"Location": "http://127.0.0.1:1/credential-leak"}}, "redirect is not followed", 0),
        ({"body": b"invalid protobuf"}, "malformed success is counted as failure", 0),
        ({"body": b"x" * 65537}, "oversized response is rejected", 0),
        ({"type": "text/html"}, "HTML success is not mistaken for OTLP success", 0),
    ]:
        with receiver([response, {}]) as sink, running(root, sink) as proc:
            proc.request()
            metric = "rgnix_otlp_logs_exported_total" if retries else "rgnix_otlp_logs_dropped_total"
            wait_for(lambda: proc.metric(metric) == 1)
            check(name, len(sink.received) == retries + 1 and proc.metric("rgnix_otlp_logs_retries_total") == retries
                  and (not retries or sink.received[0][2] == sink.received[1][2]))

    # ExportLogsServiceResponse.partial_success.rejected_log_records = 1.
    with receiver([{"body": b"\x0a\x02\x08\x01"}]) as sink, running(root, sink) as proc:
        proc.request()
        wait_for(lambda: proc.metric("rgnix_otlp_logs_partial_success_total") == 1)
        check("partial rejection is counted without replay", proc.metric("rgnix_otlp_logs_dropped_total") == 1
              and proc.metric("rgnix_otlp_logs_exported_total") == 0 and len(sink.received) == 1)

    with receiver([{"status": 429, "headers": {"Retry-After": "3"}}]) as sink, running(root, sink, {
        "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT": "300",
    }) as proc:
        proc.request()
        wait_for(lambda: proc.metric("rgnix_otlp_logs_dropped_total") == 1)
        check("Retry-After obeys total export deadline", len(sink.received) == 1 and proc.metric("rgnix_otlp_logs_retries_total") == 0)

    with receiver([{"delay": 2}]) as sink, running(root, sink, {
        "OTEL_BLRP_MAX_QUEUE_SIZE": "2", "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE": "1",
        "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT": "500",
    }) as proc:
        proc.request()
        wait_for(lambda: len(sink.received) == 1)
        start = time.monotonic()
        responses = [proc.request() for _ in range(30)]
        elapsed = time.monotonic() - start
        check("full export queue does not block requests", all(s == 200 for s, _ in responses) and elapsed < 1)
        check("queue memory and loss are bounded and visible", proc.metric("rgnix_otlp_logs_pending") <= 3
              and proc.metric('rgnix_otlp_logs_dropped_total{reason="queue_full"}') > 0)
        wait_for(lambda: proc.metric("rgnix_otlp_logs_pending") == 0)
        check("collector timeout drains as counted drops", proc.metric("rgnix_otlp_logs_dropped_total") == 31)

    cert, key = root / "ca.crt", root / "key.pem"
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost",
                    "-keyout", str(key), "-out", str(cert)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with receiver(certificate=(cert, key)) as sink:
        for host, trusted, expected in [("localhost", True, True), ("localhost", False, False), ("127.0.0.1", True, False)]:
            env = {"OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": f"https://{host}:{sink.server_port}/v1/logs"}
            if trusted:
                env["OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE"] = str(cert)
            with running(root, env=env) as proc:
                proc.request()
                metric = "rgnix_otlp_logs_exported_total" if expected else "rgnix_otlp_logs_dropped_total"
                wait_for(lambda: proc.metric(metric) == 1)
                check(f"TLS trust={trusted} host={host} accepted={expected}", proc.metric("rgnix_otlp_logs_exported_total") == int(expected))

    with receiver() as sink, running(root, sink, {"OTEL_BLRP_SCHEDULE_DELAY": "60000"}) as proc:
        proc.request()
        wait_for(lambda: proc.metric("rgnix_otlp_logs_pending") == 1)
        proc.stop()
        check("shutdown flushes an unfinished batch", len(sink.received) == 1 and proc.process.returncode == 0)

    class SlowBackend(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.server.entered.set()
            time.sleep(1)
            self.send_response(200)
            self.send_header("Content-Length", "7")
            self.end_headers()
            self.wfile.write(b"drained")

    with receiver(handler=SlowBackend) as backend, receiver() as sink, running(root, sink, {
        "OTEL_BLRP_SCHEDULE_DELAY": "60000",
    }, upstream=backend.server_port) as proc:
        responses = []
        request = threading.Thread(target=lambda: responses.append(proc.request("/inflight")))
        request.start()
        wait_for(backend.entered.is_set)
        proc.stop(graceful=True)
        request.join(timeout=3)
        check("SIGTERM drains in-flight requests before flushing their logs", responses == [(200, b"drained")]
              and len(sink.received) == 1 and b"/inflight" in sink.received[0][2])

    with receiver([{"delay": 15}]) as sink, running(root, sink, {"OTEL_EXPORTER_OTLP_LOGS_TIMEOUT": "30000"}) as proc:
        proc.request()
        wait_for(lambda: len(sink.received) == 1)
        start = time.monotonic()
        proc.stop()
        check("stalled collector cannot extend shutdown indefinitely", time.monotonic() - start < 6.5)

    with receiver() as sink, running(root, sink, {"OTEL_LOGS_EXPORTER": "none"}) as proc:
        proc.request()
        time.sleep(0.1)
        check("explicit exporter disable preserves local logs", not sink.received and proc.metric("rgnix_otlp_logs_pending") == 0)

    for env in [
        {"OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://user:secret@localhost/v1/logs"},
        {"OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://localhost", "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL": "grpc"},
        {"OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://localhost", "OTEL_EXPORTER_OTLP_LOGS_HEADERS": "authorization=secret%0d%0afoo"},
        {"OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://localhost", "OTEL_BLRP_MAX_QUEUE_SIZE": "1"},
    ]:
        proc = Process(root, env)
        try:
            proc.process.wait(timeout=5)
            proc.output.seek(0)
            check("invalid exporter config is rejected without leaking credentials", proc.process.returncode != 0 and "secret" not in proc.output.read())
        finally:
            if proc.process.poll() is None:
                proc.process.kill()
                proc.process.wait()
            proc.output.close()

print(json.dumps({"passed": len(RESULTS), "checks": RESULTS}, indent=2))

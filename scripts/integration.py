#!/usr/bin/env python3
"""Behavioral acceptance against real sockets; uses only the Python standard library."""
import base64
import concurrent.futures
import hashlib
import http.client
import http.server
import json
import os
import re
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
COMMITS = 0
HELD_ENTERED = threading.Event()
HELD_PLAIN_ENTERED = threading.Event()
HELD_RELEASE = threading.Event()


def check(name, condition):
    if not condition:
        raise AssertionError(name)
    RESULTS.append(name)
    print("PASS", name, flush=True)


class Upstream(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        self.do_GET()

    def do_GET(self):
        global COMMITS
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = bytearray()
            while True:
                line = self.rfile.readline()
                if not line:
                    self.close_connection = True
                    return
                size = int(line.strip(), 16)
                if not size:
                    self.rfile.readline()
                    break
                chunk = self.rfile.read(size)
                if len(chunk) != size:
                    self.close_connection = True
                    return
                data.extend(chunk)
                self.rfile.read(2)
        else:
            data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path == "/commit":
            COMMITS += 1
            self.close_connection = True
            return
        if self.path == "/events":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for n in range(2):
                chunk = f"data: {n}\n\n".encode()
                self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                self.wfile.flush()
                time.sleep(0.3)
            self.wfile.write(b"0\r\n\r\n")
            return
        if self.headers.get("Upgrade", "").lower() == "websocket":
            key = self.headers["Sec-WebSocket-Key"]
            accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.end_headers()
            self.close_connection = True
            while True:
                frame = self.rfile.read(2)
                if len(frame) != 2:
                    break
                size = frame[1] & 127
                extended = self.rfile.read(2 if size == 126 else 8 if size == 127 else 0)
                if extended:
                    size = int.from_bytes(extended, "big")
                mask = self.rfile.read(4)
                data = self.rfile.read(size)
                if len(mask) != 4 or len(data) != size:
                    break
                data = bytes(value ^ mask[i % 4] for i, value in enumerate(data))
                self.wfile.write(bytes([frame[0], frame[1] & 127]) + extended + data)
                self.wfile.flush()
                if frame[0] & 15 == 8:
                    break
            return
        if self.path.startswith("/slow"):
            time.sleep(0.7)
        if self.path == "/early":
            self.wfile.write(b"HTTP/1.1 103 Early Hints\r\nLink: </app.css>; rel=preload\r\n\r\n")
            self.wfile.flush()
        if self.path in ("/held", "/held-plain"):
            (HELD_ENTERED if self.path == "/held" else HELD_PLAIN_ENTERED).set()
            HELD_RELEASE.wait(10)
        payload = json.dumps({"path": self.path, "port": self.server.server_port, "headers": dict(self.headers), "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass


def request(port, path="/", method="GET", headers=None, body=None, tls=False):
    connection = (http.client.HTTPSConnection("127.0.0.1", port, timeout=5, context=ssl._create_unverified_context())
                  if tls else http.client.HTTPConnection("127.0.0.1", port, timeout=5))
    connection.request(method, path, body, {"Host": "example.test", **(headers or {})})
    response = connection.getresponse()
    result = response.status, dict((k.lower(), v) for k, v in response.getheaders()), response.read()
    connection.close()
    return result


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def exercise_limits(binary, directory, upstream):
    port, admin, unavailable = free_port(), free_port(), free_port()
    script = directory / "budget.rgl"
    script.write_text("function on_request() return route.pass() end")
    conf = directory / "budget.conf"
    conf.write_text(f'''events {{}} http {{ access_log off;
upstream app {{ server 127.0.0.1:{upstream}; }}
upstream balanced {{ server 127.0.0.1:{unavailable}; server 127.0.0.1:{upstream}; }}
server {{ listen 127.0.0.1:{port};
location /plugin/ {{ rgnix_script {script}; proxy_pass http://app/; }}
location /balanced {{ proxy_pass http://balanced; }}
location / {{ proxy_pass http://app; }}
}} }}''')
    recovered = None
    with (directory / "budget.log").open("w") as log:
        process = subprocess.Popen([binary, "serve", "-c", str(conf), "--admin", f"127.0.0.1:{admin}",
            "--max-inflight", "2", "--max-plugin-instances", "1", "--upstream-fail-timeout-secs", "1"], stdout=log, stderr=log)
        try:
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            HELD_ENTERED.clear()
            HELD_PLAIN_ENTERED.clear()
            HELD_RELEASE.clear()
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                first = pool.submit(request, port, "/plugin/held")
                try:
                    assert HELD_ENTERED.wait(3)
                    check("plugin budget rejects excess instances while plain routes work", request(port, "/plugin/")[0] == 503 and request(port, "/")[0] == 200)
                    second = pool.submit(request, port, "/held-plain")
                    assert HELD_PLAIN_ENTERED.wait(3)
                    wait_for(lambda: request(port, "/")[0], 503)
                    check("inflight budget sheds requests without blocking admin", request(admin, "/healthz")[0] == 200)
                finally:
                    HELD_RELEASE.set()
                assert first.result()[0] == 200 and second.result()[0] == 200
            check("completed requests release resource permits", request(port, "/plugin/")[0] == 200)
            statuses = [request(port, "/balanced")[0] for _ in range(8)]
            check("failed endpoint is excluded without replaying its requests", statuses.count(502) == 3 and statuses[-3:] == [200] * 3)
            recovered = http.server.ThreadingHTTPServer(("127.0.0.1", unavailable), Upstream)
            recovered.daemon_threads = True
            threading.Thread(target=recovered.serve_forever, daemon=True).start()
            time.sleep(1.1)
            reached = {json.loads(request(port, "/balanced")[2])["port"] for _ in range(6)}
            check("excluded endpoint re-enters rotation after cooldown", reached == {upstream, unavailable})
        except BaseException:
            print((directory / "budget.log").read_text()[-6000:], file=sys.stderr)
            raise
        finally:
            HELD_RELEASE.set()
            process.terminate()
            process.wait(timeout=35)
            if recovered: recovered.shutdown()


def wait_for(fn, expected=True, timeout=10):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = fn()
            if last == expected:
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {expected!r}; last={last!r}")


def certificate(directory, serial, names=("example.test", "localhost")):
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-subj", "/CN=" + names[0], "-addext", "subjectAltName=" + ",".join("DNS:" + name for name in names),
                    "-set_serial", str(serial), "-keyout", str(directory / "key.pem"), "-out", str(directory / "cert.pem")],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def exercise_body_routing(port, tls_port, upstream, canary, directory):
    def upload(path, data, **kwargs):
        status, headers, body = request(port, path, "POST", body=data, **kwargs)
        assert status == 200, (status, body)
        result = json.loads(body)
        assert result["size"] == len(data) and result["sha256"] == hashlib.sha256(data).hexdigest(), result
        return result, headers

    result, _ = upload("/body-full/", b'{"tenant":"vip","extra":[1,2,3]}')
    check("JSON body routes to the selected HTTPS backend without changing bytes", result["port"] == canary)
    result, _ = upload("/body-full/", b'{"tenant":"standard"}')
    check("JSON routing is isolated between requests", result["port"] == upstream)
    for data in (b'{"tenant":"vip"}garbage', b'{"tenant":"vip"', b'{"tenant":42}', b'{"tenant":null}', b''):
        result, _ = upload("/body-full/", data)
        assert result["port"] == upstream
    check("invalid JSON, incomplete JSON and non-string fields do not match", True)
    result, _ = upload("/body-large/", json.dumps({"tenant": "vip", "padding": "a" * 100000}).encode())
    check("full inspection can replay bodies larger than Pingora's original 64 KiB buffer", result["port"] == canary)
    payload = b"route=vip;" + bytes(range(256)) * 4096
    result, _ = upload("/body-prefix/", payload)
    check("prefix inspection routes binary uploads and preserves the entire body", result["port"] == canary and result["headers"].get("x-body-state") == "truncated")
    result, _ = upload("/body-prefix/", b"x" * 32 + b"route=vip;")
    check("content beyond the configured prefix cannot influence routing", result["port"] == upstream)
    result, _ = upload("/body-prefix/", b'{"tenant":"vip"}' + b" " * 30)
    check("a closed JSON object in a truncated prefix is not treated as complete JSON", result["port"] == upstream)
    result, _ = upload("/body-prefix/", b'{"tenant":"vip"}')
    check("short prefix-inspected bodies may use JSON routing when fully read", result["port"] == canary)
    result, _ = upload("/body-prefix/", b"route=vip;" + b"a" * 21 + "中".encode())
    check("UTF-8 cut at the prefix boundary is nil as text but searchable as bytes", result["port"] == canary and result["headers"].get("x-body-text") == "nil")
    result, _ = upload("/body-off/", b'{"tenant":"vip"}')
    check("body inspection remains opt-in", result["port"] == upstream and result["headers"].get("x-body-state") == "off")
    check("full inspection rejects oversized known-length bodies", request(port, "/body-full/", "POST", body=b"x" * 65537)[0] == 413)

    for chunked in (False, True):
        payload = b"route=vip;" + bytes(range(256)) * 1024
        with socket.create_connection(("127.0.0.1", port), timeout=4) as sock:
            framing = "Transfer-Encoding: chunked" if chunked else f"Content-Length: {len(payload)}"
            sock.sendall(f"POST /body-wide/ HTTP/1.1\r\nHost: example.test\r\n{framing}\r\n\r\n".encode())
            for index, piece in enumerate((payload[:7], payload[7:65539], payload[65539:])):
                sock.sendall((f"{len(piece):x}\r\n".encode() + piece + b"\r\n") if chunked else piece)
                if index == 0:
                    time.sleep(.03)
            if chunked:
                sock.sendall(b"0\r\n\r\n")
            response = http.client.HTTPResponse(sock)
            response.begin()
            result = json.loads(response.read())
            assert response.status == 200 and result["port"] == canary and result["size"] == len(payload)
            assert result["sha256"] == hashlib.sha256(payload).hexdigest()
            sock.sendall(b"GET /alive HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
            following = http.client.HTTPResponse(sock)
            following.begin()
            assert following.read() == b"alive"
    check("fragmented and chunked prefix uploads replay once and keep the next request aligned", True)

    with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
        sock.sendall(b"POST /body-full/ HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n10001\r\n" + b"x" * 65537 + b"\r\n0\r\n\r\n")
        response = http.client.HTTPResponse(sock)
        response.begin()
        check("full inspection rejects oversized chunked bodies before proxying", response.status == 413)
        response.read()
    for path, data, status in (("/body-prefix/", b"x" * 32, 200), ("/body-timeout/", b"x", 408)):
        with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
            sock.sendall(f"POST {path} HTTP/1.1\r\nHost: example.test\r\nContent-Length: 100000\r\nX-Stop: yes\r\n\r\n".encode() + data)
            response = http.client.HTTPResponse(sock)
            response.begin()
            assert response.status == status
            response.read()
    check("prefix decisions do not wait for the tail; incomplete prefixes have a total timeout", True)
    with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
        payload = b'{"tenant":"vip"}'
        sock.sendall(f"POST /body-full/ HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\nContent-Length: {len(payload)}\r\n\r\n".encode())
        interim = b""
        while not interim.endswith(b"\r\n\r\n"):
            interim += sock.recv(1)
        assert interim.startswith(b"HTTP/1.1 100"), interim
        sock.sendall(payload)
        response = http.client.HTTPResponse(sock)
        response.begin()
        result = json.loads(response.read())
        check("Expect 100-continue works before inspection without a second upstream expectation", result["port"] == canary and "expect" not in {k.lower() for k in result["headers"]})
    for path, payload in (("/body-full/", b'{"tenant":"vip"}'), ("/body-prefix/", b"route=vip;" + bytes(range(256)) * 2048)):
        output = directory / "h2-body.json"
        h2 = subprocess.run(["curl", "--noproxy", "*", "-sS", "--http2", "--cacert", str(directory / "cert.pem"),
            "--resolve", f"example.test:{tls_port}:127.0.0.1", f"https://example.test:{tls_port}{path}",
            "--data-binary", "@-", "-o", str(output), "-w", "%{http_version}"], input=payload, capture_output=True)
        assert h2.returncode == 0 and h2.stdout == b"2", h2.stderr
        result = json.loads(output.read_text())
        assert result["port"] == canary and result["size"] == len(payload)
        assert result["sha256"] == hashlib.sha256(payload).hexdigest()
    check("HTTP/2 full and prefix routing preserve the original body", True)
    before = COMMITS
    check("body inspection does not enable upstream POST retries", request(port, "/body-commit/", "POST", body=b"side effect")[0] == 502 and COMMITS == before + 1)
    with socket.create_connection(("127.0.0.1", port), timeout=2) as cancelled:
        cancelled.sendall(b"POST /body-full/ HTTP/1.1\r\nHost: example.test\r\nContent-Length: 1000\r\n\r\nx")
    check("cancelled body inspection leaves the service available", request(port, "/alive")[0] == 200)


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="rgnix-acceptance-") as temporary:
        directory = Path(temporary)
        certificate(directory, 1)
        root = directory / "html"
        root.mkdir()
        (root / "index.html").write_text("hello rgnix\n")
        (root / "data.txt").write_text("0123456789")
        (root / "sub").mkdir()
        (root / "sub/index.html").write_text("subdirectory\n")
        (root / "a%b").mkdir()
        (root / "a%b/index.html").write_text("literal percent\n")
        (root / "escape").symlink_to(directory / "key.pem")
        (root / "data-link").symlink_to("data.txt")
        (root / "fifo-index").mkdir()
        fifo_paths = [root / "pipe", root / "fifo-index/index.html"]
        for path in fifo_paths:
            os.mkfifo(path)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        upstream = server.server_port
        secure_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        secure_server.daemon_threads = True
        ssl_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ssl_context.load_cert_chain(directory / "cert.pem", directory / "key.pem")
        secure_server.socket = ssl_context.wrap_socket(secure_server.socket, server_side=True)
        threading.Thread(target=secure_server.serve_forever, daemon=True).start()
        untrusted = directory / "untrusted"
        untrusted.mkdir()
        certificate(untrusted, 2)
        untrusted_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        untrusted_server.daemon_threads = True
        untrusted_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        untrusted_context.load_cert_chain(untrusted / "cert.pem", untrusted / "key.pem")
        untrusted_server.socket = untrusted_context.wrap_socket(untrusted_server.socket, server_side=True)
        threading.Thread(target=untrusted_server.serve_forever, daemon=True).start()
        class IPv6Server(http.server.ThreadingHTTPServer):
            address_family = socket.AF_INET6
        ipv6_server = IPv6Server(("::1", 0), Upstream)
        ipv6_server.daemon_threads = True
        threading.Thread(target=ipv6_server.serve_forever, daemon=True).start()
        port, tls_port, admin = free_port(), free_port(), free_port()
        plugin = directory / "routes.rgl"
        plugin.write_text('''function on_request()
    if req.header("x-deny") == "yes" then return resp.reply(403, "denied") end
    if req.header("x-double-decision") == "yes" then
        local first = route.proxy("app")
        local second = route.proxy("localhost")
        return first
    end
    local selected = req.header("x-backend")
    if selected ~= nil then return route.proxy(selected) end
    if req.header("x-rewrite") == "yes" then req.set_path("/rewritten") end
    local query = req.header("x-query")
    if query ~= nil then req.set_query(query) end
    if req.header("x-static") == "yes" then
        req.set_path("/a%b")
        req.set_query("q=%23")
    end
    req.set_header("x-plugin", "compiled")
    return route.pass()
end
function on_response()
    resp.set_header("x-generation", "one")
end
''')
        wasm = directory / "routes.wasm"
        subprocess.run([binary, "compile", str(plugin), "-o", str(wasm)], check=True)
        body_plugin = directory / "body.rgl"
        body_plugin.write_text('''function on_request()
    if req.body_complete() then req.set_header("x-body-state", "complete")
    elseif req.body_truncated() then req.set_header("x-body-state", "truncated")
    else req.set_header("x-body-state", "off") end
    if req.body() == nil then req.set_header("x-body-text", "nil") end
    if req.header("x-stop") == "yes" then return resp.reply(200, "prefix ready") end
    if req.json_string("/tenant") == "vip" or req.body_contains("route=vip;") then
        return route.proxy("localhost")
    end
    return route.pass()
end''')
        body_locations = "\n".join(f"location /body-{name}/ {{ rgnix_script {body_plugin}; rgnix_request_body {policy}; {extra} proxy_pass http://app{target}; }}"
            for name, policy, extra, target in [
                ("full", "full 64k", "", "/"), ("large", "full 256k", "", "/"),
                ("prefix", "prefix 32", "", "/"), ("wide", "prefix 96k", "", "/"),
                ("off", "off", "", "/"), ("timeout", "prefix 32", "rgnix_request_body_timeout 100ms;", "/"),
                ("commit", "full 64k", "", "/commit")])
        conf = directory / "nginx.conf"
        conf.write_text(f'''events {{}}
http {{
    access_log off;
    root {root};
    types {{ text/plain txt; text/html html; }}
    upstream app {{ server 127.0.0.1:{upstream}; }}
    upstream localhost {{ server 127.0.0.1:{secure_server.server_port}; }}
    upstream example.test {{ server 127.0.0.1:{untrusted_server.server_port}; }}
    server {{
        listen 127.0.0.1:{port} default_server;
        server_name example.test;
        client_max_body_size 2m;
        add_header X-Inherited yes always;
        {body_locations}
        location /api/ {{ proxy_pass http://app/base/; proxy_set_header Host $host; }}
        location /raw/ {{ proxy_pass http://app; }}
        location /raw-plugin/ {{ proxy_pass http://app; rgnix_script {plugin}; }}
        location /v6/ {{ proxy_pass http://[::1]:{ipv6_server.server_port}/; }}
        location /trusted/ {{ proxy_pass https://localhost/; }}
        location /secure-plugin/ {{ proxy_pass https://localhost/; rgnix_script {plugin}; }}
        location /wrong-host/ {{ proxy_pass https://127.0.0.1:{secure_server.server_port}/; }}
        location /untrusted/ {{ proxy_pass https://example.test/; }}
        location /plugin/ {{ proxy_pass http://app/; rgnix_script {plugin}; }}
        location /wasm/ {{ proxy_pass http://app/; rgnix_script {wasm}; }}
        location /static-plugin/ {{ rgnix_script {plugin}; }}
        location /loop/ {{ rgnix_script {directory / 'loop.rgl'}; return 200 ok; }}
        location = /commit {{ proxy_pass http://app; }}
        location = /events {{ proxy_pass http://app; }}
        location = /ws {{ proxy_pass http://app; }}
        location /limited/ {{ client_max_body_size 16k; proxy_pass http://app/; }}
        location /timeout/ {{ proxy_pass http://app/slow; proxy_read_timeout 100ms; }}
        location = /alive {{ return 200 alive; }}
    }}
    server {{
        listen 127.0.0.1:{tls_port} ssl;
        http2 on;
        server_name example.test;
        ssl_certificate {directory / 'cert.pem'};
        ssl_certificate_key {directory / 'key.pem'};
        {body_locations}
        location / {{ return 200 secure; }}
    }}
}}''')
        (directory / "loop.rgl").write_text("function on_request() while true do end end")
        (directory / "response-fail.rgl").write_text('''function on_request() return route.pass() end
function on_response() resp.set_header("x-partial", "must-not-escape") while true do end end''')
        conf.write_text(conf.read_text().replace("location = /alive", f"location /response-fail {{ rgnix_script {directory / 'response-fail.rgl'}; proxy_pass http://app; }}\n        location = /alive"))
        subprocess.run([binary, "check", "-c", str(conf)], check=True)
        ambiguous = directory / "ambiguous.conf"
        ambiguous.write_text(conf.read_text().replace("location /trusted/", "location /plain-tls-name/ { proxy_pass http://localhost; }\n        location /trusted/"))
        rejected = subprocess.run([binary, "check", "-c", str(ambiguous)], capture_output=True, text=True)
        ambiguous.write_text(re.sub(r"rgnix_script [^;]+;", "", ambiguous.read_text()))
        without_plugin = subprocess.run([binary, "check", "-c", str(ambiguous)], capture_output=True, text=True)
        check("script aliases reject upstreams with conflicting transport protocols", rejected.returncode != 0 and without_plugin.returncode == 0)
        for directive in ("rgnix_request_body prefix 0;", "rgnix_request_body full 257k;", "rgnix_request_body anything 1k;", "rgnix_request_body_timeout 0;"):
            invalid_body = directory / "invalid-body.conf"
            invalid_body.write_text(conf.read_text().replace("rgnix_request_body full 64k;", directive))
            rejected = subprocess.run([binary, "check", "-c", str(invalid_body)], capture_output=True)
            assert rejected.returncode != 0, directive
        check("configuration rejects invalid body modes, limits and timeouts", True)
        log_path = directory / "server.log"
        with log_path.open("w") as log:
            process = subprocess.Popen([binary, "serve", "-c", str(conf), "--admin", f"127.0.0.1:{admin}"], stdout=log, stderr=log,
                                       env={**os.environ, "SSL_CERT_FILE": str(directory / "cert.pem")})
            try:
                wait_for(lambda: request(admin, "/readyz")[0], 200)
                check("static GET", request(port)[2] == b"hello rgnix\n")
                status, headers, body = request(port, "/data.txt", "HEAD")
                check("static HEAD", status == 200 and headers["content-length"] == "10" and body == b"")
                check("static single range", request(port, "/data.txt", headers={"Range": "bytes=2-5"})[2] == b"2345")
                check("unsatisfiable range", request(port, "/data.txt", headers={"Range": "bytes=20-"})[0] == 416)
                etag = request(port, "/data.txt")[1]["etag"]
                check("conditional GET", request(port, "/data.txt", headers={"If-None-Match": etag})[0] == 304)
                modified = request(port, "/data.txt")[1]["last-modified"]
                check("If-Range HTTP date retains partial response", request(port, "/data.txt", headers={"Range": "bytes=2-5", "If-Range": modified})[0] == 206)
                check("directory redirect and index", request(port, "/sub")[0] == 301 and request(port, "/sub/")[2] == b"subdirectory\n")
                check("symlink containment", request(port, "/escape")[0] == 403)
                check("contained symlink still serves a regular file", request(port, "/data-link")[2] == b"0123456789")
                with concurrent.futures.ThreadPoolExecutor(max_workers=21) as pool:
                    targets = ["/pipe", "/fifo-index/"] * 10 + ["/data.txt"]
                    statuses = list(pool.map(lambda path: request(port, path)[0], targets))
                check("FIFO files and indexes cannot exhaust the static file pool", statuses == [403] * 20 + [200])
                check("path traversal rejection", request(port, "/%2e%2e/key.pem")[0] == 400)
                reply = json.loads(request(port, "/api/item?q=1")[2])
                check("proxy_pass URI replacement", reply["path"] == "/base/item?q=1" and reply["headers"]["Host"] == "example.test")
                reply = json.loads(request(port, "/raw/%61?q=2")[2])
                check("proxy_pass preserves original URI", reply["path"] == "/raw/%61?q=2")
                check("IPv6 upstream", json.loads(request(port, "/v6/hello")[2])["path"] == "/hello")
                check("trusted HTTPS upstream", request(port, "/trusted/hello")[0] == 200)
                check("HTTPS upstream rejects hostname mismatch", request(port, "/wrong-host/hello")[0] == 502)
                check("HTTPS upstream rejects untrusted certificate", request(port, "/untrusted/hello")[0] == 502)
                check("Connection cannot remove framing or Host", request(port, "/api/a", headers={"Connection": "host"})[0] == 400)
                check("inherited response header", request(port, "/api/a")[1].get("x-inherited") == "yes")
                payload = b"rgnix-data" * 100000
                reply = json.loads(request(port, "/api/upload", "POST", body=payload)[2])
                check("streaming request body", reply["size"] == len(payload) and reply["sha256"] == hashlib.sha256(payload).hexdigest())
                for ambiguous in (False, True):
                    with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
                        framing = b"Transfer-Encoding: chunked\r\n" + (b"Content-Length: 5\r\n" if ambiguous else b"")
                        sock.sendall(b"POST /api/upload HTTP/1.1\r\nHost: example.test\r\n" + framing + b"\r\n3\r\nabc\r\n0\r\n\r\n")
                        response = http.client.HTTPResponse(sock)
                        response.begin()
                        reply = json.loads(response.read())
                        assert response.status == 200 and reply["size"] == 3
                        assert "content-length" not in {key.lower() for key in reply["headers"]}
                        if ambiguous:
                            check("TE plus Content-Length cannot re-enable connection reuse", response.will_close and sock.recv(1) == b"")
                        else:
                            assert not response.will_close
                            sock.sendall(b"GET /api/next HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                            following = http.client.HTTPResponse(sock)
                            following.begin()
                            following.read()
                            check("valid chunked request keeps its connection reusable", following.status == 200 and sock.recv(1) == b"")
                with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
                    sock.sendall(b"GET /api/legacy HTTP/1.0\r\nHost: example.test\r\n\r\n")
                    response = http.client.HTTPResponse(sock)
                    response.begin()
                    response.read()
                    check("HTTP/1.0 without keep-alive closes after the response", response.status == 200 and response.will_close and sock.recv(1) == b"")
                check("request body limit", request(port, "/api/upload", "POST", headers={"Content-Length": "3000000"})[0] == 413)
                for upgrade in (b"", b"Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"):
                    with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
                        sock.sendall(b"POST /limited/upload HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n" + upgrade + b"\r\n8000\r\n" + b"x" * 32768 + b"\r\n0\r\n\r\n")
                        response = http.client.HTTPResponse(sock)
                        response.begin()
                        check("chunked HTTP body limit" + (" before a requested WebSocket upgrade" if upgrade else ""), response.status == 413)
                        response.read()
                status, headers, body = request(port, "/plugin/item", headers={"x-rewrite": "yes"})
                reply = json.loads(body)
                check("compiled request and response hooks", status == 200 and reply["path"] == "/rewritten" and reply["headers"].get("x-plugin") == "compiled" and headers.get("x-generation") == "one")
                for path in ("/raw-plugin/a%2Fb", "/raw-plugin/%61", "/raw-plugin/a//b", "/raw-plugin/a/../b", "/raw-plugin/a%3Fb"):
                    for query in ("new=%2F", ""):
                        status, _, body = request(port, path + "?old=1", headers={"x-query": query})
                        expected = path + ("?" + query if query else "")
                        assert status == 200 and json.loads(body)["path"] == expected, (path, query, status, body)
                check("query-only edits preserve raw paths and can clear the query", True)
                replaced = json.loads(request(port, "/plugin/a%2Fb?old=1", headers={"x-query": "new=1"})[2])["path"]
                rewritten = json.loads(request(port, "/raw-plugin/a%2Fb?old=1", headers={"x-query": "new=1", "x-rewrite": "yes"})[2])["path"]
                check("query edits retain proxy URI replacement and explicit path rewrites", replaced == "/a/b?new=1" and rewritten == "/rewritten?new=1")
                check("compiled Wasm artifact loads independently", request(port, "/wasm/item")[1].get("x-generation") == "one")
                status, headers, _ = request(port, "/static-plugin/?original=1", headers={"x-static": "yes"})
                check("static plugin redirect preserves literal percent and rewritten query", status == 301 and headers.get("location") == "/a%25b/?q=%23" and request(port, headers["location"])[2] == b"literal percent\n")
                with socket.create_connection(("127.0.0.1", port), timeout=3) as early:
                    early.sendall(b"GET /plugin/early HTTP/1.1\r\nHost: example.test\r\n\r\n")
                    stream = early.makefile("rb")
                    responses = []
                    for _ in range(2):
                        headers = b""
                        while not headers.endswith(b"\r\n\r\n"):
                            line = stream.readline()
                            assert line, headers
                            headers += line
                        responses.append(headers.lower())
                    check("informational responses do not consume the final response hook", b"103" in responses[0].splitlines()[0] and b"x-generation" not in responses[0] and b"x-generation: one" in responses[1])
                    stream.close()
                check("plugin direct response", request(port, "/plugin/item", headers={"x-deny": "yes"})[0] == 403)
                check("superseded plugin decisions fail instead of selecting another backend", request(port, "/plugin/item", headers={"x-double-decision": "yes"})[0] == 500)
                status, _, body = request(port, "/secure-plugin/item", headers={"x-backend": "localhost", "Authorization": "Bearer test-marker"})
                check("script backend alias preserves HTTPS and forwarded headers", status == 200 and json.loads(body)["port"] == secure_server.server_port and json.loads(body)["headers"]["Authorization"] == "Bearer test-marker")
                check("script HTTPS backend still verifies certificates", request(port, "/plugin/item", headers={"x-backend": "example.test"})[0] == 502)
                check("fuel limits infinite loops", request(port, "/loop/")[0] == 500 and request(port, "/alive")[0] == 200)
                status, headers, _ = request(port, "/response-fail")
                check("response hook failure returns 500 without staged edits", status == 500 and "x-partial" not in headers)
                check("upstream disconnect never replays POST", request(port, "/commit", "POST", body=b"side effect")[0] == 502 and COMMITS == 1)
                check("upstream read timeout returns 504", request(port, "/timeout/a")[0] == 504)
                check("TLS listener", request(tls_port, tls=True)[2] == b"secure")
                h2 = subprocess.run(["curl", "--noproxy", "*", "-sS", "--http2", "--cacert", str(directory / "cert.pem"), "--resolve", f"example.test:{tls_port}:127.0.0.1", f"https://example.test:{tls_port}/", "-o", "/dev/null", "-w", "%{http_version}"], capture_output=True, text=True)
                check("HTTP/2 negotiation", h2.returncode == 0 and h2.stdout == "2")
                exercise_body_routing(port, tls_port, upstream, secure_server.server_port, directory)
                with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
                    payload = b'{"tenant":"vip"}'
                    sock.sendall(f"POST /body-full/ HTTP/1.1\r\nHost: example.test\r\nContent-Length: {len(payload)}\r\n\r\n".encode() + payload[:5])
                    time.sleep(.1)
                    original = conf.read_text()
                    conf.write_text(original.replace("rgnix_request_body full 64k;", "rgnix_request_body off;"))
                    process.send_signal(signal.SIGHUP)
                    wait_for(lambda: json.loads(request(port, "/body-full/", "POST", body=payload)[2])["port"], upstream)
                    sock.sendall(payload[5:])
                    response = http.client.HTTPResponse(sock)
                    response.begin()
                    check("in-flight body inspection retains its policy across reload", json.loads(response.read())["port"] == secure_server.server_port)
                    conf.write_text(original)
                    process.send_signal(signal.SIGHUP)
                    wait_for(lambda: json.loads(request(port, "/body-full/", "POST", body=payload)[2])["port"], secure_server.server_port)
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
                begin = time.monotonic()
                conn.request("GET", "/events", headers={"Host": "example.test"})
                response = conn.getresponse()
                first = response.read(9)
                check("SSE first event is streamed", first == b"data: 0\n\n" and time.monotonic() - begin < 0.25)
                response.read()
                conn.close()
                with socket.create_connection(("127.0.0.1", port), timeout=3) as sock:
                    sock.sendall(b"GET /ws HTTP/1.1\r\nHost: example.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
                    response = b""
                    while b"\r\n\r\n" not in response:
                        response += sock.recv(1)
                    mask = b"1234"
                    data = b"hello"
                    sock.sendall(b"\x81\x85" + mask + bytes(v ^ mask[i % 4] for i, v in enumerate(data)))
                    with sock.makefile("rb") as stream:
                        echoed = stream.read(7)
                        check("WebSocket upgrade and bidirectional frames", b"101" in response.split(b"\r\n")[0] and echoed == b"\x81\x05hello")
                        data = bytes(range(256)) * 256
                        masked = bytes(v ^ mask[i % 4] for i, v in enumerate(data))
                        for _ in range(48):
                            sock.sendall(b"\x82\xff" + len(data).to_bytes(8, "big") + mask + masked)
                            assert stream.read(10) == b"\x82\x7f" + len(data).to_bytes(8, "big")
                            assert stream.read(len(data)) == data
                        check("WebSocket streams beyond the HTTP body limit (3 MiB)", True)
                        sock.sendall(b"\x88\x82" + mask + bytes([0x03 ^ mask[0], 0xe8 ^ mask[1]]))
                        check("WebSocket close frame survives the long stream", stream.read(4) == b"\x88\x02\x03\xe8")
                with concurrent.futures.ThreadPoolExecutor() as pool:
                    old = pool.submit(request, port, "/plugin/slow")
                    time.sleep(0.15)
                    plugin.write_text(plugin.read_text().replace('"one"', '"two"'))
                    process.send_signal(signal.SIGHUP)
                    wait_for(lambda: request(port, "/plugin/item")[1].get("x-generation"), "two")
                    check("reload keeps in-flight snapshot", old.result()[1].get("x-generation") == "one")
                plugin.write_text("this is invalid")
                process.send_signal(signal.SIGHUP)
                wait_for(lambda: b"rgnix_reload_errors_total 1" in request(admin, "/metrics")[2])
                check("invalid reload retains active configuration", request(port, "/plugin/item")[1].get("x-generation") == "two")
                plugin.write_text("function on_request() local n = " + "+".join(["1"] * 10000) + " return route.pass() end")
                rejected = subprocess.run([binary, "compile", str(plugin), "-o", str(directory / "deep.wasm")], capture_output=True, timeout=15)
                check("compiler rejects a flat expression with excessive AST depth", rejected.returncode != 0 and rejected.returncode > 0)
                process.send_signal(signal.SIGHUP)
                wait_for(lambda: b"rgnix_reload_errors_total 2" in request(admin, "/metrics")[2])
                check("deep AST reload cannot abort the data plane", process.poll() is None and request(port, "/plugin/item")[1].get("x-generation") == "two")
                exercise_limits(binary, directory, upstream)
                check("metrics are exposed", b"rgnix_plugin_errors_total" in request(admin, "/metrics")[2])
                with socket.create_connection(("127.0.0.1", port)) as cancelled:
                    cancelled.sendall(b"GET /plugin/slow HTTP/1.1\r\nHost: example.test\r\n\r\n")
                time.sleep(0.8)
                check("client cancellation preserves service", request(port, "/alive")[2] == b"alive")
            except BaseException:
                print(log_path.read_text()[-15000:], file=sys.stderr)
                raise
            finally:
                # Release a regressed blocking FIFO open before shutting down this fixture.
                fifo_writers = [os.open(path, os.O_RDWR | os.O_NONBLOCK) for path in fifo_paths]
                process.terminate()
                try:
                    process.wait(timeout=35)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                for fd in fifo_writers:
                    os.close(fd)
                server.shutdown()
                secure_server.shutdown()
                untrusted_server.shutdown()
                ipv6_server.shutdown()
        invalid_config = directory / "invalid-kubeconfig"
        invalid_config.write_text("this is not a kubeconfig")
        admin, http_port, https_port = free_port(), free_port(), free_port()
        with log_path.open("w") as log:
            failed_controller = subprocess.Popen([binary, "ingress", "--publish-service", "qa/rgnix",
                                                  "--http-listen", f"127.0.0.1:{http_port}",
                                                  "--https-listen", f"127.0.0.1:{https_port}",
                                                  "--admin", f"127.0.0.1:{admin}"], stdout=log, stderr=log,
                                                 env={**os.environ, "KUBECONFIG": str(invalid_config)})
            try:
                wait_for(lambda: request(admin, "/healthz")[0], 503)
                check("failed controller is neither healthy nor ready", request(admin, "/readyz")[0] == 503)
            finally:
                failed_controller.terminate()
                failed_controller.wait(timeout=35)
        output = Path(os.environ.get("RGNIX_RESULTS", ".local/acceptance.json"))
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps({"passed": len(RESULTS), "checks": RESULTS}, indent=2) + "\n")
        print(f"{len(RESULTS)} integration checks passed; {output}")


if __name__ == "__main__":
    main()

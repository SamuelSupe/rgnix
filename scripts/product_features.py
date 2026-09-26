#!/usr/bin/env python3
"""Product acceptance over real sockets; protocol cases use distro grpcio and brotli."""
import base64
import concurrent.futures
import gzip
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time

from integration import Upstream, check, free_port, request, wait_for, metric_value, RESULTS


def command(*args, **kwargs):
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs).stdout


class Backend(Upstream):
    def do_GET(self):
        if self.path == "/authorize" and hasattr(self.server, "auth_traces"):
            self.server.auth_traces.append({name: self.headers.get(name) for name in ("traceparent", "tracestate")})
        if self.path == "/jwks":
            self.send_response(200)
            self.send_header("Content-Length", str(len(self.server.jwks)))
            self.end_headers()
            self.wfile.write(self.server.jwks)
            return
        if self.path in ("/authorize", "/health"):
            if self.path == "/health":
                status = 200 if self.server.healthy else 503
            else:
                status = {"Bearer yes": 200, "Bearer unavailable": 503}.get(self.headers.get("Authorization"), 401)
            self.send_response(status)
            self.send_header("Content-Length", "0")
            self.send_header("x-tenant", "verified")
            self.end_headers()
            return
        super().do_GET()


def start_backend(tls_root=None):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    server.healthy = True
    if tls_root:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(tls_root / "server.crt", tls_root / "server.key")
        server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def b64(value):
    return base64.urlsafe_b64encode(value).decode().rstrip("=")


def jwt(root, claims, algorithm="RS256"):
    data = (b64(json.dumps({"alg": algorithm, "kid": "test"}).encode()) + "." + b64(json.dumps(claims).encode())).encode()
    signature = command("openssl", "dgst", "-sha256", "-sign", str(root / "jwt.key"), input=data)
    return (data + b"." + b64(signature).encode()).decode()


def certificates(root):
    for name in ("ca", "other-ca"):
        command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / f"{name}.key"), "-out", str(root / f"{name}.crt"), "-days", "2", "-subj", f"/CN={name}", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    for name, purpose in (("server", "serverAuth"), ("client", "clientAuth")):
        command("openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / f"{name}.key"), "-out", str(root / f"{name}.csr"), "-subj", "/CN=example.test")
        (root / "extensions").write_text(f"subjectAltName=DNS:example.test,IP:127.0.0.1\nextendedKeyUsage={purpose}\n")
        command("openssl", "x509", "-req", "-in", str(root / f"{name}.csr"), "-CA", str(root / "ca.crt"), "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-out", str(root / f"{name}.crt"), "-days", "2", "-extfile", str(root / "extensions"))
    command("openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", str(root / "jwt.key"))
    modulus = command("openssl", "rsa", "-in", str(root / "jwt.key"), "-noout", "-modulus").decode().strip().split("=", 1)[1]
    (root / "jwks.json").write_text(json.dumps({"keys": [{"kty": "RSA", "kid": "test", "alg": "RS256", "use": "sig", "n": b64(bytes.fromhex(modulus)), "e": "AQAB"}]}))


def proxy_request(port, preamble):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    conn.connect()
    conn.sock.sendall(preamble)
    conn.request("GET", "/", headers={"Host": "example.test"})
    response = conn.getresponse()
    result = response.status, response.read()
    conn.close()
    return result


def main():
    binary = str(Path(sys.argv[1]).resolve())
    first, second = start_backend(), start_backend()
    process = None
    with tempfile.TemporaryDirectory(prefix="rgnix-product-") as temp:
        root = Path(temp)
        try:
            certificates(root)
            port, admin, tls, mtls, proxy_port = [free_port() for _ in range(5)]
            token = "admin-review-token-" + "a" * 40
            (root / "admin.token").write_text(token)
            (root / "web").mkdir()
            page = b"<html>application shell</html>\n" * 256
            (root / "web/index.html").write_bytes(page)
            (root / "web/file.txt").write_text("asset\n")
            (root / "data.rgl").write_text('''function on_request()
  if req.json_int("/count", -1) == 7 and req.json_bool("/enabled", false) then
    if req.arg("tenant") == "a b" and req.cookie("group") == "blue" and str.hash("x") == str.hash("x") then return resp.reply(201, "matched") end
  end
  return resp.reply(202, "fallback")
end''')
            (root / "claims.rgl").write_text('''function on_request()
  if req.claim("tenant") == "blue" then return route.pass() end
  return resp.reply(403, "tenant denied")
end''')
            config = root / "nginx.conf"
            config.write_text(f'''events {{}} http {{
access_log {root}/access.log;
root {root}/web;
upstream pool {{ server 127.0.0.1:{first.server_port}; server 127.0.0.1:{second.server_port}; least_conn; }}
upstream sticky {{ server 127.0.0.1:{first.server_port}; server 127.0.0.1:{second.server_port}; rgnix_balance hash cookie:group; }}
upstream affinity {{ server 127.0.0.1:{first.server_port}; server 127.0.0.1:{second.server_port}; rgnix_balance sticky rgnix_session; }}
upstream healthy {{ server 127.0.0.1:{first.server_port}; server 127.0.0.1:{second.server_port}; rgnix_health_check /health interval=1s timeout=200ms; }}
upstream bounded {{ server 127.0.0.1:{first.server_port}; rgnix_max_inflight 1; }}
server {{ listen 127.0.0.1:{port}; listen 127.0.0.1:{tls} ssl; http2 on; server_name example.test;
ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key;
location / {{ return 200 "version-one"; }}
location /ip {{ set_real_ip_from 127.0.0.0/8; real_ip_header X-Forwarded-For; real_ip_recursive on; return 200 "$remote_addr|$realip_remote_addr|$scheme"; }}
location /spoof {{ real_ip_header X-Forwarded-For; return 200 "$remote_addr"; }}
location /forwarded {{ set_real_ip_from 127.0.0.0/8; real_ip_header Forwarded; real_ip_recursive on; return 200 "$remote_addr"; }}
location /allow {{ allow 203.0.113.0/24; deny all; set_real_ip_from 127.0.0.0/8; real_ip_header X-Real-IP; return 200 "allowed"; }}
location /rate {{ rgnix_limit_rate 1 burst=2 key=header:x-tenant; return 200 "accepted"; }}
location /limited {{ rgnix_limit_conn 1 key=header:x-tenant; proxy_pass http://127.0.0.1:{first.server_port}/slow; }}
location /backend-limit {{ proxy_pass http://bounded/slow; }}
location /least {{ proxy_pass http://pool/slow; }}
location /sticky {{ proxy_pass http://sticky; }}
location /affinity {{ proxy_pass http://affinity; }}
location /health {{ proxy_pass http://healthy/observed; }}
location /body {{ rgnix_request_body full 16k; rgnix_script {root}/data.rgl; }}
location /jwt {{ rgnix_jwt jwks={root}/jwks.json issuer=qa audience=api; rgnix_script {root}/claims.rgl; proxy_pass http://127.0.0.1:{first.server_port}; }}
location /auth {{ rgnix_auth_request http://127.0.0.1:{first.server_port}/authorize headers=x-tenant timeout=500ms; proxy_pass http://127.0.0.1:{first.server_port}; }}
location /spa/ {{ alias {root}/web/; try_files $uri $uri/ /index.html; gzip on; brotli on; }}
location /assets/ {{ alias {root}/web/; }}
}}
server {{ listen 127.0.0.1:{mtls} ssl; server_name example.test; ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key; ssl_client_certificate {root}/ca.crt; ssl_verify_client on; return 200 "mutual"; }}
server {{ listen 127.0.0.1:{proxy_port} proxy_protocol; set_real_ip_from 127.0.0.0/8; real_ip_header proxy_protocol; return 200 "$remote_addr|$realip_remote_addr"; }}
}}''')
            command(binary, "check", "-c", str(config))
            output = open(root / "stderr", "w+")
            process = subprocess.Popen([binary, "serve", "-c", str(config), "--admin", f"127.0.0.1:{admin}", "--admin-token-file", str(root / "admin.token")], stdout=output, stderr=output)
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            check("trusted XFF resolves the last untrusted hop and retains socket peer", request(port, "/ip", headers={"X-Forwarded-For": "192.0.2.9, 203.0.113.7, 127.0.0.2", "X-Forwarded-Proto": "https"})[2] == b"203.0.113.7|127.0.0.1|https")
            check("untrusted forwarding headers do not change client identity", request(port, "/spoof", headers={"X-Forwarded-For": "203.0.113.8"})[2] == b"127.0.0.1")
            check("malformed trusted forwarding chain falls back to socket peer", request(port, "/ip", headers={"X-Forwarded-For": "bad, 203.0.113.7"})[2].startswith(b"127.0.0.1|"))
            check("Forwarded supports quoted IPv6 with ports", request(port, "/forwarded", headers={"Forwarded": 'for="[2001:db8::9]:443";proto=https'})[2] == b"2001:db8::9")
            check("CIDR rules apply to resolved client identity", request(port, "/allow", headers={"X-Real-IP": "203.0.113.2"})[0] == 200 and request(port, "/allow")[0] == 403)
            statuses = [request(port, "/rate", headers={"x-tenant": "one"})[0] for _ in range(3)]
            check("rate bucket enforces burst and isolates tenants", statuses == [200, 200, 429] and request(port, "/rate", headers={"x-tenant": "two"})[0] == 200)
            with concurrent.futures.ThreadPoolExecutor() as pool:
                held = pool.submit(request, port, "/limited", headers={"x-tenant": "one"})
                time.sleep(.15)
                check("route concurrency rejects excess requests", request(port, "/limited", headers={"x-tenant": "one"})[0] == 503)
                check("concurrency permit releases after completion", held.result()[0] == 200 and request(port, "/limited", headers={"x-tenant": "one"})[0] == 200)
                held = pool.submit(request, port, "/backend-limit")
                time.sleep(.15)
                metrics = request(admin, "/metrics")[2].decode()
                check("backend metrics expose concurrency occupancy and its configured limit", metric_value(metrics, "rgnix_backend_inflight", backend="bounded") == 1 and metric_value(metrics, "rgnix_backend_inflight_limit", backend="bounded") == 1)
                check("backend concurrency covers the upstream request lifetime", request(port, "/backend-limit")[0] == 503 and held.result()[0] == 200)
                held = pool.submit(request, port, "/least")
                time.sleep(.15)
                other = request(port, "/least")
                check("least connections selects a different idle endpoint", json.loads(held.result()[2])["port"] != json.loads(other[2])["port"])
            ports = [json.loads(request(port, "/sticky", headers={"Cookie": "group=stable"})[2])["port"] for _ in range(5)]
            check("cookie hash affinity is stable across connections", len(set(ports)) == 1)
            initial = request(port, "/affinity")
            cookie = initial[1]["set-cookie"].split(";", 1)[0]
            follow = request(port, "/affinity", headers={"Cookie": cookie})
            check("managed affinity issues a cookie and preserves the chosen backend", json.loads(initial[2])["port"] == json.loads(follow[2])["port"] and "set-cookie" not in follow[1])
            wait_for(lambda: request(port, "/health")[0], 200)
            first.healthy = False
            wait_for(lambda: all(json.loads(request(port, "/health")[2])["port"] == second.server_port for _ in range(4)), True)
            check("active health excludes an unhealthy HTTP endpoint", all(json.loads(request(port, "/health")[2])["port"] == second.server_port for _ in range(4)))
            # Named and transport-specific pools can finish their probes at different times.
            wait_for(lambda: metric_value(request(admin, "/metrics")[2].decode(), "rgnix_backend_endpoints", backend="healthy", state="unready"), 1)
            metrics = request(admin, "/metrics")[2].decode()
            check("backend metrics distinguish total, eligible and unready endpoints", metric_value(metrics, "rgnix_backend_endpoints", backend="healthy", state="total") == 2 and metric_value(metrics, "rgnix_backend_endpoints", backend="healthy", state="eligible") == 1 and metric_value(metrics, "rgnix_backend_endpoints", backend="healthy", state="unready") == 1)
            first.healthy = True
            body = json.dumps({"count": 7, "enabled": True})
            check("compiled RGL reads typed JSON, decoded args and cookies", request(port, "/body?tenant=a+b", "POST", {"Cookie": "group=blue"}, body)[0] == 201)
            check("typed JSON getters use fallbacks for wrong types", request(port, "/body?tenant=a+b", "POST", {"Cookie": "group=blue"}, '{"count":7.2,"enabled":true}')[0] == 202)
            claims = {"iss": "qa", "aud": "api", "exp": int(time.time()) + 120, "tenant": "blue"}
            good = jwt(root, claims)
            check("JWT verifies signatures and makes claims available to RGL", request(port, "/jwt", headers={"Authorization": "Bearer " + good})[0] == 200)
            check("JWT rejects missing, wrong audience, expired and tampered tokens", all(request(port, "/jwt", headers={"Authorization": "Bearer " + token})[0] == 401 for token in ["", jwt(root, {**claims, "aud": "wrong"}), jwt(root, {**claims, "exp": 0}), good[:-4] + "aaaa"]))
            check("external auth replaces spoofed identity headers", json.loads(request(port, "/auth", headers={"Authorization": "Bearer yes", "x-tenant": "forged"})[2])["headers"]["x-tenant"] == "verified")
            check("external auth fails closed on deny and service failure", request(port, "/auth")[0] == 401 and request(port, "/auth", headers={"Authorization": "Bearer unavailable"})[0] == 503)
            check("alias maps the matched prefix to its directory", request(port, "/assets/file.txt")[2] == b"asset\n")
            check("SPA fallback serves the shell within its root", request(port, "/spa/deep/link")[2] == page)
            gz = request(port, "/spa/deep/link", headers={"Accept-Encoding": "gzip"})
            check("gzip transforms streamed responses and adjusts framing", gz[1].get("content-encoding") == "gzip" and gzip.decompress(gz[2]) == page and "content-length" not in gz[1] and "accept-encoding" in gz[1].get("vary", "").lower())
            import brotli
            br = request(port, "/spa/deep/link", headers={"Accept-Encoding": "br"})
            check("Brotli negotiation produces a valid stream", br[1].get("content-encoding") == "br" and brotli.decompress(br[2]) == page)
            br = request(port, "/spa/index.html", headers={"Accept-Encoding": "gzip;q=0, br;q=0.5"})
            identity = request(port, "/spa/index.html", headers={"Accept-Encoding": "gzip;q=0, br;q=0, *;q=1"})
            check("compression respects quality weights and explicit wildcard exclusions", br[1].get("content-encoding") == "br" and brotli.decompress(br[2]) == page and "content-encoding" not in identity[1] and identity[2] == page)
            unchanged = request(port, "/spa/index.html", headers={"Accept-Encoding": "gzip", "Cache-Control": "no-transform"})
            check("request no-transform prevents response compression", "content-encoding" not in unchanged[1] and unchanged[2] == page)
            ranged = request(port, "/spa/index.html", headers={"Accept-Encoding": "gzip", "Range": "bytes=0-4"})
            check("compression preserves range representation boundaries", ranged[0] == 206 and ranged[2] == page[:5] and "content-encoding" not in ranged[1])
            check("PROXY v1 is consumed before HTTP and preserves original peer", proxy_request(proxy_port, b"PROXY TCP4 203.0.113.7 192.0.2.1 1234 80\r\n")[1] == b"203.0.113.7|127.0.0.1")
            payload = socket.inet_pton(socket.AF_INET6, "2001:db8::8") + socket.inet_pton(socket.AF_INET6, "2001:db8::1") + struct.pack("!HH", 1234, 80)
            check("PROXY v2 accepts bounded IPv6 TCP addresses", proxy_request(proxy_port, b"\r\n\r\n\x00\r\nQUIT\n" + bytes([0x21, 0x21]) + struct.pack("!H", len(payload)) + payload)[1] == b"2001:db8::8|127.0.0.1")
            context = ssl.create_default_context(cafile=str(root / "ca.crt"))
            context.load_cert_chain(root / "client.crt", root / "client.key")
            conn = http.client.HTTPSConnection("127.0.0.1", mtls, context=context, timeout=3)
            conn.request("GET", "/")
            response = conn.getresponse()
            check("mTLS accepts a client issued by the configured CA", response.status == 200 and response.read() == b"mutual")
            denied = False
            try:
                request(mtls, tls=True)
            except (OSError, http.client.HTTPException):
                denied = True
            check("mTLS rejects an anonymous TLS client", denied)
            check("admin diagnostics require a token", request(admin, "/v1/config")[0] == 401)
            auth = {"Authorization": "Bearer " + token}
            config_doc = json.loads(request(admin, "/v1/config", headers=auth)[2])
            check("effective configuration and backend state are inspectable", config_doc["version"] == 1 and "pool" in config_doc["backends"])
            explanation = json.loads(request(admin, f"/v1/explain?listener=127.0.0.1:{port}&host=example.test&path=/assets/file.txt", headers=auth)[2])
            check("route explain reports the selected prefix", explanation["selected"]["match"] == {"NginxPrefix": "/assets/"})
            config.write_text(config.read_text().replace('"version-one"', '"version-two"'))
            process.send_signal(signal.SIGHUP)
            wait_for(lambda: request(port)[2], b"version-two")
            check("authenticated rollback republishes an immutable file version", request(admin, "/v1/rollback/1", "POST", auth)[0] == 200 and request(port)[2] == b"version-one")
            check("rollback increments configuration version", json.loads(request(admin, "/v1/history", headers=auth)[2])["current"] == 3)
            config.write_text(config.read_text().replace(f"ssl_client_certificate {root}/ca.crt", f"ssl_client_certificate {root}/other-ca.crt"))
            process.send_signal(signal.SIGHUP)
            wait_for(lambda: json.loads(request(admin, "/v1/history", headers=auth)[2])["current"], 4)
            conn.request("GET", "/")
            response = conn.getresponse()
            check("CA rotation also rejects old clients on reused TLS connections", response.status == 403)
            response.read()
            conn.close()
            fixture = root / "request.json"
            fixture.write_text(json.dumps({"listener": f"127.0.0.1:{port}", "host": "example.test", "path": "/body?tenant=a+b", "headers": {"Cookie": "group=blue"}, "body": body}))
            simulated = json.loads(command(binary, "simulate", "-c", str(config), "--request", str(fixture)))
            check("offline simulation executes the compiled plugin", simulated["plugin"]["decision"]["status"] == 201)
            metrics = request(admin, "/metrics")[2].decode()
            Path(".local/metrics-product.prom").write_text(metrics)
            check("metrics distinguish configured routes and backends", "rgnix_route_requests_total{" in metrics and "rgnix_backend_requests_total{" in metrics)
            check("metrics expose upstream phases, pool reuse and response bytes", metric_value(metrics, "rgnix_upstream_connect_seconds_count", backend="http://sticky", reused="true") > 0 and metric_value(metrics, "rgnix_upstream_header_seconds_count", backend="http://sticky") == 5 and metric_value(metrics, "rgnix_upstream_request_seconds_count", backend="http://sticky") == 5 and metric_value(metrics, "rgnix_response_body_bytes_total") > len(page))
            check("metrics expose active config and successful update timing", metric_value(metrics, "rgnix_config_resources", kind="listeners") == 4 and metric_value(metrics, "rgnix_config_update_seconds_count", source="file", result="success") == 2 and metric_value(metrics, "rgnix_config_last_success_timestamp_seconds") > time.time() - 60)
            if sys.platform == "linux":
                check("Linux metrics expose real process memory, file descriptors and threads", metric_value(metrics, "process_resident_memory_bytes") > 0 and metric_value(metrics, "process_open_fds") > 0 and metric_value(metrics, "process_threads") > 0 and metric_value(metrics, "process_start_time_seconds") > time.time() - 180)
            config.write_text("\n".join(line for line in config.read_text().splitlines() if "upstream bounded " not in line and "location /backend-limit " not in line))
            process.send_signal(signal.SIGHUP)
            wait_for(lambda: metric_value(request(admin, "/metrics")[2].decode(), "rgnix_config_version"), 5)
            metrics = request(admin, "/metrics")[2].decode()
            check("removed backends disappear from current-state metrics while historical counters remain", 'rgnix_backend_endpoints{backend="bounded"' not in metrics and 'rgnix_backend_endpoints{backend="http://bounded"' not in metrics and 'rgnix_backend_requests_total{backend="http://bounded"' in metrics)
            grpc_cases(binary, root, first)
            observability_cases(binary, root, first)
            security_transport_cases(binary, root)
            platform_cases(binary, root)
            governance_cases(binary, root, first)
        except Exception:
            if (root / "stderr").exists():
                print((root / "stderr").read_text()[-10000:], file=sys.stderr)
            raise
        finally:
            if process and process.poll() is None:
                process.send_signal(signal.SIGINT)
                process.wait(timeout=15)
            first.shutdown()
            second.shutdown()
    result=json.dumps({"passed": len(RESULTS), "checks": RESULTS}, indent=2)
    Path(".local").mkdir(exist_ok=True)
    Path(".local/product-features.json").write_text(result+"\n")
    print(result)


def security_transport_cases(binary, root):
    server = start_backend(root)
    server.jwks = (root / "jwks.json").read_bytes()
    port, admin, proxy_tls = free_port(), free_port(), free_port()
    config = root / "security-transport.conf"
    config.write_text(f'''http {{ server {{ listen 127.0.0.1:{port};
location / {{ return 200 "before"; }}
location /allowed {{ proxy_pass https://127.0.0.1:{server.server_port}; proxy_ssl_name example.test; proxy_ssl_trusted_certificate {root}/ca.crt; }}
location /denied {{ proxy_pass https://127.0.0.1:{server.server_port}; proxy_ssl_name example.test; proxy_ssl_trusted_certificate {root}/other-ca.crt; }}
location /jwt {{ rgnix_jwt jwks=https://127.0.0.1:{server.server_port}/jwks issuer=qa audience=api; return 200 "verified"; }}
}} server {{ listen 127.0.0.1:{proxy_tls} ssl proxy_protocol;
set_real_ip_from 127.0.0.0/8; real_ip_header proxy_protocol;
ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key;
return 200 "$remote_addr";
}} }}''')
    process = None
    try:
        with open(root / "security-transport.log", "w+") as output:
            process = subprocess.Popen([binary, "serve", "-c", str(config), "--admin", f"127.0.0.1:{admin}"], stdout=output, stderr=output, env={**os.environ, "SSL_CERT_FILE": str(root / "ca.crt")})
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            check("HTTPS upstream validates a private CA and explicit server name", request(port, "/allowed")[0] == 200)
            check("upstream pool cannot reuse a connection across different CA policies", request(port, "/denied")[0] == 502)
            context = ssl.create_default_context(cafile=str(root / "ca.crt"))
            conn = http.client.HTTPConnection("127.0.0.1", proxy_tls, timeout=3)
            raw = socket.create_connection(("127.0.0.1", proxy_tls), timeout=3)
            raw.sendall(b"PROXY TCP4 203.0.113.45 127.0.0.1 1234 443\r\n")
            conn.sock = context.wrap_socket(raw, server_hostname="example.test")
            conn.request("GET", "/", headers={"Host": "example.test"})
            response = conn.getresponse()
            check("PROXY identity is consumed before the TLS handshake", response.status == 200 and response.read() == b"203.0.113.45")
            conn.close()
            claims = {"iss": "qa", "aud": "api", "exp": int(time.time()) + 180}
            old = jwt(root, claims)
            check("remote HTTPS JWKS authenticates a signed token", request(port, "/jwt", headers={"Authorization": "Bearer " + old})[0] == 200)
            rotated = root / "rotated"
            rotated.mkdir()
            command("openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", str(rotated / "jwt.key"))
            modulus = command("openssl", "rsa", "-in", str(rotated / "jwt.key"), "-noout", "-modulus").decode().strip().split("=", 1)[1]
            server.jwks = json.dumps({"keys": [{"kty": "RSA", "kid": "test", "alg": "RS256", "n": b64(bytes.fromhex(modulus)), "e": "AQAB"}]}).encode()
            new = jwt(rotated, claims)
            deadline = time.monotonic() + 40
            while request(port, "/jwt", headers={"Authorization": "Bearer " + new})[0] != 200 and time.monotonic() < deadline:
                time.sleep(.25)
            check("remote JWKS rotation takes effect without config reload and revokes old keys", request(port, "/jwt", headers={"Authorization": "Bearer " + new})[0] == 200 and request(port, "/jwt", headers={"Authorization": "Bearer " + old})[0] == 401)
            config.write_text(config.read_text().replace('"before"', '"after"').replace(f"proxy_ssl_trusted_certificate {root}/ca.crt", f"proxy_ssl_trusted_certificate {root}/other-ca.crt"))
            process.send_signal(signal.SIGHUP)
            wait_for(lambda: request(port)[2], b"after")
            check("upstream CA reload prevents reuse of previously trusted connections", request(port, "/allowed")[0] == 502)
    except Exception:
        print((root / "security-transport.log").read_text()[-6000:], file=sys.stderr)
        raise
    finally:
        if process and process.poll() is None:
            process.send_signal(signal.SIGINT)
            process.wait(timeout=15)
        server.shutdown()


def grpc_cases(binary, root, _):
    import grpc
    server = grpc.server(concurrent.futures.ThreadPoolExecutor(max_workers=4))
    def chat(iterator, context):
        context.set_trailing_metadata((("x-qa", "done"),))
        for message in iterator:
            yield message.upper()
    def fail(_request, context):
        context.abort(grpc.StatusCode.UNAVAILABLE, "synthetic failure")
    server.add_generic_rpc_handlers((grpc.method_handlers_generic_handler("qa.Echo", {"Chat": grpc.stream_stream_rpc_method_handler(chat), "Fail": grpc.unary_unary_rpc_method_handler(fail)}),))
    class Collector(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_): pass
        def do_POST(self):
            self.server.records.append(self.rfile.read(int(self.headers["Content-Length"])))
            self.send_response(200); self.send_header("Content-Length","0"); self.end_headers()
    collector = http.server.ThreadingHTTPServer(("127.0.0.1",0), Collector)
    collector.records = []
    threading.Thread(target=collector.serve_forever,daemon=True).start()
    upstream = server.add_insecure_port("127.0.0.1:0")
    server.start()
    port, admin = free_port(), free_port()
    config = root / "grpc.conf"
    config.write_text(f'''http {{ server {{ listen 127.0.0.1:{port} ssl; http2 on; ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key;
    location / {{ proxy_http_version 2; proxy_pass http://127.0.0.1:{upstream};
    gzip on; gzip_min_length 0; gzip_types application/grpc text/event-stream;
    }} }} }}''')
    with open(root / "grpc.log", "w+") as output:
        process = subprocess.Popen([binary, "serve", "-c", str(config), "--admin", f"127.0.0.1:{admin}"], stdout=output, stderr=output, env={**os.environ,"OTEL_EXPORTER_OTLP_TRACES_ENDPOINT":f"http://127.0.0.1:{collector.server_port}/v1/traces","RGNIX_TRACE_SAMPLE_RATIO":"1"})
        try:
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            credentials = grpc.ssl_channel_credentials((root / "ca.crt").read_bytes())
            channel = grpc.secure_channel(f"127.0.0.1:{port}", credentials)
            release = threading.Event()
            def messages():
                yield b"first"
                if not release.wait(3):
                    raise AssertionError("response was buffered until request EOF")
                yield b"second"
            call = channel.stream_stream("/qa.Echo/Chat")(messages(), timeout=8, metadata=(("accept-encoding", "gzip"),))
            check("gRPC h2c upstream returns data before client request EOF", next(call) == b"FIRST")
            release.set()
            check("gRPC bidirectional stream and trailers survive proxying", list(call) == [b"SECOND"] and call.code() == grpc.StatusCode.OK and ("x-qa", "done") in call.trailing_metadata())
            check("HTTP compression cannot corrupt a gRPC stream even with an explicit MIME rule", "content-encoding" not in dict(call.initial_metadata()))
            failure=None
            try: channel.unary_unary("/qa.Echo/Fail")(b"test",timeout=3)
            except grpc.RpcError as error: failure=error.code()
            check("gRPC failure status survives proxy trailers",failure==grpc.StatusCode.UNAVAILABLE)
            wait_for(lambda:any('grpc_status="14"' in line and line.endswith(' 1') for line in request(admin,"/metrics")[2].decode().splitlines()),True)
            check("gRPC metrics distinguish OK and UNAVAILABLE independently of HTTP 200",any('grpc_status="0"' in line and line.endswith(' 1') for line in request(admin,"/metrics")[2].decode().splitlines()))
            def failure_spans():
                spans=[]
                for payload in collector.records:
                    for resource in wire_fields(payload).get(1,[]):
                        for scope in wire_fields(resource).get(2,[]):
                            for span in wire_fields(scope).get(2,[]):
                                fields=wire_fields(span)
                                if fields.get(15) and wire_fields(fields[15][0]).get(3)==[2]:spans.append(fields)
                return spans
            wait_for(lambda:len(failure_spans())>=2,True)
            check("OTLP marks both server and client spans as failed for gRPC errors",{span[6][0] for span in failure_spans()}=={2,3})
            channel.close()
        finally:
            process.send_signal(signal.SIGINT)
            process.wait(timeout=15)
            server.stop(0)
            collector.shutdown()


def wire_fields(data):
    fields, position = {}, 0
    def number():
        nonlocal position
        result, shift = 0, 0
        while True:
            byte = data[position]
            position += 1
            result |= (byte & 127) << shift
            if byte < 128:
                return result
            shift += 7
    while position < len(data):
        tag = number()
        kind = tag & 7
        if kind == 0:
            value = number()
        else:
            length = number() if kind == 2 else {1: 8, 5: 4}[kind]
            value = data[position:position + length]
            position += length
        fields.setdefault(tag >> 3, []).append(value)
    return fields


def observability_cases(binary, root, upstream):
    class Collector(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass
        def do_POST(self):
            self.server.records.append((self.path, self.rfile.read(int(self.headers["Content-Length"]))))
            self.send_response(200)
            self.send_header("Content-Type", "application/x-protobuf")
            self.send_header("Content-Length", "0")
            self.end_headers()
    collector = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Collector)
    collector.records = []
    threading.Thread(target=collector.serve_forever, daemon=True).start()
    port, admin = free_port(), free_port()
    config = root / "trace.conf"
    access = root / "trace.access"
    upstream.auth_traces = []
    config.write_text(f"""http {{ access_log {access}; server {{ listen 127.0.0.1:{port};
location / {{ proxy_pass http://127.0.0.1:{upstream.server_port}; }}
location /auth {{ rgnix_auth_request http://127.0.0.1:{upstream.server_port}/authorize; proxy_pass http://127.0.0.1:{upstream.server_port}; }}
location /tamper {{ proxy_set_header traceparent invalid; proxy_set_header tracestate forged=value; proxy_pass http://127.0.0.1:{upstream.server_port}; }}
location /status {{ proxy_pass http://127.0.0.1:{upstream.server_port}/authorize; }}
}} }}""")
    environment = {k: v for k, v in os.environ.items() if not k.startswith("OTEL_")}
    environment.update({"OTEL_BSP_SCHEDULE_DELAY": "40", "OTEL_BLRP_SCHEDULE_DELAY": "40"})
    with open(root / "trace.log", "w+") as output:
        process = subprocess.Popen([binary, "serve", "-c", str(config), "--admin", f"127.0.0.1:{admin}",
                                    "--otlp-logs-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/logs",
                                    "--otlp-traces-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/traces", "--trace-sample-ratio", "0"], env=environment, stdout=output, stderr=output)
        try:
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            trace_id, parent = "0123456789abcdef" * 2, "1122334455667788"
            state = "rojo=one,congo=two"
            response = request(port, "/test?secret=private-query", headers={"traceparent": f"00-{trace_id}-{parent}-01", "tracestate": state})
            propagated = json.loads(response[2])["headers"]["traceparent"]
            def records(path):
                result = []
                for route, payload in collector.records:
                    if route != path:
                        continue
                    for resource in wire_fields(payload).get(1, []):
                        for scope in wire_fields(resource).get(2, []):
                            result.extend(wire_fields(record) for record in wire_fields(scope).get(2, []))
                return result
            wait_for(lambda: len(records("/v1/traces")), 2)
            wait_for(lambda: len(records("/v1/logs")), 1)
            spans, logs = records("/v1/traces"), records("/v1/logs")
            server = next(span for span in spans if span[6] == [2])
            client = next(span for span in spans if span[6] == [3])
            check("OTLP exports linked server and client spans", server[1] == client[1] == [bytes.fromhex(trace_id)] and server[4] == [bytes.fromhex(parent)] and client[4] == server[2])
            check("traceparent propagates the upstream client span identity", propagated == f"00-{trace_id}-{client[2][0].hex()}-01")
            check("tracestate reaches the backend and both OTLP spans", json.loads(response[2])["headers"]["tracestate"] == state and server[3] == client[3] == [state.encode()])
            check("span flags identify remote server parents and local client parents", int.from_bytes(server[16][0], "little") == 0x301 and int.from_bytes(client[16][0], "little") == 0x101)
            check("proxy client span timestamps are contained by the server span", int.from_bytes(server[7][0], "little") <= int.from_bytes(client[7][0], "little") <= int.from_bytes(client[8][0], "little") <= int.from_bytes(server[8][0], "little"))
            check("OTLP access logs correlate with the server span", logs[0][9] == server[1] and logs[0][10] == server[2])
            wait_for(lambda: access.exists() and trace_id in access.read_text(), True)
            check("local access logs include trace, parent and outbound span IDs", f'span_id="{server[2][0].hex()}"' in access.read_text() and f'parent_span_id="{parent}"' in access.read_text() and f'upstream_span_id="{client[2][0].hex()}"' in access.read_text() and 'trace_sampled=true' in access.read_text())
            response = request(port, "/unsampled", headers={"traceparent": f"00-{trace_id}-{parent}-00"})
            wait_for(lambda: len(records("/v1/logs")), 2)
            check("parent sampling decisions are preserved without exporting unsampled spans", len(records("/v1/traces")) == 2 and json.loads(response[2])["headers"]["traceparent"].endswith("-00"))
            check("pre-routing rejected requests are logged", request(port, "/bad%GG")[0] == 400)
            wait_for(lambda: len(records("/v1/logs")), 3)
            check("trace payload excludes query and credential content", all(b"private-query" not in payload for path, payload in collector.records if path == "/v1/traces"))

            def traced_request(path="/test", state="vendor=opaque", version="00", flags="01", suffix=""):
                ident = os.urandom(16).hex()
                reply = request(port, path, headers={"traceparent": f"{version}-{ident}-{parent}-{flags}{suffix}", "tracestate": state, "Authorization": "Bearer yes"})
                wait_for(lambda: len([s for s in records("/v1/traces") if s.get(1) == [bytes.fromhex(ident)]]), 3 if path == "/auth" else 2)
                return ident, reply, [s for s in records("/v1/traces") if s.get(1) == [bytes.fromhex(ident)]]

            ident, reply, spans = traced_request(version="02", suffix="-future-data")
            check("future traceparent versions preserve the trace while forwarding supported fields", json.loads(reply[2])["headers"]["traceparent"].startswith("00-" + ident) and all(s[3] == [b"vendor=opaque"] for s in spans))
            ident, reply, spans = traced_request(state="duplicate=a,duplicate=b")
            check("invalid tracestate is dropped without breaking a valid traceparent", "tracestate" not in json.loads(reply[2])["headers"] and all(3 not in s for s in spans))
            large_state = ",".join(f"v{n}=" + "x" * 120 for n in range(6))
            ident, reply, spans = traced_request(state=large_state)
            kept_state = ",".join(large_state.split(",")[:4])
            check("oversized tracestate retains whole vendor entries within the propagation limit", json.loads(reply[2])["headers"]["tracestate"] == kept_state and all(s[3] == [kept_state.encode()] for s in spans))
            connection = http.client.HTTPConnection("127.0.0.1", port)
            connection.putrequest("GET", "/multiple-state")
            connection.putheader("traceparent", f"00-{os.urandom(16).hex()}-{parent}-01")
            connection.putheader("tracestate", "rojo=one, ,")
            connection.putheader("tracestate", "congo=two")
            connection.endheaders()
            reply = connection.getresponse()
            check("multiple tracestate headers preserve vendor order and ignore empty members", json.loads(reply.read())["headers"]["tracestate"] == state)
            connection.close()
            ident, reply, spans = traced_request("/tamper")
            check("route header edits cannot detach propagation from the recorded spans", json.loads(reply[2])["headers"]["traceparent"].startswith("00-" + ident) and json.loads(reply[2])["headers"]["tracestate"] == "vendor=opaque")
            ident, reply, spans = traced_request("/auth")
            auth_span = next(s for s in spans if s[5] == [b"GET auth"])
            server = next(s for s in spans if s[6] == [2])
            clients = [s for s in spans if s[6] == [3]]
            check("external authorization creates a distinct child span and propagates W3C context", len({s[2][0] for s in clients}) == 2 and all(s[4] == server[2] for s in clients) and upstream.auth_traces[-1] == {"traceparent": f"00-{ident}-{auth_span[2][0].hex()}-01", "tracestate": "vendor=opaque"})
            ident = os.urandom(16).hex()
            request(port, "/status", headers={"traceparent": f"00-{ident}-{parent}-01"})
            wait_for(lambda: len([s for s in records("/v1/traces") if s.get(1) == [bytes.fromhex(ident)]]), 2)
            spans = [s for s in records("/v1/traces") if s.get(1) == [bytes.fromhex(ident)]]
            check("HTTP 401 marks the client span as error and leaves server status unset", all(wire_fields(s[15][0]).get(3, [0]) == ([2] if s[6] == [3] else [0]) for s in spans))

            edge, edge_admin = free_port(), free_port()
            edge_config = root / "trace-edge.conf"
            edge_log = root / "trace-edge.access"
            edge_config.write_text(f"http {{ access_log {edge_log} json; server {{ listen 127.0.0.1:{edge}; location / {{ proxy_pass http://127.0.0.1:{port}; }} }} }}")
            with open(root / "trace-edge.log", "w+") as edge_output:
                gateway = subprocess.Popen([binary, "serve", "-c", str(edge_config), "--admin", f"127.0.0.1:{edge_admin}", "--otlp-logs-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/logs", "--otlp-traces-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/traces", "--trace-sample-ratio", "1"], env=environment, stdout=edge_output, stderr=edge_output)
                try:
                    wait_for(lambda: request(edge_admin, "/readyz")[0], 200)
                    reply = request(edge, "/chain")
                    forwarded = json.loads(reply[2])["headers"]["traceparent"]
                    ident = bytes.fromhex(forwarded.split('-')[1])
                    wait_for(lambda: len([s for s in records("/v1/traces") if s.get(1) == [ident]]), 4)
                    chain = [s for s in records("/v1/traces") if s.get(1) == [ident]]
                    root_span = next(s for s in chain if 4 not in s)
                    cursor = root_span[2]
                    for _ in range(3):
                        cursor = next(s for s in chain if s.get(4) == cursor)[2]
                    check("two proxy hops form one trace with four linked spans and forward the last client ID", cursor == [bytes.fromhex(forwarded.split('-')[2])] and root_span[6] == [2])
                    wait_for(lambda: len([s for s in records("/v1/logs") if s.get(9) == [ident]]), 2)
                    logs = [s for s in records("/v1/logs") if s.get(9) == [ident]]
                    wait_for(lambda: edge_log.exists() and ident.hex() in edge_log.read_text())
                    check("both hops OTLP and JSON access logs correlate with their own server spans", {s[10][0] for s in logs} == {s[2][0] for s in chain if s[6] == [2]} and any(json.loads(line)["trace_id"] == ident.hex() and json.loads(line)["trace_sampled"] for line in edge_log.read_text().splitlines()))
                    connection = http.client.HTTPConnection("127.0.0.1", edge)
                    connection.putrequest("GET", "/duplicate")
                    for _ in range(2): connection.putheader("traceparent", f"00-{trace_id}-{parent}-01")
                    connection.putheader("tracestate", "old=must-not-survive")
                    connection.endheaders()
                    response = connection.getresponse()
                    headers = json.loads(response.read())["headers"]
                    connection.close()
                    check("duplicate parents start a new trace and cannot retain old tracestate", headers["traceparent"].split('-')[1] != trace_id and "tracestate" not in headers)
                    ident = os.urandom(16).hex()
                    reply = request(edge, "/unsampled-chain", headers={"traceparent": f"00-{ident}-{parent}-00", "tracestate": state})
                    wait_for(lambda: len([r for r in records("/v1/logs") if r.get(9) == [bytes.fromhex(ident)]]), 2)
                    check("unsampled W3C context and correlated logs survive both proxy hops", not any(s.get(1) == [bytes.fromhex(ident)] for s in records("/v1/traces")) and json.loads(reply[2])["headers"]["traceparent"].endswith("-00") and json.loads(reply[2])["headers"]["tracestate"] == state)
                    for invalid in [f"ff-{trace_id}-{parent}-01", f"00-{'0' * 32}-{parent}-01", f"00-{trace_id}-{'0' * 16}-01", f"00-{trace_id.upper()}-{parent}-01", f"00-{trace_id}-{parent}-01-extra"]:
                        reply = request(edge, "/bad-context", headers={"traceparent": invalid, "tracestate": "orphan=discard"})
                        echoed = json.loads(reply[2])["headers"]
                        assert echoed["traceparent"].split('-')[1] not in (trace_id, '0' * 32) and "tracestate" not in echoed
                    check("invalid versions, zero IDs and malformed context start clean traces", True)
                finally:
                    gateway.send_signal(signal.SIGINT)
                    gateway.wait(timeout=15)
                gateway = subprocess.Popen([binary, "serve", "-c", str(edge_config), "--admin", f"127.0.0.1:{edge_admin}", "--otlp-logs-endpoint", f"http://127.0.0.1:{collector.server_port}/v1/logs"], env={**environment, "OTEL_TRACES_EXPORTER": "none"}, stdout=edge_output, stderr=edge_output)
                try:
                    wait_for(lambda: request(edge_admin, "/readyz")[0], 200)
                    ident = os.urandom(16).hex()
                    request(edge, "/pass-through", headers={"traceparent": f"00-{ident}-{parent}-01", "tracestate": state})
                    wait_for(lambda: len([r for r in records("/v1/logs") if r.get(9) == [bytes.fromhex(ident)]]), 2)
                    logs = [r for r in records("/v1/logs") if r.get(9) == [bytes.fromhex(ident)]]
                    passthrough = next(r for r in logs if r[10] == [bytes.fromhex(parent)])
                    check("disabled tracing preserves incoming sampling in correlated OTLP logs", int.from_bytes(passthrough[8][0], "little") == 1 and len([s for s in records("/v1/traces") if s.get(1) == [bytes.fromhex(ident)]]) == 2)
                finally:
                    gateway.send_signal(signal.SIGINT)
                    gateway.wait(timeout=15)
        finally:
            process.send_signal(signal.SIGINT)
            process.wait(timeout=15)
            collector.shutdown()



def platform_cases(binary, root):
    class Auth(Backend):
        def do_GET(self):
            self.server.calls += 1
            return super().do_GET()
    auth = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Auth)
    auth.calls = 0
    threading.Thread(target=auth.serve_forever, daemon=True).start()
    secure = start_backend(root)
    mtls = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    mtls.healthy = True
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(root / "server.crt", root / "server.key")
    context.load_verify_locations(root / "ca.crt")
    context.verify_mode = ssl.CERT_REQUIRED
    mtls.socket = context.wrap_socket(mtls.socket, server_side=True)
    threading.Thread(target=mtls.serve_forever, daemon=True).start()
    port, admin = free_port(), free_port()
    conf, access, audit = root/"platform.conf", root/"platform-access.jsonl", root/"audit.jsonl"
    write_token, read_token = "w"*48, "r"*48
    (root/"writer").write_text(write_token)
    (root/"reader").write_text(read_token)
    (root/"historical.rgl").write_text('function on_request() return resp.reply(200,"original") end')
    command("openssl", "pkey", "-in", str(root/"client.key"), "-traditional", "-out", str(root/"client-traditional.key"))
    conf.write_text(f'''http {{
access_log {access} json; rgnix_log_client off;
upstream checked_mtls {{ server 127.0.0.1:{mtls.server_port}; rgnix_health_check /health interval=1s timeout=500ms; }}
upstream checked {{ server 127.0.0.1:{secure.server_port}; rgnix_health_check /health interval=1s timeout=500ms; }}
server {{ listen 127.0.0.1:{port};
location / {{ return 200 "one"; }}
location /historical {{ rgnix_script {root}/historical.rgl; }}
location /health {{ proxy_pass https://checked; proxy_ssl_name example.test; proxy_ssl_trusted_certificate {root}/ca.crt; }}
location /mtls {{ proxy_pass https://127.0.0.1:{mtls.server_port}; proxy_ssl_name example.test; proxy_ssl_trusted_certificate {root}/ca.crt; proxy_ssl_certificate {root}/client.crt; proxy_ssl_certificate_key {root}/client.key; }}
location /mtls-health {{ proxy_pass https://checked_mtls; proxy_ssl_name example.test; proxy_ssl_trusted_certificate {root}/ca.crt; proxy_ssl_certificate {root}/client.crt; proxy_ssl_certificate_key {root}/client-traditional.key; }}
location /anonymous {{ proxy_pass https://127.0.0.1:{mtls.server_port}; proxy_ssl_name example.test; proxy_ssl_trusted_certificate {root}/ca.crt; }}
location /auth {{ rgnix_limit_rate 1 burst=1 key=route; rgnix_auth_request http://127.0.0.1:{auth.server_port}/authorize; return 200 "authorized"; }}
location /jwt {{ rgnix_jwt jwks={root}/jwks.json issuer=qa audience=api; return 200 "verified"; }}
}} }}''')
    args=[binary,"serve","-c",str(conf),"--admin",f"127.0.0.1:{admin}","--admin-token-file",str(root/"writer"),"--admin-read-token-file",str(root/"reader"),"--admin-audit-file",str(audit),"--history-dir",str(root/"history")]
    bad=root/"bad-include.conf"
    bad.write_text("http { include broken.conf; }")
    (root/"broken.conf").write_text("server { listen 127.0.0.1:1; unknown_directive on; }")
    invalid=subprocess.run([binary,"serve","-c",str(bad)],capture_output=True,text=True)
    check("archived configuration compilation preserves original include diagnostics",invalid.returncode!=0 and str(root/"broken.conf")+":1:" in invalid.stderr)
    process=None
    with open(root/"platform.log","w+") as output:
        try:
            process=subprocess.Popen(args,stdout=output,stderr=output)
            wait_for(lambda:request(port)[2],b"one")
            wait_for(lambda:request(port,"/health")[0],200)
            check("HTTPS health probes reuse route SNI and private CA",True)
            wait_for(lambda:request(port,"/mtls-health")[0],200)
            check("mTLS health probes accept traditional PEM private keys",True)
            check("upstream mTLS presents client identity and isolates anonymous pools", request(port,"/mtls")[0]==200 and request(port,"/anonymous")[0]==502)
            statuses=[request(port,"/auth",headers={"Authorization":"Bearer yes"})[0] for _ in range(2)]
            check("pre-auth rate admission protects the auth service",statuses==[200,429] and auth.calls==1)
            request(port,"/?access_token=SYNTHETIC-TOKEN&ok=yes",headers={"Referer":"https://example.test/?password=SYNTHETIC-PASSWORD"})
            wait_for(lambda:access.exists() and "REDACTED" in access.read_text(),True)
            lines=[json.loads(v) for v in access.read_text().splitlines()]
            check("JSON local logs redact URI and Referer query credentials", "SYNTHETIC-TOKEN" not in access.read_text() and "SYNTHETIC-PASSWORD" not in access.read_text() and any("ok=yes" in v["uri"] for v in lines))
            check("local log client suppression is applied",all(v["client"]=="-" for v in lines))
            writer={"Authorization":"Bearer "+write_token}
            reader={"Authorization":"Bearer "+read_token}
            check("read-only admin can inspect but cannot roll back",request(admin,"/v1/config",headers=reader)[0]==200 and request(admin,"/v1/rollback/1","POST",reader)[0]==403)
            fixture={"path":"/auth","headers":{"authorization":"Bearer yes"},"external_auth":{"status":200},"repeat":2}
            simulation=json.loads(request(admin,"/v1/simulate","POST",reader,json.dumps(fixture).encode())[2])
            check("management simulation includes pre-auth rate budgets",simulation["results"][0]["admitted"] and simulation["results"][1]["status"]==429 and auth.calls==1)
            fixture={"path":"/jwt"}
            denied=json.loads(request(admin,"/v1/simulate","POST",reader,json.dumps(fixture).encode())[2])
            fixture["headers"]={"Authorization":"Bearer "+jwt(root,{"iss":"qa","aud":"api","exp":int(time.time())+300})}
            admitted=json.loads(request(admin,"/v1/simulate","POST",reader,json.dumps(fixture).encode())[2])
            check("simulation verifies actual JWT signatures and claims",denied["status"]==401 and admitted["admitted"])
            conf.write_text(conf.read_text().replace('"one"','"two"'))
            (root/"historical.rgl").write_text('function on_request() return resp.reply(200,"changed") end')
            process.send_signal(signal.SIGHUP)
            wait_for(lambda:request(port)[2],b"two")
            check("durable publication captures plugin dependencies",request(port,"/historical")[2]==b"changed")
            check("rollback restores the archived plugin and configuration",request(admin,"/v1/rollback/1","POST",writer)[0]==200 and request(port,"/historical")[2]==b"original")
            process.send_signal(signal.SIGINT);process.wait(timeout=15)
            conf.write_text("invalid configuration")
            (root/"historical.rgl").unlink()
            process=subprocess.Popen(args,stdout=output,stderr=output)
            wait_for(lambda:request(port)[2],b"one")
            check("restart restores committed rollback despite broken or missing source files",request(port,"/historical")[2]==b"original")
            history=json.loads(request(admin,"/v1/history",headers=reader)[2])
            check("durable history preserves version identity across restart",history["current"]==3 and history["durable_versions"]==[1,2,3])
            check("historical versions remain usable after restart",request(admin,"/v1/rollback/2","POST",writer)[0]==200 and request(port,"/historical")[2]==b"changed")
            records=[json.loads(line) for line in audit.read_text().splitlines()]
            check("management audit records denied and committed changes without tokens",any(r["actor"]=="reader" and r["result"]=="403" for r in records) and any(r["operation"]=="publish" and r["result"]=="committed" for r in records) and write_token not in audit.read_text())
            check("durable history protects private assets with owner-only permissions",(root/"history/history.json").stat().st_mode&0o777==0o600)
        except Exception:
            print((root/"platform.log").read_text()[-7000:],file=sys.stderr)
            raise
        finally:
            if process and process.poll() is None:process.send_signal(signal.SIGINT);process.wait(timeout=15)
            auth.shutdown();secure.shutdown();mtls.shutdown()


def governance_cases(binary, root, upstream):
    port, tls_port, admin = free_port(), free_port(), free_port()
    conf, candidate, users, writer, audit = [root/name for name in ("governance.conf","candidate.conf","users.json","governance-writer","governance-audit.jsonl")]
    alice, bob, legacy, replacement = "alice-"+"a"*48, "bob-"+"b"*48, "old-"+"c"*48, "new-"+"d"*48
    def identity(name,token,role="reader",namespaces=None):
        return {"name":name,"token_sha256":hashlib.sha256(token.encode()).hexdigest(),"role":role,"namespaces":namespaces}
    def publish_users(value):
        temp=users.with_suffix(".new"); temp.write_text(json.dumps({"users":value}));temp.replace(users)
    publish_users([identity("alice",alice,"writer"),identity("bob",bob,namespaces=["other-tenant"])])
    writer.write_text(legacy)
    conf.write_text(f'''http {{ access_log off; server {{
listen 127.0.0.1:{port}; listen 127.0.0.1:{tls_port} ssl; server_name example.test;
ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key;
location / {{ return 200 "active"; }}
location /proxy/ {{ proxy_pass http://127.0.0.1:{upstream.server_port}/base/; proxy_set_header X-Private private-fixture; }}
}} }}''')
    candidate.write_text(conf.read_text().replace('"active"','"candidate"'))
    mismatched=root/"mismatched.conf"
    mismatched.write_text(conf.read_text().replace("server_name example.test","server_name wrong.example.test"))
    invalid=subprocess.run([binary,"check","-c",str(mismatched)],capture_output=True,text=True)
    check("TLS configuration rejects certificates for a different hostname",invalid.returncode!=0 and "does not cover" in invalid.stderr)
    command("openssl","x509","-req","-in",str(root/"server.csr"),"-CA",str(root/"ca.crt"),"-CAkey",str(root/"ca.key"),"-CAcreateserial","-out",str(root/"expired.crt"),"-days","0")
    mismatched.write_text(conf.read_text().replace("/server.crt","/expired.crt"))
    invalid=subprocess.run([binary,"check","-c",str(mismatched)],capture_output=True,text=True)
    check("TLS configuration rejects expired certificates",invalid.returncode!=0 and "expired" in invalid.stderr)
    process=None
    def headers(token):return {"Authorization":"Bearer "+token}
    with open(root/"governance.log","w+") as output:
        try:
            process=subprocess.Popen([binary,"serve","-c",str(conf),"--admin",f"127.0.0.1:{admin}","--admin-users-file",str(users),"--admin-token-file",str(writer),"--admin-audit-file",str(audit)],stdout=output,stderr=output)
            wait_for(lambda:request(port)[2],b"active")
            check("Named management identities authenticate independently",request(admin,"/v1/config",headers=headers(alice))[0]==200 and request(admin,"/v1/config",headers=headers(bob))[0]==200)
            scoped=json.loads(request(admin,"/v1/config",headers=headers(bob))[2])
            check("Namespace-scoped identity cannot inspect standalone routes or history",scoped["routes"]==[] and scoped["backends"]=={} and request(admin,"/v1/history",headers=headers(bob))[0]==403)
            check("Scoped identity cannot simulate an unauthorized route",request(admin,"/v1/simulate","POST",headers(bob),b'{"path":"/"}')[0]==403)
            fixture={"config_path":str(candidate),"expected_version":1}
            response=request(admin,"/v1/validate","POST",headers(alice),json.dumps(fixture).encode())
            preview=json.loads(response[2])
            check("Candidate preflight reports changes without publishing",response[0]==200 and preview["valid"] and preview["diff"]["changed"] and request(port)[2]==b"active")
            fixture["expected_version"]=0
            check("Preflight rejects a stale base version",request(admin,"/v1/validate","POST",headers(alice),json.dumps(fixture).encode())[0]==409)
            diff=json.loads(command(binary,"diff","-c",str(candidate),"--against",str(conf)))
            check("CLI configuration diff agrees with management preflight",diff==preview["diff"])
            fixture={"path":"/proxy/thing?x=1","host":"example.test"}
            simulated=json.loads(request(admin,"/v1/simulate","POST",headers(alice),json.dumps(fixture).encode())[2])
            forwarded=json.loads(request(port,"/proxy/thing?x=1",headers={"Host":"example.test"})[2])
            check("Simulation reports the actual proxy_pass URI replacement",simulated["outbound"]["uri"]==forwarded["path"]=="/base/thing?x=1")
            check("Outgoing simulation keeps configured private header values redacted",simulated["outbound"]["headers"]["x-private"]=="[redacted]" and "private-fixture" not in json.dumps(simulated))
            wait_for(lambda:"rgnix_certificate_expiry_timestamp_seconds{" in request(admin,"/metrics")[2].decode(),True)
            certificate=json.loads(request(admin,"/v1/config",headers=headers(alice))[2])["certificates"][0]["health"]
            check("Certificate diagnostics expose hostname validity and expiration",certificate["valid"] and certificate["hostname_matches"] and certificate["expires_at"]>int(time.time()))
            writer.write_text(replacement)
            wait_for(lambda:request(admin,"/v1/config",headers=headers(legacy))[0],401)
            check("Legacy management token rotates without restarting the process",process.poll() is None and request(admin,"/v1/config",headers=headers(replacement))[0]==200)
            users.write_text('{"users":[{"name":"broken"}]}')
            wait_for(lambda:"rgnix_control_reload_errors_total 1" in request(admin,"/metrics")[2].decode(),True)
            check("Invalid identity updates retain the last valid authorization policy",request(admin,"/v1/config",headers=headers(alice))[0]==200)
            publish_users([identity("bob",bob)])
            wait_for(lambda:request(admin,"/v1/config",headers=headers(alice))[0],401)
            check("Removing a named identity revokes it without restart",request(admin,"/v1/config",headers=headers(bob))[0]==200 and process.poll() is None)
            records=[json.loads(line) for line in audit.read_text().splitlines()]
            check("Management audit identifies the named operator without recording credentials",any(r["actor"]=="alice" and r["operation"]=="/v1/validate" for r in records) and alice not in audit.read_text())
        except Exception:
            print((root/"governance.log").read_text()[-7000:],file=sys.stderr)
            raise
        finally:
            if process and process.poll() is None:process.send_signal(signal.SIGINT);process.wait(timeout=15)


if __name__ == "__main__":
    main()

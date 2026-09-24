#!/usr/bin/env python3
"""Compare the advertised HTTP subset with a local NGINX 1.28.0 oracle."""
import json
import http.client
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
from urllib.parse import urlsplit
from integration import Upstream, free_port, request, wait_for
from http.server import ThreadingHTTPServer


def main():
    binary = str(Path(sys.argv[1]).resolve())
    nginx = os.environ.get("NGINX", "/usr/sbin/nginx")
    version = subprocess.run([nginx, "-v"], capture_output=True, text=True, check=True).stderr.strip()
    if "nginx/1.28.0" not in version:
        raise RuntimeError(f"expected pinned oracle NGINX 1.28.0; got {version}")
    passed = []
    with tempfile.TemporaryDirectory(prefix="rgnix-parity-") as temp:
        directory = Path(temp)
        root = directory / "html"
        root.mkdir()
        (root / "index.html").write_text("index\n")
        (root / "data.txt").write_text("0123456789")
        os.utime(root / "data.txt", (1700000000, 1700000000))
        for name in ["a#b", "a?b", "a b", "a%b", "a%2Fb", "中文"]:
            (root / name).mkdir()
            (root / name / "index.html").write_text(name)
        (directory / "conf.d").mkdir()
        (directory / "shared.conf").write_text("add_header X-Parent parent always;")
        (directory / "conf.d/settings.conf").write_text("include shared.conf;")
        upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        upstream.daemon_threads = True
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        ports = [free_port(), free_port()]
        admin = free_port()
        closed = free_port()
        processes = []
        logs = []
        for engine, port in zip(["rgnix", "nginx"], ports):
            paths = "" if engine == "rgnix" else "\n".join(f"{kind}_temp_path {directory}/{kind};" for kind in ["client_body", "proxy", "fastcgi", "uwsgi", "scgi"])
            conf = directory / f"{engine}.conf"
            conf.write_text(f'''events {{}}
http {{
    {paths}
    access_log off;
    root {root};
    types {{ text/plain txt; text/html html; }}
    include conf.d/*.conf;
    upstream app {{ server 127.0.0.1:{upstream.server_port}; }}
    server {{
        listen 127.0.0.1:{port} default_server;
        server_name example.test;
        location /api/ {{ proxy_pass http://app/base/; proxy_set_header Host $host; }}
        location /slash/ {{ proxy_pass http://app/; add_header X-Child slash always; keepalive_timeout 0; }}
        location /raw/ {{ proxy_pass http://app; }}
        location "/proxy#b/" {{ proxy_pass http://app/base/; }}
        location /plain {{ return 200 prefix; }}
        location = /plain {{ return 200 exact; }}
        location /headers {{ add_header X-Child child always; return 200 child; }}
        location = /redirect {{ return 302 /destination; }}
        location /error {{ add_header X-Always yes always; proxy_pass http://127.0.0.1:{closed}; }}
        location /timeout/ {{ proxy_read_timeout 100ms; proxy_pass http://app/slow; }}
        location /no-keepalive {{ keepalive_timeout 0; return 200 done; }}
        location /denied {{ return 403 denied; proxy_pass http://app; return 200 escaped; }}
        location /denied-late {{ proxy_pass http://app; return 403 denied; }}
    }}
    server {{
        listen 127.0.0.1:{port};
        server_name *.wild.test;
        location / {{ return 200 wildcard; }}
    }}
    server {{
        listen 127.0.0.1:{port}; server_name closed.test;
        add_header X-Parent server always;
        return 403 closed;
        return 200 escaped;
        location / {{ add_header X-Parent location always; return 200 open; }}
    }}
    server {{ listen 127.0.0.1:{port}; server_name duplicate.test; return 200 first; }}
    server {{ listen 127.0.0.1:{port}; server_name duplicate.test; return 200 second; }}
}}''')
            log = (directory / f"{engine}.log").open("w")
            logs.append(log)
            args = [binary, "serve", "-c", str(conf), "--admin", f"127.0.0.1:{admin}"] if engine == "rgnix" else [nginx, "-p", str(directory), "-c", str(conf), "-e", "stderr", "-g", f"daemon off; master_process off; pid {directory}/nginx.pid;"]
            processes.append(subprocess.Popen(args, stdout=log, stderr=log))
        try:
            for port in ports:
                wait_for(lambda: request(port)[0], 200)
            cases = [("/", {}), ("/plain", {}), ("/plain-more", {}), ("/headers", {}),
                     ("/data.txt", {}), ("/data.txt", {"Range": "bytes=3-5"}),
                     ("/data.txt", {"Range": "bytes=3-5", "If-Range": "Tue, 14 Nov 2023 22:13:20 GMT"}),
                     ("/data.txt", {"Range": "bytes=3-5", "If-Range": "Tue, 02 Jan 2024 00:00:00 GMT"}),
                     ("/data.txt", {"Range": "bytes=3-5", "If-Range": "Mon, 01 Jan 2001 00:00:00 GMT"}),
                     ("/slash", {}),
                     ("/api/a?x=1", {}), ("/api/a%20b?x=1", {}), ("/api/%61%2Fb", {}),
                     ("/api/a%3Fb%25c", {}), ("/raw/%61?x=1", {}), ("/api", {}),
                     ("/api?x=1", {}), ("/redirect", {}), ("/", {"Host": "a.b.wild.test"}),
                     ("/a%23b?x=%23", {}), ("/a%3Fb?x=1", {}), ("/a%20b", {}),
                     ("/a%25b", {}), ("/a%252Fb", {}), ("/%E4%B8%AD%E6%96%87", {}), ("/proxy%23b?x=%3F", {}),
                     ("/", {"Host": "unknown.test"}), ("/", {"Host": "closed.test"}),
                     ("/", {"Host": "duplicate.test"}), ("/error", {}), ("/timeout/", {}), ("/denied", {}), ("/denied-late", {})]
            for path, headers in cases:
                results = [request(port, path, headers=headers) for port in ports]
                simplified = []
                for status, response_headers, body in results:
                    if response_headers.get("content-type") == "application/json":
                        value = json.loads(body)
                        body = (value["path"], value["headers"].get("Host"))
                    if status in (301, 302):
                        location = urlsplit(response_headers.get("location", ""))
                        body = (location.path, location.query, location.fragment)
                    if path in ("/error", "/timeout/"):
                        body = b""  # Default error-page bodies are outside the compatibility contract.
                    simplified.append((status, body, response_headers.get("x-parent"), response_headers.get("x-child"), response_headers.get("content-range"), response_headers.get("x-always")))
                if simplified[0] != simplified[1]:
                    raise AssertionError(f"parity failed {path} {headers}: rgnix={simplified[0]!r}, nginx={simplified[1]!r}")
                passed.append(f"{path} {headers}")
                print("PASS parity", passed[-1], flush=True)
                if results[0][0] == 301:
                    targets = [urlsplit(result[1]["location"]) for result in results]
                    followed = [request(port, target.path + ("?" + target.query if target.query else "")) for port, target in zip(ports, targets)]
                    assert all(result[0] == 200 for result in followed), (path, followed)
                    bodies = [json.loads(result[2])["path"] if result[1].get("content-type") == "application/json" else result[2] for result in followed]
                    assert bodies[0] == bodies[1], (path, bodies)
            raw_cases = [
                ("missing HTTP/1.1 Host", b"GET / HTTP/1.1\r\n", 400),
                ("empty HTTP/1.1 Host", b"GET / HTTP/1.1\r\nHost:\r\n", 400),
                ("invalid HTTP/1.1 Host", b"GET / HTTP/1.1\r\nHost: bad/path\r\n", 400),
                ("duplicate Host", b"GET / HTTP/1.1\r\nHost: example.test\r\nHost: other.test\r\n", 400),
                ("absolute URI still requires Host", b"GET http://example.test/ HTTP/1.1\r\n", 400),
                ("absolute URI cannot hide an invalid Host", b"GET http://example.test/ HTTP/1.1\r\nHost: bad/path\r\n", 400),
                ("HTTP/1.0 without Host", b"GET / HTTP/1.0\r\n", 200),
                ("Host with port", b"GET / HTTP/1.1\r\nHost: example.test:8080\r\n", 200),
                ("IPv6 Host with port", b"GET / HTTP/1.1\r\nHost: [::1]:8080\r\n", 200),
                ("absolute URI with matching Host", b"GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n", 200),
            ]
            for label, raw, expected in raw_cases:
                statuses = []
                for port in ports:
                    with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                        connection.sendall(raw + b"Connection: close\r\n\r\n")
                        response = http.client.HTTPResponse(connection)
                        response.begin()
                        response.read()
                        statuses.append(response.status)
                assert statuses == [expected, expected], (label, statuses)
                passed.append(label)
                print("PASS parity", label, flush=True)
            for port in ports:
                with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                    connection.sendall(b"GET /api/item HTTP/1.0\r\n\r\n")
                    response = http.client.HTTPResponse(connection)
                    response.begin()
                    assert response.status == 200
                    assert json.loads(response.read())["headers"].get("Host") == "example.test"
            passed.append("hostless HTTP/1.0 uses the selected server name for $host")
            for port in ports:
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
                connection.request("GET", "/no-keepalive", headers={"Host": "example.test"})
                response = connection.getresponse()
                assert response.status == 200 and response.will_close
                response.read()
                connection.close()
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
                connection.request("GET", "/slash", headers={"Host": "example.test"})
                response = connection.getresponse()
                assert response.status == 301 and response.will_close
                response.read()
                connection.close()
            passed.append("keepalive_timeout 0 closes the downstream connection")
            passed.append("automatic slash redirect uses the proxy location keepalive policy")
        except BaseException:
            for engine in ["rgnix", "nginx"]:
                print((directory / f"{engine}.log").read_text()[-8000:], file=sys.stderr)
            raise
        finally:
            for process in processes:
                process.terminate()
            for process in processes:
                process.wait(timeout=35)
            for log in logs:
                log.close()
            upstream.shutdown()
    output = Path(".local/nginx-parity.json")
    output.parent.mkdir(exist_ok=True)
    output.write_text(json.dumps({"oracle": version, "passed": len(passed), "checks": passed}, indent=2) + "\n")
    print(f"{len(passed)} NGINX parity scenarios passed")


if __name__ == "__main__":
    main()

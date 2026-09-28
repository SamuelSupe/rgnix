#!/usr/bin/env python3
"""Exercise the experimental transport against a real HTTP/1 origin, using stdlib only."""
import argparse
import hashlib
import http.client
import json
import socket
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Origin(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    first_upload = threading.Event()
    stream_tail = threading.Event()
    stream_timed_out = False
    attempts = 0

    def log_message(self, *args):
        pass

    def answer(self, body, status=200):
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Origin-Port", str(self.client_address[1]))
        self.send_header("Connection", "x-hop-one")
        self.send_header("Connection", "x-hop-two")
        self.send_header("X-Hop-One", "private")
        self.send_header("X-Hop-Two", "private")
        self.send_header("Set-Cookie", "a=1")
        self.send_header("Set-Cookie", "b=2")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def do_GET(self):
        if self.path == "/stream":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", "6")
            self.end_headers()
            self.wfile.write(b"one")
            self.wfile.flush()
            if not self.stream_tail.wait(3):
                type(self).stream_timed_out = True
            self.wfile.write(b"two")
        elif self.path == "/chunked":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            self.wfile.write(b"3\r\none\r\n3\r\ntwo\r\n0\r\n\r\n")
        else:
            self.answer(json.dumps({"path": self.path, "headers": dict(self.headers)}).encode())

    def do_HEAD(self):
        self.answer(b"head-body")

    def do_POST(self):
        digest = hashlib.sha256()
        count = 0
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            while True:
                length = int(self.rfile.readline().split(b";", 1)[0], 16)
                if not length:
                    assert self.rfile.readline() == b"\r\n"
                    break
                chunk = self.rfile.read(length)
                assert self.rfile.read(2) == b"\r\n"
                digest.update(chunk)
                count += len(chunk)
                self.first_upload.set()
        else:
            remaining = int(self.headers.get("Content-Length", "0"))
            while remaining:
                chunk = self.rfile.read(min(16384, remaining))
                if not chunk:
                    break
                digest.update(chunk)
                count += len(chunk)
                remaining -= len(chunk)
                self.first_upload.set()
        if self.path == "/drop":
            type(self).attempts += 1
            self.close_connection = True
            self.connection.shutdown(socket.SHUT_RDWR)
        else:
            self.answer(json.dumps({"bytes": count, "sha256": digest.hexdigest()}).encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    args = parser.parse_args()
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
    origin = ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    process = subprocess.Popen([args.binary, "--listen", f"127.0.0.1:{port}", "--upstream",
                                f"127.0.0.1:{origin.server_port}"])
    checks = []
    try:
        for attempt in range(100):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.1).close()
                break
            except OSError:
                assert process.poll() is None, "proxy exited"
                time.sleep(0.05)
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        upstream_ports = set()
        for _ in range(10):
            connection.putrequest("GET", "/raw%2Fpath?q=a%20b")
            connection.putheader("Connection", "x-private-one")
            connection.putheader("Connection", "x-private-two")
            connection.putheader("X-Private-One", "private")
            connection.putheader("X-Private-Two", "private")
            connection.endheaders()
            response = connection.getresponse()
            assert response.status == 200
            upstream_ports.add(response.getheader("x-origin-port"))
            assert not response.getheader("x-hop-one") and not response.getheader("x-hop-two")
            assert len([h for h in response.getheaders() if h[0].lower() == "set-cookie"]) == 2
            echoed = json.loads(response.read())
            assert echoed["path"] == "/raw%2Fpath?q=a%20b"
            headers = {name.lower(): value for name, value in echoed["headers"].items()}
            assert "x-private-one" not in headers and "x-private-two" not in headers
        assert len(upstream_ports) == 1, upstream_ports
        checks.append("10 keepalive requests reuse origin connection; raw URI, duplicate cookies and hop headers")

        connection.request("HEAD", "/head")
        response = connection.getresponse()
        assert response.status == 200 and response.getheader("content-length") == "9"
        assert response.read() == b""
        checks.append("HEAD preserves length and sends no body")

        payload = bytes(range(256)) * 32768
        connection.request("POST", "/digest", body=payload)
        response = connection.getresponse()
        assert response.status == 200
        assert json.loads(response.read()) == {"bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()}
        checks.append("8 MiB fixed-length upload arrives byte-for-byte")

        Origin.first_upload.clear()
        def chunks():
            yield payload[:32768]
            assert Origin.first_upload.wait(2), "proxy buffered request before forwarding"
            yield payload[32768:]
        connection.request("POST", "/digest", body=chunks(), encode_chunked=True)
        response = connection.getresponse()
        assert response.status == 200
        assert json.loads(response.read()) == {"bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()}
        checks.append("8 MiB chunked upload streams before client finishes")

        connection.request("GET", "/stream")
        response = connection.getresponse()
        assert response.read(3) == b"one" and not Origin.stream_timed_out
        Origin.stream_tail.set()
        assert response.read() == b"two"
        connection.request("GET", "/chunked")
        response = connection.getresponse()
        assert response.read() == b"onetwo"
        checks.append("response streams before origin finishes; chunked response decoded correctly")

        connection.request("POST", "/drop", body=b"side-effect")
        response = connection.getresponse()
        assert response.status == 502
        response.read()
        assert Origin.attempts == 1, "request was replayed"
        checks.append("origin disconnect after POST returns 502 with exactly one upstream attempt")

        connection.request("GET", "/", headers={"Connection": "upgrade", "Upgrade": "websocket"})
        response = connection.getresponse()
        assert response.status == 501
        response.read()
        connection.close()
        checks.append("unsupported upgrade is explicitly rejected")
        print(json.dumps({"passed": len(checks), "checks": checks}, indent=2))
    finally:
        Origin.stream_tail.set()
        process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        origin.shutdown()
        origin.server_close()


if __name__ == "__main__":
    main()

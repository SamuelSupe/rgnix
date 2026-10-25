#!/usr/bin/env python3
"""Gateway response deadlines over blocked TCP and QUIC, using the public runtime fixture."""
import argparse
import asyncio
import base64
import hashlib
import http.server
import json
from pathlib import Path
import signal
import socket
import ssl
import subprocess
import tempfile
import threading
import time

from integration import check, free_port, metric_value, request, wait_for, RESULTS
from product_features import certificates
from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import ResponseReceived, StreamEnded, StreamReset
from h2.settings import SettingCodes

BULK = 16 * 1024 * 1024


class Origin(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        try:
            if self.path in ("/hints", "/hint-slow"):
                hint = b"q" * (4096 if self.path == "/hints" else 8)
                self.wfile.write(b"HTTP/1.1 103 Early Hints\r\nX-Hint: " + hint + b"\r\n\r\n")
                self.wfile.flush()
            if self.path in ("/slow", "/slow-empty", "/hint-slow"):
                time.sleep(.4)
            elif self.path == "/sibling":
                time.sleep(.08)
            websocket = self.path == "/ws"
            size = BULK if self.path in ("/bulk", "/ws") else (0 if self.path in ("/empty", "/slow-empty") else 32)
            self.send_response(101 if websocket else 200)
            if websocket:
                key = self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
                self.send_header("Upgrade", "websocket")
                self.send_header("Connection", "upgrade")
                self.send_header("Sec-WebSocket-Accept", base64.b64encode(hashlib.sha1(key.encode()).digest()).decode())
                self.close_connection = True
            else:
                self.send_header("Content-Length", str(size))
            self.end_headers()
            if self.path == "/small":
                self.server.small.set()
            if self.command != "HEAD":
                for offset in range(0, size, 65536):
                    self.wfile.write(b"x" * min(65536, size - offset))
            self.wfile.flush()
        except OSError:
            pass

    do_HEAD = do_GET


def budgets(admin):
    metrics = request(admin, "/metrics")[2].decode()
    assert 'rgnix_budget_in_use{budget="inflight"}' in metrics
    assert 'rgnix_backend_inflight{backend="app"}' in metrics
    return (metric_value(metrics, "rgnix_budget_in_use", budget="inflight"),
            metric_value(metrics, "rgnix_backend_inflight", backend="app"))


def tcp_flush(port, admin, source, finite, tls=False, error_reply=False):
    source.small.clear()
    client = socket.socket()
    if tls:
        context = ssl._create_unverified_context()
        context.set_alpn_protocols(["h2"])
        client = context.wrap_socket(client, server_hostname="example.test")
    label = "HTTP/2 TLS" if tls else "HTTP/2 TCP"
    with client:
        client.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
        client.settimeout(.1)
        client.connect(("127.0.0.1", port))
        peer = H2Connection(H2Configuration(client_side=True))
        peer.initiate_connection()
        peer.update_settings({SettingCodes.INITIAL_WINDOW_SIZE: 2147483647})
        peer.increment_flow_control_window(2147483647 - 65535)

        def get(stream, path):
            peer.send_headers(stream, [(":method", "GET"), (":scheme", "http"),
                                       (":authority", "example.test"), (":path", path)], end_stream=True)
            client.sendall(peer.data_to_send())

        started = time.monotonic()
        get(1, "/bulk")
        time.sleep(.05)
        get(3, "/slow" if error_reply else "/small")
        if not error_reply:
            assert source.small.wait(.3), "small response was never dispatched"
        time.sleep(.6)
        if error_reply:
            check(label + " stalled 504 retains its permit during the bounded error delivery", min(budgets(admin)) > 0)
            time.sleep(1.8)
        occupied = budgets(admin)
        check(label + (" stalled 504 releases permits at its send deadline" if error_reply else
                       " final flush releases permits at the absolute deadline" if finite else
                       " disabled deadlines retain stalled work"), occupied == (0, 0) if finite else min(occupied) > 0)
        client.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
        events, closed = [], False
        until = time.monotonic() + 5
        while time.monotonic() < until:
            try:
                data = client.recv(65536)
                if not data:
                    closed = True
                    break
                events.extend(peer.receive_data(data))
                if not finite and any(isinstance(e, (StreamEnded, StreamReset)) and e.stream_id == 3 for e in events):
                    break
                pending = peer.data_to_send()
                if pending:
                    client.sendall(pending)
            except socket.timeout:
                continue
            except (ConnectionResetError, BrokenPipeError, ssl.SSLError):
                closed = True
                break
        completed = any(isinstance(e, StreamEnded) and e.stream_id == 3 for e in events)
        check(label + (" expired output cannot finish successfully after reading resumes" if finite else
                       " disabled deadlines allow delivery after reading resumes"), closed and not completed if finite else completed and not closed)
    wait_for(lambda: budgets(admin), (0, 0))
    check(label + " listener serves fresh connections after stalled delivery", request(port, "/small", tls=tls)[0] == 200)
    return {"seconds": time.monotonic() - started, "permits_before_resume": occupied,
            "connection_closed": closed, "small_completed": completed}


def timeout_responses(port, admin, finite, tls=False):
    expected = 504 if finite else 200
    label = "TLS" if tls else "TCP"
    for method in ("GET", "HEAD"):
        check(f"HTTP/1 {label} {method} preserves the delayed-header response",
              request(port, "/slow", method=method, tls=tls)[0] == expected)
        raw = socket.create_connection(("127.0.0.1", port), timeout=1)
        if tls:
            context = ssl._create_unverified_context()
            context.set_alpn_protocols(["h2"])
            raw = context.wrap_socket(raw, server_hostname="example.test")
        raw.settimeout(.05)
        with raw as client:
            peer = H2Connection(H2Configuration(client_side=True))
            peer.initiate_connection()

            def send(stream, path, verb="GET"):
                peer.send_headers(stream, [(":method", verb), (":scheme", "https" if tls else "http"),
                                          (":authority", "example.test"), (":path", path)], end_stream=True)
                client.sendall(peer.data_to_send())

            statuses, ended, resets = {}, set(), set()

            def receive_until(wanted):
                until = time.monotonic() + 1
                while not wanted.issubset(ended) and time.monotonic() < until:
                    try:
                        data = client.recv(65536)
                    except socket.timeout:
                        continue
                    assert data, "connection closed before timeout response and sibling completed"
                    for event in peer.receive_data(data):
                        if isinstance(event, ResponseReceived):
                            statuses[event.stream_id] = int(dict(event.headers)[b":status"])
                        elif isinstance(event, StreamEnded):
                            ended.add(event.stream_id)
                        elif isinstance(event, StreamReset):
                            resets.add(event.stream_id)
                    pending = peer.data_to_send()
                    if pending:
                        client.sendall(pending)

            send(1, "/slow", method)
            time.sleep(.1)
            send(3, "/sibling")
            receive_until({1, 3})
            check(f"HTTP/2 {label} {method} completes the timeout response and unexpired sibling",
                  statuses == {1: expected, 3: 200} and ended == {1, 3} and not resets)
            send(5, "/empty")
            receive_until({5})
            check(f"HTTP/2 {label} accepts another stream after {method} timeout", statuses.get(5) == 200 and 5 in ended)
    wait_for(lambda: budgets(admin), (0, 0))


async def quic_send(port, admin, finite, origin_port):
    from aioquic.asyncio import connect, QuicConnectionProtocol
    from hyper_protocols import h3_connection
    from aioquic.h3.events import HeadersReceived, DataReceived
    from aioquic.quic.configuration import QuicConfiguration
    from aioquic.quic.events import StreamReset as QuicReset, StreamDataReceived

    class Client(QuicConnectionProtocol):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.http = h3_connection(self._quic)
            self.headers, self.data, self.resets, self.ends = {}, {}, {}, set()
            self.statuses = {}
            self.limits = self._quic._write_stream_limits
            # aioquic normally replenishes credit as packets arrive. Withhold
            # MAX_STREAM_DATA to reproduce a peer that stops consuming a stream.
            self._quic._write_stream_limits = lambda *args, **kwargs: None

        def quic_event_received(self, event):
            if isinstance(event, QuicReset):
                self.resets[event.stream_id] = (event.error_code, time.monotonic())
            for item in self.http.handle_event(event):
                if isinstance(item, HeadersReceived):
                    self.headers[item.stream_id] = dict(item.headers)
                    self.statuses.setdefault(item.stream_id, []).append(dict(item.headers)[b":status"])
                if isinstance(item, DataReceived):
                    self.data[item.stream_id] = self.data.get(item.stream_id, 0) + len(item.data)
                if item.stream_ended:
                    self.ends.add(item.stream_id)
            # aioquic 1.3 omits the H3 end event when FIN follows a GREASE frame.
            if isinstance(event, StreamDataReceived) and event.end_stream and event.stream_id in self.headers:
                self.ends.add(event.stream_id)

        def get(self, path, method="GET"):
            stream = self._quic.get_next_available_stream_id()
            self.http.send_headers(stream, [(b":method", method.encode()), (b":scheme", b"https"),
                                          (b":authority", b"example.test"), (b":path", path.encode())], end_stream=True)
            self.transmit()
            return stream

        def tunnel(self, websocket):
            stream = self._quic.get_next_available_stream_id()
            headers = [(b":method", b"CONNECT"), (b":authority", b"example.test" if websocket else f"127.0.0.1:{origin_port}".encode())]
            if websocket:
                headers += [(b":scheme", b"https"), (b":path", b"/ws"), (b":protocol", b"websocket"), (b"sec-websocket-version", b"13")]
            self.http.send_headers(stream, headers)
            # A raw CONNECT can carry an HTTP exchange to the configured origin.
            payload = b"" if websocket else b"GET /bulk HTTP/1.1\r\nHost: origin\r\nConnection: close\r\n\r\n"
            self.http.send_data(stream, payload, end_stream=True)
            self.transmit()
            return stream

        async def complete(self, stream):
            until = time.monotonic() + 8
            while stream not in self.ends and stream not in self.resets and time.monotonic() < until:
                await asyncio.sleep(.01)

        def credit(self, enabled):
            self._quic._write_stream_limits = self.limits if enabled else lambda *args, **kwargs: None
            self.transmit()

    settings = QuicConfiguration(is_client=True, alpn_protocols=["h3"], server_name="example.test",
                                 verify_mode=0, max_stream_data=1024, max_data=1048576)
    async with connect("127.0.0.1", port, configuration=settings, create_protocol=Client) as client:
        started = time.monotonic()
        stream, sibling = client.get("/bulk"), client.get("/empty")
        await asyncio.sleep(.65)
        occupied = await asyncio.to_thread(budgets, admin)
        check("HTTP/3 blocked delivery leaves sibling streams independent", sibling in client.ends and client.headers[stream][b":status"] == b"200")
        if finite:
            reset = client.resets.get(stream)
            check("HTTP/3 send wait obeys the absolute deadline and releases permits",
                  reset is not None and reset[0] == 0x10c and reset[1] - started < .6 and occupied == (0, 0))
            check("HTTP/3 timeout sends RESET_STREAM instead of successful FIN", stream not in client.ends)
        else:
            check("HTTP/3 disabled deadlines keep blocked delivery active", stream not in client.resets and min(occupied) > 0)
            client._quic._write_stream_limits = client.limits
            client.transmit()
            until = time.monotonic() + 8
            while stream not in client.ends and stream not in client.resets and time.monotonic() < until:
                await asyncio.sleep(.02)
            if stream not in client.ends or client.data.get(stream) != BULK or stream in client.resets:
                print(json.dumps({"quic_bytes": client.data.get(stream), "ended": stream in client.ends, "resets": client.resets}))
            check("HTTP/3 disabled deadlines allow complete delivery after credit resumes", stream in client.ends and client.data[stream] == BULK and stream not in client.resets)
            client._quic._write_stream_limits = lambda *args, **kwargs: None
            idle = client.get("/bulk")
            until = time.monotonic() + 3
            while idle not in client.resets and time.monotonic() < until:
                await asyncio.sleep(.02)
            check("HTTP/3 disabled absolute deadlines retain the independent send timeout",
                  client.resets.get(idle, (None,))[0] == 0x10c and idle not in client.ends
                  and await asyncio.to_thread(budgets, admin) == (0, 0))
            client._quic._write_stream_limits = client.limits
        fresh = client.get("/empty")
        until = time.monotonic() + 1
        while fresh not in client.ends and time.monotonic() < until:
            await asyncio.sleep(.01)
        check("HTTP/3 connection remains usable after the affected stream ends", fresh in client.ends)
        measured = {"seconds": time.monotonic() - started, "reset_seconds": client.resets[stream][1] - started if stream in client.resets else None,
                    "permits_before_resume": occupied, "bytes": client.data.get(stream, 0)}
        client.credit(True)
        for method, path in [("GET", "/slow"), ("HEAD", "/slow-empty"), ("GET", "/hint-slow")]:
            failed = client.get(path, method)
            await asyncio.sleep(.1)
            sibling = client.get("/sibling")
            await client.complete(failed)
            await client.complete(sibling)
            expected = ([b"103"] if path == "/hint-slow" else []) + [b"504" if finite else b"200"]
            if client.statuses.get(failed) != expected or failed not in client.ends or failed in client.resets:
                print({"path": path, "statuses": client.statuses.get(failed), "expected": expected,
                       "ended": failed in client.ends, "reset": client.resets.get(failed)})
            check(f"HTTP/3 {method} {path} completes its response and preserves siblings",
                  client.statuses.get(failed) == expected and failed in client.ends and failed not in client.resets
                  and sibling in client.ends and client.headers[sibling][b":status"] == b"200")
        measured["boundaries"] = []
        for kind in ("hints", "connect", "websocket"):
            client.credit(False)
            began = time.monotonic()
            target = client.get("/hints") if kind == "hints" else client.tunnel(kind == "websocket")
            sibling = client.get("/empty")
            await asyncio.sleep(.65)
            occupied = await asyncio.to_thread(budgets, admin)
            before_resume = client.data.get(target, 0)
            reset = client.resets.get(target)
            check(f"HTTP/3 blocked {kind} preserves sibling responses", sibling in client.ends)
            if finite:
                check(f"HTTP/3 {kind} resets at the original deadline and releases permits",
                      reset is not None and reset[0] == 0x10c and reset[1] - began < .6
                      and target not in client.ends and occupied == (0, 0))
            else:
                check(f"HTTP/3 disabled deadlines retain blocked {kind}", reset is None and target not in client.ends and min(occupied) > 0)
            client.credit(True)
            if finite:
                await asyncio.sleep(.1)
                check(f"HTTP/3 expired {kind} sends no more data after credit resumes",
                      client.data.get(target, 0) == before_resume and target not in client.ends)
            else:
                await client.complete(target)
                check(f"HTTP/3 disabled deadlines allow {kind} to complete",
                      target in client.ends and target not in client.resets
                      and (client.statuses[target] == [b"103", b"200"] if kind == "hints" else client.data[target] >= BULK))
                client.credit(False)
                idle = client.get("/hints") if kind == "hints" else client.tunnel(kind == "websocket")
                until = time.monotonic() + 3
                while idle not in client.resets and time.monotonic() < until:
                    await asyncio.sleep(.02)
                check(f"HTTP/3 disabled absolute deadlines retain the {kind} send timeout",
                      client.resets.get(idle, (None,))[0] == 0x10c and idle not in client.ends
                      and await asyncio.to_thread(budgets, admin) == (0, 0))
                client.credit(True)
            measured["boundaries"].append({"kind": kind, "permits_before_resume": occupied,
                                            "bytes_before_resume": before_resume, "bytes_after_resume": client.data.get(target, 0),
                                            "reset_seconds": reset[1] - began if reset else None})
            fresh = client.get("/empty")
            await client.complete(fresh)
            check(f"HTTP/3 accepts another stream after {kind}", fresh in client.ends and client.headers[fresh][b":status"] == b"200")
        return measured


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("fixture", type=Path)
    parser.add_argument("--http3", action="store_true")
    parser.add_argument("--output", type=Path, default=Path(".local/delivery-deadlines.json"))
    args = parser.parse_args()
    source = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    source.daemon_threads = True
    source.small = threading.Event()
    threading.Thread(target=source.serve_forever, daemon=True).start()
    measured = []
    try:
        with tempfile.TemporaryDirectory(prefix="rgnix-delivery-") as directory:
            root = Path(directory)
            certificates(root)
            for mode, limits in [("request", ["--request-ms", "150"]), ("backend", ["--backend-ms", "150"]),
                                 ("earlier-backend", ["--request-ms", "1000", "--backend-ms", "150"]),
                                 ("disabled", ["--request-ms", "0", "--backend-ms", "0"])]:
                port, tls, admin = free_port(), free_port(), free_port()
                config = root / "nginx.conf"
                secure = f"server {{ listen 127.0.0.1:{tls} ssl; http2 on; {'http3 on;' if args.http3 else ''} server_name example.test; ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key; location / {{ rgnix_connect on; proxy_pass http://app; }} }}"
                config.write_text(f"http {{ access_log off; send_timeout 2s; upstream app {{ server 127.0.0.1:{source.server_port}; }} server {{ listen 127.0.0.1:{port}; http2 on; server_name example.test; location / {{ proxy_pass http://app; }} }} {secure} }}")
                with (root / "server.log").open("w+") as log:
                    process = subprocess.Popen([str(args.fixture.resolve()), "--config", str(config), "--admin", f"127.0.0.1:{admin}", "--threads", "1", "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "1", *limits], stdout=log, stderr=log)
                    try:
                        wait_for(lambda: request(admin, "/readyz")[0], 200)
                        item = {"mode": mode, "tcp": tcp_flush(port, admin, source, mode != "disabled")}
                        item["tls"] = tcp_flush(tls, admin, source, mode != "disabled", tls=True)
                        timeout_responses(port, admin, mode != "disabled")
                        timeout_responses(tls, admin, mode != "disabled", tls=True)
                        if mode != "disabled":
                            item["error_flush"] = tcp_flush(port, admin, source, True, error_reply=True)
                        if args.http3:
                            item["quic"] = asyncio.run(quic_send(tls, admin, mode != "disabled", source.server_port))
                        wait_for(lambda: budgets(admin), (0, 0))
                        measured.append(item)
                    except Exception:
                        log.flush()
                        print((root / "server.log").read_text()[-6000:])
                        raise
                    finally:
                        process.send_signal(signal.SIGINT)
                        process.wait(timeout=10)
    finally:
        source.shutdown()
        source.server_close()
    result = {"passed": len(RESULTS), "checks": RESULTS, "measurements": measured}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

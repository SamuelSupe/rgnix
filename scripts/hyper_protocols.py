#!/usr/bin/env python3
"""Real-socket checks for Hyper protocol negotiation and transport boundaries."""
import argparse
import asyncio
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import socketserver
import ssl
import subprocess
import tempfile
import contextlib
import threading
import time

from integration import check, free_port, request, wait_for, RESULTS
from product_features import certificates, start_backend
from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import DataReceived, ResponseReceived, StreamEnded, RemoteSettingsChanged
from h2.settings import SettingCodes


class Echo(socketserver.BaseRequestHandler):
    half_close_tail = b""

    def handle(self):
        while data := self.request.recv(65536):
            if data == b"half-close":
                self.request.sendall(b"half-closed")
                self.request.shutdown(socket.SHUT_WR)
                tail = bytearray()
                while data := self.request.recv(65536): tail.extend(data)
                type(self).half_close_tail = bytes(tail)
                return
            self.request.sendall(data)


class WebSocket(socketserver.StreamRequestHandler):
    def handle(self):
        self.rfile.readline()
        headers = {}
        while (line := self.rfile.readline()) != b"\r\n":
            if not line:
                return
            name, value = line.split(b":", 1)
            headers[name.lower()] = value.strip()
        accept = base64.b64encode(hashlib.sha1(headers[b"sec-websocket-key"] + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
        self.wfile.write(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
        self.wfile.flush()
        while frame := self.rfile.read(2):
            n = frame[1] & 127
            mask = self.rfile.read(4) if frame[1] & 128 else None
            data = self.rfile.read(n)
            if mask:
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
            self.wfile.write(bytes([frame[0], n]) + data)
            self.wfile.flush()


class H2Echo(socketserver.BaseRequestHandler):
    def handle(self):
        from h2.events import RequestReceived
        connection = H2Connection(H2Configuration(client_side=False, header_encoding="utf-8"))
        connection.initiate_connection()
        connection.update_settings({SettingCodes.ENABLE_CONNECT_PROTOCOL: 1})
        self.request.sendall(connection.data_to_send())
        while data := self.request.recv(65536):
            for event in connection.receive_data(data):
                if isinstance(event, RequestReceived):
                    connection.send_headers(event.stream_id, [(":status", "200")])
                elif isinstance(event, DataReceived):
                    connection.acknowledge_received_data(event.flow_controlled_length, event.stream_id)
                    connection.send_data(event.stream_id, event.data)
                elif isinstance(event, StreamEnded):
                    connection.send_data(event.stream_id, b"", end_stream=True)
            self.request.sendall(connection.data_to_send())


class EarlyHints(socketserver.StreamRequestHandler):
    def handle(self):
        while (line := self.rfile.readline()) != b"\r\n":
            if not line: return
        self.wfile.write(b"HTTP/1.1 103 Early Hints\r\nLink: </app.css>; rel=preload\r\n\r\nHTTP/1.1 102 Processing\r\n\r\n")
        self.wfile.flush()
        time.sleep(.05)
        self.wfile.write(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhinted")
        self.wfile.flush()


def origin(handler):
    server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class H2Peer:
    def __init__(self, port, upgrade=False, upgrade_path="/health", upgrade_body=b""):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=3)
        self.connection = H2Connection(H2Configuration(client_side=True, header_encoding="utf-8"))
        self.events = []
        if upgrade:
            settings = self.connection.initiate_upgrade_connection()
            self.socket.sendall(("POST" if upgrade_body else "GET").encode() + b" " + upgrade_path.encode() + b" HTTP/1.1\r\nHost: example.test\r\nConnection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\nContent-Length: " + str(len(upgrade_body)).encode() + b"\r\nHTTP2-Settings: " + settings + b"\r\n\r\n" + upgrade_body)
            headers = bytearray()
            while not headers.endswith(b"\r\n\r\n"):
                headers.extend(self.socket.recv(1))
            assert headers.startswith(b"HTTP/1.1 101"), headers
        else:
            self.connection.initiate_connection()
        self.socket.sendall(self.connection.data_to_send())
        self.receive()

    def receive(self):
        data = self.socket.recv(65536)
        if not data:
            raise EOFError("HTTP/2 connection closed")
        for event in self.connection.receive_data(data):
            if isinstance(event, DataReceived):
                self.connection.acknowledge_received_data(event.flow_controlled_length, event.stream_id)
            self.events.append(event)
        self.socket.sendall(self.connection.data_to_send())

    def send(self, stream, path, websocket=False):
        headers = [(":method", "CONNECT" if websocket else "GET"), (":scheme", "http"), (":authority", "example.test"), (":path", path)]
        if websocket:
            headers += [(":protocol", "websocket"), ("sec-websocket-version", "13")]
        self.connection.send_headers(stream, headers, end_stream=not websocket)
        self.socket.sendall(self.connection.data_to_send())

    def until(self, predicate):
        deadline = time.monotonic() + 3
        while not predicate(self.events):
            if time.monotonic() >= deadline:
                raise TimeoutError("HTTP/2 response")
            self.receive()
        return self.events


def h2_header_cases(port):
    from hyperframe.frame import HeadersFrame, ContinuationFrame
    for upgrade in [False, True]:
        peer = H2Peer(port, upgrade)
        active, stream = (3, 5) if upgrade else (1, 3)
        peer.send(active, "/ws-long", True)
        peer.until(lambda events: any(isinstance(e, ResponseReceived) and e.stream_id == active for e in events))
        headers = [(":method", "GET"), (":scheme", "http"), (":authority", "example.test"), (":path", "/health")]
        peer.connection.send_headers(stream, headers, end_stream=True)
        encoded = peer.connection.data_to_send()[9:]
        first = HeadersFrame(stream, data=encoded[:1]); first.flags.add("END_STREAM")
        last = ContinuationFrame(stream, data=encoded[1:]); last.flags.add("END_HEADERS")
        peer.socket.sendall(first.serialize())
        time.sleep(.05)
        peer.socket.sendall(last.serialize())
        peer.until(lambda events: any(isinstance(e, StreamEnded) and e.stream_id == stream for e in events))
        check("fragmented HTTP/2 headers complete alongside an active tunnel" + (" after h2c" if upgrade else ""), any(isinstance(e, ResponseReceived) and e.stream_id == stream and dict(e.headers)[":status"] == "200" for e in peer.events))
        stream += 2
        peer.connection.send_headers(stream, headers, end_stream=True)
        encoded = peer.connection.data_to_send()[9:]
        first = HeadersFrame(stream, data=encoded[:1]); first.flags.add("END_STREAM")
        # The CONTINUATION header itself is trickled, so active sibling traffic
        # cannot turn the header timeout into an indefinitely renewable idle timer.
        last = ContinuationFrame(stream, data=encoded[1:]); last.flags.add("END_HEADERS")
        peer.socket.sendall(first.serialize())
        started = time.monotonic()
        closed = False
        for byte in last.serialize():
            time.sleep(.09)
            try: peer.socket.sendall(bytes([byte]))
            except (BrokenPipeError, ConnectionResetError):
                closed = True
                break
        if not closed:
            peer.socket.settimeout(1)
            try:
                while peer.socket.recv(65536): pass
                closed = True
            except ConnectionResetError: closed = True
        check("HTTP/2 incomplete header total deadline survives active tunnels" + (" after h2c" if upgrade else ""), closed and time.monotonic() - started < 1.2)
        peer.socket.close()


async def h3_cases(port, mtls, shared_tls, root, process, config):
    from aioquic.asyncio import connect, QuicConnectionProtocol
    from aioquic.h3.connection import H3Connection, HeadersState
    from aioquic.h3.events import HeadersReceived, DataReceived as H3Data
    from aioquic.quic.configuration import QuicConfiguration
    from aioquic.quic.connection import QuicConnection
    from aioquic.quic.packet import pull_quic_header
    from aioquic.quic.events import StreamDataReceived, HandshakeCompleted, StreamReset, ConnectionTerminated
    from aioquic.buffer import Buffer

    invalid = QuicConnection(configuration=QuicConfiguration(is_client=True, alpn_protocols=["h3"], server_name="example.test"))
    invalid.connect(("127.0.0.1", port), now=time.monotonic())
    packet = bytearray(invalid.datagrams_to_send(now=time.monotonic())[0][0])
    header = pull_quic_header(Buffer(data=bytes(packet)), host_cid_length=8)
    packet[header.packet_length - 1] ^= 1
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.sendto(packet, ("127.0.0.1", port))
    await asyncio.sleep(.25)
    check("invalid QUIC Initial cannot stop the server", process.poll() is None)

    class InterimConnection(H3Connection):
        def _handle_request_or_push_frame(self, frame_type, frame_data, stream, stream_ended):
            events = super()._handle_request_or_push_frame(frame_type, frame_data, stream, stream_ended)
            # aioquic 1.3 treats a second response HEADERS as trailers. A 1xx
            # section instead leaves the stream waiting for its final response.
            for event in events:
                if isinstance(event, HeadersReceived) and 100 <= int(dict(event.headers).get(b":status", b"200")) < 200:
                    stream.headers_recv_state = HeadersState.INITIAL
                    stream.content_length = 0
            return events

    class Client(QuicConnectionProtocol):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.http = InterimConnection(self._quic)
            self.responses = {}
            self.headers_ready = {}
            self.handshake = None
            self.statuses = []
            self.resets = {}

        def quic_event_received(self, event):
            if isinstance(event, HandshakeCompleted):
                self.handshake = event
            elif isinstance(event, StreamReset):
                self.resets[event.stream_id] = event.error_code
            elif isinstance(event, ConnectionTerminated):
                for future, _, _ in self.responses.values():
                    if not future.done(): future.set_exception(ConnectionError(event.reason_phrase))
            for item in self.http.handle_event(event):
                if item.stream_id not in self.responses:
                    continue
                future, headers, data = self.responses[item.stream_id]
                if isinstance(item, HeadersReceived):
                    if b":status" in dict(item.headers):
                        status = dict(item.headers)[b":status"]
                        self.statuses.append(status)
                        if int(status) < 200: continue
                    headers.extend(item.headers)
                    if item.stream_id in self.headers_ready and not self.headers_ready[item.stream_id].done():
                        self.headers_ready[item.stream_id].set_result(dict(headers))
                elif isinstance(item, H3Data):
                    data.extend(item.data)
                if item.stream_ended and not future.done():
                    future.set_result((dict(headers), bytes(data)))
            # aioquic 1.3 does not emit an H3 end event when FIN follows a GREASE frame.
            if isinstance(event, StreamDataReceived) and event.end_stream and event.stream_id in self.responses:
                future, headers, data = self.responses[event.stream_id]
                if headers and not future.done():
                    future.set_result((dict(headers), bytes(data)))

        async def get(self, path, data=None, content_length=None, authority="example.test"):
            stream = self._quic.get_next_available_stream_id()
            future = asyncio.get_running_loop().create_future()
            self.responses[stream] = (future, [], bytearray())
            headers = [(b":method", b"POST" if data is not None else b"GET"), (b":scheme", b"https"), (b":authority", authority.encode()), (b":path", path.encode())]
            if data is not None:
                headers += [(b"content-length", str(len(data) if content_length is None else content_length).encode())]
            self.http.send_headers(stream, headers, end_stream=data is None)
            if data is not None:
                self.http.send_data(stream, data, end_stream=True)
            self.transmit()
            try:
                return await asyncio.wait_for(future, 5)
            finally:
                self.responses.pop(stream, None)

        async def tunnel(self, authority, path=None, protocol=None, payload=b"opaque-h3", extra=(), half_close=False):
            stream = self._quic.get_next_available_stream_id()
            loop = asyncio.get_running_loop()
            completed, ready = loop.create_future(), loop.create_future()
            self.responses[stream] = (completed, [], bytearray())
            self.headers_ready[stream] = ready
            headers = [(b":method", b"CONNECT"), (b":authority", authority.encode())]
            if protocol:
                headers += [(b":scheme", b"https"), (b":path", path.encode()), (b":protocol", protocol.encode()), (b"sec-websocket-version", b"13")]
            self.http.send_headers(stream, headers + list(extra), end_stream=False)
            self.transmit()
            try:
                headers = await asyncio.wait_for(ready, 3)
                if headers[b":status"] != b"200":
                    return headers, b""
                if payload is None:
                    return headers, stream
                self.http.send_data(stream, payload, end_stream=not half_close)
                self.transmit()
                if half_close:
                    result = await asyncio.wait_for(completed, 3)
                    self.http.send_data(stream, b"client-tail", end_stream=True)
                    self.transmit()
                    await asyncio.sleep(.05)
                    return result
                return await asyncio.wait_for(completed, 3)
            finally:
                self.responses.pop(stream, None)
                self.headers_ready.pop(stream, None)

    settings = QuicConfiguration(is_client=True, alpn_protocols=["h3"], server_name="example.test")
    settings.load_verify_locations(cafile=str(root / "ca.crt"))
    tickets = []
    async with connect("127.0.0.1", port, configuration=settings, create_protocol=Client, session_ticket_handler=tickets.append) as client:
        headers, body = await client.get("/health")
        check("HTTP/3 negotiates TLS and serves the shared route", headers[b":status"] == b"200" and body == b"alive")
        statuses_before = len(client.statuses)
        headers, body = await client.get("/hints")
        check("HTTP/3 forwards interim responses before the final response", client.statuses[statuses_before:] == [b"103", b"102", b"200"] and headers[b":status"] == b"200" and body == b"hinted")
        check("HTTP/3 rejects truncated uploads", (await client.get("/proxy", b"short", content_length=10))[0][b":status"] == b"400")
        headers, body = await client.get("/body", b"route-more-than-prefix")
        check("HTTP/3 uses compiled RGL prefix body inspection", headers[b":status"] == b"201" and body == b"prefix")
        responses = await asyncio.gather(*(client.get("/proxy", bytes([i]) * 32768) for i in range(12)))
        check("HTTP/3 multiplexes streamed uploads without truncation", all(h[b":status"] == b"200" and json.loads(b)["size"] == 32768 for h, b in responses))
        raw = config.read_text().split("location / { rgnix_connect on; proxy_pass http://", 1)[1].split(";", 1)[0]
        headers, body = await client.tunnel(raw)
        check("HTTP/3 CONNECT forwards opaque bytes and half-close", headers[b":status"] == b"200" and body == b"opaque-h3" and b"alt-svc" not in headers)
        payload = bytes(range(256)) * 4096
        check("HTTP/3 tunnels stream beyond the bridge buffer without truncation", (await client.tunnel(raw, payload=payload))[1] == payload)
        headers, body = await client.tunnel(raw, payload=b"half-close", half_close=True)
        check("HTTP/3 upstream half-close preserves the client upload direction", body == b"half-closed" and Echo.half_close_tail == b"client-tail")
        check("HTTP/3 CONNECT rejects arbitrary destinations", (await client.tunnel("unconfigured.test:443"))[0][b":status"] == b"403")
        check("HTTP/3 CONNECT refuses HTTP body framing", (await client.tunnel(raw, extra=[(b"content-length", b"10")]))[0][b":status"] == b"400")
        for path in ["/ws", "/ws-h2", "/ws-auto"]:
            headers, body = await client.tunnel("example.test", path, "websocket", b"\x81\x00")
            check("HTTP/3 WebSocket forwards through " + path, headers[b":status"] == b"200" and body == b"\x81\x00")
        check("HTTP/3 WebSocket enforces route access rules", (await client.tunnel("example.test", "/ws-denied", "websocket"))[0][b":status"] == b"403")
        check("HTTP/3 WebSocket executes RGL before opening the upstream", (await client.tunnel("example.test", "/ws-rgl", "websocket", extra=[(b"x-deny", b"1")]))[0][b":status"] == b"403")
        headers, stream = await client.tunnel("example.test", "/ws-limited", "websocket", payload=None)
        check("HTTP/3 active tunnels retain their route concurrency lease", headers[b":status"] == b"200" and (await client.tunnel("example.test", "/ws-limited", "websocket"))[0][b":status"] == b"503")
        client._quic.reset_stream(stream, 0x10c)
        client._quic.stop_stream(stream, 0x10c)
        client.transmit()
        await asyncio.sleep(.1)
        headers, body = await client.tunnel("example.test", "/ws-limited", "websocket", b"\x81\x00")
        check("HTTP/3 cancellation releases the tunnel concurrency lease", headers[b":status"] == b"200" and body == b"\x81\x00")
        headers, stream = await client.tunnel(raw, payload=None)
        await asyncio.sleep(.3)
        check("HTTP/3 idle tunnel failures reset their stream instead of sending a successful FIN", headers[b":status"] == b"200" and client.resets.get(stream) == 0x10c)
        check("HTTP/3 unknown CONNECT protocol returns 501", (await client.tunnel("example.test", "/ws", "unknown"))[0][b":status"] == b"501")
        check("HTTP/3 rejected tunnels do not disrupt sibling requests", (await client.get("/health"))[1] == b"alive")
    check("HTTP/3 issues bounded session resumption tickets", len(tickets) == 2)
    ticket = tickets[-1]
    unused_ticket = tickets[-2]
    check("HTTP/3 resumption tickets do not authorize early data", not ticket.max_early_data_size)
    settings.session_ticket = ticket
    async with connect("127.0.0.1", port, configuration=settings, create_protocol=Client) as client:
        check("HTTP/3 resumes within its current TLS generation without early data", (await client.get("/health"))[0][b":status"] == b"200" and client.handshake.session_resumed and not client.handshake.early_data_accepted)
    config.write_text(config.read_text().replace("location /health { return 200 alive; }", "location /health { add_header X-Revision h3-route; return 200 alive; }"))
    process.send_signal(signal.SIGHUP)
    await asyncio.sleep(.3)
    settings.session_ticket = unused_ticket
    renewed = []
    async with connect("127.0.0.1", port, configuration=settings, create_protocol=Client, session_ticket_handler=renewed.append) as client:
        check("HTTP/3 TLS tickets survive route-only publications", (await client.get("/health"))[0][b":status"] == b"200" and client.handshake.session_resumed)
        await asyncio.sleep(.05)
    settings.session_ticket = renewed[-1]
    rotate_server_certificate(root)
    process.send_signal(signal.SIGHUP)
    await asyncio.sleep(.3)
    async with connect("127.0.0.1", port, configuration=settings, create_protocol=Client) as client:
        check("HTTP/3 credential publication invalidates previous TLS tickets", (await client.get("/health"))[0][b":status"] == b"200" and not client.handshake.session_resumed)
    settings.session_ticket = None
    tickets.clear()
    async with connect("127.0.0.1", shared_tls, configuration=settings, create_protocol=Client, session_ticket_handler=tickets.append) as client:
        check("HTTP/3 shared SNI listener serves its public host", (await client.get("/"))[0][b":status"] == b"200")
    settings.session_ticket = tickets[-1]
    unused_anonymous = tickets[-2]
    async with connect("127.0.0.1", shared_tls, configuration=settings, create_protocol=Client) as client:
        check("HTTP/3 resumed public sessions cannot access the mTLS sibling host", (await client.get("/", authority="private.test"))[0][b":status"] == b"403" and client.handshake.session_resumed)
    settings.load_cert_chain(str(root / "client.crt"), str(root / "client.key"))
    settings.session_ticket = unused_anonymous
    async with connect("127.0.0.1", shared_tls, configuration=settings, create_protocol=Client) as client:
        check("HTTP/3 adding client credentials cannot upgrade an anonymous resumption ticket", (await client.get("/", authority="private.test"))[0][b":status"] == b"403" and client.handshake.session_resumed)
    settings.session_ticket = None
    async with connect("127.0.0.1", shared_tls, configuration=settings, create_protocol=Client) as client:
        check("HTTP/3 fresh authenticated handshakes reach the mTLS sibling host", (await client.get("/", authority="private.test"))[0][b":status"] == b"200")
    tickets.clear()
    async with connect("127.0.0.1", mtls, configuration=settings, create_protocol=Client, session_ticket_handler=tickets.append) as client:
        check("HTTP/3 mTLS accepts the configured client CA", (await client.get("/"))[0][b":status"] == b"200")
    check("HTTP/3 authenticated sessions receive resumption tickets", bool(tickets))
    settings.session_ticket = tickets[-1]
    async with connect("127.0.0.1", mtls, configuration=settings, create_protocol=Client) as client:
        check("HTTP/3 mTLS resumes with the original peer identity", (await client.get("/"))[0][b":status"] == b"200" and client.handshake.session_resumed)
        config.write_text(config.read_text().replace(f"ssl_client_certificate {root}/ca.crt", f"ssl_client_certificate {root}/other-ca.crt"))
        process.send_signal(signal.SIGHUP)
        await asyncio.sleep(.3)
        check("HTTP/3 CA rotation revokes clients on an existing connection", (await client.get("/"))[0][b":status"] == b"403")


def rotate_server_certificate(root):
    (root / "server.extensions").write_text("subjectAltName=DNS:example.test,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n")
    subprocess.run(["openssl", "x509", "-req", "-in", str(root / "server.csr"), "-CA", str(root / "ca.crt"), "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-out", str(root / "server.crt"), "-days", "2", "-extfile", str(root / "server.extensions")], check=True, capture_output=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--http3", action="store_true")
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    raw, ws, multiplex, upstream = origin(Echo), origin(WebSocket), origin(H2Echo), start_backend()
    hints = origin(EarlyHints)
    process = None
    with tempfile.TemporaryDirectory(prefix="rgnix-protocol-") as directory:
        root = Path(directory)
        certificates(root)
        # Give the shared-listener fixture a certificate valid for both SNI hosts.
        (root / "sni.extensions").write_text("subjectAltName=DNS:example.test,DNS:private.test\nextendedKeyUsage=serverAuth\n")
        subprocess.run(["openssl", "x509", "-req", "-in", str(root / "server.csr"), "-CA", str(root / "ca.crt"), "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-out", str(root / "sni.crt"), "-days", "2", "-extfile", str(root / "sni.extensions")], check=True, capture_output=True)
        (root / "web").mkdir()
        data = bytes(range(256)) * 32768
        (root / "web/large.bin").write_bytes(data)
        (root / "body.rgl").write_text('function on_request() if req.body() == "route" then return resp.reply(201, "prefix") end return route.pass() end')
        (root / "ws.rgl").write_text('function on_request() if req.header("x-deny") == "1" then return resp.reply(403, "denied") end return route.pass() end')
        port, tls, mtls, shared_tls, admin = [free_port() for _ in range(5)]
        config = root / "nginx.conf"
        h3 = "http3 on;" if args.http3 else ""
        config.write_text(f'''http {{ error_log stderr warn; access_log off; client_header_timeout 500ms; client_body_timeout 150ms; send_timeout 150ms; keepalive_timeout 3s;
server {{ listen 127.0.0.1:{port}; http2 on; server_name example.test;
location /health {{ keepalive_timeout 100ms; return 200 alive; }}
location /proxy {{ proxy_pass http://127.0.0.1:{upstream.server_port}; }}
location /hints {{ proxy_pass http://127.0.0.1:{hints.server_address[1]}; }}
location /quiet {{ keepalive_timeout 100ms; proxy_pass http://127.0.0.1:{upstream.server_port}/slow; }}
location /body {{ rgnix_request_body prefix 5; rgnix_script {root}/body.rgl; return 200 fallback; }}
location /ws-auto {{ proxy_http_version auto; proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location /files/ {{ alias {root}/web/; }}
location /ws-h2 {{ proxy_http_version 2; proxy_pass http://127.0.0.1:{multiplex.server_address[1]}; }}
location /ws {{ proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location /ws-long {{ client_body_timeout 2s; send_timeout 2s; proxy_read_timeout 2s; proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location / {{ rgnix_connect on; proxy_pass http://127.0.0.1:{raw.server_address[1]}; }}
}}
server {{ listen 127.0.0.1:{tls} ssl; http2 on; {h3} server_name example.test;
ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key;
location /health {{ return 200 alive; }}
location /hints {{ proxy_pass http://127.0.0.1:{hints.server_address[1]}; }}
location /body {{ rgnix_request_body prefix 5; rgnix_script {root}/body.rgl; return 200 fallback; }}
location /proxy {{ proxy_pass http://127.0.0.1:{upstream.server_port}; }}
location /ws {{ proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location /ws-auto {{ proxy_http_version auto; proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location /ws-h2 {{ proxy_http_version 2; proxy_pass http://127.0.0.1:{multiplex.server_address[1]}; }}
location /ws-denied {{ deny all; proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location /ws-rgl {{ rgnix_script {root}/ws.rgl; proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location /ws-limited {{ client_body_timeout 2s; proxy_read_timeout 2s; rgnix_limit_conn 1; proxy_pass http://127.0.0.1:{ws.server_address[1]}; }}
location / {{ rgnix_connect on; proxy_pass http://127.0.0.1:{raw.server_address[1]}; }}
}}
server {{ listen 127.0.0.1:{mtls} ssl; {h3} server_name example.test;
ssl_certificate {root}/server.crt; ssl_certificate_key {root}/server.key;
ssl_client_certificate {root}/ca.crt; ssl_verify_client on; return 200 mutual;
}}
server {{ listen 127.0.0.1:{shared_tls} ssl; {h3} server_name example.test;
ssl_certificate {root}/sni.crt; ssl_certificate_key {root}/server.key; return 200 public;
}}
server {{ listen 127.0.0.1:{shared_tls} ssl; {h3} server_name private.test;
ssl_certificate {root}/sni.crt; ssl_certificate_key {root}/server.key;
ssl_client_certificate {root}/ca.crt; ssl_verify_client on; return 200 private;
}} }}''')
        with (root / "server.log").open("w+") as output:
            try:
                process = subprocess.Popen([binary, "serve", "-c", str(config), "--experimental-hyper", "--threads", "2", "--admin", f"127.0.0.1:{admin}", "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "3"], stdout=output, stderr=output)
                wait_for(lambda: request(admin, "/readyz")[0], 200)
                check("Hyper runs without the Pingora process host", request(port, "/health")[2] == b"alive")
                conn = socket.create_connection(("127.0.0.1", port), timeout=3)
                conn.sendall(f"CONNECT 127.0.0.1:{raw.server_address[1]} HTTP/1.1\r\nHost: example.test\r\n\r\n".encode())
                headers = bytearray()
                while not headers.endswith(b"\r\n\r\n"):
                    headers.extend(conn.recv(1))
                check("HTTP/1 CONNECT reaches only the configured endpoint", headers.startswith(b"HTTP/1.1 200"))
                conn.sendall(b"tunnel"); check("CONNECT forwards both directions", conn.recv(6) == b"tunnel"); conn.close()
                check("CONNECT rejects arbitrary destinations", request(port, "unconfigured.test:443", "CONNECT", {"Host": "example.test"})[0] == 403)
                peer = H2Peer(port)
                peer.until(lambda events: any(isinstance(e, RemoteSettingsChanged) and SettingCodes.ENABLE_CONNECT_PROTOCOL in e.changed_settings for e in events))
                for stream in [1, 3]:
                    peer.send(stream, "/ws", True)
                peer.until(lambda events: sum(isinstance(e, ResponseReceived) for e in events) == 2)
                check("HTTP/2 opens simultaneous WebSocket tunnels", all(dict(e.headers)[":status"] == "200" for e in peer.events if isinstance(e, ResponseReceived)))
                for stream in [1, 3]:
                    peer.connection.send_data(stream, b"\x81\x82mask" + bytes([ord('o') ^ ord('m'), ord('k') ^ ord('a')]))
                peer.socket.sendall(peer.connection.data_to_send())
                peer.until(lambda events: len({e.stream_id for e in events if isinstance(e, DataReceived)}) == 2)
                check("HTTP/2 WebSocket streams remain isolated", all(e.data == b"\x81\x02ok" for e in peer.events if isinstance(e, DataReceived)))
                peer.socket.close()
                peer = H2Peer(port)
                peer.send(1, "/ws-h2", True)
                peer.until(lambda events: any(isinstance(e, ResponseReceived) for e in events))
                peer.connection.send_data(1, b"h2-websocket")
                peer.socket.sendall(peer.connection.data_to_send())
                peer.until(lambda events: any(isinstance(e, DataReceived) for e in events))
                check("WebSocket supports HTTP/2 upstreams", b"".join(e.data for e in peer.events if isinstance(e, DataReceived)) == b"h2-websocket")
                peer.socket.close()
                peer = H2Peer(port, upgrade=True)
                peer.until(lambda events: any(isinstance(e, StreamEnded) and e.stream_id == 1 for e in events))
                check("h2c upgrade delivers the original request exactly once", b"".join(e.data for e in peer.events if isinstance(e, DataReceived) and e.stream_id == 1) == b"alive")
                peer.send(3, "/health")
                peer.until(lambda events: any(isinstance(e, StreamEnded) and e.stream_id == 3 for e in events))
                check("h2c upgrade accepts subsequent HTTP/2 requests", b"".join(e.data for e in peer.events if isinstance(e, DataReceived) and e.stream_id == 3) == b"alive")
                peer.socket.close()
                peer = H2Peer(port, True, "/proxy", b"initial-body")
                peer.until(lambda events: any(isinstance(e, StreamEnded) and e.stream_id == 1 for e in events))
                payload = b"".join(e.data for e in peer.events if isinstance(e, DataReceived) and e.stream_id == 1)
                check("h2c consumes and forwards its initial POST body", json.loads(payload)["size"] == 12)
                peer.socket.close()
                peer = H2Peer(port, True, "/body", b"route-long-body")
                peer.until(lambda events: any(isinstance(e, StreamEnded) and e.stream_id == 1 for e in events))
                check("h2c initial request applies RGL body routing", any(isinstance(e, ResponseReceived) and dict(e.headers)[":status"] == "201" for e in peer.events))
                peer.socket.close()
                check("h2c caps its negotiation body", request(port, "/proxy", "POST", {"Connection": "Upgrade, HTTP2-Settings", "Upgrade": "h2c", "HTTP2-Settings": "", "Content-Length": str(2 * 1024 * 1024)}, b"")[0] == 413)
                for path in ["/ws-h2", "/ws-auto"]:
                    with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                        key = base64.b64encode(b"1234567890123456")
                        client.sendall(b"GET " + path.encode() + b" HTTP/1.1\r\nHost: example.test\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: " + key + b"\r\nSec-WebSocket-Version: 13\r\n\r\n")
                        headers = bytearray()
                        while not headers.endswith(b"\r\n\r\n"):
                            chunk = client.recv(1)
                            if not chunk: raise EOFError("WebSocket handshake")
                            headers.extend(chunk)
                        check("HTTP/1 WebSocket negotiates " + path, headers.startswith(b"HTTP/1.1 101") and b"sec-websocket-accept:" in headers.lower())
                        client.sendall(b"\x81\x00")
                        check("HTTP/1 WebSocket transfers " + path, client.recv(2) == b"\x81\x00")
                peer = H2Peer(port)
                peer.connection.config.validate_outbound_headers = False
                peer.connection.send_headers(1, [(":method", "CONNECT"), (":authority", f"127.0.0.1:{raw.server_address[1]}")], end_stream=False)
                peer.socket.sendall(peer.connection.data_to_send())
                peer.until(lambda events: any(isinstance(e, ResponseReceived) for e in events))
                check("HTTP/2 supports configured TCP CONNECT", next(dict(e.headers)[":status"] for e in peer.events if isinstance(e, ResponseReceived)) == "200")
                peer.connection.send_data(1, b"connect-h2")
                peer.socket.sendall(peer.connection.data_to_send())
                peer.until(lambda events: any(isinstance(e, DataReceived) for e in events))
                check("HTTP/2 CONNECT carries opaque bytes", b"".join(e.data for e in peer.events if isinstance(e, DataReceived)) == b"connect-h2")
                peer.socket.close()
                h2_header_cases(port)
                peer = H2Peer(port)
                peer.send(1, "/health")
                peer.until(lambda events: any(isinstance(e, StreamEnded) for e in events))
                peer.send(3, "/quiet")
                peer.send(5, "/quiet")
                peer.until(lambda events: len({e.stream_id for e in events if isinstance(e, StreamEnded)}) == 3)
                check("quiet active HTTP/2 streams outlive connection idle deadlines", all(dict(e.headers)[":status"] == "200" for e in peer.events if isinstance(e, ResponseReceived)))
                peer.socket.close()
                with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                    client.sendall(b"POST /proxy HTTP/1.1\r\nHost: example.test\r\nContent-Length: 100\r\n\r\nx")
                    response = http.client.HTTPResponse(client); response.begin()
                    check("client_body_timeout returns a bounded HTTP error", response.status == 408)
                    response.read()
                with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                    client.sendall(b"GET /health HTTP/1.1\r\nHost:")
                    time.sleep(.6)
                    response = client.recv(65536)
                    check("client_header_timeout closes incomplete headers", not response or b"408" in response)
                status, headers, body = request(port, "/files/large.bin")
                check("sendfile sends a complete file with correct framing", status == 200 and body == data and int(headers["content-length"]) == len(data))
                check("sendfile preserves a bounded range", request(port, "/files/large.bin", headers={"Range": "bytes=65530-65550"})[2] == data[65530:65551])
                check("sendfile preserves HEAD semantics", request(port, "/files/large.bin", "HEAD")[2] == b"")
                with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                    client.sendall(b"GET /files/large.bin HTTP/1.1\r\nHost: example.test\r\nRange: bytes=65530-65550\r\n\r\nGET /health HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                    wire = bytearray()
                    while block := client.recv(65536): wire.extend(block)
                    check("sendfile preserves pipelined response order", wire.count(b"HTTP/1.1 2") == 2 and data[65530:65551] in wire and wire.endswith(b"alive"))
                mutable = root / "web/mutable.bin"
                with mutable.open("wb") as file: file.truncate(16 * 1024 * 1024)
                with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                    client.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 65536)
                    client.sendall(b"GET /files/mutable.bin HTTP/1.1\r\nHost: example.test\r\n\r\n")
                    response = http.client.HTTPResponse(client); response.begin()
                    mutable.write_bytes(b"changed")
                    incomplete = False
                    try: response.read()
                    except http.client.IncompleteRead: incomplete = True
                    check("sendfile truncation closes an incomplete response", incomplete)
                check("sendfile failure leaves the listener available", request(port, "/health")[2] == b"alive")
                context = ssl.create_default_context(cafile=str(root / "ca.crt"))
                context.maximum_version = ssl.TLSVersion.TLSv1_2
                session = None
                reused = []
                for _ in range(2):
                    with context.wrap_socket(socket.create_connection(("127.0.0.1", tls)), server_hostname="example.test", session=session) as sock:
                        sock.sendall(b"GET /health HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                        while sock.recv(65536):
                            pass
                        reused.append(sock.session_reused)
                        session = sock.session
                check("TLS resumes within the active certificate generation", reused == [False, True])
                config.write_text(config.read_text().replace("error_log stderr warn;", "error_log stderr warn; add_header X-Revision route-only;"))
                process.send_signal(signal.SIGHUP)
                time.sleep(.3)
                with context.wrap_socket(socket.create_connection(("127.0.0.1", tls)), server_hostname="example.test", session=session) as sock:
                    check("TLS resumption survives route-only publications", sock.session_reused)
                    sock.sendall(b"GET /health HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                    while sock.recv(65536): pass
                    session = sock.session
                rotate_server_certificate(root)
                process.send_signal(signal.SIGHUP)
                time.sleep(.3)
                with context.wrap_socket(socket.create_connection(("127.0.0.1", tls)), server_hostname="example.test", session=session) as sock:
                    check("TLS credential publication invalidates old sessions", not sock.session_reused)
                context13 = ssl.create_default_context(cafile=str(root / "ca.crt"))
                context13.minimum_version = ssl.TLSVersion.TLSv1_3
                resumed13, session13 = [], None
                for _ in range(2):
                    with context13.wrap_socket(socket.create_connection(("127.0.0.1", tls)), server_hostname="example.test", session=session13) as client:
                        client.sendall(b"GET /health HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                        while client.recv(65536): pass
                        resumed13.append(client.session_reused); session13 = client.session
                check("TLS 1.3 resumes within the current generation", resumed13 == [False, True])
                mutual = ssl.create_default_context(cafile=str(root / "ca.crt"))
                mutual.minimum_version = ssl.TLSVersion.TLSv1_3
                mutual.load_cert_chain(root / "client.crt", root / "client.key")
                previous, tickets, resumed = None, [], []
                for _ in range(2):
                    with mutual.wrap_socket(socket.create_connection(("127.0.0.1", mtls)), server_hostname="example.test", session=previous) as client:
                        client.sendall(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                        while client.recv(65536): pass
                        previous = client.session
                        tickets.append(previous.has_ticket)
                        resumed.append(client.session_reused)
                check("mTLS issues no resumption tickets or reusable sessions", not any(tickets) and not any(resumed))
                if args.http3:
                    asyncio.run(h3_cases(tls, mtls, shared_tls, root, process, config))
            except Exception:
                output.flush(); print((root / "server.log").read_text()[-6000:])
                raise
            finally:
                if process and process.poll() is None:
                    process.send_signal(signal.SIGINT); process.wait(timeout=15)
                if process: check("native host exits within its drain deadline", process.returncode == 0)
                for server in [raw, ws, multiplex, upstream, hints]:
                    server.shutdown()
    print(json.dumps({"passed": len(RESULTS), "checks": RESULTS}, indent=2))


if __name__ == "__main__":
    main()

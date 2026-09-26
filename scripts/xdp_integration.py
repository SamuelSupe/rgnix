#!/usr/bin/env python3
"""Privileged Linux XDP regression gate; only creates and touches its own veth/netns."""
import argparse
import hashlib
import http.server
import ipaddress
import json
import os
import re
import pathlib
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import urllib.request


def run(*args, ok=True):
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True, timeout=40)
    if ok and result.returncode:
        raise AssertionError(f"{args}: {result.stderr}")
    return result


def frame(version=4, source=None, dest=None, protocol=6, port=443, flags=2, fragment=False, vlan=0, extension=False, options=False):
    transport = struct.pack("!HHIIHHHH", 2000, port, 0, 0, (5 << 12) | flags, 4096, 0, 0) if protocol == 6 else struct.pack("!HHHH", 2000, port, 8, 0)
    if version == 4:
        source = ipaddress.ip_address(source or "198.51.100.2").packed
        dest = ipaddress.ip_address(dest or "198.51.100.1").packed
        ihl = 6 if options else 5
        packet = struct.pack("!BBHHHBBH4s4s", 0x40 | ihl, 0, ihl * 4 + len(transport), 1, 0x2000 if fragment else 0, 64, protocol, 0, source, dest)
        packet += (b"\1" * 4 if options else b"") + transport
        ether_type = 0x0800
    else:
        source = ipaddress.ip_address(source or "2001:db8:1::2").packed
        dest = ipaddress.ip_address(dest or "2001:db8:1::1").packed
        extra = bytes([protocol, 0, 0, 1, 0, 0, 0, 0]) if fragment else (bytes([protocol, 0]) + b"\0" * 6 if extension else b"")
        next_header = 44 if fragment else (0 if extension else protocol)
        packet = struct.pack("!IHBB16s16s", 6 << 28, len(extra) + len(transport), next_header, 64, source, dest) + extra + transport
        ether_type = 0x86dd
    ethernet = bytes.fromhex("020000000001020000000002")
    for _ in range(vlan):
        ethernet += struct.pack("!HH", 0x8100, 1)
    return ethernet + struct.pack("!H", ether_type) + packet


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument("--libxdp", action="store_true", help="Also test real xdp-loader dispatcher coexistence")
    args = parser.parse_args()
    binary = args.binary.resolve()
    restricted = ["setpriv", "--bounding-set=-all,+bpf,+net_admin,+perfmon", "--inh-caps=+bpf,+net_admin,+perfmon", "--ambient-caps=+bpf,+net_admin,+perfmon", "--no-new-privs"]
    if os.geteuid() != 0:
        raise SystemExit("Run as root in an isolated Linux test VM; BPF and veth creation are required.")
    results = []

    def passed(name):
        results.append({"name": name, "status": "PASS"})
        print("PASS", name, flush=True)

    with tempfile.TemporaryDirectory(prefix="rgnix-xdp-") as temporary:
        temp = pathlib.Path(temporary)
        policy = temp / "edge.rgl"
        obj = temp / "edge.o"
        policy.write_text('''function on_xdp()
    local blocked = pkt.src_in("192.0.2.0/24") or pkt.src_in("2001:db8:bad::/48")
    if blocked or pkt.fragmented() then return xdp.drop() end
    if pkt.ip_version() == 6 and pkt.dst_in("2001:db8:2::/64") then
        blocked = pkt.is_udp() and pkt.has_ports() and pkt.src_port() >= 1024 and pkt.dst_port() == 53
    elseif pkt.is_tcp() and pkt.tcp_syn() and pkt.dst_port() == 444 then
        blocked = true
    end
    if blocked then return xdp.drop() end
    return xdp.pass()
end''')
        run(binary, "xdp", "compile", policy, "-o", obj)
        run(binary, "xdp", "check", obj, "--kernel")
        passed("RGL compiler and real kernel verifier")
        run(*restricted, binary, "xdp", "check", obj, "--kernel")
        passed("documented capability set passes verifier without SYS_ADMIN")
        original = hashlib.sha256(obj.read_bytes()).hexdigest()
        predicate = temp / "predicate.rgl"
        predicate.write_text('function on_xdp() if pkt.is_udp() then return xdp.drop("udp") end return xdp.pass("allow") end')
        run(binary, "xdp", "compile", predicate, "-o", temp / "predicate.o")
        packet = temp / "predicate.bin"
        packet.write_bytes(frame(protocol=17))
        assert json.loads(run(binary, "xdp", "test", temp / "predicate.o", "--packet", packet).stdout)["action"] == "drop"
        passed("standalone boolean predicates compile and execute")
        for bad in ['while true do end', 'return req.header("host")', 'if pkt.dst_port() then return xdp.drop() end', 'if xdp.allow_rate(0) then return xdp.pass() end', 'if pkt.src_in("bad") then return xdp.drop() end']:
            policy.write_text(f"function on_xdp() {bad} end")
            assert run(binary, "xdp", "compile", policy, "-o", obj, ok=False).returncode != 0
            assert hashlib.sha256(obj.read_bytes()).hexdigest() == original
        passed("unsafe or ill-typed policies rejected without overwriting valid object")
        policy.write_text("function on_request() return route.pass() end function on_xdp() return xdp.drop() end")
        assert run(binary, "compile", policy, "-o", temp / "http.wasm", ok=False).returncode != 0
        passed("HTTP plugins cannot silently contain a privileged XDP entry")

        cases = [
            ("IPv4 TCP pass", frame(), "pass"),
            ("IPv4 source CIDR deny", frame(source="192.0.2.7"), "drop"),
            ("IPv6 source CIDR deny", frame(version=6, source="2001:db8:bad::7"), "drop"),
            ("IPv6 TCP pass", frame(version=6), "pass"),
            ("TCP initial SYN deny", frame(port=444), "drop"),
            ("TCP SYN ACK passes initial SYN filter", frame(port=444, flags=18), "pass"),
            ("UDP is distinct from TCP", frame(protocol=17, port=444), "pass"),
            ("IPv6 destination and UDP port deny", frame(version=6, dest="2001:db8:2::3", protocol=17, port=53), "drop"),
            ("IPv4 options", frame(options=True), "pass"),
            ("IPv6 extension headers", frame(version=6, extension=True), "pass"),
            ("IPv6 extensions cannot bypass CIDR", frame(version=6, extension=True, source="2001:db8:bad::1"), "drop"),
            ("single VLAN", frame(vlan=1, source="192.0.2.7"), "drop"),
            ("double VLAN", frame(vlan=2, source="192.0.2.7"), "drop"),
            ("IPv4 fragment policy", frame(fragment=True), "drop"),
            ("IPv6 fragment policy", frame(version=6, fragment=True), "drop"),
            ("ARP passes", bytes.fromhex("ffffffffffff0200000000010806") + b"\0" * 46, "pass"),
            ("truncated IPv4 drops", frame()[:24], "drop"),
            ("truncated IPv6 drops", frame(version=6)[:50], "drop"),
            ("truncated TCP drops", frame()[:-1], "drop"),
            ("excess VLAN drops", frame(vlan=3), "drop"),
        ]
        for name, data, expected in cases:
            packet = temp / "frame.bin"
            packet.write_bytes(data)
            result = json.loads(run(binary, "xdp", "test", obj, "--packet", packet).stdout)
            assert result["action"] == expected, (name, result)
            passed(name)

        policy.write_text('''function on_xdp()
            if pkt.tcp_syn() and not xdp.allow_rate(1) then return xdp.drop() end
            return xdp.pass()
        end''')
        run(binary, "xdp", "compile", policy, "-o", obj)
        packet.write_bytes(frame())
        result = json.loads(run(binary, "xdp", "test", obj, "--packet", packet, "--repeat", 1000).stdout)
        assert result["counters"][1] >= 998 and result["counters"][0] <= 2, result
        assert result["counters"][3] == result["counters"][1], result
        passed("kernel fixed-window packet budget and denial counters")

        config = temp / "policy.json"
        capture = temp / "packets.pcap"

        def replay(frames, settings=None):
            config.write_text(json.dumps(settings or {}))
            capture.write_bytes(struct.pack("<IHHIIII", 0xa1b2c3d4, 2, 4, 0, 0, 65536, 1) + b"".join(struct.pack("<IIII", 0, i, len(data), len(data)) + data for i, data in enumerate(frames)))
            return [json.loads(line) for line in run(binary, "xdp", "replay", policy, "--pcap", capture, "--config", config).stdout.splitlines()]

        policy.write_text('function on_xdp() return xdp.drop("blocked") end')
        rows = replay([frame(), frame(port=22), frame()[:24]], {"observe": True, "scope": {"ports": [443], "protocols": [6]}})
        assert [row["action"] for row in rows] == ["pass"] * 3
        assert rows[0]["hits"] == [{"rule": "blocked", "action": "would_drop"}]
        assert rows[1]["hits"][0]["rule"] == rows[2]["hits"][0]["rule"] == "scope_bypass"
        passed("observation and port scope pass real frames with explained rule decisions")
        rows = replay([frame(), frame(dest="192.0.2.1"), frame(protocol=17)], {"scope": {"destinations": ["198.51.100.0/24"], "protocols": [6]}})
        assert [row["action"] for row in rows] == ["drop", "pass", "pass"]
        rows = replay([frame()[:24], frame(vlan=3), frame(fragment=True)], {"malformed": "pass", "unsupported": "pass", "fragments": "pass"})
        assert all(row["action"] == "pass" for row in rows)
        passed("destination and protocol scope plus explicit parser and fragment actions")
        policy.write_text('function on_xdp() if not xdp.allow("clients", "src_ip", 1, 2) then return xdp.drop("rate") end return xdp.pass("accepted") end')
        rows = replay([frame(), frame(), frame(), frame(source="198.51.100.3")])
        assert [row["action"] for row in rows] == ["pass", "pass", "drop", "pass"], rows
        passed("per-source token bucket isolates another client's burst budget")
        rows = replay([frame(), frame(source="198.51.100.3")], {"ceiling_pps": 1, "ceiling_burst": 1})
        assert rows[1]["hits"][0]["rule"] == "global_ceiling" and rows[1]["action"] == "drop"
        passed("independent global ceiling bounds new-source and LRU churn")
        policy.write_text('function on_xdp() if not xdp.allow_bytes("bytes", "dst_port", 1, 108) then return xdp.drop("bytes") end return xdp.pass() end')
        rows = replay([frame(), frame(), frame(), frame(port=8443)])
        assert [row["action"] for row in rows] == ["pass", "pass", "drop", "pass"]
        passed("byte token bucket enforces cost and independent destination port budgets")
        policy.write_text('function on_xdp() if not xdp.allow("subnets", "src_subnet", 1, 1) then return xdp.drop() end return xdp.pass() end')
        rows = replay([frame(), frame(source="198.51.100.99"), frame(source="198.51.101.1"), frame(version=6), frame(version=6, source="2001:db8:1::99"), frame(version=6, source="2001:db8:2::1")])
        assert [row["action"] for row in rows] == ["pass", "drop", "pass", "pass", "drop", "pass"]
        passed("IPv4 /24 and IPv6 /64 share bounded subnet budgets")
        policy.write_text('function on_xdp() if pkt.src_in_set("blocked") then return xdp.drop("blocklist") end return xdp.pass() end')
        rows = replay([frame(), frame(version=6), frame(source="192.0.2.1")], {"sets": {"blocked": [{"cidr": "198.51.100.0/24"}, {"cidr": "2001:db8::/32"}, {"cidr": "192.0.2.0/24", "expires_at": 1}]}})
        assert [row["action"] for row in rows] == ["drop", "drop", "pass"]
        passed("dynamic IPv4 and IPv6 LPM sets omit expired entries")
        policy.write_text('function on_xdp()\n local invalid = pkt.unknown()\n return xdp.pass()\nend')
        error = run(binary, "xdp", "compile", policy, "-o", obj, ok=False)
        assert error.returncode and "2:2" in error.stderr, error.stderr
        passed("RGL type and API diagnostics include the statement source line and column")
        policy.write_text('function on_xdp() return xdp.pass("ok") end')
        run(binary, "xdp", "compile", policy, "-o", obj)
        artifact_digest = hashlib.sha256(obj.read_bytes()).hexdigest()
        run(binary, "xdp", "compile", policy, "-o", obj)
        assert hashlib.sha256(obj.read_bytes()).hexdigest() == artifact_digest
        config.write_text(json.dumps({"policy_sha256": "0" * 64}))
        assert run(binary, "xdp", "check", obj, "--config", config, "--kernel", ok=False).returncode != 0
        passed("reproducible object digest and immutable artifact integrity validation")

        suffix = str(os.getpid())
        host, peer, namespace = "rgx" + suffix, "rgp" + suffix, "rgnix-xdp-" + suffix
        agent = None
        echo = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        stopped = threading.Event()
        try:
            run("ip", "netns", "add", namespace)
            run("ip", "link", "add", host, "type", "veth", "peer", "name", peer)
            run("ip", "link", "set", peer, "netns", namespace)
            run("ip", "addr", "add", "198.18.255.1/30", "dev", host)
            run("ip", "link", "set", host, "up")
            run("ip", "netns", "exec", namespace, "ip", "addr", "add", "198.18.255.2/30", "dev", peer)
            run("ip", "netns", "exec", namespace, "ip", "link", "set", peer, "up")
            run("ip", "netns", "exec", namespace, "ip", "link", "set", "lo", "up")
            echo.bind(("198.18.255.1", 0))
            echo.settimeout(.2)
            port = echo.getsockname()[1]

            def echo_loop():
                while not stopped.is_set():
                    try:
                        data, address = echo.recvfrom(1024)
                        echo.sendto(data, address)
                    except socket.timeout:
                        pass
            thread = threading.Thread(target=echo_loop)
            thread.start()

            def probe():
                code = f'import socket; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(0.3); s.sendto(b"probe", ("198.18.255.1",{port})); assert s.recv(100)==b"probe"'
                return run("ip", "netns", "exec", namespace, "python3", "-c", code, ok=False).returncode == 0

            assert probe()
            with socket.socket() as free:
                free.bind(("127.0.0.1", 0))
                admin = free.getsockname()[1]
            policy.write_text(f"function on_xdp() if pkt.is_udp() and pkt.dst_port() == {port} then return xdp.drop() end return xdp.pass() end")
            run(binary, "xdp", "compile", policy, "-o", obj)
            with (temp / "agent.log").open("w") as logs:
                command = restricted + [str(binary), "xdp", "run", str(obj), "--interface", host, "--mode", "generic", "--admin", f"127.0.0.1:{admin}", "--clang", "/nonexistent-clang"]
                agent = subprocess.Popen(command, stdout=logs, stderr=logs)

                def metrics():
                    with urllib.request.urlopen(f"http://127.0.0.1:{admin}/metrics", timeout=2) as response:
                        return response.read().decode()

                def wait_for(predicate):
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline:
                        assert agent.poll() is None, (temp / "agent.log").read_text()
                        try:
                            if predicate():
                                return
                        except (OSError, TimeoutError):
                            pass
                        time.sleep(.05)
                    raise AssertionError((temp / "agent.log").read_text())

                wait_for(lambda: "config_generation 1" in metrics())
                assert not probe()
                passed("generic XDP drops real veth traffic before userspace")
                duplicate = run(binary, "xdp", "run", obj, "--interface", host, "--mode", "generic", "--admin", "127.0.0.1:0", ok=False)
                assert duplicate.returncode != 0
                assert not probe()
                passed("existing attachment cannot be replaced by another agent")
                obj.write_bytes(b"bad object")
                agent.send_signal(signal.SIGHUP)
                wait_for(lambda: "reload_failures_total 1" in metrics())
                assert not probe() and "config_generation 1" in metrics()
                passed("failed reload retains the active kernel policy")
                invalid = temp / "invalid.c"
                invalid.write_text('struct ctx { unsigned data; }; __attribute__((section("xdp"))) int rgnix_xdp(struct ctx *c) { return *(volatile int *)(unsigned long)c->data; } char qa_license[] __attribute__((section("license"))) = "GPL";')
                run("clang", "-target", "bpfel", "-O2", "-c", invalid, "-o", obj)
                agent.send_signal(signal.SIGHUP)
                wait_for(lambda: "reload_failures_total 2" in metrics())
                assert not probe() and "config_generation 1" in metrics()
                passed("kernel verifier rejection retains the active filter")
                policy.write_text("function on_xdp() return xdp.pass() end")
                run(binary, "xdp", "compile", policy, "-o", obj)
                agent.send_signal(signal.SIGHUP)
                wait_for(lambda: "config_generation 2" in metrics())
                assert probe()
                passed("atomic replacement allows real traffic and advances generation")
                agent.send_signal(signal.SIGHUP)
                time.sleep(.3)
                assert "config_generation 2" in metrics()
                passed("unchanged reload preserves generation and rate state")
                policy.write_text(f"function on_xdp() if pkt.is_udp() and pkt.dst_port() == {port} then return xdp.drop() end return xdp.pass() end")
                run(binary, "xdp", "compile", policy, "-o", obj)
                agent.send_signal(signal.SIGHUP)
                wait_for(lambda: "config_generation 3" in metrics())
                assert not probe()
                agent.terminate()
                assert agent.wait(timeout=5) == 0
                assert probe()
                passed("graceful exit detaches only the owned filter")
                command[command.index("generic")] = "native"
                agent = subprocess.Popen(command, stdout=logs, stderr=logs)
                wait_for(lambda: "config_generation 1" in metrics())
                assert not probe()
                passed("native XDP on veth enforces the same packet policy")
                agent.kill()
                agent.wait(timeout=5)
                assert probe()
                passed("SIGKILL releases the unpinned link and restores normal forwarding")
                command[command.index(str(obj))] = str(policy)
                command[command.index("/nonexistent-clang")] = "clang"
                agent = subprocess.Popen(command, stdout=logs, stderr=logs)
                wait_for(lambda: "config_generation 1" in metrics())
                assert not probe()
                policy.write_text("function on_xdp() while true do end end")
                agent.send_signal(signal.SIGHUP)
                wait_for(lambda: "reload_failures_total 1" in metrics())
                assert not probe()
                passed("RGL syntax or capability rejection preserves live policy")
                policy.write_text("function on_xdp() return xdp.pass() end")
                agent.send_signal(signal.SIGHUP)
                wait_for(lambda: "config_generation 2" in metrics())
                assert probe()
                passed("RGL source reload compiles and updates real native traffic")
                agent.terminate()
                assert agent.wait(timeout=5) == 0
                history = temp / "history"
                config.write_text(json.dumps({"observe": True, "event_sample_every": 1, "sets": {"blocked": [{"cidr": "198.18.255.2/32"}]}}))
                policy.write_text(f'function on_xdp() if pkt.is_udp() and pkt.dst_port() == {port} and pkt.src_in_set("blocked") then return xdp.drop("blocked-source") end return xdp.pass("allowed") end')
                received = []

                class Receiver(http.server.BaseHTTPRequestHandler):
                    def log_message(self, *_):
                        pass

                    def do_POST(self):
                        received.append((self.path, self.headers["Content-Type"], self.rfile.read(int(self.headers["Content-Length"]))))
                        self.send_response(200)
                        self.send_header("Content-Type", "application/x-protobuf")
                        self.send_header("Content-Length", "0")
                        self.end_headers()

                sink = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Receiver)
                receiver_thread = threading.Thread(target=sink.serve_forever, daemon=True)
                receiver_thread.start()
                command += ["--config", str(config), "--history-dir", str(history), "--otlp-logs-endpoint", f"http://127.0.0.1:{sink.server_port}/v1/logs"]
                env = {k: v for k, v in os.environ.items() if not k.startswith("OTEL_")}
                env["OTEL_BLRP_SCHEDULE_DELAY"] = "50"
                agent = subprocess.Popen(command, stdout=logs, stderr=logs, env=env)

                def status():
                    with urllib.request.urlopen(f"http://127.0.0.1:{admin}/status", timeout=2) as response:
                        return json.load(response)

                wait_for(lambda: "config_generation 1" in metrics())
                assert probe()
                wait_for(lambda: bool(received))
                assert all(p == "/v1/logs" and t == "application/x-protobuf" for p, t, _ in received)
                payload = b"".join(data for _, _, data in received)
                assert all(field in payload for field in [b"rgnix.xdp.drop", b"blocked-source", b"would_drop", b"198.18.255.2"])
                assert b"http.request" not in payload
                passed("real observed packets emit bounded OTLP protobuf network events")
                first_revision = status()["policy_revision"]
                config.write_text(json.dumps({"sets": {"blocked": [{"cidr": "198.18.255.2/32"}]}}))
                wait_for(lambda: status()["generation"] == 2)
                assert not probe()
                passed("file watch atomically changes observation into enforcement without HUP")
                config.write_text('{"sets": {"blocked": [{"cidr": "invalid"}]}}')
                wait_for(lambda: status()["last_error"] is not None)
                assert not probe()
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{admin}/readyz", timeout=2)
                    raise AssertionError("invalid desired config reported ready")
                except urllib.error.HTTPError as error:
                    assert error.code == 503
                passed("failed automatic update preserves filtering and exposes unconverged readiness")
                run(binary, "xdp", "rollback", "--history-dir", history, "--revision", first_revision, "--config", config)
                wait_for(lambda: status()["last_error"] is None and status()["observe"])
                assert probe() and status()["policy_revision"] == first_revision
                passed("durable content-addressed rollback restores the exact policy revision")
                config.write_text(json.dumps({"sets": {"blocked": [{"cidr": "198.18.255.2/32", "expires_at": int(time.time()) + 5}]}}))
                wait_for(lambda: not status()["observe"])
                assert not probe()
                generation = status()["generation"]
                wait_for(probe)
                assert status()["generation"] == generation
                passed("address bans expire in kernel without recompilation or policy reload")
                config.write_text(json.dumps({"sets": {"blocked": [{"cidr": "198.18.255.0/30"}, {"cidr": "198.18.255.2/32", "expires_at": int(time.time()) + 3}]}}))
                wait_for(lambda: status()["generation"] > generation)
                assert not probe()
                time.sleep(3)
                assert not probe()
                passed("expired specific prefix cannot hide an active covering block")
                config.write_text('{}')
                policy.write_text(f'function on_xdp() if pkt.is_udp() and pkt.dst_port() == {port} and not xdp.allow_bytes("client", "src_ip", 1, 64) then return xdp.drop("client-budget") end return xdp.pass("allowed") end')
                wait_for(lambda: any(row["rule"] == "client-budget" for row in status()["rules"]))
                assert probe() and not probe()
                generation = status()["generation"]
                policy.write_text(policy.read_text().replace('xdp.pass("allowed")', 'xdp.pass("allowed-new")'))
                wait_for(lambda: status()["generation"] > generation)
                assert not probe()
                passed("stable named limiter retains its consumed budget across changed code")
                agent.terminate()
                assert agent.wait(timeout=5) == 0
                command += ["--persist"]
                agent = subprocess.Popen(command, stdout=logs, stderr=logs, env=env)
                wait_for(lambda: status()["persistent"] and status()["generation"] == 1)
                assert probe() and not probe()
                agent.kill()
                agent.wait(timeout=5)
                assert not probe()
                passed("persistent link continues filtering after SIGKILL")
                agent = subprocess.Popen(command, stdout=logs, stderr=logs, env=env)
                wait_for(lambda: status()["generation"] == 1)
                assert not probe()
                passed("restart takes over its owned link and retains the rate budget")
                info = json.loads(run(binary, "xdp", "doctor", "--interface", host).stdout)
                assert info["bpffs_mounted"] and info["interface"][0]["ifname"] == host
                assert json.loads(run(binary, "xdp", "status", "--admin", f"127.0.0.1:{admin}").stdout)["persistent"]
                passed("doctor and status report actual kernel and agent state")
                agent.terminate()
                assert agent.wait(timeout=5) == 0
                assert not probe()
                run(binary, "xdp", "detach", "--interface", host, "--mode", "native")
                assert probe()
                passed("explicit ownership-checked detach removes persistent enforcement")
                sink.shutdown()
                sink.server_close()
                receiver_thread.join(timeout=2)
                if args.libxdp:
                    peer_source = temp / "peer.c"
                    peer_object = temp / "peer.o"
                    peer_source.write_text('typedef unsigned int u32; struct xdp_md {u32 data, data_end, data_meta, ingress_ifindex, rx_queue_index, egress_ifindex;}; __attribute__((section("xdp"))) int rgnix_peer(struct xdp_md *c) { return 2; } char peer_license[] __attribute__((section("license"))) = "GPL";')
                    run("clang", "-target", "bpfel", "-O2", "-g", "-c", peer_source, "-o", peer_object)
                    policy.write_text(f'function on_xdp() if pkt.is_udp() and pkt.dst_port() == {port} then return xdp.drop("udp") end return xdp.pass() end')
                    run(binary, "xdp", "compile", policy, "--dispatcher", "-o", obj)
                    run("xdp-loader", "load", "--prio", "60", host, peer_object)
                    conflict = run(binary, "xdp", "run", policy, "--interface", host, "--mode", "generic", "--admin", "127.0.0.1:0", ok=False)
                    assert conflict.returncode != 0 and probe()
                    passed("exclusive agent refuses a foreign native dispatcher even in generic mode")
                    run("xdp-loader", "load", "--prio", "50", host, obj)
                    assert not probe()

                    def component_id(name):
                        output = run("xdp-loader", "status", host).stdout
                        return re.search(r"\b" + name + r"\s+(\d+)", output).group(1)

                    run("xdp-loader", "unload", host, "--id", component_id("rgnix_xdp"))
                    assert probe() and "rgnix_peer" in run("xdp-loader", "status", host).stdout
                    run("xdp-loader", "unload", host, "--id", component_id("rgnix_peer"))
                    passed("libxdp component enforces policy and removal preserves the peer")
                    peer_source.write_text('typedef unsigned int u32; struct xdp_md {u32 data, data_end, data_meta, ingress_ifindex, rx_queue_index, egress_ifindex;}; __attribute__((section("xdp"))) int rgnix_peer(struct xdp_md *c) { unsigned char *p=(void*)(unsigned long)c->data; void *end=(void*)(unsigned long)c->data_end; if ((void*)(p+14)>end) return 2; return p[12]==8 && p[13]==6 ? 2 : 1; } char peer_license[] __attribute__((section("license"))) = "GPL";')
                    run("clang", "-target", "bpfel", "-O2", "-g", "-c", peer_source, "-o", peer_object)
                    policy.write_text('function on_xdp() return xdp.pass("continue") end')
                    run(binary, "xdp", "compile", policy, "--dispatcher", "-o", obj)
                    run("xdp-loader", "load", "--prio", "60", host, peer_object)
                    run("xdp-loader", "load", "--prio", "50", host, obj)
                    assert not probe()
                    run("xdp-loader", "unload", host, "--id", component_id("rgnix_peer"))
                    assert probe()
                    run("xdp-loader", "unload", host, "--id", component_id("rgnix_xdp"))
                    passed("XDP_PASS continues to the next libxdp component instead of bypassing it")
                command.remove("--persist")
                agent = subprocess.Popen(command, stdout=logs, stderr=logs, env=env)
                wait_for(lambda: status()["generation"] == 1)
                run("ip", "link", "del", host)
                assert agent.wait(timeout=5) != 0
                passed("interface deletion stops the agent instead of reporting ready")
        finally:
            if agent and agent.poll() is None:
                agent.kill()
                agent.wait(timeout=5)
            if "sink" in locals():
                sink.shutdown()
                sink.server_close()
            if "command" in locals() and "--persist" in command:
                run(binary, "xdp", "detach", "--interface", host, "--mode", "native", ok=False)
            stopped.set()
            if "thread" in locals():
                thread.join(timeout=2)
            echo.close()
            if args.libxdp:
                run("xdp-loader", "unload", host, "--all", ok=False)
            run("ip", "link", "del", host, ok=False)
            run("ip", "netns", "del", namespace, ok=False)

    output = {"platform": os.uname().release, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "passed": len(results), "results": results}
    if args.output:
        args.output.write_text(json.dumps(output, indent=2) + "\n")
    print(json.dumps({"passed": len(results)}))


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Bounded, isolated veth baseline: real rgnix HTTP with optional ingress UDP load."""
import argparse
import hashlib
import json
import os
import pathlib
import socket
import statistics
import subprocess
import tempfile
import time
import urllib.request

from xdp_integration import frame, run
from benchmark import WRK_REPORT, affinity, wrk_load

CLIENT = r'''
import concurrent.futures, http.client, json, sys, time
address, port, duration = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])
end = time.monotonic() + duration
def worker(_):
    latencies, errors = [], 0
    connection = http.client.HTTPConnection(address, port, timeout=1)
    while time.monotonic() < end:
        started = time.perf_counter()
        try:
            connection.request("GET", "/", headers={"Host": "benchmark.test"})
            response = connection.getresponse()
            assert response.status == 200 and response.read() == b"ok"
            latencies.append((time.perf_counter() - started) * 1000)
        except Exception:
            errors += 1
            connection.close()
            connection = http.client.HTTPConnection(address, port, timeout=1)
    connection.close()
    return latencies, errors
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    results = list(pool.map(worker, range(4)))
values = sorted(v for batch, _ in results for v in batch)
print(json.dumps({"requests": len(values), "errors": sum(e for _, e in results),
    "http_rps": len(values) / duration,
    "p50_ms": values[int((len(values)-1)*.5)] if values else None,
    "p99_ms": values[int((len(values)-1)*.99)] if values else None}))
'''
FLOOD = r'''
import json, socket, sys, time
address, duration, target_pps = sys.argv[1], float(sys.argv[2]), int(sys.argv[3])
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
started = time.monotonic(); end = started + duration; count = 0
while time.monotonic() < end:
    sock.sendto(b"x" * 128, (address, 9)); count += 1
    if count % 64 == 0:
        ahead = started + count / target_pps - time.monotonic()
        if ahead > 0: time.sleep(ahead)
print(json.dumps({"offered_pps": count / (time.monotonic()-started)}))
'''


def free_port(address="127.0.0.1"):
    with socket.socket() as sock:
        sock.bind((address, 0))
        return sock.getsockname()[1]


def ready(port):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/readyz", timeout=1):
                return
        except OSError:
            time.sleep(.05)
    raise AssertionError("benchmark process did not become ready")


def counters(pid):
    fields = pathlib.Path(f"/proc/{pid}/stat").read_text().split()
    ticks = os.sysconf("SC_CLK_TCK")
    cpu = (int(fields[13]) + int(fields[14])) / ticks
    rss = int(fields[23]) * os.sysconf("SC_PAGE_SIZE")
    softirq = int(pathlib.Path("/proc/stat").read_text().splitlines()[0].split()[7]) / ticks
    net_rx = next(line for line in pathlib.Path("/proc/softirqs").read_text().splitlines() if line.strip().startswith("NET_RX:"))
    return cpu, rss, softirq, sum(int(value) for value in net_rx.split()[1:])


def packet_counters(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=2) as response:
        return {key: int(value) for line in response.read().decode().splitlines()
                if line.startswith("rgnix_xdp_packets_total{") for key, value in [line.rsplit(" ", 1)]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--seconds", type=float, default=3)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--flood-pps", type=int, default=100000)
    parser.add_argument("--client", choices=["python", "wrk"], default="python")
    parser.add_argument("--concurrency", type=int, default=64, help="wrk connections; Python uses four clients")
    parser.add_argument("--server-cpus")
    parser.add_argument("--client-cpus")
    parser.add_argument("--flood-cpus")
    args = parser.parse_args()
    if os.geteuid() != 0 or not 1 <= args.seconds <= 30 or not 1 <= args.rounds <= 10 or not 1000 <= args.flood_pps <= 2000000:
        raise SystemExit("root required; use 1..30 seconds and 1..10 rounds")
    if args.client == "wrk" and (args.seconds != int(args.seconds) or args.concurrency < 2):
        parser.error("wrk requires whole seconds and at least two connections")
    binary = args.binary.resolve()
    suffix = str(os.getpid())
    host, peer, namespace = "rgb" + suffix, "rgq" + suffix, "rgnix-bench-" + suffix
    server = agent = flood = None
    results, kernel = [], {}
    with tempfile.TemporaryDirectory(prefix="rgnix-xdp-bench-") as directory:
        root = pathlib.Path(directory)
        report = root / "report.lua"
        report.write_text(WRK_REPORT)
        with (root / "process.log").open("w+") as log:
            try:
                run("ip", "netns", "add", namespace)
                run("ip", "link", "add", host, "type", "veth", "peer", "name", peer)
                run("ip", "link", "set", peer, "netns", namespace)
                run("ip", "addr", "add", "198.18.254.1/30", "dev", host)
                run("ip", "link", "set", host, "up")
                run("ip", "netns", "exec", namespace, "ip", "addr", "add", "198.18.254.2/30", "dev", peer)
                run("ip", "netns", "exec", namespace, "ip", "link", "set", peer, "up")
                run("ip", "netns", "exec", namespace, "ip", "link", "set", "lo", "up")
                http_port, admin_port, xdp_port = free_port("198.18.254.1"), free_port(), free_port()
                conf = root / "nginx.conf"
                conf.write_text(f'events {{}} http {{ access_log off; server {{ listen 198.18.254.1:{http_port}; location / {{ return 200 "ok"; }} }} }}')
                server = subprocess.Popen([*affinity(args.server_cpus), str(binary), "serve", "-c", str(conf), "--admin", f"127.0.0.1:{admin_port}"], stdout=log, stderr=log)
                ready(admin_port)

                def measure(seconds):
                    if args.client == "wrk":
                        result = wrk_load(f"http://198.18.254.1:{http_port}/", report, int(seconds), args.concurrency,
                                          cpus=args.client_cpus, prefix=["ip", "netns", "exec", namespace])
                        result["http_rps"] = result.pop("rps")
                        result.pop("wrk_output")
                        return result
                    return json.loads(run("ip", "netns", "exec", namespace, *affinity(args.client_cpus), "python3", "-c",
                                          CLIENT, "198.18.254.1", http_port, seconds).stdout)

                policy = root / "edge.rgl"
                obj = root / "edge.o"
                packet = root / "frame.bin"
                packet.write_bytes(frame(protocol=17, port=9))
                for mode in ["pass", "filter"]:
                    policy.write_text('function on_xdp() ' + ('if pkt.is_udp() and pkt.dst_port() == 9 then return xdp.drop("udp-flood") end ' if mode == "filter" else '') + 'return xdp.pass("http") end')
                    run(binary, "xdp", "compile", policy, "-o", root / f"{mode}.o")
                    kernel[mode] = json.loads(run(binary, "xdp", "test", root / f"{mode}.o", "--packet", packet, "--repeat", 100000).stdout)
                for round_number in range(args.rounds):
                    # Alternate order to make warmup/thermal drift visible in the raw results.
                    modes = ["none", "pass", "filter"] if round_number % 2 == 0 else ["filter", "pass", "none"]
                    for mode in modes:
                        if mode != "none":
                            obj.write_bytes((root / f"{mode}.o").read_bytes())
                            agent = subprocess.Popen([str(binary), "xdp", "run", str(obj), "--interface", host, "--mode", "native", "--admin", f"127.0.0.1:{xdp_port}", "--watch-interval", "0"], stdout=log, stderr=log)
                            ready(xdp_port)
                        for attack in [False, True]:
                            measure(1)
                            xdp_before = packet_counters(xdp_port) if agent else {}
                            before = counters(server.pid)
                            if attack:
                                flood = subprocess.Popen(["ip", "netns", "exec", namespace, *affinity(args.flood_cpus), "python3", "-c", FLOOD, "198.18.254.1", str(args.seconds + 1), str(args.flood_pps)], stdout=subprocess.PIPE, text=True)
                            started = time.monotonic()
                            result = measure(args.seconds)
                            elapsed = time.monotonic() - started
                            after = counters(server.pid)
                            offered = json.loads(flood.communicate(timeout=5)[0])["offered_pps"] if flood else 0
                            flood = None
                            xdp_counters = {key: value - xdp_before.get(key, 0) for key, value in packet_counters(xdp_port).items()} if agent else {}
                            assert not result["errors"] and result["requests"], result
                            if agent and attack and mode == "filter":
                                assert xdp_counters['rgnix_xdp_packets_total{action="drop"}'] > 1000, xdp_counters
                            result.update(round=round_number + 1, mode=mode, udp_flood=attack, offered_pps=offered,
                                server_cpu_percent=100 * (after[0] - before[0]) / elapsed, server_rss_bytes=after[1],
                                server_cpu_us_per_request=1e6 * (after[0] - before[0]) / result["requests"],
                                system_softirq_seconds=after[2] - before[2], system_net_rx_softirqs=after[3] - before[3], xdp_counters=xdp_counters)
                            results.append(result)
                            print(json.dumps(result), flush=True)
                        if agent:
                            agent.terminate(); agent.wait(timeout=10); agent = None
                summary = []
                for mode in ["none", "pass", "filter"]:
                    for attack in [False, True]:
                        group = [r for r in results if r["mode"] == mode and r["udp_flood"] == attack]
                        summary.append({"mode": mode, "udp_flood": attack, **{field: statistics.median(r[field] for r in group) for field in ["http_rps", "p99_ms", "server_cpu_percent", "server_rss_bytes", "offered_pps", "system_softirq_seconds"]}, "errors": sum(r["errors"] for r in group)})
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps({"environment": f"OrbStack/Linux native veth; {args.client} HTTP client; not a hardware capacity result", "concurrency": args.concurrency if args.client == "wrk" else 4, "affinity": {"server": args.server_cpus, "client": args.client_cpus, "flood": args.flood_cpus}, "kernel": os.uname().release, "architecture": os.uname().machine, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "seconds": args.seconds, "rounds": args.rounds, "target_flood_pps": args.flood_pps, "kernel_microbenchmark": kernel, "summary": summary, "raw": results}, indent=2) + "\n")
            except Exception:
                log.flush(); log.seek(0); print(log.read()[-16000:]); raise
            finally:
                for process in [flood, agent, server]:
                    if process and process.poll() is None:
                        process.terminate()
                        try: process.wait(timeout=10)
                        except subprocess.TimeoutExpired: process.kill(); process.wait()
                run("ip", "link", "del", host, ok=False)
                run("ip", "netns", "del", namespace, ok=False)


if __name__ == "__main__":
    main()

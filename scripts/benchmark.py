#!/usr/bin/env python3
"""Small repeatable loopback baseline, not a maximum-capacity test."""
import concurrent.futures
import argparse
import http.client
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from integration import free_port, request, wait_for


def process_stats(pid):
    stat = Path(f"/proc/{pid}/stat").read_text().split()
    ticks = int(stat[13]) + int(stat[14])
    status = Path(f"/proc/{pid}/status").read_text().splitlines()
    rss = int(next(line.split()[1] for line in status if line.startswith("VmRSS:")))
    return ticks / os.sysconf("SC_CLK_TCK"), rss


def load(port, path, seconds=3, concurrency=8):
    deadline = time.perf_counter() + seconds
    def worker():
        latencies = []
        errors = 0
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        while time.perf_counter() < deadline:
            started = time.perf_counter()
            try:
                connection.request("GET", path, headers={"Host": "localhost", "x-route": "test"})
                response = connection.getresponse()
                response.read()
                errors += response.status != 200
            except (OSError, http.client.HTTPException):
                errors += 1
                connection.close()
            latencies.append((time.perf_counter() - started) * 1000)
        connection.close()
        return latencies, errors
    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        results = list(pool.map(lambda _: worker(), range(concurrency)))
    elapsed = time.perf_counter() - started
    latency = sorted(value for samples, _ in results for value in samples)
    return {"requests": len(latency), "rps": round(len(latency) / elapsed, 1),
            "errors": sum(errors for _, errors in results), "seconds": round(elapsed, 3),
            **{f"p{p}_ms": round(latency[min(len(latency) - 1, int(len(latency) * p / 100))], 3) for p in [50, 95, 99]}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument("--proxy", action="store_true", help="proxy 16 KiB files from a real local NGINX")
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--seconds", type=float, default=3)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--routes", type=int, default=0, help="additional exact routes")
    args = parser.parse_args()
    if args.concurrency < 1 or args.seconds <= 0 or args.rounds < 1 or args.routes < 0:
        parser.error("concurrency, seconds and rounds must be positive; routes must be nonnegative")
    binary = str(Path(args.binary).resolve())
    output = {"platform": platform.platform(), "binary": binary, "concurrency": args.concurrency, "rounds": args.rounds, "additional_routes": args.routes,
              "workload": ("16 KiB NGINX upstream responses" if args.proxy else "2-byte local responses") + "; Python HTTP/1.1 keepalive client; shared OrbStack host", "cases": {}}
    with tempfile.TemporaryDirectory(prefix="rgnix-benchmark-") as temp:
        directory = Path(temp)
        port, admin = free_port(), free_port()
        origin = None
        action = "return 200 ok;"
        if args.proxy:
            origin_port = free_port()
            (directory / "payload.bin").write_bytes(b"x" * 16384)
            paths = "\n".join(f"{kind}_temp_path {directory}/{kind};" for kind in ["client_body", "proxy", "fastcgi", "uwsgi", "scgi"])
            (directory / "origin.conf").write_text(f"events {{}} http {{ {paths} access_log off; server {{ listen 127.0.0.1:{origin_port}; root {directory}; }} }}")
            with (directory / "origin.log").open("w") as log:
                origin = subprocess.Popen([os.environ.get("NGINX", "/usr/sbin/nginx"), "-p", str(directory), "-c", str(directory / "origin.conf"),
                    "-e", "stderr", "-g", f"daemon off; master_process off; pid {directory}/origin.pid;"], stdout=log, stderr=log)
            action = f"proxy_pass http://127.0.0.1:{origin_port}/payload.bin;"
        (directory / "noop.rgl").write_text("function on_request() return route.pass() end")
        (directory / "branch.rgl").write_text('function on_request() if req.header("x-route") == "test" then req.set_header("x-value", "yes") end return route.pass() end')
        generated = "\n".join(f"location = /generated/{i} {{ {action} }}" for i in range(args.routes))
        (directory / "nginx.conf").write_text(f'''events {{}} http {{ access_log off; server {{ listen 127.0.0.1:{port};
location /plain {{ {action} }}
location /noop {{ rgnix_script {directory}/noop.rgl; {action} }}
location /branch {{ rgnix_script {directory}/branch.rgl; {action} }}
{generated}
}} }}''')
        with (directory / "server.log").open("w") as log:
            process = subprocess.Popen([binary, "serve", "-c", str(directory / "nginx.conf"), "--admin", f"127.0.0.1:{admin}",
                "--max-inflight", str(max(1024, args.concurrency)), "--max-plugin-instances", str(args.concurrency)], stdout=log, stderr=log)
            try:
                wait_for(lambda: request(admin, "/readyz")[0], 200)
                if args.proxy:
                    wait_for(lambda: request(origin_port, "/payload.bin")[0], 200)
                    assert len(request(port, "/plain")[2]) == 16384
                if args.routes:
                    assert request(port, f"/generated/{args.routes - 1}")[0] == 200
                for case in ["plain", "noop", "branch"]:
                    load(port, "/" + case, seconds=1, concurrency=args.concurrency)
                    rounds = []
                    for _ in range(args.rounds):
                        cpu_before, rss_before = process_stats(process.pid)
                        result = load(port, "/" + case, seconds=args.seconds, concurrency=args.concurrency)
                        cpu_after, rss_after = process_stats(process.pid)
                        result["server_cpu_cores"] = round((cpu_after - cpu_before) / result["seconds"], 3)
                        result["rss_kib"] = max(rss_before, rss_after)
                        if result["errors"]:
                            raise AssertionError(result)
                        rounds.append(result)
                    output["cases"][case] = {"runs": rounds, "median_rps": statistics.median(r["rps"] for r in rounds),
                                             "median_p95_ms": statistics.median(r["p95_ms"] for r in rounds)}
                    print(case, output["cases"][case], flush=True)
            finally:
                process.terminate()
                process.wait(timeout=35)
                if origin:
                    origin.terminate()
                    origin.wait(timeout=10)
    destination = Path(".local/benchmark-proxy.json" if args.proxy else ".local/benchmark.json")
    destination.parent.mkdir(exist_ok=True)
    destination.write_text(json.dumps(output, indent=2) + "\n")


if __name__ == "__main__":
    main()

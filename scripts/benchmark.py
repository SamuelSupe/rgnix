#!/usr/bin/env python3
"""Repeatable Linux HTTP baseline; use wrk and disjoint CPU sets for comparisons."""
import concurrent.futures
import argparse
import http.client
import hashlib
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


WRK_REPORT = '''
function done(summary, latency, requests)
  local e = summary.errors
  io.write(string.format('RGNIX_RESULT {"requests":%d,"seconds":%.6f,"bytes":%d,"errors":%d,"p50_ms":%.6f,"p95_ms":%.6f,"p99_ms":%.6f}\\n',
    summary.requests, summary.duration / 1000000, summary.bytes,
    e.connect + e.read + e.write + e.status + e.timeout,
    latency:percentile(50) / 1000, latency:percentile(95) / 1000, latency:percentile(99) / 1000))
end
'''


def affinity(cpus):
    return ["taskset", "-c", cpus] if cpus else []


def wrk_load(url, report, seconds, concurrency, threads=2, cpus=None, prefix=()):
    command = [*prefix, *affinity(cpus), "wrk", "-t", str(threads), "-c", str(concurrency),
               "-d", f"{seconds}s", "--timeout", "5s", "--latency", "-s", str(report),
               "-H", "Host: localhost", "-H", "x-route: test", url]
    result = subprocess.run(command, capture_output=True, text=True, timeout=seconds + 15, check=True)
    line = next(line for line in result.stdout.splitlines() if line.startswith("RGNIX_RESULT "))
    sample = json.loads(line.removeprefix("RGNIX_RESULT "))
    sample["rps"] = sample["requests"] / sample["seconds"]
    sample["wrk_output"] = result.stdout
    return sample


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
    parser.add_argument("--client", choices=["python", "wrk"], default="python")
    parser.add_argument("--compare", type=Path, help="second binary; reverse binary/case order in alternate rounds")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--client-threads", type=int, default=2)
    parser.add_argument("--cases", choices=["plain", "noop", "branch"], nargs="+", default=["plain", "noop", "branch"])
    parser.add_argument("--server-cpus", help="taskset CPU list for the server")
    parser.add_argument("--client-cpus", help="taskset CPU list for wrk")
    parser.add_argument("--origin-cpus", help="taskset CPU list for NGINX")
    args = parser.parse_args()
    if args.concurrency < 1 or args.seconds <= 0 or args.rounds < 1 or args.routes < 0:
        parser.error("concurrency, seconds and rounds must be positive; routes must be nonnegative")
    if not 1 <= args.workers <= 256:
        parser.error("workers must be 1..256")
    if args.client == "wrk" and not 1 <= args.client_threads <= args.concurrency:
        parser.error("client-threads must be 1..concurrency")
    if args.client == "wrk" and args.seconds != int(args.seconds):
        parser.error("wrk duration must be a whole number of seconds")
    if args.client == "python" and args.client_cpus:
        parser.error("--client-cpus requires --client wrk")
    binaries = {"baseline": str(Path(args.binary).resolve())}
    if args.compare:
        binaries["candidate"] = str(args.compare.resolve())
    output = {"platform": platform.platform(), "concurrency": args.concurrency, "rounds": args.rounds,
              "workers": args.workers, "client_threads": args.client_threads, "additional_routes": args.routes,
              "affinity": {"server": args.server_cpus, "client": args.client_cpus, "origin": args.origin_cpus},
              "workload": ("16 KiB NGINX upstream responses" if args.proxy else "2-byte local responses") + f"; {args.client} HTTP/1.1 keepalive client",
              "binaries": {name: {"path": binary, "sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(), "cases": {}}
                           for name, binary in binaries.items()}}
    with tempfile.TemporaryDirectory(prefix="rgnix-benchmark-") as temp:
        directory = Path(temp)
        port, admin = free_port(), free_port()
        report = directory / "report.lua"
        report.write_text(WRK_REPORT)

        def measure(case, seconds):
            if args.client == "wrk":
                return wrk_load(f"http://127.0.0.1:{port}/{case}", report, int(seconds), args.concurrency,
                                args.client_threads, args.client_cpus)
            return load(port, "/" + case, seconds=seconds, concurrency=args.concurrency)

        origin = None
        action = "return 200 ok;"
        if args.proxy:
            origin_port = free_port()
            (directory / "payload.bin").write_bytes(b"x" * 16384)
            paths = "\n".join(f"{kind}_temp_path {directory}/{kind};" for kind in ["client_body", "proxy", "fastcgi", "uwsgi", "scgi"])
            (directory / "origin.conf").write_text(f"events {{}} http {{ {paths} access_log off; server {{ listen 127.0.0.1:{origin_port}; root {directory}; }} }}")
            with (directory / "origin.log").open("w") as log:
                origin = subprocess.Popen([*affinity(args.origin_cpus), os.environ.get("NGINX", "/usr/sbin/nginx"), "-p", str(directory), "-c", str(directory / "origin.conf"),
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
        try:
            if origin:
                wait_for(lambda: request(origin_port, "/payload.bin")[0], 200)
            for round_index in range(args.rounds):
                variants = list(binaries.items())
                cases = list(args.cases)
                if round_index % 2:
                    variants.reverse()
                    cases.reverse()
                for name, binary in variants:
                    with (directory / f"{name}.log").open("w") as log:
                        process = subprocess.Popen([*affinity(args.server_cpus), binary, "serve", "-c", str(directory / "nginx.conf"),
                            "--admin", f"127.0.0.1:{admin}", "--threads", str(args.workers),
                            "--max-inflight", str(max(1024, args.concurrency)), "--max-plugin-instances", str(args.concurrency)], stdout=log, stderr=log)
                    try:
                        wait_for(lambda: request(admin, "/readyz")[0], 200)
                        for case in cases:
                            response = request(port, "/" + case)
                            assert response[0] == 200 and response[2] == (b"x" * 16384 if args.proxy else b"ok")
                        if args.routes:
                            assert request(port, f"/generated/{args.routes - 1}")[0] == 200
                        for case in cases:
                            measure(case, 1)
                            cpu_before, rss_before = process_stats(process.pid)
                            result = measure(case, args.seconds)
                            cpu_after, rss_after = process_stats(process.pid)
                            result["server_cpu_cores"] = (cpu_after - cpu_before) / result["seconds"]
                            result["server_cpu_us_per_request"] = (cpu_after - cpu_before) * 1e6 / max(1, result["requests"])
                            result["rss_kib"] = max(rss_before, rss_after)
                            result["round"] = round_index + 1
                            if result["errors"] or not result["requests"]:
                                raise AssertionError(result)
                            output["binaries"][name]["cases"].setdefault(case, {"runs": []})["runs"].append(result)
                            print(name, case, {k: v for k, v in result.items() if k != "wrk_output"}, flush=True)
                    finally:
                        process.terminate()
                        process.wait(timeout=35)
        finally:
            if origin:
                origin.terminate()
                origin.wait(timeout=10)
    for variant in output["binaries"].values():
        for case in variant["cases"].values():
            for field in ["rps", "p95_ms", "p99_ms", "server_cpu_us_per_request", "rss_kib"]:
                case["median_" + field] = statistics.median(run[field] for run in case["runs"])
    destination = args.output or Path(".local/benchmark-proxy.json" if args.proxy else ".local/benchmark.json")
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(output, indent=2) + "\n")


if __name__ == "__main__":
    main()

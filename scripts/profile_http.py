#!/usr/bin/env python3
"""Instrument the plain HTTP/1 fixture; these results are not throughput benchmarks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import time

from benchmark_compare import measure, preflight, start, stop, tree_stats
from integration import request, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--matrix", type=Path, required=True, help="completed --plain-proxy benchmark JSON")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--prototype", type=Path)
    parser.add_argument("--engines", nargs="+", choices=["pingora", "hyper", "prototype", "nginx"], default=["pingora", "hyper", "nginx"])
    parser.add_argument("--mode", choices=["profile", "syscalls"], default="profile")
    parser.add_argument("--seconds", type=int, default=15)
    parser.add_argument("--work-dir", type=Path, required=True)
    args = parser.parse_args()
    matrix = json.loads(args.matrix.read_text())
    if not matrix["settings"].get("plain_proxy") or matrix["settings"]["workers"] != 1:
        parser.error("requires a plain-proxy, single-worker fixture")
    if "prototype" in args.engines and not args.prototype:
        parser.error("--engines prototype requires --prototype")
    if args.seconds < 1:
        parser.error("--seconds must be positive")
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    previous = Path(matrix["settings"]["work_dir"])
    fixture = work / "fixture"
    shutil.copytree(previous, fixture, ignore=shutil.ignore_patterns("*.log", "*.pid"))
    for path in fixture.rglob("*"):
        if path.suffix in [".conf", ".rgl", ".lua"]:
            path.write_text(path.read_text().replace(str(previous), str(fixture)))
    ports = matrix["ports"]
    binaries = {"pingora": str(args.binary.resolve()), "hyper": str(args.binary.resolve()),
                "nginx": matrix["binaries"]["nginx"]["path"], "origin": matrix["binaries"]["openresty"]["path"]}
    if args.prototype:
        binaries["prototype"] = str(args.prototype.resolve())
    args.wrk = matrix["settings"]["wrk"]
    args.client_threads = matrix["settings"]["client_threads"]
    args.client_cpus = matrix["settings"]["client_cpus"]
    args.connection_close = False
    events_available = set(re.findall(r"syscalls:sys_enter_\w+", subprocess.check_output(["perf", "list", "tracepoint"], text=True)))
    syscalls = ["read", "write", "readv", "writev", "recvfrom", "sendto", "recvmsg", "sendmsg", "futex", "epoll_ctl", "epoll_pwait", "epoll_pwait2", "connect", "close", "mmap", "munmap"]
    events = ["task-clock", "context-switches", "cpu-migrations"] + ["syscalls:sys_enter_" + name for name in syscalls if "syscalls:sys_enter_" + name in events_available]
    result = {"mode": args.mode, "fixture_settings": matrix["settings"],
              "profile_settings": {"concurrency": 64, "warmup_seconds": 3, "seconds": args.seconds, "engines": args.engines},
              "network_namespace": os.readlink("/proc/self/ns/net"),
              "method": "Single-worker CPU-pinned c64 plain HTTP/1 proxy; 3s warmup; cpu-clock 199 Hz DWARF stacks or separate syscall counters. Instrumented counts and stack samples, not throughput evidence. Stack scopes overlap; flat samples do not identify every inlined caller.",
              "binaries": {name: {"path": path, "sha256": hashlib.sha256(Path(path).read_bytes()).hexdigest()} for name, path in binaries.items()}, "records": []}
    origin = server = instrument = None
    output = work / "result.json"
    try:
        origin = start(["taskset", "-c", matrix["settings"]["origin_cpus"], binaries["origin"], "-p", str(fixture / "origin"), "-c", str(fixture / "origin/nginx.conf"), "-g", "daemon off;"], work / "origin.log")
        wait_for(lambda: request(ports["primary"], "/proxy-1k")[0], 200)
        for engine in args.engines:
            product = engine in ["pingora", "hyper"]
            if product:
                command = [binaries[engine], "serve", "-c", str(fixture / "rgnix/nginx.conf"), "--admin", f"127.0.0.1:{ports['admin']}", "--threads", "1", "--max-inflight", "4096", "--max-plugin-instances", "512", "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "5"]
                if engine == "hyper":
                    command.append("--experimental-hyper")
            elif engine == "prototype":
                command = [binaries[engine], "--listen", f"127.0.0.1:{ports['http']}", "--upstream", f"127.0.0.1:{ports['primary']}", "--workers", "1"]
            else:
                command = [binaries[engine], "-p", str(fixture / "nginx"), "-c", str(fixture / "nginx/nginx.conf"), "-g", "daemon off;"]
            command = ["taskset", "-c", matrix["settings"]["server_cpus"], *command]
            server = start(command, work / f"{engine}.log")
            wait_for(lambda: request(ports["admin"] if product else ports["http"], "/readyz" if product else "/proxy-1k")[0], 200)
            checked = preflight("rgnix" if product else "hyper" if engine == "prototype" else engine, ports, plain_proxy=True)
            url = f"http://127.0.0.1:{ports['http']}/proxy-1k"
            script = fixture / "wrk-proxy-1k.lua"
            warmup = measure(args, url, script, 64, 3, server, origin)
            assert warmup["requests"] and not warmup["errors"]
            pids = ",".join(map(str, tree_stats(server.pid)["pids"]))
            data = work / f"{engine}.data"
            stat = work / f"{engine}.csv"
            perf = (["perf", "record", "-e", "cpu-clock", "-F", "199", "-g", "--call-graph", "dwarf,16384", "-p", pids, "-o", str(data)] if args.mode == "profile" else
                    ["perf", "stat", "--no-big-num", "-x", ";", "-e", ",".join(events), "-p", pids, "-o", str(stat)])
            instrument = subprocess.Popen(perf, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
            time.sleep(0.3)
            if instrument.poll() is not None:
                raise RuntimeError(instrument.communicate()[1])
            run = measure(args, url, script, 64, args.seconds, server, origin)
            instrument.send_signal(signal.SIGINT)
            _, diagnostic = instrument.communicate(timeout=15)
            assert instrument.returncode in [0, -signal.SIGINT], diagnostic
            instrument = None
            assert run["requests"] and not run["errors"], run
            stop(server); server = None
            item = {"engine": engine, "server_command": command, "instrument_command": perf, "instrument_output": diagnostic, "preflight": checked, "run": run}
            if args.mode == "profile":
                for options, suffix in [(["--no-children", "-g", "none"], "flat"), (["--children", "-g", "graph,1,caller"], "callgraph")]:
                    report = subprocess.check_output(["perf", "report", "--stdio", "--percent-limit", "0.25", "-i", str(data), *options], text=True, stderr=subprocess.DEVNULL)
                    (work / f"{engine}-{suffix}.txt").write_text(report)
                item["flat_report"] = (work / f"{engine}-flat.txt").read_text()
            else:
                item["raw_stat"] = stat.read_text()
                counts = {}
                for line in item["raw_stat"].splitlines():
                    fields = line.split(";")
                    if len(fields) > 2:
                        try:
                            counts[fields[2]] = float(fields[0])
                        except ValueError:
                            pass
                item["counts"] = counts
                item["per_request"] = {k: v / run["requests"] for k, v in counts.items()}
            result["records"].append(item)
            output.write_text(json.dumps(result, indent=2) + "\n")
            print(engine, args.mode, "done; requests", run["requests"], "errors", run["errors"], flush=True)
    finally:
        if instrument and instrument.poll() is None:
            instrument.send_signal(signal.SIGINT)
            instrument.communicate(timeout=15)
        stop(server); stop(origin)
        output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()

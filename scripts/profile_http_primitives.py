#!/usr/bin/env python3
"""Count libc copies and clocks for warmed HTTP/1 requests on Linux AArch64."""
import argparse
import bisect
import collections
import ctypes
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time

from benchmark_compare import start, stop
from integration import request, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, required=True, help="profile_http.py result with retained fixture paths")
    parser.add_argument("--matrix", type=Path, required=True, help="benchmark JSON used by profile_http.py")
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--engines", nargs="+", default=["hyper", "prototype", "pingora"])
    parser.add_argument("--binary", type=Path, help="replace the product binary for a candidate run")
    args = parser.parse_args()
    if os.uname().machine != "aarch64":
        parser.error("probe return-address registers currently require Linux AArch64")
    profile = json.loads(args.profile.read_text())
    matrix = json.loads(args.matrix.read_text())
    selected = {r["engine"]: r for r in profile["records"] if r["engine"] in args.engines}
    if set(selected) != set(args.engines) or "nginx" in selected:
        parser.error("engines must identify non-forking Rust processes in the profile")
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    fixture = args.profile.resolve().parent / "fixture"
    ports = matrix["ports"]
    libc = "/lib/aarch64-linux-gnu/libc.so.6"
    library = ctypes.CDLL(libc)
    base = min(int(line.split("-")[0], 16) - int(line.split()[2], 16)
               for line in Path("/proc/self/maps").read_text().splitlines() if line.endswith("/libc.so.6"))
    group = f"rgnix_http_{os.getpid()}"
    events = []
    origin = server = recorder = None
    result = {"method": "Resolved libc memcpy/memmove and clock_gettime uprobes. One connection, 10 warm and 500 measured requests. Counts include background operations and exclude inlined copies; not throughput or latency. AArch64 return addresses are mapped to ELF symbols.", "records": []}
    try:
        for kind in ["memcpy", "memmove", "clock"]:
            if kind == "clock":
                location = "clock_gettime caller=%x30:x64"
            else:
                offset = ctypes.cast(getattr(library, kind), ctypes.c_void_p).value - base
                location = f"{offset:#x} bytes=%x2:u64 caller=%x30:x64"
            subprocess.run(["perf", "probe", "-x", libc, "-a", f"{group}:{kind}={location}"], check=True)
            events.append(f"{group}:{kind}")
        origin = start(["taskset", "-c", matrix["settings"]["origin_cpus"], profile["binaries"]["origin"]["path"], "-p", str(fixture / "origin"), "-c", str(fixture / "origin/nginx.conf"), "-g", "daemon off;"], work / "origin.log")
        wait_for(lambda: request(ports["primary"], "/proxy-1k")[0], 200)
        for engine, previous in selected.items():
            binary = profile["binaries"][engine]["path"]
            command = previous["server_command"][:]
            if args.binary and engine in ["hyper", "pingora"]:
                replacement = str(args.binary.resolve())
                command = [replacement if value == binary else value for value in command]
                binary = replacement
            server = start(command, work / f"{engine}.log")
            wait_for(lambda: request(ports["http"], "/proxy-1k")[0], 200)
            connection = http.client.HTTPConnection("127.0.0.1", ports["http"], timeout=15)
            def send():
                connection.request("GET", "/proxy-1k", headers={"Host": "localhost"})
                response = connection.getresponse()
                assert response.status == 200 and response.read() == b"x" * 1024
            for _ in range(10):
                send()
            maps = Path(f"/proc/{server.pid}/maps").read_text()
            offset = min(int(line.split("-")[0], 16) - int(line.split()[2], 16)
                         for line in maps.splitlines() if line.endswith(binary))
            data = work / f"{engine}.data"
            recorder = subprocess.Popen(["perf", "record", "-e", ",".join(events), "-p", str(server.pid), "-o", str(data)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
            time.sleep(0.2)
            if recorder.poll() is not None:
                raise RuntimeError(recorder.communicate()[1])
            for _ in range(500):
                send()
            recorder.send_signal(signal.SIGINT)
            _, diagnostic = recorder.communicate(timeout=15)
            assert recorder.returncode in [0, -signal.SIGINT], diagnostic
            recorder = None
            connection.close()
            stop(server); server = None
            symbols = []
            for line in subprocess.check_output(["nm", "-nC", binary], text=True).splitlines():
                match = re.match(r"^([0-9a-f]+) [tTwW] (.+)$", line)
                if match:
                    symbols.append((int(match[1], 16), match[2]))
            addresses = [address for address, _ in symbols]
            counts, sizes, copies, clocks = (collections.Counter() for _ in range(4))
            decoded = subprocess.check_output(["perf", "script", "-i", str(data), "-F", "comm,pid,tid,time,event,trace"], text=True, stderr=subprocess.DEVNULL)
            for line in decoded.splitlines():
                event = re.search(group + r":(\w+):", line)
                if not event:
                    continue
                kind = event[1]
                counts[kind] += 1
                values = dict(re.findall(r"(bytes|caller)=(0x[0-9a-f]+|[0-9]+)", line))
                address = int(values["caller"], 0) - offset
                position = bisect.bisect_right(addresses, address) - 1
                symbol = symbols[position][1] if position >= 0 and 0 <= address - symbols[position][0] < 1_000_000 else "outside-main-binary"
                if kind == "clock":
                    clocks[symbol] += 1
                else:
                    size = int(values["bytes"], 0)
                    sizes[size] += 1
                    copies[symbol] += size
            result["records"].append({"engine": engine, "binary": binary, "sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(), "requests": 500, "events": dict(counts), "copy_bytes": sum(size * count for size, count in sizes.items()), "sizes": dict(sizes), "copy_bytes_by_caller": dict(copies.most_common()), "clock_calls_by_caller": dict(clocks.most_common()), "record_output": diagnostic, "server_command": command})
            print(engine, dict(counts), "copy bytes/request", sum(size * count for size, count in sizes.items()) / 500, flush=True)
    finally:
        if recorder and recorder.poll() is None:
            recorder.send_signal(signal.SIGINT)
            recorder.communicate(timeout=15)
        stop(server); stop(origin)
        for event in reversed(events):
            subprocess.run(["perf", "probe", "-d", event], check=True)
        (work / "result.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()

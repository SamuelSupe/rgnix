"""Allocation-only diagnostic using a completed backend_bench.py fixture."""
import argparse
import concurrent.futures
import hashlib
import http.client
import json
import os
import subprocess
import sys
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--fixture", type=Path, required=True)
parser.add_argument("--requests", type=int, default=2000)
args = parser.parse_args()
assert args.requests > 0 and args.requests % 4 == 0
matrix = json.loads(args.fixture.read_text())
work = Path(matrix["settings"]["work_dir"])
sys.path.insert(0, str(Path(matrix["settings"]["source"]) / "scripts"))
from benchmark_compare import start, stop
from integration import request, wait_for

ports = matrix["ports"]
origin = server = None
result = {"method": f"Heaptrack raw allocation events (+). Four persistent connections with {args.requests} requests, subtracting a separate same-case/config zero-request process. Instrumented data, not throughput or retained-memory measurements.",
          "binaries": matrix["binaries"], "runs": [], "per_request": {}}
try:
    origin = start([matrix["settings"]["nginx"], "-p", str(work), "-c", str(work / "origin.conf"), "-g", "daemon off;"], work / "allocation-origin.log")
    wait_for(lambda: request(ports["origin"], "/")[0], 200)
    for case in ["rr-1", "hash-8", "hash-64"]:
        for engine, binary in matrix["binaries"].items():
            assert hashlib.sha256(Path(binary["path"]).read_bytes()).hexdigest() == binary["sha256"]
            counts = []
            for count in [0, args.requests]:
                raw = work / f"alloc-{case}-{engine}-{count}"
                env = {**os.environ, "RUST_LOG": "warn", "LD_PRELOAD": "/usr/lib/heaptrack/libheaptrack_preload.so", "DUMP_HEAPTRACK_OUTPUT": str(raw)}
                for name in list(env):
                    if name.startswith("RGNIX_OTLP_"):
                        del env[name]
                with raw.with_suffix(".log").open("w") as log:
                    server = subprocess.Popen([binary["path"], "serve", "-c", str(work / f"{case}.conf"), "--admin", f"127.0.0.1:{ports['admin']}", "--threads", "1", "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "5"], stdout=log, stderr=log, env=env, start_new_session=True)
                wait_for(lambda: request(ports["admin"], "/readyz")[0], 200)
                def send(worker):
                    connection = http.client.HTTPConnection("127.0.0.1", ports["http"], timeout=10)
                    try:
                        for i in range(count // 4):
                            connection.request("GET", "/", headers={"Host": "localhost", "x-session": f"{(i+worker)%64:032x}"})
                            response = connection.getresponse()
                            assert response.status == 200 and response.read() == b"x" * 1024
                    finally:
                        connection.close()
                with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                    list(pool.map(send, range(4)))
                stop(server)
                server = None
                with raw.open() as stream:
                    allocations = sum(line.startswith("+ ") for line in stream)
                counts.append(allocations)
                result["runs"].append({"case": case, "engine": engine, "requests": count, "allocations": allocations})
                print(case, engine, count, allocations, flush=True)
            result["per_request"].setdefault(case, {})[engine] = (counts[1]-counts[0]) / args.requests
finally:
    stop(server)
    stop(origin)
    (work / "allocations.json").write_text(json.dumps(result, indent=2) + "\n")

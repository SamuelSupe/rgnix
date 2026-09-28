"""Round-six endpoint-selection comparison; run outside builds and other tests."""
import argparse
import hashlib
import json
import math
import platform
import sys
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--source", type=Path, required=True)
parser.add_argument("--baseline", type=Path, required=True)
parser.add_argument("--candidate", type=Path, required=True)
parser.add_argument("--nginx", type=Path, required=True)
parser.add_argument("--work-dir", type=Path, required=True)
parser.add_argument("--rounds", type=int, default=3)
parser.add_argument("--seconds", type=int, default=8)
parser.add_argument("--warmup", type=int, default=3)
parser.add_argument("--workers", type=int, default=1)
parser.add_argument("--aa", action="store_true")
parser.add_argument("--cases", nargs="+", default=["rr-1", "rr-64", "hash-64", "hash-256"])
args = parser.parse_args()
sys.path.insert(0, str(args.source / "scripts"))
from benchmark_compare import REPORT_LUA, measure, save_results, start, stop
from integration import free_port, request, wait_for

work = args.work_dir.resolve()
work.mkdir(parents=True, exist_ok=False)
args.wrk = "wrk"
args.client_threads = 2
args.client_cpus = "2,3"
args.connection_close = False
ports = {name: free_port() for name in ["origin", "http", "admin"]}
assert len(set(ports.values())) == len(ports)
addresses = [f"127.0.{i // 250}.{i % 250 + 1}" for i in range(256)]
keys = [f"{i:032x}" for i in range(64)]
payload = "x" * 1024
origin_conf = work / "origin.conf"
origin_conf.write_text(f'''worker_processes 2;
pid {work}/origin.pid;
error_log {work}/origin-error.log error;
events {{ worker_connections 4096; }}
http {{ access_log off; keepalive_timeout 60s; keepalive_requests 1000000;
server {{
''' + "\n".join(f"listen {ip}:{ports['origin']};" for ip in addresses) + f'''
location / {{ add_header X-Origin $server_addr; return 200 "{payload}"; }}
}} }}
''')
configs = {}
for case in args.cases:
    kind, count = case.split("-")
    count = int(count)
    assert kind in ["rr", "hash", "sticky", "long"] and 1 <= count <= 256
    selected_keys = [("x" * 4096 + key) if kind == "long" else key for key in keys]
    directive = "" if kind == "rr" else "rgnix_balance sticky session;" if kind == "sticky" else "rgnix_balance hash header:x-session;"
    config = work / f"{case}.conf"
    config.write_text(f'''events {{}} http {{ access_log off; keepalive_timeout 60s;
upstream pool {{ {directive}
''' + "\n".join(f"server {ip}:{ports['origin']} weight={1+i%5};" for i, ip in enumerate(addresses[:count])) + f'''
}}
server {{ listen 127.0.0.1:{ports['http']}; server_name localhost;
location / {{ proxy_pass http://pool; }} }} }}
''')
    script = work / f"{case}.lua"
    requests = []
    for i, key in enumerate(selected_keys, 1):
        header = f'Cookie="session={key}"' if kind == "sticky" else f'["x-session"]="{key}"'
        requests.append(f'requests[{i}] = wrk.format("GET", "/", {{Host="localhost", {header}}})')
    script.write_text("local requests = {}\nlocal counter = 0\n" + "\n".join(requests) + '\nfunction request()\n counter = counter % 64 + 1\n return requests[counter]\nend\n' + REPORT_LUA)
    configs[case] = (config, script, selected_keys)

binaries = {"before": args.baseline, "after": args.candidate}
if args.aa:
    binaries = {"aa-a": args.candidate, "aa-b": args.candidate}
output = {
    "platform": platform.platform(), "settings": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
    "binaries": {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()} for name, path in binaries.items()},
    "ports": ports, "server_cpus": "10" if args.workers == 1 else "10,11", "origin_cpus": "6,7",
    "qualification": "Distinct loopback endpoint addresses share one NGINX origin process, not separate Kubernetes Pods. No frontend NGINX comparison. No profiler or builds during measured windows.",
    "fixtures": {p.name: p.read_text() for p in work.iterdir() if p.suffix in [".conf", ".lua"]},
    "preflight": [], "runs": [], "origin_baselines": [],
}
result_path = work / "results.json"
origin = frontend = None
try:
    origin = start(["taskset", "-c", "6,7", str(args.nginx), "-p", str(work), "-c", str(origin_conf), "-g", "daemon off;"], work / "origin.log")
    wait_for(lambda: request(ports["origin"], "/")[0], 200)
    for case in args.cases:
        script = configs[case][1]
        url = f"http://127.0.0.1:{ports['origin']}/"
        measure(args, url, script, 64, args.warmup, origin, origin)
        run = measure(args, url, script, 64, args.seconds, origin, origin)
        run.update(case=case)
        output["origin_baselines"].append(run)
        assert run["requests"] and not run["errors"], run
        print(f"origin {case}: {run['rps']:.0f} req/s", flush=True)
    for round_index in range(args.rounds):
        cases = args.cases if round_index % 2 == 0 else list(reversed(args.cases))
        engines = list(binaries)
        if round_index % 2:
            engines.reverse()
        for case in cases:
            config, script, selected_keys = configs[case]
            kind, count = case.split("-")
            count = int(count)
            for engine in engines:
                command = ["taskset", "-c", output["server_cpus"], str(binaries[engine]), "serve", "-c", str(config), "--admin", f"127.0.0.1:{ports['admin']}", "--threads", str(args.workers), "--max-inflight", "4096", "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "5"]
                frontend = start(command, work / f"{case}-{round_index+1}-{engine}.log")
                wait_for(lambda: request(ports["admin"], "/readyz")[0], 200)
                for key in selected_keys:
                    headers = {"Host": "localhost", "Cookie" if kind == "sticky" else "x-session": f"session={key}" if kind == "sticky" else key}
                    status, response, body = request(ports["http"], "/", headers=headers)
                    assert status == 200 and body == payload.encode(), (status, body[:200])
                    def score(item):
                        i, ip = item
                        digest = hashlib.sha256(f"{key}\0{ip}:{ports['origin']}".encode()).digest()
                        value = int.from_bytes(digest[:8], "big")
                        return -math.log((float(value) + 1.0) / (float(2**64-1) + 2.0)) / (1+i%5)
                    slots = [ip for i, ip in enumerate(addresses[:count]) for _ in range(1+i%5)]
                    expected = slots[selected_keys.index(key) % len(slots)] if kind == "rr" else min(enumerate(addresses[:count]), key=score)[1]
                    assert response["x-origin"] == expected, (case, engine, key, expected, response)
                output["preflight"].append({"round": round_index+1, "case": case, "engine": engine, "keys_checked": len(selected_keys), "body_bytes": len(payload)})
                url = f"http://127.0.0.1:{ports['http']}/"
                warmup = measure(args, url, script, 64, args.warmup, frontend, origin)
                assert warmup["requests"] and not warmup["errors"], warmup
                run = measure(args, url, script, 64, args.seconds, frontend, origin)
                run.update(engine=engine, case=case, concurrency=64, round=round_index+1)
                output["runs"].append(run)
                save_results(output, result_path)
                print(f"round {round_index+1} {case} {engine}: {run['rps']:.0f} req/s; CPU {run['server_cpu_us_per_request']:.2f} us/request; P99 {run['p99_ms']:.3f} ms; errors {run['errors']}", flush=True)
                assert run["requests"] and not run["errors"], run
                stop(frontend)
                frontend = None
finally:
    stop(frontend)
    stop(origin)
    save_results(output, result_path)
if args.aa:
    differences = []
    for case in args.cases:
        for index in range(1, args.rounds+1):
            pair = [r["rps"] for r in output["runs"] if r["case"] == case and r["round"] == index]
            assert len(pair) == 2
            differences.append(abs(pair[0]-pair[1]) / min(pair) * 100)
    max_cv = max(row["rps_cv_pct"] for row in output["summary"].values())
    output["aa_gate"] = {"max_pair_difference_pct": max(differences), "max_rps_cv_pct": max_cv,
                         "passed": max(differences) <= 10 and max_cv <= 10 and not any(r["errors"] for r in output["runs"])}
    save_results(output, result_path)
    print(output["aa_gate"], flush=True)

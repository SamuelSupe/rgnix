#!/usr/bin/env python3
"""Compare rgnix builds, NGINX and OpenResty on Linux with wrk and fixed CPU sets."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import signal
import ssl
import statistics
import subprocess
import time

from benchmark import affinity
from integration import certificate, free_port, request, wait_for


CASES = ["return", "static", "proxy-1k", "proxy-16k", "header", "body", "tls"]
MINIMAL_ENGINES = {"pingora", "hyper", "hyper-control"}
PATHS = {"return": "/return", "static": "/static.bin", "proxy-1k": "/proxy-1k",
         "proxy-16k": "/proxy-16k", "header": "/route-header", "body": "/route-body", "tls": "/proxy-1k"}
BODY_SIZE = 65536
MARKER = b"route=canary;"
BODY_CANARY = MARKER + b"x" * (BODY_SIZE - len(MARKER))
BODY_PRIMARY = b"x" * 1024 + MARKER + b"x" * (BODY_SIZE - 1024 - len(MARKER))
REPORT_LUA = '''
function done(summary, latency, requests)
  local e = summary.errors
  io.write(string.format('BENCH_RESULT {"requests":%d,"seconds":%.6f,"bytes":%d,"connect_errors":%d,"read_errors":%d,"write_errors":%d,"status_errors":%d,"timeout_errors":%d,"p50_ms":%.6f,"p95_ms":%.6f,"p99_ms":%.6f}\\n',
    summary.requests, summary.duration / 1000000, summary.bytes,
    e.connect, e.read, e.write, e.status, e.timeout,
    latency:percentile(50) / 1000, latency:percentile(95) / 1000, latency:percentile(99) / 1000))
end
'''


def command_output(command, accepted=(0,)):
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    if result.returncode not in accepted:
        raise RuntimeError(f"{command}: {result.stdout}")
    return result.stdout.strip()


def tree_stats(pid):
    """Count master and workers; PSS avoids counting shared pages once per worker."""
    pending, seen = [pid], set()
    cpu, rss, pss = 0, 0, 0
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        base = Path(f"/proc/{current}")
        stat = (base / "stat").read_text().rsplit(")", 1)[1].split()
        cpu += int(stat[11]) + int(stat[12])
        memory = dict(line.split(":", 1) for line in (base / "smaps_rollup").read_text().splitlines()[1:])
        rss += int(memory["Rss"].split()[0])
        pss += int(memory["Pss"].split()[0])
        # NGINX forks workers from its main thread; rgnix has no subprocesses.
        pending.extend(int(child) for child in (base / f"task/{current}/children").read_text().split())
    return {"cpu_seconds": cpu / os.sysconf("SC_CLK_TCK"), "rss_kib": rss, "pss_kib": pss, "pids": sorted(seen)}


def cpu_snapshot():
    return {parts[0]: [int(value) for value in parts[1:9]]
            for line in Path("/proc/stat").read_text().splitlines()
            if (parts := line.split())[0].startswith("cpu")}


def thread_stats(pids):
    result = {}
    for pid in pids:
        for path in Path(f"/proc/{pid}/task").iterdir():
            try:
                name, rest = (path / "stat").read_text().rsplit(")", 1)
                parts = rest.split()
                result[path.name] = {"name": name.split("(", 1)[1],
                                     "cpu_seconds": (int(parts[11]) + int(parts[12])) / os.sysconf("SC_CLK_TCK"),
                                     "last_cpu": int(parts[36])}
            except FileNotFoundError:
                continue
    return result


def start(command, log_path):
    environment = os.environ.copy()
    for key in ["OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"]:
        environment.pop(key, None)
    environment["RUST_LOG"] = "warn"
    with log_path.open("w") as log:
        return subprocess.Popen(command, stdout=log, stderr=log, start_new_session=True, env=environment)


def stop(process):
    if process and process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=35)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise RuntimeError(f"benchmark process {process.pid} failed to terminate gracefully")


def measure(args, url, script, concurrency, seconds, server, origin):
    command = [*affinity(args.client_cpus), args.wrk, "-t", str(args.client_threads), "-c", str(concurrency),
               "-d", f"{seconds}s", "--timeout", "5s", "--latency", "-s", str(script), "-H", "Host: localhost"]
    if args.connection_close:
        command.extend(["-H", "Connection: close"])
    command.append(url)
    before = tree_stats(server.pid)
    threads_before = thread_stats(before["pids"])
    origin_before = tree_stats(origin.pid)
    usage_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpus_before = cpu_snapshot()
    load_before = Path("/proc/loadavg").read_text().strip()
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    peak_pss, peak_rss = before["pss_kib"], before["rss_kib"]
    deadline = time.monotonic() + seconds + 15
    try:
        while process.poll() is None:
            if time.monotonic() > deadline:
                process.kill()
                raise TimeoutError("wrk did not finish")
            sample = tree_stats(server.pid)
            if sample["pids"] != before["pids"]:
                raise RuntimeError("server worker set changed during measurement")
            peak_pss = max(peak_pss, sample["pss_kib"])
            peak_rss = max(peak_rss, sample["rss_kib"])
            time.sleep(0.2)
        stdout, _ = process.communicate()
    finally:
        if process.poll() is None:
            process.kill()
            process.communicate()
    if process.returncode:
        raise RuntimeError(stdout)
    usage_after = resource.getrusage(resource.RUSAGE_CHILDREN)
    after = tree_stats(server.pid)
    threads_after = thread_stats(after["pids"])
    origin_after = tree_stats(origin.pid)
    if after["pids"] != before["pids"] or origin_after["pids"] != origin_before["pids"]:
        raise RuntimeError("server or origin restarted during measurement")
    cpus_after = cpu_snapshot()
    result = json.loads(next(line.removeprefix("BENCH_RESULT ") for line in stdout.splitlines()
                             if line.startswith("BENCH_RESULT ")))
    result["errors"] = sum(result[key] for key in result if key.endswith("_errors"))
    result["rps"] = result["requests"] / result["seconds"]
    server_seconds = after["cpu_seconds"] - before["cpu_seconds"]
    result.update(server_cpu_cores=server_seconds / result["seconds"],
                  server_cpu_us_per_request=server_seconds * 1e6 / max(1, result["requests"]),
                  origin_cpu_cores=(origin_after["cpu_seconds"] - origin_before["cpu_seconds"]) / result["seconds"],
                  client_cpu_cores=(usage_after.ru_utime + usage_after.ru_stime - usage_before.ru_utime - usage_before.ru_stime) / result["seconds"],
                  peak_pss_kib=peak_pss, peak_rss_kib=peak_rss, server_pids=after["pids"],
                  host_cpu_ticks={key: [b - a for a, b in zip(cpus_before[key], value)] for key, value in cpus_after.items()},
                  server_threads={tid: {**value, "cpu_seconds": value["cpu_seconds"] - threads_before.get(tid, {}).get("cpu_seconds", 0)}
                                  for tid, value in threads_after.items()},
                  load_before=load_before, load_after=Path("/proc/loadavg").read_text().strip(),
                  command=command, wrk_output=stdout)
    return result


def nginx_prelude(directory, workers):
    paths = "\n".join(f"{kind}_temp_path {directory}/{kind};" for kind in ["client_body", "proxy", "fastcgi", "uwsgi", "scgi"])
    return f'''worker_processes {workers};
pid {directory}/server.pid;
error_log {directory}/error.log warn;
events {{ worker_connections 16384; }}
http {{
  {paths}
  access_log off;
  sendfile on;
  tcp_nodelay on;
  keepalive_timeout 60s;
  keepalive_requests 1000000;
  client_max_body_size 1m;
  client_body_buffer_size 128k;
  default_type application/octet-stream;
'''


def fixtures(args, directory, ports):
    data = directory / "data"
    data.mkdir()
    (data / "static.bin").write_bytes(b"x" * 16384)
    (data / "proxy-1k").write_bytes(b"x" * 1024)
    (data / "proxy-16k").write_bytes(b"x" * 16384)
    certificate(directory, 1, names=("localhost",))
    (directory / "header.rgl").write_text('''function on_request()
    if req.header("x-canary") == "1" then
        req.set_header("x-selected", "canary")
        return route.proxy("canary")
    end
    req.set_header("x-selected", "primary")
    return route.proxy("primary")
end
''')
    (directory / "body.rgl").write_text('''function on_request()
    if req.body_contains("route=canary;") then
        req.set_header("x-selected", "canary")
        return route.proxy("canary")
    end
    req.set_header("x-selected", "primary")
    return route.proxy("primary")
end
''')
    selection = '''local selected = "primary"
if CANARY then selected = "canary" end
ngx.var.selected_upstream = selected
ngx.req.set_header("x-selected", selected)
'''
    (directory / "header.lua").write_text(selection.replace("CANARY", 'ngx.var.http_x_canary == "1"'))
    (directory / "body.lua").write_text('''ngx.req.read_body()
local body = assert(ngx.req.get_body_data(), "benchmark body must stay in memory")
local prefix = string.sub(body, 1, 512)
''' + selection.replace("CANARY", 'string.find(prefix, "route=canary;", 1, true)'))
    (directory / "origin.lua").write_text('''ngx.req.read_body()
local body = assert(ngx.req.get_body_data())
if ngx.req.get_headers()["x-bench-verify"] == "1" then
    ngx.header["x-body-md5"] = ngx.md5(body)
    ngx.header["x-body-length"] = #body
end
ngx.header["content-length"] = 1024
ngx.print(string.rep("x", 1024))
''')
    origin_dir = directory / "origin"
    origin_dir.mkdir()
    origin_conf = nginx_prelude(origin_dir, args.origin_workers)
    for name in ["primary", "canary"]:
        origin_conf += f'''server {{
  listen 127.0.0.1:{ports[name]};
  root {data};
  add_header X-Origin {name} always;
  add_header X-Observed-Selected $http_x_selected always;
  location = /route-header {{ return 200 "{'x' * 1024}"; }}
  location = /route-body {{ content_by_lua_file {directory}/origin.lua; }}
}}
'''
    (origin_dir / "nginx.conf").write_text(origin_conf + "}\n")

    proxy = '''proxy_http_version 1.1;
proxy_set_header Host localhost;
proxy_set_header Connection "";
proxy_connect_timeout 5s;
proxy_read_timeout 5s;
proxy_send_timeout 5s;
proxy_buffering off;
proxy_request_buffering off;
proxy_next_upstream off;
'''
    for engine in ["rgnix", "nginx", "openresty"]:
        engine_dir = directory / engine
        engine_dir.mkdir()
        config = ("events {}\nhttp {\naccess_log off;\nkeepalive_timeout 60s;\n" if engine == "rgnix"
                  else nginx_prelude(engine_dir, args.workers) + proxy)
        for name in ["primary", "canary"]:
            keepalive = "" if engine == "rgnix" else "keepalive 512; keepalive_requests 1000000; keepalive_timeout 60s;"
            config += f"upstream {name} {{ server 127.0.0.1:{ports[name]}; {keepalive} }}\n"
        if engine == "nginx":
            config += "map $http_x_canary $selected_upstream { default primary; 1 canary; }\n"
        tls_listener = "" if args.plain_proxy else f'''listen 127.0.0.1:{ports['tls']} ssl;
ssl_certificate {directory}/cert.pem;
ssl_certificate_key {directory}/key.pem;
'''
        config += f'''server {{
listen 127.0.0.1:{ports['http']};
{tls_listener}server_name localhost;
root {data};
location = /return {{ return 200 ok; }}
location = /proxy-1k {{ proxy_pass http://primary; }}
location = /proxy-16k {{ proxy_pass http://primary; }}
'''
        config += "location / { return 404; }\n" if args.plain_proxy else "location = /static.bin { }\n"
        for case in ([] if args.plain_proxy else ["header", "body"]):
            if engine == "rgnix":
                hook = f"rgnix_script {directory}/{case}.rgl;"
                if case == "body":
                    hook += " rgnix_request_body prefix 512;"
                action = "proxy_pass http://primary;"
            elif engine == "openresty":
                hook = f"set $selected_upstream primary; access_by_lua_file {directory}/{case}.lua;"
                action = "proxy_pass http://$selected_upstream;"
            elif case == "header":
                hook = 'proxy_set_header Host localhost; proxy_set_header Connection ""; proxy_set_header X-Selected $selected_upstream;'
                action = "proxy_pass http://$selected_upstream;"
            else:
                continue
            config += f"location = /route-{case} {{ {hook} {action} }}\n"
        (engine_dir / "nginx.conf").write_text(config + "}\n}\n")

    for case in CASES:
        setup = ""
        if case in ["header", "body"]:
            setup = '''local requests = {}
local counter = 0
'''
            connection = ', ["Connection"]="close"' if args.connection_close else ""
            for index in [1, 2]:
                if case == "header":
                    setup += f'requests[{index}] = wrk.format("GET", "/route-header", {{["Host"]="localhost", ["x-canary"]="{index % 2}"{connection}}})\n'
                else:
                    body = (f'"route=canary;" .. string.rep("x", {BODY_SIZE - len(MARKER)})' if index == 1 else
                            f'string.rep("x", 1024) .. "route=canary;" .. string.rep("x", {BODY_SIZE - 1024 - len(MARKER)})')
                    setup += f'requests[{index}] = wrk.format("POST", "/route-body", {{["Host"]="localhost", ["Content-Type"]="application/octet-stream"{connection}}}, {body})\n'
            setup += '''function request()
  counter = counter % 2 + 1
  return requests[counter]
end
'''
        (directory / f"wrk-{case}.lua").write_text(setup + REPORT_LUA)


def preflight(engine, ports, plain_proxy=False):
    results = []
    for case in CASES:
        if (engine in MINIMAL_ENGINES or plain_proxy) and case not in ["proxy-1k", "proxy-16k"]:
            continue
        if engine == "nginx" and case == "body":
            continue
        if case in ["header", "body"]:
            for selected, body in [("primary", BODY_PRIMARY), ("canary", BODY_CANARY)]:
                headers = {"Host": "localhost", "x-canary": "1" if selected == "canary" else "0", "x-bench-verify": "1"}
                status, response, payload = request(ports["http"], PATHS[case], "POST" if case == "body" else "GET",
                                                     headers, body if case == "body" else None)
                assert status == 200 and payload == b"x" * 1024, (engine, case, status, payload[:100])
                assert response["x-origin"] == selected and response["x-observed-selected"] == selected, response
                if case == "body":
                    assert response["x-body-md5"] == hashlib.md5(body).hexdigest(), response
                    assert int(response["x-body-length"]) == BODY_SIZE, response
                results.append(f"{case}:{selected}:correct backend, header, payload" + (", full body MD5/length" if case == "body" else ""))
        else:
            status, _, payload = request(ports["tls" if case == "tls" else "http"], PATHS[case], tls=case == "tls", headers={"Host": "localhost"})
            expected = b"ok" if case == "return" else b"x" * (16384 if case in ["static", "proxy-16k"] else 1024)
            assert status == 200 and payload == expected, (engine, case, status, payload[:100])
            results.append(f"{case}:200 and exact payload")
    if engine in MINIMAL_ENGINES or plain_proxy:
        return results
    import socket
    with socket.create_connection(("127.0.0.1", ports["tls"])) as raw:
        with ssl._create_unverified_context().wrap_socket(raw, server_hostname="localhost") as connection:
            results.append({"tls_version": connection.version(), "cipher": connection.cipher()})
    return results


def save_results(output, path):
    groups = {}
    for run in output["runs"]:
        key = f"{run['engine']}/{run['case']}/c{run['concurrency']}"
        groups.setdefault(key, []).append(run)
    output["summary"] = {}
    for key, runs in groups.items():
        metrics = ["rps", "p50_ms", "p95_ms", "p99_ms", "server_cpu_cores", "server_cpu_us_per_request",
                   "client_cpu_cores", "origin_cpu_cores", "peak_pss_kib", "peak_rss_kib"]
        summary = {"samples": len(runs), "requests": sum(run["requests"] for run in runs), "errors": sum(run["errors"] for run in runs)}
        for field in metrics:
            values = [run[field] for run in runs]
            summary[field] = {"median": statistics.median(values), "min": min(values), "max": max(values)}
        summary["rps_cv_pct"] = statistics.stdev(run["rps"] for run in runs) / statistics.mean(run["rps"] for run in runs) * 100 if len(runs) > 1 else 0
        output["summary"][key] = summary
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(output, indent=2) + "\n")
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for engine in ["rgnix", "nginx", "openresty"]:
        parser.add_argument(f"--{engine}", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, help="additional rgnix binary using exactly the same configuration")
    parser.add_argument("--plain-proxy", action="store_true", help="only common plain HTTP/1 proxy configuration; no TLS/static/RGL routes")
    parser.add_argument("--rgnix-transport", choices=["pingora", "hyper"], default="pingora")
    parser.add_argument("--candidate-transport", choices=["pingora", "hyper"], default="pingora")
    parser.add_argument("--pingora", type=Path, help="minimal_proxy example; diagnostic proxy-only baseline without product policies")
    parser.add_argument("--hyper", type=Path, help="experimental Hyper HTTP/1 proxy; no product policies")
    parser.add_argument("--hyper-control", type=Path, help="same Hyper binary for interleaved A/A calibration")
    parser.add_argument("--engines", nargs="+", choices=["rgnix", "candidate", "nginx", "openresty", "pingora", "hyper", "hyper-control"],
                        help="engines to measure; defaults to all supplied binaries")
    parser.add_argument("--wrk", default="wrk")
    parser.add_argument("--work-dir", type=Path, required=True, help="new directory on a native Linux filesystem")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--seconds", type=int, default=8)
    parser.add_argument("--warmup", type=int, default=2)
    parser.add_argument("--concurrency", type=int, nargs="+", default=[64, 256])
    parser.add_argument("--cases", choices=CASES, nargs="+", default=CASES)
    parser.add_argument("--connection-close", action="store_true", help="open a fresh HTTP/1.1 connection per request")
    parser.add_argument("--interleave", action="store_true", help="restart and compare engines back-to-back for each case/concurrency pair")
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--origin-workers", type=int, default=4)
    parser.add_argument("--client-threads", type=int, default=4)
    parser.add_argument("--server-cpus", default="0,1")
    parser.add_argument("--client-cpus", default="2,3,4,5")
    parser.add_argument("--origin-cpus", default="6,7,8,9")
    args = parser.parse_args()
    if args.plain_proxy and any(case not in ["proxy-1k", "proxy-16k"] for case in args.cases):
        parser.error("--plain-proxy requires --cases proxy-1k and/or proxy-16k")
    if "hyper" in [args.rgnix_transport, args.candidate_transport] and not args.plain_proxy:
        parser.error("product Hyper transport requires --plain-proxy")
    if args.engines and "candidate" in args.engines and not args.candidate:
        parser.error("--engines candidate requires --candidate")
    if args.engines and "pingora" in args.engines and not args.pingora:
        parser.error("--engines pingora requires --pingora")
    for name in ["hyper", "hyper-control"]:
        if args.engines and name in args.engines and not getattr(args, name.replace("-", "_")):
            parser.error(f"--engines {name} requires --{name}")
    if min(args.rounds, args.seconds, args.warmup, args.workers, args.origin_workers, args.client_threads, *args.concurrency) < 1:
        parser.error("counts and durations must be positive")
    if args.client_threads > min(args.concurrency):
        parser.error("client threads cannot exceed concurrency")
    cpu_sets = [set(int(cpu) for cpu in value.split(",")) for value in [args.server_cpus, args.client_cpus, args.origin_cpus]]
    if any(cpu_sets[a] & cpu_sets[b] for a, b in [(0, 1), (0, 2), (1, 2)]):
        parser.error("CPU sets must not overlap")
    if not set.union(*cpu_sets) <= os.sched_getaffinity(0):
        parser.error("requested CPUs are not available")
    directory = args.work_dir.resolve()
    directory.mkdir(parents=True, exist_ok=False)
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    ports = {}
    for name in ["http", "tls", "admin", "primary", "canary"]:
        port = free_port()
        while port in ports.values():
            port = free_port()
        ports[name] = port
    binaries = {name: str(getattr(args, name).resolve()) for name in ["rgnix", "nginx", "openresty"]}
    if args.candidate:
        binaries["candidate"] = str(args.candidate.resolve())
    for name in sorted(MINIMAL_ENGINES):
        binary = getattr(args, name.replace("-", "_"))
        if binary:
            binaries[name] = str(binary.resolve())
            (directory / name).mkdir()
    output = {"started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "platform": platform.platform(),
              "os_release": Path("/etc/os-release").read_text(), "lscpu": command_output(["lscpu"]),
              "settings": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
              "binaries": {name: {"path": binary, "sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
                                   "version": command_output([binary, "--version" if name in {"rgnix", "candidate"} | MINIMAL_ENGINES else "-V"])} for name, binary in binaries.items()},
              "wrk_version": command_output([args.wrk, "--version"], accepted=(0, 1)).splitlines()[0],
              "ports": ports, "preflight": {}, "runs": [], "origin_baselines": []}
    fixtures(args, directory, ports)
    output["fixtures"] = {str(path.relative_to(directory)): path.read_text() for path in sorted(directory.rglob("*"))
                          if path.suffix in [".conf", ".lua", ".rgl"]}
    origin_dir = directory / "origin"
    origin = start([*affinity(args.origin_cpus), binaries["openresty"], "-p", str(origin_dir), "-c", str(origin_dir / "nginx.conf"), "-g", "daemon off;"], origin_dir / "process.log")
    frontend = None
    try:
        wait_for(lambda: request(ports["primary"], "/proxy-1k")[0], 200)
        # Establish upstream headroom separately; body baseline includes the same 64 KiB upload.
        for case in (args.cases if args.plain_proxy else ["proxy-1k", "proxy-16k", "header", "body"]):
            url = f"http://127.0.0.1:{ports['primary']}{PATHS[case]}"
            script = directory / f"wrk-{case}.lua"
            measure(args, url, script, max(args.concurrency), args.warmup, origin, origin)
            run = measure(args, url, script, max(args.concurrency), args.seconds, origin, origin)
            run.update(case=case, concurrency=max(args.concurrency))
            output["origin_baselines"].append(run)
            save_results(output, args.output)
            assert not run["errors"] and run["requests"], run
            print(f"origin {case}: {run['rps']:.0f} req/s", flush=True)
        engines = args.engines or list(binaries)
        for round_index in range(args.rounds):
            ordered_engines = engines[round_index % len(engines):] + engines[:round_index % len(engines)]
            cases = args.cases if round_index % 2 == 0 else list(reversed(args.cases))
            concurrencies = args.concurrency if round_index % 2 == 0 else list(reversed(args.concurrency))
            windows = [(concurrency, case) for concurrency in concurrencies for case in cases]
            batches = ([(engine, [window]) for window in windows for engine in ordered_engines]
                       if args.interleave else [(engine, windows) for engine in ordered_engines])
            for batch_index, (engine, batch) in enumerate(batches):
                batch = [(concurrency, case) for concurrency, case in batch if engine != "nginx" or case != "body"]
                if engine in MINIMAL_ENGINES:
                    batch = [(concurrency, case) for concurrency, case in batch if case in ["proxy-1k", "proxy-16k"]]
                if not batch:
                    continue
                is_rgnix = engine in ["rgnix", "candidate"]
                engine_dir = directory / ("rgnix" if is_rgnix else engine)
                config = engine_dir / "nginx.conf"
                if is_rgnix:
                    command = [binaries[engine], "serve", "-c", str(config), "--admin", f"127.0.0.1:{ports['admin']}",
                               "--threads", str(args.workers), "--max-inflight", "4096", "--max-plugin-instances", "512",
                               "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "5"]
                    if getattr(args, f"{engine}_transport") == "hyper":
                        command.append("--experimental-hyper")
                elif engine in MINIMAL_ENGINES:
                    command = [binaries[engine], "--listen", f"127.0.0.1:{ports['http']}",
                               "--upstream", f"127.0.0.1:{ports['primary']}", "--workers", str(args.workers)]
                else:
                    command = [binaries[engine], "-p", str(engine_dir), "-c", str(config), "-g", "daemon off;"]
                suffix = f"-batch{batch_index + 1}" if args.interleave else ""
                frontend = start([*affinity(args.server_cpus), *command], engine_dir / f"{engine}-process-{round_index + 1}{suffix}.log")
                ready_path = "/readyz" if is_rgnix else "/proxy-1k" if engine in MINIMAL_ENGINES or args.plain_proxy else "/return"
                wait_for(lambda: request(ports["admin"] if is_rgnix else ports["http"], ready_path)[0], 200, timeout=30)
                output["preflight"][f"{engine}/round{round_index + 1}{suffix}"] = preflight(engine, ports, args.plain_proxy)
                for concurrency, case in batch:
                    url = f"{'https' if case == 'tls' else 'http'}://127.0.0.1:{ports['tls' if case == 'tls' else 'http']}{PATHS[case]}"
                    script = directory / f"wrk-{case}.lua"
                    warmup = measure(args, url, script, concurrency, args.warmup, frontend, origin)
                    assert not warmup["errors"] and warmup["requests"], warmup
                    run = measure(args, url, script, concurrency, args.seconds, frontend, origin)
                    run.update(engine=engine, case=case, concurrency=concurrency, round=round_index + 1)
                    output["runs"].append(run)
                    save_results(output, args.output)
                    print(f"round {round_index + 1} {engine} {case} c{concurrency}: {run['rps']:.0f} req/s; p99 {run['p99_ms']:.3f} ms; CPU {run['server_cpu_cores']:.2f}; PSS {run['peak_pss_kib'] / 1024:.1f} MiB; errors {run['errors']}", flush=True)
                    assert not run["errors"] and run["requests"], run
                stop(frontend)
                frontend = None
        output["completed_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    finally:
        stop(frontend)
        stop(origin)
        save_results(output, args.output)
    print(f"Saved {len(output['runs'])} runs to {args.output}", flush=True)


if __name__ == "__main__":
    main()

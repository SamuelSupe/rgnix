#!/usr/bin/env python3
"""Exercise shared rate limits using independent rgnix processes and a private Redis."""
import concurrent.futures
import json
import pathlib
import signal
import subprocess
import sys
import tempfile
import time

from integration import RESULTS, check, free_port, request, wait_for


def main():
    binary = str(pathlib.Path(sys.argv[1]).resolve())
    processes = []
    with tempfile.TemporaryDirectory(prefix="rgnix-shared-rate-") as temp:
        root = pathlib.Path(temp)
        redis_port = free_port()
        password = "qa-shared-limit-secret"
        output = (root / "processes.log").open("w+")

        def redis_start():
            process = subprocess.Popen(["redis-server", "--bind", "127.0.0.1", "--port", str(redis_port),
                                        "--save", "", "--appendonly", "no", "--requirepass", password,
                                        "--dir", str(root)], stdout=output, stderr=output)
            processes.append(process)
            wait_for(lambda: subprocess.run(["redis-cli", "-p", str(redis_port), "ping"], capture_output=True).stdout.startswith(b"NOAUTH"), True)
            return process

        def start(mode="closed", scope="qa", index=0):
            port, admin = free_port(), free_port()
            config = root / f"{mode}-{index}.json"
            config.write_text(json.dumps({"url": f"redis://:{password}@127.0.0.1:{redis_port}", "scope": scope,
                                          "failure_mode": mode, "timeout_ms": 100}))
            conf = root / f"{mode}-{index}.conf"
            conf.write_text(f'''http {{ access_log off; server {{ listen 127.0.0.1:{port};
location /health {{ return 200 "ready"; }}
location /rate {{ rgnix_limit_rate 1 burst=2 key=header:x-client; return 200 "accepted"; }}
location /parallel {{ rgnix_limit_rate 1 burst=16 key=route; return 200 "accepted"; }}
}} }}''')
            process = subprocess.Popen([binary, "serve", "-c", str(conf), "--threads", "1", "--admin", f"127.0.0.1:{admin}",
                                        "--global-rate-limit-file", str(config)], stdout=output, stderr=output)
            processes.append(process)
            wait_for(lambda: request(port, "/health")[0], 200)
            return port, admin, config

        def hit(port, client="one"):
            return request(port, "/rate", headers={"x-client": client})[0]

        try:
            redis = redis_start()
            first, second = start(index=1), start(index=2)
            check("Two processes share one token bucket instead of doubling burst", [hit(first[0]), hit(second[0]), hit(first[0])] == [200, 200, 429])
            check("Shared buckets isolate request identities", hit(second[0], "other") == 200)
            third = start(scope="separate", index=3)
            check("Deployment scopes isolate identical routes and identities", hit(third[0]) == 200)
            started = time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
                statuses = list(pool.map(lambda n: request([first[0], second[0]][n % 2], "/parallel")[0], range(48)))
            elapsed = time.monotonic() - started
            check("Atomic concurrent debit across processes bounds accepted requests", 16 <= statuses.count(200) <= 16 + int(elapsed) and set(statuses) <= {200, 429})
            metrics = request(first[1], "/metrics")[2].decode()
            check("Shared limiter exports bounded outcome and latency metrics", 'rgnix_global_rate_limit_total{result="allowed"}' in metrics and 'rgnix_global_rate_limit_seconds_count{result="limited"}' in metrics)
            open_ = start("open", index=4)
            local = start("local", index=5)
            redis.terminate(); redis.wait(timeout=5)
            started = time.monotonic()
            check("Unavailable coordinator fails closed by default", hit(first[0], "closed") == 503 and time.monotonic() - started < 1)
            check("Explicit open mode bypasses only the shared rate gate", [hit(open_[0], "open") for _ in range(3)] == [200, 200, 200])
            check("Explicit local mode retains per-process rate protection", [hit(local[0], "fallback") for _ in range(3)] == [200, 200, 429])
            check("Routes without a rate gate remain available during Redis outage", request(first[0], "/health")[0] == 200)
            redis = redis_start()
            for port in (first[0], second[0]):
                wait_for(lambda: hit(port, "connection-probe"), 200)
            check("Coordinator recovery reconnects without a process restart", [hit(first[0], "reconnected"), hit(second[0], "reconnected"), hit(first[0], "reconnected")] == [200, 200, 429])
            config = json.loads(first[2].read_text()); config["url"] = "rediss://:leak-test@127.0.0.1:1/#insecure"
            first[2].write_text(json.dumps(config))
            wait_for(lambda: b"rgnix_control_reload_errors_total 1" in request(first[1], "/metrics")[2], True)
            check("Invalid hot configuration retains the accepted coordinator", hit(first[0], "retained") == 200 and hit(second[0], "retained") == 200 and hit(first[0], "retained") == 429)
            output.flush()
            check("Coordinator credentials stay out of logs and metrics", password not in (root / "processes.log").read_text() and "leak-test" not in (root / "processes.log").read_text() and password not in metrics)
            pathlib.Path(".local").mkdir(exist_ok=True)
            pathlib.Path(".local/global-rate-results.json").write_text(json.dumps(RESULTS, indent=2) + "\n")
            print(f"PASS {len(RESULTS)} shared-rate checks")
        except Exception:
            output.flush()
            print((root / "processes.log").read_text()[-5000:], file=sys.stderr)
            raise
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    process.wait(timeout=15)
            output.close()


if __name__ == "__main__":
    main()

"""Reproduce idle HTTP/1 TLS reuse; narrow latency observations, not saturation capacity."""
import argparse
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import ssl
import statistics
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
from benchmark_compare import start, stop, connection_counts
from integration import certificate, free_port, request, wait_for

class Origin(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *_):
        pass
    def setup(self):
        super().setup()
        import socket
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "1024")
        self.send_header("X-Origin-Peer", str(self.client_address[1]))
        self.end_headers()
        self.wfile.write(b"x" * 1024)

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--before", type=Path, required=True)
parser.add_argument("--after", type=Path, required=True)
parser.add_argument("--work-dir", type=Path, required=True)
args = parser.parse_args()
work = args.work_dir.resolve()
work.mkdir(parents=True, exist_ok=False)
certificate(work, 1, names=("localhost",))
origin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
origin.daemon_threads = True
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(work / "cert.pem", work / "key.pem")
origin.socket = context.wrap_socket(origin.socket, server_side=True)
def serve_origin():
    os.sched_setaffinity(0, {6})
    origin.serve_forever()
threading.Thread(target=serve_origin, daemon=True).start()
os.sched_setaffinity(0, {2})
port, admin = free_port(), free_port()
config = work / "nginx.conf"
config.write_text(f"""events {{}} http {{ access_log off;
upstream origin {{ server 127.0.0.1:{origin.server_port}; }}
server {{ listen 127.0.0.1:{port}; keepalive_timeout 5s;
location / {{ proxy_pass https://origin; proxy_ssl_name localhost;
proxy_ssl_trusted_certificate {work}/cert.pem; proxy_read_timeout 100ms; }} }} }}
""")
result = {"method": "Three rounds with two same-binary windows per round, rotated before/after; one worker CPU10, client CPU2, Python TLS origin CPU6. 10 body-validated requests per window, one warm request excluded, 250ms sleep after complete responses; response deadline 100ms, keepalive 5s; upstream CA and hostname verified. Latency covers downstream send through full body, not sleep. Connection counts are successful acquisition metrics, not all TCP attempts.",
          "settings": {"rounds": 3, "requests_per_window": 10, "sleep_seconds": .25},
          "config": config.read_text(), "binaries": {}, "runs": []}
for name in ("before", "after"):
    binary = getattr(args, name).resolve()
    result["binaries"][name] = {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
process = None
try:
    for round_index in range(3):
        order = ["before", "before", "after", "after"]
        order = order[round_index:] + order[:round_index]
        for index, name in enumerate(order):
            binary = result["binaries"][name]["path"]
            process = start(["taskset", "-c", "10", binary, "serve", "-c", str(config), "--experimental-hyper", "--threads", "1", "--admin", f"127.0.0.1:{admin}", "--shutdown-grace-seconds", "0", "--shutdown-timeout-seconds", "5"], work/f"{name}-{round_index}-{index}.log")
            wait_for(lambda: request(admin, "/readyz")[0], 200)
            client = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
            def send():
                begun = time.perf_counter_ns()
                client.request("GET", "/", headers={"Host": "localhost"})
                response = client.getresponse()
                assert response.status == 200 and response.read() == b"x" * 1024
                return {"milliseconds": (time.perf_counter_ns() - begun) / 1e6, "origin_peer": response.getheader("x-origin-peer")}
            send()
            time.sleep(.25)
            counts = connection_counts(admin)
            samples = []
            for _ in range(10):
                samples.append(send())
                time.sleep(.25)
            end = connection_counts(admin)
            run = {"engine": name, "round": round_index+1, "samples": samples,
                   "latency_ms_median": statistics.median(s["milliseconds"] for s in samples),
                   "upstream_checkouts": {k:end[k]-v for k,v in counts.items()},
                   "unique_origin_peers": len({s["origin_peer"] for s in samples})}
            result["runs"].append(run)
            (work/"result.json").write_text(json.dumps(result, indent=2)+"\n")
            print(name, round_index+1, run["latency_ms_median"], run["upstream_checkouts"], flush=True)
            client.close()
            stop(process)
            process = None
finally:
    stop(process)
    origin.shutdown()
    (work/"result.json").write_text(json.dumps(result, indent=2)+"\n")

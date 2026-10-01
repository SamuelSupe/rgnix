#!/usr/bin/env python3
"""Verify the packaged executable and engine selection inside the final image."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

from integration import request, wait_for

parser = argparse.ArgumentParser()
parser.add_argument("image")
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--arch", choices=["amd64", "arm64"], required=True)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()


def docker(*argv):
    return subprocess.check_output(["docker", *argv], text=True).strip()


metadata = json.loads(docker("image", "inspect", args.image))[0]
assert metadata["Architecture"] == args.arch, metadata["Architecture"]
checks = []
with tempfile.TemporaryDirectory(prefix="rgnix-image-") as temporary:
    directory = Path(temporary)
    copied = directory / "rgnix"
    container = docker("create", args.image)
    try:
        docker("cp", f"{container}:/usr/local/bin/rgnix", str(copied))
    finally:
        docker("rm", container)
    digest = hashlib.sha256(copied.read_bytes()).hexdigest()
    assert digest == hashlib.sha256(args.binary.read_bytes()).hexdigest(), "image binary differs from tested artifact"
    config = directory / "nginx.conf"
    directory.chmod(0o755)
    config.write_text("events {} http { server { listen 8080; return 200 image-ok; } }")
    config.chmod(0o644)
    for engine in ("hyper", "pingora"):
        container = docker("run", "-d", "--read-only", "--cap-drop=ALL",
            "--tmpfs", "/tmp", "-p", "127.0.0.1::8080", "-p", "127.0.0.1::9090",
            "-v", f"{config}:/etc/rgnix/nginx.conf:ro", args.image, "serve",
            "-c", "/etc/rgnix/nginx.conf", "--engine", engine, "--admin", "0.0.0.0:9090",
            "--shutdown-grace-seconds", "0")
        try:
            ports = json.loads(docker("inspect", container))[0]["NetworkSettings"]["Ports"]
            port = int(ports["8080/tcp"][0]["HostPort"])
            admin = int(ports["9090/tcp"][0]["HostPort"])
            wait_for(lambda: request(admin, "/readyz")[0] == 200, timeout=30)
            assert request(port)[2] == b"image-ok"
            metrics = request(admin, "/metrics")[2].decode()
            assert f'rgnix_engine_info{{engine="{engine}"}} 1' in metrics
            checks.append({"engine": engine, "http": "PASS", "ready": "PASS", "metrics": "PASS"})
        except Exception:
            print(docker("logs", container), flush=True)
            raise
        finally:
            docker("stop", "--time", "10", container)
            docker("rm", container)
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps({"image": args.image, "image_id": metadata["Id"],
    "architecture": args.arch, "host_architecture": os.uname().machine,
    "binary_sha256": digest, "checks": checks}, indent=2) + "\n")
print(json.dumps(checks), flush=True)

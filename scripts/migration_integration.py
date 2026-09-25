#!/usr/bin/env python3
import json
import pathlib
import subprocess
import sys
import tempfile

binary = str(pathlib.Path(sys.argv[1]).resolve())
checks = []


def run(*args, success=True):
    result = subprocess.run([binary, *map(str, args)], capture_output=True, text=True, timeout=30)
    assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
    return json.loads(result.stdout)


def passed(name):
    checks.append(name)
    print("PASS", name)


with tempfile.TemporaryDirectory() as temporary:
    directory = pathlib.Path(temporary)
    source = directory / "nginx.conf"
    destination = directory / "out"; destination.mkdir()
    candidate = destination / "rgnix.conf"
    source.write_text('worker_processes auto; events { worker_connections 1024; } http { server { listen 18080; location / { return 200 "hello"; } } }')
    report = run("migrate", "nginx", "-c", source, "-o", candidate, success=False)
    assert not candidate.exists() and len([f for f in report["findings"] if f["severity"] == "blocker"]) == 2
    passed("process-model differences require an explicit choice")
    report = run("migrate", "nginx", "-c", source, "-o", candidate, "--accept-process-differences")
    assert report["compatible"] and str(directory / "html") in candidate.read_text()
    passed("candidate preserves the original default root across directories")
    result = subprocess.run([binary, "check", "-c", str(candidate)], capture_output=True)
    assert result.returncode == 0
    passed("generated NGINX candidate loads")
    fixtures = directory / "requests.json"; fixtures.write_text(json.dumps([{"path": "/"}, {"method": "POST", "path": "/a?q=1", "body": "body"}]))
    assert run("migrate", "compare", "--before", candidate, "--after", candidate, "--requests", fixtures)["compatible"]
    changed = directory / "changed.conf"; changed.write_text(candidate.read_text().replace('"hello"', '"changed"'))
    assert not run("migrate", "compare", "--before", candidate, "--after", changed, "--requests", fixtures, success=False)["compatible"]
    passed("request comparison detects changed responses")
    source.write_text('http { server { listen 18080; mystery x; another y; } }')
    assert len([f for f in run("migrate", "nginx", "-c", source, success=False)["findings"] if f["severity"] == "blocker"]) == 2
    passed("unsupported directives are inventoried together")
    ingress = {"apiVersion": "networking.k8s.io/v1", "kind": "Ingress", "metadata": {"name": "app", "namespace": "test"}, "spec": {"rules": [{"host": "app.example.test", "http": {"paths": [{"path": "/api", "pathType": "Prefix", "backend": {"service": {"name": "app", "port": {"name": "web"}}}}]}}]}}
    service = {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "app", "namespace": "test"}, "spec": {"ports": [{"name": "web", "port": 8080}]}}
    manifest = directory / "input.yaml"; output = directory / "gateway.yaml"
    manifest.write_text(json.dumps(ingress))
    assert not run("migrate", "ingress", "-f", manifest, "-o", output, success=False)["compatible"] and not output.exists()
    passed("unresolved named Service ports block conversion")
    manifest.write_text(json.dumps(ingress) + "\n---\n" + json.dumps(service))
    assert run("migrate", "ingress", "-f", manifest, "-o", output)["compatible"]
    assert "HTTPRoute" in output.read_text() and "8080" in output.read_text()
    passed("multi-document YAML converts with Service port resolution")
    ingress["metadata"]["annotations"] = {"nginx.ingress.kubernetes.io/auth-url": "https://auth.example.test"}
    manifest.write_text(json.dumps({"apiVersion": "v1", "kind": "List", "items": [ingress, service]}))
    assert not run("migrate", "ingress", "-f", manifest, success=False)["compatible"]
    passed("unmapped authentication is never silently dropped")
    ingress["metadata"].pop("annotations"); ingress["spec"]["rules"][0]["host"] = "*.example.test"
    manifest.write_text(json.dumps({"apiVersion": "v1", "kind": "List", "items": [ingress, service]}))
    assert not run("migrate", "ingress", "-f", manifest, success=False)["compatible"]
    passed("wildcard conversion cannot widen hostname access")
    ingress["spec"]["rules"][0].pop("host")
    ingress["spec"]["defaultBackend"] = {"service": {"name": "app", "port": {"number": 8080}}}
    manifest.write_text(json.dumps({"apiVersion": "v1", "kind": "List", "items": [ingress, service]}))
    assert not run("migrate", "ingress", "-f", manifest, success=False)["compatible"]
    passed("default backend overlap cannot silently change fallback precedence")
print(json.dumps({"passed": len(checks), "checks": checks}))

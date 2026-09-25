import base64
import http.client
import json
import subprocess
import tempfile
import time
from pathlib import Path

namespace = "rgnix-governance-final-20260925"
context = "orbstack"
image = "rgnix:0.1.0-governance-qa4"
checks = []
kube = ["kubectl", "--context", context, "-n", namespace]

def run(*args, data=None):
    result = subprocess.run(args, input=data, text=True, capture_output=True, check=True)
    return result.stdout

def k(*args, **kwargs):
    return run(*kube, *args, **kwargs)

def check(name, condition):
    assert condition, name
    checks.append(name)
    print("PASS", name, flush=True)

def wait(fn, expected=True, timeout=45):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            if fn() == expected:
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(.2)
    raise AssertionError("condition did not converge")

labels = json.loads(k("get", "namespace", namespace, "-o", "json"))["metadata"]["labels"]
assert labels.get("rgnix-qa") == "true"
token = base64.b64decode(json.loads(k("get", "secret", "policy-admin", "-o", "json"))["data"]["token"]).decode()
run("helm", "upgrade", "rgnix-qa", "charts/rgnix", "--kube-context", context, "-n", namespace,
    "--reuse-values", "--set", "image.tag=0.1.0-governance-qa4", "--wait", "--timeout", "180s")
pods = [p for p in json.loads(k("get", "pods", "-l", "app.kubernetes.io/instance=rgnix-qa", "-o", "json"))["items"]
        if not p["metadata"].get("deletionTimestamp")]
assert len(pods) == 2 and all(p["spec"]["containers"][0]["image"] == image for p in pods)
with tempfile.TemporaryDirectory(prefix="rgnix-upgrade-") as temp:
    for pod in pods:
        name = pod["metadata"]["name"]
        log_path = Path(temp) / name
        with log_path.open("w") as log:
            process = subprocess.Popen([*kube, "port-forward", "--address", "127.0.0.1", "pod/"+name, ":9090", ":8080"], stdout=log, stderr=log)
            try:
                ports = {}
                def connected():
                    for line in log_path.read_text().splitlines():
                        if line.startswith("Forwarding from"):
                            ports[int(line.split()[-1])] = int(line.split()[2].rsplit(":",1)[1])
                    return len(ports) == 2
                wait(connected)
                def request(path, host="", body=None, admin=False):
                    connection = http.client.HTTPConnection("127.0.0.1", ports[9090 if admin else 8080], timeout=6)
                    headers = {"Host": host, "Authorization":"Bearer "+token} if admin else {"Host":host}
                    try:
                        connection.request("POST" if body is not None else "GET", path, body, headers)
                        response = connection.getresponse()
                        return response.status, response.read()
                    finally:
                        connection.close()
                wait(lambda: request("/fail", "forced-fallback.product.test")[0], 200)
                check("Image upgrade preserves forced RGL rollback on " + name,
                      json.loads(request("/fail", "forced-fallback.product.test")[1])["role"] == "stable")
                check("Image upgrade preserves promoted weights on " + name,
                      json.loads(request("/", "staged.product.test")[1])["role"] == "candidate")
                candidate = {"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"upgrade-preview","namespace":namespace},
                             "spec":{"ingressClassName":namespace,"rules":[{"host":"preflight.product.test","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"stable","port":{"name":"http"}}}}]}}]}}
                preview = request("/v1/validate-ingress", body=json.dumps(candidate), admin=True)
                check("Final image preflight and admission accept an authorized candidate on " + name,
                      preview[0] == 200 and json.loads(preview[1])["valid"])
                run("kubectl", "--context", context, "apply", "--dry-run=server", "-f", "-", data=json.dumps(candidate))
                if pod is pods[0]:
                    original = json.loads(k("get", "ingressclass", namespace, "-o", "json"))
                    assert original["metadata"]["annotations"]["meta.helm.sh/release-namespace"] == namespace
                    original["metadata"] = {key:value for key,value in original["metadata"].items() if key in ("name","labels","annotations")}
                    foreign = json.loads(json.dumps(original))
                    foreign["spec"]["controller"] = "other.example/controller"
                    try:
                        k("delete", "ingressclass", namespace)
                        run("kubectl", "--context", context, "create", "-f", "-", data=json.dumps(foreign))
                        wait(lambda: "belongs to a different controller" in request("/v1/validate-ingress", body=json.dumps(candidate), admin=True)[1].decode())
                        check("Final image refuses a class reassigned to another controller", True)
                        foreign_candidate = json.loads(json.dumps(candidate))
                        foreign_candidate["metadata"]["annotations"] = {"rgnix.io/client-max-body-size":"invalid"}
                        wait(lambda: subprocess.run(["kubectl", "--context", context, "apply", "--dry-run=server", "-f", "-"], input=json.dumps(foreign_candidate), text=True, capture_output=True).returncode, 0)
                        check("Admission leaves a foreign controller's class to that controller", True)
                    finally:
                        k("delete", "ingressclass", namespace, "--ignore-not-found")
                        run("kubectl", "--context", context, "apply", "-f", "-", data=json.dumps(original))
                    wait(lambda: request("/", "staged.product.test")[0], 200)
                    check("Restored class recovers its persisted promoted stage", json.loads(request("/", "staged.product.test")[1])["role"] == "candidate")
            finally:
                process.terminate()
                process.wait(timeout=5)
result = {"namespace":namespace,"image":image,"passed":len(checks),"checks":checks}
Path("docs/validation/governance-upgrade.json").write_text(json.dumps(result,indent=2)+"\n")

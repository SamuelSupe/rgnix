#!/usr/bin/env python3
"""Keep release tags, package metadata and chart images on the same version."""
import os
import pathlib
import re
import sys
import tomllib

root = pathlib.Path(__file__).resolve().parents[1]
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?", version):
    raise SystemExit("invalid release version")
chart = (root / "charts/rgnix/Chart.yaml").read_text()
for key in ("version", "appVersion"):
    if not re.search(rf'^{key}: ["\']?{re.escape(version)}["\']?$', chart, re.M):
        raise SystemExit(f"Chart {key} must equal Cargo version {version}")
values = (root / "charts/rgnix/values.yaml").read_text()
if not re.search(rf'^  tag: ["\']?{re.escape(version)}["\']?$', values, re.M):
    raise SystemExit(f"Chart image tag must equal Cargo version {version}")
if os.environ.get("RELEASE_REF_TYPE") == "tag" and os.environ.get("RELEASE_REF") != "v" + version:
    raise SystemExit("release tag must match Cargo and Chart versions")
if sys.argv[1:2] == ["--prepare-chart"]:
    image, requested_version = sys.argv[2:]
    if requested_version != version or not re.fullmatch(r"ghcr.io/[a-z0-9_./-]+", image):
        raise SystemExit("invalid chart image or version")
    path = root / "charts/rgnix/values.yaml"
    values = path.read_text()
    values, count = re.subn(r'(?m)^  repository: .+$', "  repository: " + image, values, count=1)
    if count != 1:
        raise SystemExit("chart image repository is missing")
    values = re.sub(r'(?m)^  tag: .+$', f'  tag: "{version}"', values, count=1)
    path.write_text(values)
elif sys.argv[1:2] == ["--notes"]:
    print((root / "docs/releases" / f"v{version}.md").read_text())
else:
    print(version)

#!/usr/bin/env python3
"""Build a native rgnix binary using profiles collected from real traffic."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time


ROOT = Path(__file__).resolve().parent.parent


def source_digest():
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT
    ).decode().split("\0")
    digest = hashlib.sha256()
    for name in sorted(set(paths)):
        path = ROOT / name
        if path.is_file() and (path.suffix == ".rs" or path.name in ["Cargo.toml", "Cargo.lock"]
                               or name in [".cargo/config", ".cargo/config.toml"]):
            digest.update(name.encode() + b"\0" + path.read_bytes() + b"\0")
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["generate", "use"])
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path,
                        default=Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")))
    parser.add_argument("--features", default="hyper-experimental,jemalloc")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--llvm-profdata", help="must match rustc's LLVM major version")
    args = parser.parse_args()
    if os.environ.get("CARGO_BUILD_TARGET"):
        parser.error("PGO training must run natively; unset CARGO_BUILD_TARGET")
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    target = args.target_dir.resolve()
    profile_dir = work / "profiles"
    manifest_path = work / "pgo.json"
    rustc = os.environ.get("RUSTC", "rustc")
    compiler = subprocess.check_output([rustc, "-Vv"], text=True)
    source = source_digest()
    flags = {key: value for key, value in os.environ.items()
             if key in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "RUSTC_WRAPPER"]
             or key.startswith("CARGO_PROFILE_RELEASE_")
             or (key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS"))}
    if args.mode == "generate":
        if manifest_path.exists() or profile_dir.exists():
            parser.error("use a fresh work directory to avoid mixing training generations")
        manifest = {"compiler": compiler, "source_sha256": source,
                    "features": args.features, "environment": flags, "builds": []}
        rust_flags = ["-Cprofile-generate=" + str(profile_dir)]
    else:
        manifest = json.loads(manifest_path.read_text())
        if (manifest["compiler"], manifest["source_sha256"], manifest["features"], manifest["environment"]) != (
                compiler, source, args.features, flags):
            parser.error("compiler, source, features or release flags changed; collect fresh profiles")
        profiles = sorted(profile_dir.glob("*.profraw"))
        if not profiles or any(path.stat().st_size == 0 for path in profiles):
            parser.error("no complete training profiles; stop the instrumented server gracefully first")
        sysroot = Path(subprocess.check_output([rustc, "--print", "sysroot"], text=True).strip())
        bundled = list(sysroot.glob("lib/rustlib/*/bin/llvm-profdata"))
        tool = args.llvm_profdata or (str(bundled[0]) if bundled else shutil.which("llvm-profdata"))
        if not tool:
            parser.error("llvm-profdata unavailable; install rustup's llvm-tools component")
        version = subprocess.check_output([tool, "--version"], text=True)
        rust_llvm = re.search(r"LLVM version: (\d+)", compiler)
        tool_llvm = re.search(r"LLVM version (\d+)", version)
        if not rust_llvm or not tool_llvm or rust_llvm[1] != tool_llvm[1]:
            parser.error("llvm-profdata and rustc LLVM major versions differ")
        merged = work / "rgnix.profdata"
        subprocess.run([tool, "merge", "-o", str(merged), *map(str, profiles)], check=True)
        manifest["profiles"] = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in profiles}
        manifest["llvm_profdata"] = version
        manifest["merged_sha256"] = hashlib.sha256(merged.read_bytes()).hexdigest()
        rust_flags = ["-Cprofile-use=" + str(merged), "-Cllvm-args=-pgo-warn-missing-function"]
    command = ["cargo", "rustc", "--locked", "--release", "--features", args.features,
               "--bin", "rgnix", "--target-dir", str(target)]
    if args.offline:
        command.append("--offline")
    command.extend(["--", *rust_flags])
    begun = time.monotonic()
    with (work / ("pgo-" + args.mode + ".log")).open("w") as log:
        result = subprocess.run(command, cwd=ROOT, stdout=log, stderr=log)
    build = {"mode": args.mode, "command": command, "returncode": result.returncode,
             "seconds": time.monotonic() - begun}
    manifest["builds"].append(build)
    if result.returncode == 0:
        if source_digest() != source:
            raise RuntimeError("source changed during compilation; discard this build and train again")
        binary = work / ("rgnix-pgo-" + args.mode)
        shutil.copyfile(target / "release/rgnix", binary)
        binary.chmod(0o755)
        build.update(binary=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(build, indent=2), flush=True)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()

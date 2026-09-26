#!/usr/bin/env python3
"""Paired BPF_PROG_TEST_RUN costs, not NIC throughput or end-to-end latency."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import tempfile

from xdp_integration import frame, run


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--cpu", type=int, default=0)
    args = parser.parse_args()
    if os.geteuid() != 0 or not 1 <= args.rounds <= 30:
        parser.error("root required; rounds must be 1..30")
    binaries = {"baseline": args.binary.resolve()}
    if args.compare:
        binaries["candidate"] = args.compare.resolve()
    allow = 'function on_xdp() return xdp.pass("allow") end'
    sets = 'function on_xdp() if pkt.src_in_set("blocked") then return xdp.drop("blocked") end return xdp.pass("allow") end'
    rates = 'function on_xdp() if not xdp.allow("client", "src_ip", 1000000000, 1000000000) then return xdp.drop("limited") end return xdp.pass("allow") end'
    cases = {
        "pass": (allow, {}, "pass", "allow", frame()),
        "drop": ('function on_xdp() if pkt.is_udp() and pkt.dst_port() == 443 then return xdp.drop("udp") end return xdp.pass("allow") end', {}, "drop", "udp", frame(protocol=17)),
        "scope_one": (allow, {"scope": {"ports": [443], "protocols": [6]}}, "pass", "allow", frame()),
        "scope_first": (allow, {"scope": {"ports": [443] + list(range(1000, 1031)), "protocols": [6] + list(range(100, 115))}}, "pass", "allow", frame()),
        "scope_last": (allow, {"scope": {"ports": list(range(1000, 1031)) + [443], "protocols": list(range(100, 115)) + [6]}}, "pass", "allow", frame()),
        "scope_bypass": (allow, {"scope": {"ports": [80], "protocols": [6]}}, "pass", "scope_bypass", frame()),
        "set_miss": (sets, {"sets": {"blocked": [{"cidr": "192.0.2.0/24"}]}}, "pass", "allow", frame()),
        "set_hit": (sets, {"sets": {"blocked": [{"cidr": "198.51.100.0/24"}]}}, "drop", "blocked", frame()),
        "per_ip_bucket": (rates, {"ceiling_pps": 1000000000, "ceiling_burst": 1000000000}, "pass", "allow", frame()),
    }
    output = {"kernel": os.uname().release, "architecture": os.uname().machine, "cpu": args.cpu,
              "repeat": 100000, "rounds": args.rounds,
              "method": "Kernel-reported mean ns over repeated identical frames; fresh maps per run; warmed single key, no NIC or contention",
              "binaries": {name: {"sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "cases": {}}
                           for name, binary in binaries.items()}}
    with tempfile.TemporaryDirectory(prefix="rgnix-xdp-perf-") as temporary:
        root = Path(temporary)
        for case, (source, config, action, rule, packet) in cases.items():
            (root / f"{case}.rgl").write_text(source)
            (root / f"{case}.json").write_text(json.dumps(config))
            (root / f"{case}.bin").write_bytes(packet)
            for name, binary in binaries.items():
                obj = root / f"{name}-{case}.o"
                run(binary, "xdp", "compile", root / f"{case}.rgl", "-o", obj)
                output["binaries"][name]["cases"][case] = {"config": config, "source": source,
                    "object_sha256": hashlib.sha256(obj.read_bytes()).hexdigest(), "runs_ns": []}
        for index in range(args.rounds):
            variants = list(binaries.items())
            if index % 2:
                variants.reverse()
            for case, (_, _, action, rule, packet) in cases.items():
                for name, binary in variants:
                    result = json.loads(run("taskset", "-c", args.cpu, binary, "xdp", "test", root / f"{name}-{case}.o",
                        "--config", root / f"{case}.json", "--packet", root / f"{case}.bin", "--repeat", 100000).stdout)
                    assert result["action"] == action, result
                    count = next(item[action] for item in result["rules"] if item["rule"] == rule)
                    assert count["packets"] == 100000 and count["bytes"] == len(packet) * 100000, result
                    output["binaries"][name]["cases"][case]["runs_ns"].append(result["duration_ns"])
                    print(index + 1, name, case, result["duration_ns"], flush=True)
    for variant in output["binaries"].values():
        for case in variant["cases"].values():
            case["median_ns"] = statistics.median(case["runs_ns"])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2) + "\n")


if __name__ == "__main__":
    main()

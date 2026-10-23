#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
binary=$(realpath "${1:?usage: validate_artifact.sh /path/to/rgnix}")
results="${RGNIX_ARTIFACT_RESULTS:-.local/artifact-validation}"
mkdir -p "$results"
sha256sum "$binary" > "$results/SHA256SUMS"
readelf -h -d --version-info "$binary" > "$results/ELF-INFO.txt"
for engine in pingora hyper; do
  export RGNIX_ENGINE="$engine"
  export RGNIX_EXPERIMENTAL_HYPER=$([ "$engine" = hyper ] && echo true || echo false)
  for suite in integration ingress_recovery otlp_integration log_rotation product_features migration_integration global_rate_limit; do
    python3 "scripts/$suite.py" "$binary"
  done
  mkdir -p "$results/$engine"
  for report in acceptance ingress-recovery log-rotation product-features global-rate-results; do
    cp ".local/$report.json" "$results/$engine/"
  done
done
python3 scripts/hyper_integration.py "$binary"
"${RGNIX_PROTOCOL_PYTHON:-python3}" scripts/hyper_protocols.py "$binary" --http3

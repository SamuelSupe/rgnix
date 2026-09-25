#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --check
cargo test --locked -j "${CARGO_BUILD_JOBS:-2}"
cargo build --locked -j "${CARGO_BUILD_JOBS:-2}"
python3 scripts/integration.py "${CARGO_TARGET_DIR:-target}/debug/rgnix"
python3 scripts/ingress_recovery.py "${CARGO_TARGET_DIR:-target}/debug/rgnix"
python3 scripts/otlp_integration.py "${CARGO_TARGET_DIR:-target}/debug/rgnix"
python3 scripts/log_rotation.py "${CARGO_TARGET_DIR:-target}/debug/rgnix"
python3 scripts/product_features.py "${CARGO_TARGET_DIR:-target}/debug/rgnix"

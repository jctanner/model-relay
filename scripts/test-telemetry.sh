#!/usr/bin/env bash
set -euo pipefail
relay_workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$relay_workspace"
cargo build --locked --offline
python3 tests/probe_telemetry.py
if [[ "${1:-}" == "--stress" ]]; then
  shift
  python3 tests/stress_telemetry.py "$@"
fi

#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
export SHIM_CLIENT_API=messages
exec ./scripts/test-upstream.sh

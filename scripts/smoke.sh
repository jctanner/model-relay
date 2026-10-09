#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
session=${VERTEX_SHIM_SESSION:-dev}
network=${VERTEX_SHIM_NETWORK:-vertex-shim-${session}}
state_dir=${workspace}/.state/${session}
./scripts/dev.sh exec client claude-proxy -p 'Reply with a short greeting.' --tools '' --max-turns 1
podman run --rm --pull never --http-proxy=false --network "$network" \
    --userns keep-id:uid=1000,gid=1000 --user 1000:1000 \
    --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests:/tests:ro,z" \
    --volume "${state_dir}/proxy-api-key:/run/secrets/proxy-api-key:ro,z" \
    --volume "${state_dir}/admin-api-key:/run/secrets/admin-api-key:ro,z" \
    docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641 \
    python /tests/probe_stack.py

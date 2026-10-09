#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
session=runtime-$(date +%s)-$$
network=vertex-shim-${session}
container=vertex-shim-${session}-proxy
volume=vertex-shim-${session}-telemetry
# Generate an isolated config/key pair for the baked-image acceptance test.
VERTEX_SHIM_SESSION=$session SHIM_CLIENT_LAYOUT=single ./scripts/dev.sh init
state_dir=${workspace}/.state/${session}
cleanup() {
    trap - EXIT INT TERM
    podman rm --force "$container" >/dev/null 2>&1 || true
    podman network rm "$network" >/dev/null 2>&1 || true
    podman volume rm "$volume" >/dev/null 2>&1 || true
    rm -f "${state_dir}/proxy-api-key" "${state_dir}/admin-api-key"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
podman network create --internal "$network" >/dev/null
podman run --detach --pull never --http-proxy=false --name "$container" \
    --network "$network" --network-alias vertex-shim \
    --userns keep-id:uid=1000,gid=1000 --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${state_dir}/config:/config:ro,z" \
    --volume "${state_dir}/admin-api-key:/run/secrets/admin-api-key:ro,z" \
    --volume "${volume}:/data:U" --env SHIM_ADMIN_LISTEN=0.0.0.0:8081 \
    localhost/vertex-shim:0.1.0 >/dev/null
podman run --rm --pull never --http-proxy=false --network "$network" \
    --userns keep-id:uid=1000,gid=1000 \
    --read-only --cap-drop all --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,size=256m \
    --tmpfs /home/claude:rw,mode=1777,size=128m \
    --volume "${state_dir}/proxy-api-key:/run/secrets/proxy-api-key:ro,z" \
    --env ANTHROPIC_MODEL=claude-sonnet-4-5@20250929 \
    --env ANTHROPIC_DEFAULT_SONNET_MODEL=claude-sonnet-4-5@20250929 \
    --env ANTHROPIC_DEFAULT_OPUS_MODEL=claude-sonnet-4-5@20250929 \
    --env ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-sonnet-4-5@20250929 \
    localhost/vertex-shim-claude:dev claude-proxy -p 'Reply with a short greeting.' --tools '' --max-turns 1
podman run --rm --pull never --http-proxy=false --network "$network" \
    --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests:/tests:ro,z" \
    --volume "${state_dir}/proxy-api-key:/run/secrets/proxy-api-key:ro,z" \
    --volume "${state_dir}/admin-api-key:/run/secrets/admin-api-key:ro,z" \
    docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641 python /tests/probe_stack.py
echo "PASS: baked Rust runtime used with no source mounts"

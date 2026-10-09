#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
session=probe-$(date +%s)-$$
export VERTEX_SHIM_SESSION=$session
export VERTEX_SHIM_NETWORK=vertex-shim-${session}
export SHIM_CLIENT_WORK_DIR=''
image=${CLAUDE_CLIENT_IMAGE:-localhost/vertex-shim-claude:dev}
fixture_image=${FIXTURE_IMAGE:-docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641}
fixture=vertex-shim-${session}-fixture
client=vertex-shim-${session}-client
home_volume=vertex-shim-${session}-client-home
work_volume=vertex-shim-${session}-client-work
key_file=${workspace}/.state/${session}/proxy-api-key
export PROXY_API_KEY_FILE=$key_file

"${workspace}/scripts/client.sh" doctor
podman image exists "$image" || { echo "Build the pinned client image first." >&2; exit 1; }
podman pull "$fixture_image" >/dev/null
# Only resources with this invocation's unique session names are cleaned up.
cleanup() {
    trap - EXIT INT TERM
    if podman container exists "$fixture"; then
        podman logs "$fixture" > "${workspace}/.state/${session}/gateway.jsonl" 2>/dev/null || true
    fi
    podman rm --force "$client" "$fixture" >/dev/null 2>&1 || true
    podman network rm "$VERTEX_SHIM_NETWORK" >/dev/null 2>&1 || true
    podman volume rm "$home_volume" "$work_volume" >/dev/null 2>&1 || true
    # Delete only the synthetic key; preserve probe logs for review.
    rm -f -- "$key_file"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
"${workspace}/scripts/client.sh" init-key
podman network create --internal "$VERTEX_SHIM_NETWORK" >/dev/null
podman run --detach --pull never --http-proxy=false --name "$fixture" \
    --network "$VERTEX_SHIM_NETWORK" --network-alias vertex-shim \
    --userns keep-id:uid=1000,gid=1000 --user 1000:1000 \
    --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests/fixtures:/fixtures:ro,z" \
    --volume "${key_file}:/run/secrets/proxy-api-key:ro,z" \
    "$fixture_image" python /fixtures/vertex_gateway.py >/dev/null
"${workspace}/scripts/client.sh" service > "${workspace}/.state/${session}/client.log" 2>&1 &
service_pid=$!
ready=false
for _ in {1..60}; do
    if podman exec "$client" curl -fsS http://vertex-shim:8080/health >/dev/null 2>&1; then
        ready=true
        break
    fi
    if ! kill -0 "$service_pid" 2>/dev/null; then
        cat "${workspace}/.state/${session}/client.log" >&2
        exit 1
    fi
    sleep 1
done
[[ "$ready" == true ]] || { echo "Client/fixture readiness timed out." >&2; exit 1; }
podman exec "$client" claude --version
# A bounded, text-only probe on an internal network; no Google or OAuth files.
if ! timeout 90s podman exec "$client" claude-proxy -p 'Reply with a short greeting.' \
    --tools '' --max-turns 1 > "${workspace}/.state/${session}/response.txt"; then
    cat "${workspace}/.state/${session}/response.txt" >&2
    exit 1
fi
podman logs "$fixture" > "${workspace}/.state/${session}/gateway.jsonl"
python3 - "${workspace}/.state/${session}" <<'PY'
import json
import pathlib
import sys

results = pathlib.Path(sys.argv[1])
records = [json.loads(line) for line in (results / 'gateway.jsonl').read_text().splitlines()]
calls = [r for r in records if 'path' in r]
assert calls and all(r['authorized'] for r in calls), calls
assert any(r['path'].endswith(':streamRawPredict') for r in calls), calls
assert any(r.get('anthropic_version') == 'vertex-2023-10-16' for r in records), records
assert 'Greetings from vertex-shim.' in (results / 'response.txt').read_text()
print(f'PASS: native Claude Code used authenticated Vertex SSE routes; results: {results}')
PY
"${workspace}/scripts/client.sh" stop >/dev/null
if wait "$service_pid"; then
    :
else
    status=$?
    [[ "$status" == 143 ]] || exit "$status"
fi

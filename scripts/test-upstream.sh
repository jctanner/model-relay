#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
export VERTEX_SHIM_SESSION=upstream-$(date +%s)-$$
export SHIM_HOST_PORT=0 SHIM_HOST_ADMIN_PORT=0 SHIM_BACKEND=anthropic
export SHIM_CLIENT_WORK_DIR=''
export SHIM_CLIENT_LAYOUT=single
export SHIM_UPSTREAM_BASE_URL=http://upstream-fixture:8090 SHIM_UPSTREAM_SYNTHETIC=true
export SHIM_UPSTREAM_WORKSPACE=wrkspc_fixture SHIM_UPSTREAM_TIMEOUT_SECONDS=2
session=$VERTEX_SHIM_SESSION
network=vertex-shim-${session}
proxy=vertex-shim-${session}-proxy
fixture=vertex-shim-${session}-upstream
state_dir=${workspace}/.state/${session}
export SHIM_UPSTREAM_TOKEN_FILE=${state_dir}/upstream-token
python_image=docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641
launcher=''
cleanup() {
    trap - EXIT INT TERM
    if [[ -n "$launcher" ]]; then kill -TERM "$launcher" 2>/dev/null || true; wait "$launcher" || true; fi
    ./scripts/dev.sh stop >/dev/null 2>&1 || true
    podman rm --force "$fixture" >/dev/null 2>&1 || true
    podman network rm "$network" >/dev/null 2>&1 || true
    for role in client-home client-work telemetry build; do
        podman volume rm "vertex-shim-${session}-${role}" >/dev/null 2>&1 || true
    done
    rm -f "${state_dir}/proxy-api-key" "${state_dir}/admin-api-key" "${state_dir}/restricted-key" "$SHIM_UPSTREAM_TOKEN_FILE"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
./scripts/dev.sh init
python3 - "$state_dir" <<'PY'
import json, pathlib, secrets, sys
state = pathlib.Path(sys.argv[1])
(state / 'upstream-token').write_text('synthetic-upstream-' + secrets.token_urlsafe(32) + '\n')
(state / 'upstream-token').chmod(0o600)
model_file = state / 'config/models.json'
models = json.loads(model_file.read_text())
for model in models.values(): model['upstream_model'] = 'fixture-sonnet'
model_file.write_text(json.dumps(models))
PY
if [[ "${SHIM_CLIENT_API:-vertex}" == messages ]]; then
    python3 - "$state_dir" <<'PY'
import hashlib, json, pathlib, secrets, sys
state = pathlib.Path(sys.argv[1])
restricted = 'synthetic-restricted-' + secrets.token_urlsafe(32)
(state / 'restricted-key').write_text(restricted + '\n')
(state / 'restricted-key').chmod(0o600)
path = state / 'config/keys.json'
registry = json.loads(path.read_text())
registry['keys'][0]['catalog_models'] = True
registry['keys'].append({'client_id': 'restricted', 'key_id': 'restricted',
    'sha256': hashlib.sha256(restricted.encode()).hexdigest(),
    'models': ['claude-sonnet-4-5-20250929'], 'backends': ['anthropic']})
path.write_text(json.dumps(registry))
PY
fi
podman network create --internal --label "io.vertex-shim.session=$session" "$network" >/dev/null
podman run --detach --pull never --http-proxy=false --name "$fixture" \
    --network "$network" --network-alias upstream-fixture --userns keep-id:uid=1000,gid=1000 --user 1000:1000 \
    --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests:/tests:ro,z" \
    --volume "${SHIM_UPSTREAM_TOKEN_FILE}:/run/secrets/upstream-token:ro,z" \
    "$python_image" python -B /tests/fixtures/anthropic_upstream.py >/dev/null
./scripts/dev.sh start > "${state_dir}/stack.log" 2>&1 &
launcher=$!
for _ in {1..180}; do
    if podman exec "$proxy" curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
        && podman exec "vertex-shim-${session}-client" true >/dev/null 2>&1; then break; fi
    kill -0 "$launcher" 2>/dev/null || { cat "${state_dir}/stack.log" >&2; exit 1; }
    sleep 1
done
./scripts/dev.sh exec client test ! -e /run/secrets/upstream-token
./scripts/dev.sh exec client claude-proxy -p 'Reply with a short greeting.' --tools '' --max-turns 1
podman run --rm --pull never --http-proxy=false --network "$network" \
    --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests:/tests:ro,z" \
    --volume "${state_dir}/proxy-api-key:/run/secrets/proxy-api-key:ro,z" \
    --volume "${state_dir}/admin-api-key:/run/secrets/admin-api-key:ro,z" \
    --volume "${SHIM_UPSTREAM_TOKEN_FILE}:/run/secrets/upstream-token:rw,z" \
    "$python_image" python -B /tests/probe_upstream.py
if [[ "${SHIM_CLIENT_API:-vertex}" == messages ]]; then
    podman run --rm --pull never --http-proxy=false --network "$network" \
        --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only --cap-drop all --security-opt no-new-privileges \
        --volume "${workspace}/tests:/tests:ro,z" \
        --volume "${state_dir}/config:/config:rw,z" \
        --volume "${state_dir}/proxy-api-key:/run/secrets/proxy-api-key:ro,z" \
        --volume "${state_dir}/admin-api-key:/run/secrets/admin-api-key:ro,z" \
        --volume "${state_dir}/restricted-key:/run/secrets/restricted-key:ro,z" \
        --volume "${SHIM_UPSTREAM_TOKEN_FILE}:/run/secrets/upstream-token:rw,z" \
        "$python_image" python -B /tests/probe_messages.py
    podman exec "vertex-shim-${session}-client" bash -c \
        'jq -s -e "all(.[]; .modelPicker.replaceBuiltInOptions == true and (.modelPicker.options | length) == 3 and ([.modelPicker.options[].model] | unique | length) == 3 and .model == \"claude-sonnet-4-6\")" /tmp/vertex-shim-models.*.json >/dev/null'
fi
./scripts/dev.sh telemetry requests > "${state_dir}/logging-records.json"
# Exercise graceful process replacement so startup and shutdown are observed in
# the same real container log, without rebuilding an image or calling upstream.
./scripts/dev.sh restart
podman logs "$proxy" > "${state_dir}/proxy.log" 2>&1
python3 tests/probe_logging.py --log "${state_dir}/proxy.log" \
    --records "${state_dir}/logging-records.json" \
    --secret-file "${state_dir}/proxy-api-key" \
    --secret-file "${state_dir}/admin-api-key" \
    --secret-file "$SHIM_UPSTREAM_TOKEN_FILE"

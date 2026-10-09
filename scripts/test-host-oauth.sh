#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
export VERTEX_SHIM_SESSION=hostoauth-$(date +%s)-$$ SHIM_HOST_PORT=0 SHIM_HOST_ADMIN_PORT=0
export SHIM_CLIENT_WORK_DIR=''
export SHIM_CLIENT_LAYOUT=${SHIM_CLIENT_LAYOUT:-single} SHIM_VERTEX_WORK_DIR=''
export SHIM_BACKEND=anthropic SHIM_UPSTREAM_AUTH=claude-oauth SHIM_OAUTH_SOURCE=host SHIM_OAUTH_MODE=read-only
export SHIM_UPSTREAM_BASE_URL=http://upstream-fixture:8090 SHIM_UPSTREAM_SYNTHETIC=true SHIM_UPSTREAM_WORKSPACE=wrkspc_fixture
session=$VERTEX_SHIM_SESSION
state_dir=${workspace}/.state/${session}
export SHIM_OAUTH_DIR=${state_dir}/auth SHIM_HOST_OAUTH_FILE=${state_dir}/synthetic-host/.credentials.json
network=vertex-shim-${session}
proxy=vertex-shim-${session}-proxy
fixture=vertex-shim-${session}-oauth
python_image=docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641
launcher=''
cleanup() {
    trap - EXIT INT TERM
    if [[ -n "$launcher" ]]; then kill -TERM "$launcher" 2>/dev/null || true; wait "$launcher" || true; fi
    ./scripts/dev.sh stop >/dev/null 2>&1 || true
    podman logs "$fixture" > "${state_dir}/fixture.log" 2>&1 || true
    podman rm --force "$fixture" >/dev/null 2>&1 || true
    podman network rm "$network" >/dev/null 2>&1 || true
    for role in client-home client-work vertex-client-home vertex-client-work telemetry build; do podman volume rm "vertex-shim-${session}-${role}" >/dev/null 2>&1 || true; done
    rm -f "${state_dir}/proxy-api-key" "${state_dir}/admin-api-key" "${state_dir}/upstream-token" "${state_dir}/initial-refresh" "$SHIM_HOST_OAUTH_FILE" "${SHIM_OAUTH_DIR}/.credentials.json"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
podman run --rm --pull never --http-proxy=false --network none \
    --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only \
    --tmpfs /tmp:rw,nosuid,nodev,size=32m --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/scripts:/scripts:ro,z" --volume "${workspace}/tests:/tests:ro,z" \
    "$python_image" python -B /tests/test_oauth_mirror.py
./scripts/dev.sh init
python3 - "$state_dir" <<'PY'
import json, pathlib, secrets, sys, time
state = pathlib.Path(sys.argv[1])
(state / 'synthetic-host').mkdir(mode=0o700)
source = state / 'synthetic-host/.credentials.json'
source.write_text(json.dumps({'claudeAiOauth': {'accessToken': 'synthetic-host-access', 'refreshToken': 'synthetic-host-refresh',
    'expiresAt': int(time.time() * 1000) + 3600000, 'scopes': ['user:inference', 'user:profile']}, 'otherAuth': 'never_export_other_auth'}))
source.chmod(0o600)
for name, value in [('upstream-token', 'synthetic-host-access'), ('initial-refresh', 'synthetic-host-refresh')]:
    (state / name).write_text(value + '\n')
    (state / name).chmod(0o600)
(state / 'control').mkdir()
(state / 'control/mode').write_text('invalid_grant')
models = state / 'config/models.json'
value = json.loads(models.read_text())
for model in value.values(): model['upstream_model'] = 'fixture-sonnet'
models.write_text(json.dumps(value))
PY
podman network create --internal --label "io.vertex-shim.session=$session" "$network" >/dev/null
podman run --detach --pull never --http-proxy=false --name "$fixture" \
    --network "$network" --network-alias upstream-fixture --userns keep-id:uid=1000,gid=1000 --user 1000:1000 \
    --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests:/tests:ro,z" --volume "${state_dir}/control:/control:ro,z" \
    --volume "${state_dir}/initial-refresh:/run/secrets/initial-refresh:ro,z" \
    --volume "${state_dir}/upstream-token:/run/secrets/upstream-token:ro,z" \
    "$python_image" python -B /tests/fixtures/claude_oauth.py >/dev/null
./scripts/dev.sh start > "${state_dir}/stack.log" 2>&1 &
launcher=$!
for _ in {1..180}; do
    if podman exec "$proxy" curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 && podman exec "vertex-shim-${session}-client" true >/dev/null 2>&1; then break; fi
    kill -0 "$launcher" 2>/dev/null || { cat "${state_dir}/stack.log" >&2; exit 1; }
    sleep 1
done
./scripts/dev.sh exec client test ! -e /auth/.credentials.json
if podman exec "$proxy" test -w /auth; then echo 'Proxy auth mount must be read-only' >&2; exit 1; fi
./scripts/dev.sh exec client claude-proxy -p 'Reply with a short greeting.' --tools '' --max-turns 1
if [[ "$SHIM_CLIENT_LAYOUT" == dual ]]; then
    ./scripts/dev.sh exec vertex-client claude-proxy -p 'Reply with a short greeting.' --tools '' --max-turns 1
    ./scripts/dev.sh exec vertex-client claude-proxy -p 'Reply with a short greeting.' --model=claude-opus-4-6 --tools '' --max-turns 1
    podman run --rm --pull never --http-proxy=false --network "$network" \
        --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only --cap-drop all --security-opt no-new-privileges \
        --volume "${workspace}/tests:/tests:ro,z" --volume "${state_dir}:/probe:rw,z" \
        "$python_image" python -B /tests/probe_vertex_catalog.py
fi
podman run --rm --pull never --http-proxy=false --network "$network" \
    --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests:/tests:ro,z" --volume "${state_dir}/synthetic-host:/synthetic-host:rw,z" \
    --volume "${SHIM_OAUTH_DIR}:/mirror:ro,z" --volume "${state_dir}/upstream-token:/run/secrets/upstream-token:rw,z" \
    --volume "${state_dir}/proxy-api-key:/run/secrets/proxy-api-key:ro,z" \
    --volume "${state_dir}/admin-api-key:/run/secrets/admin-api-key:ro,z" \
    "$python_image" python -B /tests/probe_host_oauth.py
# Losing the credential watcher must stop inference/client, rather than serve a
# stale mirror. Target only the dedicated child process owned by this launcher.
auth_pid=$(pgrep -P "$launcher" -f honcho)
mirror_pid=$(pgrep -f "oauth_mirror.py --source ${SHIM_HOST_OAUTH_FILE}")
[[ -n "$auth_pid" && "$mirror_pid" =~ ^[0-9]+$ ]] || { echo 'Could not identify supervised watcher' >&2; exit 1; }
kill -TERM "$mirror_pid"
for _ in {1..30}; do
    if ! podman container exists "$proxy" && ! podman container exists "vertex-shim-${session}-client"; then break; fi
    sleep 1
done
! podman container exists "$proxy"
! podman container exists "vertex-shim-${session}-client"
! podman container exists "vertex-shim-${session}-vertex-client"
! test -e "${SHIM_OAUTH_DIR}/.credentials.json"
wait "$launcher" || true; launcher=''
echo 'PASS: read-only host-login reuse, atomic rotation/logout, and watcher-failure cleanup'

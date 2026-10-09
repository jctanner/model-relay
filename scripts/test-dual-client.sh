#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
export VERTEX_SHIM_SESSION=dual-$(date +%s)-$$
export SHIM_CLIENT_LAYOUT=dual SHIM_BACKEND=mock SHIM_HOST_PORT=0 SHIM_HOST_ADMIN_PORT=0
export SHIM_MOCK_ADC_KEY_ID=vertex-inference
unset SHIM_CLIENT_EGRESS_NETWORK
session=$VERTEX_SHIM_SESSION
state_dir=${workspace}/.state/${session}
export SHIM_CLIENT_WORK_DIR=${state_dir}/anthropic-work SHIM_VERTEX_WORK_DIR=${state_dir}/vertex-work
proxy=vertex-shim-${session}-proxy
client=vertex-shim-${session}-client
vertex=vertex-shim-${session}-vertex-client
python_image=docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641
launcher=''
cleanup() {
    trap - EXIT INT TERM
    if [[ -n "$launcher" ]]; then kill -TERM "$launcher" 2>/dev/null || true; wait "$launcher" || true; fi
    ./scripts/dev.sh stop >/dev/null 2>&1 || true
    for role in client-home vertex-client-home telemetry build; do
        podman volume rm "vertex-shim-${session}-${role}" >/dev/null 2>&1 || true
    done
    rm -f "${state_dir}/proxy-api-key" "${state_dir}/admin-api-key"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
./scripts/dev.sh init
python3 - "$state_dir" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1]) / 'config/models.json'
models = json.loads(path.read_text())
# This upstream catalog entry has no explicit client grant or canonical route deployment.
models['fixture-opus-deployment'] = {'window_tokens': 1000000, 'upstream_model': 'claude-opus-4-6'}
path.write_text(json.dumps(models))
PY
start() {
    ./scripts/dev.sh start >> "${state_dir}/stack.log" 2>&1 &
    launcher=$!
    for _ in {1..180}; do
        if podman exec "$client" curl -fsS http://vertex-shim:8080/health >/dev/null 2>&1 \
            && podman exec "$vertex" curl -fsS http://vertex-shim:8082/health >/dev/null 2>&1; then return; fi
        kill -0 "$launcher" 2>/dev/null || { cat "${state_dir}/stack.log" >&2; exit 1; }
        sleep 1
    done
    echo 'Dual-client startup timed out' >&2; exit 1
}
wait_for_stop() {
    for _ in {1..30}; do
        if ! podman container exists "$client" && ! podman container exists "$vertex" \
            && ! podman container exists "$proxy" && ! podman network exists "vertex-shim-${session}"; then
            wait "$launcher" || true; launcher=''; return
        fi
        sleep 1
    done
    echo 'Dual-client shutdown left transient resources' >&2; exit 1
}
start
[[ "$(podman ps --filter "label=io.vertex-shim.session=${session}" --format '{{.Names}}' | wc -l)" -eq 3 ]]
./scripts/dev.sh exec vertex-client test ! -e /run/secrets/proxy-api-key
./scripts/dev.sh exec vertex-client test ! -e /auth/.credentials.json
./scripts/dev.sh exec anthropic-client test ! -e /auth/.credentials.json
[[ -z "$(podman port "$proxy" 8082/tcp 2>/dev/null || true)" ]]
# Each client has a different writable home and work directory.
./scripts/dev.sh exec anthropic-client bash -c 'printf anthropic > /work/identity; touch "$HOME/anthropic-only"'
./scripts/dev.sh exec vertex-client test ! -e /work/identity
./scripts/dev.sh exec vertex-client test ! -e /home/claude/anthropic-only
./scripts/dev.sh exec vertex-client bash -c 'printf vertex > /work/identity; touch "$HOME/vertex-only"'
./scripts/dev.sh exec anthropic-client test ! -e /home/claude/vertex-only
[[ "$(cat "$SHIM_CLIENT_WORK_DIR/identity")" == anthropic && "$(cat "$SHIM_VERTEX_WORK_DIR/identity")" == vertex ]]
./scripts/dev.sh exec anthropic-client claude-proxy -p 'Say hello.' --tools '' --max-turns 1 > "${state_dir}/anthropic-response.txt" &
anthropic_pid=$!
./scripts/dev.sh exec vertex-client claude-proxy -p 'Say hello.' --tools '' --max-turns 1 > "${state_dir}/vertex-response.txt" &
vertex_pid=$!
wait "$anthropic_pid"
wait "$vertex_pid"
./scripts/dev.sh exec vertex-client claude-proxy -p 'Say hello.' --model=claude-haiku-5-5 --tools '' --max-turns 1 > "${state_dir}/haiku-response.txt"
./scripts/dev.sh exec vertex-client claude-proxy -p 'Say hello.' --model=claude-opus-4-6 --tools '' --max-turns 1 > "${state_dir}/opus-response.txt"
./scripts/dev.sh exec vertex-client claude-proxy -p 'Say hello.' --model claude-sonnet-4-5-20250929 --tools '' --max-turns 1 > "${state_dir}/dated-response.txt"
if ./scripts/dev.sh exec vertex-client claude-proxy -p 'Say hello.' --model=unauthorized-model --tools '' --max-turns 1 > "${state_dir}/unauthorized.stdout" 2> "${state_dir}/unauthorized.stderr"; then
    echo 'Unsupported Vertex model unexpectedly accepted' >&2; exit 1
else
    [[ "$?" -eq 2 ]]
fi
podman run --rm --pull never --http-proxy=false --network "vertex-shim-${session}" \
    --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only --cap-drop all --security-opt no-new-privileges \
    --volume "${workspace}/tests:/tests:ro,z" --volume "${state_dir}:/probe:rw,z" \
    "$python_image" python -B /tests/probe_dual_client.py
before=$(./scripts/dev.sh telemetry summary)
./scripts/dev.sh restart >/dev/null
[[ "$(./scripts/dev.sh telemetry summary)" == "$before" ]]
podman logs "$proxy" > "${state_dir}/proxy.log" 2>&1
python3 - "$state_dir" <<'PY'
import json, pathlib, sys
state = pathlib.Path(sys.argv[1])
for role in ('anthropic', 'vertex'):
    assert 'Greetings from vertex-shim.' in (state / f'{role}-response.txt').read_text()
assert 'Greetings from vertex-shim.' in (state / 'haiku-response.txt').read_text()
assert 'Greetings from vertex-shim.' in (state / 'opus-response.txt').read_text()
assert 'Greetings from vertex-shim.' in (state / 'dated-response.txt').read_text()
assert 'Unsupported or unauthorized Vertex model' in (state / 'unauthorized.stderr').read_text()
assert 'Google Cloud authentication failed' not in (state / 'unauthorized.stderr').read_text()
log = (state / 'proxy.log').read_text()
for name in ('proxy-api-key', 'admin-api-key'):
    assert (state / name).read_text().strip() not in log
assert 'mock-gcp-' not in log
print('PASS: both native clients concurrently, separate homes/bind mounts, and stdout privacy')
PY
# Any failed client terminates all three supervised containers.
podman stop --time 10 "$vertex" >/dev/null
wait_for_stop
start
[[ "$(./scripts/dev.sh telemetry summary)" == "$before" ]]
./scripts/dev.sh exec anthropic-client test -e /home/claude/anthropic-only
./scripts/dev.sh exec vertex-client test -e /home/claude/vertex-only
kill -TERM "$launcher"
wait_for_stop
echo "PASS: dual-client lifecycle, process restart, persistent telemetry/homes, and cleanup; artifacts: $state_dir"

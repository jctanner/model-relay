#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
export VERTEX_SHIM_SESSION=mockadc-$(date +%s)-$$
export SHIM_CLIENT_API=vertex SHIM_CLIENT_AUTH=mock-adc SHIM_CLIENT_WORK_DIR=''
export SHIM_CLIENT_LAYOUT=single
export SHIM_BACKEND=mock SHIM_HOST_PORT=0 SHIM_HOST_ADMIN_PORT=0
export SHIM_MOCK_ADC_KEY_ID=dev-inference SHIM_MOCK_ADC_PROJECT=vertex-shim-local
export SHIM_MOCK_ADC_TOKEN_TTL_SECONDS=600
export SHIM_MOCK_ADC_HOST=vertex-shim:8082
session=$VERTEX_SHIM_SESSION
proxy=vertex-shim-${session}-proxy
client=vertex-shim-${session}-client
state_dir=${workspace}/.state/${session}
python_image=docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641
launcher=''
cleanup() {
    trap - EXIT INT TERM
    if [[ -n "$launcher" ]]; then kill -TERM "$launcher" 2>/dev/null || true; wait "$launcher" || true; fi
    ./scripts/dev.sh stop >/dev/null 2>&1 || true
    for role in client-home client-work telemetry build; do podman volume rm "vertex-shim-${session}-${role}" >/dev/null 2>&1 || true; done
    rm -f "${state_dir}/proxy-api-key" "${state_dir}/admin-api-key"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
start() {
    ./scripts/dev.sh start >> "${state_dir}/stack.log" 2>&1 &
    launcher=$!
    for _ in {1..180}; do
        if podman exec "$client" curl -fsS http://vertex-shim:8080/health >/dev/null 2>&1 \
            && podman exec "$client" curl -fsS http://vertex-shim:8082/health >/dev/null 2>&1; then return; fi
        kill -0 "$launcher" 2>/dev/null || { cat "${state_dir}/stack.log" >&2; exit 1; }
        sleep 1
    done
    echo 'Mock ADC stack startup timed out' >&2; exit 1
}
probe() {
    podman run --rm --pull never --http-proxy=false --network "vertex-shim-${session}" \
        --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only --cap-drop all --security-opt no-new-privileges \
        --volume "${workspace}/tests:/tests:ro,z" --volume "${state_dir}:/probe:rw,z" \
        "$python_image" python -B /tests/probe_mock_adc.py "$1"
}
./scripts/dev.sh init
start
./scripts/dev.sh exec client test ! -e /run/secrets/proxy-api-key
./scripts/dev.sh exec client test ! -e /auth/.credentials.json
[[ -z "$(podman port "$proxy" 8082/tcp 2>/dev/null || true)" ]]
python3 tests/probe_mock_adc_native.py "$client" "${state_dir}/native-reuse.jsonl" 0
probe grants
# Recreate with a short token lifetime to exercise actual expiry and renewal.
./scripts/dev.sh stop
wait "$launcher" || true; launcher=''
export SHIM_MOCK_ADC_TOKEN_TTL_SECONDS=2
start
python3 tests/probe_mock_adc_native.py "$client" "${state_dir}/native-renewal.jsonl" 3
probe expiry
podman logs "$proxy" > "${state_dir}/renewal-proxy.log" 2>&1
python3 - "$state_dir" <<'PY'
import json, pathlib, sys
state = pathlib.Path(sys.argv[1])
records = [json.loads(line) for line in (state / 'renewal-proxy.log').read_text().splitlines() if line.startswith('{')]
assert sum(r['event'] == 'mock_adc_token_issued' for r in records) >= 3
assert sum(r['event'] == 'request_completed' and r['operation'] == 'stream_generation' and r['http_status'] == 200 for r in records) == 2
assert 'mock-gcp-' not in (state / 'renewal-proxy.log').read_text()
print('PASS: native renewal after expiry, container recreation, and no token values in stdout')
PY
echo "PASS: mock ADC acceptance; artifacts: $state_dir"

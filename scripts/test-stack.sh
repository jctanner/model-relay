#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
export VERTEX_SHIM_SESSION=accept-$(date +%s)-$$
export SHIM_HOST_PORT=0 SHIM_HOST_ADMIN_PORT=0
export SHIM_CLIENT_WORK_DIR=''
export SHIM_CLIENT_LAYOUT=single
session=$VERTEX_SHIM_SESSION
proxy=vertex-shim-${session}-proxy
client=vertex-shim-${session}-client
state_dir=${workspace}/.state/${session}
launcher=''
cleanup() {
    trap - EXIT INT TERM
    if [[ -n "$launcher" ]]; then kill -TERM "$launcher" 2>/dev/null || true; wait "$launcher" || true; fi
    ./scripts/dev.sh stop >/dev/null 2>&1 || true
    for role in client-home client-work telemetry build; do
        podman volume rm "vertex-shim-${session}-${role}" >/dev/null 2>&1 || true
    done
    rm -f "${state_dir}/proxy-api-key" "${state_dir}/admin-api-key"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start() {
    mkdir -p "$state_dir"
    ./scripts/dev.sh start >> "${state_dir}/stack.log" 2>&1 &
    launcher=$!
    for _ in {1..180}; do
        if podman exec "$proxy" curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 && podman exec "$client" true >/dev/null 2>&1; then return; fi
        kill -0 "$launcher" 2>/dev/null || { cat "${state_dir}/stack.log" >&2; exit 1; }
        sleep 1
    done
    echo "Stack startup timed out" >&2; exit 1
}
wait_for_stop() {
    for _ in {1..30}; do
        if ! podman container exists "$client" && ! podman container exists "$proxy" && ! podman network exists "vertex-shim-${session}"; then
            wait "$launcher" || true
            launcher=''
            return
        fi
        sleep 1
    done
    echo "Stack shutdown left transient resources" >&2; exit 1
}

start
./scripts/smoke.sh
before=$(./scripts/dev.sh telemetry summary)
container_id=$(podman inspect "$proxy" --format '{{.Id}}')
./scripts/dev.sh restart
[[ "$(podman inspect "$proxy" --format '{{.Id}}')" == "$container_id" ]]
[[ "$(./scripts/dev.sh telemetry summary)" == "$before" ]]
# Unexpected client exit must stop the other service and remove the network.
podman stop --time 10 "$client" >/dev/null
wait_for_stop
# Recreate both containers using the preserved SQLite volume.
start
[[ "$(./scripts/dev.sh telemetry summary)" == "$before" ]]
kill -TERM "$launcher"
wait_for_stop
echo "PASS: process restart, service-failure cleanup, container recreation/persistence, and launcher shutdown"

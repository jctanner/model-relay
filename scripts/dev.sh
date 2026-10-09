#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
session=${VERTEX_SHIM_SESSION:-dev}
[[ "$session" =~ ^[a-z][a-z0-9-]{0,39}$ ]] || { echo "Invalid VERTEX_SHIM_SESSION" >&2; exit 2; }
export VERTEX_SHIM_SESSION=$session
export VERTEX_SHIM_NETWORK=${VERTEX_SHIM_NETWORK:-vertex-shim-${session}}
proxy=vertex-shim-${session}-proxy
client=vertex-shim-${session}-client
vertex_client=vertex-shim-${session}-vertex-client
layout=${SHIM_CLIENT_LAYOUT:-dual}
[[ "$layout" == dual || "$layout" == single ]] || { echo "Unknown SHIM_CLIENT_LAYOUT" >&2; exit 2; }
export SHIM_CLIENT_LAYOUT=$layout
state_dir=${workspace}/.state/${session}
image=${SHIM_DEV_IMAGE:-localhost/vertex-shim-dev:1.90}
client_image=${CLAUDE_CLIENT_IMAGE:-localhost/vertex-shim-claude:dev}
backend=${SHIM_BACKEND:-mock}
[[ "$backend" == mock || "$backend" == anthropic ]] || { echo "Unknown SHIM_BACKEND" >&2; exit 2; }
export SHIM_BACKEND=$backend
if [[ "$layout" == dual ]]; then
    export SHIM_CLIENT_API=messages SHIM_CLIENT_AUTH=api-key
    export SHIM_MOCK_ADC_KEY_ID=${SHIM_MOCK_ADC_KEY_ID:-vertex-inference}
fi
client_auth=${SHIM_CLIENT_AUTH:-api-key}
[[ "$client_auth" == api-key || "$client_auth" == mock-adc || "$client_auth" == mock-federated ]] || { echo "Unknown SHIM_CLIENT_AUTH" >&2; exit 2; }
if [[ "$client_auth" == mock-adc || "$client_auth" == mock-federated ]]; then
    [[ "${SHIM_CLIENT_API:-vertex}" == vertex ]] || { echo "Mock ADC requires SHIM_CLIENT_API=vertex" >&2; exit 2; }
    export SHIM_MOCK_ADC_KEY_ID=${SHIM_MOCK_ADC_KEY_ID:-dev-inference}
fi
export PROXY_API_KEY_FILE=${PROXY_API_KEY_FILE:-${state_dir}/proxy-api-key}
inference_port=${SHIM_HOST_PORT:-8080}
admin_port=${SHIM_HOST_ADMIN_PORT:-8081}
publish_inference=$inference_port
publish_admin=$admin_port
if [[ "$inference_port" == 0 ]]; then publish_inference=''; fi
if [[ "$admin_port" == 0 ]]; then publish_admin=''; fi

initialize() {
    mkdir -p "$state_dir/config"
    chmod 700 "$state_dir"
    [[ -f "$PROXY_API_KEY_FILE" ]] || ./scripts/client.sh init-key
    python3 - "$state_dir" "$PROXY_API_KEY_FILE" "$backend" "${SHIM_MOCK_ADC_KEY_ID:-}" "$layout" <<'PY'
import hashlib, json, os, pathlib, secrets, sys
state = pathlib.Path(sys.argv[1])
key_file = pathlib.Path(sys.argv[2])
admin = state / 'admin-api-key'
if not admin.exists():
    fd = os.open(admin, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as stream:
        stream.write(secrets.token_urlsafe(32) + '\n')
models = state / 'config/models.json'
if not models.exists():
    models.write_text(pathlib.Path('config/models.json').read_text())
keys = state / 'config/keys.json'
if not keys.exists():
    key = key_file.read_text().strip()
    entry = {'client_id': 'claude-code-dev', 'key_id': (sys.argv[4] if sys.argv[5] == 'single' else '') or 'dev-inference',
        'sha256': hashlib.sha256(key.encode()).hexdigest(), 'models': list(json.loads(models.read_text())),
        'backends': [sys.argv[3]]}
    if sys.argv[5] == 'dual':
        entry['catalog_models'] = True
    elif sys.argv[4]:
        entry['mock_adc'] = True
    keys.write_text(json.dumps({'keys': [entry]}, indent=2) + '\n')
    keys.chmod(0o600)
if sys.argv[5] == 'dual':
    registry = json.loads(keys.read_text())
    adc_id = sys.argv[4]
    matching = [entry for entry in registry['keys'] if entry['key_id'] == adc_id]
    if matching:
        if (len(matching) != 1 or matching[0]['client_id'] != 'claude-code-vertex'
                or not matching[0].get('mock_adc') or sys.argv[3] not in matching[0]['backends']):
            raise SystemExit('Existing mock ADC identity conflicts with dual-client profile; use a fresh session.')
    else:
        # This verifier permits revocation/rotation without mounting a key in the Vertex client.
        registry['keys'].append({'client_id': 'claude-code-vertex', 'key_id': adc_id,
            'sha256': hashlib.sha256(secrets.token_bytes(32)).hexdigest(),
            'models': list(json.loads(models.read_text())), 'backends': [sys.argv[3]],
            'mock_adc': True, 'vertex_catalog_models': True})
        temporary = keys.with_suffix('.json.next')
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, 'w') as stream:
            stream.write(json.dumps(registry, indent=2) + '\n')
        os.replace(temporary, keys)
PY
}

cleanup() {
    trap - EXIT INT TERM
    for name in "$vertex_client" "$client" "$proxy"; do
        if podman container exists "$name"; then
            owner=$(podman inspect "$name" --format '{{index .Config.Labels "io.vertex-shim.session"}}' 2>/dev/null) || owner=''
            [[ "$owner" != "$session" ]] || podman stop --time 10 "$name" >/dev/null 2>&1 || true
        fi
    done
    if podman network exists "$VERTEX_SHIM_NETWORK"; then
        owner=$(podman network inspect "$VERTEX_SHIM_NETWORK" --format '{{index .Labels "io.vertex-shim.session"}}' 2>/dev/null) || owner=''
        [[ "$owner" != "$session" ]] || podman network rm "$VERTEX_SHIM_NETWORK" >/dev/null 2>&1 || true
    fi
}

case ${1:-help} in
    build-images)
        ./scripts/client.sh doctor
        ./scripts/client.sh build "${CLAUDE_CODE_VERSION:-2.1.293}"
        podman build --file containers/Containerfile.dev --tag "$image" .
        ;;
    init)
        initialize
        ;;
    start)
        ./scripts/client.sh doctor
        [[ -x .venv/bin/honcho ]] || { echo "Install requirements-dev.txt into .venv first." >&2; exit 1; }
        podman image exists "$image" && podman image exists "$client_image" || { echo "Run scripts/dev.sh build-images first." >&2; exit 1; }
        if podman container exists "$proxy" || podman container exists "$client" || podman container exists "$vertex_client"; then
            echo "Session containers already exist; choose a new VERTEX_SHIM_SESSION or stop this session." >&2; exit 1
        fi
        initialize
        exec 9>"${state_dir}/session.lock"
        flock -n 9 || { echo "This session already has a running launcher." >&2; exit 1; }
        podman network exists "$VERTEX_SHIM_NETWORK" || podman network create --internal --label "io.vertex-shim.session=${session}" "$VERTEX_SHIM_NETWORK" >/dev/null
        podman volume create --ignore "vertex-shim-${session}-telemetry" >/dev/null
        podman volume create --ignore "vertex-shim-${session}-build" >/dev/null
        podman volume create --ignore vertex-shim-cargo-1-90 >/dev/null
        trap cleanup EXIT
        trap 'exit 130' INT
        trap 'exit 143' TERM
        procfile=Procfile.dev
        if [[ "$layout" == single ]]; then procfile=Procfile.single; fi
        if [[ "$backend" == anthropic && "${SHIM_UPSTREAM_AUTH:-bearer-file}" == claude-oauth && "${SHIM_OAUTH_SOURCE:-host}" == host ]]; then
            [[ "${SHIM_OAUTH_MODE:-read-only}" == read-only ]] || { echo "Host OAuth mirrors must use read-only mode." >&2; exit 2; }
            ./scripts/oauth.sh sync
            procfile=Procfile.oauth
            if [[ "$layout" == single ]]; then procfile=Procfile.single-oauth; fi
        fi
        .venv/bin/honcho -f "$procfile" start -e /dev/null &
        honcho_pid=$!
        ready=false
        for _ in {1..180}; do
            if podman exec "$proxy" curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
                && podman exec "$client" curl -fsS http://vertex-shim:8080/health >/dev/null 2>&1; then
                if [[ "$layout" == single ]] || { podman exec "$vertex_client" curl -fsS http://vertex-shim:8080/health >/dev/null 2>&1 \
                    && podman exec "$vertex_client" curl -fsS http://vertex-shim:8082/health >/dev/null 2>&1; }; then
                    ready=true; break
                fi
            fi
            kill -0 "$honcho_pid" 2>/dev/null || { wait "$honcho_pid"; exit 1; }
            sleep 1
        done
        if [[ "$ready" != true ]]; then echo "Stack readiness timed out." >&2; kill -TERM "$honcho_pid"; exit 1; fi
        echo "Ready: inference http://$(podman port "$proxy" 8080/tcp)/v1; admin http://$(podman port "$proxy" 8081/tcp)/api/private/v1"
        wait "$honcho_pid"
        ;;
    service)
        case ${2:-} in
            client) exec ./scripts/client.sh service ;;
            vertex-client)
                export SHIM_CLIENT_ROLE=vertex SHIM_CLIENT_API=vertex SHIM_CLIENT_AUTH=mock-adc
                exec ./scripts/client.sh service
                ;;
            proxy)
                cid_dir=$(mktemp -d); cid_file=${cid_dir}/proxy.cid
                service_cleanup() {
                    trap - EXIT INT TERM
                    if [[ -s "$cid_file" ]]; then podman stop --time 10 "$(cat "$cid_file")" >/dev/null 2>&1 || true; fi
                    rm -f "$cid_file"; rmdir "$cid_dir"
                }
                trap service_cleanup EXIT
                trap 'exit 130' INT
                trap 'exit 143' TERM
                upstream_args=(--env "SHIM_BACKEND=$backend")
                if [[ -n "${SHIM_UPSTREAM_PROFILES_DIR:-}" ]]; then
                    profiles_dir=$(realpath -- "$SHIM_UPSTREAM_PROFILES_DIR")
                    [[ -f "$profiles_dir/profiles.json" ]] || { echo 'Missing profiles.json in SHIM_UPSTREAM_PROFILES_DIR' >&2; exit 2; }
                    upstream_args+=(--volume "${profiles_dir}:/upstreams:ro,z" --env SHIM_UPSTREAM_PROFILES_FILE=/upstreams/profiles.json)
                fi
                if [[ -n "${SHIM_MOCK_ADC_KEY_ID:-}" ]]; then
                    upstream_args+=(--env "SHIM_MOCK_ADC_KEY_ID=$SHIM_MOCK_ADC_KEY_ID"
                        --env "SHIM_MOCK_ADC_PROJECT=${SHIM_MOCK_ADC_PROJECT:-vertex-shim-local}"
                        --env "SHIM_MOCK_ADC_TOKEN_TTL_SECONDS=${SHIM_MOCK_ADC_TOKEN_TTL_SECONDS:-600}"
                        --env SHIM_MOCK_ADC_LISTEN=0.0.0.0:8082)
                fi
                if [[ "$backend" == anthropic ]]; then
                    credential_mode=${SHIM_UPSTREAM_AUTH:-bearer-file}
                    upstream_args+=(--env "SHIM_UPSTREAM_AUTH=$credential_mode"
                        --env "SHIM_UPSTREAM_BASE_URL=${SHIM_UPSTREAM_BASE_URL:?Set SHIM_UPSTREAM_BASE_URL}"
                        --env "SHIM_UPSTREAM_SYNTHETIC=${SHIM_UPSTREAM_SYNTHETIC:-false}"
                        --env "SHIM_UPSTREAM_TIMEOUT_SECONDS=${SHIM_UPSTREAM_TIMEOUT_SECONDS:-120}")
                    case "$credential_mode" in
                        bearer-file)
                            [[ -f "${SHIM_UPSTREAM_TOKEN_FILE:-}" ]] || { echo "SHIM_UPSTREAM_TOKEN_FILE must name a readable token file." >&2; exit 2; }
                            upstream_file=$(realpath "$SHIM_UPSTREAM_TOKEN_FILE")
                            upstream_args+=(--volume "${upstream_file}:/run/secrets/upstream-token:ro,z" --env SHIM_UPSTREAM_TOKEN_FILE=/run/secrets/upstream-token)
                            ;;
                        claude-oauth)
                            [[ "${SHIM_OAUTH_MODE:-read-only}" == read-only ]] || { echo "Claude OAuth credentials are read-only; host CLI owns refresh." >&2; exit 2; }
                            auth_dir=${SHIM_OAUTH_DIR:-${state_dir}/auth}
                            [[ -d "$auth_dir" ]] || { echo "Read-only OAuth mirror is missing." >&2; exit 2; }
                            auth_dir=$(realpath "$auth_dir")
                            upstream_args+=(--volume "${auth_dir}:/auth:ro,z" --env SHIM_OAUTH_DIR=/auth)

                            ;;
                        *) echo "Unknown SHIM_UPSTREAM_AUTH" >&2; exit 2 ;;
                    esac
                    if [[ -n "${SHIM_UPSTREAM_WORKSPACE:-}" ]]; then upstream_args+=(--env "SHIM_UPSTREAM_WORKSPACE=$SHIM_UPSTREAM_WORKSPACE"); fi
                fi
                if [[ -n "${SHIM_UPSTREAM_EGRESS_NETWORK:-}" ]]; then
                    podman network exists "$SHIM_UPSTREAM_EGRESS_NETWORK" || { echo "Create SHIM_UPSTREAM_EGRESS_NETWORK first." >&2; exit 2; }
                    upstream_args+=(--network "$SHIM_UPSTREAM_EGRESS_NETWORK")
                fi
                podman run --rm --pull never --http-proxy=false --cidfile "$cid_file" --name "$proxy" \
                    --label "io.vertex-shim.session=${session}" \
                    --network "$VERTEX_SHIM_NETWORK" --network-alias vertex-shim \
                    --userns keep-id:uid=1000,gid=1000 --read-only --cap-drop all --security-opt no-new-privileges \
                    --tmpfs /tmp:rw,nosuid,nodev,size=256m \
                    --volume "${workspace}/Cargo.toml:/work/Cargo.toml:ro,z" \
                    --volume "${workspace}/Cargo.lock:/work/Cargo.lock:ro,z" \
                    --volume "${workspace}/src:/work/src:ro,z" \
                    --volume "${state_dir}/config:/config:ro,z" \
                    --volume "${state_dir}/admin-api-key:/run/secrets/admin-api-key:ro,z" \
                    --volume "vertex-shim-${session}-telemetry:/data:U" \
                    --volume "vertex-shim-${session}-build:/build:U" \
                    --volume "vertex-shim-cargo-1-90:/cargo:U" \
                    --tmpfs /run/vertex-shim:rw,mode=1777,size=1m \
                    --env SHIM_ADMIN_LISTEN=0.0.0.0:8081 \
                    --publish "127.0.0.1:${publish_inference}:8080" --publish "127.0.0.1:${publish_admin}:8081" \
                    "${upstream_args[@]}" "$image" &
                wait $!
                ;;
            *) echo "Usage: dev.sh service {proxy|client|vertex-client}" >&2; exit 2 ;;
        esac
        ;;
    exec)
        target=${2:-proxy}; shift 2
        case "$target" in proxy) name=$proxy;; client|anthropic-client) name=$client;; vertex-client) name=$vertex_client;; *) echo "Unknown target" >&2; exit 2;; esac
        tty_flags=(); if [[ -t 0 && -t 1 ]]; then tty_flags=(-it); fi
        exec podman exec "${tty_flags[@]}" "$name" "$@"
        ;;
    restart)
        # Compile first. A failed build never signals/replaces the running binary.
        podman exec "$proxy" bash -c 'export SHIM_BUILD_ID=$(sha256sum Cargo.toml Cargo.lock src/*.rs src/admin_ui/* | sha256sum | cut -d " " -f1); cargo build --locked --offline && install /build/target/debug/vertex-shim /build/bin/vertex-shim.next && mv /build/bin/vertex-shim.next /build/bin/vertex-shim && touch /run/vertex-shim/restart'
        sleep 1
        podman exec "$proxy" curl -fsS http://127.0.0.1:8080/health
        ;;
    telemetry)
        endpoint=${2:-requests}
        case "$endpoint" in requests|summary|export) ;; *) echo "Expected requests, summary or export" >&2; exit 2;; esac
        podman exec "$proxy" bash -c 'key=$(cat /run/secrets/admin-api-key); printf "header = \"Authorization: Bearer %s\"\n" "$key" | curl -fsS --config - "$1"' -- "http://127.0.0.1:8081/api/private/v1/telemetry/${endpoint}"
        ;;
    stop)
        cleanup
        ;;
    status)
        podman ps --all --filter "label=io.vertex-shim.session=${session}" --format 'table {{.Names}}\t{{.Status}}\t{{.Image}}'
        ;;
    admin-url)
        address=$(podman port "$proxy" 8081/tcp)
        printf 'http://%s/admin/\n' "$address"
        ;;
    help|--help|-h)
        echo "Usage: dev.sh {build-images|init|start|exec proxy|client|anthropic-client|vertex-client COMMAND...|restart|admin-url|telemetry [requests|summary|export]|status|stop}"
        ;;
    *) echo "Unknown command: $1" >&2; exit 2;;
esac

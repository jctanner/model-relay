#!/usr/bin/env bash
# Bootstrap the MVP client before the Rust/Honcho stack is ready. The full
# launcher can supervise `client.sh service` as its foreground client process.
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
session=${VERTEX_SHIM_SESSION:-dev}
[[ "$session" =~ ^[a-z][a-z0-9-]{0,39}$ ]] || {
    echo "VERTEX_SHIM_SESSION must be a lowercase resource-name prefix." >&2
    exit 2
}
image=${CLAUDE_CLIENT_IMAGE:-localhost/vertex-shim-claude:dev}
network=${VERTEX_SHIM_NETWORK:-vertex-shim-${session}}
role=${SHIM_CLIENT_ROLE:-client}
[[ "$role" == client || "$role" == vertex ]] || { echo 'Unknown SHIM_CLIENT_ROLE' >&2; exit 2; }
resource=client
if [[ "$role" == vertex ]]; then resource=vertex-client; fi
container=vertex-shim-${session}-${resource}
home_volume=vertex-shim-${session}-${resource}-home
work_volume=vertex-shim-${session}-${resource}-work
# An explicitly empty value keeps disposable fixture clients on named volumes.
work_dir=${SHIM_CLIENT_WORK_DIR-${workspace}/client-work/${session}}
if [[ "$role" == vertex ]]; then
    work_dir=${SHIM_VERTEX_WORK_DIR-${workspace}/client-work/${session}-vertex}
fi
key_file=${PROXY_API_KEY_FILE:-${workspace}/.state/${session}/proxy-api-key}

doctor() {
    command -v podman >/dev/null || { echo "Podman is required." >&2; return 1; }
    local rootless
    rootless=$(podman info --format '{{.Host.Security.Rootless}}') || {
        echo "This process cannot access Podman. Check session sandbox permissions before changing the host's Podman configuration." >&2
        return 1
    }
    [[ "$rootless" == true ]] || { echo "Use rootless Podman for this workspace." >&2; return 1; }
}

case ${1:-help} in
    doctor)
        doctor
        ;;
    build)
        version=${2:-${CLAUDE_CODE_VERSION:-}}
        [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
            echo "Usage: scripts/client.sh build <exact Claude Code version>" >&2
            exit 2
        }
        doctor
        podman build --file "${workspace}/containers/Containerfile.claude" \
            --build-arg "CLAUDE_CODE_VERSION=${version}" \
            --build-arg "CLIENT_BASE_IMAGE=${CLIENT_BASE_IMAGE:-docker.io/library/debian@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587}" \
            --tag "$image" "$workspace"
        ;;
    init-key)
        # Generate only a local development key. Register its verifier in the
        # proxy when that service is added; never print the key to stdout.
        command -v python3 >/dev/null
        python3 - "$key_file" <<'PY'
import os
import pathlib
import secrets
import sys

path = pathlib.Path(sys.argv[1]).absolute()
path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
try:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
except FileExistsError:
    print(f"Preserved existing local inference key file: {path}")
    sys.exit(0)
with os.fdopen(fd, "w") as stream:
    stream.write(secrets.token_urlsafe(32) + "\n")
print(f"Created local inference key file: {path}")
PY
        ;;
    service)
        doctor
        podman image exists "$image" || { echo "Build the client image first." >&2; exit 1; }
        podman network exists "$network" || {
            echo "Missing stack network: $network (the proxy launcher owns it)." >&2
            exit 1
        }
        client_api=${SHIM_CLIENT_API:-vertex}
        [[ "$client_api" == vertex || "$client_api" == messages ]] || { echo 'Unknown SHIM_CLIENT_API' >&2; exit 2; }
        client_auth=${SHIM_CLIENT_AUTH:-api-key}
        case "$client_auth" in
            api-key|mock-federated)
                if [[ "$client_auth" == mock-federated && "$client_api" != vertex ]]; then echo 'Mock federation requires Vertex mode' >&2; exit 2; fi
                [[ -f "$key_file" ]] || { echo "Run scripts/client.sh init-key first." >&2; exit 1; }
                key_file=$(realpath -- "$key_file")
                credential_args=(--volume "${key_file}:/run/secrets/proxy-api-key:ro,Z")
                ;;
            mock-adc)
                [[ "$client_api" == vertex ]] || { echo 'Mock ADC requires Vertex mode' >&2; exit 2; }
                credential_args=()
                ;;
            *) echo 'Unknown SHIM_CLIENT_AUTH' >&2; exit 2 ;;
        esac
        model_args=()
        vertex_flag='' skip_vertex_auth=''
        network_args=()
        if [[ -n "${SHIM_CLIENT_EGRESS_NETWORK:-}" ]]; then
            podman network exists "$SHIM_CLIENT_EGRESS_NETWORK" || { echo 'Missing SHIM_CLIENT_EGRESS_NETWORK' >&2; exit 2; }
            network_args+=(--network "$SHIM_CLIENT_EGRESS_NETWORK")
        fi
        if [[ "$client_api" == vertex ]]; then
            vertex_flag=1
            if [[ "$client_auth" == api-key ]]; then skip_vertex_auth=1; fi
            for variable in ANTHROPIC_MODEL ANTHROPIC_DEFAULT_SONNET_MODEL ANTHROPIC_DEFAULT_OPUS_MODEL ANTHROPIC_DEFAULT_HAIKU_MODEL; do
                model_args+=(--env "$variable=${VERTEX_SHIM_MODEL:-claude-sonnet-4-5@20250929}")
            done
        fi
        # Preserve existing resources; never replace a concurrent client's name.
        if podman container exists "$container"; then
            echo "Client container already exists: $container. Stop it explicitly first." >&2
            exit 1
        fi
        podman volume create --ignore "$home_volume" >/dev/null
        if [[ -n "$work_dir" ]]; then
            mkdir -p -- "$work_dir"
            work_dir=$(realpath -- "$work_dir")
            [[ -d "$work_dir" && -w "$work_dir" && -x "$work_dir" ]] || {
                echo "SHIM_CLIENT_WORK_DIR must be a writable directory: $work_dir" >&2
                exit 2
            }
            work_mount=${work_dir}:/work:rw,z
            echo "Client /work bind mount: $work_dir"
        else
            podman volume create --ignore "$work_volume" >/dev/null
            work_mount=${work_volume}:/work
        fi
        cid_dir=$(mktemp -d)
        cid_file=${cid_dir}/client.cid
        # The foreground command stays alive under Honcho. Explicit cleanup
        # also stops the container if the host Podman process is interrupted.
        cleanup() {
            trap - EXIT INT TERM
            # Use the ID created by this invocation, not a name that a racing
            # invocation might own if our `run` failed before container creation.
            if [[ -s "$cid_file" ]]; then
                podman stop --time 10 "$(cat "$cid_file")" >/dev/null 2>&1 || true
            fi
            rm -f -- "$cid_file"
            rmdir -- "$cid_dir"
        }
        trap cleanup EXIT
        trap 'exit 130' INT
        trap 'exit 143' TERM
        podman run --rm --pull never --http-proxy=false \
            --cidfile "$cid_file" --name "$container" \
            --label "io.vertex-shim.session=${session}" \
            --network "$network" --userns keep-id:uid=1000,gid=1000 \
            "${network_args[@]}" \
            --read-only --cap-drop all --security-opt no-new-privileges \
            --tmpfs /tmp:rw,nosuid,nodev,size=256m \
            --volume "${home_volume}:/home/claude" \
            --volume "$work_mount" \
            "${credential_args[@]}" \
            --volume "${workspace}/scripts/container/claude-proxy.sh:/usr/local/bin/claude-proxy:ro,z" \
            --env "ANTHROPIC_VERTEX_BASE_URL=${ANTHROPIC_VERTEX_BASE_URL:-http://vertex-shim:8080/v1}" \
            --env "SHIM_CLIENT_API=$client_api" \
            --env "SHIM_CLIENT_AUTH=$client_auth" \
            --env "SHIM_CLIENT_ROLE=$role" \
            --env "CLAUDE_CODE_USE_VERTEX=$vertex_flag" \
            --env "CLAUDE_CODE_SKIP_VERTEX_AUTH=$skip_vertex_auth" \
            --env "SHIM_PROJECT_ID=${SHIM_PROJECT_ID:-vertex-shim-local}" \
            --env "CLOUD_ML_PROJECT_ID=${SHIM_PROJECT_ID:-vertex-shim-local}" \
            --env "ANTHROPIC_VERTEX_PROJECT_ID=${SHIM_PROJECT_ID:-vertex-shim-local}" \
            --env "SHIM_MOCK_ADC_HOST=${SHIM_MOCK_ADC_HOST:-vertex-shim:8082}" \
            --env "SHIM_MESSAGES_BASE_URL=${SHIM_MESSAGES_BASE_URL:-http://vertex-shim:8080}" \
            --env "SHIM_MESSAGES_MODEL=${SHIM_MESSAGES_MODEL:-}" \
            "${model_args[@]}" \
            "$image" &
        runner_pid=$!
        wait "$runner_pid"
        ;;
    exec)
        shift
        [[ $# -gt 0 ]] || { echo "Usage: scripts/client.sh exec <command> [args...]" >&2; exit 2; }
        tty_flags=()
        if [[ -t 0 && -t 1 ]]; then tty_flags=(-it); fi
        podman exec "${tty_flags[@]}" "$container" "$@"
        ;;
    stop)
        podman stop --time 10 "$container"
        ;;
    status)
        podman ps --all --filter "name=^${container}$" \
            --format 'table {{.Names}}\t{{.Status}}\t{{.Image}}'
        ;;
    help|--help|-h)
        echo "Usage: scripts/client.sh {doctor|build VERSION|init-key|service|exec COMMAND...|status|stop}"
        ;;
    *)
        echo "Unknown client command: $1" >&2
        exit 2
        ;;
esac

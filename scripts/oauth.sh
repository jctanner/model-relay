#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
session=${VERTEX_SHIM_SESSION:-dev}
[[ "$session" =~ ^[a-z][a-z0-9-]{0,39}$ ]] || { echo "Invalid session" >&2; exit 2; }
auth_dir=${SHIM_OAUTH_DIR:-${workspace}/.state/${session}/auth}
source_mode=${SHIM_OAUTH_SOURCE:-host}
case ${1:-help} in
    init)
        mkdir -p "$auth_dir"
        chmod 700 "$auth_dir"
        ;;
    watch|sync)
        [[ "$source_mode" == host ]] || { echo "Host watcher requires SHIM_OAUTH_SOURCE=host." >&2; exit 2; }
        source_dir=${CLAUDE_SECURESTORAGE_CONFIG_DIR:-${CLAUDE_CONFIG_DIR:-${HOME}/.claude}}
        source_file=${SHIM_HOST_OAUTH_FILE:-${source_dir}/.credentials.json}
        flags=(); if [[ "$1" == sync ]]; then flags=(--once); fi
        exec python3 scripts/oauth_mirror.py --source "$source_file" --directory "$auth_dir" "${flags[@]}"
        ;;
    status)
        endpoint=http://127.0.0.1:8081/api/private/v1/auth/status
        podman exec "vertex-shim-${session}-proxy" bash -c 'key=$(cat /run/secrets/admin-api-key); printf "header = \"Authorization: Bearer %s\"\n" "$key" | curl -fsS --config - "$1"' -- "$endpoint"
        ;;
    help|--help|-h)
        echo "Usage: oauth.sh {init|sync|watch|status}; VERTEX_SHIM_SESSION scopes the access-token mirror."
        ;;
    *) echo "Unknown command" >&2; exit 2 ;;
esac

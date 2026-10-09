#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
export VERTEX_SHIM_SESSION=projects-$(date +%s)-$$ SHIM_HOST_PORT=0 SHIM_HOST_ADMIN_PORT=0
export SHIM_CLIENT_LAYOUT=single SHIM_CLIENT_API=messages SHIM_CLIENT_AUTH=api-key SHIM_CLIENT_WORK_DIR=''
export SHIM_BACKEND=anthropic SHIM_UPSTREAM_AUTH=bearer-file SHIM_UPSTREAM_SYNTHETIC=true
export SHIM_UPSTREAM_BASE_URL=http://account-a:8090 SHIM_UPSTREAM_WORKSPACE=wrkspc_fixture
export SHIM_MOCK_ADC_KEY_ID=dev-inference
session=$VERTEX_SHIM_SESSION
state_dir=${workspace}/.state/${session}
export SHIM_UPSTREAM_TOKEN_FILE=${state_dir}/account-a-token
export SHIM_UPSTREAM_PROFILES_DIR=${state_dir}/upstreams
network=vertex-shim-${session}
proxy=vertex-shim-${session}-proxy
python_image=docker.io/library/python@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641
launcher='' alpha_pid='' beta_pid=''
cleanup() {
    trap - EXIT INT TERM
    for pid in "$alpha_pid" "$beta_pid" "$launcher"; do
        if [[ -n "$pid" ]]; then kill -TERM "$pid" 2>/dev/null || true; wait "$pid" || true; fi
    done
    ./scripts/dev.sh stop >/dev/null 2>&1 || true
    for role in account-a account-b; do podman rm --force "vertex-shim-${session}-${role}" >/dev/null 2>&1 || true; done
    podman network rm "$network" >/dev/null 2>&1 || true
    for resource in client-home client-work telemetry build alpha-client-home beta-client-home; do
        podman volume rm "vertex-shim-${session}-${resource}" >/dev/null 2>&1 || true
    done
    rm -f "$state_dir/proxy-api-key" "$state_dir/admin-api-key" "$state_dir/alpha-key" "$state_dir/beta-key" "$state_dir/account-a-token" "$state_dir/upstreams/account-b-token" "$state_dir/upstreams/account-b/.credentials.json"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
./scripts/dev.sh init
python3 - "$state_dir" <<'PY'
import json,pathlib,secrets,sys,time
s=pathlib.Path(sys.argv[1]);p=s/'upstreams';p.mkdir(mode=0o700);o=p/'account-b';o.mkdir(mode=0o700)
a='synthetic-a-'+secrets.token_urlsafe(32);b='synthetic-b-'+secrets.token_urlsafe(32)
for f,value in [(s/'account-a-token',a),(p/'account-b-token',b)]: f.write_text(value+'\n');f.chmod(0o600)
f=o/'.credentials.json';f.write_text(json.dumps({'claudeAiOauth':{'accessToken':b,'expiresAt':int(time.time()*1000)+3600000,'scopes':['user:inference']}}));f.chmod(0o600)
(p/'profiles.json').write_text(json.dumps({'account-b':{'mode':'claude-oauth','base_url':'http://account-b:8090','oauth_dir':'/upstreams/account-b','synthetic':True,'workspace':'wrkspc_fixture'}}))
k=s/'config/keys.json';r=json.loads(k.read_text());r['keys'][0]['catalog_models']=True;k.write_text(json.dumps(r))
PY
podman network create --internal --label "io.vertex-shim.session=$session" "$network" >/dev/null
for role in account-a account-b; do
    token=$SHIM_UPSTREAM_TOKEN_FILE script=anthropic_upstream.py
    if [[ "$role" == account-b ]]; then token=$SHIM_UPSTREAM_PROFILES_DIR/account-b-token; script=claude_oauth.py; fi
    podman run --detach --pull never --http-proxy=false --name "vertex-shim-${session}-${role}" \
        --network "$network" --network-alias "$role" --userns keep-id:uid=1000,gid=1000 --user 1000:1000 \
        --read-only --cap-drop all --security-opt no-new-privileges \
        --volume "${workspace}/tests:/tests:ro,z" --volume "${token}:/run/secrets/upstream-token:ro,z" \
        "$python_image" python -B "/tests/fixtures/$script" >/dev/null
done
./scripts/dev.sh start > "$state_dir/stack.log" 2>&1 & launcher=$!
for _ in {1..180}; do
    if podman exec "$proxy" curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 && podman exec "vertex-shim-${session}-client" true >/dev/null 2>&1; then break; fi
    kill -0 "$launcher" 2>/dev/null || { cat "$state_dir/stack.log" >&2; exit 1; }; sleep 1
done
python3 tests/probe_projects.py "$session" provision
for project in alpha beta; do
    VERTEX_SHIM_SESSION=${session}-${project} VERTEX_SHIM_NETWORK=$network \
        SHIM_CLIENT_API=vertex SHIM_CLIENT_AUTH=mock-federated SHIM_PROJECT_ID=agent-${project} \
        PROXY_API_KEY_FILE=$state_dir/${project}-key SHIM_CLIENT_WORK_DIR=$state_dir/${project}-work \
        ./scripts/client.sh service > "$state_dir/${project}-container.log" 2>&1 &
    if [[ "$project" == alpha ]]; then alpha_pid=$!; else beta_pid=$!; fi
    for _ in {1..30}; do if podman exec "vertex-shim-${session}-${project}-client" true >/dev/null 2>&1; then break; fi; sleep 1; done
    podman exec "vertex-shim-${session}-${project}-client" test ! -e /upstreams
    podman exec "vertex-shim-${session}-${project}-client" test ! -e /auth/.credentials.json
done
podman exec "vertex-shim-${session}-alpha-client" claude-proxy -p 'Reply with a short greeting.' --model=claude-opus-4-6 --tools '' --max-turns 1 > "$state_dir/alpha-native.txt" & native_a=$!
podman exec "vertex-shim-${session}-beta-client" claude-proxy -p 'Reply with a short greeting.' --model=claude-opus-4-6 --tools '' --max-turns 1 > "$state_dir/beta-native.txt" & native_b=$!
wait "$native_a"; wait "$native_b"
python3 tests/probe_projects.py "$session" exercise
./scripts/dev.sh restart > "$state_dir/restart.log"
python3 tests/probe_projects.py "$session" persistence
podman logs "$proxy" > "$state_dir/proxy.log" 2>&1
python3 tests/probe_projects.py "$session" privacy
echo "PASS: two projects, independent provider auth, concurrent native ADC clients, CRUD, revocation, restart, and telemetry; artifacts: $state_dir"

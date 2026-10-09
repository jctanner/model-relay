#!/usr/bin/env bash
set -euo pipefail

if [[ "${SHIM_CLIENT_AUTH:-api-key}" == mock-adc || "${SHIM_CLIENT_AUTH:-api-key}" == mock-federated ]]; then
    [[ "${SHIM_CLIENT_API:-vertex}" == vertex ]] || { echo 'Mock ADC requires Vertex mode' >&2; exit 2; }
    # Exercise the real Google ADC chain with our synthetic metadata server.
    # Never inject an Authorization header or read the opaque inference key.
    unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_CUSTOM_HEADERS GOOGLE_APPLICATION_CREDENTIALS
    unset CLAUDE_CODE_SKIP_VERTEX_AUTH CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_FOUNDRY CLAUDE_CODE_USE_MANTLE
    unset ANTHROPIC_BASE_URL CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY
    if [[ -e "$HOME/.config/gcloud/application_default_credentials.json" ]]; then
        echo 'Mock ADC needs a client home without existing Google ADC credentials.' >&2; exit 2
    fi
    export CLAUDE_CODE_USE_VERTEX=1
    if [[ "$SHIM_CLIENT_AUTH" == mock-federated ]]; then
        key_file=${PROXY_API_KEY_FILE:-/run/secrets/proxy-api-key}
        [[ -r "$key_file" ]] || { echo 'Missing workload credential file' >&2; exit 2; }
        project=${SHIM_PROJECT_ID:-${CLOUD_ML_PROJECT_ID:-vertex-shim-local}}
        [[ "$project" =~ ^[a-z][a-z0-9-]{0,62}$ && "$project" != *- ]] || { echo 'Invalid SHIM_PROJECT_ID' >&2; exit 2; }
        export CLOUD_ML_PROJECT_ID=$project GOOGLE_CLOUD_PROJECT=$project ANTHROPIC_VERTEX_PROJECT_ID=$project
        audience="//iam.googleapis.com/projects/000000000001/locations/global/workloadIdentityPools/${project}/providers/vertex-shim"
        token_url="http://${SHIM_MOCK_ADC_HOST:-vertex-shim:8082}/v1/token"
        umask 077
        adc_config=$(mktemp /tmp/vertex-shim-adc.XXXXXX.json)
        jq -n --arg audience "$audience" --arg token_url "$token_url" --arg file "$key_file" \
            '{type:"external_account",audience:$audience,token_url:$token_url,
              subject_token_type:"urn:ietf:params:oauth:token-type:access_token",
              credential_source:{file:$file,format:{type:"text"}}}' > "$adc_config"
        export GOOGLE_APPLICATION_CREDENTIALS=$adc_config
        unset GCE_METADATA_HOST GCE_METADATA_IP METADATA_SERVER_DETECTION
        metadata_token=$(curl -fsS --max-time 5 "$token_url" \
            --data-urlencode 'grant_type=urn:ietf:params:oauth:grant-type:token-exchange' \
            --data-urlencode 'scope=https://www.googleapis.com/auth/cloud-platform' \
            --data-urlencode 'requested_token_type=urn:ietf:params:oauth:token-type:access_token' \
            --data-urlencode 'subject_token_type=urn:ietf:params:oauth:token-type:access_token' \
            --data-urlencode "audience=$audience" --data-urlencode "subject_token@${key_file}" | jq -er '.access_token')
        unset audience project token_url adc_config
    else
        export GCE_METADATA_HOST=${SHIM_MOCK_ADC_HOST:-vertex-shim:8082}
        export GCE_METADATA_IP=$GCE_METADATA_HOST METADATA_SERVER_DETECTION=ping-only
        # Discovery only; native Claude acquires its own inference token via ADC.
        metadata_token=$(curl -fsS --max-time 5 -H 'Metadata-Flavor: Google' \
            "http://${GCE_METADATA_HOST}/computeMetadata/v1/instance/service-accounts/default/token" | jq -er '.access_token')
    fi
    catalog=$(printf 'header = "Authorization: Bearer %s"\n' "$metadata_token" | \
        curl -fsS --config - --max-time 5 "${SHIM_MESSAGES_BASE_URL:-http://vertex-shim:8080}/v1/models?client_api=vertex&limit=1000")
    unset metadata_token
    jq -e '.has_more == false and (.data | length > 0)' <<< "$catalog" >/dev/null || {
        echo 'Proxy returned an empty or incomplete Vertex deployment catalog.' >&2; exit 1;
    }
    unset ANTHROPIC_DEFAULT_SONNET_MODEL ANTHROPIC_DEFAULT_OPUS_MODEL ANTHROPIC_DEFAULT_HAIKU_MODEL ANTHROPIC_DEFAULT_FABLE_MODEL
    for family in SONNET OPUS HAIKU FABLE; do
        id=$(jq -r --arg family "${family,,}" '[.data[] | select(.line == $family or (.line == null and (.id | contains($family))))][0].id // empty' <<< "$catalog")
        if [[ -n "$id" ]]; then export "ANTHROPIC_DEFAULT_${family}_MODEL=$id"; fi
    done
    requested=''; expect_model=false; model_index=''; model_equals=false
    forward_args=("$@")
    for ((index=0; index<${#forward_args[@]}; index++)); do
        argument=${forward_args[index]}
        if [[ "$expect_model" == true ]]; then requested=$argument; model_index=$index; model_equals=false; expect_model=false; continue; fi
        case "$argument" in
            --model) expect_model=true ;;
            --model=*) requested=${argument#--model=}; model_index=$index; model_equals=true ;;
            --) break ;;
        esac
    done
    if [[ -n "$requested" ]]; then
        case "$requested" in
            sonnet) requested=${ANTHROPIC_DEFAULT_SONNET_MODEL:-sonnet} ;;
            haiku) requested=${ANTHROPIC_DEFAULT_HAIKU_MODEL:-haiku} ;;
            opus) requested=${ANTHROPIC_DEFAULT_OPUS_MODEL:-opus} ;;
        esac
        if [[ "$requested" =~ ^(.+)-([0-9]{8})$ ]]; then
            candidate=${BASH_REMATCH[1]}@${BASH_REMATCH[2]}
            if jq -e --arg candidate "$candidate" 'any(.data[]; .id == $candidate)' <<< "$catalog" >/dev/null; then requested=$candidate; fi
        fi
        jq -e --arg requested "$requested" 'any(.data[]; .id == $requested)' <<< "$catalog" >/dev/null || {
            echo "Unsupported or unauthorized Vertex model: $requested" >&2
            echo 'Start claude-proxy and use /model to see authorized Vertex deployments.' >&2
            exit 2
        }
        if [[ "$model_equals" == true ]]; then forward_args[model_index]=--model=$requested
        else forward_args[model_index]=$requested; fi
    fi
    model=${requested:-${ANTHROPIC_MODEL:-${ANTHROPIC_DEFAULT_SONNET_MODEL:-}}}
    if ! jq -e --arg model "$model" 'any(.data[]; .id == $model)' <<< "$catalog" >/dev/null; then
        model=$(jq -r '.data[0].id' <<< "$catalog")
    fi
    export ANTHROPIC_MODEL=$model
    umask 077
    settings=$(mktemp /tmp/vertex-shim-vertex-models.XXXXXX.json)
    jq --arg model "$model" '{model: $model, availableModels: [.data[].id],
        modelPicker: {replaceBuiltInOptions: true, options: [.data[] |
            {model: .id, label: .display_name, description: ("Vertex route: " + .id)}]}}' \
        <<< "$catalog" > "$settings"
    unset catalog requested model family id argument
    exec /usr/local/bin/claude --settings "$settings" "${forward_args[@]}"
fi
[[ "${SHIM_CLIENT_AUTH:-api-key}" == api-key ]] || { echo 'Unknown SHIM_CLIENT_AUTH' >&2; exit 2; }

# Read a local inference key when launching the CLI. The secret is never an
# image layer, a Podman argv value, or the upstream provider's OAuth token.
key_file=${PROXY_API_KEY_FILE:-/run/secrets/proxy-api-key}
if [[ ! -r "$key_file" ]]; then
    echo "Missing readable proxy API key file: $key_file" >&2
    exit 1
fi
proxy_key=$(cat "$key_file")
if [[ ! "$proxy_key" =~ ^[A-Za-z0-9_-]{32,}$ ]]; then
    echo "Expected a generated local proxy key (at least 32 URL-safe characters)." >&2
    exit 1
fi
# This wrapper's key format is for our generated development keys only. The
# proxy's auth registry must not impose a provider API-key prefix or length.
# Ensure later project commands cannot inherit an accidentally supplied real
# API key or a Google credential path from a manually invoked exec command.
unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN GOOGLE_APPLICATION_CREDENTIALS ANTHROPIC_CUSTOM_HEADERS
case ${SHIM_CLIENT_API:-vertex} in
    vertex)
        export ANTHROPIC_CUSTOM_HEADERS="Authorization: Bearer $proxy_key"
        unset proxy_key
        export CLAUDE_CODE_USE_VERTEX=1 CLAUDE_CODE_SKIP_VERTEX_AUTH=1
        exec /usr/local/bin/claude "$@"
        ;;
    messages)
        # Discovery requires a native credential variable, not custom headers.
        # Only our local inference key enters the client process environment.
        export ANTHROPIC_API_KEY=$proxy_key
        unset proxy_key
        unset CLAUDE_CODE_USE_VERTEX CLAUDE_CODE_SKIP_VERTEX_AUTH CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_FOUNDRY CLAUDE_CODE_USE_MANTLE
        unset ANTHROPIC_MODEL ANTHROPIC_DEFAULT_SONNET_MODEL ANTHROPIC_DEFAULT_OPUS_MODEL ANTHROPIC_DEFAULT_HAIKU_MODEL ANTHROPIC_DEFAULT_FABLE_MODEL
        export ANTHROPIC_BASE_URL=${SHIM_MESSAGES_BASE_URL:-http://vertex-shim:8080}
        export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
        # Fail closed rather than let the CLI fall back to a stale cached menu.
        # Fetch only metadata, and never put credentials in a curl argv value.
        catalog=$(printf 'header = "x-api-key: %s"\n' "$ANTHROPIC_API_KEY" |
            curl -fsS --config - --max-time 5 "${ANTHROPIC_BASE_URL}/v1/models?limit=1000")
        jq -e '.has_more == false and (.data | length > 0)' <<< "$catalog" >/dev/null || {
            echo 'Proxy returned an empty or incomplete model catalog.' >&2; exit 1;
        }
        for family in SONNET OPUS HAIKU FABLE; do
            id=$(jq -r --arg family "${family,,}" '[.data[] | select(.line == $family or (.line == null and (.id | contains($family))))][0].id // empty' <<< "$catalog")
            if [[ -n "$id" ]]; then export "ANTHROPIC_DEFAULT_${family}_MODEL=$id"; fi
        done
        saved_model=''
        if [[ -f "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json" ]]; then
            saved_model=$(jq -r '.model // empty' "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json")
        fi
        umask 077
        settings=$(mktemp /tmp/vertex-shim-models.XXXXXX.json)
        jq --arg saved "$saved_model" --arg requested "${SHIM_MESSAGES_MODEL:-}" --arg sonnet "${ANTHROPIC_DEFAULT_SONNET_MODEL:-}" '
            (.data | map(.id)) as $ids |
            {availableModels: $ids,
             model: (if $requested != "" then $requested
                     elif ($ids | index($saved)) != null then $saved
                     elif $sonnet != "" then $sonnet else .data[0].id end),
             modelPicker: {replaceBuiltInOptions: true, options: [.data[] |
                 {model: .id, label: .display_name,
                  description: ("From upstream catalog" + (if .lifecycle == "deprecated" then " (deprecated)" else "" end))}]}}
        ' <<< "$catalog" > "$settings"
        unset catalog saved_model id
        exec /usr/local/bin/claude --settings "$settings" "$@"
        ;;
    *) echo 'Unknown SHIM_CLIENT_API; use vertex or messages.' >&2; exit 2 ;;
esac

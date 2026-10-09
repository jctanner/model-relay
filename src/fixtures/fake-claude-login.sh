#!/bin/bash
# Synthetic native-login fixture. Never reads host credentials or calls a provider.
set -euo pipefail
if [[ "${1:-}" == --version ]]; then
    printf '2.1.293 (Claude Code)\n'
    exit 0
fi
[[ "$*" == 'auth login --claudeai' ]]
[[ -z "${ANTHROPIC_AUTH_TOKEN:-}" && -z "${CLAUDE_CODE_USE_VERTEX:-}" ]]
[[ "$CLAUDE_CONFIG_DIR" == "$HOME/.claude" && "$BROWSER" == /bin/false ]]
state=${HOME##*/}
printf 'Open https://claude.com/cai/oauth/authorize?response_type=code&state=%s&code_challenge=fixture-challenge&code_challenge_method=S256\nPaste code: ' "$state"
read -r code
[[ "$code" == "fixture-code#$state" ]]
expires=$(( $(date +%s) * 1000 + 3600000 ))
umask 077
printf '{"claudeAiOauth":{"accessToken":"fixture-access","refreshToken":"fixture-refresh","expiresAt":%s,"scopes":["user:inference"]}}' "$expires" > "$CLAUDE_CONFIG_DIR/.credentials.json"
# Deliberately put sensitive-looking material in stdout. It must never reach REST.
printf 'fixture-code fixture-access fixture-refresh\n'

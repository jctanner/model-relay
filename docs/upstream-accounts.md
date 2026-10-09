# Managed Anthropic OAuth accounts

The admin console's **Upstream accounts** view manages relay-owned subscription
accounts alongside existing server-configured profiles. Each managed account
becomes a project-selectable upstream profile. It does not change local client
authentication or the current default host login.

## Browser authorization

1. Add an account ID and display name.
2. The relay starts the pinned Claude Code 2.1.293 native command,
   `claude auth login --claudeai`, with a private temporary HOME,
   `CLAUDE_CONFIG_DIR`, and `CLAUDE_SECURESTORAGE_CONFIG_DIR`. Client keys, Vertex
   settings, host credentials, and inherited environment are excluded.
3. The console polls for an allowlisted Anthropic authorization URL. Open it in
   a browser signed in to the desired account and authorize access.
4. Paste the returned code into the console. A `code#state` value must match the
   account's current login. Claude handles the actual authorization-code/PKCE
   exchange; the relay does not construct its own login request.
5. After the subprocess exits successfully, the relay validates the native
   credentials and atomically publishes them into the account's permanent store.
   Assign projects to the account's profile ID.

The [Claude authentication documentation](https://code.claude.com/docs/en/authentication)
describes browser login and the manual code fallback for remote environments;
the [CLI reference](https://code.claude.com/docs/en/cli-reference) documents
`auth login`. The login parser and refresh transport are verified against the
pinned native binary. The subscription token schema/refresh endpoint are an
experimental integration, not a public subscription gateway API contract.

There is one active flow per account, up to four across the relay. Flows expire
in ten minutes. Closing the login dialog requests cancellation; expiry also
cleans up abandoned flows. Restart cancels active subprocesses and removes
stale staging stores. Reconnecting publishes new credentials only after success;
failed or cancelled attempts preserve the previous connected account.

## Private REST API

All routes require the admin bearer key on the separate admin listener. The
base path is `/api/private/v1/upstream-accounts`; responses are `no-store`.

| Method | Suffix | Behavior |
| --- | --- | --- |
| GET | empty | List safe account status, including external profiles and helper availability |
| POST | empty | Create with `{ "account_id": "team-a", "display_name": "Team A" }` |
| GET | `/{account}` | Managed account status and safe login summary |
| PUT | `/{account}` | Rename using `{ "display_name": "New name" }` |
| DELETE | `/{account}` | Remove account/credentials; 409 if any live project references it |
| POST | `/{account}/login` | Start login; 202 with flow ID and expiry |
| GET | `/{account}/login/{flow}` | Poll state; authorization URL only while awaiting a code |
| POST | `/{account}/login/{flow}/code` | Submit `{ "code": "<provider code>" }`; 202, no echo |
| DELETE | `/{account}/login/{flow}` | Cancel this flow |
| POST | `/{account}/disconnect` | Cancel login and remove stored credentials; retain metadata/project references |
| POST | `/{account}/refresh` | Explicitly refresh access and return safe status |

States are `starting`, `awaiting_code`, `exchanging`, then `connected`, `failed`,
`cancelled`, or `expired`. Authorization URLs are cleared on terminal states.
Account lists/detail contain expiry, readiness, refresh owner, and safe error
codes, never access/refresh tokens or raw subprocess output. Externally configured
profiles are listed but cannot be modified through these routes.

IDs are immutable, unique across managed and static profiles, and cannot use
`default`. Display names are editable. `/upstream-profiles` includes new account
IDs immediately; no restart is required to create, connect, or assign accounts.
Missing credentials fail closed, without switching a project to another account.

## Refresh and durable state

The relay owns refresh only for managed accounts. A per-account mutex serializes
refresh, native-login publication, and credential deletion. Requests renew when
access expires within two minutes; a background worker checks every 30 seconds.
Transient failures back off for 60 seconds. A rejected refresh appears as
`needs_login` with `upstream_auth_reauthentication_required`; reconnect from the
console. The explicit refresh action bypasses the timing/backoff window.

Refresh uses the native subscription transport's JSON `refresh_token` grant,
client ID, and scopes. Rotated refresh tokens and access expiry are saved by
mode-0600 atomic replacement and fsync. Unusable responses do not overwrite the
previous store. The previous host access-only mirror still has host-owned
refresh; no managed operation writes host credentials.

SQLite stores account ID/name alongside projects, keys, and telemetry. Credentials
are separate private JSON files under `/data/oauth-accounts/{id}/credentials/`.
Temporary native homes are under `{id}/logins/{flow}/`. Persist and back up the
**whole `/data` volume** for a recoverable deployment; a SQLite-only backup does
not include managed upstream credentials. Client container homes remain separate.

| Server setting | Default |
| --- | --- |
| `SHIM_ACCOUNTS_DIR` | `oauth-accounts` next to `SHIM_DB` (normally `/data/oauth-accounts`) |
| `SHIM_CLAUDE_BIN` | `/usr/local/bin/claude`; development uses `/build/bin/claude` if present |
| `SHIM_MANAGED_UPSTREAM_BASE_URL` | `https://api.anthropic.com` |
| `SHIM_OAUTH_TOKEN_URL` | `https://platform.claude.com/v1/oauth/token` |

Paths must be absolute. HTTP provider/token endpoints require the existing
server-side synthetic-test opt-in. These settings cannot be supplied by inference
clients, project JSON, or account JSON. Proxy images use the curl installer to
pin the native helper. An absent/mismatched helper disables new login attempts;
stored accounts can continue to serve and refresh.

For Kubernetes, mount a PVC at `/data`, provide the admin key separately as a
Secret, and allow DNS/HTTPS egress for provider login, token exchange, and
inference. Browser authorization requires no externally reachable pod callback
port. Keep the private admin API behind authenticated administrative access.
A single replica with `Recreate` rollout is the supported deployment: an OS file
lock allows one credential owner per account volume. Do not scale this store to
multiple pods. Ensure the runtime UID can write the volume and preserve private
file/directory permissions; volume permission reconciliation must not add group
or world access to credential files. Supply a writable `/tmp` (for example an
`emptyDir`) when running with a read-only container filesystem. Login is needed once per account, then
credentials survive pod replacement on the persistent volume.

Run `./scripts/test.sh` for isolated native subprocess fixtures, account routing,
cancellation/expiry, persistence, private API access, and serialized refresh tests.

# Managed upstream accounts

The admin console's **Upstream accounts** view manages relay-owned subscription
accounts alongside existing server-configured profiles. Each managed account
becomes a project-selectable upstream profile. It does not change local client
authentication or the current default host login.

## Vertex service accounts

Choose **Google Vertex · service account** when adding an account. Open
**Vertex credentials**, enter the upstream Google Cloud project and region
(`global` is supported), and paste the exported service-account JSON. The
secret field starts masked, is cleared after submission or closing the dialog,
and is never populated from the server. Leave it blank when editing project or
region to keep the saved key. Paste a replacement to rotate the key.

Assign the new account as a project's default upstream, or as the destination
for selected models. Both Messages and Vertex clients can use it. Clients keep
their existing relay credentials and local Vertex project ID; the relay uses
the upstream account's real project and location for Google requests.

The relay signs an RS256 service-account assertion with the cloud-platform
scope and exchanges it at Google's fixed `https://oauth2.googleapis.com/token`
endpoint. Uploaded JSON cannot redirect credential exchange to another host.
Access tokens stay in memory, refresh within one minute of expiration, and
share one refresh across concurrent requests. Failures back off for ten seconds;
**Refresh access** retries immediately. Rotation and disconnect invalidate the
cache under the same lock used for refresh. Status responses show configuration,
expiry and safe error codes, without the JSON, account email or Google token.

The JSON is stored by atomic mode-0600 replacement in
`/data/oauth-accounts/{id}/vertex-credentials.json`, inside a mode-0700 directory.
It is not encrypted at rest: protect the volume and its backups as credentials.
SQLite stores account metadata and model policy, without the credential JSON.

Vertex has no Anthropic `/v1/models` endpoint. Its **Models** editor starts with
manual entries for Opus 4.6, Sonnet 4.6 and Haiku 4.5. Add or disable Claude
models there as needed; the entries do not prove Google project access, model
enablement or quota. Dated IDs are translated to Vertex's `@YYYYMMDD` format.
GLM is not a Vertex Claude model and must route to a suitable other provider.
Google authentication and Vertex inference are separate checks; saving a valid
key alone does not verify IAM permissions or model availability.

Protocol references: [Google service-account OAuth](https://developers.google.com/identity/protocols/oauth2/service-account)
and [Anthropic's Vertex SDK transport](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/packages/vertex-sdk/src/client.ts).

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
| PUT | `/{account}/vertex` | Vertex only: `{ "project_id": "gcp-project", "region": "global", "service_account_json": "<secret JSON text>" }`; omit the secret to retain it |
| PUT | `/{account}/token` | EnMaaS only: set or rotate `{ "token": "<personal API token>" }` |
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

Creation accepts `provider`: `anthropic` (the default), `enmaas`, or `vertex`.
Browser login applies to Anthropic accounts; Vertex and EnMaaS use their own
credential editors and never launch the Claude login helper.

## Anthropic refresh and durable state

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

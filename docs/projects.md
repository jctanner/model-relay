# Projects, credentials, and telemetry

Projects represent GCP project identities inside this simulator. Each client
credential belongs to exactly one project. API-key and bearer authentication
work on both Messages and Vertex endpoints; mock Google tokens inherit the same
project and current grants. Exchange-only workload keys must first obtain a
Google bearer token; their raw secrets cannot authenticate inference or model
discovery. A Vertex URL naming another project returns 403.
Messages requests derive their project from the credential, rather than a
client-supplied label or run ID.

`vertex-shim-local` is seeded once as the built-in development project. Existing
file-registry keys belong to it unless they explicitly set `project_id`; the
legacy Vertex route project `local` and the configured metadata project alias it. Historical telemetry from before
projects existed is migrated into that built-in project. Its deletion persists
across restarts, and administrators can edit, disable, or delete it like any
other project. Legacy keys remain managed in `config/keys.json`; credentials
created through REST are managed in SQLite. Disabling/deleting their project
rejects both kinds of credentials.

Project state and managed credential verifiers persist in the existing `/data`
volume alongside telemetry. Changes take effect on subsequent requests without
rebuilding images or restarting the proxy. A request already in flight uses the
project/auth snapshot with which it was accepted.

## Private REST API

All routes below live under `/api/private/v1` on the admin listener and require
its separate bearer key. Project inference keys and mock Google tokens cannot
administer projects or read private telemetry. Responses use `Cache-Control:
no-store`; bodies are limited to 64 KiB.

| Method | Path | Result |
| --- | --- | --- |
| GET / POST | `/projects` | Paginated list / create project |
| GET / PUT / DELETE | `/projects/{project_id}` | Read / replace mutable fields / delete |
| GET / POST | `/projects/{project_id}/credentials` | List managed credentials / issue a key |
| GET / PUT / DELETE | `/projects/{project_id}/credentials/{key_id}` | Read / replace grants / revoke |
| POST | `/projects/{project_id}/credentials/{key_id}/rotate` | Replace the key, keeping its ID |
| GET | `/projects/{project_id}/auth/status` | Project upstream profile and credential readiness |
| GET | `/upstream-profiles` | Installed profile IDs, backend, and auth mode |
| GET | `/projects/{project_id}/telemetry/requests` | Project request records |
| GET | `/projects/{project_id}/telemetry/summary` | Project token totals for a bounded page |
| GET | `/projects/{project_id}/telemetry/keys` | Read-only key identities for the telemetry selector |

Project create/PUT bodies contain `project_id`, `display_name`, optional
`enabled` (default true), `labels` (default empty), and `upstream_profile`
(default `default`). PUT replaces the document; `project_id` is immutable.
IDs start with a lowercase letter, contain lowercase letters/digits/hyphens,
are at most 63 characters, and cannot end with a hyphen. Duplicate creation is
409; unknown project/credential operations are 404; unknown profiles are 400.
Lists accept `limit=1..500` and `after_id`; continue using `next_after_id`.

The telemetry key inventory includes managed credentials and file-registry
bootstrap keys, including disabled projects' and expired credentials' identities
for historical usage. Each entry contains `project_id`, `key_id`, `client_id`,
`name`, `token_type` (`maas`, `anthropic`, `vertex`, or `legacy` combined access),
and nullable `expires_at_ms`. It omits secrets, verifiers, and grants, uses the
same `limit`/`after_id` pagination, and grants no ability to edit bootstrap keys.
In the console, key options appear under their project; selecting one applies
both `project_id` and `key_id` to telemetry requests, totals, and exports.

Credential POST/PUT bodies contain `client_id`, `models`, `backends`, and the
booleans `catalog_models`, `vertex_catalog_models`, `mock_adc`, `exchange_only`. Optional grants
default to empty/false, so PUT must include all grants that should remain.
Backend grants are explicit: `mock` or `anthropic`. The catalog flags are
independent permissions for Messages and Vertex respectively. `mock_adc` allows
the key to obtain expiring synthetic Google credentials as well as ordinary
API-key access. These are local keys, not upstream account tokens.

Set `exchange_only: true` together with `mock_adc: true` to issue a Vertex
workload key. Its secret is accepted only as the subject of Google token
exchange (`POST /v1/token` on the ADC listener). Use the resulting short-lived
bearer token for Vertex inference and model discovery. PUT and rotation retain
the exchange-only restriction; issue a separate API key for direct access.
Existing keys default to `exchange_only: false` and retain their previous
behavior.

The admin console inherits the backend from the project's upstream profile;
the production provider is **Anthropic OAuth**. Client token types determine
authentication automatically: Anthropic and MaaS API keys use direct API
authentication without Google exchange, while Vertex workload keys use only
token exchange. Model catalog access follows the selected client interface.
The development simulator remains available for isolated testing, but is not
an optional token grant in the console. Legacy keys with combined access retain
their permissions when edited.

Credential POST also accepts `token_type: "maas"` to issue a MaaS-compatible
`sk-oai-<key-id>_<secret>` API key. The key is shown once in `api_key`; only a
salted SHA-256 verifier is stored. Regular keys require `name`, default to 90
days, and cannot exceed 90 days. Set `ephemeral: true` for a short-lived key;
it defaults to one hour, has a one-hour maximum, and does not require a name.
`expiresIn` accepts durations such as `30d`, `90d`, `1h`, or `2h30m`. The proxy
rejects an expired key at authentication time. Existing credentials without
`token_type` keep their current format and behavior.

For example, issue a MaaS-format key scoped to one project and model:

```json
{
  "token_type": "maas",
  "client_id": "batch-agent",
  "name": "Nightly batch",
  "expiresIn": "30d",
  "models": ["claude-sonnet-4-5"],
  "backends": ["anthropic"]
}
```

Only issuance and rotation return `api_key`. Reads omit both the secret and its
hash. Rotation/revocation reject outstanding mock tokens on their next request.
Disabling a project or changing its upstream profile invalidates issued mock
tokens; re-enabling requires fresh ADC tokens. Deleting removes managed keys
and retains historical usage. Recreating the same ID does not revive its old
managed credentials or tokens.

Global `/telemetry/requests`, `/telemetry/summary`, and `/telemetry/export` also
accept `project_id`. Project telemetry accepts the existing time/model/client/
key/run/operation/status filters and cursor pagination. Conflicting project
filters are rejected. Summaries cover one page (default 100, maximum 500);
continue with `next_cursor` for larger histories. Counting requests are reported
separately from generation totals. Requests record `project_id` and
`upstream_profile`, with token usage, context capacity, and the existing timing
and lifecycle fields. Deleted projects' historical telemetry remains queryable.
Prompts, response text, credentials, and mock token values are excluded.

## Create a project and launch a client

The Python helper uses the session's admin key without printing it or putting
it in process arguments. Substitute your running session for `liveoauth`.

```bash
./scripts/admin.py --session liveoauth GET /projects
./scripts/admin.py --session liveoauth --body \
  '{"project_id":"agent-alpha","display_name":"Alpha agent"}' POST /projects
./scripts/admin.py --session liveoauth --key-file .state/agent-alpha/inference-key \
  --body '{"client_id":"alpha-agent","backends":["anthropic"],"catalog_models":true,"vertex_catalog_models":true,"mock_adc":true}' \
  POST /projects/agent-alpha/credentials
./scripts/admin.py --session liveoauth GET /projects/agent-alpha/telemetry/summary
```

The key destination must be a new file. The helper writes it mode 0600 and prints
only credential metadata and its location. It supports all REST methods; use
`--body -` to read a JSON document from stdin. Use a new destination file with
`--key-file` when rotating a credential.

Start an additional rootless client attached to the existing proxy network:

```bash
VERTEX_SHIM_SESSION=agent-alpha \
VERTEX_SHIM_NETWORK=vertex-shim-liveoauth \
SHIM_CLIENT_API=vertex SHIM_CLIENT_AUTH=mock-federated \
SHIM_PROJECT_ID=agent-alpha \
PROXY_API_KEY_FILE="$PWD/.state/agent-alpha/inference-key" \
./scripts/client.sh service
# In another terminal:
VERTEX_SHIM_SESSION=agent-alpha ./scripts/client.sh exec claude-proxy
```

This foreground service owns only the new client's container; Ctrl-C stops it.
Its home volume and `client-work/agent-alpha` bind mount persist. Use distinct
session names, home volumes, work directories, and workload keys for additional
agents. `SHIM_CLIENT_EGRESS_NETWORK` can attach the client to your existing
Internet network. The proxy's network is owned by its stack; stop additional
clients before shutting down that stack.

For a Messages/API-key client, use `SHIM_CLIENT_API=messages
SHIM_CLIENT_AUTH=api-key` with the same project key. Only the workload key is
mounted into either client. Real upstream credentials stay in the proxy.
The existing three-container `liveoauth` stack remains usable; its original
metadata-based Vertex client continues using the built-in project.

`mock-federated` creates a temporary `external_account` ADC configuration,
points `GOOGLE_APPLICATION_CREDENTIALS` to it, and uses a file-sourced workload
key. Native Claude exchanges that key at the proxy's internal `/v1/token`
listener on port 8082 and uses the returned project-bound token for inference.
The wrapper obtains a separate token for model discovery. No skip-auth flag or
custom authorization header is injected into native Claude in this mode.
Token audience must match the key's project. Google ADC reads the documented
[external-account configuration](https://google.aip.dev/auth/4117); this local
issuer substitutes opaque workload keys for real federation assertions and
performs no Google IAM validation or Google token exchange. The existing
metadata endpoint remains a single trusted-network development identity; use
`mock-federated` for distinct client identities.

Port 8082 must be enabled on the proxy (`SHIM_MOCK_ADC_KEY_ID` already does so in
the dual stack) and remains unpublished. An additional client cannot select a
project simply by changing an environment variable: its key and Vertex project
must agree. Temporary tokens expire and disappear on proxy restart; clients
can acquire replacements with their workload keys.

## Independent upstream accounts

Every project defaults to the existing `default` upstream profile, including
its read-only host OAuth mirror. Separate client identities and telemetry do
not require separate provider accounts. To use another account, put a
`profiles.json` and its credentials in an owner-only server directory, then
start the proxy with `SHIM_UPSTREAM_PROFILES_DIR=/absolute/host/path`. The launcher
mounts that directory read-only at `/upstreams`, only in the proxy.

Example `profiles.json` (credential paths are container paths):

```json
{
  "team-b": {
    "mode": "claude-oauth",
    "base_url": "https://api.anthropic.com",
    "oauth_dir": "/upstreams/team-b",
    "workspace": null
  },
  "team-c": {
    "mode": "bearer-file",
    "base_url": "https://api.anthropic.com",
    "token_file": "/upstreams/team-c-token",
    "workspace": null
  }
}
```

`claude-oauth` expects the same access-only `.credentials.json` mirror schema
and owner-only directory/file permissions as the existing profile. Its owner
must maintain that mirror; the proxy never logs in, refreshes, or modifies the
source credentials. The existing Honcho watcher continues to manage only its
configured default host mirror. Other profile mirrors need their own producer.
`bearer-file` reads its token file on each request. Directory mounts follow
atomic replacements. Profiles never accept tokens, paths, or provider URLs
from inference clients or project JSON.

Set `upstream_profile` to `team-b` when creating/updating the corresponding
project. Each profile has its own HTTP client, credential source, and model
catalog cache; model discovery and inference use that project's selected
account. There is no fallback to another account if a profile or credential is
unavailable. Uninstalled profile IDs are rejected on project create/update.
Profiles themselves are server configuration, rather than secret-bearing REST
resources. Adding/changing the mounted profile configuration requires a proxy
process restart; adding the mount to an existing container requires stack
recreation. Project/grant CRUD needs neither.

Profiles support `mock`, `bearer-file`, and `claude-oauth`; only synthetic fixture
profiles may set `synthetic: true` to permit HTTP origins. Real origins require
HTTPS. The proxy is still an Anthropic-backed Vertex simulator: creating these
records does not provision real Google Cloud projects or IAM accounts.

## Verification

```bash
./scripts/test.sh
./scripts/test-projects.sh
./scripts/test-dual-client.sh
./scripts/test-mock-adc.sh
SHIM_CLIENT_LAYOUT=dual ./scripts/test-host-oauth.sh
```

The project suite launches two native Claude clients concurrently with different
ADC keys/projects and independent synthetic bearer/OAuth provider accounts. It
checks project routing, request counting, CRUD, rotation, revocation, restart
persistence, retained telemetry after deletion, and secret/content exclusion.
All paid-provider traffic is excluded from that fixture suite.

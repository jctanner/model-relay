# EnMaaS upstream and per-upstream model management

## Status and objective

Implemented and deployed 2026-10-11. Authenticated EnMaaS validation and native
Claude 2.1.293 checks passed through Messages and Vertex. This plan belongs
in the Model Relay repository, separately from the strategy-workflow evaluation.

Add an `enmaas` upstream provider whose personal API token is managed through
the relay console. Clients authenticate with relay-issued credentials and
can use the existing Anthropic Messages or Vertex facade. Initially expose
only **`rits/zai-org/glm-5-3`**, labeled **GLM 5.3**, with **medium effort**.
Add admin API and frontend controls for the models enabled on each upstream.

## Provider details supplied by the user

- Origin: `https://api.enmaas.devshift.net`; API prefix: `/v1`.
- Discovery: `GET /v1/models`.
- Documented authentication alternatives: `x-api-key` or `Authorization: Bearer`,
  carrying the personal EnMaaS token. Verify inference as well as discovery.
- Direct Claude configuration uses `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`
  and `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`.
- Exact hosted model ID: `rits/zai-org/glm-5-3`; do not rename it upstream.
- Provider instructions prescribe `effortLevel: medium` for this model.

The token itself is not part of this plan or source configuration.

## Current implementation and gaps

The relay already has project-to-upstream routing, local API keys, Vertex
token exchange, MaaS-format key issuance, per-token explicit model grants,
and separate Messages/Vertex catalog permissions. The console can edit
grants but currently uses a text field and an all-catalog checkbox.

Upstream catalogs are fetched using the selected account's credentials.
There is no persisted administrator-controlled enabled-model inventory
for each upstream. Existing managed accounts support Anthropic OAuth;
static bearer-file profiles can point to another origin but do not provide
console-managed personal-token entry/rotation.

Specific compatibility work:

- `src/models.rs`: catalog model-ID validation excludes `/`.
- `src/main.rs`: configured upstream model-ID validation also excludes `/`.
- `src/projects.rs`: credential model grants exclude `/`.
- `src/lib.rs`: model-detail and Vertex operation routes assume one path
  segment. Verify correct handling of namespaced IDs and URL encoding.
- `src/backend.rs`: catalog parser expects Anthropic metadata and
  `has_more=false`; normalize EnMaaS discovery rather than assuming its shape.
- `src/accounts.rs` and `src/credentials.rs`: managed-account lifecycle is
  OAuth-specific; add a managed API-token credential path without native login.
- `src/admin_ui/`: account editor and provider labels assume Anthropic OAuth.

Existing timeout repair changes are uncommitted in this checkout. Preserve
them, their regression tests and configured whole-stream deadline.

## Design

### Separate provider, protocol and client authentication

Represent EnMaaS as its own upstream provider/account type, with a token
stored privately on the relay. Keep provider identity separate from its
Anthropic-compatible wire format and from the client's credential type.
Do not send local client keys or Google credentials to EnMaaS.

Start with the existing Messages and Vertex interfaces. MaaS is a credential
format, not an additional inference protocol. Genuine OpenAI-compatible
`/v1/chat/completions` or `/v1/responses` support would be a separate adapter;
neither exists today and neither is required for this first implementation.

Use a separate EnMaaS-backed project for initial validation; retain existing
Anthropic projects. Credential rotation/status should fit the current admin
API, private file storage and restart persistence behavior.

### Model inventory and policy per upstream account/profile

Scope inventory to the account/profile, not just the provider name, because
credentials can expose different models. Persist exact model IDs, display
names, enabled state and explicitly configured metadata/request policy.
Keep discovered metadata distinct from administrator overrides.

Effective client access is **upstream-enabled models intersected with
project-allowed models and client-token grants, minus project/token blocks**.
All-catalog grants are bounded by the selected upstream and project policy.
Apply the same policy to model listing, detail, counting and
streaming/nonstreaming inference across both interfaces. Resolve existing
aliases consistently to canonical upstream IDs for policy evaluation;
aliases must not bypass enablement, allowlists or blocks.

Newly discovered models on an explicitly managed upstream start disabled.
Disabling a model rejects subsequent requests even for previously issued
keys; do not terminate already accepted streams. Catalog refresh must not
overwrite administrator enablement or silently expose new models. Preserve
last discovery status and distinguish a refresh failure from an empty catalog.

Allow manual model registration for incomplete provider catalogs. Unknown
limits/capabilities remain unknown until verified; do not invent Claude-like
context limits or claim unsupported features for GLM.

Introduce policy compatibly: existing upstreams retain current access until
explicit policy is configured. Define and test the difference between an
unconfigured legacy policy and a configured empty enabled set (deny all).
EnMaaS begins with explicit policy enabling only the exact GLM model ID.

### Project and client-token model policy

2026-10-11 user discussion extends the design to granular project/token
allow and block lists. Projects set a model-access ceiling for every client;
individual tokens can narrow that access further, never expand it.

| Scope | Policy controls | Meaning |
| --- | --- | --- |
| Upstream | Enabled model inventory | Models the relay permits on this account |
| Project | Inherit upstream or explicit allowed IDs; blocked IDs | Shared ceiling for the project's clients |
| Token | Existing explicit grants or all permitted models; blocked IDs | Client-specific subset of the project ceiling |

Use exact IDs initially, including namespaced IDs. Do not add glob/regex
rules or implied model-family matches in the first implementation. Explicit
blocks win over allows at either scope. A configured empty allowlist denies
all; inheritance/all-model access must be a distinct explicit mode.

Extend project CRUD with a model policy and credential CRUD with block lists,
retaining the current `models`, `catalog_models` and `vertex_catalog_models`
semantics and interface permissions. Missing new fields in legacy records
preserve their previous grants; project policy defaults to inheritance.
An all-model token follows future administrator-enabled models only within
its current project ceiling; explicit token grants remain limited to their IDs.

In project/token editors, show inherited availability, select allowed/blocked
models from the upstream inventory, and display effective access plus the
reason a model is unavailable. Show the effect of profile/policy changes on
existing credentials. Changes take effect on subsequent requests without
rotation, restart or rebuilding. Keep already accepted streams running.

Example: an upstream enables GLM, Opus and Sonnet; a project allows GLM and
Sonnet; a token inherits project access but blocks Sonnet. That client can
invoke and discover only GLM. No token can restore project-blocked Opus.

### Admin API and frontend

Proposed private admin routes beneath `/api/private/v1`:

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/upstream-profiles/{id}/models` | Inventory, policy, discovery status |
| POST | `/upstream-profiles/{id}/models/refresh` | Refresh provider catalog without generation |
| PUT | `/upstream-profiles/{id}/models` | Save enabled set, manual entries and overrides |

Pass exact model IDs in JSON bodies when editing inventory, avoiding ambiguous
slash-delimited admin resource IDs. Specify concurrency/version handling so
a refresh and editor save cannot silently overwrite each other.

Add a Models view to each upstream: refresh, exact ID, display name,
discovered/manual origin, enable toggle, context/output limits and supported
request policy. Replace token-editor free typing with an inventory selector,
and label catalog access as "Allow all enabled models". Preserve existing
grants when editing; make disabled/unavailable entries visible to administrators.

### Client configuration and medium effort

Provider credentials and model enforcement live on the relay. Avoid requiring
new edits to user `settings.json`: launchers can use `--model` or
`ANTHROPIC_MODEL`, gateway discovery and `CLAUDE_CODE_EFFORT_LEVEL=medium`.
Existing relay clients keep their authentication and endpoint arrangement;
their model selection still needs to point at GLM. Verify custom-ID handling
through Vertex with the actual pinned Claude client.

The relay cannot control all local model-picker rows. Enforce GLM-only access
on the server even if a client displays other models. Pin/check helper requests
as well as the primary session; no hidden fallback to a Claude model.

Verify what the user-facing medium effort setting produces on the wire and
what EnMaaS accepts. Then implement provider-specific normalization/enforcement
of medium effort, rather than assuming an Anthropic field is interchangeable
with an OpenAI reasoning parameter or GLM thinking configuration.

## Implementation sequence and verification

1. Establish the EnMaaS contract: authenticated discovery, model metadata,
   Messages request/response, streaming SSE, tool use and token counting.
   Capture sanitized shapes and capability findings; keep secrets out of evidence.
2. Add managed API-token account support and the EnMaaS adapter, preserving
   OAuth accounts and the repaired timeout behavior.
3. Add persisted upstream/project/token model policy, namespaced-ID handling,
   consistent authorization and catalog normalization with local provider fixtures.
4. Add account/token configuration and Models frontend; update component docs.
5. Verify actual Claude clients through Messages and Vertex, then rebuild and
   validate deployment with a separate GLM-only project and local credentials.

Use synthetic fixtures for auth headers, catalog variants, upstream/project/token
intersection, deny precedence, inheritance versus empty allowlists, immediate
restriction of existing tokens, disabled-model rejection, slash-containing IDs, counting,
streaming tool calls, provider errors, timeout classification and persistence.
Check that enabling/disabling models takes effect without restart and that
existing Anthropic routes/accounts continue working. Run Rust fmt, tests and
clippy plus relevant frontend checks before deployment.

Authenticated service compatibility and the available token source remain
unverified. If count_tokens or other required capabilities are absent, record
that explicitly and design the compatibility behavior before claiming native
Claude support. Use small live checks; do not launch a strategy benchmark
as a side effect of this enhancement.

## Acceptance criteria

- [x] Console can create an EnMaaS account, store/rotate its personal token,
      and assign a project without exposing the upstream token to clients.
- [x] Admin API/frontend manage per-upstream model inventory and enablement.
- [x] Only `rits/zai-org/glm-5-3` is enabled on the initial EnMaaS upstream.
- [x] Project/token API and frontend support granular model allow/block policy.
- [x] Catalog and inference enforce the same upstream/project/token intersection
      with blocks taking precedence and no alias bypass.
- [x] Exact namespaced model ID works end to end on Messages and Vertex.
- [x] Medium effort is verified/enforced through the EnMaaS adapter.
- [x] Streaming, tools, counting, model metadata and errors are validated;
      any compatibility limitations are explicit.
- [x] Existing OAuth accounts, grants, telemetry and timeout repair remain intact.
- [x] Client launch works without additional user settings-file manipulation.
- [x] Tests/docs/deployed checks pass; no benchmark run is launched implicitly.

## References

- Current component docs: `docs/projects.md`, `docs/upstream-accounts.md`.
- [Claude gateway compatibility](https://code.claude.com/docs/en/llm-gateway-protocol).
- [Claude model configuration](https://code.claude.com/docs/en/model-config).
- [Claude environment variables](https://code.claude.com/docs/en/env-vars).


## Implementation and validation, 2026-10-11

- Managed EnMaaS provider uses private atomic API-token storage, rotation and
  disconnect. Existing OAuth account metadata migrates with provider default
  `anthropic`; existing timeout repair remains intact.
- Added `src/model_policy.rs`, SQLite inventories, versioned admin CRUD/refresh,
  manual registration, model metadata overrides, project ceilings and token
  blocks. Older editor updates omitting the new fields retain saved restrictions.
- Console provides provider selection/token entry, Models controls, inventory
  selectors and previews of effective project/token policy with denial reasons.
- Server permits only the exact GLM ID on EnMaaS, regardless of other discoveries.
  Medium effort is enforced in Messages `output_config.effort`; the live endpoint
  accepts that contract. Acceptance is not proof of an internal GLM compute budget.
- Live Bearer authentication returned 401 "missing API key". `x-api-key` returned
  200 for models, GLM generation, SSE, count_tokens, tool use, streamed tool
  arguments and continuation with a tool result.
- The upstream catalog lists Claude models but omits GLM. The initial manual GLM
  entry is retained across refresh, and the discovered Claude models stay disabled.
  Input/output limits remain unknown. SSE reports `zai-org/GLM-5.3`; its response
  spelling is preserved while outbound routing always uses `rits/zai-org/glm-5-3`.
- Native pinned Claude 2.1.293 completed one small turn through each interface
  using only environment/CLI model selection. Vertex used external_account ADC
  and real relay token exchange. No settings.json was created or edited. Helpers
  were pinned to the same GLM model through model environment variables.
- Native Claude emits an unrecognized_model diagnostic and a gateway auto-mode
  notice but completes successfully. Its reported 200k context, 32k output limit
  and estimated dollar cost are client fallback metadata, not verified GLM limits
  or provider billing. The relay inventory leaves limits unset.
- Rust 1.90 production Docker checks: fmt, all 32 tests, clippy -D warnings and
  release compilation passed. Host suite also passed. Tests cover existing OAuth,
  timeout repair, account privacy/rotation/restart, exact IDs, aliases, policy
  updates, catalog refresh, stale inventory edits, counting and streaming tools.
- Playwright exercised the deployed console assets with synthetic admin responses:
  token submission/clearing, Models save, project selectors and token block preview.
  No page errors. Backend functionality was verified separately against the live
  deployed API; the browser test did not mutate live account settings.
- Built and deployed with `make host-rebuild-relay` from the Breadboard root.
  Final pod: `model-relay-9df9b6b5f-dwj8n`, ready, zero restarts.
  Image: `sha256:8ce6d8674802126e50bdff3fa69e6eff4e3ca099a714eb698cc358ed7f2ca96c`.
  Source/build ID: `11ff92fa5011a48c8121e98f5099ec55501359e7ad5ae70b02b90ae6be1e85df`.
- After the final restart, checked build/asset identity, persisted EnMaaS readiness,
  existing `tanner-jc-personal` OAuth readiness, GLM-only inventory/project ceiling,
  both counting interfaces, Vertex detail and immediate token-block enforcement.
- Configured account `enmaas-personal` and project `enmaas-glm`. All temporary
  validation credentials were revoked; issue normal client credentials in the
  console. No benchmark workflow was launched or production project reassigned.

Documentation: [EnMaaS and model policy](../../docs/enmaas-and-model-policy.md).
Sanitized evidence is archived under Breadboard `var/run-data/model-relay/`;
see [the implementation task](../tasks/done/enmaas-model-policy.md).

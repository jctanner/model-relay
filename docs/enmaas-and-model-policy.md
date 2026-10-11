# EnMaaS and model access

In the console, add an upstream account with provider **EnMaaS**, then use
**Set token**. The relay stores the personal token in an owner-only file on
its credential volume. Status APIs never return it. Rotation replaces the file
atomically; disconnect removes it. Assign the account as a project's default
or a model override; see [Project model routing](project-model-routing.md).
Existing Anthropic OAuth accounts retain their login and refresh behavior.

EnMaaS uses `https://api.enmaas.devshift.net/v1`, authenticated upstream with
`x-api-key`. Relay clients continue using their own Messages API keys,
MaaS-format keys, or exchanged Vertex credentials. MaaS keys do not add an
OpenAI inference protocol. The only permitted EnMaaS model is
`rits/zai-org/glm-5-3`; generation requests enforce
`output_config.effort: "medium"`. Other output_config fields are preserved.

The live catalog currently omits GLM despite successful GLM inference. New
EnMaaS accounts therefore start with a manual **GLM 5.3** entry. Refreshing
discovery records other models as disabled and does not change enablement.
Unknown input/output limits are left unset. Manual entries omit unknown
creation timestamps. The provider reports a different model spelling in SSE
message_start; the relay preserves that upstream response.

## Inventory and policy

Use **Models** on an account or upstream profile to refresh discovery, register
exact IDs, enable models and set metadata overrides. Saving configures explicit
policy. A saved empty enabled set denies all models. Profiles without configured
policy retain legacy behavior, including existing deployment aliases.

Effective access intersects upstream enablement, project allowed models and
token grants, then subtracts project/token blocks. Blocks win. Policies resolve
deployment aliases and Vertex date aliases to canonical upstream IDs. The same
ceiling applies to catalogs, details, generation and token counting, including
streaming. Subsequent requests see changes without restart or token rotation;
already accepted streams retain their policy snapshot.

Project `model_policy` is `{ "allowed": null, "blocked": [] }` for inheritance,
or `{ "allowed": ["rits/zai-org/glm-5-3"], "blocked": [] }` for an explicit
ceiling. `allowed: []` denies all. Credential `models`, `catalog_models` and
`vertex_catalog_models` keep their existing interface permissions; add
`blocked_models: []` to narrow them. Omitting the new policy/block fields on
updates preserves saved restrictions. Send an explicit object/empty block
array to clear them. Legacy stored records default to inheritance/no blocks.

Selectors retain unavailable existing grants. The editors show access per API
and reasons such as upstream_disabled, project_blocked, project_not_allowed,
token_blocked and token_not_granted.

## Private admin API

Routes are relative to `/api/private/v1` and require the admin key.

| Method | Route | Body / result |
| --- | --- | --- |
| POST | `/upstream-accounts` | `account_id`, `display_name`, `provider` (`anthropic` default or `enmaas`) |
| PUT | `/upstream-accounts/{id}/token` | `{ "token": "..." }`; returns status only |
| GET | `/upstream-profiles/{id}/models` | Inventory, version, discovery status |
| POST | `/upstream-profiles/{id}/models/refresh` | Merges discovery into latest saved policy; safe error code on failure |
| PUT | `/upstream-profiles/{id}/models` | Inventory with expected `version`; stale edits return 409 |
| POST | `/model-access` | Preview profile/project/token intersection without inference |

Inventory fields: `version`, `configured`, `models`, `refreshed_at`,
`refresh_error`. Entries contain `id`, `enabled`, `manual`, `discovered` and
nullable overrides `display_name`, `max_input_tokens`, `max_tokens`.
Discovery and refresh status are server-owned; client saves cannot forge them.
Refresh failures retain prior metadata and policy and report the error. Refresh
and save use SQLite transactions and versioning to avoid lost edits.

Preview accepts `routing` or legacy `profile_id`, optional `backends`,
`project_policy`, `models`, `blocked_models`,
`catalog_models`, `vertex_catalog_models`; returns each ID, canonical ID,
destination, Messages/Vertex policy access and denial reason. It explains policy, rather
than probing credential readiness or provider inference capability.

## Claude launch

Select the exact model with `--model rits/zai-org/glm-5-3` or ANTHROPIC_MODEL.
For Messages clients use the relay's ANTHROPIC_BASE_URL and local API key;
optionally enable CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1. Set
CLAUDE_CODE_EFFORT_LEVEL=medium. Vertex clients keep the relay token-exchange
setup and select the same exact hosted ID. No additional settings.json edits
are required. Server policy denies accidental helper requests to other models.

Build and deploy the Breadboard instance using `make host-rebuild-relay` from
the Breadboard repository root.

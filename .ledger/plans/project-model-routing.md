# Project model routing

## Scope

Projects choose a default upstream account and exact canonical model overrides.
With no default, only explicitly routed models are accessible. Tokens inherit
routes but retain their existing interface, backend and model grants. Inventory
and project/token blocks still win. Unavailable destinations fail without fallback.

Preserve legacy `upstream_profile` projects and credentials. Explicit routing
updates take effect on the next request, including already issued Vertex tokens.
Protect referenced accounts from deletion and validate assignments transactionally.

Replace the admin Projects Profile column with a routing summary. Add/edit forms
provide a default dropdown, model/account rows and an effective access preview;
keep permissions separate from routing and preserve unavailable selections.

Configure `vertex-shim-local`: default `tanner-jc-personal`, hosted model
`rits/zai-org/glm-5-3` override `enmaas-personal`.

## Validation

Exercise both inference interfaces, counting, streaming, catalog metadata, aliases,
strict routing, policy denials, unavailable accounts and credential continuity.
Run fmt, tests, clippy and diff checks, browser-check the form, deploy using
Breadboard `make host-rebuild-relay`, then verify live routing without printing
secrets. Record results in the task and session log.

## Work

[Implementation task](../tasks/done/project-model-routing.md).

Completed and deployed 2026-10-11; validation evidence is in the task.

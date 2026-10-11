# ADR-0002: Default account plus canonical model overrides

## Status

Accepted, 2026-10-11.

## Decision

Project routing names account/profile IDs. Match an exact canonical model override
first, then the optional default. Without either, deny the request. Never retry a
different account as fallback. Preserve independent upstream inventory, project
policy and token grants. Discovery uses the selected account's metadata.

Authenticate client/project identity before selecting a model's destination.
Google token exchange authenticates the project without selecting a provider;
inference checks backend grants later. Routing changes keep the project epoch so
already issued tokens use new routes. Disabling still changes the epoch.

Legacy records keep their single `upstream_profile`. Explicit routing takes
precedence; omitted/null routing updates preserve it. Account deletion checks
defaults and overrides transactionally with project writes.

## Consequences

One client credential can access permitted models on multiple upstream accounts
without client settings changes. Route changes apply on the next request, while
accepted streams retain their snapshot. An unavailable destination returns an
error, including discovery; callers do not silently consume another account.

See [implementation plan](../plans/project-model-routing.md).

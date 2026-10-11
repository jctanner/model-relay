# Project model routing

The Projects table shows the default account and model overrides. In Add/Edit
Project, choose a default account and add model/account rows. Choose **No default
— explicit routes only** to deny every model without a route. Existing unavailable
selections remain visible for repair. The access preview shows each model's
destination and policy denials. Routing does not grant model permissions.

Project POST/PUT accepts:

```json
{
  "project_id": "vertex-shim-local",
  "display_name": "Built-in development project",
  "routing": {
    "default_upstream": "tanner-jc-personal",
    "model_overrides": {
      "rits/zai-org/glm-5-3": "enmaas-personal"
    }
  }
}
```

Targets name specific upstream account/profile IDs. The relay canonicalizes
deployment aliases and Vertex date aliases before matching exact overrides.
Conflicting destinations for aliases of the same model return 400. Wildcards
and automatic fallback are not supported. A selected destination's error is
returned even if another account could serve that model.

Messages, Vertex, counting and streaming select the destination after project
authentication. Upstream inventory, project policy, token backend grants, token
model grants and interface permissions still constrain access. Model discovery
merges the permitted routed inventories; metadata comes from each model's selected
account. Unavailable destinations fail discovery with 503, and profiles outside
the token's backend grants are excluded. Already accepted requests keep their
snapshot; subsequent requests and existing Vertex bearer tokens use new routes
without rotating credentials. Disabling/deleting a project still revokes tokens.

Legacy projects without `routing` retain `upstream_profile` selection. Omitting
or nulling `routing` on PUT preserves saved explicit routes. Send an explicit
object to replace them; an empty overrides map with a default sends all models
to that account. A null default and empty overrides denies all models. The legacy
field remains readable, but explicit `routing` takes precedence. To change an
explicitly routed project, update `routing`, not only `upstream_profile`.

Accounts referenced by defaults or overrides cannot be deleted, including
references from disabled projects. Project writes validate managed references
inside the same SQLite transaction as account deletion.

Private admin APIs under `/api/private/v1`:

- `POST /model-access` accepts a draft `routing` instead of legacy `profile_id`,
  plus optional `backends`, project policy and token grants. Items include
  `upstream_profile`, canonical ID, interface permissions and denial reason
  (`unrouted`, `upstream_unavailable`, `backend_not_granted` and policy reasons).
- `GET /upstream-profiles/{id}/models/available` returns effective catalog metadata
  without persisting discovery or changing upstream enablement.
- `GET /projects/{id}/auth/status` includes `routing` and per-account `upstreams`.
  Its legacy summary fields describe the default account or are null without one.

Client settings and credential formats stay the same. Select the desired model;
the relay chooses its account. Deploy using Breadboard `make host-rebuild-relay`.

# Model Relay session log

## 2026-10-11 — Recorded EnMaaS and upstream model-management plan

User requested the plan under this component checkout. Created
`plans/enmaas-upstream-and-model-management.md` and linked it from the ledger
index. Captured managed personal-token accounts, exact GLM model ID,
medium effort, per-upstream model inventory/API/frontend, model-grant
intersection, existing client interfaces, identified compatibility gaps,
migration behavior and validation criteria. Implementation remains proposed;
no code, provider credentials, deployment or benchmark configuration changed.

## 2026-10-11 — Added granular project/token model policy

Extended the plan following user discussion: upstream enablement sets the
outer bound, projects set a shared allow/block ceiling and token grants/blocks
narrow client access. Blocks win; exact IDs first; explicit inheritance differs
from an empty deny-all allowlist. Added API/editor scope, effective-access
explanations, consistent discovery/inference checks and migration/test criteria.
No implementation or deployment change.

## 2026-10-11: EnMaaS implementation and deployment

Completed managed API-token accounts, per-upstream model inventory, granular
project/token policy and console controls. Live provider and native Claude
Messages/Vertex checks passed; discovered upstream catalog omission of GLM
and Bearer authentication rejection. GLM is registered manually with unknown
limits and forced medium effort. All 32 Rust tests, fmt/clippy, browser fixture
checks and final deployed persistence/policy checks passed. Deployed only
through Breadboard host-rebuild-relay; retained existing OAuth/timeout changes.
See plan/task for build identity, evidence and client fallback metadata caveats.

## 2026-10-11 — Project model routing

Implemented default accounts, exact canonical overrides and explicit-only mode.
Model-aware selection is shared by Messages/Vertex generation, counting and
streaming; catalogs merge routed metadata with existing policy restrictions.
Routing updates retain credential epochs; model-less Vertex exchange follows the
project, while account deletion checks all effective references transactionally.
Admin Projects now show Routing with default/account rows and access previews;
token forms inherit all effective routes.

33 tests, fmt, clippy, JS syntax, production image checks and browser checks passed.
Deployed using Breadboard `make host-rebuild-relay`. Configured `vertex-shim-local`
with GLM → `enmaas-personal`, everything else → `tanner-jc-personal`. Live counts
passed for GLM/Claude across both APIs with a pre-change Vertex bearer; native
Claude GLM succeeded with the existing cluster workload key. No keys rotated;
temporary validation key revoked. Build ID
`55574d2306bb00d6711eace1a71f4257665b3cd675e15fe2ee89ba5aa57dd3c4`.

Evidence: Breadboard `var/run-data/model-relay/project-routing-20261011/`.
See [task](../tasks/done/project-model-routing.md) and
[decision](../decisions/ADR-0002-project-model-routing.md).

## 2026-10-11 — Publish EnMaaS and routing changes

User authorized committing and pushing all component changes to origin/main.
The earlier managed-account timeout commit is also ahead of the remote and belongs
in the push. Runtime credentials, databases, logs, client work and environment
files remain ignored; the private EnMaaS key source stays outside this repository.
An in-memory scan of all tracked/untracked candidate files passed against the
actual private key and common encodings, plus provider credential/private-key
patterns. No secret values were printed. Recheck the exact staged tree and all
outgoing commit objects before pushing. Formatting, JS syntax and diff checks
passed; the existing 33-test and production validation evidence remains applicable
because source files match the verified deployment fingerprint.

# Model Relay work index

## Completed work

- [Vertex upstream accounts](tasks/done/vertex-upstream-accounts.md): deployed
  2026-10-15; secret JSON editor, Google token caching/refresh and Vertex
  forwarding. 43 tests and browser checks pass. Live Google authentication
  succeeds; built-in default routing now uses the new account, with GLM's
  EnMaaS override preserved. No paid inference.

- [Full-filter telemetry and stress tests](tasks/done/telemetry-aggregates-and-stress.md):
  deployed 2026-10-13; totals independent of table pages. Actual 32/128/256-stream
  stress and minute-crossing continuations preserve requests and events, with
  concurrent reporting and 100,000 history rows; zero paid calls.
  [Results](notes/telemetry-aggregates-stress-results.md).

- [Provider model editor layout](tasks/done/provider-model-editor-layout.md):
  deployed 2026-10-12; compact checkboxes and four-column model rows in a wider modal.

- [Project editor layout](tasks/done/project-editor-layout.md): deployed
  2026-10-12; wider responsive modal and aligned route Remove controls.

- [Project model routing](plans/project-model-routing.md) and
  [implementation evidence](tasks/done/project-model-routing.md). Deployed 2026-10-11.

- [EnMaaS upstream and model management](plans/enmaas-upstream-and-model-management.md).
  Implemented, validated and deployed 2026-10-11: managed personal tokens,
  GLM-only routing, upstream inventory and project/token allow/block policy.
- [Implementation evidence](tasks/done/enmaas-model-policy.md).

## Operational instructions

Build and deploy the Breadboard instance with `make host-rebuild-relay`
from the Breadboard root. See [AGENTS.md](../AGENTS.md).

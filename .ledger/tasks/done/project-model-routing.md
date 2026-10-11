# Task: Project model routing

## Acceptance criteria

- [x] Default account plus canonical model overrides, strict explicit-only mode.
- [x] All inference interfaces and catalogs enforce the same routing and policies.
- [x] Credential continuity and safe account deletion.
- [x] Routing summaries, editable rows and access previews in the admin console.
- [x] Tests, browser checks, requested deployment and built-in project configuration.

## Status

Completed and deployed 2026-10-11. See [plan](../../plans/project-model-routing.md).

## Evidence

- Host Rust fmt, 33 tests, clippy with warnings denied, JS syntax and diff checks passed.
- Production pinned Rust 1.90 Dockerfile repeated fmt, 33 tests and clippy successfully.
- Regression fixture exercises two providers: Messages/Vertex generation, streaming,
  counting, canonical aliases, selected-account catalog metadata, explicit-only mode,
  model-less exchange, policy blocks/backend grants, unavailable destinations,
  account deletion races, omitted update compatibility and routing telemetry.
- Browser source/deployed asset checks used synthetic admin responses: route rows,
  add/remove, default/strict mode, summaries, saving, combined token selectors,
  MaaS submission, secret clearing, no page errors. Backend was tested live separately.
- Built-in `vertex-shim-local`: default `tanner-jc-personal`; GLM exact hosted ID
  override `enmaas-personal`; inherited project policy preserved.
- Live GLM and Claude Opus token counting passed through both Messages and Vertex.
  Vertex used a bearer issued before routing was changed. No paid Claude generation
  was required for the default-account verification.
- Native Claude 2.1.293 Vertex GLM returned OK using the existing `breadboard-cluster`
  workload credential. Existing credential metadata matched before/after; the
  temporary validation credential was revoked.
- Telemetry confirmed actual destinations. Both managed accounts remain ready.
- Pod `model-relay-796898f8b-mfmm6` ready after the required Breadboard rebuild target.
- Build ID `55574d2306bb00d6711eace1a71f4257665b3cd675e15fe2ee89ba5aa57dd3c4`
  matches final source fingerprint.
- Sanitized evidence: Breadboard `var/run-data/model-relay/project-routing-20261011/`.

## Operational behavior

No automatic upstream fallback. Tokens retain interface/model/backend restrictions;
new routes never silently grant access. Unavailable routed accounts fail catalog
requests with 503. Explicit routing takes precedence over legacy `upstream_profile`.
Client settings do not need changes for routing. Source and documentation are
included in the EnMaaS/model-routing publication; runtime credentials are excluded.

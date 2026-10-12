# Task: Compact provider model editor

## Goal

Use normal-sized checkboxes and a wider four-column table, with one compact row
per model and all existing inventory controls preserved.

## Findings

Generic input sizing gives checkboxes a 40px height. The 560px dialog clips the
six-column inventory, exposing only its first three columns. Group identity,
display name, input/output limits and enabled/manual access into four columns.

## Acceptance criteria

- [x] Compact checkboxes and four columns in a wider responsive dialog.
- [x] Preserve all draft values with collection independent of input order.
- [x] Verify desktop/mobile layout and editing against fixture models.
- [x] Run required checks and deploy using Breadboard make host-rebuild-relay.
- [x] Verify deployed assets/layout without changing saved provider policy.

## Status

Completed and deployed 2026-10-12. Browser inspection used non-secret fixture
models; no policy saved and no paid inference requested.

## Verification

- Desktop at 1440px: dialog 1200px, four visible columns, approximately 63px
  rows and 16px checkboxes; no horizontal scrolling.
- Mobile at 390px: dialog 358px, table scrolling contained within the dialog.
- Thirty models: list bounded to 50vh with sticky headers and visible Save footer.
- Enabled, manual, display name and both token limits round-trip correctly.
  Adding a manual model preserves drafts; empty overrides remain null.
- JavaScript syntax, Rust formatting, all 33 tests, clippy with warnings denied
  and git diff --check passed.
- Deployed using Breadboard make host-rebuild-relay. Served HTML, CSS and
  JavaScript exactly match source. Repeated desktop/mobile checks against
  deployed assets with preview overrides removed.
- Anthropic and EnMaaS provider inventories exactly match pre-deploy snapshots.
- Ready pod: model-relay-77bfb666db-xffg7, zero restarts.
- Image: sha256:c7db16b1b9848f6c54f62bb5682f1850076abf9dd18097ef781a14b68e8076db.
- Build ID: af34cdd899b38a2d3bb019cd452a0684938ac47d81efc0b4a0989ee54f6f53ed.

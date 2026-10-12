# Task: Widen the project editor and align route actions

## Goal

Give the project editor enough width for model IDs and upstream accounts, and
align each Remove button with the routing input/select controls.

## Findings

The project dialog inherits the shared wide-dialog width of 560px. At a 1440px
viewport, routing controls are only about 188px wide. Global label bottom margins
and the smaller action button place Remove 13px below the controls.

## Acceptance criteria

- [x] Widen only the project dialog while retaining its viewport-width limit.
- [x] Align Remove with controls; keep routing rows stacked on small screens.
- [x] Check desktop/mobile layout and required component checks.
- [x] Deploy using Breadboard make host-rebuild-relay and verify served CSS.

## Status

Done, deployed 2026-10-12. Layout preview uses the live admin assets with
non-secret in-memory fixture data; no project configuration or credentials changed.

## Implementation and verification

Scoped project-dialog width to 960px, retaining the global viewport/max-height
limits. Removed routing label bottom margins and gave Remove a 40px minimum
height with matching vertical padding. Native row controls have identical top,
bottom and 40px height on desktop. Routing rows still stack below 640px.

Playwright checks on deployed assets, without a stylesheet override:
1440px viewport -> 960px dialog, about 384px routing inputs; 390px viewport ->
358px dialog, single-column routing controls. No horizontal overflow. Preview
also checked representative long model options. Private screenshots remain
in Breadboard's ignored .playwright-mcp directory.

cargo fmt --check, cargo test --offline (33 tests), cargo clippy --offline
--all-targets -- -D warnings and git diff --check pass. The required host build
also runs formatting/tests/clippy before compiling release.

Deployed through make host-rebuild-relay from Breadboard root. Pod
model-relay-85bfd6f4c7-dkqc9 is ready, zero restarts, image
sha256:9f7e1877aee4055ba6cde1c238ca51f6483a6cc42be3ec369f364c81ec19b652.
Health build ID d501c933c226ebf9a7feb6fa045e0bd69d2abcdbfb2872eccf124a17c1e22344.
HTTPS-served CSS exactly matches local source. Development project response
is unchanged before/after deployment, including GLM routing to enmaas-personal
and the default tanner-jc-personal route. No project save or paid inference.

# Task: EnMaaS accounts and model policy

Implement [the plan](../../plans/enmaas-upstream-and-model-management.md),
preserving the existing timeout repair. Add managed token accounts, persistent
upstream inventory, project/token restrictions and console controls. Verify
both inference interfaces and deploy through Breadboard's host-rebuild-relay.

Status: done, 2026-10-11. Managed token source supplied through .env.enmaas.
All implementation and deployment checks passed. See plan for exact findings.

Evidence: Breadboard `var/run-data/model-relay/enmaas-20261011/` contains
sanitized provider/deployed/native-client probes, browser-check summary,
unit/build/deploy logs, final pod details and a SHA-256 receipt manifest.

Final build ID: `11ff92fa5011a48c8121e98f5099ec55501359e7ad5ae70b02b90ae6be1e85df`.
Deployment command: `make host-rebuild-relay` from Breadboard root.

32 Rust tests passed on host and production Rust 1.90; fmt and clippy passed.
Native Claude 2.1.293 completed GLM calls on Messages and Vertex. Temporary
verification keys were revoked. Existing OAuth readiness and the timeout
repair were verified after the final restart. Source and documentation are included
in the EnMaaS/model-routing publication; runtime credentials are excluded.

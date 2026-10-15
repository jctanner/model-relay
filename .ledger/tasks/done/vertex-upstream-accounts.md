# Vertex upstream accounts

## Goal

Add managed Vertex accounts with a secret service-account JSON editor and
explicit upstream project and location. Route the local Vertex client project
through the new account without exposing its Google credential to clients.

## Acceptance criteria

- Add, rotate, disconnect and delete Vertex credentials using the account API/UI.
- Keep JSON private, absent from account responses, SQLite metadata and logs.
- Refresh Google OAuth access tokens with single-flight caching per account.
- Forward Messages and Vertex clients, streaming and counting, to the configured
  real Vertex project/location with correct model IDs and credentials.
- Preserve other provider accounts, model policy, client tokens and telemetry.
- Verify with local OAuth/Vertex fixtures, Rust tests, fmt, clippy and diff checks.
- Deploy through Breadboard's `make host-rebuild-relay`; configure the real account
  once the credential file location is supplied. No paid inference required.

## Implementation notes

Use the existing private account directory and atomic 0600 credential files.
The JSON's token URI must not permit arbitrary outbound credential delivery.
Google OAuth errors must never echo credential or token response bodies.
Token refresh, credential rotation and disconnect share one account lock so a
completed old refresh cannot restore credentials after disconnect or rotation.
Vertex models use explicit administrator inventory because Vertex does not
provide the Anthropic `/v1/models` discovery endpoint. Seed the tested current
Claude models as manual entries; account access is still enforced by Google.

## Validation

Local checks pass: 43 Rust tests, `cargo fmt --all --check`,
`cargo clippy --offline --all-targets -- -D warnings`, JavaScript syntax and
`git diff --check`. The new cases cover API access control/redaction,
credential persistence and 0600 permissions, malformed/redirected credentials,
128 concurrent token users with one exchange, expiration, rotation, disconnect
and safe failure/backoff. A local integration test covers Messages and Vertex
clients, including local Google-token exchange, streaming and counting,
upstream project/location substitution, header isolation and persisted telemetry.
Test RSA keys are generated in memory, not stored as source fixtures.

Breadboard's local credential filename is explicitly ignored, mode 0600, and
untracked. Scanning 1,241 source files across both repositories found no actual
key material. The real key is only read for private API import, not copied into
source or command arguments. Deployment and live Google authentication complete.

Live configuration replaces the built-in project's default destination;
its existing GLM-to-EnMaaS override remains in place.

## Deployment evidence

Deployed 2026-10-15 using Breadboard's `make host-rebuild-relay`; the image's
test/format/clippy checks passed and the deployment rolled out healthy. The
new `vertex-strat-evals` account successfully exchanged its service-account
assertion for a real Google access token. Safe API readback reports the requested
project and `global` region, `google-service-account` authentication and readiness.
Credential directory/file modes are 0700/0600. The built-in development project's
default route now selects this account; its GLM override still selects
`enmaas-personal`. All previously configured accounts remain present.

Browser checks against deployed assets with synthetic admin responses confirm
native password masking, explicit reveal/hide, clearing on close, blank-secret
edits preserving the saved key, Vertex-only manual inventory controls, provider
selection, a 760px desktop modal and 358px modal at a 390px viewport. No browser
JavaScript errors. No real credential entered the browser test.

Real model inference was not run: Google authentication is verified, but model
enablement, Vertex prediction IAM permissions and model quota remain untested.
No model tokens were spent. Source is on `feat/vertex-upstream-accounts`.

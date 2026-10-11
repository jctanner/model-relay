# Model Relay

Keep plans, tasks and investigation notes in `.ledger/`.
Preserve existing account credentials and SQLite data during upgrades.
Never log upstream tokens or return them in account API responses.

Run Rust formatting, tests and clippy and `git diff --check` for changes.
Build and deploy the Breadboard `relay.local` instance with
`make host-rebuild-relay` from
`/home/jtanner/workspace/github/jctanner.redhat/breadboard`.
Do not substitute a component-local deployment command.

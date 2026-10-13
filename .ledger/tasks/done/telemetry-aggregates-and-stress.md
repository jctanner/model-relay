# Task: Full-filter telemetry aggregates and concurrent stress coverage

## Goal

Keep console totals independent of loaded request pages and verify concurrent
SQLite reporting and streaming at dozens and hundreds of synthetic clients.

## Acceptance criteria

- [x] Preserve raw history and existing paginated API contracts during migration.
- [x] Provide bounded server aggregates using the same scope filters as lists.
- [x] Keep console totals stable across Load more and Refresh; label scopes.
- [x] Add reusable isolated stress tooling with no paid inference.
- [x] Exercise 32, 128 and 256 concurrent clients, historical data and live readers.
- [x] Reconcile final request IDs, usage, SQLite health, migration and restart.
- [x] Run fmt, tests, clippy, frontend checks and git diff --check.
- [x] Build/deploy via Breadboard's make host-rebuild-relay when safe.

## Status

Done, 2026-10-13. Implemented, stress tested and deployed; no commit/push requested.

## Design

Add a full-filter aggregate endpoint while retaining bounded summary/list/export
APIs. A typed reporting projection is backfilled from existing raw rows and
updated in the telemetry writer's transaction, together with global/minute
rollups. Partial boundary minutes preserve exact time filters. Summary reads
use one snapshot, bounded workers and a short bounded cache. The console
fetches aggregate totals independently of the table and never treats 100 rows
as the whole selected history.

Stress runs use a temporary relay/database and synthetic streaming traffic,
including continuous summary reads and a large retained-history fixture.
Protect existing credentials and deployment data. No commit/push is requested.

## Evidence

[Stress results](../../notes/telemetry-aggregates-stress-results.md): all six
32/128/256-client cases pass on one CPU with 100,000 history rows; 16,640 new
requests and 1,028 aggregate reads, zero dropped requests/events or storage
errors. Initial read timeouts and event-buffer pressure were found and corrected.
Two additional minute-crossing cases passed: 23,040 requests, 391,680 independently
verified stored events and 1,445 aggregate reads. Total successful stress traffic:
39,680 requests. Deployed database backup is private on its existing volume;
all 4,859 raw requests, control data and credential files survived the required
host rebuild unchanged. Full-filter totals match raw SQLite and served console
code matches source. No deployed inference or paid model calls occurred.

# Full-filter telemetry and concurrent stress results

Local validation and deployed verification: 2026-10-13.

## Change

The console overview now reads `/telemetry/aggregate`, independently of its
paginated request table. A 150-request console fixture retains totals of 150
requests and 450 synthetic output tokens across Load more and Refresh, while
loaded rows move from 100 to 150 to 100. A failed aggregate refresh retains the
previous complete view. The existing summary endpoint still summarizes a page.

Raw request snapshots, their indexed reporting projection, and global/minute
rollups commit in one writer transaction. Updating a streaming request replaces
its earlier contribution, including status, model, scope and latency changes.
Whole-minute totals serve time ranges; only partial boundary minutes inspect
individual records. Totals and model bars use the same read snapshot. WAL,
the 250 ms execution budget and 500 ms lock wait remain; aggregate work is
bounded to two workers and a 64-selection, one-second cache.

## Failures found before the final pass

The initial indexed-row aggregate still scanned history. With 100,000 historical
rows and concurrent streams, its read budget produced HTTP 500 responses: 1,
32 and 38 in the 32-, 128- and 256-client reader cases. Every final inference
record survived. Maintained rollups removed these reporting failures without
increasing the read timeout.

The rollup pass also exposed event shedding at the existing 1,024 pending-write
threshold, including 7,049 intermediate usage events in its 256-client reader
case. Final request records and token totals reconciled. The bounded event
threshold is now 8,192; the request threshold remains 65,536. The final matrix
requires zero event drops, as well as zero request drops and storage errors.

## Final matrix

Each case uses an optimized relay pinned to one CPU, 32 fixture keys, two models,
128 KiB user-content payloads, 100,000 legacy historical rows and 20 requests per
client. A loopback provider holds streams for 0.7 seconds, then sends 16 cumulative
usage updates at 5 ms intervals. Cases run sequentially on fresh temporary data.
The history fixture uses one historical timestamp and run; this matrix does not
measure highly diverse, long-retained filter combinations.

| Clients / measured active streams | New requests per case | Requests/s, no readers | Requests/s, four readers | Aggregate p95, four readers | Peak RSS, four readers |
| --- | ---: | ---: | ---: | ---: | ---: |
| 32 / 32 | 640 | 39.21 | 39.29 | 4.99 ms | 37.39 MiB |
| 128 / 128 | 2,560 | 149.22 | 148.73 | 18.05 ms | 81.31 MiB |
| 256 / 256 | 5,120 | 249.37 | 244.24 | 48.43 ms | 122.59 MiB |

All six cases passed: **16,640 requests and 1,028 reporting reads**. Every
inference and aggregate response was HTTP 200; final request IDs and output usage
matched independent raw SQLite queries and survived restart. All writer queues
drained. Dropped requests, dropped usage events and storage errors were zero.
The highest sampled writer backlog was 2,190; the maximum sampled RSS across all
six cases was 122.59 MiB. Legacy startup/backfill took about 1.8 seconds per case.

The reporting readers rotate through all history, all-project history, active
run, recent project and active run/key scopes. Their latency measurements include
cache hits. The throughput pairs are single observations, not statistical claims.
Memory is sampled, not capped; CPU affinity is not a Kubernetes cgroup limit.
Loopback transport and artificial pacing do not establish real provider capacity,
network/TLS performance, or multi-hour service reliability. Paid provider calls:
**zero**. Temporary fixture credentials and raw logs are not retained in this note.

## Reproduction

From the component root:

```bash
./scripts/test-telemetry.sh
cargo build --locked --offline --release
python3 tests/stress_telemetry.py --one-cpu --binary target/release/vertex-shim \
  --concurrency 32,128,256 --waves 20 --history 100000 --readers 4 \
  --payload-bytes 131072 --output target/telemetry-stress/full.json
```

The harness creates and removes its own relay, provider and SQLite directory,
uses only generated credentials, excludes host provider configuration, and sets
the model executable to `/bin/false`. Socket access must be allowed. The latest
harness additionally reconciles retained rows and every intermediate event.
Reports and earlier failing matrices are under ignored `target/telemetry-stress/`.

Rust: 37 tests, formatting and clippy passed. JavaScript syntax, Python compilation
and shell syntax passed. Inclusive timestamp, timezone, empty scope, counting
request, migration, replacement-update, cancellation, refresh and restart checks
are covered.

## Minute-boundary continuation

The strengthened harness ran two additional 128-client cases, 90 requests per
client, with and without four reporting readers. Each case crossed minute
boundaries and produced 11,520 requests and exactly 195,840 stored usage events
(17 per stream). Independent SQLite checks found all 111,520 retained rows and
correct full-history and run-specific aggregates. Both cases survived restart
and had zero dropped requests/events or storage errors; all 1,445 reporting reads
returned HTTP 200. This brings the successful local stress traffic to **39,680
requests and 2,473 reporting reads**.

These continuation cases overlapped the image build. Their roughly 74–76 second
durations and latency values should not be used for a reader-overhead comparison.
They check event preservation, growing history and minute-boundary behavior.
Results: ignored `target/telemetry-stress/minute-crossing.json`.

## Deployment

Built and deployed with Breadboard's `make host-rebuild-relay`. Its image build
also passed Rust formatting, all 37 tests and clippy. No jobs were active before
the upgrade. A consistent SQLite online backup passed integrity checking and
is stored privately under the existing data volume's `backups/` directory.

After rollout, all **4,859** existing raw requests and their generation-token
totals were unchanged. The aggregate endpoint matched those independently
calculated raw totals. Control-table fingerprints and account-file fingerprints
were unchanged; credentials were neither printed nor saved in these results.
The database passed integrity checking and reporting migrations 1 and 2 were
present. Served console JavaScript matched the updated source byte for byte.
The new pod reported zero pending writes, dropped requests/events and storage
errors. No deployed inference was invoked.

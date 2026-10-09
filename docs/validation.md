# Container validation, 2026-10-08

All Rust compilation, tests, lint checks, and CLI execution ran in rootless
Podman containers. The host ran only development supervision/test launchers.

Validated artifacts:

- Initially Claude Code 2.1.280, now 2.1.293, installed in the client image with Anthropic's curl
  installer; `claude --version` checked during build and in an unprivileged,
  network-disabled runtime container. No host CLI installation.
- Rust 1.90.0, Axum 0.8.9, Tokio 1.52.1, rusqlite 0.37.0,
  reqwest 0.12.28, and `Cargo.lock`.
- Digest-pinned Rust, Debian, and Python fixture base references in the
  Containerfiles/launchers. Package repository and installer inputs remain mutable.

Passing checks:

| Command | Evidence |
| --- | --- |
| `scripts/test-client.sh` | Native CLI sends local bearer credentials and Vertex SSE URLs on an internal network without Google credentials |
| `scripts/test.sh` | Thirteen Rust tests, formatting, and Clippy with warnings denied; unit container has `--network none` |
| `scripts/smoke.sh` | Native Claude Code talks to Rust mock; 15 REST probe requests per run cover auth, counting, cancellation/partial usage, concurrency, privacy, query validation, pagination, and summary accounting |
| `scripts/test-host-oauth.sh` | Native CLI over the OAuth adapter; read-only source reuse, access-only projection, atomic rotation, expiry/logout/permissions, and coordinated cleanup when the watcher stops; zero refresh requests |
| `tests/test_oauth_mirror.py` | Two offline Python tests verify source contents stay unchanged, refresh/unrelated credentials are excluded, atomic replacement is followed, and invalid source state removes stale mirrors |
| `scripts/test-upstream.sh` | Native Claude Code through the real Rust adapter plus 14 REST requests against an isolated synthetic Messages provider; zero fixture contract failures; container stdout privacy and lifecycle checks |
| `scripts/test-messages.sh` | Claude Code 2.1.293 in Messages mode; three-model discovery, pagination, per-key filtering/revocation, credentials checked despite cache, five Messages/counting requests, existing Vertex regression, and 20 matching stdout completion logs |
| `scripts/test-mock-adc.sh` | Native Google ADC without static auth injection; two turns in one process, short-lived-token renewal, actual expiry/rejection, concurrent tokens, revocation/rotation, listener separation, old API-key compatibility, private identity attribution, and log/REST privacy |
| fresh-cache `scripts/test.sh` | All eight tests and lint checks run offline using an empty Cargo volume seeded from the rebuilt dev image |
| `scripts/test-stack.sh` | Same-container build/restart, persisted usage after container recreation, coordinated cleanup after client failure, and launcher shutdown |
| `scripts/test-dual-client.sh` | Concurrent native Messages/API-key and Vertex/mock-ADC clients against one proxy; mixed REST auth/API combinations, independent revocation, admin separation, distinct telemetry identities, home/work isolation, process/container restart, persistence, coordinated three-container cleanup, and stdout privacy |
| runtime image build | Clean source-baked build runs format checks, unit tests, Clippy, and release compilation |
| `scripts/test-runtime.sh` | Native client and REST acceptance pass against baked Rust image with no source mount |

REST health reported zero dropped records and zero write errors in the acceptance
runs. Telemetry survived process restart and container recreation with the same
volume. The incremental restart retained the proxy container ID and updated its
source/dependency build ID when the source changed.

Compatibility findings and resolved defects:

- The pinned native CLI appends `/projects/...` directly to its base URL. It
  therefore requires `http://vertex-shim:8080/v1`, despite the current documentation
  showing an origin-only example.
- Podman's OCI build ignored the Dockerfile `SHELL` directive; Bash-dependent
  build steps now invoke Bash explicitly.
- Rootless file mounts containing mode-0600 synthetic keys require consistent
  UID mapping; launchers use `--userns keep-id:uid=1000,gid=1000`.
- Lifecycle cleanup now tolerates a container disappearing between the existence
  and ownership checks. Cleanup only removes session-owned stack resources.
- Acceptance tests use ephemeral published ports and unique session names to
  avoid affecting the running development stack.

Probe outputs and lifecycle logs are preserved in ignored `.state/` directories.
Synthetic secret files are deleted when disposable acceptance suites finish.
Development telemetry/client/cache volumes are preserved by normal shutdown.

Structured stdout logging was validated against actual Podman logs and the REST
records: 15 accepted requests each emitted exactly one terminal completion log,
including streamed usage, cancellation, and provider failures. Failed requests
also emitted a correlated `upstream_failure`; two rejected authentication attempts
emitted `request_rejected`. Process replacement produced startup and graceful
shutdown JSON events. Token counts, partial usage, status, and latency matched
the REST records. Synthetic inference/admin/upstream keys, prompt/response/tool
markers, custom headers, and provider error messages were absent from stdout.
A Rust test additionally verifies arbitrary usage/context strings and client/run
labels cannot enter completion logs. No dependency or dev image change was needed.

The Anthropic adapter fixture verifies generation/counting URL and body
translation, explicit model mapping, version/beta/workspace headers, server-owned
bearer auth, rejection before upstream access, credential isolation from the
client, and re-reading a rotated token. A tool-use response retains its original
input block without storing arguments in telemetry. SSE frames are split inside
JSON/UTF-8, delayed to prove incremental delivery, and optionally gzip-compressed.
Usage snapshots of 0/1/2/3 produce a final output count of 3; the private events
endpoint retains those snapshots. Truncated, malformed, timed-out, errored, and
cancelled streams retain their observed partial usage. HTTP 429/500/307 preserve
status and retry timing with exactly one fixture attempt each; redirects are not
followed. Private records/events contain neither prompt/response/tool content nor
credential values. Mixed summary pages are labelled separately by synthetic and
provider request counts.

The default mock dev proxy was rebuilt/restarted in its existing container after
these changes. Mock lifecycle acceptance and baked-runtime native CLI/REST
acceptance were repeated successfully. Dev/runtime images were rebuilt because
the dependency graph changed; routine source edits continue to use exec/restart.
Image-prepared dependencies now merge into persistent Cargo caches after upgrades;
an entirely empty cache was tested without network access.

Live OAuth validation used the user's selected existing host login without
modifying its credential file. The host watcher mirrored only access/expiry/scopes
into a separate project directory. The proxy mount is read-only; the client has
no mirror/source mount. The host CLI owns login and refresh. The proxy retained
its `vertex-shim` user agent and added only the OAuth beta header to identify the
credential mode. No native-identity/system-prompt spoofing was added.

The final Claude Code turn, with `MAX_THINKING_TOKENS=0`, output cap 128, no tools,
one agent turn, and SDK transport retries zero, returned `OK.`. The last request
had HTTP 200, complete streamed usage, input 3, output 5, cache creation 192, and
cache read 5,949 tokens. Normalized input occupancy was 6,144; configured context
capacity remains unknown/null. First-event latency was 1,573 ms; completion
latency was 1,576 ms. These are provider-reported counters, not a billing total.
Private records/events and SQLite health were available during the live run.

The minimal REST compatibility request returned HTTP 429 (`rate_limit_error`).
The initial native probe's 16-token cap triggered four separate client truncation
recovery requests despite zero SDK transport retries; each was HTTP 200 with 16
reported output tokens. These were client requests, not proxy retries. The
completed 128-token-cap turn made one generation request. The live session thus
contains six request records: one 429 and five completed streamed generations.
Do not infer arbitrary-client compatibility from the native-client success.

The final source-baked runtime was rebuilt with all eight Rust tests, formatting,
Clippy, and release compilation; native mock client/REST acceptance and mock
lifecycle acceptance were repeated successfully after the OAuth changes. No dev
image rebuild was needed for this source-only change. Test/validation logs are
preserved under ignored `.state/validation-oauth-2026-10-08/`; live telemetry stays
in the `liveoauth` session volume. The default mock session remains separate.

Scope limits: synthetic suites do not establish provider entitlement or billing.
The live run proves a basic native-client turn for this account/model/version,
not a supported general subscription-token API contract. Other live model/counting
operations, real agentic tool execution, independent login/refresh, proxy retry
accounting, grouped summaries, and retention/export CLI remain pending. Host-token
reuse requires host-side refresh activity; expired mirrors fail closed until the
source changes. Generic bearer-file mode still requires in-place updates to its
individual mounted file. OAuth mirrors follow directory-level atomic replacements.

The subsequent Messages/model-discovery implementation was validated with
Claude Code 2.1.293, installed using the same curl-based container installer.
Ten Rust tests, formatting, and Clippy passed. The Messages acceptance stack ran
the native CLI against a three-model provider catalog, then the 14-request Vertex
regression and five Messages/counting requests. Discovery pagination, model
detail, per-key filtering, immediate permission changes, unknown-model rejection,
and credential checks while metadata is cached all passed. Twenty accepted
requests had matching terminal stdout records; content/credential privacy checks
passed. Mock lifecycle regression also passed with the upgraded native client.

The live OAuth Models API returned 14 models. The live client was recreated in
Messages mode with its existing home/work volumes; only its existing inference
key received explicit `catalog_models: true`. The generated model picker has
14 unique native IDs and upstream display names, and native gateway discovery
cached those same 14 models. An interactive `/model` screen showed the configured
lineup instead of the prior all-Sonnet aliases. Standard Default/current-model
rows remain native CLI behavior. Catalog refresh requires reopening Claude and
may use metadata cached by the proxy for up to 60 seconds.

One bounded native Sonnet 5.5 turn through `/v1/messages` returned `OK`, HTTP 200,
complete streamed usage, input 2, output 4, cache creation 2,166, and cache read 0.
The Models API supplied a 1,000,000-token context capacity; normalized occupancy
was 2,168. The live proxy stdout contained neither the local keys nor its OAuth
access token. Host login/refresh ownership and credential isolation are unchanged.
Other listed models have catalog availability evidence, not live generation
validation. The existing source-baked Rust runtime image predates these new
interfaces; development runs use the compiled source-mounted proxy. Build the
runtime image from current source when testing or deploying that artifact.

The live client's `/work` was subsequently migrated from its named volume to a
writable bind mount of `client-work/liveoauth/` on the host, without rebuilding
images. About 1.8 GB of existing repositories were copied with metadata and
symlinks preserved; a comparison without dereferencing links found no
differences. The original named volume remains as a backup. Host-to-container
and container-to-host writes were verified, including host ownership of files
created inside the rootless client. Its existing Internet network attachment was
restored after recreation, and GitHub returned HTTP 200. Development clients now
default to per-session host work directories; fixture scripts explicitly select
their existing disposable named work volumes.
Shell syntax checks and the mock stack acceptance suite passed after this
change, including the native CLI, 15 REST requests, process restart, container
recreation with persisted telemetry, and coordinated service cleanup.

Optional mocked Google ADC passed with native Claude Code 2.1.293. The client had
no mounted inference-key file or Google credential file, and its wrapper removed
the skip-auth flag and authorization override. The Google auth library discovered
the proxy's metadata listener, obtained a synthetic token, and completed Vertex
requests. Tokens were reusable, concurrent tokens stayed valid until expiry, and
registry revocation, backend/model denial, and key rotation applied immediately.
Two native turns in one process also passed with a two-second lifetime and a
three-second pause; the expired credential was rejected by REST and a renewed
one succeeded. This native build fetched fresh metadata tokens on each observed
inference turn; the test does not claim cross-turn client caching.

`SHIM_CLIENT_AUTH=mock-adc SHIM_CLIENT_API=vertex scripts/test-upstream.sh`
passed the native turn, the 14-request adapter regression, and 16 matching
terminal stdout records against the synthetic upstream. The same options with
`scripts/test-host-oauth.sh` passed native inference through the access-only
OAuth mirror, atomic host rotation/logout/expiry, zero refresh requests, and
watcher-failure cleanup. These show that mocked Google tokens are replaced by
the selected server-owned upstream credential. Regular Messages/API-key
acceptance also passed with 20 completion records. No new paid provider calls
were made for this feature, and the running live OAuth client was not restarted
or switched away from Messages mode. The source-baked runtime image predates
mocked ADC; current development containers compile the mounted source.

## Three-container stack

The default Honcho stack now supervises the proxy, a Messages/API-key client,
and a Vertex/mock-ADC client. `SHIM_CLIENT_LAYOUT=single` selects the existing
single-client fixtures. Native Messages authentication now uses
`ANTHROPIC_API_KEY` and was observed as `auth_scheme: x-api-key`; Vertex still
uses real native ADC acquisition against our synthetic metadata listener.

The dual acceptance suite passed on the final launcher environment settings.
It exercised both native clients concurrently; twelve concurrent REST requests
covered Messages/Vertex with API-key, ordinary bearer, and mock-ADC auth; revoking
either identity did not invalidate the other. Clients had separate home volumes
and writable host bind directories, and the Vertex client had no inference-key
or upstream credential mount. Metadata port 8082 remained unpublished. Telemetry
survived process restart and container recreation. Stopping the Vertex client
terminated all three supervised containers and removed the session network.
Artifacts: `.state/validation-dual-client-final-env.log` and
`.state/dual-1791511973-1792005/`.

`SHIM_CLIENT_LAYOUT=dual scripts/test-host-oauth.sh` also passed both native
clients through the synthetic OAuth provider, host rotation/logout/expiry, zero
refresh calls, and coordinated cleanup after credential watcher failure.
Messages and single-client lifecycle regressions passed unchanged, including
20 matching stdout completion records and 15 REST lifecycle probe records.
The final single-client mock-ADC regression passed acquisition, reusable and
concurrent tokens, revocation/rotation, expiry/renewal, listener isolation, and
privacy (`.state/validation-dual-adc-regression.log`).

With user approval, the running `liveoauth` stack was upgraded in place. Its
existing primary Claude home, `client-work/liveoauth/` bind mount, API-key grants,
999 historical telemetry records, and host ports 43173/44443 were preserved.
The new Vertex client uses its own home and `client-work/liveoauth-vertex/` bind
mount. Both clients and the proxy retain the explicit Internet egress network.

Two concurrent bounded native live turns returned `OK`: the primary used
`claude-haiku-5-5` over Messages with `x-api-key`, and Vertex used
`claude-sonnet-4-5-20250929` with `mock-adc`. Private REST telemetry confirmed
HTTP 200, complete usage, different client identities, and server-side
`upstream_auth: claude-oauth` for both. Neither client sees upstream OAuth;
both resolve GitHub. `.state/liveoauth/dual-validation.json` contains the safe
REST evidence; `.state/liveoauth/start-dual.sh` preserves the launch settings.
This verifies the current Vertex model mapping, not a dynamic Vertex model menu.

## Vertex Haiku selection and deployment picker

Claude's reported Google authentication failure for `--model=claude-haiku-5-5`
was a local model-grant rejection: mock metadata issuance succeeded, but the
Vertex registry only permitted Sonnet 4.5. Haiku 5.5 now has an explicit
deployment mapping and grant in the shipped defaults and live Vertex identity.

`GET /v1/models?client_api=vertex` lists only configured deployments authorized
for the requesting key, using upstream display metadata where available.
Messages `catalog_models` permission does not broaden this list. The mock-ADC
wrapper uses a synthetic token to fetch this deployment list and builds a
restricted model picker. Native inference still obtains its own token through
Google ADC. Unsupported explicit model arguments fail with a clear wrapper
error before native startup. The menu represents explicit local mappings,
not all models offered by Google or the upstream catalog.

Twelve Rust tests, formatting, and Clippy passed. The updated dual acceptance
suite passed native Haiku selection, clear rejection of unauthorized model
arguments, deployment-list filtering despite broad Messages catalog permission,
and the existing concurrent-client/isolation/lifecycle checks. ADC acquisition,
expiry/renewal, revocation/rotation, privacy, and Messages regressions passed.
Logs: `.state/validation-vertex-picker-{unit,dual,adc,messages}.log`.

The live proxy process was rebuilt/restarted without recreating any containers.
An actual interactive PTY session launched with `--model=claude-haiku-5-5`,
displayed `Claude Haiku 5.5 · Google Vertex AI`, and replied `OK`. `/model`
displayed Default plus exactly the authorized Haiku 5.5 and Sonnet 4.5
deployments. Launches without a model were confirmed to have exited normally
at the user's request; no silent-exit defect was observed.

## Full upstream catalog in Vertex

Vertex now supports `vertex_catalog_models`, an explicit permission independent
of Messages `catalog_models`. Fresh dual-client Vertex identities receive it;
the live Vertex identity was enabled at the user's request. Discovery returns
every upstream catalog model using native Vertex route IDs, and inference
resolves those same IDs without static deployment entries. Dated IDs translate
between `-YYYYMMDD` upstream and `@YYYYMMDD` Vertex forms; the wrapper accepts
both spellings. Unknown IDs are rejected before inference. Explicit deployments
and their window overrides remain available to explicitly granted identities.

Thirteen offline Rust tests, formatting, and Clippy passed. The dual-client
acceptance suite verified native Opus 4.6 with no explicit route/model grant,
dated-ID normalization, clear unknown-model errors, discovery filtering after
permission revocation, and existing concurrency/isolation/lifecycle behavior.
The OAuth fixture verified catalog-only Opus 4.6 via Vertex generation, SSE, and
counting, with 1M context metadata attributed to the upstream Models API in
private telemetry. Pagination, backend denial, and revocation despite cached
catalog metadata passed. Messages and mock-ADC regressions also passed.
Logs: `.state/validation-vertex-catalog-{unit,dual,oauth,messages,adc}.log`.

The live proxy process was rebuilt/restarted without recreating containers,
then catalog permission was enabled atomically in the live Vertex registry.
Discovery exposed 14 unique models, including `claude-opus-4-6`, exactly matching
the Messages catalog after dated-ID normalization. The safe metadata snapshot is
`.state/liveoauth/vertex-full-catalog.json`. This is the Anthropic upstream
account's catalog presented through the simulated Vertex interface.
An actual interactive Opus 4.6 session returned `OK`; `/model` showed the full
catalog with Opus 4.6 selected. Provider telemetry confirmed HTTP 200, complete
usage, `auth_scheme: mock-adc`, `upstream_auth: claude-oauth`, and context capacity
from the upstream Models API. Safe request evidence is saved in
`.state/liveoauth/vertex-catalog-opus-validation.json`.


## Multi-project control plane and client authentication

The project implementation passed 15 Rust tests, formatting, and Clippy with
`-D warnings` in the offline rootless development container. Additional tests
cover project and managed-key CRUD, effective grant replacement, cross-project
403s, rotation, disable/re-enable, token incarnation revocation, SQLite restart
persistence, retained history, migration of pre-project records, and fail-closed
selection of missing upstream profiles.

`./scripts/test-projects.sh` passed with two concurrently running native Claude
Code 2.1.293 clients, each in a separate rootless Podman container, home volume,
and work bind mount. Both used file-sourced external-account ADC and the local
mock STS exchange, with no skip-auth flag or custom bearer header. Project Alpha
used a synthetic upstream bearer account; Beta used an independent access-only
OAuth mirror/account. Both selected catalog-only Opus 4.6. Separate fixture
providers observed zero credential contract failures and zero refresh calls.
REST requests, counting, cross-project denial, admin separation, rotation,
disable/re-enable, process restart, deletion, and project-scoped telemetry all
passed. Client/project secrets and prompt/response text were excluded from
stdout and REST telemetry. Temporary containers and fixture credential files
were cleaned up.

Regression suites passed for the default dual clients, mock metadata ADC
(including two native turns, expiry/renewal, ambiguous identity rejection, grant
revocation, and key rotation), and the dual-client read-only host OAuth fixture.
The OAuth fixture's base-provider module now guards its server startup with
`if __name__ == "__main__"`, allowing the OAuth subclass to execute its
OAuth-beta contract check instead of blocking during import.

The live `liveoauth` proxy was upgraded by compiling and replacing its process
in the existing container. Before migration, a consistent SQLite backup was
saved to `.state/liveoauth/telemetry-before-projects.sqlite`. All 1,016 existing
records were retained and assigned to the built-in `vertex-shim-local` project.
A temporary live validation project exercised admin CRUD, the mode-0600
one-time key helper, mock STS token exchange, cross-project 403, private admin
401, and key/token revocation after deletion. Existing API-key and metadata
clients both continued to discover all 14 authorized models. The temporary
project/key was deleted. This live verification made catalog reads and local
auth/control calls, without paid inference or host credential modification.

Artifacts:

- `.state/validation-projects-unit.log`
- `.state/validation-projects-containers.log` and
  `.state/projects-1791515746-2144087/`
- `.state/validation-projects-dual.log`
- `.state/validation-projects-adc.log`
- `.state/validation-projects-oauth.log`
- `.state/liveoauth/projects-validation.json`

See [project API and launcher examples](projects.md). Separate upstream profile
mirrors are server-mounted read-only inputs; their producers own login/refresh.
The existing host watcher still manages only its original default mirror.
Projects/grants are dynamic REST resources; installing additional upstream
profile mounts requires stack recreation, and changing installed profile
configuration requires a proxy process restart.

## Browser admin console, 2026-10-09

The console was exercised against a disposable rootless Podman mock relay,
with an internal network and synthetic credentials. Browser checks covered
login rejection and reconnection, project creation/editing/deletion, safe text
rendering of HTML-looking labels, all three credential types, ephemeral MaaS
issuance, grant editing, rotation, and revocation. A rotated MaaS key successfully
authenticated an inference request and was rejected after revocation; inference
credentials could not read the admin API.

Telemetry checks loaded more than 100 requests across cursor pages, verified
that generation token totals exclude counting requests, filtered by project,
opened usage events, and downloaded the loaded telemetry as JSON. Desktop and
390-pixel mobile layouts were checked; disconnect and reload clear the admin
key, issued-secret field, and displayed data. No localStorage or sessionStorage
entries were created. The final browser checks reported no JavaScript errors.

`scripts/test.sh` passed all 18 Rust tests, formatting, and Clippy in Podman.
The UI tests also verify browser response headers and that the page is served
only on the admin listener while private data routes retain bearer protection.
JavaScript syntax, shell syntax, and diff whitespace checks passed.

Artifacts are under ignored `.state/admin-ui-validation/`, including
`rust-checks.log` and `telemetry-desktop.png`. No live inference or upstream
usage calls were used for these checks.

The running `liveoauth` proxy was then rebuilt and restarted in place. Its
HTML/CSS/JavaScript responses match the checked source byte-for-byte; unauthenticated
private API access returns 401, the UI is absent from the inference listener,
and storage health reports no pending writes, dropped records, or errors.
Evidence: `.state/admin-ui-validation/live-verification.json` and
`live-upgrade.log`.

## Token form simplification, 2026-10-09

The token form now inherits the project's upstream provider, showing Anthropic
OAuth for the live session. Backend grant and Google-exchange checkboxes were
removed. Anthropic and MaaS API keys receive direct API access; new Vertex
workload keys receive exchange-only access. One model-catalog checkbox maps to
the selected client interface. Existing credentials retain their auth grants.

`./scripts/test.sh` passed 19 Rust tests, formatting, and Clippy with warnings
denied. The new routing test covers raw workload-key rejection on Messages,
Vertex, and discovery, successful Google exchange and Vertex catalog access,
restriction preservation on PUT and rotation, and revocation of exchanged
tokens. `node --check src/admin_ui/admin.js` and `git diff --check` passed.

Browser checks against a disposable rootless Podman mock stack verified all
three token types, their derived grants, direct-inference/exchange behavior,
grant edits, unchanged MaaS expiration, and preservation of legacy combined
permissions. The simplified dialog fit a 390-pixel viewport. No real upstream
requests were needed. The temporary container and network were removed.

The liveoauth proxy was rebuilt in its existing container and its process
restarted. Served HTML/JS matched the workspace assets; private health reported
zero storage errors and dropped records. The console remains at
`http://127.0.0.1:44443/admin/`.

## Telemetry project and key selector, 2026-10-09

The telemetry selector now groups projects with their individual MaaS,
Anthropic, Vertex workload, and legacy combined keys. A read-only, paginated
`/projects/{project}/telemetry/keys` inventory includes both SQLite-managed
credentials and file-registry bootstrap keys without secrets or verifiers.
Key selections apply both project and key filters. Inventory loading uses at
most four concurrent project requests; issuance, edits, revocation, and refresh
update the selector. Revoking a selected key retains its historical scope.

All 20 Rust tests passed through `./scripts/test.sh`, including inventory
pagination, authentication/listener separation, bootstrap and managed key
types, disabled-project identities, and telemetry isolation between two keys
sharing a client ID. Formatting, Clippy, JavaScript syntax, and diff checks
passed.

Browser verification used a disposable rootless Podman mock stack with two
projects and 105 requests. It verified 101 Anthropic-key requests across two
pages, two MaaS-key requests sharing that client ID, project/all-key scope
changes, bootstrap key selection, key-scoped totals and export data, refresh,
historical usage after revocation, and newly issued keys becoming selectable.
The filters fit desktop and 390-pixel mobile viewports. Live assets matched
workspace HTML/JS after rebuilding and restarting the liveoauth proxy process;
the new live inventory exposed both bootstrap identities with no secret fields,
disconnect cleared the selector, and storage health reported zero errors or
dropped records. The temporary test container and network were removed.

## Managed upstream OAuth accounts, 2026-10-09

Added the **Upstream accounts** admin view and authenticated private CRUD/login
routes. Account metadata is persisted in SQLite; managed native credentials
live in private files on the same `/data` volume. Host-mirror profiles retain
host-owned refresh and are not writable through account management.

`./scripts/test.sh` passed all 26 Rust tests, formatting, and Clippy. New tests
cover provider/PKCE URL validation, listener/admin-key separation, per-account
login and code-state isolation, duplicate login rejection, credential publication
and restart persistence, cancellation/expiry and repeated concurrent cancellation,
disconnect, deletion reference checks, concurrent project assignment/deletion,
and serialized refresh-token rotation. Rejected refreshes preserve the existing
store and report reauthentication. Existing host-mirror tests still pass.

Both proxy images now install pinned Claude Code 2.1.293 with the curl installer.
The development and source-baked runtime images built successfully with locked
Rust dependencies. `./scripts/test-runtime.sh` passed native Claude inference and
REST/telemetry acceptance without source mounts: 15 recorded requests, zero
storage errors or dropped records.

A disposable rootless Podman stack exercised the browser flow with a synthetic
native helper/provider: account creation, browser-link/code submission, rename,
explicit refresh, project assignment, reconnect cancellation, disconnect, and
deletion. Project-scoped model discovery and Messages inference used the selected
managed account and recorded `upstream_profile: ui-team` / `claude-oauth` in
telemetry. Container restart preserved account metadata and credentials while
clearing login state. Desktop (1440px) and mobile (390px) layouts, dialog clearing,
and secret-free stdout were checked.

The real pinned native helper was tested through the REST login API in both the
development and baked runtime containers. It returned an allowlisted provider
URL with S256 PKCE; cancellation reaped the process, cleared the URL, and left no
connected credentials. The temporary account was deleted. Actual browser
consent/code exchange and provider refresh require a human-authorized account;
those completion/rotation paths were tested against the synthetic provider.

The running `liveoauth` proxy was rebuilt/restarted in its existing container.
Its existing host account remained ready with host-owned refresh, and both
client containers remained running. Account management and the native login
helper are enabled at `http://127.0.0.1:44443/admin/`. A consistent SQLite backup
was saved before adding the account table. No real managed account was created
or host credential modified by this validation.

Artifacts are under ignored `.state/admin-ui-validation/`:
`managed-tests.log`, `managed-image-build.log`, `managed-runtime-build.log`,
`managed-runtime-checks.log`, `managed-routing.json`, `managed-native-login.json`,
`managed-live-upgrade.log`, `managed-live-verification.json`, and
`accounts-{desktop,mobile}.png`. Native-login evidence contains only validation
booleans/version metadata, never the real authorization URL or state.

## Root deployment Dockerfile, 2026-10-09

Added a regular root `Dockerfile` as the canonical production recipe, with the
legacy `containers/Containerfile.runtime` path retained as a compatibility
symlink. A regular `.dockerignore` contains the build-input allowlist;
`.containerignore` links to it so Docker and Podman share the same exclusions.
The README documents root build commands and required Kubernetes mounts/listeners.

Both the default rootless `podman build .` and the legacy explicit `-f` build
succeeded. They produced the exact previously validated production image,
including UID/GID 1000, the compiled relay entrypoint, ports 8080/8081, and the
pinned OAuth helper. No application source or runtime behavior changed.
`git diff --check` passed. Build logs and image metadata checks are under ignored
`.state/dockerfile-validation/`.

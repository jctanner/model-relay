# Container builds

Build the production relay from the root `Dockerfile` with
`podman build -t localhost/model-relay:local .` or
`docker build -t model-relay:local .`. It includes the release Rust binary,
embedded admin UI, and native OAuth login helper. The legacy
`containers/Containerfile.runtime` is a compatibility link to the same file.

`Containerfile.dev` provides the source-mounted development toolchain;
`Containerfile.claude` builds the separate client image. Docker and Podman share
the build-input allowlist in the root `.dockerignore`.

## Claude Code MVP client bootstrap

Claude Code is the first client. `Containerfile.claude` executes Anthropic's
[native curl installer](https://code.claude.com/docs/en/setup#install-a-specific-version)
inside the build container with an exact version argument. It verifies the
installed version and places the binary in `/usr/local/bin`, outside the
persistent client home volume. Updates are disabled in the runtime image.
The proxy images also install the pinned CLI through the curl installer. It is
used only for isolated upstream-account login subprocesses; inference routing
remains in Rust.

The optional single-client API-key/Vertex wrapper uses the documented
[Vertex override](https://code.claude.com/docs/en/google-vertex-ai) and
[environment variables](https://code.claude.com/docs/en/env-vars):
`CLAUDE_CODE_SKIP_VERTEX_AUTH=1`, `ANTHROPIC_VERTEX_BASE_URL`, and a local bearer
key in `ANTHROPIC_CUSTOM_HEADERS`. The current documentation's example has no
`/v1` suffix, but the installed 2.1.280 CLI was observed to append `/projects/...`
directly to the configured base. Our local endpoint therefore includes `/v1`.
Use the wire-level probe rather than assuming future versions behave identically.

The client is now wired into the Rust proxy/Honcho stack; see the root
[README](../README.md) for normal development commands. No CLI has been installed
on the host. The earlier agent sandbox
blocked writes to `/run/user/1000/libpod` and `newuidmap`; those restrictions were
removed for this development session, and rootless Podman now works.

```bash
./scripts/client.sh doctor
# Choose an exact release to validate, not "latest" or "stable".
./scripts/client.sh build 2.1.293
./scripts/client.sh init-key
```

The version above has been installed and verified in a rootless, unprivileged
container. Build fails unless the installer returns the requested version.
`CLIENT_BASE_IMAGE` accepts a digest-qualified Debian base reference and the
default is now digest-pinned. Debian package downloads and the remote installer
remain mutable build inputs.

The proxy launcher creates the internal `vertex-shim-dev` network and registers
the development key's verifier. Its Honcho client entry invokes
`./scripts/client.sh service`. That wrapper runs the container in the foreground,
stops it on termination, and preserves its home volume and host work directory. In another
terminal, use:

```bash
./scripts/client.sh exec claude --version
./scripts/client.sh exec claude-proxy -p 'Reply with a short greeting.'
# With an interactive terminal:
./scripts/client.sh exec claude-proxy
./scripts/client.sh stop
```

Source edits and repeated client probes use `podman exec`; only installation
or image changes require a client image build. The client mounts its own
named home volume, a writable host work directory, one local inference key file, and the read-only launcher
script. It receives no host
home, ADC, OAuth store, or inherited host environment. The build context has an
allowlist for container build files, Rust source/lockfiles, and container wrappers;
it excludes credentials, local state, and the reference checkout. Keep inference keys
separate from upstream and administrative credentials.

`/work` defaults to a bind mount of `client-work/<session>/` in this workspace.
Set `SHIM_CLIENT_WORK_DIR=/absolute/path/to/project` before starting to use another
directory. The launcher creates the directory if needed and mounts it with
`rw,z`; it does not change host ownership. The rootless UID mapping lets the host
user read and edit agent-created files. Setting `SHIM_CLIENT_WORK_DIR` to an
explicitly empty string retains the named work volume for disposable tests.
Changing the mount requires container recreation, without an image rebuild.
Existing named work volumes are not removed or automatically migrated.

`VERTEX_SHIM_SESSION` scopes resource names; `VERTEX_SHIM_NETWORK`,
`CLAUDE_CLIENT_IMAGE`, `PROXY_API_KEY_FILE`, `VERTEX_SHIM_MODEL`, and
`ANTHROPIC_VERTEX_BASE_URL` override the corresponding local settings. An
internal network prevents the mock client from reaching real providers.
Do not use a real-provider endpoint override for the mock profile.

Run `./scripts/test-client.sh` for the native CLI gateway compatibility probe.
It uses disposable client home/work volumes and an internal network, tests a
synthetic bearer key against Vertex-shaped SSE routes, and preserves sanitized
request-shape logs in `.state/probe-*/`. Its Python fixture is a test server,
not the Rust proxy. The mock profile receives neither Google nor upstream credentials. The optional
Anthropic adapter mounts its bearer file only in the proxy; the client keeps its
own local key and home volume. `scripts/test-upstream.sh` adds a disposable
provider fixture without rebuilding images.

The selected OAuth profile uses `Procfile.oauth`: three runtime containers plus a
host watcher. Only an access-token mirror is mounted in the proxy, read-only;
the host credential directory and refresh token are never mounted in either
container for this host-mirror profile. Its login/refresh stays with the host CLI. Atomic host credential changes
are mirrored without rebuilding/recreating containers. Use
`scripts/test-host-oauth.sh` for the isolated synthetic acceptance suite and see
the root README for the validated `liveoauth` session commands.

Use `SHIM_CLIENT_API=messages` for native Anthropic Messages routing and model
discovery. The launcher fetches the proxy's authenticated catalog and builds a
session-only model picker using upstream IDs/names. It removes Vertex/provider
flags and gives Claude only a local API key. The native version is now
2.1.293 to support the newest Sonnet/Haiku models; existing images/containers
are replaced when selecting a new pinned version. `scripts/test-messages.sh`
tests the native client, model discovery, permissions, and Messages REST routes.

In `SHIM_CLIENT_LAYOUT=single`, use `SHIM_CLIENT_AUTH=mock-adc` with
`SHIM_CLIENT_API=vertex` for the optional
synthetic Google authentication profile. The client mounts no local API key and
its wrapper removes `CLAUDE_CODE_SKIP_VERTEX_AUTH` and custom auth headers. The
normal Google ADC chain discovers `vertex-shim:8082`, which serves expiring
metadata tokens from the Rust proxy on a separate, unpublished listener.
Existing API-key profiles remain supported. The source-mounted wrapper makes
this available without a client image rebuild. See the root README for registry
grants, identity/network scope, upstream credential separation, lifetime, and
the `scripts/test-mock-adc.sh` acceptance suite.

The default stack runs both client modes at once. `dev.sh exec anthropic-client`
(also `exec client`) targets the Messages/API-key container; `exec vertex-client`
targets the native Vertex/mock-ADC container. Both reuse the same pinned client
image and source-mounted wrapper. They have separate home volumes, work bind
mounts, and telemetry identities. Vertex mounts no inference-key file. Set
`SHIM_VERTEX_WORK_DIR` to override its default `client-work/<session>-vertex/`.
Both clients can join an explicitly supplied `SHIM_CLIENT_EGRESS_NETWORK`; the
launcher validates this user-owned network and preserves it at shutdown.

The default Vertex identity has `vertex_catalog_models: true` and discovers all
models offered by the shim's upstream account, including Opus 4.6. The proxy
translates dated IDs to Vertex route format and resolves catalog models without
static deployment entries. Both grant checks and catalog membership apply on
each request; explicit-only Vertex identities remain supported. Reopen
`claude-proxy` to refresh its model picker.


Additional project clients can attach to the running proxy network with their
own session name, home volume, work bind, and admin-issued workload key. Vertex
clients use `mock-federated` to exercise external-account ADC with expiring
project-bound tokens; Messages clients use the same project's opaque API key.
`SHIM_UPSTREAM_PROFILES_DIR` mounts additional server-owned credential profiles
read-only at `/upstreams`, only in the proxy. Project/grant CRUD persists in the
telemetry volume and requires no container rebuild or restart. See
[project administration and client launch instructions](../docs/projects.md).

Relay-managed accounts instead keep credentials in `/data/oauth-accounts/` and
use the web console for browser login. The proxy owns their refresh and mounts
no host Claude home. See [managed upstream accounts](../docs/upstream-accounts.md).

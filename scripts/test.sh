#!/usr/bin/env bash
set -euo pipefail
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
image=${SHIM_DEV_IMAGE:-localhost/vertex-shim-dev:1.90}
cache_volume=${SHIM_TEST_CARGO_VOLUME:-vertex-shim-cargo-1-90}
session=unit-$(date +%s)-$$
name=vertex-shim-${session}
trap 'podman rm --force "$name" >/dev/null 2>&1 || true' EXIT
# The source is writable only here so the explicit fmt command can update it.
podman run --detach --pull never --http-proxy=false --name "$name" --network none \
    --userns keep-id:uid=1000,gid=1000 --read-only --cap-drop all --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,size=256m \
    --volume "${workspace}/Cargo.toml:/work/Cargo.toml:ro,z" \
    --volume "${workspace}/Cargo.lock:/work/Cargo.lock:ro,z" \
    --volume "${workspace}/src:/work/src:rw,z" \
    --volume "${cache_volume}:/cargo:U" \
    --volume vertex-shim-unit-build:/build:U \
    "$image" sleep infinity >/dev/null
podman exec "$name" bash -c 'mkdir -p /cargo/registry; cp -au /opt/cargo-registry/. /cargo/registry/'
podman exec "$name" cargo fmt --all
podman exec "$name" cargo test --locked --offline
podman exec "$name" cargo clippy --locked --offline --all-targets -- -D warnings

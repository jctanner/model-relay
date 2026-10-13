#!/usr/bin/env bash
set -euo pipefail
mkdir -p /cargo/registry
# Merge image-prepared dependencies after a lockfile/image upgrade, retaining
# newer entries fetched into the persistent cache during development.
cp -au /opt/cargo-registry/. /cargo/registry/
mkdir -p /build/bin
export SHIM_BUILD_ID=$(sha256sum Cargo.toml Cargo.lock src/*.rs src/telemetry/*.rs src/admin_ui/* | sha256sum | cut -d ' ' -f1)
cargo build --locked --offline
install /build/target/debug/vertex-shim /build/bin/vertex-shim
child=''
stop() {
    trap - EXIT INT TERM
    if [[ -n "$child" ]]; then kill -TERM "$child" 2>/dev/null || true; wait "$child" || true; fi
}
trap stop EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
while true; do
    /build/bin/vertex-shim &
    child=$!
    while kill -0 "$child" 2>/dev/null; do
        if [[ -f /run/vertex-shim/restart ]]; then
            rm -f /run/vertex-shim/restart
            kill -TERM "$child"
            wait "$child"
            child=''
            break
        fi
        sleep 0.25 & wait $!
    done
    if [[ -n "$child" ]]; then
        # An unexpected exit terminates the supervised service, even exit zero.
        wait "$child" || exit $?
        exit 1
    fi
done

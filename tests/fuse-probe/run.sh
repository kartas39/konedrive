#!/bin/sh
# Builds the probe and runs its tests in a virtme-ng guest. Nothing is mounted on the host.
# Usage: tests/fuse-probe/run.sh [t1 t2 t3 t4 t5]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build --release --manifest-path "$here/Cargo.toml"
work=$(mktemp -d "$here/target/run.XXXXXX")
trap 'rm -rf "$work"' EXIT
{
    echo '#!/bin/sh'
    echo "bash '$here/guest.sh' '$here/target/release' $*"
} > "$work/inner.sh"
chmod +x "$work/inner.sh"
vng --run --rw --memory "${VM_MEMORY:-10G}" --user root --exec "$work/inner.sh" < /dev/null 2>&1 | tr -d '\r'

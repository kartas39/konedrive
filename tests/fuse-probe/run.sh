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
# The guest's disk: a file on the host's own disk, with no copy-on-write and opened with O_DIRECT,
# so the guest's writes and cold reads are the disk's and not the host's memory.
img="$here/target/disk.img"
if [ ! -f "$img" ]; then
    touch "$img"
    chattr +C "$img" 2> /dev/null || true
    fallocate -l 24G "$img"
fi
vng --run --rw --memory "${VM_MEMORY:-8G}" --user root \
    --qemu-opts="-drive if=none,id=probe,file=$img,format=raw,cache=none,aio=native -device virtio-blk-pci,drive=probe" \
    --exec "$work/inner.sh" < /dev/null 2>&1 | tr -d '\r'

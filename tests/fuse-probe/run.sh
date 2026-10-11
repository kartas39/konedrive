#!/bin/sh
# Builds the suite's programs and runs its tests in a virtme-ng guest. Nothing is mounted on the
# host. Usage: tests/fuse-probe/run.sh [<group>…] (guest.sh names the groups); ONLY="<scenario>…"
# runs those scenarios alone. The run fails when a scenario fails that is not named in
# expected-to-fail.txt, or when the guest does not reach its end.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build --release --manifest-path "$here/Cargo.toml"
work=$(mktemp -d "$here/target/run.XXXXXX")
trap 'rm -rf "$work"' EXIT
# `konedrivectl`'s tests of a mounted folder (crates/konedrivectl/tests/sync_cli), built here
# when the `cli` group is among those asked for: the binary the guest runs is this tree's.
cli_test=
case " ${*:-cli} " in
    *" cli "*)
        cli_test=$(cargo test --manifest-path "$here/../../Cargo.toml" -p konedrivectl --test sync_cli --no-run --message-format=json \
            | sed -n 's/.*"executable":"\([^"]*sync_cli-[^"]*\)".*/\1/p' | tail -1)
        [ -x "$cli_test" ] || { echo "the tests of crates/konedrivectl/tests/sync_cli were not built"; exit 1; }
        ;;
esac
# The probe of KIO's trash (tests/kio/trash_probe.cpp), built here when the `kio_trash` group is
# asked for, which it is by name only: it needs the development packages of Qt 6 and KDE
# Frameworks 6 on the host, and the guest runs it with the host's libraries.
trash_probe=
case " $* " in
    *" kio_trash "*)
        cmake -S "$here/../kio" -B "$here/target/kio-probe" -DCMAKE_BUILD_TYPE=RelWithDebInfo > /dev/null
        cmake --build "$here/target/kio-probe" --target trash_probe > /dev/null
        trash_probe=$here/target/kio-probe/trash_probe
        ;;
esac
{
    echo '#!/bin/sh'
    echo "export CLI_TEST='$cli_test'"
    echo "export TRASH_PROBE='$trash_probe'"
    echo "export ONLY='${ONLY:-}'"
    # For a person who reads a failure: the level of the daemon's log, and how much of it a failed scenario prints.
    echo "export SERVICE_LOG='${SERVICE_LOG:-info}'"
    echo "export EVIDENCE_LOG_LINES='${EVIDENCE_LOG_LINES:-25}'"
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
    --exec "$work/inner.sh" < /dev/null 2>&1 | tr -d '\r' | tee "$work/output"
grep -q '^guest-done$' "$work/output" || { echo "the guest did not reach its end"; exit 1; }
grep -q '^scenarios: [0-9]* passed, 0 failed,' "$work/output"

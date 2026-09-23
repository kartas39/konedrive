#!/bin/sh
# Boots the host kernel in a virtme-ng VM and runs $1 as root inside it, with
# three loop-backed filesystems mounted. Extra arguments are forwarded to the
# binary. Nothing here touches the host session: vng needs no privileges and
# everything privileged happens inside the guest.
#
# Usage: tests/vm/run.sh <binary> [args...]
#        tests/vm/run.sh scenarios            # the end-to-end suite
#        tests/vm/run.sh scenarios --fs ext4,xfs --only 'burst|daemon death'
#        tests/vm/run.sh measure [args...]    # the measurement mode
#
# The last two build what they need on the host first (nothing privileged) and
# then run `vm-scenarios` in the guest, handing it the path of the real helper
# binary — the suite starts the helper itself, because half of what it asserts
# is about the helper dying, restarting, or running out of descriptors.
#
# That helper is built with the `fault-injection` feature (Ruling H121), which
# compiles in the two environment-armed panics the unwind scenarios need and
# which a shipped helper must not contain. It goes into this directory's own
# target directory, never the workspace's `target/release`, so that the
# binary with the hooks in it cannot be mistaken for the one that ships.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)

memory=${VM_MEMORY:-4G}
tmpfs=${VM_TMPFS:-4G}

case ${1:-} in
    scenarios | measure)
        mode=$1
        shift
        cargo build --release --manifest-path "$repo/Cargo.toml" -p konedrive-helper \
            --features fault-injection --target-dir "$here/target"
        cargo build --release --manifest-path "$here/Cargo.toml" --bin vm-scenarios
        set -- "$here/target/release/vm-scenarios" \
            --helper "$here/target/release/konedrive-helper" \
            "$@"
        if [ "$mode" = measure ]; then
            # The realistic tree is 10 000 directories and 100 000 files on a
            # loop image that lives on the guest's tmpfs, so both the tmpfs and
            # the VM need more room than the scenarios do.
            memory=${VM_MEMORY:-10G}
            tmpfs=${VM_TMPFS:-8G}
            set -- "$@" --measure
        fi
        ;;
esac

binary=$1
shift
BINARY=$(readlink -f "$binary")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The guest sees the host's filesystem, so the script and the binary are
# reachable by their host paths. The binary path and its arguments are baked
# into the script rather than passed through the environment, which vng does
# not propagate into the guest.
{
    echo '#!/bin/sh'
    echo 'set -eu'
    # The helper holds roughly 1.3 descriptors per suspended open, and the
    # burst scenario suspends thousands at once. The guest's default soft
    # limit is 1024, which would make the burst a measurement of RLIMIT_NOFILE
    # rather than of the worker pool; guest root may raise the hard limit too.
    echo 'ulimit -n 1048576 || true'
    # The guest root is the host filesystem over virtiofs, and virtiofsd runs as
    # the unprivileged host user, so guest root cannot create /mnt/btrfs there
    # (EACCES). A tmpfs over /mnt gives us writable mount points.
    # Nothing has needed the loop driver yet, so no /dev/loop* exists and
    # `mount -o loop` fails before it starts.
    echo 'modprobe loop'
    echo "mount -t tmpfs -o size=$tmpfs tmpfs /mnt"
    # Same problem, same answer: the helper persists its registrations in
    # /var/lib/konedrive, which is on the host's read-only-to-guest-root
    # virtiofs. Without this every `RegisterRoot` fails on the state file's
    # write and the suite cannot start. /run is already a tmpfs.
    echo 'mount -t tmpfs -o size=64M tmpfs /var/lib'
    # The images live on that same tmpfs: a loop device cannot be backed by a
    # file on the overlayfs that vng mounts over /tmp.
    echo 'mkdir /mnt/img'
    echo 'for fs in btrfs ext4 xfs; do'
    echo '    mkdir -p /mnt/$fs'
    echo '    truncate -s 2G /mnt/img/$fs.img'
    echo '    case $fs in'
    echo '        btrfs) mkfs.btrfs -q /mnt/img/$fs.img ;;'
    echo '        ext4)  mkfs.ext4 -q -F /mnt/img/$fs.img ;;'
    echo '        xfs)   mkfs.xfs -q /mnt/img/$fs.img ;;'
    echo '    esac'
    echo '    mount -o loop /mnt/img/$fs.img /mnt/$fs'
    echo 'done'
    printf 'set +e\n'
    printf '%s' "\"$BINARY\""
    for arg in "$@"; do
        printf ' %s' "\"$arg\""
    done
    printf '\n'
    echo 'rc=$?'
    echo 'set -e'
    echo 'echo "inner-exit=$rc"'
} > "$work/inner.sh"
chmod +x "$work/inner.sh"

out="$work/out.txt"
# --memory 4G, not 2G: with exactly 2048 MiB this host's kernel hangs early in
# boot under vng's microvm machine type (see docs/kernel-behavior-7.2.md).
vng --run --rw --memory "$memory" --user root --exec "$work/inner.sh" < /dev/null 2>&1 | tee "$out"

# Exit with the binary's own status, not grep's: a caller needs to tell "two
# checks failed" from "the VM never got that far".
rc=$(tr -d '\r' < "$out" | sed -n 's/^inner-exit=\([0-9]\{1,\}\)$/\1/p' | tail -1)
if [ -z "$rc" ]; then
    echo "run.sh: the guest never reported an exit status" >&2
    exit 125
fi
exit "$rc"

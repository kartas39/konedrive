#!/bin/sh
# Boots the host kernel in a virtme-ng VM and runs $1 as root inside it, with
# one or more loop-backed filesystems mounted. Extra arguments are forwarded
# to the binary. Nothing here touches the host session: vng needs no
# privileges and everything privileged happens inside the guest.
#
# Usage: tests/vm/run.sh <binary> [args...]
#        tests/vm/run.sh quick [args...]     # the suite, btrfs only, one VM
#        tests/vm/run.sh full [args...]      # the suite, three VMs in parallel
#        tests/vm/run.sh scenarios [args...] # the suite, all three FS, one VM
#        tests/vm/run.sh measure [args...]   # the measurement mode
#        tests/vm/run.sh unit                # the shipped systemd unit, under systemd
#
# `quick` is the normal run, every time: one filesystem (the user's own,
# btrfs), one VM, so the loop is short. `full` runs only when the user asks for
# it — never as a routine step, not at a merge: btrfs, ext4 and xfs, each in
# its own VM, all three booted at once — the wall time of the slowest one, not
# the sum. `scenarios` is the original all-three-in-one-VM-in-sequence run,
# kept because it needs only one VM (useful where three concurrent vng
# instances are not available) and because CI or a habit may already call it
# by that name. `quick`/`full`/`scenarios` all build what they need on the
# host first (nothing privileged) and then run `vm-scenarios` in the guest,
# handing it the path of the real helper binary — the suite starts the helper
# itself, because half of what it asserts is about the helper dying,
# restarting, or running out of descriptors.
#
# That helper is built with the `fault-injection` feature, which
# compiles in the two environment-armed panics the unwind scenarios need and the
# stall a race scenario needs (KONEDRIVE_FAULT_DELAY_IGNORE_MS) — hooks a
# shipped helper must not contain. It goes into this directory's own
# target directory, never the workspace's `target/release`, so that the
# binary with the hooks in it cannot be mistaken for the one that ships.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)

memory=${VM_MEMORY:-4G}
tmpfs=${VM_TMPFS:-4G}
# VM_NETWORK=user gives the guest QEMU's user-mode network (the real
# account). Unset by default — every other scenario runs with no network at
# all, which is the point of a fanotify suite that must never touch a real
# server by accident.
network=${VM_NETWORK:-}

# shquote word
#
# Prints word as one single-quoted shell word, each ' written as '\'', so
# inner.sh hands it to the binary exactly as given: the guest's root shell
# expands no $, backtick, " or \ in it.
shquote() {
    printf "'"
    printf '%s' "$1" | sed "s/'/'\\\\''/g"
    printf "'"
}

# write_inner work_dir binary fs_list [args...]
#
# Writes $work_dir/inner.sh: it creates and loop-mounts one 2G image per
# filesystem named in fs_list (space-separated, e.g. "btrfs" or
# "btrfs ext4 xfs"), then runs $binary with the remaining arguments. Only the
# filesystems actually asked for are made — a `quick` run building the other
# two idle images was most of what made it no faster than `scenarios`.
write_inner() {
    work=$1
    binary=$2
    fslist=$3
    shift 3
    BINARY=$(readlink -f "$binary")
    {
        echo '#!/bin/sh'
        echo 'set -eu'
        if [ -n "$network" ]; then
            # Fedora's /etc/resolv.conf is a symlink into /run, which is a
            # fresh tmpfs in the guest (docs/kernel-behavior-7.2.md §9): with
            # `--network user`, QEMU's own resolver sits at 10.0.2.3, but
            # nothing in the guest points at it until this writes the stub
            # systemd-resolved expects.
            echo 'mkdir -p /run/systemd/resolve'
            echo 'echo "nameserver 10.0.2.3" > /run/systemd/resolve/stub-resolv.conf'
        fi
        # The helper holds roughly 1.3 descriptors per suspended open, and the
        # burst scenario suspends thousands at once. The guest's default soft
        # limit is 1024, which would make the burst a measurement of
        # RLIMIT_NOFILE rather than of the worker pool; guest root may raise
        # the hard limit too.
        echo 'ulimit -n 1048576 || true'
        # The guest root is the host filesystem over virtiofs, and virtiofsd
        # runs as the unprivileged host user, so guest root cannot create
        # /mnt/btrfs there (EACCES). A tmpfs over /mnt gives us writable mount
        # points. Nothing has needed the loop driver yet, so no /dev/loop*
        # exists and `mount -o loop` fails before it starts.
        echo 'modprobe loop'
        echo "mount -t tmpfs -o size=$tmpfs tmpfs /mnt"
        # Same problem, same answer: the helper persists its registrations in
        # /var/lib/konedrive, which is on the host's read-only-to-guest-root
        # virtiofs. Without this every `RegisterRoot` fails on the state
        # file's write and the suite cannot start. /run is already a tmpfs.
        echo 'mount -t tmpfs -o size=64M tmpfs /var/lib'
        # The images live on that same tmpfs: a loop device cannot be backed
        # by a file on the overlayfs that vng mounts over /tmp.
        echo 'mkdir /mnt/img'
        echo "for fs in $fslist; do"
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
        shquote "$BINARY"
        for arg in "$@"; do
            printf ' '
            shquote "$arg"
        done
        printf '\n'
        echo 'rc=$?'
        echo 'set -e'
        echo 'echo "inner-exit=$rc"'
    } > "$work/inner.sh"
    chmod +x "$work/inner.sh"
}

# run_vm work_dir memory_size > exit code on stdout via $work_dir/rc
#
# Boots vng against $work_dir/inner.sh, tees the guest's output to
# $work_dir/out.txt, and leaves the binary's own exit status (not vng's, and
# not grep's — a caller needs to tell "two checks failed" from "the VM never
# got that far") in $work_dir/rc.
run_vm() {
    work=$1
    mem=$2
    out="$work/out.txt"
    vng --run --rw --memory "$mem" --user root ${network:+--network "$network"} \
        --exec "$work/inner.sh" < /dev/null > "$out" 2>&1 || true
    rc=$(tr -d '\r' < "$out" | sed -n 's/^inner-exit=\([0-9]\{1,\}\)$/\1/p' | tail -1)
    if [ -z "$rc" ]; then
        rc=125
    fi
    echo "$rc" > "$work/rc"
}

build_scenarios() {
    cargo build --release --manifest-path "$repo/Cargo.toml" -p konedrive-helper \
        --features fault-injection --target-dir "$here/target"
    cargo build --release --manifest-path "$here/Cargo.toml" --bin vm-scenarios
    helper="$here/target/release/konedrive-helper"
    vm_scenarios="$here/target/release/vm-scenarios"
}

mode=${1:-}
case $mode in
    unit)
        # The helper as a real machine runs it: systemd as PID 1 starts it
        # from packaging/systemd/konedrive-helper.service, and a daemon then
        # works through its socket (tests/vm/helper_unit_test.sh). Separate
        # from `quick`, which starts the helper itself as plain root.
        #
        # A release helper WITHOUT fault-injection — what ships — in a target
        # directory of its own, so it neither replaces nor is replaced by the
        # suite's hooked build in $here/target/release.
        cargo build --release --manifest-path "$repo/Cargo.toml" -p konedrive-helper \
            --target-dir "$here/target/unit"
        cargo build --release --manifest-path "$here/Cargo.toml" --bin vm-scenarios
        # Not under /tmp: systemd mounts a fresh tmpfs there in the guest.
        work=$(mktemp -d "$here/target/unit-run.XXXXXX")
        trap 'rm -rf "$work"' EXIT
        {
            echo '#!/bin/sh'
            printf 'sh %s %s %s %s\n' "$(shquote "$here/helper_unit_test.sh")" \
                "$(shquote "$here/target/unit/release/konedrive-helper")" \
                "$(shquote "$here/target/release/vm-scenarios")" \
                "$(shquote "$repo/packaging/systemd/konedrive-helper.service")"
            echo 'echo "inner-exit=$?"'
        } > "$work/inner.sh"
        chmod +x "$work/inner.sh"
        # No --rw: the guest sees the host's root read-only, and vng puts
        # tmpfs-backed overlays over /etc, /usr and /var, so the unit and the
        # binary are installed in the guest alone (the script checks this
        # before writing anything). selinux=0: with the host's policy
        # enforcing over an unlabelled virtiofs root, systemd cannot mount
        # /run and freezes. --disable-microvm: the microvm machine
        # hangs at "ACPI: Core revision" under the 7.2.7 host kernel
        # (2026-09-25); the standard machine boots. Piped through cat: vng
        # opens the same output file from several chardevs, which overwrite
        # one another in a regular file.
        vng --run --systemd --disable-microvm --memory "$memory" --user root \
            --append selinux=0 \
            --exec "$work/inner.sh" < /dev/null 2>&1 | cat > "$work/out.txt"
        tr -d '\r' < "$work/out.txt" | sed -n '/^==== guest ====$/,$p'
        rc=$(tr -d '\r' < "$work/out.txt" | sed -n 's/^inner-exit=\([0-9]\{1,\}\)$/\1/p' | tail -1)
        if [ -z "$rc" ]; then
            tr -d '\r' < "$work/out.txt" | tail -40
            echo "run.sh: the guest never reported an exit status" >&2
            exit 125
        fi
        exit "$rc"
        ;;
    quick)
        shift
        build_scenarios
        work=$(mktemp -d)
        trap 'rm -rf "$work"' EXIT
        # Forced last, so it always wins over anything forwarded on the
        # command line: the guest below mounts btrfs only, and `--fs` must
        # match or the suite fails its own filesystem check rather than
        # silently testing tmpfs.
        write_inner "$work" "$vm_scenarios" btrfs \
            --helper "$helper" "$@" --fs btrfs
        run_vm "$work" "$memory"
        cat "$work/out.txt"
        exit "$(cat "$work/rc")"
        ;;
    full)
        shift
        build_scenarios
        base=$(mktemp -d)
        trap 'rm -rf "$base"' EXIT
        # Probe whether this host can run three vng instances at once
        # (virtiofsd, the guest sockets, and vng's own /tmp overlay each get
        # instantiated per invocation) before committing 5+ minutes to it. If
        # it cannot, one VM per filesystem in sequence still gets a correct
        # answer, just not a faster one.
        parallel=1
        probe=$base/probe
        mkdir -p "$probe/1" "$probe/2" "$probe/3"
        for n in 1 2 3; do
            {
                echo '#!/bin/sh'
                echo 'set -eu'
                echo 'echo probe-ok'
            } > "$probe/$n/inner.sh"
            chmod +x "$probe/$n/inner.sh"
        done
        for n in 1 2 3; do
            ( vng --run --rw --memory "$memory" --user root \
                  --exec "$probe/$n/inner.sh" < /dev/null \
                  > "$probe/$n/out.txt" 2>&1 || true ) &
        done
        wait
        for n in 1 2 3; do
            grep -q '^probe-ok$' "$probe/$n/out.txt" || parallel=0
        done
        rm -rf "$probe"
        if [ "$parallel" -eq 1 ]; then
            echo "run.sh: three concurrent VMs work; running btrfs, ext4 and xfs in parallel" >&2
        else
            echo "run.sh: three concurrent VMs did not all come up cleanly; falling back to one VM per filesystem, in sequence" >&2
        fi

        overall=0
        for fs in btrfs ext4 xfs; do
            work="$base/$fs"
            mkdir -p "$work"
            write_inner "$work" "$vm_scenarios" "$fs" \
                --helper "$helper" "$@" --fs "$fs"
        done
        if [ "$parallel" -eq 1 ]; then
            for fs in btrfs ext4 xfs; do
                ( run_vm "$base/$fs" "$memory" ) &
            done
            wait
        else
            for fs in btrfs ext4 xfs; do
                run_vm "$base/$fs" "$memory"
            done
        fi
        for fs in btrfs ext4 xfs; do
            work="$base/$fs"
            echo "==== $fs ===="
            sed "s/^/[$fs] /" "$work/out.txt"
            if [ -f "$work/rc" ]; then
                rc=$(cat "$work/rc")
            else
                rc=125
                echo "[$fs] run.sh: this VM never reported an exit status" >&2
            fi
            if [ "$rc" -eq 0 ]; then
                echo "==== $fs: PASS ===="
            else
                echo "==== $fs: FAIL (exit $rc) ===="
                overall=1
            fi
        done
        exit "$overall"
        ;;
    scenarios | measure)
        shift
        build_scenarios
        set -- "$vm_scenarios" --helper "$helper" "$@"
        if [ "$mode" = measure ]; then
            # The realistic tree is 10 000 directories and 100 000 files on a
            # loop image that lives on the guest's tmpfs, so both the tmpfs
            # and the VM need more room than the scenarios do.
            memory=${VM_MEMORY:-10G}
            tmpfs=${VM_TMPFS:-8G}
            set -- "$@" --measure
        fi
        binary=$1
        shift
        work=$(mktemp -d)
        trap 'rm -rf "$work"' EXIT
        write_inner "$work" "$binary" "btrfs ext4 xfs" "$@"
        run_vm "$work" "$memory"
        cat "$work/out.txt"
        exit "$(cat "$work/rc")"
        ;;
esac

binary=$1
shift
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
write_inner "$work" "$binary" "btrfs ext4 xfs" "$@"
run_vm "$work" "$memory"
cat "$work/out.txt"
exit "$(cat "$work/rc")"

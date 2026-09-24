#!/bin/sh
# Runs the helper the way a real machine does: systemd starts it from the
# shipped packaging/systemd/konedrive-helper.service, with every line of its
# sandbox, and a daemon then works through its socket. Everything else in
# tests/vm starts the helper binary itself, as plain root.
#
# Only for a guest that `tests/vm/run.sh unit` booted with systemd as PID 1,
# where it runs as root. Arguments: the helper binary (a release build
# WITHOUT fault-injection), the vm-scenarios binary, and the unit file.
#
# Two passes. The first runs the unit exactly as shipped. Its seccomp filter
# answers a denied call with EPERM, which the kernel does not log — a denial
# could pass unseen, and the helper might carry on in some worse way. So the
# second pass adds one drop-in, SystemCallErrorNumber=kill, and does it all
# again: any call the filter denies now kills the helper with SIGSYS, which
# systemd records in the journal. Nothing else differs. The second pass also
# starts from the first pass's registration, so it covers the helper's
# startup walk as well.
set -u

helper_bin=$1
scenarios_bin=$2
unit_src=$3
unit=konedrive-helper.service
installed_bin=/usr/local/libexec/konedrive-helper

fails=0
check() {
    if eval "$2"; then
        echo "PASS $1"
    else
        echo "FAIL $1"
        fails=$((fails + 1))
    fi
}
section() { echo; echo "==== $* ===="; }
show() { systemctl show -p "$1" --value "$unit"; }

# --- the guest, as run.sh must have booted it --------------------------------
section "guest"
if [ "$(cat /proc/1/comm)" != systemd ]; then
    echo "FAIL PID 1 is $(cat /proc/1/comm), not systemd: boot the guest with tests/vm/run.sh unit"
    exit 1
fi
# The root is the host's own filesystem, shared read-only; vng puts a
# tmpfs-backed overlay over /etc, /usr and /var (and /run is systemd's own
# tmpfs). Every write below lands in one of those, so check they are there
# before writing anything: a guest booted with `vng --rw` would be writing
# into the host's /etc and /usr.
case ",$(findmnt -n -o OPTIONS /)," in
    *,ro,*) ;;
    *) echo "FAIL / is writable in this guest: refusing to install anything"; exit 1 ;;
esac
for dir in /etc/systemd/system /usr/local/libexec /var/lib /run; do
    fstype=$(findmnt -n -o FSTYPE --target "$dir")
    case $fstype in
        overlay | tmpfs) echo "$dir: $fstype" ;;
        *) echo "FAIL $dir is on $fstype, not a guest-only overlay or tmpfs"; exit 1 ;;
    esac
done
systemctl --version | head -1

# The guest boots the host's own /etc/systemd/system. On a machine where
# konedrive is installed, the host's enabled unit has already started here,
# from the host's installed binary. Stop it; the one under test replaces it.
systemctl stop "$unit" 2>/dev/null
systemctl reset-failed "$unit" 2>/dev/null

# --- install, only in the guest ----------------------------------------------
section "install"
check "the helper under test has no fault-injection hooks" \
    "! grep -q KONEDRIVE_FAULT_PANIC_ON_SIZE '$helper_bin'"
mkdir -p "${installed_bin%/*}"
install -m 0755 "$helper_bin" "$installed_bin"
install -m 0644 "$unit_src" "/etc/systemd/system/$unit"
# Whatever the host has, the guest starts with no registrations and no
# drop-ins: the unit file is the whole configuration.
rm -rf "/etc/systemd/system/$unit.d" "/run/systemd/system/$unit.d"
mkdir -p /var/lib/konedrive
mount -t tmpfs -o mode=0755,size=16M tmpfs /var/lib/konedrive
systemctl daemon-reload
# Drop-ins for every service (Fedora ships service.d/10-timeout-abort.conf)
# apply to it on a real machine as well, so only its own are ruled out.
check "systemd loads the shipped unit file, byte for byte, with no drop-in of its own" \
    "cmp -s '$unit_src' \"\$(show FragmentPath)\" && ! show DropInPaths | grep -q '$unit.d'"
check "ExecStart is the binary under test" "cmp -s '$helper_bin' '$installed_bin'"

# --- the folder --------------------------------------------------------------
# Mounted before the unit starts, as /home is on a real machine: the helper's
# mount namespace is set up at start, from what is mounted then.
modprobe loop
mount -t tmpfs -o size=3G tmpfs /mnt
mkdir -p /mnt/img /mnt/btrfs
truncate -s 2G /mnt/img/btrfs.img
mkfs.btrfs -q /mnt/img/btrfs.img
mount -o loop /mnt/img/btrfs.img /mnt/btrfs

# run_pass name
#
# (Re)starts the unit, checks that it stays up with its sandbox applied, runs
# the daemon's side of the check against it, and scans what the helper
# logged. Every pass registers a folder of its own under /mnt/btrfs/unit;
# the folders of the passes before it are the helper's to find again at
# startup, from its state file.
run_pass() {
    name=$1
    section "$name: start"
    systemctl daemon-reload
    systemctl restart "$unit"
    sleep 3
    check "[$name] the unit is active 3 s after start" "[ \"\$(systemctl is-active '$unit')\" = active ]"
    pid=$(show MainPID)
    restarts=$(show NRestarts)
    invocation=$(show InvocationID)
    check "[$name] its main process runs the binary under test" \
        "[ \"\$(readlink /proc/$pid/exe)\" = '$installed_bin' ]"
    # The sandbox is really there, not quietly skipped: a seccomp filter, no
    # new privileges, and exactly CAP_SYS_ADMIN (21) and
    # CAP_DAC_READ_SEARCH (2) in the effective set.
    grep -E '^(CapEff|NoNewPrivs|Seccomp):' "/proc/$pid/status"
    check "[$name] seccomp filter, no_new_privs, and only its two capabilities" \
        "grep -q '^Seccomp:[[:space:]]*2$' /proc/$pid/status && grep -q '^NoNewPrivs:[[:space:]]*1$' /proc/$pid/status && grep -q '^CapEff:[[:space:]]*0000000000200004$' /proc/$pid/status"
    check "[$name] it sees /mnt/btrfs, read-only" \
        "grep -q ' /mnt/btrfs ro[, ]' /proc/$pid/mountinfo"

    section "$name: a daemon works through the unit's socket"
    echo "SystemCallErrorNumber=$(show SystemCallErrorNumber)"
    "$scenarios_bin" --unit "$pid" /mnt/btrfs/unit "$name"
    rc=$?
    check "[$name] every step of the daemon's check passed" "[ $rc -eq 0 ]"
    check "[$name] the helper is still the same process, never restarted" \
        "[ \"\$(systemctl is-active '$unit')\" = active ] && [ \"\$(show MainPID)\" = '$pid' ] && [ \"\$(show NRestarts)\" = '$restarts' ]"

    section "$name: systemctl status"
    systemctl status "$unit" --no-pager -l --lines=0
    # This start only: what the helper wrote, and what systemd said about it.
    section "$name: the unit's journal"
    journalctl -b --no-pager -o short-monotonic \
        _SYSTEMD_INVOCATION_ID="$invocation" + INVOCATION_ID="$invocation" > /run/journal.txt
    cat /run/journal.txt
    section "$name: denials"
    # The helper killed by SIGSYS (systemd records the exit status; see the
    # control), an audit SECCOMP record should an audit daemon be running,
    # and anything the helper reported as refused — ENOSYS too, which is how
    # RestrictSUIDSGID= and RestrictNamespaces= answer the calls whose
    # arguments seccomp cannot read. One refusal is expected and set aside:
    # the helper's feature probe cannot write into the user's folder (EACCES
    # without CAP_DAC_OVERRIDE, EROFS under ProtectSystem=strict), which
    # check_filesystem in main.rs logs and tolerates.
    journalctl -b --no-pager | grep -E 'type=1326|SECCOMP' | grep -i konedrive > /run/denied.txt
    grep -v 'stopped the feature probe' /run/journal.txt |
        grep -iE 'EPERM|ENOSYS|operation not permitted|permission denied|function not implemented|os error (1|13|38)\)|status=31|SIGSYS|core-dump' \
        >> /run/denied.txt
    echo "expected: $(grep -c 'stopped the feature probe' /run/journal.txt) feature probe(s) refused by the sandbox"
    if [ -s /run/denied.txt ]; then
        cat /run/denied.txt
    else
        echo "(none)"
    fi
    check "[$name] no seccomp denial, and nothing refused with EPERM, EACCES or ENOSYS" "[ ! -s /run/denied.txt ]"
}

run_pass as-shipped
shipped_action=$(show SystemCallErrorNumber)

# The detector the second pass relies on, shown to work in this guest: a
# transient service with the same kind of filter, killing on denial, calls
# mount(2). The kernel writes no audit record of it here — with no audit
# daemon no task has an audit context, and seccomp logs a kill only through
# one — but systemd records how the process ended, and that is what the
# scan above matches.
section "control: a denial, seen"
systemd-run --wait --unit=seccomp-control \
    -p SystemCallFilter='~@mount' -p SystemCallErrorNumber=kill \
    /bin/sh -c 'mkdir -p /run/control && mount -t tmpfs control /run/control'
journalctl -b --no-pager -u seccomp-control.service | grep 'status=31/SYS'
check "[control] a filtered mount(2) is killed, and the journal says status=31/SYS" \
    "journalctl -b --no-pager -u seccomp-control.service | grep -q 'status=31/SYS'"

mkdir -p "/run/systemd/system/$unit.d"
printf '[Service]\nSystemCallErrorNumber=kill\n' > "/run/systemd/system/$unit.d/zz-seccomp-kill.conf"
run_pass denials-kill
check "[denials-kill] the drop-in changed the filter's action" \
    "[ \"\$(show SystemCallErrorNumber)\" != '$shipped_action' ]"

section "exposure, as systemd rates it"
systemd-analyze security "$unit" --no-pager 2>/dev/null | tail -1

echo
echo "$fails failed"
exit "$fails"

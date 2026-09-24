#!/bin/sh
# Installs the konedrive helper — the privileged part that makes a file in the
# sync folder download when a program opens it — as a system service.
#
# Build it first, as yourself (this script never builds anything as root):
#     cargo build --release -p konedrive-helper
# then:
#     sudo scripts/install-helper.sh               # install, or update
#     sudo scripts/install-helper.sh --uninstall
#
# It says what it will do and asks before doing it (--yes skips the question).
# --uninstall refuses while a folder is registered with the helper: run
# `konedrivectl sync forget` first (--force uninstalls anyway).
#
# KONEDRIVE_INSTALL_ROOT and KONEDRIVE_SYSTEMCTL exist only for the VM test
# (tests/vm/install_helper_test.sh); leave them unset.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
binary="$repo/target/release/konedrive-helper"
unit="$repo/packaging/systemd/konedrive-helper.service"
root=${KONEDRIVE_INSTALL_ROOT:-}
systemctl=${KONEDRIVE_SYSTEMCTL:-systemctl}
installed_binary="$root/usr/local/libexec/konedrive-helper"
installed_unit="$root/etc/systemd/system/konedrive-helper.service"
# The helper's own list of registered folders (ROOTS_FILE in
# crates/konedrive-helper/src/main.rs): {"by_id": {"<id>": {..., "path": ...,
# "root_id": ...}}}, pretty-printed, one key per line.
roots_file="$root/var/lib/konedrive/roots.json"

mode=install
ask=yes
force=no
for arg in "$@"; do
    case $arg in
        --uninstall) mode=uninstall ;;
        --yes) ask=no ;;
        --force) force=yes ;;
        *) echo "unknown argument: $arg (see the top of this script)" >&2; exit 2 ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "run this with sudo: it installs a system service" >&2
    exit 1
fi

if [ -n "${KONEDRIVE_INSTALL_ROOT+x}" ] || [ -n "${KONEDRIVE_SYSTEMCTL+x}" ]; then
    echo "WARNING: KONEDRIVE_INSTALL_ROOT and/or KONEDRIVE_SYSTEMCTL is set in this" >&2
    echo "WARNING: environment. Those exist only for the VM test and redirect every path" >&2
    echo "WARNING: and every systemctl call below — a real install must not have them set." >&2
    [ -n "${KONEDRIVE_INSTALL_ROOT+x}" ] && echo "WARNING:   KONEDRIVE_INSTALL_ROOT=$KONEDRIVE_INSTALL_ROOT" >&2
    [ -n "${KONEDRIVE_SYSTEMCTL+x}" ] && echo "WARNING:   KONEDRIVE_SYSTEMCTL=$KONEDRIVE_SYSTEMCTL" >&2
fi

confirm() {
    [ "$ask" = no ] && return 0
    printf 'Go ahead? [y/N] '
    read -r answer || answer=
    case $answer in
        y | Y | yes) ;;
        *) echo "Nothing was changed."; exit 1 ;;
    esac
}

# The Z1 warning: stopping the helper hands every open it holds back unfilled.
warn_if_running() {
    if "$systemctl" is-active --quiet konedrive-helper.service 2>/dev/null; then
        echo
        echo "The helper is running and will be $1. A program waiting for a file to download"
        echo "at that moment gets it as empty (docs/limitations-and-workarounds.md, Z1): close"
        echo "programs that are opening files in the sync folder first."
    fi
}

if [ "$mode" = uninstall ]; then
    # A folder the helper still holds would read as zeros once it is gone, and
    # its Forget is then refused (NoHelper) because the helper must let go of
    # it first. Fail closed: a file that cannot be read counts as a folder.
    registered=no
    if [ -e "$roots_file" ]; then
        found=0
        grep -q '"root_id"' "$roots_file" 2>/dev/null || found=$?
        case $found in
            0) registered=yes ;;
            1) ;;
            *) registered=unknown ;;
        esac
    fi
    if [ "$registered" != no ]; then
        if [ "$registered" = yes ]; then
            echo "A folder is still registered with the helper:" >&2
            sed -n 's/^ *"path": *"\(.*\)",\{0,1\}$/    \1/p' "$roots_file" >&2
        else
            echo "Cannot read $roots_file to tell whether a folder is still registered." >&2
        fi
        echo "Without the helper, its files that are not downloaded read as zeros, and" >&2
        echo "\`konedrivectl sync forget\` is then refused (NoHelper)." >&2
        if [ "$force" = no ]; then
            echo "Run \`konedrivectl sync forget\` first, as the folder's owner, then run this again" >&2
            echo "(--force uninstalls anyway)." >&2
            exit 1
        fi
        echo "--force: uninstalling anyway." >&2
    fi
    echo "This will:"
    echo "  stop and disable konedrive-helper.service"
    echo "  remove $installed_unit"
    echo "  remove $installed_binary"
    warn_if_running stopped
    confirm
    "$systemctl" disable --now konedrive-helper.service || true
    rm -f "$installed_unit" "$installed_binary"
    "$systemctl" daemon-reload
    echo "The helper is uninstalled. A OneDrive folder is kept in step only while it is installed"
    echo "and running."
    exit 0
fi

if [ -L "$binary" ] || [ ! -f "$binary" ]; then
    echo "there is no release build at $binary (or it is not a regular file). Build it as" >&2
    echo "yourself first:" >&2
    echo "    cargo build --release -p konedrive-helper" >&2
    exit 1
fi
# Everything below checks, and then installs, one root-owned copy: the file in
# target/release belongs to the user and could change between the check and
# the install. -P copies a symlink as a symlink, which the test after it
# refuses, so a swap in between is caught rather than followed.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
staged="$stage/konedrive-helper"
cp -P --preserve=timestamps "$binary" "$staged"
if [ -L "$staged" ] || [ ! -f "$staged" ]; then
    echo "$binary changed into something other than a regular file while it was copied" >&2
    exit 1
fi
# The test-only fault hooks (part 1's) must never be installed.
# grep exits 1 for "not there"; anything else (2: it could not read the file)
# is not a clean answer and refuses.
found=0
grep -a -q 'KONEDRIVE_FAULT_' "$staged" || found=$?
case $found in
    1) ;;
    0)
        echo "$binary was built with the fault-injection feature, which is for the VM suite only." >&2
        echo "Rebuild it without: cargo build --release -p konedrive-helper" >&2
        exit 1
        ;;
    *)
        echo "cannot check $binary for the fault-injection feature (grep exit $found)" >&2
        exit 1
        ;;
esac
for src in "$repo/crates/konedrive-helper" "$repo/crates/konedrive-proto" "$repo/crates/konedrive-fs" \
        "$repo/crates/konedrive-helper/Cargo.toml"; do
    if [ ! -e "$src" ]; then
        echo "cannot check whether $binary is current: $src does not exist" >&2
        exit 1
    fi
done
# Only what cargo relinks the helper for: the three crates' Rust sources and
# the helper's own manifest. find's own exit status, not a pipe's: piping into
# `head` would hide a failed find (an unreadable source, say) behind head's
# own success and let a stale or tampered binary install as if it were current.
newer_out="$stage/newer"
if find "$repo/crates/konedrive-helper" "$repo/crates/konedrive-proto" "$repo/crates/konedrive-fs" \
        -type f -name '*.rs' -newer "$staged" -print >"$newer_out" 2>/dev/null \
        && find "$repo/crates/konedrive-helper/Cargo.toml" -newer "$staged" -print >>"$newer_out" 2>/dev/null; then
    newer=$(head -n 1 "$newer_out")
else
    echo "cannot check whether $binary is current: find failed checking its sources" >&2
    exit 1
fi
if [ -n "$newer" ]; then
    echo "$binary is older than $newer. Rebuild it first:" >&2
    echo "    cargo build --release -p konedrive-helper" >&2
    echo "If this still refuses after a build (cargo relinks only when something it compiles" >&2
    echo "changed), run:" >&2
    echo "    touch crates/konedrive-helper/src/main.rs && cargo build --release -p konedrive-helper" >&2
    exit 1
fi

echo "This will:"
echo "  install $binary"
echo "       as $installed_binary"
echo "  install $unit"
echo "       as $installed_unit"
echo "  reload systemd, enable konedrive-helper.service and (re)start it"
warn_if_running restarted
confirm
install -D -m 0755 -o root -g root "$staged" "$installed_binary"
install -D -m 0644 -o root -g root "$unit" "$installed_unit"
if command -v restorecon >/dev/null 2>&1; then
    restorecon "$installed_binary" "$installed_unit" 2>/dev/null || true
fi
"$systemctl" daemon-reload
"$systemctl" enable konedrive-helper.service
"$systemctl" restart konedrive-helper.service
echo "The helper is installed and running. The daemon connects to it within half a minute;"
echo "\`konedrivectl sync status\` then says that opens are intercepted."

#!/bin/sh
# Exercises scripts/install-helper.sh as root inside the VM:
# against a scratch repository layout and a fake systemctl, never the host.
set -u
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
fails=0
check() { if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi; }

scratch=$(mktemp -d)
mkdir -p "$scratch/repo/scripts" "$scratch/repo/packaging/systemd" "$scratch/repo/target/release" \
         "$scratch/repo/crates/konedrive-helper/src" "$scratch/repo/crates/konedrive-proto/src" "$scratch/repo/crates/konedrive-fs/src"
cp "$repo/scripts/install-helper.sh" "$scratch/repo/scripts/"
cp "$repo/packaging/systemd/konedrive-helper.service" "$scratch/repo/packaging/systemd/"
for f in crates/konedrive-helper/src/main.rs crates/konedrive-proto/src/lib.rs crates/konedrive-fs/src/lib.rs crates/konedrive-helper/Cargo.toml; do
    echo source > "$scratch/repo/$f"
done
touch -d '2 hours ago' "$scratch/repo/crates"/*/src/* "$scratch/repo/crates/konedrive-helper/Cargo.toml"
printf 'a helper binary' > "$scratch/repo/target/release/konedrive-helper"
export KONEDRIVE_INSTALL_ROOT="$scratch/root" KONEDRIVE_SYSTEMCTL="$scratch/systemctl"
printf '#!/bin/sh\necho "$@" >> %s/systemctl.log\n' "$scratch" > "$scratch/systemctl"
chmod +x "$scratch/systemctl" "$scratch/repo/scripts/install-helper.sh"
installer="$scratch/repo/scripts/install-helper.sh"
bin="$scratch/root/usr/local/libexec/konedrive-helper"
unit="$scratch/root/etc/systemd/system/konedrive-helper.service"

check "a plain user is refused" "! setpriv --reuid 1000 --regid 1000 --clear-groups sh '$installer' --yes >/dev/null 2>&1"
check "answering no changes nothing" "! echo n | sh '$installer' >/dev/null 2>&1 && [ ! -e '$bin' ]"
check "installs the binary and the unit" "sh '$installer' --yes >/dev/null && [ -x '$bin' ] && [ -f '$unit' ]"
check "modes and owner" "[ \"\$(stat -c %a:%U '$bin')\" = 755:root ] && [ \"\$(stat -c %a:%U '$unit')\" = 644:root ]"
check "reloads, enables and starts" "grep -q '^daemon-reload' '$scratch/systemctl.log' && grep -q '^enable konedrive-helper.service' '$scratch/systemctl.log' && grep -q '^restart konedrive-helper.service' '$scratch/systemctl.log'"
check "running it again is harmless" "sh '$installer' --yes >/dev/null && [ -x '$bin' ]"

sh "$installer" --yes >"$scratch/warn_out.txt" 2>&1
check "warns about the test-only variables" "grep -q 'KONEDRIVE_INSTALL_ROOT' '$scratch/warn_out.txt' && grep -q 'KONEDRIVE_SYSTEMCTL' '$scratch/warn_out.txt'"

touch "$scratch/repo/crates/konedrive-proto/src/lib.rs"
check "a binary older than its sources is refused" "! sh '$installer' --yes >/dev/null 2>&1"
touch -d '2 hours ago' "$scratch/repo/crates/konedrive-proto/src/lib.rs"

rm -rf "$scratch/repo/crates/konedrive-fs"
check "a missing source path is refused" "! sh '$installer' --yes >/dev/null 2>&1"
mkdir -p "$scratch/repo/crates/konedrive-fs/src"
echo source > "$scratch/repo/crates/konedrive-fs/src/lib.rs"
touch -d '2 hours ago' "$scratch/repo/crates/konedrive-fs/src/lib.rs"

printf 'x KONEDRIVE_FAULT_PANIC_ON_SIZE x' > "$scratch/repo/target/release/konedrive-helper"
check "a fault-injection build is refused" "! sh '$installer' --yes >/dev/null 2>&1"
rm "$scratch/repo/target/release/konedrive-helper"
check "no build at all is refused" "! sh '$installer' --yes >/dev/null 2>&1"

# The helper's own roots.json, as it writes it, with one folder in it.
mkdir -p "$scratch/root/var/lib/konedrive"
roots="$scratch/root/var/lib/konedrive/roots.json"
printf '{\n  "by_id": {\n    "r1": {\n      "uid": 1000,\n      "dev": 42,\n      "ino": 7,\n      "path": "/home/u/OneDrive",\n      "root_id": "r1"\n    }\n  }\n}' > "$roots"
check "uninstall refuses while a folder is registered" "! sh '$installer' --uninstall --yes >/dev/null 2>&1 && [ -x '$bin' ] && ! grep -q '^disable' '$scratch/systemctl.log'"
printf '{\n  "by_id": {}\n}' > "$roots"

check "uninstall removes both and disables" "sh '$installer' --uninstall --yes >/dev/null && [ ! -e '$bin' ] && [ ! -e '$unit' ] && grep -q '^disable --now konedrive-helper.service' '$scratch/systemctl.log'"

rm -rf "$scratch"
echo "$fails failed"
exit "$fails"

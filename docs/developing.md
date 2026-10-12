# Developing KOneDrive

Building, installing a development copy and running the tests. The checklist before sending a
change is in [CONTRIBUTING.md](../CONTRIBUTING.md); the map of the code is
[code-map.md](code-map.md).

## Build dependencies (Fedora)

```
sudo dnf install rust cargo cmake extra-cmake-modules gcc-c++ qt6-qtbase-devel \
  qt6-qtdeclarative-devel kf6-kirigami-devel kf6-kirigami-addons-devel kf6-ki18n-devel \
  kf6-kcoreaddons-devel kf6-kconfig-devel kf6-knotifications-devel \
  kf6-kstatusnotifieritem-devel kf6-kdbusaddons-devel kf6-kio-devel kf6-kwindowsystem-devel \
  kf6-kjobwidgets-devel dbus-daemon desktop-file-utils
```

These are what `app/CMakeLists.txt` and `dolphin/CMakeLists.txt` look for.

A stable Rust toolchain (edition 2021), CMake 3.24+ and Extra CMake Modules are needed.

## What it needs to run

- **FUSE 3**: the daemon serves each sync folder as a FUSE mount, put on and taken off with
  `fusermount3` (the `fuse3` package). Nothing runs as root.
- **KDE Plasma 6 and Qt 6.8 or later.**
- **KDE Frameworks 6.21 or later.** The build asks for 6.8, but with KIO before 6.21 "Move to
  Trash" in a sync folder copies the file into the home trash, which downloads it
  ([kio-behavior.md](kio-behavior.md), D; limitations log K34).

## Building the RPM packages

Build the packages as yourself, never as root. `rpm-build` and the build
dependencies are needed once (`builddep` installs whatever of the list above, and of the spec's,
is missing):

```
sudo dnf install rpm-build
sudo dnf builddep packaging/rpm/konedrive.spec
scripts/build-rpm.sh
```

`scripts/build-rpm.sh` packages the committed tree (`HEAD`: uncommitted changes are left out),
with its Rust crates vendored so that the build itself is offline. The version is the next
release's, from `Cargo.toml`, with the number of commits in `HEAD`'s history — for example
`0.1.1~dev.57` — so a later build upgrades an earlier one, and the release 0.1.1 upgrades them all
(`docs/releasing.md`). Everything it makes is under `target/rpm/`, and it lists the RPMs at the end. Then, from the
repository:

```
sudo dnf install ./target/rpm/RPMS/x86_64/konedrive-*.rpm
```

Installing, upgrading and removing them: [user-guide.md](user-guide.md), "Install".

## Install for your user

```
scripts/dev-install.sh
```

This installs `konedrived`, `konedrivectl` and `konedrive` into `~/.local/bin`, the systemd
user unit, the D-Bus activation file and the launcher entry. The daemon starts on demand; it
needs `fusermount3` (the `fuse3` package) to mount a folder. `scripts/dev-uninstall.sh`
removes it all again.

## Switching from the developer install to the packages

The developer install (`scripts/dev-install.sh`) and the packages must not be installed
together. The developer install's files take precedence over the package's: the daemon's unit in
`~/.config`, its D-Bus activation file and programs in `~/.local`. Build the RPMs first, then,
from the repository:

```
scripts/dev-uninstall.sh
sudo dnf install ./target/rpm/RPMS/x86_64/konedrive-*.rpm
```

(or, from a release, `sudo dnf install ./konedrive-*.rpm` where you downloaded them).

`scripts/dev-uninstall.sh` runs as you. It stops the daemon, removes exactly the files
`scripts/dev-install.sh` installed, and reloads your systemd and D-Bus. It never touches your
settings (`~/.config/konedrive/config.toml`), the tree stores, the refresh tokens in KWallet or
the sync folders, so the packaged daemon carries on from where this one stopped. It points
"Start at login" at `/usr/bin/konedrive`, and it points out Dolphin plugins you installed for
your user by hand: remove those, and `~/.config/plasma-workspace/env/konedrive-dolphin.sh`,
as "The Dolphin plugins" below says, so that Dolphin loads the packaged ones. If the KOneDrive
window was running, quit it from its tray icon and start it again from the launcher.

## The Dolphin plugins

`dolphin/` holds the two Dolphin plugins of the `konedrive-kde` package; what they do is in
[user-guide.md](user-guide.md), "Dolphin".

Build and test (the tests run on private D-Bus buses, and one of them takes 30
seconds):

```
cmake -S dolphin -B build/dolphin -DBUILD_TESTING=ON && cmake --build build/dolphin && ctest --test-dir build/dolphin --output-on-failure
```

**Install for your user** (no root). This puts two files under
`~/.local/lib64/plugins/kf6/` (the install output shows the exact paths):

```
cmake -S dolphin -B build/dolphin-user -DCMAKE_INSTALL_PREFIX="$HOME/.local" -DCMAKE_BUILD_TYPE=RelWithDebInfo -DBUILD_TESTING=OFF
cmake --build build/dolphin-user && cmake --install build/dolphin-user
```

Qt does not look there by itself. Tell Plasma to, then log out and back in:

```
mkdir -p ~/.config/plasma-workspace/env
echo 'export QT_PLUGIN_PATH="$HOME/.local/lib64/plugins${QT_PLUGIN_PATH:+:$QT_PLUGIN_PATH}"' > ~/.config/plasma-workspace/env/konedrive-dolphin.sh
```

To try it before logging out, start a separate Dolphin with the variable set:
`QT_PLUGIN_PATH="$HOME/.local/lib64/plugins" dolphin --new-window`. To remove it,
delete the files listed in `build/dolphin-user/install_manifest.txt` and the
`konedrive-dolphin.sh` above.

**Install for the whole system** — Qt finds the plugins there with no
environment; restart Dolphin afterwards:

```
cmake -S dolphin -B build/dolphin-system -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_BUILD_TYPE=RelWithDebInfo -DBUILD_TESTING=OFF
cmake --build build/dolphin-system && sudo cmake --install build/dolphin-system
```

This installs into `/usr/lib64/qt6/plugins/kf6/overlayicon/` and
`.../kf6/kfileitemaction/`; `sudo xargs rm < build/dolphin-system/install_manifest.txt`
removes it.

## What `konedrivectl` says of a refusal

When the daemon refuses a command, `konedrivectl` says what that means for
your file and what to do next — for example, that a file you changed here
and has not been uploaded cannot be freed up without losing your edits —
rather than repeating the daemon's D-Bus error. It tells the refusals apart
by their D-Bus error names (`org.konedrive.Error.ModifiedLocally`,
`.NotHydrated`, `.NotUp`, …), which is also what a script should match on.

## Tests

```
cargo test --workspace
cmake -S app -B build/app -DBUILD_TESTING=ON && cmake --build build/app && ctest --test-dir build/app --output-on-failure
```

The Secret Service test touches your real KWallet (under a test-only attribute) and is opt-in:

```
cargo test -p konedrived secret_service_round_trip -- --ignored
```

### The VM suite

A sync folder is a FUSE mount, and nothing is mounted on the host: the tests that need a mounted
folder run inside a `virtme-ng` VM, on an ext4 disk of its own, over the fake OneDrive.

```
tests/fuse-probe/run.sh                           # every default group
tests/fuse-probe/run.sh file_reads                # one group, or several
ONLY="<scenario>" tests/fuse-probe/run.sh file_writes     # one scenario of a group
tests/fuse-probe/run.sh kio_trash                 # by name only
```

- **The groups**: `service` (the daemon's service command by command; it prints lines for a
  person and checks nothing), `mounted_folder`, `file_writes`, `file_reads` and
  `daemon_restarts` (scenarios with checks, a file each in `tests/fuse-probe/`), and `cli`
  (`konedrivectl`'s tests of a mounted folder, `crates/konedrivectl/tests/sync_cli`).
- **`kio_trash`** runs by name only: it builds `tests/kio/trash_probe.cpp`, which needs the Qt 6
  and KDE Frameworks 6 development packages on the host, and the guest runs it with the host's
  libraries.
- **One scenario**: `ONLY` names the scenarios to run, of the groups asked for.
- **Reading a failure**: `SERVICE_LOG=debug EVIDENCE_LOG_LINES=400` set the level of the daemon's
  log and how much of it a failed scenario prints.
- **The result**: the run fails when a scenario fails that `tests/fuse-probe/expected-to-fail.txt`
  does not name, or when the guest does not reach its end.
- **What the host needs**: `vng` (virtme-ng), QEMU and `/dev/kvm` (your user in the group `kvm`).
  No `sudo`. `run.sh` builds the suite's programs in release mode into `tests/fuse-probe/target/`
  and makes the guest's disk there, `disk.img`, 24 GiB; `VM_MEMORY` sets the guest's memory (8G
  unless said).

A trial of this tree's daemon on your own machine, beside an installed one and with a
configuration and a state of its own: `tests/fuse-probe/try-on-host.sh start | restart | stop |
status`. Register a new, empty folder with it, never the installed daemon's.

### The test account

Uploads are checked against a real account only on a separate test account, by hand, with
`konedrive-write-test` (`tests/write-account/`): it refuses to start unless the drive is the test
account's, is listed in `write_test_drive_ids` in `config.toml`, and looks like a test account, and it writes only inside a
folder of its own. How to run it: [`docs/design/writes.md`](design/writes.md) §12.1.

For a heavier, end-to-end workout of the upload path — many files, edits, moves, a file moved or
edited mid-upload, deletes — against a real read-write test account, see
[`tests/stress/README.md`](../tests/stress/README.md). It refuses to start unless the account's drive
is listed in `write_test_drive_ids` in `config.toml`: that an account is read-write does not make
it a test account.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the full checklist before sending a change, and
[SECURITY.md](../SECURITY.md) for how to report a vulnerability privately.

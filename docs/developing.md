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
user unit, the D-Bus activation file and the launcher entry. The daemon starts on demand. The
helper is installed separately ("Installing the helper", below). `scripts/dev-uninstall.sh`
removes it all again, apart from the helper.

## Switching from the developer install to the packages

The developer install (`scripts/dev-install.sh` and `scripts/install-helper.sh`) and the packages
must not be installed together. The developer install's files take precedence over the
package's: the daemon's unit in `~/.config`, its D-Bus activation file and programs in
`~/.local`, and the helper's unit in `/etc/systemd/system`, which would keep the old helper
running instead of the packaged one (R2). Build the RPMs first, then, from the repository:

```
scripts/dev-uninstall.sh
sudo scripts/install-helper.sh --uninstall --force
sudo dnf install ./target/rpm/RPMS/x86_64/konedrive-*.rpm
```

(or, from a release, `sudo dnf install ./konedrive-*.rpm` where you downloaded them).

1. `scripts/dev-uninstall.sh` runs as you. It stops the daemon, removes exactly the files
   `scripts/dev-install.sh` installed, and reloads your systemd and D-Bus. It never touches your
   settings (`~/.config/konedrive/config.toml`), the tree stores, the refresh tokens in KWallet or
   the sync folders, so the packaged daemon carries on from where this one stopped. It points
   "Start at login" at `/usr/bin/konedrive`, and it points out Dolphin plugins you installed for
   your user by hand: remove those, and `~/.config/plasma-workspace/env/konedrive-dolphin.sh`,
   as "The Dolphin plugins" below says, so that Dolphin loads the packaged ones.
2. `--force`, because your folders are registered: they stay registered, and the packaged helper
   takes them over when it starts. Until then nothing intercepts the folders, so run the three
   commands one after the other, with nothing opening files in them.
3. The install starts the new helper. If the KOneDrive window was running, quit it from its tray
   icon and start it again from the launcher.

## Installing the helper

The helper is the privileged part of the sync folder: a small systemd service
that makes a placeholder download the moment a program opens it, instead of
that program reading zeros. The `konedrive` package installs it
(`/usr/libexec/konedrive-helper`) and starts it; this section is for the
developer install. Build it as yourself, then install it as root:

```
cargo build --release -p konedrive-helper
sudo scripts/install-helper.sh
```

`scripts/install-helper.sh` copies the built binary to
`/usr/local/libexec/konedrive-helper` and the unit in
`packaging/systemd/konedrive-helper.service` to `/etc/systemd/system/`, then
reloads systemd, enables the service and starts (or restarts) it. It refuses
to run as a plain user. It refuses a binary that is missing, a symlink or not
a regular file, built with the VM suite's fault-injection hooks, or older than
its sources (the Rust files of the helper and the two crates it uses, and the
helper's `Cargo.toml`). It checks one root-owned copy of the binary and
installs that same copy. It always says exactly what it is about to do and
asks before doing it — pass `--yes` to skip the question. Running it again
updates the helper in place; if it is already running, the installer restarts
it and says so before it asks (see `docs/limitations/`, Z1:
a program waiting for a file to download at that moment gets an error, and a
file that is not downloaded reads as zeros if it is opened in the moment no
helper runs; a helper that crashes or is killed still lets a waiting open
through to zeros).

If it says the binary is older than its sources right after a build, cargo
had nothing to relink; this makes it:

```
touch crates/konedrive-helper/src/main.rs && cargo build --release -p konedrive-helper
```

The daemon connects to a newly started helper within half a minute; until
then `konedrivectl sync status` does not say `Helper: connected`, and
`konedrivectl sync register` is refused (`NoHelper`).

```
sudo scripts/install-helper.sh --uninstall
```

stops and disables the service and removes both installed files. It refuses
while a folder is registered with the helper (it reads the helper's own
`/var/lib/konedrive/roots.json`): without the helper, that folder's files that
are not downloaded would read as zeros, and `konedrivectl sync forget` would
then be refused (`NoHelper`). Run `konedrivectl sync forget` first, for each
account with a folder (`--account`); `--force` uninstalls anyway. Like an update, it warns first if the helper is running
(Z1): a program waiting for a file to download when the helper stops gets an
error.

See [SECURITY.md](../SECURITY.md) for what the helper can do as root and how its
systemd unit narrows that down.

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

## A folder without OneDrive or the helper

This is a developer's and tester's mode, not a way to use KOneDrive: the window does not offer it.
It drives the sync folder entirely from the command line, with a local directory standing in for
the cloud and no helper at all. The folder belongs to an account like any other; an account that
never signs in is enough, and only a development build (`scripts/dev-install.sh`, the `dev-tools`
feature) can add one, with `konedrivectl dev add-account`, as below. A release build adds an
account only by signing in.

`konedrivectl sync register-without-interception` always makes a local folder, filled with
`populate-from` — even when you are signed in, it never shows your OneDrive (that needs
`konedrivectl sync register` and the helper). And it has a real cost that the name is meant to
make obvious: **nothing** fills a placeholder when something opens it. A file in this folder reads
as zeros — not an error, not a missing file, silently the wrong bytes — until you fetch it
yourself with `konedrivectl sync hydrate`. `konedrivectl sync status` keeps repeating that warning
for as long as the folder stays registered this way. On a machine where a helper *is* running,
freeing a file up here still asks it to clear the file's mark first, and is refused (`NoHelper`)
while the daemon is not connected to it; with no helper at all, as below, nothing is asked.

```
mkdir -p ~/OneDrive-test ~/fake-cloud/sub
head -c 1M </dev/urandom > ~/fake-cloud/big.bin
echo hello > ~/fake-cloud/sub/note.txt

konedrivectl dev add-account Test            # an account for the test, never signed in
export KONEDRIVE_ACCOUNT=Test                # the account the commands below act on
konedrivectl sync register-without-interception ~/OneDrive-test
konedrivectl sync populate-from ~/fake-cloud
ls -l ~/OneDrive-test        # real sizes
du -sh ~/OneDrive-test       # ~0: nothing is stored yet
cat ~/OneDrive-test/sub/note.txt                        # reads as zeros: nothing fills it here
konedrivectl sync hydrate ~/OneDrive-test/sub/note.txt
cat ~/OneDrive-test/sub/note.txt                        # now "hello"
konedrivectl sync state ~/OneDrive-test/sub/note.txt    # hydrated
konedrivectl sync dehydrate ~/OneDrive-test/sub/note.txt
du -sh ~/OneDrive-test       # ~0 again
konedrivectl sync forget
konedrivectl account remove Test
unset KONEDRIVE_ACCOUNT
```

`konedrivectl sync status` always says something useful, including with no
helper installed at all: `State: none` and `Folder: (none)` before anything
is registered; `no-interception` for the mode above, with an `Opens:` line
saying in plain words that a file that is not downloaded reads as zeros until
you hydrate it — every time, success or not; `ready` once a folder is bound
with the real helper intercepting it (`Opens: intercepted`); or `error`
with a `Last error:` line explaining what needs attention (for example, if
startup recovery could not finish, or a OneDrive folder is waiting for the
helper). Its `Helper:` line says what the daemon knows of the helper, with
what to do about it. `error` is never printed to look like a
success — and neither is `register`/`register-without-interception`
themselves: either command exits non-zero, without its usual "Folder
registered" line, if the root it just bound was not fully recovered.

When the daemon refuses a command, `konedrivectl` says what that means for
your file and what to do next — for example, that a file you changed here
and has not been uploaded cannot be freed up without losing your edits —
rather than repeating the daemon's D-Bus error. It tells the refusals apart
by their D-Bus error names (`org.konedrive.Error.ModifiedLocally`,
`.NotHydrated`, `.NoHelper`, …), which is also what a script should match on.

## Tests

Tests:

```
cargo test --workspace
cmake -S app -B build/app -DBUILD_TESTING=ON && cmake --build build/app && ctest --test-dir build/app --output-on-failure
```

The Secret Service test touches your real KWallet (under a test-only attribute) and is opt-in:

```
cargo test -p konedrived secret_service_round_trip -- --ignored
```

The fanotify helper needs a real kernel and root, so its end-to-end suite runs inside a
`virtme-ng` VM — no root and no privileges needed on the host, which boots the VM and does
everything privileged inside it:

```
tests/vm/run.sh quick   # the normal run: the end-to-end suite on ext4 (one VM)
tests/vm/run.sh full    # btrfs, ext4 and xfs, three VMs at once: slower; for changes that
                         # may behave differently per filesystem
```

A run against your actual OneDrive account is also possible. It lists your whole drive into the
VM, as placeholders (names and sizes, no content: the daemon cannot list just one folder), but it
opens and downloads files only inside one folder you name, each under a size cap, and fetches no
thumbnails. The token is the chosen account's: with several accounts, name it with `--account`.
It needs a development install (`scripts/dev-install.sh`, which builds with the `dev-tools`
feature): the released package does not hand out tokens.

```
konedrivectl dev export-access-token --out /tmp/konedrive-token   # about an hour of read access
VM_NETWORK=user tests/vm/run.sh quick --graph-token /tmp/konedrive-token \
    --graph-folder "<a folder in your OneDrive>"                  # --graph-max-bytes N: default 32 MiB
rm /tmp/konedrive-token
```

`--graph-folder` is required with `--graph-token`. It must name a folder (an empty path or one
with `..` is refused; a leading `/` is fine), and the run fails if nothing in the listing lies
inside it. The dropped-connection and restart-resume checks (G3, G4) run against the real account
only if you add `--graph-resume-checks`. The token is never the refresh token and is read only
inside the guest. See `docs/limitations/`, W15.

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

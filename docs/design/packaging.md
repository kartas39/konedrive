# Packaging

KOneDrive installs on Fedora as two RPM packages, built from one spec
(`packaging/rpm/konedrive.spec`) by `scripts/build-rpm.sh`, on the user's own machine. There is no
package repository yet: a COPR one needs a Fedora account and comes later. The README's "Install
from RPM" and "Switching from the developer install" are the user's side of this page.

## The two packages

| Package | Contents | Relations |
|---|---|---|
| `konedrive` | `/usr/bin/konedrived`, `/usr/bin/konedrivectl`, `/usr/bin/konedrive` (the window); `/usr/libexec/konedrive-helper`; the system unit `/usr/lib/systemd/system/konedrive-helper.service` and its preset `/usr/lib/systemd/system-preset/80-konedrive.preset`; the user unit `/usr/lib/systemd/user/konedrived.service`; the D-Bus activation file `/usr/share/dbus-1/services/org.konedrive.Daemon.service`; the launcher entry `/usr/share/applications/org.konedrive.KOneDrive.desktop`; the notification events `/usr/share/knotifications6/konedrive.notifyrc`; README, SECURITY.md, LICENSE and `docs/` | `Recommends: konedrive-kde` of the same version and release; `Requires:` the Kirigami, Kirigami Addons and Qt Quick QML modules, which no library dependency pulls in |
| `konedrive-kde` | the Dolphin plugins, `/usr/lib64/qt6/plugins/kf6/overlayicon/konedriveoverlay.so` and `/usr/lib64/qt6/plugins/kf6/kfileitemaction/konedriveactions.so`; LICENSE | `Requires: dolphin`; not `konedrive` |

There are no icons of our own: the launcher entry, the autostart entry and the tray use the theme's
`folder-cloud`.

**Why the helper is in the main package.** The daemon and the helper cannot work without each
other. Without the helper a OneDrive folder is not kept in step at all, and a placeholder opened
without it reads as zeros; without the daemon the helper has no one to hand an open to. A separate
helper package would only add a way to install half of the client.

**Why the Dolphin plugins are separate.** They are the one part that belongs to another program.
They load into Dolphin, so their package requires Dolphin, and they need nothing from `konedrive`
to load: the emblems come from each file's extended attribute and work with the daemon stopped,
and the menu actions say plainly when the daemon is not there. `konedrive` recommends them, so
`dnf` installs them by default, and `sudo dnf remove konedrive-kde` takes them away alone. The
name leaves room for other KDE integration later.

## What the build does

`scripts/build-rpm.sh` runs as the user and writes only under `target/rpm/` (rpmbuild's
`_topdir`), which it empties first so that the output directories hold this build alone.

1. **Source0** is `git archive` of `HEAD`: the committed tree, never the working tree.
2. **Source1** holds the crates of that tree's `Cargo.lock` (`cargo vendor`, run on an unpacked
   copy of Source0) and the `.cargo/config.toml` that `cargo vendor` prints, which points cargo at
   them. The spec unpacks it into the source tree.
3. `rpmbuild -ba` builds the binary packages and the source RPM.

The spec's version is Cargo's (`[workspace.package]` in `Cargo.toml`); the script refuses to build
when the two differ.

Inside the spec:

- **Rust.** `cargo build --release --offline --locked` for `konedrived`, `konedrivectl` and
  `konedrive-helper`, with Fedora's `RUSTFLAGS`, cargo's home and output inside the build
  directory, and no `--features`. The `fault-injection` features are the VM suite's alone
  (limitations log W8), and the build stops if the helper contains a `KONEDRIVE_FAULT_` string,
  the same check `scripts/install-helper.sh` makes.
- **C++.** `app/` and `dolphin/` are configured with Fedora's `%cmake_kf6` (installed under
  `/usr`, KDE's system directories, tests off), each in its own build directory, and installed
  with their own CMake install rules. That is what puts the plugins under
  `%{_qt6_plugindir}/kf6/`.
- **The units and the activation file** come from `packaging/`, where they serve the developer
  install: the daemon runs from `~/.local/bin`, the helper from `/usr/local/libexec`, and the
  D-Bus file's `Exec=` is `/bin/false`, leaving the start to systemd (`SystemdService=`).
  `%install` rewrites each of those lines to the packaged program (`/usr/bin/konedrived`,
  `/usr/libexec/konedrive-helper`) and stops if a line it expects is not there, so the two
  installs keep one set of files and cannot drift apart silently. The helper's hardening
  (SECURITY.md, limitations log W16) is therefore the same in both.
- **`%check`** runs `desktop-file-validate` on the launcher entry. The test suites do not run in
  the package build: they are run in development (CONTRIBUTING.md), and the helper's need root
  and a VM.
- **No debuginfo packages.** The RPMs are installed with a glob over the output directory, which
  the debuginfo and debugsource packages would match too, and the debugsource one would carry
  every vendored crate.

## Install, upgrade and removal

The helper's scriptlets are the systemd macros, plus one start. The daemon's are the user-unit
macros.

| When | The helper (`konedrive-helper.service`) | The daemon (`konedrived.service`, per user) |
|---|---|---|
| First install | the preset enables it (`%systemd_post`), and `%post` starts it at once | nothing: D-Bus starts it on demand, when the window or `konedrivectl` first calls it |
| Upgrade | restarted when the transaction ends (`%systemd_postun_with_restart`), if it was running | restarted when the transaction ends, for every logged-in user whose daemon runs (`%systemd_user_postun_with_restart`) |
| Removal | stopped and disabled (`%systemd_preun`) | stopped and disabled for every logged-in user (`%systemd_user_preun`) |

**Why the helper is enabled and started on install.** A OneDrive folder can only be registered
while the helper runs, and is kept in step only while it runs, so an installed but stopped helper
is a broken install. Fedora leaves services off unless a preset enables them, so the package
carries its own preset. It also starts the helper at once, on first install only, rather than at
the next boot: the user can then sign in and register a folder straight away. The daemon connects
to it within half a minute (`konedrivectl sync status` then says `Helper: connected`). The start
runs in `%post`, before systemd's end-of-transaction reload, so `%post` reloads systemd first;
a start that fails says so in `dnf`'s output, naming `systemctl status konedrive-helper`, and
does not fail the install (limitations log R5).

**What an upgrade costs.** The helper is restarted so that the new binary runs, and the daemons
with it, so that the two sides always run the same version. Stopping the helper closes its fanotify
group, and the kernel then lets every open still waiting for a download through: a program that
waits for a file at that moment reads the placeholder's zeros, and that download is cut off
(limitations log Z1, R1). `scripts/install-helper.sh` warns before it does the same; the package
cannot, as `dnf` asks once for the whole transaction. Upgrade when nothing is opening files in the
folder.

The first upgrade from a single-account version to one with multiple accounts also migrates each
user's configuration, when the restarted daemon starts ([accounts.md](accounts.md) §8), and moves
the daemon's D-Bus objects: a KOneDrive window or a Dolphin that was running across it has to be
restarted (limitations log F46). There is no downgrade (F41).

**What removal leaves.** The helper's list of registered folders (`/var/lib/konedrive/roots.json`)
and each user's settings, tree stores, KWallet entries and sync folders stay. A folder still
registered when the helper stops reads as zeros where its files are not downloaded, and
`konedrivectl` goes with the package, so `konedrivectl --account <account> sync forget` comes
first, for every account's folder (limitations log R3). Unlike
`scripts/install-helper.sh --uninstall`, the package does not refuse.

## The developer install and the packages

The two must not be installed together (limitations log R2). Each file of the developer install
takes precedence over the package's:

| Developer install | Takes precedence over |
|---|---|
| `~/.config/systemd/user/konedrived.service` | `/usr/lib/systemd/user/konedrived.service` |
| `~/.local/share/dbus-1/services/org.konedrive.Daemon.service` | `/usr/share/dbus-1/services/org.konedrive.Daemon.service` |
| `~/.local/bin/konedrive*`, ahead of `/usr/bin` on the usual `PATH` | `/usr/bin/konedrive*` |
| `/etc/systemd/system/konedrive-helper.service` (from `scripts/install-helper.sh`), running `/usr/local/libexec/konedrive-helper` | `/usr/lib/systemd/system/konedrive-helper.service` |
| Dolphin plugins installed for the user by hand, with `QT_PLUGIN_PATH` | the `konedrive-kde` plugins |

So the switch removes the developer install first (README, "Switching from the developer
install"):

1. `scripts/dev-uninstall.sh`, as the user, stops the daemon and removes exactly the files
   `scripts/dev-install.sh` installs, then reloads the user's systemd and D-Bus. It leaves the
   settings, the tree store, the refresh token and the sync folder alone, so the packaged daemon
   starts where this one stopped. It points "Start at login" (limitations log A4) at
   `/usr/bin/konedrive`, and points out per-user Dolphin plugins without removing them.
2. `sudo scripts/install-helper.sh --uninstall --force` removes the old helper and its `/etc` unit.
   `--force` because a folder is registered: its record in `/var/lib/konedrive/roots.json` stays,
   and the packaged helper, which keeps its state in the same place, marks that folder again when
   it starts. Until then, the folder is not intercepted.
3. `sudo dnf install` of both RPMs.

The package's `%post` warns when `/etc/systemd/system/konedrive-helper.service` exists. It cannot
see the per-user files.

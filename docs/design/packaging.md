# Packaging

KOneDrive installs on Fedora as two RPM packages, built from one spec
(`packaging/rpm/konedrive.spec`) by `scripts/build-rpm.sh`: on the developer's machine, or, for a
release, by the release workflow, which publishes them on GitHub Releases (`docs/releasing.md`).
There is no package repository. "Install" and "Upgrading and removing" in `docs/user-guide.md`
and "Switching from the developer install to the packages" in `docs/developing.md` are the user's
side of this page.

## 1. The two packages

| Package | Contents | Relations |
|---|---|---|
| `konedrive` | `/usr/bin/konedrived`, `/usr/bin/konedrivectl`, `/usr/bin/konedrive` (the window); the user unit `/usr/lib/systemd/user/konedrived.service`; the D-Bus activation file `/usr/share/dbus-1/services/org.konedrive.Daemon.service`; the launcher entry `/usr/share/applications/org.konedrive.KOneDrive.desktop`; the notification events `/usr/share/knotifications6/konedrive.notifyrc`; README, SECURITY.md, LICENSE and `docs/` | `Requires: fuse3`, since every folder is mounted with `fusermount3`; `Requires: kf6-kio-core >= 6.21` (K34); `Requires:` the Kirigami, Kirigami Addons, KItemModels and Qt Quick QML modules and the desktop style of Qt Quick Controls (`kf6-qqc2-desktop-style`), which no library dependency pulls in; `Recommends: konedrive-kde` of the same version and release |
| `konedrive-kde` | the Dolphin plugins, `/usr/lib64/qt6/plugins/kf6/overlayicon/konedriveoverlay.so` and `/usr/lib64/qt6/plugins/kf6/kfileitemaction/konedriveactions.so`; LICENSE | `Requires: dolphin`; not `konedrive` |

Nothing is installed as a system service and nothing runs as root: the daemon is a user service,
and `fusermount3`, the setuid program of `fuse3`, is the only privileged program involved.

There are no icons of our own: the launcher entry, the autostart entry and the tray menu use the
theme's `folder-cloud`, and the tray shows the theme's icon for each state.

**Why the Dolphin plugins are separate.** They are the one part that belongs to another program.
They load into Dolphin, so their package requires Dolphin, and they need nothing from `konedrive`
to load: the emblems come from the attributes of the files, and the menu hides its section when the
daemon is not there ([desktop.md](desktop.md) §11). `konedrive` recommends them, so `dnf` installs
them by default, and `sudo dnf remove konedrive-kde` takes them away alone.

## 2. What the build does

`scripts/build-rpm.sh` runs as the user and builds under `target/rpm/` (rpmbuild's `_topdir`), which
it empties first, so that the output directories hold this build alone. Only `cargo vendor` reaches
outside it: it fetches the crates from the network, through the user's cargo home.

1. **Source0** is `git archive` of `HEAD`: the committed tree, never the working tree.
2. **Source1** holds the crates of that tree's `Cargo.lock` (`cargo vendor --locked`, into
   `crates-io/`, since the tree has a `vendor/` of its own for `vendor/fuser`) and the
   `.cargo/config.toml` that `cargo vendor` prints, which points cargo at them. The spec unpacks it
   into the source tree.
3. `rpmbuild -ba` builds the binary packages and the source RPM. A local build keeps Cargo's output
   in `target/rpm-cargo/`, outside the emptied directory, so that only what changed is compiled
   again; a release's build compiles everything anew.

**The version** is the one in `Cargo.toml` (`[workspace.package]`; `scripts/version.sh`,
`docs/releasing.md`). A release's `X.Y.Z` is given with `--version`, and must be the file's. Any
other build takes the file's version with a suffix that sorts below it and grows with the history
(`0.1.2~dev.57`). The script writes it into the copies of `Cargo.toml`, `Cargo.lock` and the spec
that Source0 holds — the spec's `Version:`, the `commit` and `build_version` globals that `%build`
hands to Cargo and CMake, and a `%changelog` entry — and stops if a line it rewrites is not there;
`cargo vendor --locked` stops it if the rewritten lock file does not fit. The spec's `Version:` in
git is `0.0.0`, which every build writes over.

**Inside the spec:**

- **Rust.** `cargo build --release --offline --locked -p konedrived -p konedrivectl`, with Fedora's
  `RUSTFLAGS`, and cargo's home and output inside the build directory. The `fault-injection`
  feature is the VM suite's alone and is never packaged. `--dev-tools` (`rpmbuild --with
  dev_tools`), never together with `--version`, builds a local development package whose daemon
  serves the token export ([desktop.md](desktop.md) §2.11); the build checks the binary both ways,
  and stops if a release build's `konedrived` holds `org.konedrive.TokenExport` or a `--dev-tools`
  build's does not.
- **C++.** `app/` and `dolphin/` are configured with Fedora's `%cmake_kf6` (installed under `/usr`,
  KDE's system directories, tests off), each in its own build directory, and installed with their
  own CMake install rules. The window is given the version and the commit it shows.
- **The unit and the activation file** come from `packaging/`, where they serve the developer
  install: the unit runs `%h/.local/bin/konedrived`, and the D-Bus file's `Exec=` is `/bin/false`,
  leaving the start to systemd (`SystemdService=konedrived.service`). `%install` rewrites
  `ExecStart=`, `ExecStopPost=` and `Exec=` to `/usr/bin/konedrived`, and stops if a line it expects
  is not there, so the two installs keep one set of files and cannot drift apart silently.
- **`%check`** runs `desktop-file-validate` on the launcher entry. The test suites do not run in the
  package build: they run in development (`CONTRIBUTING.md`), and the VM suite needs a VM.
- **No debuginfo packages** (`%global debug_package %{nil}`): the RPMs are installed with a glob over
  the output directory, which the debuginfo and debugsource packages would match too, and the
  debugsource one would carry every vendored crate.

Releases are built the same way by `.github/workflows/release.yml`, in a `fedora:44` and a
`fedora:45` container; each pair is then installed in a clean container of the same Fedora, where
every program must find its libraries and `konedrived --version` and `konedrivectl --version` must
run, before the tag and the release are made.

## 3. The daemon's unit

`konedrived.service` is a user unit of `Type=dbus` with `BusName=org.konedrive.Daemon`:

- **`Restart=on-failure`**, `RestartSec=5`: the daemon exits with a failure when a task it needs is
  gone, and each start brings every folder up again, so a failure that comes back is tried every few
  seconds, not in a tight loop.
- **`ExecStopPost=konedrived --tidy`**, after the daemon's end however it ended: the tidy step takes
  off a mount the daemon left on a folder and leaves the folder a plain directory without its write
  bits that shows the downloaded files, and tells KDE's programs to read it again
  ([mount.md](mount.md) §8; [desktop.md](desktop.md) §10).
- **`LimitNOFILE=16384`**, well clear of anything the daemon needs.
- `WantedBy=default.target`; the package enables nothing: D-Bus starts the daemon on demand.

## 4. Install, upgrade and removal

The scriptlets are the systemd user-unit macros alone.

| When | `konedrived.service`, per user |
|---|---|
| First install | `%systemd_user_post`, which starts nothing: D-Bus starts the daemon on the first call of the window or `konedrivectl` |
| Upgrade | restarted when the transaction ends, for every logged-in user whose daemon runs (`%systemd_user_postun_with_restart`) |
| Removal | stopped and disabled for every logged-in user (`%systemd_user_preun`) |

**What an upgrade costs.** The daemon is restarted so that the new binary runs. While it restarts
the folder is not served (F296): the stopping daemon takes its mount off, and until the new one has
mounted the folder again it is the backing directory, which shows only what is on disk and takes
nothing. A program that held a file of the folder open gets an error from it. The new daemon mounts
the folder again and tells KDE's programs to list it again. A running window restarts itself as the
installed program once the daemon is back ([desktop.md](desktop.md) §4, "After an update"). `dnf`
asks once for the whole transaction, so the package cannot warn first: the user guide says to close
programs that have files of the folder open before upgrading.

**What removal leaves.** Each user's `config.toml`, stores, KWallet entries and folders stay. The
daemon is stopped, so each folder is left the backing directory without its write bits, showing the
downloaded files, and `konedrivectl`, which could forget it and give the write bits back, goes with
the package. So `konedrivectl --account <account> sync forget` comes first, for every account's
folder; the package cannot refuse (R3).

## 5. The developer install and the packages

`scripts/dev-install.sh` builds with the `dev-tools` feature and installs for the user alone. The
two installs must not be used together: each file of the developer install takes precedence over
the package's.

| Developer install | Takes precedence over |
|---|---|
| `~/.config/systemd/user/konedrived.service`, running `~/.local/bin/konedrived` | `/usr/lib/systemd/user/konedrived.service` |
| `~/.local/share/dbus-1/services/org.konedrive.Daemon.service` | `/usr/share/dbus-1/services/org.konedrive.Daemon.service` |
| `~/.local/bin/konedrive*`, ahead of `/usr/bin` on the usual `PATH` | `/usr/bin/konedrive*` |
| `~/.local/share/applications/org.konedrive.KOneDrive.desktop`, `~/.local/share/knotifications6/konedrive.notifyrc` | the package's in `/usr/share` |
| Dolphin plugins installed for the user by hand, with `QT_PLUGIN_PATH` | the `konedrive-kde` plugins |

So the switch removes the developer install first. `scripts/dev-uninstall.sh`, as the user,
disables the user unit, removes exactly the files `scripts/dev-install.sh` installs, stops the
daemon, and reloads the user's systemd and D-Bus. It leaves the settings, the stores, the refresh
tokens and the folders alone, so the packaged daemon starts where this one stopped. It points the
"Start at login" entry at `/usr/bin/konedrive`, points out per-user Dolphin plugins without removing
them, and says to restart a window that still runs the removed program. Then `sudo dnf install` of
both RPMs. Nothing of the developer install needs root to remove.

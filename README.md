# KOneDrive

A OneDrive client for KDE Plasma. This first part signs in to a personal Microsoft account,
keeps the session in KWallet and shows the account in a Kirigami window. File syncing comes
in later parts (see `onedrive-linux-design.md` and `docs/superpowers/specs/`).

## Build dependencies (Fedora)

```
sudo dnf install rust cargo cmake extra-cmake-modules gcc-c++ qt6-qtbase-devel \
  qt6-qtdeclarative-devel kf6-kirigami-devel kf6-kirigami-addons-devel kf6-ki18n-devel \
  kf6-kcoreaddons-devel dbus-daemon desktop-file-utils
```

## Registering the application (once)

Microsoft only lets programs sign in with an application (client) ID. Each user registers
their own; it is free.

1. Sign in to <https://entra.microsoft.com> with an account that has a directory. An existing
   Azure account works. Otherwise create a free Azure account at <https://azure.microsoft.com/free>;
   it asks for a bank card and a phone number to verify your identity and charges nothing unless
   you upgrade.
2. Open **App registrations → New registration**.
   - Name: `KOneDrive`
   - Supported account types: **Personal Microsoft accounts only**
   - Redirect URI: platform **Public client/native (mobile & desktop)**, value `http://localhost`
3. Copy the **Application (client) ID**.

No API permissions need to be configured; KOneDrive asks for them when you sign in. The consent
screen will call the app "unverified" — expected for a personal registration. The account that
registers the app and the OneDrive account you sign in with can be different.

## Install for your user

```
scripts/dev-install.sh
```

This installs `konedrived`, `konedrivectl` and `konedrive` into `~/.local/bin`, the systemd
user unit, the D-Bus activation file and the launcher entry. The daemon starts on demand.

## Use

- Open **KOneDrive** from the launcher, enter the client ID under **Advanced**, press **Sign In to OneDrive**.
- Or from a terminal:

  ```
  konedrivectl set-client-id 00000000-0000-0000-0000-000000000000
  konedrivectl login
  konedrivectl status
  konedrivectl logout
  ```

## Trying the folder without OneDrive

The sync folder can be driven entirely from the command line, with a local
directory standing in for the cloud — no Microsoft sign-in needed. Read this
paragraph before the first command below, not after: there are two ways to
bind the folder, and they cost different things.

- `konedrivectl sync register` needs the privileged helper connected, so that
  opening a placeholder is transparently intercepted and filled. A standing
  project ruling keeps that helper inside a VM and off your own machine, so
  on an ordinary checkout this command fails immediately (`NotSignedIn`, or
  `NoHelper` once you are signed in) — it is not part of this walkthrough.
  A folder bound this way is also forgotten through the helper: without it,
  `konedrivectl sync forget` is refused and the folder stays registered.
- `konedrivectl sync register-without-interception` needs neither a helper
  nor a sign-in. This is the one below, and it has a real cost that the name
  is meant to make obvious: **nothing** fills a placeholder when something
  opens it. A file in this folder reads as zeros — not an error, not a
  missing file, silently the wrong bytes — until you fetch it yourself with
  `konedrivectl sync hydrate`. `konedrivectl sync status` keeps repeating
  that warning for as long as the folder stays registered this way, not just
  once at registration. On a machine where a helper *is* running, freeing a
  file up here still asks it to clear the file's mark first, and is refused
  (`NoHelper`) while the daemon is not connected to it; with no helper at all,
  as below, nothing is asked.

```
mkdir -p ~/OneDrive-test ~/fake-cloud/sub
head -c 1M </dev/urandom > ~/fake-cloud/big.bin
echo hello > ~/fake-cloud/sub/note.txt

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
```

`konedrivectl sync status` always says something useful, including with no
helper installed at all: `State: none` and `Folder: (none)` before anything
is registered; `no-interception` for the mode above, with an `Opens:` line
saying in plain words that a file that is not downloaded reads as zeros until
you hydrate it — every time, success or not; `ready` once a folder is bound
with the real, VM-only helper intercepting it (`Opens: intercepted`); or `error`
with a `Last error:` line explaining what needs attention (for example, if
startup recovery could not finish, or a folder bound with the helper is
waiting for it after a restart). `error` is never printed to look like a
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

```
cargo test --workspace
cmake -S app -B build/app -DBUILD_TESTING=ON && cmake --build build/app && ctest --test-dir build/app --output-on-failure
```

The Secret Service test touches your real KWallet (under a test-only attribute) and is opt-in:

```
cargo test -p konedrived secret_service_round_trip -- --ignored
```

## Troubleshooting

- Daemon log: `journalctl --user -u konedrived -f`; more detail with
  `systemctl --user edit konedrived` → `Environment=RUST_LOG=konedrived=debug`.
- Files: `~/.config/konedrive/config.toml` (client ID), `~/.local/state/konedrive/account.json`
  (cached name and quota). The refresh token is in KWallet under "KOneDrive refresh token".

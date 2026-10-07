# Using KOneDrive

How to install KOneDrive, connect your OneDrive and use it, in the window and from the command
line. What it is, in short: [the README](../README.md). How it works inside:
[design/](design/README.md).

## Install

On Fedora 44 (x86_64), KOneDrive installs as two packages (there is no package repository yet):

- `konedrive` — the daemon, `konedrivectl`, the KOneDrive window, and the helper with its system
  service, which is enabled and started when the package is installed;
- `konedrive-kde` — the Dolphin plugins (see "Dolphin integration"). `konedrive` recommends it,
  so `dnf` installs it too; `sudo dnf remove konedrive-kde` removes it alone.

**From a release.** Every release is on the Releases page of the repository on GitHub
(`kartas39/konedrive`). Download the two binary RPMs of the latest one,
`konedrive-X.Y.Z-1.fc44.x86_64.rpm` and `konedrive-kde-X.Y.Z-1.fc44.x86_64.rpm` — not the
`.src.rpm`, which the command below would pick up too. Its `SHA256SUMS` lists their checksums
(`sha256sum -c --ignore-missing SHA256SUMS`). Then, in the directory they are in:

```
sudo dnf install ./konedrive-*.rpm
```

The packages are not signed (`docs/limitations/`, R6); `dnf` installs a local
file without checking a signature.

To build the packages yourself, or if a developer install is on this machine, see
[developing.md](developing.md). After the install, open **KOneDrive** from the launcher. The daemon connects
to the helper within half a minute; `konedrivectl sync status` then says `Helper: connected`.

- **Upgrading** is the same `dnf install` with the newer RPMs. It restarts the helper when it
  finishes: a program waiting for a file to download at that moment gets an error from its open,
  and a file that is not downloaded reads as zeros if it is opened in the moment no helper runs
  (`docs/limitations/`, Z1 and R1): close programs that are opening files in
  the sync folder first. `dnf` treats a rebuild with the same version and release as the package
  already installed; install such a rebuild with `sudo dnf reinstall` and the same paths.
- **Upgrading from a single-account version.** The first start of the new daemon turns your setup
  into one account named "Personal", with its folder, sign-in and history
  ([`docs/design/accounts.md`](design/accounts.md) §8). A KOneDrive window or a Dolphin that
  was running across the upgrade has to be restarted (F46). There is no way back to the older
  version except restoring `~/.config/konedrive/config.toml.v1` by hand (F41).
- **Removing:** forget every account's folder first — **Forget Folder** on each account's
  **Account** page, or `konedrivectl --account <account> sync forget` for each account — then
  `sudo dnf remove konedrive konedrive-kde`. Removing the package stops the helper, and a folder
  still registered then reads as zeros where its files are not downloaded (R3).

What goes where, and why: [`docs/design/packaging.md`](design/packaging.md).

## Using your OneDrive

- **Add an account and sign in.** Open **KOneDrive** from the launcher (or `konedrive` from a
  terminal) and choose **Sign in…** — on the Status page while there is no account yet, or in the
  account switcher's menu at the top of the sidebar. It opens the Microsoft sign-in in your
  browser straight away, with the account picker; there is nothing to register and nothing to
  enter first. The account is created only once the sign-in succeeds, named after its email, and
  the folder picker then opens for it: choose an empty folder, and your OneDrive appears in it. A
  cancelled or failed sign-in leaves nothing behind. Or from a terminal — `account add` opens the
  same browser sign-in, and says what the new account is called:

  ```
  konedrivectl account add
  konedrivectl sync register ~/OneDrive
  konedrivectl status
  ```

  `konedrivectl logout` signs the account out again and deletes its stored token;
  `konedrivectl login` signs it in again.

  For a sign-in over SSH or without a desktop, run `KONEDRIVE_NO_BROWSER=1 konedrivectl account
  add` (or `login`): no browser is opened, and the address is printed to open elsewhere.

  konedrive signs in with its own application registration, so this needs no setup. Anyone who
  wants to sign in with their own Microsoft Entra registration instead can set its client ID with
  `konedrivectl set-client-id <id>`.

- **Several accounts.** Each Microsoft account you add gets its own folder. **Sign in…** is in
  the menu at the top of the sidebar, which also switches between the accounts. Two accounts'
  folders cannot be inside one another, and a Microsoft account can be connected once: signing
  an account in as a different Microsoft account than its own is refused — add that one as a new
  account. Only personal Microsoft accounts are supported, not work or school ones. **Rename…**
  and **Remove Account…** are on the **Account** page. Removing an account signs it out and
  forgets it on this computer: its folder's files stay where they are (files that were never
  downloaded stay as empty placeholders), and nothing in OneDrive is deleted. Every account is
  read-only until you turn uploading on for it (see "Uploading changes"). How it
  works:
  [`docs/design/accounts.md`](design/accounts.md).

  From a terminal, `konedrivectl account list` shows every account, and each command that acts on
  one account takes it with `--account <id, label or email>`, or from the environment variable
  `KONEDRIVE_ACCOUNT`. With a single account neither is needed; with several and none named, the
  command stops and lists them. `status` and `sync status` show every account when none is named.
  Commands that take a path, such as `sync hydrate <path>`, find the account from the path.

  ```
  konedrivectl account add                       # signs in; the new account is named by its email
  konedrivectl account rename bob@outlook.com Family
  konedrivectl --account Family sync register ~/OneDrive-Family
  konedrivectl account remove Family             # asks nothing; says what it deleted and kept
  ```

- **The window.** The sidebar starts with the account switcher: the account shown, a menu of
  every account and **Sign in…**; a warning sign on it means that another account needs your
  attention. Below it, five pages show that account: **Status** (the folder, its item count,
  "Free Up Space…", "Refresh Now", "Open in File Manager"), **Activity** (downloads under way
  now, and the most recent of what the daemon keeps), **Conflicts** (local edits rescued out of
  the way, with a count badge), **Not in the Folder** (what OneDrive has that was skipped, and
  why) and **Account** (the account's name with **Rename…**; sign in or out, and the quota; the
  folder, with **Choose Folder…** and **Forget Folder**; and **Remove Account…**). **Settings**
  is the whole app's: "Start at login", "Show download progress" (a download that takes more
  than 2 s shows in Plasma's notifications), "Show in Places", and "Show a tray icon for each
  account". While the helper is not
  connected, a card on the Status page says so, with the same instruction as the `Helper:` line
  of `konedrivectl sync status` (below). A tray icon
  shows an account's state — needs attention, signed out, paused, syncing, synced. With several
  accounts each has its own icon, named in its tooltip, with a menu for that account; with the
  setting off, one icon shows the worst state across your accounts, with a line per account in its
  tooltip (an account that pauses by itself on a metered
  connection or on battery shows as paused, and the one icon's **Sync Anyway** lifts that for every
  account, as `konedrivectl sync anyway --all` does; an account's own icon lifts its own), and keeps KOneDrive running in the background so
  notifications still reach you with the window closed; with more than one account,
  notifications and download progress name the account. "Start at login" is on by default after
  the first run. Each account's folder also gets an entry named `OneDrive — <name>` in Dolphin's
  Places panel and in file dialogs ("Show in Places" in Settings, on by default).
  The foot of the sidebar names the build, `Version 0.1.1-dev.57 · commit 5254595` (selectable,
  for a bug report), and adds a line when the running service is another build — installed but
  not restarted; `konedrivectl --version` prints the same for itself and the daemon. After a newer
  package is installed, the window restarts by itself once the service is back.

- **The helper.** A small privileged service that makes a placeholder download the moment a
  program opens it, instead of that program reading zeros. The `konedrive` package installs and
  starts it; with the developer install, install it with `sudo scripts/install-helper.sh`
  ([developing.md](developing.md); see also SECURITY.md for what runs as root and why). One helper
  serves every account, and your OneDrive folders need it: a folder is kept in step with OneDrive
  only while the helper is connected. `konedrivectl sync status` has a `Helper:` line:
  - `connected` — files download when opened;
  - `not-installed` — no konedrive-helper service on this system: install it
    (`sudo scripts/install-helper.sh`);
  - `stopped` — installed, not running: `sudo systemctl start konedrive-helper`;
  - `failed` — the service failed: `systemctl status konedrive-helper` says why;
  - `unknown` — systemd cannot be asked, or the helper is running but the
    daemon has no link to it yet (the first few seconds after it starts).

- **Registering a folder.** **Choose Folder…** on the account's **Account** page, or
  `konedrivectl sync register <path>` (with `--account` when there are several accounts), with
  the helper connected. The folder
  must be empty, and must not be inside another account's folder or contain one. Right after the
  helper is installed or started, the daemon takes up to half a minute to connect to it, and
  `register` is refused (`NoHelper`) until then: wait for `Helper: connected`.

- **From the command line** — each of these acts on the chosen account, except `sync status` with
  none chosen, which shows every account's folder, and `sync hydrate`, whose path decides:
  - `konedrivectl sync status` — the folder, its phase and item count, the helper, and how
    changes made in OneDrive arrive: `live` (within seconds, through OneDrive's change
    notifications) or `every minute (connecting)` while those cannot be reached.
  - `konedrivectl sync activity [--limit N]` — what happened lately: downloads, free-ups,
    changes from OneDrive, conflicts, failures.
  - `konedrivectl sync transfers` — downloads and uploads under way right now, and for each way
    how much is left, about how long it takes, and how much is done.
  - `konedrivectl sync conflicts` — local edits rescued out of the way; `konedrivectl sync
    dismiss <path>` takes one off the list (the file itself stays where it was moved to).
  - `konedrivectl sync free-up-space` — send every downloaded file that is not in use back to
    online-only.
  - `konedrivectl sync open <path> [--print]` — open the page of a file or folder in OneDrive's
    web interface, where it can be shared and its versions seen; the account's folder itself
    opens the drive. The address is always printed; `--print` only prints it. The path decides
    the account.
  - `konedrivectl sync skipped` — what OneDrive has that did not make it into the folder, and
    why (the Personal Vault, a shared folder, a OneNote notebook, a name too long for Linux).
  - `konedrivectl sync refresh` — ask OneDrive for changes now. Changes made in OneDrive usually
    arrive by themselves within seconds; the poll behind them runs every 5 minutes while they do,
    and every minute while they cannot. The window's status line then ends in "· live".
  - `konedrivectl sync hydrate <path>` — download one file now.
  - For an account that uploads (`konedrivectl account mode read-write`):
    - `konedrivectl sync outbox [--all]` — how much is left to upload, then the changes waiting
      to go up, and why each waits;
    - `konedrivectl sync pause [--for 2h]` and `konedrivectl sync resume` — nothing is uploaded
      and OneDrive is not asked for changes meanwhile; opening a file still downloads it;
    - `konedrivectl sync ignore [list | add <pattern> | remove <pattern>]` — names of local files
      that are never uploaded (editors' swap and temporary files by default);
    - `konedrivectl sync not-uploaded` — what stays on this computer, and why;
    - `konedrivectl sync deletes confirm | restore` — decide on a large delete held back for
      confirmation.

- **Read-only until you turn uploading on.** An account only reads from OneDrive unless it uploads (below): files
  are `r--r--r--`, directories `r-xr-xr-x`, so nothing here can diverge from the cloud on its own.
  A local edit forced past that lock is rescued, not lost — moved aside and listed under
  **Conflicts** rather than overwritten.

- **Forget.** **Forget Folder** on the **Account** page, or `konedrivectl sync forget`, unbinds
  the account's folder and takes the read-only lock off it; the files themselves are left exactly
  as they are.

## Uploading changes

**Off until you turn it on.** KOneDrive can send what you change in the folder back to OneDrive.
Every account starts read-only, and uploading is your choice, account by account. Once it is on
for an account, what you change, move or delete in its folder is changed, moved or deleted in your
OneDrive: nothing but that switch stands in between.

It works like this:

- **Turn it on per account**: **Upload changes made on this computer** on the **Account** page, or
  `konedrivectl account mode read-write`. It signs in again, in the browser, for permission to
  change your files; then the folder's read-only lock comes off.
- **What goes up**: new files and folders, edits, renames, moves and deletes, a couple of seconds
  after the last change, once no program has the file open for writing. The **Status** page counts
  the changes waiting; **Activity** lists them; Dolphin shows an emblem on each.
- **Changed on both sides**: both are kept. OneDrive's version keeps the name, and yours goes up
  beside it as `Report-<computer name>.docx`, listed under **Conflicts**.
- **What stays on this computer**, listed under **Not Uploaded**: names OneDrive refuses (rename
  them to upload them), links, and anything on another filesystem inside the folder. Editors'
  temporary files stay too, unlisted (`konedrivectl sync ignore` edits that list).
- **Moving a file out of the folder** deletes it in OneDrive, but only after it is downloaded where
  you moved it. Deleting to the Trash just deletes it; OneDrive keeps it in its recycle bin.
- **A large delete** is held until you choose **Delete in OneDrive Too** or **Restore Them**.
- **Pause Syncing…** on the **Status** page or in the tray, or `konedrivectl sync pause`.
- **Turning it off** (`konedrivectl account mode read-only`) asks first if changes are still
  waiting; the files stay as they are, and the folder is locked again.

How it works: [`docs/design/writes.md`](design/writes.md).

## Dolphin

`dolphin/` holds two Dolphin plugins. Files in the sync folder get an emblem — a
cloud when online-only, sync arrows while downloading or freeing up, a check
mark when downloaded, a filled check when kept on this device — and their context
menu offers, under the heading **OneDrive**, **Always keep on this device** and
**Free up space**, for files and folders, and **Open in OneDrive** for one file
or folder — or the account's folder itself — which opens its page in OneDrive's
web interface in the browser. Emblems come from each file's
`user.konedrive.state` and work with the daemon stopped; the menu actions ask
the daemon, and say plainly when it is not running. Neither plugin ever
opens a file in the sync folder. Dolphin itself still opens some, and that
downloads them. For images and videos, KOneDrive fills the previews itself from
OneDrive's own thumbnails, up to the x-large size (512 px), and Dolphin draws
those without opening the file. They are filled in the background, two a
second (limitations log K16). What still opens a file: an image or video shown
before its preview is filled, a preview of any other kind of file, a preview
at the largest zoom (xx-large; K15), and telling the type of a file whose name
has no known extension (K1). To avoid those downloads, turn previews off in
that folder (View → Show Previews).

The plugins come with the `konedrive-kde` package. The menu actions can be switched off in Dolphin
under Configure Dolphin → Context Menu ("KOneDrive: Always Keep on This Device, Free Up Space and
Open in OneDrive").

## Troubleshooting

- Daemon log: `journalctl --user -u konedrived -f`; more detail with
  `systemctl --user edit konedrived` →
  `Environment=RUST_LOG=konedrived=debug,konedrive_graph=debug,konedrive_tree=debug`.
- Files: `~/.config/konedrive/config.toml` (the client ID, and each account with its name and
  folder; `config.toml.v1` is the single-account file it was migrated from, if any). Each
  account's state is in `~/.local/state/konedrive/accounts/<id>/`: `account.json` (cached name
  and quota) and `tree.sqlite` (the map of the drive, the activity log and the conflicts).
  Changed files moved out of the way are in `~/.local/share/konedrive/rescued/<id>/`. Each
  account's refresh token is in KWallet, under `KOneDrive: <email>`, and never leaves it (see
  SECURITY.md). Where everything else lives:
  [`docs/design/accounts.md`](design/accounts.md) §4.2.

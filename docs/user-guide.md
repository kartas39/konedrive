# Using KOneDrive

How to install KOneDrive, connect your OneDrive and use it — in the window, the tray and Dolphin.
Everything here can also be done from a terminal: [command-line.md](command-line.md). What
KOneDrive is, in short: [the README](../README.md). How it works inside:
[design/](design/README.md).

## Install

KOneDrive is two packages, for Fedora 44 and 45 (x86_64):

- `konedrive` — the service, the window, `konedrivectl` and the helper;
- `konedrive-kde` — the Dolphin plugins. `konedrive` recommends it, so it is installed too.

Download both for your Fedora from the Releases page of the repository on GitHub
(`kartas39/konedrive`): the files with `.fc45` in the name for Fedora 45, `.fc44` for Fedora 44 —
not the other Fedora's, and not the `.src.rpm`. Then, in the directory they are in:

```
sudo dnf install ./konedrive-*.rpm
```

There is no package repository yet, and the packages are not signed; `SHA256SUMS` on the same page
lists their checksums (`sha256sum -c --ignore-missing SHA256SUMS`).

## First steps

1. Open **KOneDrive** from the launcher.
2. Choose **Sign in…** on the Status page. Your browser opens Microsoft's sign-in, with its account
   picker; there is nothing to register or type in first.
3. When the sign-in is done, a folder picker opens. Choose an **empty** folder.

Your OneDrive appears in that folder: every file with its real name and size, taking no space until
you open it. Opening a file downloads it, and the program that opened it just reads it.

A cancelled or failed sign-in leaves nothing behind. Only personal Microsoft accounts work; work and
school accounts are not supported yet.

## The window

The sidebar starts with the **account switcher**: the account shown, and a menu with every account
and **Sign in…**. Below it are the pages of that account.

- **Status** — the folder and what it is doing now: up to date, downloading, uploading, paused, or
  what needs your attention. Buttons: **Refresh Now**, **Pause Syncing…**, **Free Up Space…**,
  **Open in File Manager**.
- **Activity** — the downloads and uploads under way, and what happened lately.
- **Conflicts** — your versions of files that changed on both sides, kept so that nothing is lost,
  with **Show in Folder** and **Dismiss**.
- **Not in the Folder** — what OneDrive has that could not be placed here, and why: the Personal
  Vault, a OneNote notebook, a name Linux cannot hold.
- **Not Uploaded** — what stays on this computer, and why (see "Uploading changes").
- **Account** — the account's name (**Rename…**), signing in and out, the quota, the folder
  (**Choose Folder…**, **Forget Folder**), uploading, thumbnails, and **Remove Account…**.

**Settings** is for the whole app:

- **Start at login** — on after the first run.
- **Show download and upload progress** — a long transfer shows in Plasma's notifications.
- **Show in Places** — each account's folder is in Dolphin's Places panel and in file dialogs.
- **Show a tray icon for each account**.
- **Pause on metered connections** and **On battery** — when every account pauses by itself.
- **Quit KOneDrive**.

Closing the window does not stop anything: KOneDrive stays in the tray, and the service that syncs
runs without the window at all. The foot of the sidebar names the version, for a bug report.

## The tray

The tray icon shows an account's state: synced, syncing, paused, signed out, or needing your
attention. With several accounts each has its own icon, named in its tooltip; with that setting
off, one icon shows the most serious state of them all.

A click shows the window on that account, or hides it. The menu opens the folder or the window,
refreshes, pauses for 2, 8 or 24 hours or until resumed, resumes, and quits.

## Dolphin

In the OneDrive folder every file and folder has an emblem:

- a cloud — only in OneDrive, takes no space;
- sync arrows — downloading or freeing up;
- a check mark — downloaded;
- a filled check — always kept on this device.

The right-click menu has, under **OneDrive**:

- **Always keep on this device** — downloads it and keeps it, for a file or a whole folder;
- **Free up space** — makes it online-only again;
- **Open in OneDrive** — opens its page in OneDrive's web interface, where it can be shared and its
  versions seen. With two personal accounts this opens under the account your browser is signed in
  to (limitations log, F295).

Previews of images and videos come from OneDrive's own thumbnails, so showing them downloads
nothing. A preview of another kind of file, or one at the largest zoom, does download the file; to
avoid that, turn previews off in that folder (View → Show Previews).

## Freeing up space

- One file or folder: **Free up space** in Dolphin's menu.
- Everything that is not in use: **Free Up Space…** on the Status page.

A file you changed and that has not been uploaded yet is never freed.

## Pausing

**Pause Syncing…** on the Status page or in the tray menu: for 2, 8 or 24 hours, or until you
resume. Nothing is uploaded and OneDrive is not asked for changes meanwhile; opening a file still
downloads it. The pause stays across a restart.

An account can also pause by itself on a metered connection or on battery (Settings). **Sync
Anyway** on the Status page or in the tray lifts that, until the connection or the power changes
again.

## Several accounts

**Sign in…** in the account switcher's menu adds another Microsoft account, with a folder of its
own. Two accounts' folders cannot be inside one another, and one Microsoft account can be connected
once.

**Remove Account…** on the Account page signs the account out and forgets it on this computer. Its
files stay where they are — those never downloaded stay as empty placeholders — and nothing in
OneDrive is deleted.

## Uploading changes

**Off until you turn it on.** Every account starts read-only: its files cannot be changed here, so
nothing can drift from OneDrive by accident.

Turn it on with **Upload changes made on this computer** on the Account page. The browser opens
once more, for permission to change your files. From then on, what you add, change, move or delete
in the folder is added, changed, moved or deleted in your OneDrive.

- **What goes up**: new files and folders, edits, renames, moves and deletes, a couple of seconds
  after the last change, once no program has the file open for writing. The Status page counts
  what is waiting; Dolphin shows an emblem on each.
- **Changed on both sides**: both are kept. OneDrive's version keeps the name, and yours goes up
  beside it as `Report-<computer name>.docx`, listed under **Conflicts**.
- **What stays on this computer**, listed under **Not Uploaded**: names OneDrive refuses (rename
  them to upload them), links, and anything on another filesystem inside the folder. Editors'
  temporary files stay too; the list of such names is on the Account page ("Never uploaded").
- **Moving a file out of the folder** deletes it in OneDrive, but only after it is downloaded where
  you moved it. Deleting to the Trash deletes it; OneDrive keeps it in its recycle bin.
- **A large delete** is held until you choose **Delete in OneDrive Too** or **Restore Them**.
- **Turning it off** asks first if changes are still waiting; the files stay as they are, and the
  folder is read-only again.

## Changing or forgetting the folder

**Forget Folder** on the Account page stops keeping the folder in step and takes the read-only lock
off it; the files are left exactly as they are. **Choose Folder…** then binds the account to
another empty folder.

If you move or rename the OneDrive folder itself, KOneDrive stops syncing that account and says
so. Move it back, or forget the folder and choose it again.

## The helper

A small service that runs as root and makes a file download the moment a program opens it. The
package installs and starts it, and one helper serves every account. It has no network access and
no credentials ([SECURITY.md](../SECURITY.md)).

Right after an install the service takes up to half a minute to reach it. While it is not
connected, a card on the Status page says so and what to do, and a folder cannot be chosen yet.

## Upgrading and removing

- **Upgrading** is the same `dnf install` with the newer packages. The service and the window
  restart by themselves. The helper restarts too: a program waiting for a file to download at that
  moment gets an error, so close programs that are opening files in the folder first (limitations
  log, Z1).
- **Removing**: first **Forget Folder** for every account, then
  `sudo dnf remove konedrive konedrive-kde`. A folder still registered when the package goes reads
  as zeros where its files are not downloaded (limitations log, R3).

## Troubleshooting

- The service's log: `journalctl --user -u konedrived -f`; more detail with
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

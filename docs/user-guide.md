# Using KOneDrive

How to install KOneDrive, connect your OneDrive and use it — in the window, the tray and Dolphin.
Everything here can also be done from a terminal: [command-line.md](command-line.md). What
KOneDrive is, in short: [the README](../README.md). What does not work and cannot simply be
fixed: [limitations/](limitations/README.md).

## Install

KOneDrive is two packages, for Fedora 44 and 45 (x86_64):

- `konedrive` — the service, the window and `konedrivectl`;
- `konedrive-kde` — the Dolphin plugins. `konedrive` recommends it, so it is installed too.

It needs KDE Plasma 6 with KDE Frameworks 6.21 or later, and FUSE 3 (`fuse3`, which the package
asks for).

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
it is read.

A cancelled or failed sign-in leaves nothing behind. Only personal Microsoft accounts work; work and
school accounts are not supported yet.

## The sync folder

The sync folder is served by the KOneDrive service while it runs. What that means:

- **Every file is there from the start**, with its name, size and time, and takes no space on
  this computer.
- **Reading a file fetches it.** A program that reads a part of a file gets that part from
  OneDrive. Once more of it is read, the file is downloaded whole and stays on this computer; the
  next read asks OneDrive nothing.
- **A downloaded file stays** until you free up its space (see "Freeing up space").
- **While the service is not running** — it was stopped, or it is being upgraded — the folder
  shows only what is on this computer: the downloaded files and files of your
  own that were not sent yet. It cannot be changed then. Everything is back when the service
  runs again.
- **Only you reach it.** Another user, and a program run as root, cannot open the folder.

To download a file without opening it: `konedrivectl sync hydrate <path>`. To keep files
downloaded for good, pin them ("Always keep on this device", under "Dolphin").

## The window

The sidebar starts with the **account switcher**: the account shown, and a menu with every account
and **Sign in…**. Below it are the pages of that account.

- **Status** — the folder and what it is doing now: up to date, downloading, uploading, paused, or
  what needs your attention. Buttons: **Refresh Now**, **Pause Syncing…**, **Free Up Space…**,
  **Open in File Manager**.
- **Activity** — the downloads and uploads under way, and what happened lately.
- **Conflicts** — your versions of files that changed on both sides, kept so that nothing is lost,
  with **Show in Folder** and **Dismiss**.
- **Not in the Folder** — what OneDrive has that cannot be shown here, and why: the Personal
  Vault, a OneNote notebook, a shared folder, a name Linux cannot hold.
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

- **Always Keep on This Device** — downloads it and keeps it, for a file or a whole folder; what
  comes into a pinned folder later is downloaded too;
- **Free Up Space** — makes it online-only again, and unpins it;
- **Open in OneDrive** — opens its page in OneDrive's web interface, where it can be shared and its
  versions seen. With two personal accounts this opens under the account your browser is signed in
  to (limitations log, F295).

Previews of images and videos come from OneDrive's own thumbnails, so showing them downloads
nothing. A preview of another kind of file, or one at the largest zoom, does download the file; to
avoid that, turn previews off in that folder (View → Show Previews).

Desktop search (KRunner, Dolphin's search) finds nothing in the folder: indexing it would
download every file.

## Freeing up space

- One file or folder: **Free Up Space** in Dolphin's menu.
- Everything that is not in use: **Free Up Space…** on the Status page.

The file stays in the folder with its name and size; only its copy on this computer goes. A file
you changed that has not been uploaded yet, and a file a program has open for writing, is never
freed.

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
once. A file moved from one account's folder to another's is copied and removed, as between two
disks.

**Remove Account…** on the Account page signs the account out and forgets it on this computer. Its
files stay where they are — those that were downloaded, and your own; a file that was never
downloaded is not on this computer — and nothing in OneDrive is deleted. If changes were not sent
to OneDrive yet, the window says how many before it goes on: they stay only on this computer.

## Uploading changes

**Off until you turn it on.** Every account starts read-only: its files cannot be changed here
(a program that tries is told the folder is read-only), so nothing can drift from OneDrive by
accident.

Turn it on with **Upload changes made on this computer** on the Account page. The browser opens
once more, for permission to change your files. From then on, what you add, change, move or delete
in the folder is added, changed, moved or deleted in your OneDrive.

- **What goes up**: new files and folders, edits, renames, moves and deletes. A changed file goes
  up a couple of seconds after the last change, once no program has it open for writing. The
  Status page counts what is waiting; Dolphin shows an emblem on each.
- **Changed on both sides**: both are kept. OneDrive's version keeps the name, and yours stays
  beside it as `Report-<computer name>.docx` and goes up under that name. It is listed under
  **Conflicts**.
- **What stays on this computer**, listed under **Not Uploaded**: a file whose name OneDrive
  refuses (rename it to upload it), a file larger than OneDrive takes, and, while OneDrive is
  full, every file with new content. Editors' temporary files stay too; the list of such names is
  on the Account page ("Never uploaded").
- **What the folder does not take**: symbolic and hard links, device files, pipes and sockets.
  OneDrive holds none of them.
- **Moving a file out of the folder** removes it in OneDrive. The file is copied to where it
  goes first, so one that was not downloaded is downloaded on the way.

### Removing files

A file or folder removed here is removed in OneDrive, and goes to **OneDrive's recycle bin**.

- **There is no trash on this computer for the sync folder**, and no question before a large
  removal: removing a folder with a thousand files removes them in OneDrive.
- **To get something back**, open the recycle bin in OneDrive on the web and restore it there; it
  appears in the folder again.
- **"Move to Trash" in Dolphin** asks whether to delete instead, and then deletes. Nothing is
  downloaded and nothing lands in the trash of your home directory. Shift+Delete deletes without
  the detour.
- **A folder that will not go.** A folder that holds in OneDrive something this computer cannot
  show (see **Not in the Folder**) cannot be removed here: removing it fails with "directory not
  empty", though it looks empty. The files shown in it are removed as you asked. A notification
  and the Activity page name the folder; remove it in OneDrive on the web.

### Turning uploading off

Turning **Upload changes made on this computer** off does not drop what is still to be sent.

- **Nothing waiting**: the folder is read-only again at once.
- **Changes still to send**: the account sends them first and then switches by itself. The Status
  and Account pages say **Switching to read-only: 3 changes still to send** meanwhile.
- **Switch Now** on the Account page switches at once. What is not sent stays on this computer as
  it is, and is sent when uploading is turned on again.
- **Cancel** leaves uploading on.

The wish survives a restart of the service.

## Changing or forgetting the folder

**Forget Folder** on the Account page stops keeping the folder in step. The folder becomes a plain
folder again: the files that were downloaded and your own stay in it, and can be changed; files
that were never downloaded are not in it. Nothing in OneDrive is deleted.

If changes were not sent to OneDrive yet, the window says how many before it goes on. Forgetting
the folder then leaves them as plain local files, and they are not sent.

**Choose Folder…** then binds the account to another empty folder.

If the OneDrive folder itself is moved, renamed or deleted while the service is not running,
KOneDrive stops syncing that account and says so. Move it back, or forget the folder and choose it
again.

## While the service was not running

What you do in the folder while the service is stopped is not followed as you did it. The folder
is locked then, to keep that from happening by mistake. If it is changed anyway:

- a new file, and a file whose content you changed, is uploaded once the service runs and
  uploading is on;
- a file of OneDrive that you moved is a new file at its new place, and the file is shown again
  at its old place;
- a file of OneDrive that you removed is shown again, not downloaded. Nothing is removed in
  OneDrive.

To move or remove something in OneDrive, do it while the service runs.

## Upgrading and removing

- **Upgrading** is the same `dnf install` with the newer packages. The service and the window
  restart by themselves. While the service restarts, the folder shows only the files that are on
  this computer, and a program reading a file in it gets an error: close programs that have files
  of the folder open first.
- **Removing**: first **Forget Folder** for every account, then
  `sudo dnf remove konedrive konedrive-kde`. A folder still registered when the package goes
  stays locked, and shows only the files that are on this computer.

## Troubleshooting

- The service's log: `journalctl --user -u konedrived -f`; more detail with
  `systemctl --user edit konedrived` →
  `Environment=RUST_LOG=konedrived=debug,konedrive_graph=debug,konedrive_tree=debug`.
- **Every file in the folder fails with "Transport endpoint is not connected".** The service
  ended without taking its folder off. Start it again (`systemctl --user start konedrived`): it
  clears the folder and serves it again.
- **The folder stopped by itself.** After a fault the service takes the folder down, shows the
  cause on the Status page and starts it again by itself: after 1 minute, then 5, then 30, then
  every hour. **Refresh Now** tries at once.
- Files: `~/.config/konedrive/config.toml` (the client ID, and each account with its name and
  folder; `config.toml.v1` is the single-account file it was migrated from, if any). Each
  account's state is in `~/.local/state/konedrive/accounts/<id>/`: `account.json` (cached name
  and quota) and `fuse.sqlite` (the items of the drive, the pins, the activity log and the
  conflicts). That file can be lost without losing a file: it is made again from OneDrive and
  the folder, but the pins and the activity log start empty. Each account's refresh token is in
  KWallet, under `KOneDrive: <email>`, and never leaves it (see SECURITY.md).

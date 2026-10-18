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
2. Choose **Sign In…** under **Add your OneDrive**. Your browser opens Microsoft's sign-in, with
   its account picker; there is nothing to register or type in first.
3. When the sign-in is done, Status asks where your OneDrive should appear. An empty folder is
   offered, `~/OneDrive` while it is free; **Choose Another…** picks another one, which must be
   **empty**. Leave **Two-way sync** off for now (see "Uploading changes") and choose **Continue**.

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
- **When OneDrive asks to wait.** OneDrive sometimes tells a computer that asked much of it to
  wait, for some seconds or a few minutes. Until then a file that is not downloaded cannot be
  opened: the program is told at once ("Resource temporarily unavailable") and is not kept
  waiting, a notification says how long the wait is and names the file, and the window, the tray
  and `konedrivectl sync status` say "OneDrive asked to wait" with the time left. Downloaded
  files open as always. Open the file again after the wait.
- **While the service is not running** — it was stopped, or it is being upgraded — the folder
  shows only what is on this computer: the downloaded files and files of your
  own that were not sent yet. It cannot be changed then. Everything is back when the service
  runs again.
- **Only you reach it.** Another user, and a program run as root, cannot open the folder.

To download a file without opening it: `konedrivectl sync hydrate <path>`. To keep files
downloaded for good, pin them ("Always keep on this device", under "Dolphin").

## The window

The sidebar starts with the **account switcher**: the OneDrive shown, and a menu with every one and
**Add Another OneDrive…**. Below it are the pages of that OneDrive.

- **Status** — what the folder is doing now: up to date, syncing, paused, or what needs your
  attention, with a line that leads to Issues. At the top: **Sync Now**, **Pause** or **Resume**,
  and **Open Folder**. Below:
  - the space, in one bar as long as your OneDrive's storage: bright for what is on this computer,
    pale for the rest of what your OneDrive holds; how many items are always kept here, and their
    size; **Free Up Space…** and **Unpin All…**;
  - **Downloading** and **Uploading**: the speed, the files moving, what is left with its size and
    time, and a chart of the last two minutes; then the files moving now;
  - **History**, folded until you open it: what happened lately, with a search by name or folder.
- **Issues** — what needs you: the changes that **Can't Upload** and why (see "Uploading
  changes"); files changed on both sides, of which both versions are kept so that nothing is lost
  (**Kept Both Versions**, with **Show Both** and **Dismiss**); and what **Can't Be Shown in the
  Folder**, and why: the Personal Vault, a OneNote notebook, a shared folder, a name Linux cannot
  hold.
- **OneDrive Settings** — its name (**Rename…**), the Microsoft account, **Two-way sync**, the
  folder (**Use Another Folder…**), previews, the free space to keep, what is never uploaded,
  **Sign Out** and **Disconnect This OneDrive…**.

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
  comes into a pinned folder later is downloaded too. Status says how many files are still to
  download, how much the folder takes on this computer, and how much of it is always kept;
- **Free Up Space** — makes it online-only again, and unpins it;
- **Open in OneDrive** — opens its page in OneDrive's web interface, where it can be shared and its
  versions seen. With two personal accounts this opens under the account your browser is signed in
  to (limitations log, F295).

Previews of images, videos, PDF and PostScript files, office documents (OpenDocument and Microsoft
Office) and EPUB books come from OneDrive's own thumbnails, so showing them downloads nothing. A
preview of another kind of file, plain text for one, or one at the largest zoom, does download the
file; to avoid that, turn previews off in that folder (View → Show Previews).

Desktop search (KRunner, Dolphin's search) finds nothing in the folder: indexing it would
download every file.

## Freeing up space

- One file or folder: **Free Up Space** in Dolphin's menu.
- Everything that is not in use: **Free Up Space…** on Status.

The file stays in the folder with its name and size; only its copy on this computer goes. A file
you changed that has not been uploaded yet, and a file a program has open for writing, is never
freed. Neither is a file always kept on this device: **Unpin All…** on Status makes nothing always
kept any more, and then **Free Up Space…** frees those files too.

## Pausing

**Pause** at the top of Status, or **Pause Syncing** in the tray menu: for 2, 8 or 24 hours, or
until you resume. Nothing is uploaded and OneDrive is not asked for changes meanwhile; opening a
file still downloads it. The pause stays across a restart.

An account can also pause by itself on a metered connection or on battery (Settings). **Sync
Anyway** on Status or in the tray lifts that, until the connection or the power changes
again.

## Several accounts

**Add Another OneDrive…** in the account switcher's menu adds another Microsoft account, with a
folder of its own. Two accounts' folders cannot be inside one another, and one Microsoft account can
be connected once. A file moved from one account's folder to another's is copied and removed, as
between two disks.

**Disconnect This OneDrive…** on OneDrive Settings signs the account out and forgets it on this
computer. Its files stay where they are — those that were downloaded, and your own; a file that was
never downloaded is not on this computer — and nothing in OneDrive is deleted. If changes were not
sent to OneDrive yet, the window says how many before it goes on: they stay only on this computer.

## Uploading changes

**Off until you turn it on.** Every account starts read-only: its files cannot be changed here
(a program that tries is told the folder is read-only), so nothing can drift from OneDrive by
accident.

Turn it on with **Two-way sync** on OneDrive Settings, or when you choose the folder. The browser
opens once more, for permission to change your files. From then on, what you add, change, move or
delete in the folder is added, changed, moved or deleted in your OneDrive.

- **What goes up**: new files and folders, edits, renames, moves and deletes. A changed file goes
  up a couple of seconds after the last change, once no program has it open for writing. Status
  counts what is waiting; Dolphin shows an emblem on each.
- **Changed on both sides**: both are kept. OneDrive's version keeps the name, and yours stays
  beside it as `Report-<computer name>.docx` and goes up under that name. Issues lists it under
  **Kept Both Versions**.
- **What stays on this computer**, listed on Issues under **Can't Upload**: a file whose name
  OneDrive refuses (rename it to upload it), a file larger than OneDrive takes, and, while OneDrive
  is full, every file with new content. Editors' temporary files stay too; the list of such names is
  on OneDrive Settings ("Never uploaded").
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
  show (Issues lists it under **Can't Be Shown in the Folder**) cannot be removed here: removing
  it fails with "directory not empty", though it looks empty. The files shown in it are removed as
  you asked. A notification and Status's History name the folder; remove it in OneDrive on the
  web.

### Turning uploading off

Turning **Two-way sync** off does not drop what is still to be sent.

- **Nothing waiting**: the folder is read-only again at once.
- **Changes still to send**: the account sends them first and then switches by itself. OneDrive
  Settings says **Switching to read-only: 3 changes still to send** meanwhile.
- **Switch Now** there switches at once. What is not sent stays on this computer as it is, and is
  sent when two-way sync is turned on again.
- **Keep Two-Way Sync** leaves it on.

The wish survives a restart of the service.

## Using another folder

**Use Another Folder…** on OneDrive Settings stops keeping the folder in step, and Status then asks
for another empty one. The old folder becomes a plain folder again: the files that were downloaded
and your own stay in it, and can be changed; files that were never downloaded are not in it.
Nothing in OneDrive is deleted.

If changes were not sent to OneDrive yet, the window says how many before it goes on. Leaving the
folder then leaves them as plain local files, and they are not sent.

If the OneDrive folder itself is moved, renamed or deleted while the service is not running,
KOneDrive stops syncing that account, and Status says why, with **Try Again**. Move the folder back
and try again, or choose another folder right there: **Continue** forgets the old one first.

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
- **Removing**: first **Disconnect This OneDrive…** for every account, then
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
  cause on Status and starts it again by itself: after 1 minute, then 5, then 30, then every hour.
  **Try Again** there tries at once.
- Files: `~/.config/konedrive/config.toml` (the client ID, and each account with its name and
  folder; `config.toml.v1` is the single-account file it was migrated from, if any). Each
  account's state is in `~/.local/state/konedrive/accounts/<id>/`: `account.json` (cached name
  and quota) and `fuse.sqlite` (the items of the drive, the pins, the activity log and the
  conflicts). That file can be lost without losing a file: it is made again from OneDrive and
  the folder, but the pins and the activity log start empty. Each account's refresh token is in
  KWallet, under `KOneDrive: <email>`, and never leaves it (see SECURITY.md).

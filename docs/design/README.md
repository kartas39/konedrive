# konedrive design

konedrive is a OneDrive client for KDE Plasma that reproduces Windows' *Files On-Demand*. Each
account's folder is a FUSE mount the daemon serves as the user: every file of the drive is there
at its real name, size and time, and a file that is not downloaded is read from OneDrive when a
program reads it. Under the mount lies the **backing directory**, the folder on disk itself, which
holds only the downloaded copies and what the user made here. What the folder shows is written in
one place, the store's row of each item.

This directory says how that works and why, for someone who will review or change the system. How
a single function works is said in its module doc; these documents say what the parts are, what
holds between them, and why.

## Documents

| Document | What it covers |
|---|---|
| This page | The overview: the processes, a read and a write from start to finish, the invariants everything else serves, the words |
| [sync.md](sync.md) | The item and its three versions, who writes what, the listing, the sync plan, a step and what it remembers, the plan's task, a folder's start and stop, how the model is tested |
| [mount.md](mount.md) | The mount and the backing directory: what each request of the kernel does, reading a file that is not on disk, downloads, freeing up space, the start-up check, the mount's end |
| [writes.md](writes.md) | Uploads: the access mode, what a program's change does, the requests to OneDrive, conflicts, what holds sending back (pausing, the automatic hold, throttling), what the bus shows of what waits, the test account |
| [pinning.md](pinning.md) | "Always keep on this device": the pin, what a pin downloads and keeps, freeing up around pins, and how the bus, the command line and Dolphin show it |
| [accounts.md](accounts.md) | Several accounts, each with its own folder: what an account is, the configuration and each account's files, keeping accounts apart, adding and removing |
| [desktop.md](desktop.md) | The D-Bus API, `konedrivectl`, the window and tray icon, notifications, progress, thumbnails, Baloo, the Dolphin plugins |
| [decisions.md](decisions.md) | The notable decisions, each with its reason and its cost |
| [packaging.md](packaging.md) | The RPM packages: what goes where, upgrades, and the developer install |

Related documents elsewhere in the repository:

- [`../code-map.md`](../code-map.md) — every crate, directory and file, with its tests.
- [`../kernel-behavior-7.2/`](../kernel-behavior-7.2/README.md) — what the kernel and the
  filesystems were measured to do on Linux 7.2, and how to reproduce each measurement.
- [`../kio-behavior.md`](../kio-behavior.md) — what KIO and Dolphin open; the thumbnail cache.
- [`../limitations/`](../limitations/README.md) — the limits that cannot simply be removed,
  because the kernel, the filesystem, OneDrive or the desktop decides, or because we chose them.
  Its entries have short identifiers (F296, K1, …), which these documents use to point at them.

## Status

The client lists the whole drive, keeps the folder in step with changes made in OneDrive, reads a
file that is not downloaded from OneDrive, downloads and frees up files on request, keeps pinned
files downloaded, and serves several accounts, each with its own folder.

A folder is **read-only** until its account is switched to uploading. A read-only folder refuses
every change a program makes (`EROFS`); its account's sign-in asks for `Files.Read`, so Microsoft
itself refuses any write made with its token. A **read-write** folder takes changes, and what is
changed in it is uploaded ([writes.md](writes.md)). Uploading is the user's choice, account by
account: the switch on the Account page, or `konedrivectl account mode read-write`.

Supported: several personal Microsoft accounts, each with its own folder; KDE Plasma 6 with KDE
Frameworks 6.21 or later; FUSE 3 (`fusermount3`). A folder's filesystem must give nameless files
(`O_TMPFILE`), sparse files and `user.*` extended attributes (the probe when a folder is
chosen, [accounts.md](accounts.md)); the design was measured on Linux 7.2.

## The processes

```text
                 Microsoft Graph (HTTPS: reads, and a read-write account's uploads)
                                          ▲
                                          │
 program ──read, write──▶ kernel ──FUSE──▶ konedrived ◀──D-Bus──┬── KOneDrive window + tray
                         (the mount      (the user;            ├── konedrivectl
                          over the       user service)         └── Dolphin plugins
                          folder)              │
                                               └──▶ backing directory (the folder on disk,
                                                    by a descriptor opened before the mount)
```

| Process | Runs as | Does | Never does |
|---|---|---|---|
| `konedrived` | the user; systemd user service, D-Bus activated | Everything, for each of the user's accounts: sign-in and tokens, the listing, the store, the mount and its answers to the kernel, the downloads, the sync plan and its steps, the uploads, thumbnails, the D-Bus API | Run with any privilege |
| `konedrive` (KOneDrive) | the user | The window and tray icon: shows what the daemon publishes and calls its methods | Touch the folder itself |
| `konedrivectl` | the user | The command line for every feature of the window, plus developer commands | — |
| Dolphin plugins | inside Dolphin | Emblems from the daemon's answer for a file or a folder; "Always keep on this device", "Free up space" and "Open in OneDrive" in the context menu | Open a file in the folder |

The mount is put on and taken off with `fusermount3`, the setuid program of the `fuse3` package,
as every FUSE filesystem of an ordinary user is. It is `nosuid` and `nodev`, without `allow_other`:
only the user reaches the folder through it. There is no privileged helper and no system service
([decisions.md](decisions.md), §1.1).

## A read, from start to finish

1. A program opens `~/OneDrive/report.pdf`. The kernel asks the daemon to look the name up; the
   mount answers from the row whose local place is that name in that directory, with the item's
   local item id as the kernel's node id. No request of the kernel reads the disk to learn what
   the folder contains.
2. The row says the content is not on disk. The open is answered at once; the handle remembers
   which content it was opened as.
3. The program reads. The bytes asked for are fetched from OneDrive as a range of that content.
   Once more than a threshold of the file was read this way, the file is downloaded whole, and the
   reads go on from the download as it arrives.
4. The download is checked against the content's size and hash, synced, and landed: linked under
   the name the row has by then, its directory synced, and only then the row says the content is
   on disk. Later reads come from the backing file.

If OneDrive by now has another content than the one the handle was opened as, the read fails
(`ESTALE`) instead of mixing two versions; the next open reads the new one. [mount.md](mount.md)
has the details.

## A write, from start to finish

1. A program opens a file of a read-write folder for writing. On the mount's one write-operation
   thread the row is changed first: the local content is marked changed here. Then bytes go to the
   backing file with no row touched per write.
2. At the last close the size and time are written into the row. The row's local version now
   differs from the synced one, and the store gives it a stamp, `unsent_since`.
3. The sync plan's task is woken. It reads the rows that differ, makes the plan — one pure
   function of the rows — and starts its steps: here, sending the content.
4. The step opens an upload session, sends the file in fragments, guarded by the cTag of the
   content OneDrive has as the row knows it, and commits OneDrive's answer into the row under the folder's lock, only
   if the row is still what the step was planned from. The two sides agree; the stamp goes.

Changes made in OneDrive travel the other way. When OneDrive's notification socket says
something changed, on request, and otherwise every 60 seconds (every 5 minutes while that socket
is up), the daemon reads the delta listing into the rows' OneDrive versions, and the plan makes
the folder follow. [sync.md](sync.md) has the details.

## Invariants

These properties are what the rest of the design serves. Each names where it is kept.

**What the folder shows is the rows.** Every name, place and attribute the mount answers comes
from the item's local version in the store, never from the disk, the plan or OneDrive. Three
exceptions are named: the sync state is a function of the row; the blocks a file takes are the
backing file's; and the size and times of a file a program holds open for writing are the backing
file's, because the kernel puts an append where the answered size says
([sync.md](sync.md) §3).

**A program never reads bytes of the wrong content.** A file not on disk is read as ranges of the
content it was opened as; a download lands only after its size and hash were checked; a row says
the content is on disk only once the disk holds it for good ([mount.md](mount.md) §3, §5).

**Local changes are never lost.** A row never says a copy is clean while the disk may hold the
user's bytes: the first writer's open marks the content changed before any byte is written, and a
copy is freed only while the local content is the synced one and no program writes it. A file
changed on both sides is kept twice: the local content under a conflict copy's name. Nothing is
removed in OneDrive because a file is missing from the disk at start ([mount.md](mount.md) §7;
[sync.md](sync.md) §6).

**Every step is conditional.** A step carries the versions it was planned from, runs outside the
lock, and commits only if those are still the row's; otherwise it changes nothing and the next
plan decides ([sync.md](sync.md) §7).

**A crash loses nothing that costs the user.** A change of a name in the backing directory is on
record before the disk is touched, and the start-up check finishes or undoes exactly that change;
every request that changes OneDrive is on record before it leaves, and is settled at start by
reading its item; a download lands before its row says it. Losing the store costs one listing:
the downloaded copies carry the OneDrive id they belong to ([mount.md](mount.md) §3, §7;
[sync.md](sync.md) §7).

**Nothing runs as root.** The daemon serves the mount as the user; `fusermount3` is the only
setuid program involved, and it is the system's.

## Where things live

| What | Where |
|---|---|
| The folders | one per account, anywhere the user owns, on a filesystem that passes the probe; chosen when the account is added; never one inside another |
| The backing directory | the folder itself, under its mount: reached by the daemon through a descriptor opened before the mount; a plain directory showing the downloaded files, without its write bits, while no daemon runs |
| Daemon configuration | `~/.config/konedrive/config.toml`: the client id, the transfer and pause settings, and each account with its label, mode, drive and folder ([accounts.md](accounts.md)) |
| The store: the rows, what the steps remember, the pins, the activity log, the conflicts | `$XDG_STATE_HOME/konedrive/accounts/<account id>/fuse.sqlite` |
| Cached account name and quota | `$XDG_STATE_HOME/konedrive/accounts/<account id>/account.json` |
| Refresh tokens | the Secret Service (KWallet), one item per account; never written anywhere else |
| Thumbnails | the freedesktop cache, `~/.cache/thumbnails/{normal,large,x-large}` |

The crates and directories are in [`../code-map.md`](../code-map.md).

## The words

One word per thing, in these documents, in the code and in the window.

| Word | What it is |
|---|---|
| sync folder, folder | the folder an account's OneDrive is shown in: a FUSE mount the daemon serves |
| backing directory | the directory on disk the mount lies over: the downloaded copies and what the user made here |
| item | one file or directory the folder shows or OneDrive has, from the moment the daemon learns of it until it is gone from both sides |
| local item id | the number the store gives an item, never reused, never changed; it is the kernel's node id too |
| OneDrive item reference | a drive id and a OneDrive item id: what names one item of OneDrive |
| binding | which item a OneDrive item reference belongs to; it can move from one item to another |
| place | the directory an item is in and its name there |
| content version | what tells one content of a file from another: OneDrive's hash and cTag, or a count of changes made here before a hash is known |
| version | what an item is on one side: its place and, for a file, its content |
| local, OneDrive, synced version | what the folder shows; what OneDrive had when last asked; what the two sides last agreed on |
| on disk, not on disk | whether the backing directory holds a file's whole content (the downloaded copy, or what was made here) |
| unsent | an item whose local version is not the synced one; its stamp `unsent_since` orders what is sent and shown |
| listing | OneDrive's delta listing, written into the rows page by page |
| sync plan | the steps that make the local and the OneDrive versions agree again; a pure function of rows |
| step | one thing the plan does: towards OneDrive (a request to Graph), towards the folder, or of the rows alone |
| step memory | what a step keeps of an item between tries and across a stop: no source of truth |
| start-up check | the walk of the backing directory before the mount goes over it, the one place the disk is read to learn what the folder contains |
| download, land | fetch a file's whole content into a file with no name; give it its name and make the row say on disk |
| free up | remove a file's downloaded copy; the row says not on disk first |
| pin | "Always keep on this device": a rule that a file, or what is in a folder, is kept on disk |
| access mode | read-only or read-write: whether the folder takes changes and what is changed is uploaded |
| conflict copy | the local content of a file changed on both sides, renamed `<name>-<machine>.<ext>` beside the other and uploaded as a new file |
| account | one Microsoft account signed in to konedrive, with its own folder; known by a random id and a label, and identified by its drive |

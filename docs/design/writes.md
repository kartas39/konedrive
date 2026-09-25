# Uploads: changes made on this computer

How a read-write account's folder sends what is changed in it back to OneDrive: the mode that
allows it, noticing local changes, deciding what each one is, the outbox and its worker, the
requests and their guards, conflicts, objects moved out of the folder, and what the reconcile does
differently when the folder holds work of its own. How a placeholder is filled is in
[hydration.md](hydration.md); how the folder follows OneDrive is in [sync.md](sync.md); the mode
itself is in [accounts.md](accounts.md) §10.

**In this version uploading is gated.** Only an account whose drive is listed in
`write_test_drive_ids` in `config.toml` can be switched to read-write, and that list is empty
unless someone puts a test account's drive in it by hand (§2.3). Every other account is read-only,
exactly as [sync.md](sync.md) describes.

**What always holds.**

- **WR1 — only local content is uploaded.** Content is read only from a file whose state is
  `hydrated` or that carries no konedrive state; an `online-only`, `hydrating` or `dehydrating`
  file is never read for an upload: its bytes are not the item's.
- **WR2 — every write is guarded, and a failed guard is resolved by reading again.** Changes carry
  `If-Match`, creates `conflictBehavior=fail`; a `412` or `409` leads to a fresh read and a
  decision (§6.2, §7), never to a retry without the guard.
- **WR3 — no local byte is lost.** Where the read phase would rescue a file, a read-write folder
  makes a conflict copy in the folder and uploads it; a change from OneDrive removes a local file
  only if it holds nothing only this computer has.
- **WR4 — the disk is the truth about local changes.** The outbox records intent and progress; the
  examination can rebuild it from the disk and the base. A rebuilt base never deletes anything in
  OneDrive.
- **WR5 — nothing is deleted in OneDrive while its content exists only there and the user still
  holds the file.** A placeholder moved out of the folder is downloaded before its item is deleted
  (§8).
- **WR6 — no echo.** The daemon's own local changes never become outbox rows, and its own changes
  in OneDrive never come back as changes from OneDrive (§3.2, §9).
- **WR7 — every outbox step can be replayed.** After a crash it reaches the same end; "did it
  land?" is answered by content hash or by place, never by guessing (§10).

## 1. What the user sees

- **An account is read-only until it is switched.** "Upload changes made on this computer" on the
  Account page, or `konedrivectl account mode read-write`, signs in again, in the browser, for
  permission to change files. Once Microsoft grants it, the folder loses its read-only lock and what
  is changed in it goes up.
- **Anything that changes the folder counts**: a program saving a file, a new folder, a rename, a
  move within the folder, a delete. A change goes up once nothing has touched its directory for 2
  seconds and nobody has the file open for writing. A file kept open for writing (a database, a log)
  waits until it is closed, as on Windows.
- **Dolphin** shows the sync emblem on a file waiting to go up or going up, and the error emblem on
  one that cannot go up. The window's Status page counts the changes waiting and their size;
  Activity lists "Uploading now" and "Waiting to upload"; a Plasma job shows an upload that takes
  two seconds or more.
- **Changed on both sides**, both versions are kept, as on Windows: the cloud's version keeps the
  name, and this computer's goes up beside it as `Report-<machine>.docx`. The pair is listed under
  Conflicts, with "Show Both".
- **Names OneDrive refuses** (`a:b.txt`, a trailing space, `CON`, …) are not renamed for the user.
  They are listed under "Not Uploaded" until the user renames them. So are symbolic links, FIFOs,
  sockets and devices, hard links, and files on another filesystem mounted inside the folder.
  Editors' temporary files (the ignore list) stay local without being listed.
- **Moving a file out of the folder** removes it from OneDrive (to its recycle bin), but only once
  its content is on this computer: a file that was not downloaded is downloaded where it went
  first. Sending it to the desktop Trash instead removes it without a download, since OneDrive
  keeps it in its recycle bin.
- **A large delete** (more than 500 items, or a fifth of the folder) is held back and asked about:
  "Delete in OneDrive Too" or "Restore Them".
- **Pause** stops uploads (and asking OneDrive for changes) for 2, 8 or 24 hours or until resumed;
  opening a file still downloads it.
- **Offline**, changes collect; ten saves of one document are one upload when the network is back.
- **Back to read-only** is refused while changes wait, unless the user chooses to drop them; the
  files themselves stay as they are, and the folder is locked again.

Every one of these has a command: `sync outbox`, `sync pause`/`resume`, `sync ignore`,
`sync not-uploaded`, `sync deletes confirm|restore`, `account mode` ([desktop.md](desktop.md) §3).

## 2. The mode

### 2.1 Read-only and read-write

The mode is per account, in `config.toml`, a new account read-only; `Account1.Mode` publishes the
mode the account *runs* in ([accounts.md](accounts.md) §10). The scope follows it:
`Files.Read User.Read offline_access` for read-only, asked for at every refresh, so that Microsoft
refuses a read-only account's writes whatever it was once granted;
`Files.ReadWrite User.Read offline_access` for read-write.

### 2.2 The switch

**To read-write**: a sign-in for `Files.ReadWrite`, pinned to the account (its password asked for
again, its email filled in). Only a token response that grants it, for this account's own drive,
writes the mode. Then the folder's sync restarts in read-write mode, in this order:

1. the watcher starts and walks the folder, marking every directory (§3);
2. only once every directory is marked does the read-only lock come off (files `0644`,
   directories `0755`); a folder whose watcher cannot start stays locked, runs as a read-only one
   and says why in `LastError`;
3. the walk ends in a Full local scan, which finds anything forced past the old lock (§4.6);
4. the first cycle waits for that scan, and the outbox worker for the first cycle, before it
   sends anything (§9).

**To read-only**: refused `PendingUploads` while rows wait, unless forced. Forced, the rows are
dropped and the files stay, as ordinary local changes the read phase's stamp check protects (a
rename half-done under a temporary name stays). The watcher and the worker stop, the lock walk
runs, and the next refresh asks for `Files.Read`. No sign-in. Any other way to read-only — a
sign-out, an expired sign-in, the gate or `config.toml`, a narrower grant — keeps the rows: the
folder is locked, and its sync runs no cycle while they wait, so the read phase's reconcile never
puts back what they describe; they go once the account is read-write again, and a forced switch
drops them then too (limitations log F140). A Forget and removing the account are refused
`PendingUploads` while rows wait (F141).

### 2.3 The write gate

While uploads are being finished, `SetMode("read-write")` and `Dev1.ReadWriteAccessToken` are
refused `WritesNotAllowed` for any account whose drive id is not in `write_test_drive_ids`, a
top-level list in `config.toml`. The list is empty by default and nothing in konedrive writes it; a
developer adds a test account's drive by hand. An account whose `mode` was set to read-write by
hand runs read-only unless its drive is listed, and `LastError` says so. The gate is read from the
file each time the mode is worked out, and the outbox worker asks it again before each row, so a
drive taken off the list turns read-only at once and nothing more is sent. The
release removes the gate in a change of its own ([decisions.md](decisions.md), "The mode, and the
write gate"; limitations log F60).

## 3. Noticing local changes: the watcher

### 3.1 A second fanotify group, in the daemon

The helper's permission group cannot report directory entries: the kernel refuses file-handle
reporting for a pre-content group. So the daemon holds a second group of its own, unprivileged,
for each read-write folder, marking the same directories:

```text
fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME_TARGET | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY | O_CLOEXEC)
every directory:  FAN_CREATE | FAN_DELETE | FAN_RENAME | FAN_CLOSE_WRITE | FAN_ATTRIB | FAN_ONDIR | FAN_EVENT_ON_CHILD
the root also:    FAN_DELETE_SELF | FAN_MOVE_SELF
```

`FAN_RENAME` carries both sides of a move in one event, with the moved object's handle; a side
that is missing means the object came from, or went to, a directory nobody watches.
`FAN_MOVED_FROM`/`FAN_MOVED_TO` are not subscribed (they would triple the queue's use per rename),
nor is `FAN_MODIFY`: a write is looked at when it is closed. What the kernel does with each of
these was measured on 7.2 ([`../kernel-behavior-7.2.md`](../kernel-behavior-7.2.md) §14).

An unprivileged group has limits of its own, all measured: inode marks only, a queue of 16 384
events, and a budget of marks per user that every group of the user shares. A full queue is
reported (`FAN_Q_OVERFLOW`) and costs a Full local scan and a walk that marks whatever was missed,
never a missed change (limitations log F70). A spent mark budget turns the watcher *degraded*: a
Full scan and a walk every 10 minutes instead of events, and `LastError` says so (F71).

### 3.2 The daemon's own changes

To an unprivileged listener the kernel reports the pid of an event only when the listener caused
it. Events carrying the daemon's own pid are dropped: placeholders placed, the reconcile's renames,
conflict copies, attribute writes and every write of a fill (a fill runs in the daemon's process,
so its writes are the daemon's even through the helper's descriptor). The watcher still follows the
daemon's own new directories in its map, without making them dirty.

### 3.3 The directory map

An unprivileged process cannot open a file handle, so the watcher keeps a map from each directory's
handle to its parent and name, built by its walk and kept current from `FAN_ONDIR` events. An
event's directory handle becomes a path through it; the directory is opened beneath the root
(`openat2`, `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`) and its own handle compared with the event's.
An unknown handle asks for a walk, at most once a minute.

### 3.4 Batches

Each event makes its directory and, by the object's handle, its object dirty; `FAN_CLOSE_WRITE`
also asks for the content to be hashed. Dirt is kept by handle and turned into paths only when it
is handed over, so a directory renamed meanwhile is examined where it now is. A batch is handed
over when no event arrived for **2 s**, and at the latest **30 s** after its first event. The
examination runs on a thread of its own, so a long Full scan never holds up the queue's reader.

### 3.5 A new directory

A directory made or moved into the folder by anyone but the daemon is, in this order: sent to the
helper's `MarkDir` (the permission mark, M1), given the watcher's own mark, then listed, every
subdirectory going through the same steps before its contents are looked at. Whatever was put in it
before the mark is found by the listing. A tree moved into the folder is marked this way, top down.
The window in which a placeholder moved into a brand-new directory could be opened unintercepted
shrinks to the event's latency plus one `MarkDir` (measured under 30 ms; limitations log Z2).

### 3.6 Other devices

A directory on another device than the folder's root — a nested Btrfs subvolume, a filesystem
mounted inside the folder — is neither watched nor uploaded: the helper cannot mark it, and so
nothing in it could be protected. It is listed under "Not Uploaded" as `other-device`, and
`LastError` counts such places, as Windows treats a mount point inside OneDrive (limitations log
F72).

### 3.7 The root itself

`FAN_DELETE_SELF` or `FAN_MOVE_SELF` on the root, or an examination that finds the root gone,
stops the folder's sync and outbox and sets `RootState` to `error`. Nothing is deleted in OneDrive
because the folder went away.

## 4. The examination: from a batch to rows

### 4.1 Three sides

The tree store's `items` table is the **base**: the last state the folder and OneDrive agreed on.
The disk is **local**, the delta feed **remote**. A local change is local ≠ base. An item is known
on disk by its `user.konedrive.item-id` attribute, never by its path: a rename is an item id seen
under another name, and an editor's save by rename is a new inode taking over an id. Where two
inodes carry one id (a copy that kept the attributes), the one whose file handle the base recorded
(`items.local_handle`, taken when the item was placed, adopted or committed) is the item.

Nothing is examined before the folder's first listing has completed: until then there is no base
to compare with.

### 4.2 The rules

Each dirty directory is listed, and each entry read by name (`lstat`, `lgetxattr`), never opened.

1. The daemon's own names (`.konedrive-*`) are skipped; a user's file named so is listed
   (`reserved-name`).
2. Not a regular file or a directory: never uploaded, listed once (`symlink`, `fifo`, `socket`,
   `device`), and never followed. On another device: listed (`other-device`, §3.6).
3. A name on the ignore list without an item id stays local, unlisted. The list is per account
   (`ignore` in `config.toml`, `konedrivectl sync ignore`, the Account page): shell globs matching
   the name, case-sensitively, including editors' swap and backup files, `~$*`, `*.part`,
   `*.crdownload`, `*.tmp` and `.goutputstream-*` by default. A directory whose name is ignored
   keeps everything under it local. A shorter list runs a Full local scan, so what is no longer
   ignored goes up.
4. A directory without an item id: a `mkdir` row, and its contents are examined.
5. A file without an item id: a `create` row.
6. An entry with an item id: where the base has it, the content check (§4.3); elsewhere in the
   folder, a `move` row and the content check. An id the base does not know (a file from another
   account's folder, or one deleted since) is stripped of konedrive's attributes and created if it
   is downloaded, and left and listed (`not-downloaded`) if it is not. A second inode carrying an
   id is a copy: stripped and created if downloaded. A second name of the same inode, a hard link
   OneDrive cannot hold, is listed (`hard-link`), and so is a downloaded file from elsewhere with
   other links, whose other names stripping would change too.
7. A base item missing from its place and not found elsewhere in the batch:
   - a new file at its name (**save by rename**: a temporary file renamed over the original, or the
     original renamed to a backup and a new one written) is an `update` of the item from the new
     inode, so the item keeps its id, its version history and its sharing links. An original left
     under an ignored name beside it (`file~`) stays as the user's backup;
   - otherwise the helper is asked whether the object still exists, by the file handle the base
     recorded (`OpenByHandle`, §8). **Gone** (`ESTALE`) is a `delete`; **alive outside the folder**
     is a `move-out`; any other answer, or no helper, decides nothing and the item is looked at
     again later. `ESTALE` is a delete only with its evidence: nothing, or another object, stands
     where the item was last proved to be, and the store's handles were taken on the filesystem the
     folder is on now (`meta.handles_root`: the root directory's own handle and, where the kernel
     gives one, the filesystem's UUID). When that record changes (the folder's home moved to a new
     disk, a snapshot rolled back), a Full local scan takes every handle again, and an item missing
     then is placed again from OneDrive rather than deleted (limitations log F54, F121).

A folder's removal waits for everything the base has inside it to be accounted for: an item moved
out of it first is a `move-out` of its own, and an item that cannot be found holds the folder back.

### 4.3 The content check

| The file | Decision |
|---|---|
| `hydrating` or `dehydrating` | busy: looked at again in 30 s |
| `online-only`, size as in the base | unchanged |
| `online-only`, another size | a `truncate(2)` by path, which opens nothing and fills nothing: the size and time are put back through the daemon's own descriptor, and nothing is uploaded (limitations log F75) |
| `hydrated`, a read lease refused | someone has it open for writing: a row in `waiting` (`open-for-writing`), looked at again in 30 s (F81) |
| `hydrated`, size ≠ stamp | changed: `update` |
| `hydrated`, same size, another time, or a `FAN_CLOSE_WRITE` seen | hashed (quickXorHash, one read): another hash is an `update`; the same hash only refreshes the stamp, so a `touch` uploads nothing |
| `hydrated`, same size and time, no event | unchanged |

The read lease (`F_RDLCK` on a read-only descriptor) is taken and released at once, never held
across anything slow. Only content that is local is ever read for an upload: a file `online-only`,
`hydrating` or `dehydrating` holds bytes that are not the item's.

### 4.4 The name pre-check

A name OneDrive refuses is blocked before a request is spent on it: one of `" * : < > ? \ |`, a
leading or trailing space, a reserved name (`.lock`, `CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`,
`LPT0`–`LPT9`, `desktop.ini`, `_vti_` anywhere, a `~$` prefix), a name that is not UTF-8, and a
file over 250 GB. The check blocks only what Microsoft's page names; whatever it misses is refused
by the service (`400`), and blocked with the service's own message. Nothing is ever substituted.

### 4.5 The mass-delete guard

A batch or a scan that would remove more than **500** items, or **20 %** of the folder's items when
it holds at least 10, holds every removal (`delete` and `move-out` rows, counted once per item,
those already waiting included) as `held`. `HeldCount` counts them; the window asks, and so does a
notification. `ConfirmDeletes` releases them all; `RestoreDeletes` drops them and runs a Full
reconcile, which places the items again from OneDrive (and tidies what a held move-out left
outside, §8.5). A removal once confirmed is not held again. The numbers are provisional.

### 4.6 The Full local scan

The same examination over every directory, at bring-up, after an overflow, after the helper comes
back and when the ignore list shrinks. It finds what events would have shown, with one exception:
an edit made while the daemon was not running that kept both the size and the time (limitations
log F52). With a rebuilt base (the store lost or recreated) it makes no `delete` rows: an item
without a recorded handle cannot be proved gone, and is placed again from OneDrive instead.

## 5. The outbox

### 5.1 In the tree store

The store's schema is 3. Its tables beside the read phase's:

```sql
CREATE TABLE outbox (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,  -- detection order, never reused
  kind TEXT NOT NULL,                     -- create | mkdir | update | move | delete | move-out
  item_id TEXT,                           -- NULL until a create or mkdir lands
  dev INTEGER, ino INTEGER,               -- the local inode concerned
  rel TEXT NOT NULL,                      -- where it was last seen, relative to the root
  base_etag TEXT, base_ctag TEXT, base_parent TEXT, base_name TEXT,  -- what it was made against
  target_parent TEXT, target_name TEXT,
  state TEXT NOT NULL,                    -- waiting | ready | running | retry | blocked | held
  reason TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_try INTEGER,
  snapshot TEXT,                          -- '<size> <mtime>' of the content being sent
  session_url TEXT, session_expires INTEGER, session_next INTEGER,
  handle BLOB,                            -- the object's file handle
  confirmed INTEGER NOT NULL DEFAULT 0);  -- a removal the user confirmed
CREATE TABLE local_skipped (rel TEXT PRIMARY KEY, reason TEXT NOT NULL, at INTEGER NOT NULL);
-- items and staging: + local_handle BLOB, local_seq INTEGER (the outbox commit that last wrote the row)
-- conflicts: + kind ('rescued' | 'copy')
-- outbox_seen: what the base held below a folder when its removal was decided
-- outbox_gone, deferred: the reconcile's (§9)
-- meta: + outbox_seq, paused_until, handles_root
```

The store is `0600`, and an upload URL, a credential for its one file until it expires, is kept
nowhere else and never logged. A store of an older schema is rebuilt, which is safe because a
folder of schema 2 was read-only and had nothing pending.

**The disk is the truth about local changes; the outbox records intent and progress.** Every row
can be found again by comparing the disk with the base, so losing the outbox or the whole store
costs a listing and restarted uploads, never data. The worst case is a forgotten pending delete:
the file comes back from OneDrive.

### 5.2 One live row per item

A new detection merges into the item's row. A row being sent is never changed: one follow-up row
waits behind it, and the commit rebases it (a new eTag, or the item id behind a create).

| Row + detection | Result |
|---|---|
| `create` + `update` or `move` | `create`, of the newest content at the newest place |
| `create` + `delete` | nothing: the row goes |
| `mkdir` + `delete` | nothing, with the rows inside it |
| `update` + `move` | one row: the move, then the content |
| `update` or `move` + `delete` | `delete`, against the update's base |
| `move` + `move` | one move to the final place; back where the base has it, the row goes |
| `delete` + a new file at the name | `update` (save by rename) |

**States**: `waiting` (the quiet spell, or open for writing) → `ready` → `running` → gone at the
commit; or `retry` (with `next_try`), `blocked` (needs the user: a refused name, too large,
OneDrive full, no permission) or `held` (§4.5).

### 5.3 Order

Rows run in `seq` order under four rules:

1. a row waits for every earlier row on the same item;
2. a row waits for the `mkdir` or `create` of its new parent, whose item id it needs;
3. a folder's `delete` or `move-out` waits for every row of what the base has inside it, whatever
   their order;
4. a row that takes a name in OneDrive (a `mkdir`, a `create`, a move's target) waits for a row
   that frees that name, names compared without regard to case.

Rule 4 alone can close a circle (`mv d t/ && mv t d`); inside such a circle its waits are dropped.
A row that then meets its name still taken (`409`), by an item a live row is freeing, takes it
through a temporary name, `.konedrive-swap-<id>`, saved in the row before the request, and a
final `move` row follows once the name is free. Swaps (`a` ↔ `b`) go the same way.

Metadata rows (`mkdir`, `move`, `delete`) run one at a time; content rows beside them, at most 4 of
up to 10 MiB and 2 larger; `move-out` rows one at a time. These numbers are provisional.

### 5.4 The commit

After a successful answer, under the folder's tree lock (the lock a cycle holds from staging to its
swap, §9):

1. **On the file**, through its own descriptor, never by path: the stamp from the snapshot, the
   cTag, `state=hydrated`, `fsync`, and the item id last. An item id without a state is the one
   combination the helper refuses with `EIO`; a state without an id is simply an ordinary file.
2. **In the store**, one transaction: the base row from Graph's answer (with `local_handle`),
   `local_seq = ++outbox_seq`, the row deleted, and the activity event.

A crash between the two is replayed (§10). A file freed up meanwhile stays a placeholder of the
version just sent. While a row waits, its file carries `user.konedrive.sync` (`pending`,
`uploading` or `blocked`), which the Dolphin plugin turns into an emblem; the commit takes it off.

## 6. The requests

### 6.1 What is sent

Every change carries a guard: `If-Match` on anything that exists, `conflictBehavior=fail` on
anything new. A guard that fails is resolved by reading the item again, never by sending the change
without it.

| Operation | Request | Guard |
|---|---|---|
| a new empty file | `PUT /items/{parent}:/{name}:/content?@microsoft.graph.conflictBehavior=fail`, then a `PATCH` of its time | `conflictBehavior=fail` in the URL: a `PUT`'s default is to replace |
| an emptied file | `PUT /items/{id}/content`, then the `PATCH` | `If-Match: <base eTag>` |
| a new file, 1 B – 10 MiB | `POST /items/{parent}:/{name}:/createUploadSession` with `conflictBehavior: fail`, its name and `fileSystemInfo`, then one `PUT` of the whole body | `conflictBehavior: fail` |
| a changed file, 1 B – 10 MiB | `POST /items/{id}/createUploadSession` with `fileSystemInfo`, one `PUT` | `If-Match: <base eTag>` |
| over 10 MiB | the same session, in fragments of 10 MiB (32 × 320 KiB) | as above |
| a new folder | `POST /items/{parent}/children` | `conflictBehavior: fail` |
| a rename or move | `PATCH /items/{id}` with `name` and/or `parentReference.id` | `If-Match: <base eTag>` |
| a file's delete | `DELETE /items/{id}`, into the recycle bin | `If-Match: <base eTag>` |
| a folder's delete | `DELETE /items/{id}` | `If-Match: <the folder's cTag when its removal was decided>` |
| a session's status, its end | `GET` / `DELETE` of the upload URL | — |

A row made against a download that was not the base's version carries only its cTag, and that is
its guard. Every file but an empty one goes through a session, even a small one: the session
request documents both guards and carries the file's time, while a plain `PUT` documents neither
and would need a `PATCH` for the time anyway, so the session costs no extra request
([decisions.md](decisions.md), "Small files go up in an upload session"). A session request carries no `fileSize`: a personal
drive refuses it with `400 invalidRequest` (measured on the test account), so a full drive shows
itself when a fragment is refused. Requests to an upload URL never carry the account's token.

### 6.2 What the answers mean

| Answer | Action |
|---|---|
| `200`, `201` | the commit (§5.4) |
| `202` | a fragment accepted: `session_next` persisted, the next one sent |
| `409` | the item at that name is read. A create adopts it when its hash is ours (it is this content already), a folder adopts a folder and the two merge, a move adopts its own item (it landed); a name a live row is freeing goes through a temporary name (§5.3); anything else makes the file here a copy (§7) |
| `412` | the item is read again: the same hash as ours means done already; the base's cTag means only its metadata changed, and the request goes again with the fresh eTag; otherwise §7 |
| `404` | gone in OneDrive: §7 |
| `404` from an upload URL | the session ended: the item is read and adopted if its hash is ours, else a new session from zero |
| `416` | a fragment the session has: its status says where to go on |
| `423` | locked (co-authoring): retried later |
| `507`, `quotaLimitReached` | `blocked` (`quota-exceeded`), tried again when the quota changes or every 30 minutes |
| `400` | `blocked`, with the service's message |
| `401` | the token refreshed once |
| `403` | `blocked` (`forbidden`), and `LastError` says to sign in again; a new sign-in releases the rows |
| `429`, `503` | the whole account's worker waits until `Retry-After` (in seconds or as an HTTP date, at most an hour; without one, 10 s doubling) |
| another `5xx`, the network | the row retries after 1 s, doubling to an hour |

No row is ever dropped for failing; `upload-failed` is recorded once per row and reason.

### 6.3 A large file

1. The file is quiet; its size and time are the **snapshot**, stored in the row.
2. The session is created, and its URL, expiry and `session_next = 0` persisted before the first
   byte. A crash before that leaves an orphan session, which expires on its own.
3. Each fragment is read into one buffer, fed to the hash, sent, and on `202` its progress
   persisted. Memory does not grow with the file.
4. Before the last fragment the worker probes for a writer, compares the file with the snapshot and
   reads the item's eTag again. `If-Match` is checked when a session is created, not when it
   completes; this narrows the window in which an edit made in OneDrive meanwhile is superseded to
   one fragment. Such an edit stays in the item's version history (limitations log F80).
5. The last fragment's answer carries the item; its hash must be the one computed while sending. A
   mismatch is never committed: the content goes up again from zero.
6. After a crash or a dropped connection the session's status says where to go on from, but only
   for the content it was opened for: a file that changed since starts a new session.

A file that changes while it is sent (its size or time moves from the snapshot) is abandoned and
waits again. A file over 250 GB is blocked before any request.

## 7. Conflicts

A local change meets a change in OneDrive on the same item. **A copy** means: the local file is
renamed in its directory to `<stem>-<machine><.ext>` with `RENAME_NOREPLACE` (`-2`, `-3`, … when
taken), stripped of konedrive's attributes, and uploaded as a new file; the cloud's version takes
the original name as a placeholder, and a conflict of kind `copy` is recorded. The stem ends at the
last dot: `Report.docx` → `Report-fedora.docx`, `archive.tar.gz` → `archive.tar-fedora.gz`,
`.bashrc` → `.bashrc-fedora`. `<machine>` is `machine_name` in the account's config, by default the
host name up to its first dot, with any character OneDrive refuses replaced by `-`, at most 32
characters.

| Here \ in OneDrive | edited | renamed or moved | deleted |
|---|---|---|---|
| **edited** | a copy | the upload goes to the renamed item, and the file here takes OneDrive's name | this computer's wins: uploaded again as a new item (`restored`) |
| **renamed or moved** | the rename is sent again with the fresh eTag | the first to reach OneDrive wins: the file here follows OneDrive's place | a downloaded file is uploaded again at its new place; a placeholder follows the delete |
| **deleted** | OneDrive's wins: the delete is dropped and the item comes back as a placeholder (`restored`) | deleted with the fresh eTag | done |

**Both new at one name** (create/create): the same hash is adopted, no copy and nothing sent;
another hash gets a copy. A name that differs only in case is the same name to OneDrive, and gets a
copy. Two new folders of one name merge, their contents meeting file by file under these rules.

**Folders.** A folder deleted here whose cloud copy gained or changed anything meanwhile is deleted
only in part: whatever OneDrive has below it that this computer never saw stays, with its folders.
A folder deleted in OneDrive that holds local work here is kept, with the work, and made again in
OneDrive (§9).

Every copy and every `restored` goes into the activity log; copies are listed by `Conflicts()` and
on the window's Conflicts page with "Show Both". Nothing here deletes a local byte, and nothing
deletes content in OneDrive that this computer has not seen, apart from §6.3's one-fragment window,
which leaves a version.

## 8. Moves out of the folder

### 8.1 What decides

An object missing from the folder is a delete or a move out; the events cannot always tell (an
event can be lost, or the move made while the daemon was not running). The object can: the helper
opens it by its file handle (`OpenByHandle`), and either it is gone (`ESTALE`), or it is somewhere,
and `/proc/self/fd` says where, proved by opening that path again and finding the same inode. A
move out is a delete for OneDrive, done only after the content is on this computer (§8.4).

### 8.2 `OpenByHandle`

The one message the helper gained for uploads. The daemon sends a file handle and the folder's root
descriptor; the helper opens the object with `open_by_handle_at` relative to that descriptor, and
answers its descriptor only if the anchor is the user's directory on the device of one of the
user's folders, and the object is the user's own regular file or directory, on that device, still
linked, carrying `user.konedrive.item-id`. Anything else is `EPERM`; a stale handle or an unlinked
object is `ESTALE`. A file comes back read-only (under its unit the helper, without
`CAP_DAC_OVERRIDE`, cannot open a user's `0644` file for writing), and the daemon reopens it for
writing as its owner. The helper exempts its own opens by its own pid, since opening a placeholder in
a marked directory raises an event it would otherwise wait on itself. Details: [hydration.md](hydration.md)
§10.1, §11; limitations log F90–F92; SECURITY.md.

### 8.3 A `move-out` row

- **Re-marked first.** A placeholder that left sits outside every marked directory and would read
  zeros. Before any row runs, and at every wake, every pending `move-out` object is opened by its
  handle and given `MarkFile` (a directory: `MarkDir` for it and every directory below), paused and
  held rows included; again after the helper comes back (limitations log F120).
- **Anywhere but the Trash**: a placeholder is downloaded where it went, through its own
  descriptor, by the ordinary fill; a folder has every placeholder of the item inside it
  downloaded. Only when each reads `hydrated`, the row is still the item's newest, and the object is
  still proved to be outside the folder, is a marker stored in the row; then konedrive's attributes
  come off (the item id first), the directories are unmarked, and the item is deleted in OneDrive
  as any delete is. While it waits (offline, say) nothing is lost either way: the file stays a
  marked placeholder, the item stays in OneDrive.
- **In the Trash** (the user's own, or a `.Trash-<uid>` or sticky `.Trash/<uid>` at the top of a
  mount, with the entry's `.trashinfo`), nothing is downloaded: a placeholder is removed with its
  `.trashinfo`, a downloaded file stays as the user's, and the item goes to OneDrive's recycle bin.
  A folder sent to the Trash loses its placeholders and keeps what was downloaded. This is what
  Dolphin's Delete key does, and what Windows does.
- **Doubt keeps the row**, and the item in OneDrive: `EPERM`, no helper, a download that stopped, a
  place that cannot be proved, the object back in the folder, anything a moved-out folder held that
  is alive but unreachable. `ESTALE` is the user's delete only with the evidence of §4.2, rule 7,
  the place being where the row last reached the object; for the row's own object it must also say
  so twice, 5 s apart.
- **Fills of moved-out objects** are routed by the item id they carry to the account whose row
  names them, before device and path, so a placeholder moved from one account's folder into
  another's is filled by the first (F122).
- **Into another account's folder**, the move is a move out of the first account and, if the
  second is read-write, a new file there. The second never removes an object whose id its tree
  does not know while another account claims it: read-only, it sets it aside, alive, in its rescue
  directory, where the first account's move out downloads it. The first never deletes on `ESTALE`,
  or on `EPERM` with the object still standing there stripped, an object last proved to be inside
  another account's folder: the row goes, and the item is placed again (F124).

### 8.4 Content first

Nothing is deleted in OneDrive while its content exists only there and the user still holds the
file somewhere. A cloud-only folder moved out of the folder is therefore downloaded in full, with
no prompt, as on Windows ([decisions.md](decisions.md), "A move out of the folder downloads first";
limitations log F121).

### 8.5 Restore

`RestoreDeletes` of a held move-out places the item again in the folder and tidies what left: a
placeholder outside is removed, a downloaded file stripped, the item's directories unmarked,
stripped and removed if empty. Every other drop of `move-out` rows tidies the same way: a switch
to read-only, a Forget, a Remove (F123).

## 9. Reconcile in read-write mode

A read-only folder's cycle is [sync.md](sync.md)'s, unchanged. A read-write folder's keeps the
user's changes.

**The cycle.** The first cycle after bring-up waits for the watcher's Full local scan, so that
changes made while the daemon was down are rows before anything from OneDrive is applied. Each
cycle holds the folder's tree lock from staging to its swap, the lock every outbox commit and every
examination holds, so none of them sees another's changes half made. The outbox, in turn, waits
for a cycle at its start and whenever the network comes back, so the base catches up with OneDrive
before any guard is sent (limitations log F117).

**The stale-delta guard.** A delta fetched before an outbox commit can carry an older version of
the item the commit wrote. The cycle records `outbox_seq` when its fetch begins; an entry about an
item the outbox committed or deleted since (`items.local_seq`, and the tombstones in `outbox_gone`)
is trusted only if it is the commit itself (the same eTag), and otherwise read again with
`GET /items/{id}` under the lock. Reading again rather than dropping keeps a change OneDrive made
just after the commit, which the delta cursor would not send twice. This is also what makes echo
harmless: the delta that reports the daemon's own upload finds the base already as it says, and
nothing is done (limitations log F113).

**What the reconcile leaves.** An item with an outbox row, in any state, and everything under a
folder a `move` row moves, is never moved, replaced or removed; a local move not examined yet is
left where it is. OneDrive's change to such an item waits in the store (`deferred`), staged again
every cycle until the disk takes it or an outbox commit supersedes it; below a pending `delete` or
`move-out` the change goes into the base and nothing is placed (F112). An item OneDrive moves into
a folder deleted here waits with it. A Changed reconcile does not turn Full over a disagreement
that a row explains. OneDrive's moves still go through the holding directory, but whatever sits
there is placed from it, removed as below if OneDrive removed it, or put back into the folder; it
never leaves the folder, where it would be taken for a move out (F117).

**Where the read phase rescued, a copy.** A file in the way of an item OneDrive brings or changed
is renamed to a conflict copy in place (§7) and handed to the examination, which uploads it: the
daemon's own renames raise no event the watcher keeps, so after its swap the cycle hands the
watcher whatever it kept, copied or stripped. A folder of the user's at the name of a folder
OneDrive brings merges with it rather than being copied. An unmanaged file at the name of an item
OneDrive did not change (a save by rename not examined yet) is left alone. A missing item is placed
again only with something to place: new in OneDrive, a file whose content changed there, or an item
with no local object on record (and the folders above such an item); otherwise its absence is a
delete or a move not examined yet (F114, F115).

**What OneDrive removed** is removed in place, not through the holding directory: a placeholder
goes; a downloaded file only under a write lease; a changed file stays, stripped, and goes up
again; a folder that holds local work stays, and is made again in OneDrive as a new item, while one
that keeps only what is not local work (a file in use, an ignored name) waits (F116). An object
whose item id the base does not know is never removed: it may be another account's.

**Replacements** of a downloaded file run under a write lease on the old file, taken before the
file is checked and granted only while nobody has it open: a writer is not left writing into an
unlinked inode. A refused lease leaves the old version for a later cycle (F110).

**`410`.** `resyncChangesApplyDifferences` (and a `410` naming neither variant) lists the drive
afresh and reconciles Full, local changes still going to the outbox. `resyncChangesUploadDifferences`
keeps what the new listing left out: downloaded files go up again as new, while placeholders, which
hold nothing here, are removed and logged; a downloaded file whose version differs from the
listing's is kept beside it as a conflict copy (§7) (F111).

## 10. A crash at each step

Every step can be replayed after a crash and reaches the same end; "did the last request land?"
is answered by content hash or by place, never by guessing.

| Crashed | The next start finds | It does |
|---|---|---|
| after the event, before the row | nothing in the outbox; the change on disk | the bring-up's Full local scan finds it (all but an edit that kept size and time, §4.6) |
| a small upload sent, no answer | the row `running` | replays: a create meets `409`, an update `412`; the same hash is adopted |
| a session created, not persisted | nothing about it | a new session; the orphan expires |
| mid-session | `session_url`, `session_next` | the session's status, then on |
| the last fragment sent, no answer | `session_url` | the status answers `404`: the item's hash equal, adopted |
| the answer received, the commit's first step partial | some attributes written, the row `running` | replays, meets `409`/`412`, adopts, commits again |
| the first step done, the second not | the file with the new cTag and id, the store with the old base | replays, `412`, the same hash: adopted, the store's step runs |
| a `PATCH` sent, no answer | the row `running` | replays, `412`: the item is where the row wanted it, adopted |
| a `DELETE` sent, no answer | the row `running` | replays, `404`: done |
| a `mkdir` sent, no answer | the row `running` | replays, `409`: a folder, adopted |
| a conflict copy | the one rename done or not | done: the copy is a new file, `create`; not done: the check runs again |
| a move out part-way | the row with its handle | re-marked, the download resumed; the marker says whether the strip was the row's own |
| the store lost | a rebuilt base, no outbox | a listing and a Full local scan: creates, edits and moves are found again; deletes are not, and those items come back |
| the helper restarted | its marks gone | moved-out objects re-marked, failed `MarkDir`s asked again, then a Full local scan |

## 11. On the bus and the command line

Per account, on `org.konedrive.Sync1`: `Outbox`, `Pause`/`Resume`, `SetIgnorePatterns`,
`ConfirmDeletes`/`RestoreDeletes`, `NotUploaded`; the properties `PendingCount`, `PendingBytes`,
`BlockedCount`, `HeldCount`, `Uploads`, `Paused`, `PausedUntil`, `IgnorePatterns`, `MachineName`;
the activity kinds `uploaded`, `cloud-moved`, `cloud-deleted`, `upload-failed` and `restored`; the
error `NotUploaded`, which "Free up space" gets for a file with changes not uploaded yet. On
`Account1`: `SetMode` and `Mode`. [desktop.md](desktop.md) has each member, the commands and the
window's pages.

**Pause** stops the account's outbox, its poll (so no cycle and no replacement) and its thumbnails;
fills on open, `Hydrate` and the watcher go on, so rows keep collecting. It is kept in the tree
store, so it outlasts a restart, and a timed pause ends by itself. The tray's "Pause Syncing" pauses
every account.

## 12. Testing

- **The Graph client** against wiremock: every request of §6.1, its guard, and each answer of §6.2.
- **The outbox worker** end to end against a stateful fake OneDrive on wiremock: every crash row of
  §10 through fault points, every cell of §7, create/create, the partial folder delete, swaps and
  circles, throttling, offline, pause and the blocked states.
- **The examination** in temporary directories: every rule of §4.2, save by rename in the shapes
  real editors use (vim, Kate, LibreOffice, GNOME's `.goutputstream`), copies, hard links, the
  ignore list, the mass-delete guard.
- **The watcher**, as an unprivileged group on the host: `FAN_RENAME` pairing, own events, an
  overflow, a new directory's mark-then-scan, the degraded mode, the root moved.
- **The reconcile in read-write mode** on the fake OneDrive: the tree lock, the stale-delta guard
  both ways, echoes of the outbox's own create, edit and delete, both `410` variants, a replacement
  under a lease.
- **The VM** (`tests/vm/run.sh quick`), for what involves the helper: a directory made and at once
  given a placeholder, a tree moved in, `OpenByHandle` and its refusals, moves out of a file and a
  folder and into the Trash, a download that stops part-way, a helper restart.
- **The test account**, once, through the guarded harness below: what the service does that no mock
  can say (§13).

### 12.1 Running against the test account

`konedrive-write-test` (`tests/write-account/`) checks, against OneDrive itself, what §13 lists.
It writes, so it runs only against a separate test account, by hand, and **refuses to start unless
every guard holds**:

1. `--graph-test-drive` is the drive both tokens reach (`GET /me/drive`), and is listed in
   `write_test_drive_ids` in the `config.toml` given with `--daemon-config`;
2. the drive looks like a test account: less than 1 GiB in use and fewer than 1000 items;
3. every write stays in `/konedrive-write-test/<run id>/`, which the run makes and puts into the
   recycle bin at the end. Every request goes through a proxy on `127.0.0.1` whose guard asserts,
   before it sends anything, that the item the request names, or the parent of what it makes, lies
   inside that folder; a request it refuses never leaves the machine, and after a refusal only the
   cleanup is sent. Reads may go anywhere: the preflight counts the drive's items;
4. at most 64 MiB per file, 200 MiB of content and 500 requests per run;
5. the write token comes from `konedrivectl dev export-access-token --read-write`, which the daemon
   hands out only for a drive on the same list, and both token files must be `0600` and the user's.

Every flag is required; without one it does not start. It needs no root and no VM, and touches
nothing but the two token files, the `config.toml` it is given, and the network.

Once, before the first run: add the test account and sign it in as the test Microsoft account; its
drive id is then the `drive_id` of its section in `~/.config/konedrive/config.toml`. Add that id by
hand to `write_test_drive_ids = ["<id>"]` at the top of the same file. Each run, within the hour an
access token lasts:

```
konedrivectl --account Test account mode read-write        # a sign-in for Files.ReadWrite
konedrivectl --account Test dev export-access-token --read-write --out /tmp/kd-rw.token
konedrivectl --account Test account mode read-only         # the switch back
konedrivectl --account Test dev export-access-token --out /tmp/kd-ro.token
cargo run -p konedrive-write-test -- --graph-test-drive <id> \
    --graph-token /tmp/kd-rw.token --graph-read-only-token /tmp/kd-ro.token \
    --daemon-config ~/.config/konedrive/config.toml
rm /tmp/kd-rw.token /tmp/kd-ro.token
```

The second export is the check of the switch back: it is the token of the refresh that followed
it, and the daemon hands it out only if Microsoft answered with no write scope (a refusal saying
"Microsoft answered a request for a read-only token with one valid for …" is that check failing,
limitations log F66). The harness then shows that OneDrive refuses a write made with it.

Each check prints `PASS`, `FAIL` or `LOOK` (done, but to be confirmed by hand: the recycle bin, whose
restore Graph allows only with `Files.ReadWrite.All`). The exit status is 0 when nothing failed, 1
when a check failed, 2 when a guard refused: `REFUSED` before anything was written, `STOPPED` when
the guard refused a request mid-run (the run folder still goes to the recycle bin). Record what it
reports in §13.

## 13. What is assumed of OneDrive

Verified against Microsoft's documentation: the session protocol, `If-Match` on sessions, `PATCH`
and `DELETE`, `conflictBehavior` on folders and sessions, the delta feed's rules, throttling, and
the refresh's "equivalent to or a subset of" the scopes first granted. **Assumed** until the
test-account run (§12.1), each handled safely either way:

- `If-Match` is honoured on the `PUT` that empties a file (otherwise an emptying could overwrite an
  edit made meanwhile, which version history keeps);
- `conflictBehavior=fail` works in a `PUT`'s URL;
- a folder's cTag changes with anything inside it and guards the folder's delete;
- a rename or move onto a taken name is refused `409`;
- names collide without regard to case;
- the delta feed returns the daemon's own changes with the eTags their writes were answered with;
- 10 MiB fragments, and a resume from the session's status, work as documented;
- a delete goes to the recycle bin;
- an edit made in OneDrive during a session is superseded by its last fragment (§6.3);
- a read-only refresh after a switch back answers a token that cannot write.

## 14. Known limits

Recorded in [`../limitations-and-workarounds.md`](../limitations-and-workarounds.md):

- the gate (F60), the mode following the grant (F61), the lock walks of a switch (F62), the
  switch's outcome (F64), file modes lost across a round trip (F65), consent that stays with
  Microsoft (F66);
- an edit that kept size and time while nothing watched is missed (F52); copies and moves told
  apart by the recorded handle (F53); deletes decided only on the helper's word (F54); the
  examination's shortcuts (F55);
- the watcher: overflows (F70), the mark budget (F71), other devices (F72), writes through a hard
  link outside (F73), its shortcuts (F74), a size change by path (F75); placeholders moved into a
  brand-new directory (Z2);
- the one-fragment window (F80), files open for writing (F81), the worker's shortcuts (F82);
- `OpenByHandle` (F90–F92), moved-out objects and their windows (Z3, F120–F124);
- the outbox on the bus (F100–F102);
- the reconcile in read-write mode (F110–F117);
- the test-account harness, and what stays assumed until it runs (F130, F131).

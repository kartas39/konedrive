# Uploads: changes made on this computer

How a read-write account's folder sends what is changed in it back to OneDrive: the mode that allows
it, noticing local changes, deciding what each one is, the outbox and its worker, the requests and
their guards, conflicts, objects moved out of the folder, and what the reconcile does differently.
How a placeholder is filled is in [hydration.md](hydration.md); how the folder follows OneDrive is
in [sync.md](sync.md); the mode itself is in [accounts.md](accounts.md) §10.

**Uploading is the user's choice, per account.** Every account starts read-only, as
[sync.md](sync.md) describes, and any signed-in account can be switched to read-write (§2.2). No
list decides it (§2.3).

**What always holds.**

- **WR1 — only local content is uploaded.** Content is read only from a file whose state is
  `hydrated` or that carries no konedrive state; the bytes of an `online-only`, `hydrating` or
  `dehydrating` file are not the item's.
- **WR2 — every write is guarded, and a failed guard is resolved by reading again.** Changes carry
  `If-Match`, creates `conflictBehavior=fail`; a `412` or `409` leads to a fresh read and a decision
  (§6.2, §7), never to a retry without the guard — except a folder's delete (§6.1), sent with no
  guard.
- **WR3 — no local byte is lost.** Where the read phase would rescue a file, a read-write folder
  makes a conflict copy in the folder and uploads it; a change from OneDrive removes a local file
  only if it holds nothing only this computer has.
- **WR4 — the disk is the truth about local changes.** The outbox records intent and progress; the
  examination can rebuild it from the disk and the base. A rebuilt base never deletes anything in
  OneDrive.
- **WR5 — nothing is deleted in OneDrive while its content exists only there and the user still
  holds the file.** A placeholder moved out of the folder is downloaded before its item is deleted
  (§8).
- **WR6 — no echo.** The daemon's own local changes never become outbox rows, and its own changes in
  OneDrive never come back as changes from OneDrive (§3.2, §9).
- **WR7 — every outbox step can be replayed.** After a crash it reaches the same end; "did it land?"
  is answered by content hash or by place, never by guessing (§10).

## 1. What the user sees

- **An account is read-only until it is switched.** "Upload changes made on this computer" on the
  Account page, or `konedrivectl account mode read-write`, signs in again, in the browser, for
  permission to change files. Then the folder loses its read-only lock and what is changed in it
  goes up.
- **Anything that changes the folder counts**: a program saving a file, a new folder, a rename, a
  move within the folder, a delete. A change goes up once nothing in the folder has changed for 2
  seconds (after 30 s at the latest) and nobody has the file open for writing; a file kept open for
  writing (a database, a log) waits until it is closed.
- **Dolphin** shows the sync emblem on a file waiting to go up or going up, and the error emblem on
  one that cannot go up. The window shows what waits (Status, Activity), and a Plasma job shows an
  upload that takes two seconds or more.
- **Changed on both sides**, both versions are kept, as on Windows: the cloud's version keeps the
  name, and this computer's goes up beside it as `Report-<machine>.docx`, listed under Conflicts.
- **Names OneDrive refuses** (`a:b.txt`, a trailing space, `CON`, …) are not renamed for the user:
  they are listed under "Not Uploaded" until the user renames them. So are symbolic links, FIFOs,
  sockets and devices, hard links, and files on another filesystem mounted inside the folder.
  Editors' temporary files (the ignore list) stay local, unlisted.
- **Moving a file out of the folder** removes it from OneDrive (to its recycle bin), but only once
  its content is on this computer: a file that was not downloaded is downloaded where it went first.
  Sending it to the desktop Trash removes it without a download.
- **A large delete** (more than 500 items, or 10 and more that are over a fifth of the folder) is
  held back and asked about: "Delete in OneDrive Too" or "Restore Them".
- **Pause** stops uploads (and asking OneDrive for changes) until resumed or for a time; opening a
  file still downloads it. **Offline**, changes collect; ten saves of one document are one upload
  when the network is back.
- **Back to read-only** is refused while changes wait, unless the user chooses to drop them; the
  files stay as they are, and the folder is locked again.

Every one of these has a command: `sync outbox`, `sync pause`/`resume`, `sync ignore`,
`sync not-uploaded`, `sync deletes confirm|restore`, `account mode` ([desktop.md](desktop.md) §3).

## 2. The mode

### 2.1 Read-only and read-write

The mode is per account, in `config.toml`, a new account read-only; `Account.Mode` publishes the
mode the account *runs* in ([accounts.md](accounts.md) §10). The scope follows the mode it runs in:
`Files.Read User.Read offline_access` for read-only, asked for at every refresh, so that Microsoft
refuses a read-only account's writes whatever it was once granted;
`Files.ReadWrite User.Read offline_access` for read-write.

### 2.2 The switch

**To read-write**: a sign-in for `Files.ReadWrite`, pinned to the account (its password asked for
again, its email filled in), refused `NotSignedIn` for an account that is signed out. An account
with no drive recorded is asked for it first (`GET /me/drive`), and refused before any sign-in URL
if none can be recorded. Only a token response that grants it, for this account's own drive, writes
the mode. Then the folder's sync restarts in read-write mode, in this order:

1. the watcher starts and walks the folder, giving every directory the helper's mark and its own
   (§3);
2. only once that walk has ended does the read-only lock come off (files `0644`, directories
   `0755`). A directory the walk could not mark (§3.5) is asked for again later. A folder whose
   watcher cannot start, or ends by itself, stays locked, runs as a read-only one and says why in
   `LastError`;
3. the walk asks for a Full local scan, which finds anything forced past the old lock (§4.6);
4. the first cycle waits for the watcher's first batch to be examined, and the outbox worker for the
   first cycle, before it sends anything (§9).

**To read-only**: refused `PendingUploads` while rows wait, unless forced; what the watcher still
holds is examined first (for up to 30 s). Forced, the rows are dropped, their upload sessions
cancelled (§6.1) and what move-outs left outside tidied (§8.5); the files stay, as ordinary local
changes the read phase's stamp check protects. A row half-way through a temporary name (§5.3) is not
dropped. The watcher and the worker stop, the lock walk runs, and the next refresh asks for
`Files.Read`. No sign-in.

Any other way to read-only — a sign-out, an expired sign-in, the write gate or `config.toml`, a
narrower grant — keeps the rows: the folder is locked, and its sync runs no cycle while they wait,
so the read phase's reconcile never puts back what they describe; they go once the account is
read-write again (limitations log F140). A Forget and removing the account are refused
`PendingUploads` while rows wait.

### 2.3 The write gate

The outbox worker asks the write gate before each row and between an upload's fragments. It is open
only while the folder and the account run read-write, `config.toml`, read again, says read-write and
records a drive for the account, the token carries `Files.ReadWrite` and was last seen to reach that
drive, and the folder's sync is not stopped. Closed, nothing more is sent, the rows wait, and
`LastError` says why. A `mode` set to read-write by hand counts once the token grants it for the
recorded drive. No list of accounts decides the mode: `write_test_drive_ids`, empty by default and
written by nothing in konedrive, only makes `TokenExport.ReadWrite` refuse `WritesNotAllowed` for a
drive not on it (§12.1; [decisions.md](decisions.md), "The mode, and the write gate").

## 3. Noticing local changes: the watcher

### 3.1 A second fanotify group, in the daemon

The helper's permission group cannot report directory entries: the kernel refuses file-handle
reporting for a pre-content group. So the daemon holds a second group of its own, unprivileged, for
each read-write folder, marking the same directories:

```text
fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME_TARGET | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY | O_CLOEXEC | O_LARGEFILE)
every directory:  FAN_CREATE | FAN_DELETE | FAN_RENAME | FAN_CLOSE_WRITE | FAN_ATTRIB | FAN_ONDIR | FAN_EVENT_ON_CHILD
the root also:    FAN_DELETE_SELF | FAN_MOVE_SELF
```

`FAN_RENAME` carries both sides of a move in one event, with the moved object's handle; a side that
is missing means the object came from, or went to, a directory nobody watches. `FAN_MODIFY` is not
subscribed: a write is looked at when it is closed. Each was measured on 7.2:
[`../kernel-behavior-7.2/notification.md`](../kernel-behavior-7.2/notification.md) §14.

An unprivileged group has the kernel's limits: inode marks only, a queue of 16 384 events, and a
budget of marks per user. A full queue is reported (`FAN_Q_OVERFLOW`) and costs a Full local scan
and a walk that marks whatever was missed, never a missed change. A mark the kernel refuses (the
budget spent, no group to be had) turns the watcher *degraded*: a Full scan and a walk every 10
minutes besides the events, and `LastError` says so.

### 3.2 The daemon's own changes

To an unprivileged listener the kernel reports the pid of an event only when the listener caused it.
Events carrying the daemon's own pid are dropped: placeholders placed, the reconcile's renames,
conflict copies, attribute writes and the commit of a fill (the data a fill writes through the
helper's descriptor raises no event at all). A directory the daemon made is still followed in the
watcher's map, marked, and its tree examined once.

### 3.3 The directory map

An unprivileged process cannot open a file handle, so the watcher keeps a map from each directory's
handle to its parent and name, built by its walk and kept current from `FAN_ONDIR` events. An
event's directory handle becomes a path through it when the batch is handed over. A directory the
watcher opens itself is opened beneath the root (`openat2`,
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`) and its handle compared. A handle
the map does not know asks for a walk, at most once a minute.

### 3.4 Batches

Each event makes the name it is about, and by its handle the object, dirty; `FAN_CLOSE_WRITE` also
asks for the content to be hashed. Dirt is kept by handle and turned into paths only when it is
handed over, so a directory renamed meanwhile is examined where it now is. A batch is handed over
when no event arrived in the folder for **2 s**, and at the latest **30 s** after its first event.
The examination runs on a thread of its own, so a long Full scan never holds up the queue's reader.
A batch whose examination fails is offered again after 5 s, doubling up to 10 minutes.

### 3.5 A new directory

A directory made or moved into the folder by anyone but the daemon is, in this order: sent to the
helper's `MarkDir` (the permission mark, M1), given the watcher's own mark, then listed, every
subdirectory going through the same steps before its contents are looked at. Whatever was put in it
before the mark is found by the listing. A `MarkDir` that fails is asked again every 60 s and when
the helper is back, and `LastError` counts the directories not yet protected. The window in which a
placeholder moved into a brand-new directory could be opened unintercepted is the event's latency
plus one `MarkDir` (limitations log Z2).

### 3.6 Other devices

A directory on another device than the folder's root — a nested Btrfs subvolume, a filesystem
mounted inside the folder — is neither watched nor uploaded: the helper cannot mark it, and so
nothing in it could be protected. It is listed under "Not Uploaded" as `other-device`, and
`LastError` counts such places (limitations log F72).

### 3.7 The root itself

`FAN_DELETE_SELF` or `FAN_MOVE_SELF` on the root, or an examination that finds the root gone, stops
the folder's sync and outbox and sets `Folder.State` to `error`. Nothing is deleted in OneDrive
because the folder went away.

## 4. The examination: from a batch to rows

### 4.1 Three sides

The tree store's `items` table is the **base**: the last state the folder and OneDrive agreed on.
The disk is **local**, the delta feed **remote**. A local change is local ≠ base. An item is known
on disk by its `user.konedrive.item-id` attribute, never by its path: a rename is an item id seen
under another name, and an editor's save by rename is a new inode taking over an id. Where two
inodes carry one id (a copy that kept the attributes), the one whose file handle the base recorded
(`items.local_handle`: taken when the item was placed, adopted or committed, and by the examination
when it finds the item's entry under another handle) is the item (limitations log F53).

Nothing is examined before the folder's first listing has completed: there is no base yet.

### 4.2 The rules

Each dirty directory is listed, and each entry read by name (`lstat`, `lgetxattr`), never opened.

1. The daemon's own working names (`.konedrive-holding`, `.konedrive-new-*`) are skipped; any other
   name with the `.konedrive-` prefix is listed (`reserved-name`).
2. Not a regular file or a directory: never uploaded, never followed, and listed (`symlink`, `fifo`,
   `socket`, `device`) unless its name is on the ignore list. On another device: listed
   (`other-device`, §3.6).
3. A name on the ignore list without an item id stays local, unlisted. The list is per account
   (`ignore` in `config.toml`, `konedrivectl sync ignore`, the Account page): shell globs matching
   the name, case-sensitively, by default editors' swap, lock and backup files, `~$*`, `*.part`,
   `*.crdownload`, `*.tmp` and `.goutputstream-*`. A directory whose name is ignored keeps
   everything under it local. Every change of the list runs a Full local scan.
4. A directory without an item id: a `mkdir` row, and its contents are examined.
5. A file without an item id: a `create` row — in `waiting` (`open-for-writing`) while someone has
   it open for writing.
6. An entry with an item id: where the base has it, the content check (§4.3); elsewhere in the
   folder, a `move` row and the content check. An id the base does not know (a file from another
   account's folder, or one deleted since), and a second inode carrying an id (a copy), is stripped
   of konedrive's attributes and created if it is downloaded or a directory; not downloaded, it is
   left and listed (`not-downloaded`), or removed if it is an empty copy of an item whose own object
   the same run saw. A second name of the same inode, a hard link OneDrive cannot hold, is listed
   (`hard-link`), and so is a downloaded file from elsewhere with other links.
7. A base item missing from its place and not found elsewhere in the batch:
   - a new file at its name (**save by rename**: a temporary file renamed over the original, or the
     original renamed to a backup and a new one written) is an `update` of the item from the new
     inode, so the item keeps its id, its version history and its sharing links;
   - an item with no recorded handle (a rebuilt base, restored deletes) is never asked after: it
     cannot be proved gone, and no row is made;
   - otherwise the helper is asked whether the object still exists, by the file handle the base
     recorded (`OpenByHandle`, §8). **Gone** (`ESTALE`) is a `delete`; **alive outside the folder**
     is a `move-out`; any other answer, or no helper, decides nothing and the item is looked at
     again 30 s later. `ESTALE` is a delete only with its evidence: nothing, or another object,
     stands where the item was last proved to be, and the store's handles were taken on the
     filesystem the folder is on now (`meta.handles_root`: the root directory's handle and, where
     the kernel gives one, the filesystem's UUID). When that record changes (the folder moved to a
     new disk, a snapshot rolled back), every handle is forgotten and a Full local scan takes them
     again; an item missing then is not deleted (limitations log F54, F121).

A folder's removal waits for everything the base has inside it to be accounted for: an item moved
out of it first is a `move-out` of its own, and an item that cannot be found holds the folder back
(`local/examine.rs`).

### 4.3 The content check

| The file | Decision |
|---|---|
| `hydrating` or `dehydrating` | busy: looked at again in 30 s |
| `online-only`, size as in the base | unchanged |
| `online-only`, another size | a `truncate(2)` by path, which opens nothing and fills nothing: the size and time are put back through the daemon's own descriptor, and nothing is uploaded (limitations log F75) |
| `hydrated`, same size and time as its stamp, no `FAN_CLOSE_WRITE` seen | unchanged; the file is not opened |
| `hydrated`, may have changed, a read lease refused | someone has it open for writing: an `update` row in `waiting` (`open-for-writing`), looked at again in 30 s |
| `hydrated`, size ≠ stamp | changed: `update` |
| `hydrated`, same size, another time, or a `FAN_CLOSE_WRITE` seen | hashed (quickXorHash, one read): another hash is an `update`; the same hash only refreshes the stamp, so a `touch` uploads nothing. Without a hash in the base, or with a cTag on the file that is not the base's, it is an `update` unhashed |
| an item id and no state, or a state no version writes | left alone and listed (`state-unreadable`) until the state can be read or the file is replaced; so is a copy with such marks |
| may not be read by name, or is refused at an open, a strip or a read | not examined: no row, and nothing at or below it counts as missing; listed (`unreadable`) and looked at again, after 5 s doubling up to 10 minutes |

The read lease (`F_RDLCK` on a read-only descriptor) is taken and released at once.

### 4.4 The name pre-check

A name OneDrive refuses is blocked before a request is spent on it: one of `" * : < > ? \ |`, a
leading or trailing space, a reserved name (`.lock`, `CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`,
`LPT0`–`LPT9`, `desktop.ini`, `_vti_` anywhere, a `~$` prefix), a name that is not UTF-8, and a new
file over 250 GiB. An item's name is checked only when it differs from the base's. The check blocks
only what Microsoft's page names; whatever it misses is refused by the service (`400`), and blocked
with the service's own message. Nothing is ever substituted.

### 4.5 The mass-delete guard

A batch or a scan whose removals (`delete` and `move-out` rows, a folder counted with everything the
base has inside it, those already waiting included) come to more than **500** items, or to at least
**10** that are more than **20 %** of the items the base has placed, holds every one of them as
`held`. `HeldCount` counts them; the window asks, and so does a notification. `ConfirmDeletes`
releases them all, and a removal once confirmed is not held again; `RestoreDeletes` drops them,
forgets the recorded objects of what they named and runs a Full reconcile, which places the items
again from OneDrive (§8.5). The numbers are provisional.

### 4.6 The Full local scan

The same examination over every directory: at bring-up, after an overflow, after the helper comes
back, when the ignore list changes, every 10 minutes while the watcher is degraded, and when the
record of the filesystem changed (§4.2). It finds what events would have shown, with one exception:
an edit made while the daemon was not running that kept both the size and the time.

**How it goes is published** in `org.konedrive.LocalScan` ([desktop.md](desktop.md) §2.4), at most
once a second: the reason (`start`, `read-write` after a switch, `overflow`, `helper-back`,
`ignore-list`, `periodic`), what was seen so far, and at the end when it finished and how long it
took. `Expected` is the count of placed items the folder last published, so nothing shows a
percentage. A read-only folder, which has no watcher, reads `none`.

## 5. The outbox

### 5.1 In the tree store

The outbox came with schema 3; the store's schema is 8 ([sync.md](sync.md) §5.3). Its tables beside
the read phase's:

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
  snapshot_size INTEGER,                  -- the content being sent: its size,
  snapshot_mtime INTEGER, snapshot_mtime_nsec INTEGER,  -- and its time (seconds, nanoseconds)
  moved_out TEXT,                         -- a move-out row's marker: 'local' | 'trash'
  session_url TEXT, session_expires INTEGER, session_next INTEGER,
  handle BLOB,                            -- the object's file handle
  confirmed INTEGER NOT NULL DEFAULT 0,   -- a removal the user confirmed
  size INTEGER,                           -- the file's size when the change was detected
  bad_item TEXT, bad_item_ctag TEXT, bad_item_etag TEXT);  -- what a bad upload left (§6)
CREATE TABLE local_skipped (rel TEXT PRIMARY KEY, reason TEXT NOT NULL, at INTEGER NOT NULL, size INTEGER);
-- outbox_gone (id, local_seq), deferred (an item's row as OneDrive has it, + seq, gone, waits): the reconcile's (§9)
-- upload_sessions (url, parent, name, opened): the sessions open until completed or cancelled (§6.1)
-- upload_openings (seq, parent, name, at, last), upload_openings_left: sessions about to be opened (§6.1)
-- meta: + outbox_seq, handles_root (paused_until: an older version's pause, moved to config.toml, §11)
```

The store is `0600`, and an upload URL, a credential for its one file until it expires, is kept
nowhere else and never logged. **The disk is the truth about local changes; the outbox records
intent and progress.** Every row can be found again by comparing the disk with the base, so losing
the outbox or the whole store costs a listing and restarted uploads, never data. The worst case is a
forgotten pending delete: the file comes back from OneDrive.

### 5.2 One live row per item

A new detection merges into the item's row. A detection is never merged into a row being sent: one
follow-up row waits behind it, and the commit rebases it (a new eTag, or the item id behind a
create).

| Row + detection | Result |
|---|---|
| `create` + `update` or `move` | `create`, of the newest content at the newest place |
| `create` + `delete` or `move-out` | nothing: the row goes |
| `mkdir` + `delete` or `move-out` | nothing; the examination removes the rows inside it that have no item id |
| `update` + `move` | one `update` row with the new place: the move is sent first, then the content. With the content back as the base has it, a plain `move` |
| `move` + `update` | `update`, at the move's place |
| `update` or `move` + `delete` or `move-out` | the removal, against the update's base |
| `move` + `move` | one move to the final place |
| any row + the item back where and as the base has it | nothing: the row goes |
| `delete` + a new file at the name | `update` (save by rename) |

**States**: `waiting` (the file is open for writing, its content is not local yet, or the worker
stopped between fragments) → `ready` → `running` → gone at the commit; or `retry` (with `next_try`),
`blocked` (needs the user) or `held` (§4.5). A row waiting for space stays `ready` (§6.4).

**An object gone before it landed.** A row is bound to its local object, not to its name. A `create`
or `mkdir` whose object is under none of the names its rows saw ends on that run, with no retry. Its
upload session is cancelled, and it leaves the outbox with the rows behind it of the same object
that never got an item id; the activity log records it once as `not-uploaded`. Only a `create` whose
last request may have gone out — the last fragment, the only one of a file up to 10 MiB — looks its
name up in the parent first: an item there that nothing here knows and that is its own, by size and
time, goes to the recycle bin (issue #231). An `update` whose file is gone ends the same way, with
no event: the version OneDrive has stays until a removal of the item deletes it. A `delete` with no
item id leaves with no request.

### 5.3 Order

Rows run in `seq` order under four rules:

1. a row waits for every earlier row on the same item or the same local object;
2. a row that is not a removal waits for the `mkdir` of the directory it is in, whose item id it
   needs;
3. a folder's `delete` or `move-out` waits for every row of what the base has inside it, whatever
   their order;
4. a row that takes a name in OneDrive (a `mkdir`, a `create`, a move's target) waits for a row that
   frees that name, names compared without regard to case.

Rule 4 alone can close a circle (`mv d t/ && mv t d`); inside such a circle its waits are dropped. A
row that then meets its name still taken (`409`), by an item a live row is freeing, takes it through
a temporary name, `.konedrive-swap-<item id>`, saved in the row before the request, and a final
`move` row follows. Swaps (`a` ↔ `b`) and renames that change only case go the same way.

**How the next rows are picked.** The worker never reads the whole queue for a step. It reads the
rows that are due, in `seq` order, a hundred at a time, and asks each one's waits by point queries
on the outbox's indexes. Portions are read until enough runnable rows are found or the queue ends,
so a portion in which every row waits (a thousand files behind the `mkdir` of their folder) never
stops the worker.

**The invariant.** When nothing in the outbox can run, it is because something runs, something waits
for a time (`retry`, `waiting`), or something waits for the user (held deletes, a refused name, a
full OneDrive, a move-out waiting for the helper). A pick that finds due rows and none of these has
the worker report them as stalled, in the log (`upload/engine.rs`).

Metadata rows (`mkdir`, `move`, `delete`) run one at a time, `move-out` rows one at a time beside
them, both in the metadata slots of the account's transfer pool, which go before transfers
([hydration.md](hydration.md) §6.4). Content rows run beside them, each in any free slot; a file of
100 MiB and up also waits for the pool's large-file limit, shared with downloads, and lets the small
rows behind it go meanwhile.

### 5.4 The commit

After a successful answer, under the folder's tree lock (the lock a cycle holds from staging to its
swap, §9) and, for a file, its per-inode lock:

1. **On the file**, through its own descriptor, never by path: the stamp from the snapshot, the
   cTag, `state=hydrated`, `fsync`, then the item id and a second `fsync`. An item id without a
   state is the one combination the helper refuses with `EIO`; a state without an id is simply an
   ordinary file. A file freed up meanwhile takes only the cTag and the id, and stays a placeholder
   of the version just sent.
2. **In the store**, one transaction: the base row from Graph's answer (with `local_handle`),
   `local_seq = ++outbox_seq`, the row deleted, and the activity event. An answer the folder cannot
   hold (the item was renamed in OneDrive to a name over 255 bytes, or moved under a folder that is
   not placed, while its content went up) never takes a placed item's place away: the base keeps the
   place the disk has, and the answer waits as the item's deferred change (§9).

The descriptor is the one the content was read from, and a folder's is opened before its `mkdir`:
the item is the object that was sent, wherever it went during the request, and `local_handle` names
it. The examination then decides what happened since, as if it came just after the commit; an item
committed with no object could never be proved gone (limitations log F54).

A crash between the two is replayed (§10). While a `create`, `update` or `move` row waits, its file
carries `user.konedrive.sync` (`pending`; `uploading` while its content is sent; `blocked`), which
the Dolphin plugin turns into an emblem; the commit takes it off.

## 6. The requests

### 6.1 What is sent

Every change carries a guard: `If-Match` on anything that exists, `conflictBehavior=fail` on
anything new — with one exception: a folder's delete, sent with no guard at all. The folder goes
whole, whatever changed inside it in OneDrive since; the recycle bin is the safety net, as on
Windows ([decisions.md](decisions.md), "A folder delete is the whole folder, as on Windows").

| Operation | Request | Guard |
|---|---|---|
| a new empty file | `PUT /items/{parent}:/{name}:/content?@microsoft.graph.conflictBehavior=fail`, then a `PATCH` of its time | `conflictBehavior=fail` in the URL: a `PUT`'s default is to replace |
| an emptied file | `PUT /items/{id}/content`, then the `PATCH` | `If-Match: <base eTag>` |
| a new file, 1 B – 10 MiB | `POST /items/{parent}:/{name}:/createUploadSession` with `conflictBehavior: fail`, its name and `fileSystemInfo`, the session persisted, then one `PUT` of the whole body | `conflictBehavior: fail` |
| a changed file, 1 B – 10 MiB | `POST /items/{id}/createUploadSession` with `conflictBehavior: replace` and `fileSystemInfo`, one `PUT` | `If-Match: <base eTag>` |
| over 10 MiB | the same session, in fragments of 10 MiB (32 × 320 KiB) | as above |
| a new folder | `POST /items/{parent}/children` | `conflictBehavior: fail` |
| a rename or move | `PATCH /items/{id}` with `name` and/or `parentReference.id` | `If-Match: <base eTag>` |
| a file's delete | `DELETE /items/{id}`, into the recycle bin | `If-Match: <base eTag>` |
| a folder's delete | `DELETE /items/{id}`, whole, into the recycle bin | none |
| a session's status, its end | `GET` / `DELETE` of the upload URL | — |

A row made against a download that was not the base's version carries only its cTag, and that is its
guard. Every file but an empty one goes through a session, even a small one: the session request
documents both guards and carries the file's time, so it costs no extra request
([decisions.md](decisions.md), "Small files go up in an upload session"). A session request carries
no `fileSize`: a personal drive refuses it with `400 invalidRequest` (limitations log F39), so a
full drive shows itself when a fragment is refused. Requests to an upload URL never carry the
account's token.

**An open session holds its name**: until it completes or is cancelled, a new file's session leaves
an empty file under its name in OneDrive, which a second session of that name meets as
`409 nameAlreadyExists`. So a session is never simply dropped:

- **Every session is persisted** as soon as it is opened, before its first `PUT`: in one
  transaction, the row's `session_url` and the store's list of open sessions (`upload_sessions`,
  with the parent and name a new file's session holds).
- **A refused fragment goes again to the same session.** A `429` or `503` on a session's `PUT`, a
  dropped connection or a timeout waits for `Retry-After` (or 10 s), asks the session where it
  stands, and sends the same fragment again, five sends in all. Refused still, the row fails for now
  and keeps its session. A new session is opened only when the old one has ended (§6.2) or the file
  is no longer the snapshot it was opened for.
- **A session given up is cancelled** (`DELETE` of the upload URL): the content changed, the file
  was removed (§5.2), a conflict copy, a fragment answered `409` or `412`, the row leaving the
  outbox, a forced switch to read-only. A row blocked or waiting keeps its session. A session no row
  points at any more is cancelled when the worker next looks while it may send (after a cancel that
  failed, not before a minute), and leaves the list only then.
- **A `409` from our own placeholder.** When a new file's create meets `409` and a listed session
  holds that name, the holder is that session's placeholder: this row's own is resumed by its next
  run, a session another row still points at is waited for (`upload-session-open`), and any other is
  cancelled and the create goes again.
- **The place is recorded before the session is opened** (parent id, name; `upload_openings`), with
  its first time and the time of its latest attempt whose outcome is unknown; the session's URL
  replaces the record once it is persisted. The outcome is unknown only after a failure typed
  `Transient` (a timeout, a lost connection, a `5xx` other than `503`) or a stop; any other answer
  made no placeholder and clears the record the attempt made. A record whose row leaves the outbox
  or moves elsewhere is kept without a row for 7 days from then.
- **A `409` at a recorded place.** With no listed session there but openings recorded from earlier
  attempts, the holder is read. An empty file the delta feed never listed (neither the items table
  nor a listing being staged knows it), created within one record's window — from its first
  recording to its latest attempt, each widened by 5 minutes for the clocks — is that opening's
  placeholder: it is deleted (with its eTag; the delete ends the session — limitations log F172) and
  the create goes again. A delete OneDrive refuses leaves the row waiting (`upload-session-open`).
  The records at the place are cleared once the question is settled.
- **A placeholder not ours is never deleted**: nothing in OneDrive says which machine opened a
  session, and deleting a placeholder another device is filling would kill that device's upload.
  Only this folder's own records make a placeholder ours; any other holds its name, and the row
  waits (§6.2).

**The daemon's stop.** On SIGTERM or SIGINT the outbox workers take no more rows; the rows in flight
finish their step — a metadata row its request and commit, an upload the fragment in flight, with
its session kept — and the daemon exits once they have, or after 10 s at most (`stop::STOP_BOUND`).
What is still in flight then is cut, and the recorded place covers it. A second signal exits at
once. The daemon stops the same way, and exits with a failure, when a task it cannot work without is
gone (the helper's supervisor, the watch of the helper's state).

### 6.2 What the answers mean

| Answer | Action |
|---|---|
| `200`, `201` | the commit (§5.4) |
| `202` | a fragment accepted: `session_next` persisted, the next one sent |
| `409` | a name a listed session of ours holds: its placeholder (§6.1). Otherwise the item at that name is read. A create adopts it when its hash is ours and nothing here knows the item yet, a folder adopts a folder and the two merge, a move adopts its own item (it landed); a name a live row is freeing goes through a temporary name (§5.3). What would be a copy but is an empty file neither the items table nor a listing being staged knows is taken for an upload session's placeholder (never in the feed): never a copy, never deleted — the row waits (`name-held-by-an-upload`, the usual backoff) until the name is free, or the holder has content or the feed lists it, and then decides again. This holds for every `409`: a create, a move or rename, a folder's `mkdir`. Anything else makes the file here a copy (§7); a `move` row's object takes the copy's name with its attributes kept, and the row becomes the move of the same item to that name |
| `412` | the item is read again: the same hash as ours means done already; the base's cTag means only its metadata changed, and the request goes again with the fresh eTag (an upload from its first byte); otherwise §7 |
| `404` | an `update` or a `move`: the item is gone in OneDrive, §7. A `delete`: done. A `create` or `mkdir`: its parent is gone; a cycle is asked for and the row retries |
| an upload URL that answers `404`, `410`, `401` or `403` | the session ended: the item is read and adopted if its hash is ours, else a new session from zero |
| `416` | a fragment the session has: its status says where to go on |
| `423` | locked (co-authoring): retried later |
| `507`, `quotaLimitReached` | the quota is read at once and decides: the account full, or only this file too big (§6.4) |
| `400` | `blocked`, with the service's message; an upload keeps its session |
| `401` | the token refreshed once |
| `403` | the row is `blocked` (`forbidden`), and listed in Not Uploaded, whose words say to sign in again; the other rows go on (whether the sign-in allows writes at all is the write gate's to say, §2.3). A worker that begins — after a sign-in, a restart or a mode switch — makes such rows `ready` once (issue #223) |
| `429`, `503` | a fragment to an upload session is sent again to the same session first (§6.1); then, or for any other write, the whole account's worker waits until `Retry-After` (in seconds or as an HTTP date, at most an hour; without one, 10 s doubling with each throttle up to an hour, and back to 10 s once a row lands). While a wait is under way a time OneDrive names takes the place of one the worker chose, of two named times the later end stands, and an answer without `Retry-After` adds nothing. For a wait of 5 s or more the folder's `LastError` says "OneDrive asked to slow down; uploads continue at HH:MM" until it is over. A read the worker makes waits in place instead ([sync.md](sync.md) §4.3) |
| another `5xx`, the network | the row retries after 1 s, doubling to an hour |

No row is ever dropped for failing. A row that becomes `blocked` records `upload-failed` once per
row and reason; a row in backoff records nothing in the activity log, only a line in the journal.

### 6.4 A full OneDrive

**Free space** is Graph's `quota.remaining`, never `total - used`. Between two reads the bytes of
each commit are taken off it, so the figure shown (`Account.QuotaRemaining`) does not go stale.
There is one quota per account: the space check and the account's own reads (a sign-in,
`RefreshInfo`) update the same figures.

**A refusal** (`507`, `quotaLimitReached`) reads the quota at once — one request; a read made in the
last 10 s, the account's included, is used instead. Then:

- **no space left** — `quota.state` is `exceeded`, or less than 1 MiB is free: the account is
  *full*. No row that adds content is taken, but for a `create` whose file was removed, which sends
  nothing and leaves; an upload under way stops at its next fragment and keeps its session — the
  same stop as a pause's (§11). A row that met the refusal or the stop has the reason
  `waiting-for-space`; the others simply wait;
- **space left** — only the refused file waits, `too-big:<bytes needed>:<bytes free>`, until a quota
  read shows it fits. Every other file is sent whatever the known free space says: a figure gone
  stale never holds files back, and OneDrive has the last word.

A quota that cannot be read after a refusal counts as full. A waiting row stays `ready` in its
place, with no timer of its own. Moves, renames, deletes and new folders go on, but one itself
refused `507` waits as a file does. A delete frees space, but does not end *full* by itself.

**Leaving full.** Every quota read decides again: `Folder.Refresh` (`konedrivectl sync refresh`),
`Account.RefreshInfo`, and an automatic read every 30 minutes while the account is full or a file is
too big, made while the worker may send and has no row in flight. With space again, *full* ends and
every too-big file that now fits goes. At a start, rows an earlier version blocked with
`quota-exceeded` become `waiting-for-space`, and the quota is read once before content is sent.

**Shown by** `UploadQueue.QuotaFull`, `QuotaWaitingCount`/`QuotaWaitingBytes`, `TooBigCount`,
`Account.QuotaState` and `QuotaRemaining`; a line in `sync status`, on the Status page and in Not
Uploaded.

### 6.3 A file's session

One path for every file with content (`upload/content/session.rs`, `send_session`): a file up to
10 MiB is a session of one fragment, and every step below holds for it as for a larger one.

1. The worker checks the file's state, probes for a writer, and takes its size and time as the
   **snapshot**, stored in the row.
2. The session is created, and its URL, expiry and `session_next = 0` persisted before the first
   byte (§6.1). A crash before that leaves an orphan session, which expires on its own.
3. Each fragment is read into one buffer, fed to the hash, sent, and on `202` its progress
   persisted. A session that has ended (§6.2) has completed with its answer lost — the item holds
   this content and is adopted — or is gone, and the upload starts over with a new session, once in
   a run. Before each fragment, the first too, the upload may stop (§11), and the file is looked for
   under its row's names: removed, the upload ends as §5.2 says; moved by a recorded row, it goes
   on.
4. Before the last fragment the worker probes for a writer, compares the file with the snapshot and,
   for a changed file, reads the item again: its eTag or cTag must still be the guard's. `If-Match`
   is checked when a session is created, not when it completes; this narrows the window in which an
   edit made in OneDrive meanwhile is superseded to about one fragment, and such an edit stays in
   the item's version history (limitations log F80). The read is skipped when the session's opening
   was the request just before; a last fragment sent again (§6.1) is not preceded by a new read
   (issue #231).
5. The last fragment's answer carries the item; a hash in it must be the one computed while sending.
   A mismatch is never committed: the content goes up again from zero.
6. After a crash or a dropped connection the session's status says where to go on from, but only for
   the content it was opened for; a resumed upload reads the file again from its start, to rebuild
   the hash.

A file that changes while it is sent (its size or time moves from the snapshot) is abandoned and
waits again. A file over 250 GiB is blocked before any request.

## 7. Conflicts

A local change meets a change in OneDrive on the same item. **A copy** means: the local file is
renamed in its directory to `<stem>-<machine><.ext>` with `RENAME_NOREPLACE` (`-2`, `-3`, … when
taken), stripped of konedrive's attributes, and uploaded as a new file; the cloud's version takes
the original name as a placeholder, and a conflict of kind `copy` is recorded. The stem ends at the
last dot: `archive.tar.gz` → `archive.tar-fedora.gz`, `.bashrc` → `.bashrc-fedora`. `<machine>` is
`machine_name` in the account's config, by default the host name; either is cut at its first dot,
any character OneDrive refuses replaced by `-`, at most 32 characters.

| Here \ in OneDrive | edited | renamed or moved | deleted |
|---|---|---|---|
| **edited** | a copy; the same hash on both sides is adopted | the upload goes to the renamed item, and the file here takes OneDrive's name where the folder can hold it | this computer's wins: uploaded again as a new item (`restored`) |
| **renamed or moved** | the rename is sent again with the fresh eTag | the first to reach OneDrive wins: the file here follows OneDrive's place where the folder can hold it | a downloaded file, or a folder, is uploaded again at its new place (`restored`); a placeholder follows the delete |
| **deleted** | OneDrive's wins: the delete is dropped and the item comes back as a placeholder (`restored`) | deleted with the fresh eTag | done |

**Where the folder cannot hold OneDrive's place** for the item, the worker sends only what the user
changed: content goes into the item where it is, with no name and no folder sent, and a rename or a
move made here is sent as that alone.

**Both new at one name** (create/create): the same hash is adopted, no copy and nothing sent;
another hash gets a copy, and so does a name that differs only in case, which is the same name to
OneDrive. An empty file at the name that the delta feed has not listed is never a conflict: it is
taken for an upload's placeholder, and the row waits (§6.2). Two new folders of one name merge,
their contents meeting file by file under these rules.

**Folders.** A folder deleted here is deleted whole in OneDrive, unguarded, whatever it gained or
changed there meanwhile (§6.1). A folder deleted in OneDrive that holds local work here is kept,
with the work, and made again in OneDrive; what it held of OneDrive's goes (§9).

Every copy and every `restored` goes into the activity log; copies are listed by `Conflicts.List()`
and on the window's Conflicts page with "Show Both". Nothing here deletes a local byte, and nothing
deletes content in OneDrive that this computer has not seen, apart from §6.3's one-fragment window
and a folder's own delete.

## 8. Moves out of the folder

### 8.1 What decides

An object missing from the folder is a delete or a move out; the events cannot always tell (an event
can be lost, or the move made while the daemon was not running). The object can: the helper opens it
by its file handle (`OpenByHandle`), and either it is gone (`ESTALE`), or it is somewhere, and
`/proc/self/fd` says where, proved by opening that path again and finding the same inode. A move out
is a delete for OneDrive, done only after the content is on this computer (§8.4).

A helper that does not answer in time is asked once in an examination; what is undecided is asked
after again 30 s later. On a changed filesystem each `move-out` row takes the handle of the item's
object at its last place, or goes (limitations log F261).

### 8.2 `OpenByHandle`

The one message the helper gained for uploads. The daemon sends a file handle and the folder's root
descriptor; the helper opens the object with `open_by_handle_at` relative to that descriptor, and
answers its descriptor only if the anchor is the user's directory on the device of one of the user's
folders, and the object is the user's own regular file or directory, on that device, still linked,
carrying `user.konedrive.item-id`. A stale handle or the user's own unlinked object is `ESTALE`; a
file under a lease is `EAGAIN`; anything else it will not hand out is `EPERM`. A file comes back
read-only, and the daemon reopens it for writing as its owner. The helper exempts its own opens by
its own pid. Details: [hydration.md](hydration.md) §10.1, §11; SECURITY.md.

### 8.3 A `move-out` row

- **Re-marked first.** A placeholder that left sits outside every marked directory and would read
  zeros. Before any row runs, and once for each connection to the helper, every pending `move-out`
  object whose content is not local yet is given `MarkFile` (a directory: `MarkDir` for it and every
  directory below), paused and held rows included (limitations log F120, Z3).
- **Anywhere but the Trash**: a placeholder is downloaded where it went, through its own descriptor,
  by the ordinary fill; a folder has every placeholder of the item inside it downloaded. Only when
  each reads `hydrated`, the row is still the item's newest, and the object is still proved to be
  outside the folder, is a marker stored in the row; then konedrive's attributes come off (the item
  id first), the directories are unmarked, and the item is deleted in OneDrive. While it waits
  nothing is lost either way: the file stays a marked placeholder, the item stays in OneDrive.
- **In the Trash** (the user's own, or a `.Trash-<uid>` or sticky `.Trash/<uid>` at the top of a
  mount, with the entry's `.trashinfo`), nothing is downloaded: a placeholder is removed with its
  `.trashinfo`, a downloaded file stays as the user's, and the item goes to OneDrive's recycle bin.
  A file whose fill or free-up was cut short goes as a placeholder does: a free-up cut short before
  its punch still holds the whole content, so an edit made in place since is lost with the file. A
  folder sent to the Trash loses its placeholders and keeps what was downloaded (the Trash inside a
  folder that is a mount point: issue #225).
- **Doubt keeps the row**, and the item in OneDrive: `EPERM`, no helper, a download that stopped, a
  place that cannot be proved, anything a moved-out folder held that is alive but unreachable.
  `ESTALE` is the user's delete only with the evidence of §4.2, rule 7; for the row's own object it
  must also say so twice, 5 s apart. Once the marker is stored, `ESTALE` or `EPERM` finishes the
  delete.
- **Fills of moved-out objects** are routed by the item id they carry to the account whose row names
  them, before device and path.
- **Into another account's folder**, the move is a move out of the first account and, for a
  downloaded file in a read-write second, a new file there. A read-only second sets the object
  aside, alive, in its rescue directory. The first never deletes an item whose object was last
  proved to be inside another account's folder: the row goes, and the item is placed again
  (limitations log F124).

### 8.4 Content first

Nothing is deleted in OneDrive while its content exists only there and the user still holds the file
somewhere. A cloud-only folder moved out is therefore downloaded in full, with no prompt, as on
Windows ([decisions.md](decisions.md), "A move out of the folder downloads first"; limitations
log F121).

### 8.5 Restore

`RestoreDeletes` of a held move-out places the item again in the folder and tidies what left: a
placeholder outside is removed, a downloaded file stripped, the item's directories unmarked,
stripped and removed if empty. What cannot be proved to be outside every folder, or cannot be
reached, is left as it is. A switch to read-only that drops the rows, a Forget, a Remove, and a
cycle that drops a `move-out` row because OneDrive removed the item tidy the same way (issue #210).

## 9. Reconcile in read-write mode

A read-only folder's cycle is [sync.md](sync.md)'s; a read-write folder's keeps the user's changes.

**The cycle.** The first cycle after bring-up waits for the watcher's first batch to be examined —
the Full local scan, once a base exists — so that changes made while the daemon was down are rows
before anything from OneDrive is applied. Each cycle holds the folder's tree lock from staging to
its swap (a first listing, once per page), the lock every outbox commit and every examination holds.
The outbox waits for a cycle at its start and whenever the network comes back, so the base catches
up before any guard is sent.

**The order of the locks.** A cycle takes the tree lock at staging and the folder's lifecycle lock
([sync.md](sync.md) §6.3), as a reader, at the reconcile; both waits end when its poller is stopped.
A writer of the lifecycle lock takes no tree lock: it cancels every part of the sync, takes the lock
and waits for the parts to end — the cycle, then the outbox worker, then the watcher, whose
examination under way holds the tree lock.

The lifecycle lock is fair: a writer waiting for it keeps new readers out. So nothing but a cycle
may hold the tree lock while it waits for the lifecycle lock, and nothing may wait for the tree lock
while it holds the lifecycle lock as a reader: with a cycle on one side, such a caller on the other
and a writer waiting between them, the three would wait for each other for good. What holds the tree
lock alone (an outbox commit, an examination, a replacement, `RestoreDeletes`) never waits for the
lifecycle lock. A forced switch to read-only drops the rows as a writer, with the sync stopped, and
turns the folder read-only in the same step, so no watcher records again what was dropped.

A file's inode lock comes after the tree lock. A cycle, which holds the lifecycle lock, only tries a
file's lock, or waits for it for a bounded time (a removal's stopped downloads, 10 s in all); Free
up space takes the file's lock, then the lifecycle lock as a reader.

**An idle cycle.** A cycle whose delta is empty, with no Full reconcile asked for, no deferred
change that can go, no outbox commit since the last cycle's fetch, and nothing placed without a
local object on record, stages nothing and only stores the new link. Asking that reads indexes,
never a whole table.

**The stale-delta guard.** A delta fetched before an outbox commit can carry an older version of the
item the commit wrote. The cycle records `outbox_seq` when its fetch begins; a delta's entry about
an item the outbox committed since (`items.local_seq`) is trusted only if it is the commit itself
(the same eTag), and otherwise read again with `GET /items/{id}` under the lock. So is every entry
that removes such an item, and every entry about one the outbox deleted since (`outbox_gone`).
Reading again rather than dropping keeps a change OneDrive made just after the commit. This also
makes echo harmless: a delta reporting the daemon's own upload finds the base already as it says.

**What the reconcile leaves.** An item with an outbox row, in any state, and everything under a
folder that has a row which is not a removal, is not moved or replaced; a local move not examined
yet is left where it is. OneDrive's change to such an item waits in the store (`deferred`), staged
again every cycle until the disk takes it or an outbox commit supersedes it; below a pending
`delete` or `move-out` the change goes into the base and nothing is placed, but an item OneDrive
moves into such a folder from elsewhere waits where the base has it, its move not in the base. A
Changed reconcile does not turn Full over an item that is not where the base has it. OneDrive's
moves still go through the holding directory, but what sits there is placed from it, removed if
OneDrive removed it, or put back; it never leaves the folder, where it would pass for a move out.

**Where the read phase rescued, a copy.** A file in the way of an item OneDrive brings or changed is
renamed to a conflict copy in place (§7) and handed to the examination, which uploads it: the
daemon's own renames raise no event the watcher keeps, so after its swap the cycle hands the watcher
whatever it kept, copied or stripped. An object with no id at the name of an item OneDrive did not
change (a save by rename not examined yet) is left alone. A folder of the user's at the name of a
folder OneDrive brings merges with it. A missing item whose local object is on record is not placed
again: its absence is a delete or a move not examined yet, and the examination is given its place.

**What OneDrive removed** is removed here at once, in place, whatever rows wait for it, and what
OneDrive never had is kept (§7). What goes is what OneDrive had and the daemon placed: a file not
downloaded, a download unchanged since, a download in progress (stopped first), a folder once
nothing is left in it. What stays is what only this computer has: a file made here, a download
changed here — its stamp differs, it is open for writing, or an `update` waits for it — a download
that is not of the removed item at all, a file of ours whose state cannot be read and that holds
data, with the folders above them. Their konedrive attributes come off, so they are the user's own:
the files go up as new, the folders are made again in OneDrive, and the rows that waited there and
are not being sent are dropped, the examination recording new ones. What is kept under an ignored
name, or a name OneDrive refuses, stays on this computer only and is never uploaded. The activity
log has a `removed` entry for each outermost place where something stays (limitations log F187).
While another filesystem is mounted inside a removed folder, nothing of that folder is touched
(issue #204).

**What can no longer be placed** while OneDrive still has it (a name too long, the Personal Vault,
…, or a folder above it that is one) is a change that may have to wait (`take_off`, `Unplaced`):

- **Nothing waits in it: it goes in the cycle.** No outbox row has a place at or below it, and the
  disk shows there exactly what the base has: the store forgets the objects, it leaves the disk
  whole, and the base takes OneDrive's row. It is listed in `Skipped()`. Each object is looked at
  again right before its unlink, and only what was looked at goes: local work, an item id not looked
  at or a mount stops the removal there.
- **Something waits: nothing of it is touched.** The base keeps the item placed where the disk has
  it, and OneDrive's row waits in `deferred`. Meanwhile it is an item like any other: what the user
  does in it is sent. It yields its name to an item that takes it: renamed aside to
  `name-<machine>`, in the base too.
- **What waits**: an outbox row at or below it; something on disk that differs from the base and
  that an examination has still to record; a file open for writing; an item OneDrive moved out of it
  that could not be placed yet; and, until the user does something, a file of ours whose state
  cannot be read, a file that is not downloaded and is not where the base has it, something under an
  ignored name that only this computer has (limitations log F255), another filesystem mounted
  inside. What was found is on the item's line in `Skipped()`.

**The daemon never deletes or moves anything in OneDrive because it took something off the disk
itself.** Before the reconcile removes anything, the store forgets the recorded local object of
everything it removes, in `items` and in `staging`: the item, what the tree has below it, and every
object found there by its own id and file handle. An examination that then misses such an item finds
it unproven, never deleted (§4.2 rule 7, WR4). The store's own rule: **the base records a local
object only for an item it places**. Whatever writes a row into `items` that the base then does not
place clears its recorded object and those of everything below it in the same transaction.

**Replacements** of a downloaded file run under a write lease on the old file, granted only while
nobody has it open: a writer is not left writing into an unlinked inode ([sync.md](sync.md) §9).

**`410`.** A `410` that does not name `resyncChangesUploadDifferences` lists the drive afresh and
reconciles Full. `resyncChangesUploadDifferences` keeps what the new listing left out: downloaded
files go up again as new, while placeholders are removed; a downloaded file whose version differs
from the listing's, and that has no row waiting, is kept beside it as a conflict copy (§7).

## 10. A crash at each step

Every step can be replayed after a crash and reaches the same end (WR7).

| Crashed | The next start finds | It does |
|---|---|---|
| after the event, before the row | nothing in the outbox; the change on disk | the bring-up's Full local scan finds it (all but an edit that kept size and time, §4.6) |
| a small upload sent, no answer | the row `running`, its `session_url` | the session has ended: the item's hash equal, adopted |
| a session created, not persisted | a new file's recorded place, no URL | a new session; the orphan expires — a new file's replay meets its placeholder as `409`, deletes it and creates again, or waits for the orphan to expire if OneDrive refuses the delete; never a copy (§6.1) |
| mid-session | `session_url`, `session_next` | the session's status, then on, if the file is still the snapshot; else a new session |
| the last fragment sent, no answer | `session_url` | the session has ended: the item's hash equal, adopted |
| a new file's last request sent, no answer, then the file removed | the row `running`, no object | the name looked up: an item of this size and time, unknown here, goes to the recycle bin; the rows leave (§5.2) |
| the answer received, the commit's first step partial | some attributes written, the row `running` | replays, meets `409`/`412`, adopts, commits again |
| the first step done, the second not | the file with the new cTag and id, the store with the old base | replays; an `update` meets `412`, a `create` `409`: the same hash, adopted, and the store's step runs |
| a `PATCH` sent, no answer | the row `running` | replays, `412`: the item is where the row wanted it, adopted |
| a `DELETE` sent, no answer | the row `running` | replays, `404`: done |
| a `mkdir` sent, no answer | the row `running` | replays, `409`: a folder, adopted |
| a conflict copy | the rename done or not, the strip done or not | not renamed: the check runs again; renamed and stripped: the copy is a new file, `create` |
| a move out part-way | the row with its handle | re-marked, the download resumed; the marker says whether the strip was the row's own |
| the store lost | a rebuilt base, no outbox | a listing and a Full local scan: creates, edits and moves are found again; deletes are not, and those items come back |
| the helper restarted | its marks gone | moved-out objects re-marked, failed `MarkDir`s asked again, then a Full local scan |

## 11. On the bus and the command line

On the bus, per account: `org.konedrive.UploadQueue` (the changes waiting, what is not uploaded, the
held deletes, the counts), `org.konedrive.LocalScan` (§4.6), `Transfers.Uploads`,
`Conflicts.MachineName`, and on `org.konedrive.Folder` the pause, the ignore list, the automatic
hold's `HeldBack` and `SyncAnyway`, and `LiveChanges`; on `Account`, `SetMode`, `Mode` and the
quota; on `org.konedrive.Accounts`, the hold's two settings. The activity kinds are `uploaded`,
`cloud-moved`, `cloud-deleted`, `upload-failed`, `restored` and `not-uploaded`.
[desktop.md](desktop.md) §2–§4 has each member, the commands and the window's pages.

**The error `NotUploaded`** is what "Free up space" gets for a file with a change not uploaded yet:
a downloaded file for which the outbox has any row, by its item id or by its inode. A file named
itself refuses the whole call; one inside a folder being freed is left and counted busy
([hydration.md](hydration.md) §8).

**Answers from memory.** The counts and the Not Uploaded summary are kept in memory by the daemon,
and summed again by SQL after the outbox changes, at most once a second. The lists (`Changes`,
`NotUploadedFiles`, `NotUploaded`) are read through a second, read-only connection to the tree
store, which in WAL mode never waits for a writer. The store's read-write connection is owned by one
thread per account; every other part of the daemon sends it jobs and waits for the answer
(`store.call`), so a long store operation never delays the async runtime.

**What runs is decided in one place** per account (`conditions/running.rs`), from the user's pause,
the automatic hold (below) and the thumbnail setting ([desktop.md](desktop.md) §8). The outbox
worker, the poll and the replacements it runs, the notification socket and the thumbnail filler ask
it, and the transfer pool is told. A pause and a hold stop the same work; thumbnails off stop only
the thumbnail requests.

**The automatic hold** holds an account back by itself (on mains power the battery never does):

- on a **metered connection** — NetworkManager's `Metered` is `1` (yes) or `3` (guessed yes) — while
  `pause_on_metered` is on (the default);
- **on battery** — UPower's `OnBattery` — as `on_battery` says: `sync`, the battery changes nothing;
  `power-saver` (the default), while the power profile (`ActiveProfile` of
  `org.freedesktop.UPower.PowerProfiles`, or of the older `net.hadess.PowerProfiles`) is
  `power-saver`; `pause`, always.

One watcher for the daemon (`conditions/mod.rs`) follows the three sources. A source that is missing
or cannot be read is no reason to hold back (limitations log F175).

**The two settings are the whole app's**: top-level keys of `config.toml`; a change
(`Accounts.SetPauseOnMetered`, `SetOnBattery`) reaches every account at once and ends every
account's `SyncAnyway`. A start that still finds either key in an account's section, where an
earlier version kept them, moves it to the top level once, the strictest value winning. `HeldBack`
says why an account holds back now — `metered`, `on-battery`, `power-saver`, or empty; with a
network and a battery reason at once, `metered`.

The hold is not the user's pause: it is not written anywhere, never changes `Paused` or
`PausedUntil`, and is worked out again from the sources after a restart. The account runs only when
neither is on. `SyncAnyway()` (`sync anyway`, the window's **Sync anyway**) lifts the hold until
what a source says changes or one of the two settings does. It is per account: lifting every
account's hold from one account's Status page would sync accounts the user did not look at. The
whole app's action is the tray's **Sync Anyway** and `konedrivectl sync anyway --all`, which call it
on every account that holds back by itself and is not paused by the user.

**Pause** stops the account's outbox, its poll (so no cycle and no replacement), its notification
socket, its pinned downloads (the pool gives a slot only for opens) and its thumbnails; fills on
open, `Hydrate` and the watcher go on, so rows keep collecting. It is kept with the account's
settings (`paused_until` in its section of `config.toml`), so it outlasts a restart, a timed pause
ends by itself, and it is set and shown whether the account's sync runs or not. It needs a
registered OneDrive folder. What it does to work already under way:

| Work in progress | On pause |
|---|---|
| an upload (a session) | stops after the fragment being sent — before its first one, if its session is only being opened; the session and its offset stay in the row, which waits with the reason `paused` |
| the only fragment of a file up to 10 MiB, being sent | finishes: it is short |
| a metadata request (mkdir, move, delete) | finishes |
| a fill on open, `Hydrate` | goes on: a pause never blocks opening a file |

No new row starts. `Changes()` lists every row that waits, retries or runs as `paused`, while
blocked and held rows keep their state; a pause writes no `upload-failed`. Resume, or the end of a
timed pause, makes the rows due at once: a kept session goes on from its offset. The stop between
fragments is one check (`upload/content/session.rs`, `stop_between_fragments`), asked before every
fragment, with five reasons: a pause or a hold, the daemon stopping, a full OneDrive (§6.4), the
write gate — each keeps the session — and the file removed (§5.2), which cancels the session and
ends the row. None of them interrupts a request that is in flight.

## 12. Testing

- **The Graph client** against wiremock: the requests of §6.1 and the answers of §6.2.
- **The outbox worker** end to end against a stateful fake OneDrive on wiremock: the crash rows of
  §10 through fault points, the cells of §7, swaps, throttling, offline, pause, the blocked states.
- **The examination** in temporary directories: the rules of §4.2, save by rename as real editors do
  it, copies, hard links, the ignore list, the mass-delete guard.
- **The watcher**, as an unprivileged group on the host: own events, an overflow, a new directory's
  mark-then-scan, the degraded mode, the root moved.
- **The reconcile in read-write mode** on the fake OneDrive: the tree lock, the stale-delta guard,
  echoes, both `410` variants, a replacement under a lease.
- **The VM** (`tests/vm/run.sh quick`), for what involves the helper: a new directory given a
  placeholder at once, `OpenByHandle` and its refusals, moves out and into the Trash, a helper
  restart.
- **The test account**, by hand, through the guarded harness below; and **a stress run**, by hand,
  against a read-write test account whose drive is in `write_test_drive_ids`
  (`tests/stress/README.md`).

### 12.1 Running against the test account

`konedrive-write-test` (`tests/write-account/`) checks, against OneDrive itself, what §13 lists, the
small file's one-request session, and what a write sends on the notification socket. It writes, so
it runs only against a separate test account, by hand, and **refuses to start unless every guard
holds**:

1. `--graph-test-drive` is the drive both tokens reach (`GET /me/drive`), and is listed in
   `write_test_drive_ids` in the `config.toml` given with `--daemon-config`;
2. the drive looks like a test account: less than 1 GiB in use and fewer than 1000 items — unless
   `--large-test-drive` says the test account holds more;
3. every write stays in `/konedrive-write-test/<run id>/`, which the run makes and puts into the
   recycle bin at the end. Every Graph request goes through a proxy on `127.0.0.1` whose guard
   asserts, before it sends anything, that the item the request names, or the parent of what it
   makes, lies inside that folder; a request it refuses never leaves the machine, and after a
   refusal only the cleanup is sent. Reads may go anywhere. Only the notification socket is opened
   directly;
4. at most 64 MiB per file, 200 MiB of content and 500 requests per run;
5. the write token comes from `konedrivectl dev export-access-token --read-write`, which the daemon
   hands out only for a drive on the same list, and both token files must be `0600` and the user's.
   The tokens need a development build (the `dev-tools` feature, `scripts/dev-install.sh`).

Once, before the first run: add the test account and sign it in; its drive id is then the `drive_id`
of its section in `~/.config/konedrive/config.toml`. Add that id by hand to
`write_test_drive_ids = ["<id>"]` at the top of the same file. Each run, within the hour an access
token lasts:

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

Two checks run alone, with the read-write token only: `--only placeholders` (limitations log F172;
with `--large-test-drive` on an account that holds more than 1 GiB) and `--only notifications` (what
a write sends on the drive's Socket.IO endpoint). The second export above is the check of the switch
back: the daemon hands it out only if Microsoft answered the refresh with no write scope
(limitations log F66).

Each check prints `PASS`, `FAIL` or `LOOK` (to be confirmed by hand). The exit status is 0 when
nothing failed, 1 when a check failed, 2 when a guard refused: `REFUSED` before anything was
written, `STOPPED` mid-run (the run folder still goes to the recycle bin).

## 13. What is assumed of OneDrive

Verified against Microsoft's documentation: the session protocol (but for `fileSize`, §6.1),
`If-Match` on sessions, `PATCH` and `DELETE`, `conflictBehavior` on folders and sessions, the delta
feed's rules, throttling, and the refresh's "equivalent to or a subset of" the scopes first granted.
**Assumed**, with no result of a test-account run (§12.1) on record, each handled safely either way:

- `If-Match` is honoured on the `PUT` that empties a file;
- `conflictBehavior=fail` works in a `PUT`'s URL, a rename or move onto a taken name is refused
  `409`, and names collide without regard to case;
- the delta feed returns the daemon's own changes with the eTags their writes were answered with;
- 10 MiB fragments, and a resume from the session's status, work as documented, and a delete goes to
  the recycle bin;
- an edit made in OneDrive during a session is superseded by its last fragment (§6.3);
- a read-only refresh after a switch back answers a token that cannot write.

## 14. Known limits

[`../limitations/`](../limitations/) holds the limits that cannot simply be removed. Those of
uploads: rows kept when an account turns read-only by itself (F140) and consent that stays with
Microsoft (F66); copies and moves told apart by the recorded object (F53), deletes decided only on
the helper's word (F54); other devices (F72), changes that must come from the daemon's own process
to be told apart (F74), a size change by path (F75), placeholders moved into a brand-new directory
(Z2); `fileSize` left out of a session (F39), the placeholder an open session leaves (F172), the
one-fragment window (F80); moved-out objects (Z3, F120, F121, F124, F261); what a removal keeps
(F187) and what keeps an item that can no longer be placed (F255); the automatic hold without
NetworkManager or UPower (F175). File modes lost across a round trip are issue #216.

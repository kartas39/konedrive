# Hydration

How a file in the sync folder can exist without its content, how opening it brings the content in
before the opener sees anything, how the space is given back, and how all of it survives crashes.
The processes involved are introduced in [README.md](README.md); how the folder is kept in step with
the cloud is in [sync.md](sync.md).

Measurements quoted here come from the privileged suite in `tests/vm/`, run in a VM against Btrfs,
ext4 and XFS; `docs/kernel-behavior-7.2/` has each one and its evidence (cited as *kernel* §n).

## 1. Scope

Placeholders on disk; the helper, its marks and how it decides an open; filling a placeholder
(hydration) and freeing it up again (dehydration); recovery after a crash; the protocol between
helper and daemon and what the helper lets each user do; registering a folder.

## 2. Placeholders on disk

### 2.1 Directories

Directories are ordinary directories, created up front from the listing. There are no directory
placeholders: listing a folder never needs the network. The sync root carries `user.konedrive.root`
(a random version-4 UUID, the *root id*) and, when it shows OneDrive, its account's drive; every
other directory created from OneDrive carries `user.konedrive.item-id`. Every directory under the
root carries the helper's permission mark (§4): that mark is what makes the files inside it visible
to the helper.

### 2.2 Files and their states

| State | On disk | Interception |
|---|---|---|
| `online-only` | a sparse file: `st_size` is the remote size, no data blocks, `mtime` is the remote `lastModifiedDateTime` in whole seconds | covered by its directory's mark |
| `hydrating` | content being written | covered by its directory's mark |
| `hydrated` | an ordinary file with its data | covered, plus an evictable ignore mark placed on the first open that finds it `hydrated`, so later opens do not reach the helper |
| `dehydrating` | blocks being released | covered; the state is made durable before the ignore mark is removed (§8) |

A file inside the folder with neither a state nor an item id is not managed: the helper lets its
opens through and never marks it. Zero-byte files are created directly as `hydrated`: there is
nothing to download and nothing to misread.

On ext4 a placeholder's attributes take a 4 KiB block of their own, so a file holding no data
reports `st_blocks = 8`, not 0 (Btrfs and XFS report 0; *kernel* §11.5). Anything that reads "takes
no space" from `st_blocks` must allow one block.

### 2.3 Extended attributes

All values are plain text, readable with `getfattr -d`.

| Attribute | On | Value |
|---|---|---|
| `user.konedrive.root` | the sync root | the root id |
| `user.konedrive.drive` | the root of a folder that shows OneDrive | the id of the account's drive ([accounts.md](accounts.md) §6) |
| `user.konedrive.item-id` | files and directories | the Graph item id, stable across renames and moves |
| `user.konedrive.state` | files | `online-only`, `hydrating`, `hydrated` or `dehydrating` |
| `user.konedrive.ctag` | files, when OneDrive gave one | the Graph cTag of the version the file represents |
| `user.konedrive.stamp` | downloaded files | `<size> <mtime_sec>.<mtime_nsec>`: what the file was when its content was last known to be the version's |
| `user.konedrive.progress` | files part-way through a download | `<ctag> <bytes durably written>` (§7.4) |
| `user.konedrive.pin` | pinned files and folders | `1` ([pinning.md](pinning.md) §2) |
| `user.konedrive.sync` | files with a change waiting to upload, in a read-write folder | `pending`, `uploading` or `blocked`; removed when the change is committed ([writes.md](writes.md) §5.4) |

The stamp is how the daemon recognises its own work: a `hydrated` file whose size or time no longer
matches its stamp has been changed locally, and that change may be the only copy (§8,
[sync.md](sync.md) §10). It is written when a download or a replacement completes, and for a
zero-byte file the sync places. After an upload it is the size and time of the content sent, so it
means "what the version its cTag names holds" either way ([writes.md](writes.md) §5.4).

### 2.4 Building a placeholder

A placeholder is built nameless and linked in complete, so that nothing can ever open a half-built
one:

1. `open(dir, O_TMPFILE | O_RDWR)` — a nameless inode in the target directory;
2. `ftruncate(size)`, set `item-id`, `ctag` and the state, `futimens(mtime)`, and the file's mode
   (`0444` in a folder under the read-only lock, [sync.md](sync.md) §11; `0644` otherwise);
3. `linkat(fd, "", dirfd, name, AT_EMPTY_PATH)` — the file gets its name, already covered by its
   directory's mark.

A crash before step 3 leaves nothing behind. The `O_TMPFILE` open in step 1 raises a permission
event when the directory is marked, although the file has no name yet (*kernel* §7); the daemon's
own opens are let through by its pid exemption (§5.1), and a file with no state yet is allowed
without an ignore mark, so construction neither waits for itself nor blinds the file it builds. A
local folder's placeholders (§14.3) are built the same way, with no cTag and mode `0644`.

## 3. Invariants

These four hold for everything below, and are referred to by name elsewhere.

**M1 — every directory under a registered root carries the permission mark, and a new directory is
marked before anything is created inside it.** The helper's registration and startup walks mark each
directory before descending into it. They stop at 128 levels and at another filesystem, and a
directory that could not be marked is not descended into (§12). The daemon creates a new directory
under a temporary name, has the helper mark it, and only then renames it into place
([sync.md](sync.md) §7.3). In a read-only account's folder no other program can make a directory
(they are `0555`). In a read-write one the daemon's notification watcher sends `MarkDir` for a
directory any program makes, before anything inside it is looked at ([writes.md](writes.md) §3.5); a
placeholder moved into it and opened in between is not covered (limitations log Z2).

**M2 — a file's state lives only in its extended attributes.** The kernel knows nothing about it,
and neither does any database: losing every mark costs coverage, never data, and the tree store
([sync.md](sync.md) §5) is only a map.

**M3 — no file is emptied while an ignore mark of ours is on it.** The ignore mark survives
modification (§4.3), so a file emptied under one is empty *and* permanently unintercepted: every
later open reads zeros, and nothing notices (limitations log Z5). This is the one failure that is
both silent and unrecoverable. A downloaded file is emptied only once it reads `hydrating` or
`dehydrating`: both states say "the content is not to be trusted", and the helper never marks such a
file. And before a file that may carry a mark is emptied, the mark is cleared by **the local rule**,
decided where the file is emptied and never argued from what happened to it before (arguing from
where a mark can be was proved wrong three times, each by a race nobody had seen):

- with a link to the helper, the daemon asks the helper to `ClearIgnore` the file, and stops on any
  failure;
- an intercepted folder with no link is refused outright (`NoHelper`): nothing is probed;
- a folder without interception, and recovery, ask whether a helper is bound to its socket at all.
  With none bound they go ahead: no fanotify group of ours exists, so no mark of ours does. With one
  bound and no link, or when it cannot be told, they stop and try again later.

Whether a helper is bound is read without connecting, which would register this process as the
user's daemon: a `SOCK_STREAM` `connect` to the helper's `SOCK_SEQPACKET` path fails with
`EPROTOTYPE` when its socket is bound there, and with `ENOENT` or `ECONNREFUSED` when nothing is.

A free-up (§8) clears after it made `dehydrating` durable. A fill (§6.1) of a file found in any
state but `online-only` clears right after it made `hydrating` durable, before the first byte is
fetched, because a failed fill empties the file (§6.3). Recovery (§9) clears each file it finds
`hydrating` or `dehydrating` before it punches.

Two cases are argued from the state instead, and ask nothing. A fill that began from `online-only`
needs no `ClearIgnore`: the helper marks only a file it reads `hydrated`, reads the state again
after marking and takes the mark off unless it still says `hydrated`; such a file was never marked,
and once the fill made `hydrating` durable no mark placed on it can stay. For the same reason a
reconcile punches the stale checkpoint of an `online-only` placeholder ([sync.md](sync.md) §7.4)
without asking.

The helper keeps two further guards as defence in depth: its walks clear the ignore mark of every
regular file they pass, and it keeps no mark on a file whose owner had a folder unregistered while
the open was being decided. The local rule does not depend on either.

**M4 — a placeholder that leaves the root is marked on its own inode, so it is still filled where it
now is.** The protocol has `MarkFile` for this, and it works: a placeholder given `MarkFile` and
then renamed out of the tree is intercepted and filled (*kernel* §1). In a read-only account's
folder nothing can be moved out (its directories are `0555`). In a read-write one the daemon finds
what left by its file handle and sends `MarkFile` for a file that is not downloaded (a downloaded
one needs none), or `MarkDir` for a directory and every directory below it. It does so once the
watcher's quiet spell has made the `move-out` row, before any row runs, and again at each connection
to the helper ([writes.md](writes.md) §8.3); until then the object is not covered (limitations log
Z3, F120).

## 4. What the kernel watches

### 4.1 One mark per directory, and ignore marks on downloaded files

The helper marks **every directory inside the sync root** with `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`,
and places an **evictable ignore mark** on each file it has seen `hydrated`. An open of any file in
a marked directory raises a permission event unless the file's own ignore mark suppresses it.

Kernel memory therefore grows with the number of folders, not files: a directory mark pins its inode,
measured at **1658 bytes per directory** on a realistic tree of 10 000 directories and 100 000 files
(*kernel* §8, §11.7, §13) — about 16.6 MB for 10 000 folders, and it does not grow as the drive
fills. The kernel offers no subtree marks, and directory marks are not recursive. The cost of the
choice is one helper round trip on the first open of each downloaded file, and again whenever the
kernel reclaims its ignore mark. The alternatives — a mark per placeholder, a filesystem-wide mark —
are weighed in [decisions.md](decisions.md), "One fanotify mark per directory, plus evictable ignore
marks". `FAN_ONDIR` is deliberately not set: opening a directory is never intercepted, so listing a
folder never waits for the helper.

### 4.2 Why `FAN_OPEN_PERM`

Every path to a file's content — `read`, `mmap`, `sendfile`, `copy_file_range`, reflink, io_uring —
begins with an open, so intercepting the open covers all of them by construction. `FAN_PRE_ACCESS`
could fill ranges lazily, but relies on a hook in every access path; it has not been evaluated
([decisions.md](decisions.md), "`FAN_OPEN_PERM`, not `FAN_PRE_ACCESS`"). The VM suite checks that
`mmap` after open and `cp` see the real content on all three filesystems, and `cp --reflink` on
Btrfs.

### 4.3 The group and the flags that are not optional

```text
fanotify_init(FAN_CLASS_PRE_CONTENT | FAN_CLOEXEC | FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS | FAN_NONBLOCK,
              O_RDWR | O_LARGEFILE | O_CLOEXEC | O_NONBLOCK)
directory:   fanotify_mark(FAN_MARK_ADD, FAN_OPEN_PERM | FAN_EVENT_ON_CHILD, dirfd)
ignore mark: fanotify_mark(FAN_MARK_ADD | FAN_MARK_IGNORE | FAN_MARK_IGNORED_SURV_MODIFY | FAN_MARK_EVICTABLE,
                           FAN_OPEN_PERM, fd)
```

- **`FAN_NONBLOCK` on the group.** Without it, reading events blocks once the queue is drained. The
  loop `poll`s the group and reads until `EAGAIN`.
- **`O_RDWR` event descriptors.** The daemon writes the content through the event's own descriptor,
  which the kernel opened on the opener's mount; that descriptor carries `FMODE_NONOTIFY`, so the
  writes raise no events of their own (*kernel* §6).
- **`O_NONBLOCK` on the event descriptors.** The kernel opens each event's descriptor inside the
  helper's `read()`. Without `O_NONBLOCK`, opening a file that anyone holds a write lease on waits
  for the lease to break, up to the kernel's 45 s, and stops every intercepted open on the machine
  meanwhile (measured: 4.8 s for a 5 s lease; any local user can hold one). With it, the kernel
  answers that one event itself with `FAN_DENY`: the opener gets `EPERM` in 7–8 ms, and every other
  open goes on (*kernel* §12.4; limitations log P2).
- **`FAN_MARK_IGNORED_SURV_MODIFY`.** Without it the kernel refuses to add an ignore mark to an
  inode that anyone holds open for writing — and reports success while creating nothing, which only
  `/proc/self/fdinfo` of the group shows (*kernel* §2.1). The helper is always in that situation: it
  marks through an `O_RDWR` event descriptor while the daemon holds a copy of it.
- **The price of that flag.** Without `SURV_MODIFY` any write cleared an ignore mark, so a punch
  that forgot to clear it repaired itself. With the flag it does not: invariant M3 exists because of
  it (*kernel* §2.2). The kernel still reclaims an evictable mark under memory pressure.
- **The response errno.** `FAN_DENY` with an errno in its top byte is accepted for exactly
  `EPERM, EIO, EAGAIN, EBUSY, ETXTBSY, ENOSPC, EDQUOT` (Linux 6.14 and later). Any other value makes
  the response `write()` fail with `EINVAL` and leaves the opener suspended until the group closes —
  and the values outside the set (`ENOENT`, `ECONNRESET`, `ETIMEDOUT`) are exactly what a network
  source produces (*kernel* §5). The daemon reports only accepted values, the helper clamps anything
  else to `EIO`, and if the kernel refuses the response anyway (an older kernel), the helper writes
  a plain `FAN_DENY`, which the opener sees as `EPERM`.

### 4.4 The helper's own opens are events too

fanotify does not exempt the listening process: an `open()` of a file inside a marked directory by a
thread that must also answer events waits for itself forever (*kernel* §7). So the helper opens no
file to do its marking. It marks through descriptors it is handed (the event's own, or one the
daemon sent), walks trees by opening directories only (a directory open raises nothing without
`FAN_ONDIR`, *kernel* §4), and clears ignore marks by name relative to a directory descriptor.

The one file it opens is an object asked for by file handle (`OpenByHandle`, §11). That open can
raise an event in the helper's own group. The event loop allows every event that carries the
helper's own pid at once, before the worker pool: decided like any other, a placeholder's would wait
for a fill whose report the waiting connection thread itself would have to read. The helper reads
nothing from such a file and hands it to its owner's daemon (*kernel* §15).

## 5. An intercepted open

### 5.1 What the helper decides

1. An application calls `open()`. The kernel suspends it and queues `FAN_OPEN_PERM` with an `O_RDWR`
   event descriptor.
2. The helper's own open is allowed at once (§4.4). Otherwise a worker `fstat`s the descriptor.
   Anything but a regular file is allowed. Otherwise the file's owner is the user whose daemon will
   be asked, and the worker decides:
   - **the owning daemon's own open** — allowed, before anything else. The exemption is narrow: the
     opener's pid is the pid `SO_PEERCRED` reported for the owner's current top connection (§10.2),
     and that user has a registered root. Recovery and placeholder construction rely on it;
   - **neither a state nor an item id** (a file the user created in the folder) — allowed, with no
     ignore mark: a placeholder still being built looks the same;
   - **`hydrated`** — the helper adds an ignore mark through the event descriptor, **reads the state
     again**, and allows. The mark stays only if the file still reads `hydrated` and none of its
     owner's folders was unregistered since the event was read. If one was, the mark comes off and
     the open is allowed without it. If the file no longer reads `hydrated`, the mark comes off and
     the file is decided again from what it reads now;
   - **`online-only`, `hydrating` or `dehydrating`** — step 3;
   - **an item id with no state, an unknown state, or unreadable attributes** — denied `EIO` and
     logged. It is a file of ours whose content may not be there, and it must not read as zeros.
3. The helper coalesces by `(st_dev, st_ino)`: an existing job for the inode gains a waiter;
   otherwise a new job sends `HydrateRequest{req_id}` with the descriptor (`SCM_RIGHTS`) to the
   owner's daemon. At most 64 requests are outstanding per connection; a job beyond that is
   enrolled, its openers stay suspended, and each returning credit sends the oldest (§10.3). At most
   8192 opens wait for one user's daemons; one more is denied `EAGAIN` (limitations log F208).
4. The daemon fills the file (§6) and answers `HydrateDone{req_id, errno}`.
5. On success the helper reads the state again from the event descriptor. Only if it says `hydrated`
   does it add the ignore mark — read back as in step 2 — and allow every waiter; otherwise it
   denies them all `EIO`. On failure it denies every waiter with the daemon's errno, clamped to the
   accepted set (§4.3).

The event loop waits on nothing but the group. Each event goes to a **pool of 64 workers with a
1024-deep queue**; a full pool answers `EAGAIN` rather than growing. A worker's decision is an
`fstat`, an `fgetxattr` and a hash lookup, and at most a bounded wait for a daemon that is away
(§5.2); the download happens in the daemon, with no time limit. Measured: 3000 concurrent opens on
each filesystem, 3000 filled, none refused, a peak of 69 threads and under 4 MiB of memory
(*kernel* §11.3, §11.4).

**An open for writing** is decided the same way: the event carries no open flags. In a read-only
account's folder it never reaches the helper, because the `0444` mode refuses it first. In a
read-write one it does, and the placeholder is filled before the open returns: a write to a
placeholder downloads it first. `open(O_TRUNC)` of a placeholder therefore downloads the whole file,
which the kernel then truncates (limitations log P6); `truncate(2)` by path opens nothing, fills
nothing, and the daemon puts the size back ([writes.md](writes.md) §4.3).

### 5.2 Waiting for a daemon that is not there

If the owner's daemon is not connected, a worker holds the open up to **30 s** for it to connect,
then denies it `EIO` — but only for a user who has a registered root (anyone else is denied at
once), and at most 8 workers wait for any one user and 32 for all users together; beyond either cap
the open is denied `EIO` at once. Without the caps, one user opening another's placeholders while
that user's daemon was down could park every worker.

### 5.3 What an opener can receive

| Situation | The opener gets |
|---|---|
| The fill succeeds | the real content |
| The download fails after its retries (network, a missing item, a hash that does not match twice) | `EIO`; the file is `online-only` again, and the next open tries again |
| The disk fills up during the fill | `ENOSPC` (or `EDQUOT`); the file is rolled back |
| The daemon is not running | a wait of up to 30 s, then `EIO`; `EIO` at once beyond the caps (§5.2) |
| The daemon's connection ends mid-fill | `EIO` for its own waiters only |
| The helper's pool is saturated, or 8192 opens already wait for the owner's daemons | `EAGAIN` (neither reached at 3000 concurrent opens) |
| The helper is out of file descriptors | `EPERM` from the kernel, or `EIO` from the helper — never an unfilled file |
| The file is leased by someone (a free-up's punch, or another program) | `EPERM` at once, from the kernel (§4.3) |
| An open through a read-only mount (a Flatpak app with `home:ro`, `ProtectHome=read-only`) of any file whose ignore mark is not in place | `EPERM` at once: the kernel cannot open the `O_RDWR` event descriptor on that mount (issue #218) |
| A second open or `exec` of an executable in the folder while it runs, without its ignore mark | `EPERM` (`ETXTBSY` on the event descriptor; issue #218) |
| A managed file with an unreadable or unknown state | `EIO`, logged |
| Kernels before 6.14 | `EPERM` wherever an errno would have been sent |

## 6. Filling a placeholder

### 6.1 The steps

From step 1 on, the same code runs for an intercepted open and for "download now" (`Hydrate`, §6.5).

0. **Look again.** Under the per-inode lock, read the state from the descriptor. A request can wait
   a long time before its turn — for a fill slot, or in the helper for credit — and the file may
   have been filled meanwhile. For an open, a file that reads `hydrated` is answered success at once
   and **not touched**: with a matching stamp it is simply there; with a stamp that does not match
   it was edited in place, and that edit is the only copy. A file with no state, or one that cannot
   be read, is answered `EIO`. Only `online-only`, `hydrating` and `dehydrating` are filled.
1. Read the item id. Set `state=hydrating`, `fsync`. A file found in any state but `online-only` may
   carry an ignore mark, and a failed fill empties the file, so the way is cleared now by M3's local
   rule, before the first byte is fetched. If it cannot be cleared, the state is put back and the
   answer is `EIO`, with nothing fetched or written.
2. Ask the content source for the bytes from the current offset. It answers with a stream, **the
   offset that stream actually starts at**, the current size and time, and the version (cTag and
   hash) when it knows them.
3. Write sequentially into the event descriptor through a 256 KiB buffer; memory does not grow with
   file size. The hash is computed over the bytes as they are written. **A resumed stream is written
   at the offset the source says it served, or not at all.** A server may answer a `Range` request
   with `200` and the whole body; written at the resume offset, that would put the file's beginning
   in its middle and report a success. The Graph client skips the bytes already on disk in that
   case, and the fill fails (`EIO`) on any stream that starts anywhere other than where it was asked
   to.
4. A declared size of 0 against a placeholder with a non-zero size is refused `EIO`: nothing
   corroborates that answer and nothing could retry it. A file genuinely emptied in the cloud is a
   metadata change, handled by the sync.
5. Verify (§7.2). Then commit: set the file's length to the remote size (the current version wins),
   set the remote time (`futimens`), `fdatasync`, write the version's cTag, remove the checkpoint,
   write the stamp, `fsync`, write `state=hydrated`, `fsync`. A time the local filesystem cannot
   represent does not fail the fill: the content is correct, and the stamp records the time the file
   actually has.
6. Answer success.

### 6.2 The commit point

**`state=hydrated` is the commit point, and it is written last.** It is what makes the helper allow
the open and add an ignore mark, so nothing that can still fail may come after it. Written before
the stamp, a full or failing disk would produce a file marked `hydrated` that holds zeros, which the
helper would allow and ignore-mark. For the same reason no step of a fill is best-effort, except the
time: a swallowed error is a success report for content that is not there.

### 6.3 Rolling back

Only a file still `hydrating` is rolled back; a file that reads anything else is no longer this
fill's to empty. The roll-back runs in this order:

1. drop the checkpoint, unless its prefix is kept (§7.4): a count of bytes never outlives the bytes
   it counts;
2. punch everything past the kept prefix, or the whole file; restore the original size and time;
3. `fsync`;
4. `state=online-only`, and remove the stamp.

The state is changed last. A failure or a crash before step 4 leaves the file `hydrating`, which
never reads as content: the helper fills such a file on its next open, never marks it, and startup
recovery resets it. So at no point does a file read `online-only` or `hydrated` while it holds
something else than that state says — except a kept prefix, which its checkpoint counts and the next
fill checks against the file's hash. A free-up and recovery empty a file in the same order
(`hydration/demote.rs`).

The errno answered is one the kernel will deliver: a local error in the accepted set (§4.3) travels
as itself — a full disk as `ENOSPC` or `EDQUOT` — and anything else becomes `EIO`. A panic inside a
fill is caught and answered `EIO` rather than leaving the opener suspended.

### 6.4 Concurrency and the per-inode lock

Every transfer of an account takes a slot of that account's **transfer pool**
(`crates/konedrive-graph/src/pool.rs`): fills on open and `Hydrate`, pinned downloads, replacements
of changed files, thumbnails, uploads and metadata changes alike; the delta feed and the account's
information stay outside it. OneDrive publishes no limit and throttles an account as a whole with
`429`/`503`, so the pool finds its own level:

- it starts at 16 (or the ceiling, if lower) and grows by one slot for each successful transfer made
  while work waits and every slot is busy, up to the ceiling (`[transfers] max` in `config.toml`, 32
  by default, 1 to 256). Latency is not measured: only a throttle or the ceiling stops it;
- a `429` or `503` on the account's requests — the delta feed and the pre-authenticated download
  URLs included — halves it, once per burst, and nothing gets a slot for the whole `Retry-After`
  (10 s when the answer gives none), not even an open. At and above the size the throttle came at,
  the pool then grows by one slot only after as many successes as it has slots, until five minutes
  pass without a throttle.

A **large** file is one of 100 MiB and up, by its placeholder's size, the new version's for a
replacement, or the local file's for an upload. `[transfers] large` (4 by default, clamped to
1…`max`) limits the streams of large **sync transfers** that run at once per account: pinned
downloads, replacements and uploads, each stream in a pool slot (a download in parts runs several,
§7.5). The other slots go to small files, and a large one waiting for the limit lets the small ones
behind it go. A file being opened, and `Hydrate`, is outside the limit and its count, and is not
among `LargeStreams`; it still takes a slot of the pool and counts in `PoolInUse`.

A file being opened goes first: it may use two reserve slots above the pool, and while any open
waits or runs nothing else takes a new slot. Then metadata changes, then background downloads and
uploads, one to each in turn. A pause holds back everything but opens. Every number here is chosen,
not measured against a real account. A request is taken off the daemon's request queue — at most as
many at once as the helper's credit (§10.3) — routed to its account ([accounts.md](accounts.md)
§3.4), and only then waits for a slot of that account's pool.

Fills, `Hydrate`, free-ups and recovery, and the steps of the sync and of the uploads that change a
file, take a **per-inode lock**, keyed by `(st_dev, st_ino)` read from the descriptor — never by a
name, so two links to one file serialise too. Most steps of the sync try it without waiting and
leave a file whose lock is held for the next cycle.

### 6.5 Download now (`Hydrate`)

"Download now" does not go through interception. The daemon opens the file inside the root —
`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS` from the root's own
descriptor, and only while the folder still carries this root's id — then takes a pool slot and the
per-inode lock, reads the state again under it, and fills the file with the same code (§6.1). The
order matters:

- opening first and locking second avoids a deadlock against the fill the open itself would trigger
  in an intercepted folder;
- opening beneath the root's descriptor, rather than opening a checked path by name, means a
  directory renamed in between cannot make the fill write outside the root.

A file with no konedrive state is refused `NotManaged`. A file labelled `hydrated` with a matching
stamp, or with no bytes, is already there; with no stamp it is filled (nothing this daemon wrote can
be in that state); one whose stamp does not match is refused `ModifiedLocally` and never
overwritten. A way that cannot be cleared (§6.1 step 1) is `NoHelper` in an intercepted folder with
no link to the helper, whether one runs or not, and in a folder without interception when a helper
is bound and this daemon has no link to it. A helper that refuses the clearing is a plain failure.

### 6.6 Content sources

```text
trait ContentSource {
    fetch(item_id, from, end: Option) -> Fetched { served_from, size, mtime, version: Option<{ ctag, quickXorHash: Option }>, stream }
}
```

- **Graph** (`hydration/graph_source.rs`) serves a folder that shows OneDrive (§7).
- **LocalDir** serves a local directory, for tests and for the developer's local folder
  (`PopulateFromDirectory`, §14.3). It is held in memory only: after a daemon restart an intercepted
  open of an `online-only` file there is denied `EIO`, and `Hydrate` answers `NoSource`, until the
  folder is populated again. It refuses a source file that leads into the sync folder or carries
  konedrive's own attributes, when mirroring and again when reading: such a file would copy a
  placeholder's zeros.
- Every source reports `served_from`, where its stream really begins; `end` bounds a piece (§7.5).

## 7. Downloading from OneDrive

### 7.1 The request

Each fetch asks `GET /me/drive/items/{id}` for fresh metadata: size, cTag, `quickXorHash`, and the
pre-authenticated `@microsoft.graph.downloadUrl`. It streams from that URL, falling back to
`GET /me/drive/items/{id}/content` (which redirects to the same kind of URL) only when the metadata
carries none. The content of a 2 GB file is one request and one response, except for a large pinned
file, which goes in parallel parts (§7.5). `429` and `503` are honoured with their `Retry-After`
([sync.md](sync.md) §4.3).

### 7.2 Verification

`quickXorHash` is computed over the bytes as they are written, and **a file whose version has a hash
is marked `hydrated` only if it matches**. On a mismatch the file returns to `online-only` and the
opener gets `EIO`: nobody sees wrong bytes. The implementation (`konedrive-graph/src/quickxor.rs`)
follows Microsoft's published algorithm. The version of the first answer is the one the file must
end up as. A later answer for another version means the file changed in the cloud mid-download. A
changed version and a hash mismatch share one start-over: the first of them starts the download
over, the second is `EIO`. A file Graph hands out without a hash is downloaded unverified and logged
(limitations log F34). So is one it hands out without a cTag, which is not logged.

### 7.3 Resume within a run

A read error, a short stream or a fetch that fails in passing (the network, a `5xx`) is a break, not
the end: the next fetch asks from where the bytes stopped, with `Range: bytes=<written>-`, after
200 ms, then 400 ms. The third break of a download ends it with `EIO`; the count is the whole
download's, and a start-over (§7.2) does not reset it. Every fetch asks for the metadata again, and
so for a fresh download URL. A URL that has expired (`401`, `403`, `404`, `410`) is replaced once
within a fetch; a second expiry is a break.

### 7.4 Resume across a restart

Every **16 MiB** the fill `fdatasync`s the file and records `user.konedrive.progress` =
`<ctag> <bytes>`, when the version has a hash to verify the result by. A fill that gives up, and
recovery after a crash (§9), keep that durable prefix: the file goes back to `online-only` with its
stamp removed, but only what lies past the checkpoint is punched. A fill that ends on a hash
mismatch keeps nothing. Recovery accepts a checkpoint by its byte count alone (more than 0, no more
than the file's size). The cTag is compared later, when a fill resumes and weighs the checkpoint
against fresh metadata: a checkpoint for another version, for a version with no hash, or for more
bytes than the file now has, is discarded then and the download restarts from zero. Otherwise the
fill reads the written part once to rebuild the hash state and continues from there.

Resume is safe because verification covers the whole file, including what lay on disk across the
restart: anything wrong there fails the hash, and the file is downloaded from zero. The cost is that
a file shown as `online-only` may hold part of its blocks until the next fill or a change in the
cloud (issue #221). A reconcile keeps the checkpoint too: it judges a placeholder's content by cTag
and size, not by its time, which a fill's writes change ([sync.md](sync.md) §7.4).

### 7.5 Large pinned files in parallel parts

One stream from OneDrive does not fill a fast link (about 17 MiB/s measured on a link that carries
~68 MiB/s), so a **pinned download of a large file** (100 MiB and up, by its placeholder's size)
goes in parts (`hydration/source/parts.rs`). A file being opened, `Hydrate`, a replacement of a
changed file and every small file keep one stream.

**Pieces.** The file is cut into pieces of **256 MiB** (the last one shorter). Each stream downloads
one piece at a time with a bounded range (`Range: bytes=<start>-<end>`), writes it at its offset,
and then takes the next piece not yet taken, in file order. Every piece's answer must be of the
version and the size the first answer had; another starts the whole file over, once (§7.2).

**Streams.** The file's first stream runs in the slot the pin queue took for it. It adds extra
streams, each in a large slot of the account's pool (§6.4), taken only when one is free and nothing
waits for a slot. Free slots go evenly: the next one goes to the file in parts with the fewest
streams. After each piece an extra stream gives its slot back if any transfer waits for one, or if
another file in parts that still has pieces to take has two streams fewer; a waiting transfer can so
wait for one piece to end. With `[transfers] large = 4` and nothing else waiting, one large file
runs in up to 4 streams, two in 2 each, four or more in 1 each. `LargeStreams` counts streams;
`Transfers.Downloads` shows the file once, with its overall progress, so it is one file in
`ActiveDownloads` and in `LargeFiles`. A throttle applies to each stream as to any transfer. An open
of the file waits for the whole fill, as always (§6.4).

**Checking.** QuickXorHash is positional: each byte's contribution depends only on its offset. Each
piece is hashed at its offset as it arrives, and the pieces combine into the whole file's hash at
the end, with no second read. A mismatch starts the whole file over once; a second one is `EIO`.

**Checkpoint.** `user.konedrive.progress` keeps its meaning (§7.4): the bytes on disk **from the
start without a gap**. It grows with that gap-free start once its bytes are durable, every 16 MiB,
inside the piece at the front too. A piece finished beyond a gap is not recorded: a fill that gives
up, and a restart, keep only the gap-free start, and everything past it is downloaded again (issue
#230). A restart continues from the checkpoint as a single stream does, then splits the rest.

**Breaks.** A dropped or short answer continues that piece from where its bytes stopped. Three
breaks of the same piece and the whole download fails, as three breaks fail a single stream (§7.3).

## 8. Freeing up space (dehydration)

`Dehydrate` of a file a pin keeps is refused `NotAllowed` ([pinning.md](pinning.md) §5), and in an
intercepted folder with no link to the helper the whole operation is refused `NoHelper` and nothing
changes: freed up with nothing intercepting, the file would read zeros. Then:

1. **Open the file once**, inside the root as `Hydrate` does (§6.5), and take the per-inode lock on
   that descriptor. Every step below uses this one descriptor; nothing re-opens the file by name,
   because a rename in between — an editor's atomic save, say — would otherwise send the punch to
   another file. In a folder that shows OneDrive, a downloaded file with a change waiting to upload
   is refused `NotUploaded` ([writes.md](writes.md) §11), and one whose outbox cannot be asked is
   refused too. A zero-byte file that is `hydrated` and has no stamp has nothing to free: the call
   succeeds and changes nothing (one the sync placed carries a stamp, and takes the steps below).
   Otherwise require `state=hydrated` and a stamp matching the current size and time, or refuse: no
   konedrive state → `NotManaged`; another state → `NotHydrated`; a stamp mismatch →
   `ModifiedLocally` (a local edit not uploaded is the only copy). Record the file's times, because
   the punch will change them.
2. Set `state=dehydrating`, `fsync`, and clear the way by M3's local rule: with a link, the helper
   `ClearIgnore`s the file — from now on every open reaches the helper again. **Any failure stops
   here**: the state goes back to `hydrated` and the call fails. With no link (a folder without
   interception), go on only if no helper is bound to its socket; otherwise roll back and refuse
   `NoHelper`. The helper reports a mark that was never there, or that the kernel had reclaimed, as
   success, so a failure here means something genuinely went wrong.
3. Take a write lease, `fcntl(F_SETLEASE, F_WRLCK)`. The kernel grants it only if nobody else has
   the file open — a descriptor, or a mapping whose descriptor has since been closed
   (*kernel* §12.1). Starting or forking a process does not refuse it (*kernel* §12.2). Refused →
   roll back to `hydrated` and answer `InUse`; a lease that cannot be asked for at all is a failure.
   `SIGIO` is ignored before the daemon takes its first lease, because an opener breaking the lease
   signals the holder and `SIGIO`'s default action kills it (*kernel* §12.3).
4. `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, size)`, restore the recorded times (an
   `online-only` file carries the remote time), `fsync`.
5. `state=online-only`, remove the stamp, release the lease. A failure from step 4 on leaves the
   file `dehydrating` with nothing to roll back to; recovery (§9) finishes it.

**Races.** An open arriving after step 2's `ClearIgnore` and before the lease reaches the helper,
which reads `dehydrating` and holds the open's descriptor while it asks for a fill — so the lease is
refused, the dehydration rolls back `InUse`, and the fill request then finds the file `hydrated` and
answers it as it is (§6.1 step 0). An open arriving while the lease is held is denied `EPERM` by the
kernel at once (§4.3); an opener the helper let through before the lease already refuses the lease
(*kernel* §12.5). Nothing waits on the punch, and nothing is let through onto it.

On filesystems with snapshots, or for files cloned with `cp --reflink`, punched blocks stay
referenced elsewhere and no space comes back until those references go. The file still becomes
`online-only` correctly.

"Free Up Space" for the whole folder ([desktop.md](desktop.md) §2.3) runs this per file. It leaves,
without waiting, any file that is open or whose per-inode lock is held, any file changed here or
waiting to upload, and any pinned file.

## 9. Recovery

At startup, and again after every reconnect to the helper, the daemon first re-registers an
intercepted root with the helper and **then** walks it. The order matters: a file left `dehydrating`
may still carry its ignore mark (the daemon died between the state write and `ClearIgnore`), and
clearing it needs the helper, which acts only for a user with a registered root.

The walk runs from the root's own directory descriptor. Every entry is opened with `openat` from the
directory it was listed in (`O_NOFOLLOW`) and accepted only if it is a regular file or a directory
on the root's own device; one file is open at a time, so the walk does not run into the descriptor
limit, and it stops at 128 levels. For each file in `hydrating` or `dehydrating`:

1. try the per-inode lock **without waiting**; if it is held, the file is being filled or freed up
   right now — by a fill from the previous connection, say — and is counted `busy` and left to it;
2. reopen the file writable through its descriptor (the same inode, whatever its name leads to by
   now) and close the read-only one;
3. clear the way by M3's local rule, on that descriptor;
4. take the write lease (a refusal is retried for about 75 ms, long enough for a copy of the
   read-only descriptor inherited by a process being spawned to go) and **read the state again**: a
   file finished meanwhile is left as it is;
5. punch it (past a usable checkpoint for a `hydrating` file, §7.4, else entirely), restore the time
   it had, `fsync`, set `state=online-only`, remove the stamp.

A refusal anywhere — the helper's, the lease's, the punch's — leaves the file in the state it was
found in, to be retried at the next start. A refused lease means something has the file open, most
often the very open that will fill it, so it counts as `busy`, not a failure. A folder registered
without interception is recovered by the same rule: when a helper runs that this daemon has no link
to, its files are left as found and counted `deferred`, and recovered once a link exists. The report
distinguishes `scanned`, `reset`, `failed`, `skipped` (a subtree on another filesystem or deeper
than 128 levels, and anything that could not be opened or read), `busy` and `deferred`, so that
"nothing needed fixing" and "nothing could be fixed" do not read alike. The report gives one
outcome, the first that applies: `failed > 0` publishes `Folder.State = error`; `skipped` puts a
note in `LastError`; `busy` is only logged; `deferred` puts a note in `LastError`. So a `deferred`
count leaves no note when any file was `busy`.

## 10. The helper–daemon link

### 10.1 Transport and messages

`/run/konedrive/helper.sock`, a `SOCK_SEQPACKET` socket. The helper learns the peer's uid from
`SO_PEERCRED`, never from a message. Messages are small and versioned, each one JSON in a datagram
of its own, at most 64 KiB (`crates/konedrive-proto`; version 2 since `OpenByHandle`); descriptors
travel with `SCM_RIGHTS`, at most one with a message, and arrive close-on-exec.

| Direction | Message |
|---|---|
| daemon → helper | `Hello{version}` (its first call); `RegisterRoot{root_id}` + directory descriptor; `UnregisterRoot{root_id}`; `MarkDir` / `UnmarkDir` + directory descriptor; `MarkFile` + file descriptor; `ClearIgnore` + file descriptor; `HydrateDone{req_id, errno}`; `OpenByHandle{handle_type, handle}` + directory descriptor (uploads, [writes.md](writes.md) §8.2) |
| helper → daemon | `Welcome{version}` (unprompted, on accept); `Ack{errno}` — one per daemon message, in order, carrying the object's descriptor when it answers an `OpenByHandle` with 0; `HydrateRequest{req_id}` + the event descriptor |



### 10.2 Connections

No handshake gates anything: `SO_PEERCRED` is the authority, and a greeting could only carry a
version the peer might lie about. The helper greets first; the daemon sends `Hello` as its first
ordinary call, so the version check runs both ways: the daemon hangs up on a `Welcome` with another
version, and the helper closes the connection, with no `Ack`, on a `Hello` with one. A peer that
sends no `Hello` is served.

Replies are paired by order — every daemon message gets exactly one `Ack`, and `HydrateRequest` is
told apart by type — so a daemon call is bounded (30 s; 120 s for `RegisterRoot` and
`UnregisterRoot`, which walk the tree) and a timeout ends the connection: a reply dropped on the
floor would pair every later one with the wrong call. After losing the link the daemon publishes the
loss at once and reconnects with a backoff from 1 s doubling to 30 s; fills already running finish
on their own, and a request from a connection that has ended is not filled: its opener was answered
when the connection went.

Every job, suspended open and pid exemption belongs to one **(uid, connection)**; a connection
ending denies only its own openers `EIO`. Each uid has a **stack** of live connections: fill
requests and the pid exemption go to the top one, and a disconnect removes its connection wherever
it sits, handing control back to the next one. So a second process of the same user that connects
and hangs up cannot evict the live daemon. No uid may hold more than **16** connections: each costs
two threads and a few descriptors, and the socket is open to every local user.

Because requests go to one connection per uid, a daemon keeps one link for all of its accounts, and
decides itself which account's folder a request's file is in ([accounts.md](accounts.md) §3.3,
§3.4). The helper knows users, not accounts.

### 10.3 Flow control

At most **64** `HydrateRequest`s are outstanding on a connection
(`konedrive_proto::MAX_OUTSTANDING_HYDRATIONS`), and the daemon's request queue is exactly that
deep: a contract between the two ends. Beyond it a request is enrolled in the helper with its
openers suspended, and each `HydrateDone` hands its credit to the oldest one waiting. Without the
credit, the two bounded queues deadlocked under a burst; refusing beyond it instead of queuing gave
`EAGAIN` to most of 3000 opens (*kernel* §11.4).

Everything the helper sends goes through a per-connection outbox with its own writer thread, so no
worker ever blocks on a socket the peer controls. The outbox keeps room for the helper's requests
apart from room for `Ack`s (128), so replying never costs a live connection. A blocked send is not
evidence of a wedged daemon; silence is: the connection ends only when a send is blocked *and* the
daemon has sent nothing for 60 s.

## 11. What the helper allows each user

One helper serves every user, and its socket is open to all of them (mode `0666`). Every request is
authorised by the peer's uid and by who owns the object it is about; the request carries a
descriptor, not a path.

- **`RegisterRoot`** — the directory is owned by the peer (`EPERM` otherwise), on a filesystem the
  helper accepts (§14.2), and neither inside nor containing another registered root, whatever that
  root's id. An entry of the peer's own that stands in the way is dropped instead of refusing when
  its directory is gone: its stored path leads nowhere or to another directory, or it is an entry
  from before handles were kept (§12) on the offered directory's device and inode. A folder removed
  without being unregistered leaves such an entry. Another user's entry always refuses: a peer who
  can write above another's root could otherwise have its entry dropped. A root id that another uid registered is refused `EPERM` (ids are chosen by the client,
  so without this one user could replace another's registration). A path that is not valid UTF-8 is
  refused. The id must have the form the daemon mints (a version 4 UUID in its canonical text), or
  the request is refused `EINVAL`; a uid that already holds 32 roots is refused another (`EDQUOT`;
  limitations log F208), and can still register one it holds again. The whole tree is walked and
  marked, and the ignore mark of every file walked is cleared. An id the uid already holds,
  registered onto another directory, replaces the old entry, and the old directory's tree is
  unmarked as by `UnregisterRoot`. If the old directory is still at its path and the new one lies
  inside it or contains it, the request is refused `EINVAL` instead: unmarking the old tree would
  leave the shared part unmarked until the new walk. If the id's entry changed while the old
  directory was being opened, the request is refused `EAGAIN`.
- **`UnregisterRoot`** — only a root the peer's uid owns, from any of its connections (roots outlive
  connections), under whatever id it was registered (the form is not asked here). It removes the
  marks the helper placed on the tree, including files' ignore marks, as well as the entry: removing
  the entry alone would leave the tree intercepted with no daemon to ask, and every placeholder
  would answer `EIO`. The walk is best effort: what it cannot unmark — a root renamed or moved
  meanwhile, a single failure — is logged, and the answer is still success, so marks can be left
  behind. M3's local rule does not depend on them being gone.
- **`ClearIgnore`** — a regular file the peer owns, anywhere. Removing an ignore mark can only send
  the file's next open back to the helper; and a folder registered without interception, which must
  ask before every punch (M3), may hold no root with the helper at all.
- **`MarkDir`, `UnmarkDir`, `MarkFile`** — the object is owned by the peer and lives on the device
  of one of that peer's roots. Without the owner check, a user could make the helper intercept opens
  of files they do not own (say `/etc/passwd`) and stall system processes. The check is scoped to
  the device, not the root: keeping every operation inside the root is the daemon's job.
- **`OpenByHandle`** (uploads, for an object that left its folder: [writes.md](writes.md) §8) — the
  directory is the peer's own, on the device of one of its roots. The object the handle names is
  looked at through `O_PATH` first, and handed back only if it is the peer's own regular file or
  directory, on that directory's device, still linked, and carrying `user.konedrive.item-id`:
  `EPERM` otherwise, `ESTALE` for the peer's own deleted object. So a user reaches such an object of
  theirs even in a directory they cannot enter. A file comes back `O_RDONLY | O_NONBLOCK`, since
  under the unit the helper cannot open a user's file for writing; the daemon, its owner, reopens it
  for writing (*kernel* §15).
- **The pid exemption** (§5.1) — only for files of a uid that holds a registered root, and only to
  that uid's top connection. The helper's own opens are let through apart from it (§4.4).
- **`HydrateRequest`** descriptors go only to the daemon of the file's owner, so the `O_RDWR`
  descriptor grants nothing that user could not already open.

The VM suite runs a second, unprivileged uid against the helper: holding nothing but the socket, it
can neither force-allow another user's suspended opens by guessing request ids, nor unregister their
root by id, nor unmark their directories.

## 12. Helper startup and persistence

Registered roots live in `/var/lib/konedrive/roots.json` (uid, path, device, inode, file handle,
root id; mode `0600`). The file handle is what says that a directory is the registered one: it
carries the inode's generation, which a directory that only got a removed root's inode number does
not share. An entry written before handles were kept gets its directory's the first time a start of
the helper finds it; until then it is told by device and inode. A corrupt file is moved aside, and one that cannot be read at all leaves the helper
starting with no roots rather than not starting: with no interception every placeholder reads zeros,
so not starting is worse.

On startup the helper first builds its worker pool, binds its socket and prepares its accept thread
— everything that can fail and end the process — and only then marks anything, because exiting over
marked trees would release every open suspended meanwhile. It then checks each root again: the
nesting rule (in a stable order, so the same root wins every boot), and, on the re-opened directory,
ownership and the filesystem type. It walks each tree **depth-first**, marking each directory before
descending into it and clearing the ignore mark of each file by name, to at most 128 levels. Files
are never opened. Measured: **1.03 s for 10 000 directories and 100 000 files** on a cold cache
(*kernel* §11.7).

The walk resolves each component beneath a held `/` descriptor with
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, and below the root additionally
with `RESOLVE_NO_XDEV`. `RESOLVE_NO_XDEV` is deliberately not applied on the way *to* the root: the
sync folder normally lives under `/home`, a separate mount or subvolume. The directory reached must
also have the registered device and inode. That is not proof that it is the registered directory,
because ext4 and xfs give a deleted directory's inode number to the next one made; the file handle
is, and a registration goes by it. A start covers a directory whose handle differs all the same,
and says so: a root left unmarked reads zeros, and the directory is its owner's either way. A directory that cannot be opened or marked never ends a walk:
everything reachable is marked, each failure is logged, and the root is flagged degraded in the
helper's log, the only place that says so (issue #219). Measured against 57 766 directory renames
racing the walk: the helper never followed a symlink out of the root and marked every directory that
stood still.

The systemd unit (`packaging/systemd/konedrive-helper.service`) starts the helper before the display
manager, restarts it always, and gives it two capabilities, no network, a read-only view of the
system and of `/home`, and `LimitNOFILE=65536`; `SECURITY.md` says what that sandbox does and does
not prevent. `ProtectHome=read-only` does not stop fills: the daemon writes through the event
descriptor, which belongs to the opener's mount (*kernel* §11.6).

## 13. The helper must not die

When the helper's fanotify group closes with opens still suspended — the process is killed or
crashes — **the kernel answers every one of them with "allow"**, and every opener reads whatever the
placeholder holds: zeros. Measured: 200 opens suspended on a slow source, `SIGKILL`, all 200 released within 525 ms,
every one reading zeros (*kernel* §11.6). Nothing the helper does can change what the kernel does on
close, and queuing beyond the credit (§10.3) means every open in flight is exposed. So the
requirement is that the helper does not exit with an open unanswered. What defends it:

- a bounded worker pool, so running out of threads is `EAGAIN`, not a panic in the event loop;
- a panic on a worker is caught, its opener denied `EIO`, and the pool kept at strength; a panic on
  a connection, or on the thread that writes to it, runs that connection's clean-up, which denies
  its openers `EIO`;
- a panic in the event loop is caught: the open in hand and the events read with it are denied
  `EIO`, and the loop reads on; a panic on the thread that accepts connections closes the connection
  in hand, and the thread accepts again a second later;
- running out of descriptors is survivable: the event loop keeps the group and retries every 50 ms,
  the accept loop backs off, and openers the kernel could not hand over are denied (`EPERM` by the
  kernel, `EIO` by the helper), never allowed;
- an event whose descriptor the kernel could not open — through a read-only mount (`EROFS`), of a
  running executable (`ETXTBSY`) — is that one event's failure, already answered `EPERM` by the
  kernel; only an error of the group's own descriptor (`EBADF`, `EINVAL`, `EFAULT`) ends the loop;
- a daemon disconnecting, dying or wedging costs only its own connection, and no uid holds more than
  16 connections; one that takes its requests and answers nothing costs only its own user: at most
  8192 opens wait for one uid's daemons at once, and a further open of that uid's files is refused
  `EAGAIN` (limitations log F208);
- no lock is held across a blocking call on a socket a peer controls;
- the fault-injection hooks the suite uses to prove the unwind paths exist only in builds with the
  `fault-injection` cargo feature; the installer refuses a helper that contains them.

**An ordinary stop answers first.** `SIGTERM` and `SIGINT` — what systemd sends at a restart or an
upgrade — are blocked in every thread and read by the event loop from a descriptor. On either, the
loop hands nothing more to a worker and, before it lets go of the group:

- answers every open the helper holds, and those the stop answers itself are denied `EIO`: the ones
  waiting for a daemon's answer are taken out of the table, which enrolls nobody from then on; the
  ones queued for a worker are denied by the workers; a worker waiting for a daemon that is not
  connected stops waiting and denies its own. An open a worker had begun deciding, or one a
  connection thread took on its daemon's answer, is answered as usual, and may be allowed because
  the content is there;
- reads what is still in the kernel's queue and denies each of those `EIO`, until a read finds the
  queue empty. `EAGAIN` from a read is not taken for that while the group still has something to
  read: it is also what the read of a leased file's open returns (§4.3). The helper's own opens, for
  a daemon's `OpenByHandle`, are allowed as the loop allows them;
- exits with status 0, once no open read from the group is without its answer. Every such open is
  counted from the read to the written answer, in whichever thread holds it, and the stop ends on
  that count, not on having asked.

It waits for nothing outside the helper — not for a daemon, not for a download. The helper exits
with status 1 instead, after at most 5 seconds, when the count is not zero by then, when no read
found the kernel's queue empty by then (a program that reopens in a loop can keep it so), when a
read of the group failed for good, or when the stop panicked: the panic is contained and the stop
run once more. The kernel lets through what has no answer. One line in the log says how many opens
were answered at the stop and, at status 1, which of these it was.

The two signals are blocked from the helper's first instruction, so a stop that comes during the
walk at startup (§12) is seen only when the event loop begins; a walk longer than systemd allows a
stop ends in a kill, with the outcome of a kill. Measured:
200 opens suspended on a slow source, `SIGTERM`, all 200 denied `EIO` and none reading zeros, the
helper gone with status 0 within 20 ms; 8 opens parked with no daemon connected, all 8 denied `EIO`
at the signal, the helper gone with status 0 within 5 ms (*kernel* §11.6). The daemon is told nothing: it sees its link drop, as at any other
end of the helper (§10.2).

The VM suite proves the panics of a worker, a connection, the event loop and the accept thread, the
exhaustion of descriptors, the two events that cannot be opened, a daemon's death and the ordinary
stop. Nothing in it reaches the writer thread's panic, the full pool, the caps of 16 connections and
8192 opens, or a stop that runs into its bound. The remaining exposure is recorded as limitations
log Z1: a helper that crashes or is killed releases every open waiting at that moment, an open that
arrives between an ordinary stop's last read and the group closing is let through, and while no
helper runs nothing is intercepted.

## 14. Registration and modes

### 14.1 Registering a folder

`Folder.Register(path)` binds an **empty** directory to the account's drive. It is refused, with a
named error, when the account is not signed in (`NotSignedIn`), when it already has a folder
(`AlreadyRegistered` — each account keeps one), when no helper is connected (`NoHelper`: a
placeholder nobody intercepts reads as zeros), when the folder is, is inside, or contains another
account's (`Overlaps`), or when the folder fails the checks below (`NotEmpty`, `Unsupported`).
"Empty" applies to a first registration only: a folder that already carries a well-formed root id is
one this daemon claimed before, and restarts rely on it — unless it also carries the drive of
another account, which is refused `NotEmpty` ([accounts.md](accounts.md) §6.3).

The registration is written to `config.toml` *before* the helper is told, and refused if it cannot
be; a failure after the helper may have stored it is undone at the helper and in `config.toml` — or,
when the helper cannot confirm it let go, kept intercepted with `Folder.State = error`, because a
folder the helper may hold must never be one the daemon treats as unintercepted. A `config.toml`
that exists but cannot be read is never overwritten with defaults. Calls that change an account's
registration take turns.

At startup, and after every reconnect, the daemon re-registers the root with the helper and then
recovers it (§9). A restored intercepted root is held as registered before the helper is back:
`Folder.State` is `waiting`, and `error` once the helper is known to be not installed, stopped or
failed. So it answers `AlreadyRegistered` to a second registration and `NoHelper` to a Forget.

### 14.2 Filesystem requirements and the probe

Required: sparse files with `FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE`, `user.*` extended
attributes, `O_TMPFILE` (every placeholder is built with it) and file leases (dehydration and
recovery cannot empty a file safely without one). Tested: **Btrfs, ext4, XFS**. The helper refuses
by type (`fstatfs`): FAT and exFAT (no sparse files, no attributes), and network and FUSE
filesystems — NFS, SMB/CIFS, FUSE, AFS, Ceph — where files can change on another machine, bypassing
interception. It is a deny-list: any other filesystem that passes the probe registers.

At a first registration the daemon creates a nameless `O_TMPFILE` file in the folder and exercises
each feature: size, hole punching, a `user.*` attribute, a write lease. A feature the filesystem
does not support is named in the refusal. A named probe file would outlive a crash and make the
folder permanently "not empty", so the probe never has a name. The helper probes nothing itself. A
symbolic link is refused as a root; `konedrivectl` does not resolve a path's last component, so a
link given there reaches the daemon as a link. A folder that already carries its root id is not
probed again: a OneDrive folder is locked read-only, so the probe's write would fail. A filesystem
that lost a feature since is found by the first free-up or recovery that needs it.

### 14.3 Without interception (developer's mode)

`Folder.RegisterWithoutInterception(path)` makes a folder with the daemon's checks and the same kind
of placeholders, but **nothing intercepts opens**: files read as zeros until they are downloaded by
hand with `Hydrate`. It needs no helper and no sign-in, and it always makes a *local* folder, filled
from a local directory with `PopulateFromDirectory`, never one that shows OneDrive
([sync.md](sync.md) §3). The helper's list of refused filesystems (§14.2) is not asked for it. It
exists for development and for tests where no helper runs. The window does not offer it;
`konedrivectl sync register-without-interception` is the only way in, and `sync status` repeats the
cost on a line of its own. Even here, emptying a file follows M3's local rule: a helper may be
running and may have marked the file while it belonged to an intercepted folder.

### 14.4 When the helper arrives later

A folder registered without interception *because no helper was connected* records
`upgrade_when_helper = true`. When a helper connects, such a folder is switched, in its turn among
the calls that change the registration: its sync stops, the switch is written to `config.toml`, the
helper registers the root (its walk marks every directory), recovery runs, and the sync starts
again, intercepted. The same call made while a helper *was* connected was a deliberate choice and is
never switched. A OneDrive folder left without interception by an older version is switched whatever
it recorded. A switch that fails leaves the folder as it was and says why in `LastError` — except
when the helper may still hold the root, in which case the folder stays intercepted with its sync
stopped until the next connect.

### 14.5 Forget

`Folder.Unregister` forgets the folder and leaves every file's content and state as they are. An
intercepted folder is forgotten **through the helper or not at all**: with no link it is refused
`NoHelper`, because dropping it locally while the helper keeps its marks would leave placeholders
that answer `EIO` with no daemon to fill them. Two cases count as forgotten: the helper answers that
it holds no such root, or no root id is recorded or readable for the folder. A folder with changes
waiting to upload is refused `PendingUploads`. For a folder that shows OneDrive, Forget also takes
the read-only lock off ([sync.md](sync.md) §11), drops the tree store, and removes the Baloo
exclusion the daemon added ([desktop.md](desktop.md) §9).

## 15. Error summary

What an opener receives is in §5.3; this table adds what follows it.

| Situation | What the application sees | Recovery |
|---|---|---|
| The daemon crashes during a fill | `EIO` for its waiters | recovery at the next start (§9) |
| A fill panics | `EIO` | the file is left `hydrating`; the next open fills it, recovery clears it |
| The helper's connection is lost, the helper still runs | opens wait for a daemon (§5.2) | the daemon publishes `error` at once, reconnects, re-registers and recovers |
| The helper is not running yet (early boot) | placeholders read as zeros | the helper starts before the display manager |
| A managed file with an unreadable or unknown state | `EIO`, logged | by hand |
| The file is renamed or deleted during a fill | unaffected (work goes through the descriptor) | a deleted file's download completes into the unlinked inode |
| A placeholder moved out of a read-write folder | reads zeros until the daemon marks it again (M4) | marked, downloaded where it went, then deleted in OneDrive ([writes.md](writes.md) §8) |
| The waiting program is killed | its open is abandoned | the fill continues; other waiters are unaffected |

## 16. Known gaps

- **Moves into a new directory and out of a read-write folder are covered after the fact** (M1, M4):
  a new directory within milliseconds, a placeholder that left the folder after a quiet spell of
  2 s. Opened in between, it reads zeros (limitations log Z2, Z3, F120).
- **Fail-open windows.** The helper not running, and the moment it crashes or is killed (Z1). The blast radius
  is the sync folder only.
- **Eager hydration.** Anything that opens a placeholder downloads it — thumbnailers, indexers,
  `open(O_TRUNC)`; there is no way to tell them from a person (P6).
- **Coverage follows names.** A hardlink to a placeholder in an unmarked directory escapes
  interception and reads zeros (Z3); a second bind mount of the same filesystem is intercepted,
  because a mark is on the inode (*kernel* §1, measured on Btrfs).
- **A tool that re-sparsifies a downloaded file** (`fallocate --dig-holes`, some deduplication
  tools) leaves it ignore-marked, reading zeros until the inode is evicted.
- **Provisional numbers.** The 32-waiter global cap and the 60 s liveness window are chosen, not
  measured; nothing in the suite reaches either.

Two claims were measured false and must not return as reasons: that starting a process makes
`F_SETLEASE` fail (0 failures in 2000 spawns and 6000 forks, *kernel* §12.2), and that a
`MAP_SHARED` mapping whose descriptor was closed does not refuse a lease (it does, *kernel* §12.1).
Nor must the daemon close its copy of the event descriptor before it answers, now that the ignore
mark carries `SURV_MODIFY`.

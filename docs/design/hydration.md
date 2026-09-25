# Hydration

How a file in the sync folder can exist without its content, how opening it brings the content in
before the opener sees anything, how the space is given back, and how all of it survives crashes.
The processes involved are introduced in [README.md](README.md); how the folder is kept in step
with the cloud is in [sync.md](sync.md).

Measurements quoted here come from the privileged suite in `tests/vm/`, run in a virtme-ng VM
against Btrfs, ext4 and XFS; `docs/kernel-behavior-7.2.md` has each one, its evidence and how to
reproduce it (cited as *kernel* §n).

## 1. Scope

- Placeholders: their format on disk, how they are built.
- The helper: its fanotify group, its marks, how it decides an open, what it lets each user do.
- Filling a placeholder (hydration) and freeing it up again (dehydration).
- Recovery after a crash.
- The protocol between helper and daemon.
- Registering a folder, the filesystems it may be on, and the developer's mode without
  interception.

## 2. Placeholders on disk

### 2.1 Directories

Directories are ordinary directories, created up front from the listing. There are no directory
placeholders: listing a folder never needs the network. The sync root carries `user.konedrive.root`
(a random version-4 UUID, the *root id*); every other directory created from OneDrive carries
`user.konedrive.item-id`. Every directory under the root carries the helper's permission mark
(§4): that mark is what makes the files inside it visible to the helper.

### 2.2 Files and their states

| State | On disk | Interception |
|---|---|---|
| `online-only` | a sparse file: `st_size` is the remote size, no data blocks, `mtime` is the remote `lastModifiedDateTime` | covered by its directory's mark |
| `hydrating` | content being written | covered by its directory's mark |
| `hydrated` | an ordinary file with its data | covered, plus an evictable ignore mark placed on the first open that finds it `hydrated`, so later opens do not reach the helper |
| `dehydrating` | blocks being released | covered; the state is made durable before the ignore mark is removed (§8) |

A file with no konedrive attributes at all inside the folder is not managed: the helper lets its
opens through and never marks it.

Zero-byte files are created directly as `hydrated`: there is nothing to download and nothing to
misread.

On ext4 a placeholder's attributes take a 4 KiB block of their own, so a file holding no data
reports `st_blocks = 8`, not 0 (Btrfs and XFS report 0; *kernel* §11.5). Anything that reads
"takes no space" from `st_blocks` must allow one block.

### 2.3 Extended attributes

All values are plain ASCII, readable with `getfattr -d`.

| Attribute | On | Value |
|---|---|---|
| `user.konedrive.root` | the sync root | the root id |
| `user.konedrive.item-id` | files and directories | the Graph item id, stable across renames and moves |
| `user.konedrive.state` | files | `online-only`, `hydrating`, `hydrated` or `dehydrating` |
| `user.konedrive.ctag` | files | the Graph cTag of the version the file represents |
| `user.konedrive.stamp` | downloaded files | `<size> <mtime_sec>.<mtime_nsec>`, recorded when the download completed |
| `user.konedrive.progress` | files part-way through a download | `<ctag> <bytes durably written>` (§7.4) |

The stamp is how the daemon recognises its own work: a `hydrated` file whose size or time no longer
matches its stamp has been changed locally, and that change may be the only copy (§8,
[sync.md](sync.md) §10).

### 2.4 Building a placeholder

A placeholder is built nameless and linked in complete, so that nothing can ever open a half-built
one:

1. `open(dir, O_TMPFILE | O_RDWR)` — a nameless inode in the target directory;
2. `ftruncate(size)`, set `item-id`, `ctag` and `state=online-only`, `futimens(mtime)`, and the
   file's mode (`0444` in a OneDrive folder, [sync.md](sync.md) §11);
3. `linkat(fd, "", dirfd, name, AT_EMPTY_PATH)` — the file gets its name, already covered by its
   directory's mark.

A crash before step 3 leaves nothing behind. The `O_TMPFILE` open in step 1 raises a permission
event when the directory is marked, although the file has no name yet (*kernel* §7); the daemon's
own opens are let through by its pid exemption (§5.1), and a file with no state yet is allowed
without an ignore mark, so construction neither waits for itself nor blinds the file it builds.

## 3. Invariants

These four hold for everything below. They are referred to by name elsewhere.

**M1 — every directory under a registered root carries the permission mark, and a new directory is
marked before anything is created inside it.** The helper's registration and startup walks mark
each directory before descending into it. The daemon creates a new directory under a temporary
name, has the helper mark it, and only then renames it into place ([sync.md](sync.md) §7.3).
*Gap:* a directory made by some other program is not marked until the helper's next walk, because
the notification watcher that would notice it is not built (§16).

**M2 — a file's state lives only in its extended attributes.** The kernel knows nothing about it,
and neither does any database: losing every mark costs coverage, never data, and the tree store
([sync.md](sync.md) §5) is only a map.

**M3 — no file is emptied while an ignore mark of ours is on it.** The ignore mark survives
modification (§4.3), so a file emptied under one is empty *and* permanently unintercepted: every
later open reads zeros, and nothing notices. This is the one failure that is both silent and
unrecoverable, so the invariant is kept at the place where a file is emptied rather than argued
from where marks can be. Each of the three places that empty a file — a dehydration (§8), recovery
(§9) and the roll-back of a failed fill (§6.3) — first makes the state that announces it durable
(`dehydrating` or `hydrating`), and then follows **the local rule**:

- with a link to the helper, it asks the helper to `ClearIgnore` the file, and stops on any failure;
- with no helper bound to its socket, it goes ahead: no fanotify group of ours exists, so no mark
  of ours does;
- with a helper bound and no link, it stops, changes nothing, and tries again later (`NoHelper`).

Whether a helper is bound is read without connecting: a `SOCK_STREAM` `connect` to the helper's
`SOCK_SEQPACKET` path fails with `EPROTOTYPE` exactly when a socket is bound there. (A real
connection would be registered as this user's daemon for as long as it lived.)

One case needs no `ClearIgnore`: rolling back a fill that began from `online-only`. The helper marks
only a file it reads `hydrated`, reads the state again after marking and takes the mark off unless
it still says `hydrated`; such a file was never marked, and once the fill made `hydrating` durable
no mark placed on it can stay.

The helper keeps two further guards as defence in depth: its registration walk clears the ignore
mark of every file it walks, and it places no mark on a user's file for an open read before one of
that user's folders was unregistered. The local rule does not depend on either.

**M4 — a managed file that leaves the root carries an individual inode mark, so it is still filled
where it now is.** The protocol has `MarkFile` for this, and it works: a placeholder given
`MarkFile` and then renamed out of the tree is intercepted and filled (*kernel* §1). *Not upheld
yet:* nothing sends it on its own until the notification watcher exists (§16).

## 4. What the kernel watches

### 4.1 One mark per directory, and ignore marks on downloaded files

The helper marks **every directory inside the sync root** with
`FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`, and places an **evictable ignore mark** on each file it has
seen `hydrated`. An open of any file in a marked directory raises a permission event unless the
file's own ignore mark suppresses it.

Kernel memory therefore grows with the number of folders, not files: a directory mark pins its
inode, measured at **1658 bytes per directory** on a realistic tree of 10 000 directories and
100 000 files (*kernel* §8, §11.7, §13) — about 16.6 MB for 10 000 folders, and it does not grow as
the drive fills. A mark per placeholder would cost the same per file, about 330 MB for 200 000
files. The kernel offers no subtree marks: marks are per inode, mount, filesystem or mount
namespace, directory marks are not recursive, and in-kernel subtree filtering is not in 7.2.
A filesystem-wide mark with a path filter in the helper would cost almost no memory, but would put
the helper in the open path of every file in `/home`, so a stalled helper would freeze the
desktop; with directory marks, only the sync folder is affected.
([decisions.md](decisions.md), "One fanotify mark per directory".)

The cost of the choice is one helper round trip on the first open of each downloaded file (about
0.1 ms), and again whenever the kernel reclaims its ignore mark.

`FAN_ONDIR` is deliberately not set: opening a directory is never intercepted, so listing a folder
never waits for the helper.

### 4.2 Why `FAN_OPEN_PERM`

Every path to a file's content — `read`, `mmap`, `sendfile`, `copy_file_range`, reflink,
io_uring — begins with an open, so intercepting the open covers all of them by construction.
`FAN_PRE_ACCESS` fires on access and could fill ranges lazily, but relies on a hook in every access
path, and a missing hook means a program reads zeros. It has not been evaluated; the design stays on
`FAN_OPEN_PERM` and fills whole files. Measured through `FAN_OPEN_PERM`: `mmap` after open, `cp`
and `cp --reflink` all see the real content on all three filesystems.

### 4.3 The group and the flags that are not optional

```text
fanotify_init(FAN_CLASS_PRE_CONTENT | FAN_CLOEXEC | FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS | FAN_NONBLOCK,
              O_RDWR | O_LARGEFILE | O_CLOEXEC | O_NONBLOCK)
directory:   fanotify_mark(FAN_MARK_ADD, FAN_OPEN_PERM | FAN_EVENT_ON_CHILD, dirfd)
ignore mark: fanotify_mark(FAN_MARK_ADD | FAN_MARK_IGNORE | FAN_MARK_IGNORED_SURV_MODIFY | FAN_MARK_EVICTABLE,
                           FAN_OPEN_PERM, fd)
```

- **`FAN_NONBLOCK` on the group.** Without it, reading events blocks forever once the queue is
  drained (*kernel* §7). The loop `poll`s the group and reads until `EAGAIN`.
- **`O_RDWR` event descriptors.** The daemon writes the content through the event's own
  descriptor, which the kernel opened on the opener's mount; that descriptor carries
  `FMODE_NONOTIFY`, so the writes raise no events of their own.
- **`O_NONBLOCK` on the event descriptors.** The kernel opens each event's descriptor inside the
  helper's `read()`. Without `O_NONBLOCK`, opening a file that anyone holds a write lease on waits
  for the lease to break — and stops the whole event loop, every intercepted open on the machine,
  for as long as the lease is held (measured: 4.8 s for a 5 s lease; any local user can hold one).
  With it, the kernel cannot hand that one event over and answers it itself with `FAN_DENY`: the
  opener gets `EPERM` in 7–8 ms, and every other open goes on (*kernel* §12.4; limitations log P2).
- **`FAN_MARK_IGNORED_SURV_MODIFY`.** Without it the kernel refuses to add an ignore mark to an
  inode that anyone holds open for writing — and reports success while creating nothing. The
  helper is always in that situation, since it marks through an `O_RDWR` event descriptor while
  the daemon holds a copy of it. This is invisible unless `/proc/self/fdinfo` of the group is read
  (*kernel* §2.1).
- **The price of that flag.** An ignore mark without `SURV_MODIFY` was cleared by any write, so a
  punch that forgot to clear it repaired itself. With the flag it does not: invariant M3 exists
  because of it (*kernel* §2.2). `SURV_MODIFY` does not make a mark permanent — the kernel still
  reclaims an evictable mark under memory pressure, and losing one costs one extra round trip.
- **The response errno.** `FAN_DENY` with an errno in its top byte is accepted for exactly
  `EPERM, EIO, EAGAIN, EBUSY, ETXTBSY, ENOSPC, EDQUOT` (Linux 6.14 and later). Any other value
  makes the response `write()` fail with `EINVAL` and leaves the opener suspended until the group
  closes — and the values outside the set (`ENOENT`, `ECONNRESET`, `ETIMEDOUT`) are exactly what a
  network source produces (*kernel* §5). The daemon reports only accepted values, the helper clamps
  anything else to `EIO`, and if the kernel refuses the response anyway (an older kernel), the
  helper writes a plain `FAN_DENY`, which the opener sees as `EPERM`.

### 4.4 The helper's own opens are events too

fanotify does not exempt the listening process: an `open()` of a file inside a marked directory by
a thread that must also answer events waits for itself forever (*kernel* §7). So the helper never
opens a file under a marked directory. It marks through descriptors it is handed (the event's own,
or one the daemon sent), walks trees by opening directories only (a directory open raises nothing
without `FAN_ONDIR`, *kernel* §4), and clears ignore marks by name relative to a directory
descriptor rather than by opening the file.

## 5. An intercepted open

### 5.1 What the helper decides

1. An application calls `open()`. The kernel suspends it and queues `FAN_OPEN_PERM` with an
   `O_RDWR` event descriptor.
2. The helper `fstat`s the descriptor. Anything but a regular file is allowed. Otherwise the
   file's owner is the user whose daemon will be asked, and the helper decides:
   - **the owning daemon's own open** — allowed, before anything else. The exemption is narrow: the
     opener's pid is the pid `SO_PEERCRED` reported for the owner's current top connection (§10.2),
     and that user has a registered root. Recovery and placeholder construction rely on it;
   - **no konedrive attributes** (a file the user created in the folder) — allowed, with no ignore
     mark: a placeholder still being built looks the same;
   - **`hydrated`** — the helper adds an ignore mark through the event descriptor, **reads the state
     again**, and allows. The mark stays only if the file still reads `hydrated` and none of its
     owner's folders was unregistered since the event was read; otherwise it comes off at once and
     the file is decided again from what it reads now;
   - **`online-only`, `hydrating` or `dehydrating`** — step 3;
   - **an item id with no state, an unknown state, or unreadable attributes** — denied `EIO` and
     logged. It is a file of ours whose content may not be there, and it must not read as zeros.
3. The helper coalesces by `(st_dev, st_ino)`: an existing job for the inode gains a waiter;
   otherwise a new job sends `HydrateRequest{req_id}` with the descriptor (`SCM_RIGHTS`) to the
   owner's daemon. At most 64 requests are outstanding per connection; a job beyond that is
   enrolled, its openers stay suspended, and each returning credit sends the oldest (§10.3).
4. The daemon fills the file (§6) and answers `HydrateDone{req_id, errno}`.
5. On success the helper reads the state again from the event descriptor. Only if it says
   `hydrated` does it add the ignore mark — read back as in step 2 — and allow every waiter;
   otherwise it denies them all `EIO`. On failure it denies every waiter with the daemon's errno,
   clamped to the accepted set (§4.3).

The event loop waits on nothing but the group. Each event goes to a **pool of 64 workers with a
1024-deep queue**; a full pool answers `EAGAIN` rather than growing. Every decision a worker makes
is an `fstat`, an `fgetxattr` and a hash lookup; the download happens in the daemon, and there is
no time limit on it — large files legitimately take minutes. Measured: 3000 concurrent opens on
each filesystem, 3000 filled, none refused, a peak of 69 threads and under 4 MiB of memory
(*kernel* §11.3, §11.4).

### 5.2 Waiting for a daemon that is not there

If the owner's daemon is not connected, a worker holds the open up to **30 s** for it to connect,
then denies it `EIO` — but only for a user who has a registered root (anyone else is denied at
once), and at most 8 workers wait for any one user and 32 for all users together; beyond either
cap the open is denied `EIO` at once. Without the caps, one user opening another's placeholders
while that user's daemon was down could park every worker.

### 5.3 What an opener can receive

| Situation | The opener gets |
|---|---|
| The fill succeeds | the real content |
| The download fails after its retries (network, a missing item, a hash that does not match twice) | `EIO`; the file stays `online-only`, and the next open tries again |
| The disk fills up during the fill | `ENOSPC` (or `EDQUOT`); the file is rolled back |
| The daemon is not running | a wait of up to 30 s, then `EIO` (§5.2) |
| The daemon's connection ends mid-fill | `EIO` for its own waiters only |
| The helper's pool is saturated | `EAGAIN` (not reached at 3000 concurrent opens) |
| The helper is out of file descriptors | `EPERM` from the kernel, or `EIO` from the helper — never an unfilled file |
| The file is leased by someone (a free-up's punch, or another program) | `EPERM` at once, from the kernel (§4.3) |
| An open through a read-only mount (a Flatpak app with `home:ro`, `ProtectHome=read-only`) of any file whose ignore mark is not in place | `EPERM` at once: the kernel cannot open the `O_RDWR` event descriptor on that mount (limitations log P1) |
| A second open or `exec` of an executable in the folder while it runs, without its ignore mark | `EPERM` (`ETXTBSY` on the event descriptor; limitations log P8) |
| A managed file with an unreadable or unknown state | `EIO`, logged |
| Kernels before 6.14 | `EPERM` wherever an errno would have been sent |

## 6. Filling a placeholder

### 6.1 The steps

The same code runs for an intercepted open and for "download now" (`Hydrate`, §6.5).

0. **Look again.** Under the per-inode lock, read the state from the descriptor. A request can wait
   a long time before its turn — for a fill slot, or in the helper for credit — and the file may
   have been filled meanwhile. A file that reads `hydrated` is answered success at once and **not
   touched**: with a matching stamp it is simply there; with a stamp that does not match it was
   edited in place, and that edit is the only copy. Only `online-only`, `hydrating` and
   `dehydrating` are filled.
1. Read the item id. Set `state=hydrating`, `fsync`. A file found in any state but `online-only`
   may carry an ignore mark, and a failed fill empties the file, so the way is cleared now by M3's
   local rule, before the first byte is fetched. If it cannot be cleared, the state is put back and
   the answer is `EIO` (`NoHelper` to `Hydrate`), with nothing fetched or written.
2. Ask the content source for the bytes from the current offset. It answers with a stream, **the
   offset that stream actually starts at**, the current size and time, and the version (cTag and
   hash) when it knows them.
3. Write sequentially into the event descriptor through a 256 KiB buffer; memory does not grow
   with file size.
   **A resumed stream is written at the offset the source says it served, or not at all.** An HTTP
   server may answer a `Range` request with `200` and the whole body; written at the resume offset,
   that would put the file's beginning in its middle, with every byte accounted for and the fill
   reported as a success. A stream that starts anywhere other than where it was asked to is a
   failure (`EIO`).
4. If the remote size differs from the placeholder's, truncate to the remote size: the current
   version wins. **Except** a declared size of 0 against a placeholder with a non-zero size, which is
   refused `EIO`: nothing corroborates that answer and nothing could retry it. A file genuinely
   emptied in the cloud is a metadata change, handled by the sync.
5. Verify (§7.2), set the remote time (`futimens`), `fdatasync`, write the stamp, `fsync`, write
   `state=hydrated`, `fsync`. A time the local filesystem cannot represent does not fail the fill:
   the content is correct, and the stamp records the time the file actually has.
6. Answer success.

### 6.2 The commit point

**`state=hydrated` is the commit point, and it is written last.** It is what makes the helper allow
the open and add an ignore mark, so nothing that can still fail may come after it. Written before
the stamp, a full or failing disk would produce a file marked `hydrated` that holds nothing but
zeros; the helper would deny that one opener and then allow, and ignore-mark, the next one. For the
same reason no step of a fill is best-effort: a swallowed error is a success report for content
that is not there.

### 6.3 Rolling back

Only a file still `hydrating` is rolled back; a file that reads anything else is no longer this
fill's to empty. The roll-back runs in this order:

1. `state=online-only`;
2. remove the stamp;
3. punch everything past the durable checkpoint, if there is a usable one (§7.4), else the whole
   file; restore the original size and time.

The demotion comes first because every step runs on the disk that just failed the fill, and it is
the one step whose failure cannot be recovered from: in the other order, a crash between the punch
and the demotion leaves a file whose state promises content that was just removed. Demoting first
makes the worst case an `online-only` file still holding stale bytes, which the next fill
overwrites. None of the results is discarded; logged, they are the only sign this path ran.

The errno answered is one the kernel will deliver: a local `ENOSPC` or `EDQUOT` travels as itself,
anything else becomes `EIO`. A panic inside a fill is caught and answered `EIO` rather than leaving
the opener suspended.

### 6.4 Concurrency and the per-inode lock

At most **4** fills run at once. The slot is taken before a fill starts, so requests waiting for one
stay in the daemon's request queue — exactly as deep as the helper's credit (§10.3) — rather than
piling up as tasks that each hold an event descriptor.

Every fill, `Hydrate`, `Dehydrate`, recovery and every reconcile step that changes a file take a
**per-inode lock**, keyed by `(st_dev, st_ino)` read from the descriptor — never by a name, so two
links to one file serialise too.

### 6.5 Download now (`Hydrate`)

"Download now" does not go through interception. The daemon opens the file inside the root —
`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` from the root's own descriptor, and only
while the folder still carries this root's id — then takes the per-inode lock, reads the state
again under it, and fills the file with the same code (§6.1). The order matters:

- opening first and locking second avoids a deadlock against the fill the open itself would
  trigger in an intercepted folder;
- opening beneath the root's descriptor, rather than checking a canonical path and then opening
  the name, means a directory renamed in between cannot make the fill write outside the root.

A file labelled `hydrated` with no stamp is filled (nothing this daemon wrote can be in that
state); one whose stamp does not match is refused `ModifiedLocally` and never overwritten.

### 6.6 Content sources

```text
trait ContentSource {
    fetch(item_id, from) -> Fetched { served_from, size, mtime, version: Option<(ctag, quickXorHash)>, stream }
}
```

- **Graph** (`sync/graph_source.rs`) serves a folder that shows OneDrive (§7).
- **LocalDir** serves a local directory, for tests and for the developer's local folder
  (`PopulateFromDirectory`, §14.3). It is held in memory only: after a daemon restart an intercepted
  open of an `online-only` file there is denied `EIO`, and `Hydrate` answers `NoSource`, until the
  folder is populated again. It refuses a source file that leads into the sync folder or carries
  konedrive's own attributes (a symlink or hardlink to a placeholder would otherwise copy its zeros
  into the file being filled), both when mirroring and again when reading. It can inject faults for
  tests: a fixed delay, and a failure at byte N, permanently or once.

Every source must report `served_from`, the offset its stream really begins at.

## 7. Downloading from OneDrive

### 7.1 The request

The fill asks `GET /me/drive/items/{id}` for fresh metadata: size, cTag, `quickXorHash`, and the
pre-authenticated `@microsoft.graph.downloadUrl`. It streams from that URL, falling back to
`GET /items/{id}/content` (which answers `302` to the same kind of URL) only when the metadata
carries none. One request gives everything the fill checks against, and a 2 GB file is one request
and one response. `429` and `503` are honoured with their `Retry-After` ([sync.md](sync.md) §4.3).

### 7.2 Verification

`quickXorHash` is computed over the bytes as they are written, and **a file is marked `hydrated`
only if it matches** the hash Graph reported. On a mismatch the file returns to `online-only` and
the opener gets `EIO`: nobody sees wrong bytes. `quickXorHash` is the one hash Graph provides for
personal and business drives alike; the implementation (`src/quickxor.rs`) follows Microsoft's
published algorithm and is checked against hand-worked vectors and against real downloads.

The version of the first answer is the one the file must end up as. A later answer for another
version means the file changed in the cloud mid-download: the download starts over, once. A hash
mismatch also starts it over once; a second mismatch is `EIO`. A file Graph hands out without a
hash is downloaded unverified and logged as an anomaly (limitations log F34).

### 7.3 Resume within a run

A read error or a short stream is a break, not the end: the next request asks from where the bytes
stopped, with `Range: bytes=<written>-` on the download URL, after 200 ms, then 400 ms. Three breaks
and the download gives up with `EIO`. An expired download URL is replaced by fetching the metadata
again.

### 7.4 Resume across a restart

Every **16 MiB** the fill `fdatasync`s the file and records `user.konedrive.progress` = `<ctag>
<bytes>`. A fill that gives up, and recovery after a crash (§9), keep that durable prefix: the file
goes back to `online-only` with its stamp removed, but only what lies past the checkpoint is
punched. Recovery accepts a checkpoint by its byte count alone (more than 0, no more than the file's
size). The cTag is compared later, when a fill resumes and weighs the checkpoint against fresh
metadata: a checkpoint for another version, or for a version with no hash to verify it by, is
discarded then and the download restarts from zero. Otherwise the fill reads the written part once
to rebuild the hash state and continues from there.

Resume is safe because verification covers the whole file, including what lay on disk across the
restart: anything wrong there fails the hash, and the file is downloaded from zero. The cost is
that a file shown as `online-only` may hold part of its blocks until the next fill or a change in
the cloud (limitations log F33). A reconcile keeps the checkpoint too: it judges a placeholder's
content by cTag and size, not by its time, which a fill's writes change ([sync.md](sync.md) §7.4).

## 8. Freeing up space (dehydration)

1. **Open the file once**, inside the root as `Hydrate` does (§6.5), and take the per-inode lock
   on that descriptor. Every step below uses this one descriptor; nothing re-opens the file by name,
   because a rename in between — an editor's atomic save, say — would otherwise send the punch to
   another file.
   A zero-byte file has nothing to free: the call succeeds and changes nothing. Otherwise require
   `state=hydrated` and a stamp matching the current size and time, or refuse: no konedrive state →
   `NotManaged`; another state → `NotHydrated`; a stamp mismatch → `ModifiedLocally` (a local edit
   is the only copy in this phase). Record the file's times, because the punch will change them.
2. Set `state=dehydrating`, `fsync`, and clear the way by M3's local rule: with a link, the helper
   `ClearIgnore`s the file — from now on every open reaches the helper again. **Any failure stops
   here**: the state goes back to `hydrated` and the call fails. With no link, go on only if no
   helper is bound to its socket; otherwise roll back and refuse `NoHelper`. The helper reports a
   mark that was never there, or that the kernel had reclaimed, as success, so a failure here means
   something genuinely went wrong.
3. Take a write lease, `fcntl(F_SETLEASE, F_WRLCK)`. The kernel grants it only if nobody else has
   the file open — a descriptor, or a mapping whose descriptor has since been closed (*kernel*
   §12.1). Starting or forking a process does not refuse it (*kernel* §12.2), so a refusal is never
   spurious. Refused → roll back to `hydrated` and answer `InUse`. `SIGIO` is ignored before the
   daemon takes its first lease, because an opener breaking the lease signals the holder and
   `SIGIO`'s default action kills it (*kernel* §12.3).
4. `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, size)`, restore the recorded times
   (an `online-only` file carries the remote time), `fsync`.
5. `state=online-only`, remove the stamp, release the lease. A failure from step 4 on leaves the
   file `dehydrating` with nothing to roll back to; recovery (§9) finishes it.

**Races.** An open arriving after step 2's `ClearIgnore` and before the lease reaches the helper,
which reads `dehydrating` and holds the open's descriptor while it asks for a fill — so the lease is
refused, the dehydration rolls back `InUse`, and the fill request then finds the file `hydrated`
and answers it as it is (§6.1 step 0). An open arriving while the lease is held is denied `EPERM` by
the kernel at once (§4.3); an opener the helper let through before the lease already refuses the
lease (*kernel* §12.5). Nothing waits on the punch, and nothing is let through onto it.

In an intercepted folder with no link to the helper, the whole operation is refused `NoHelper` and
nothing changes: freed up with nothing intercepting, the file would read zeros.

On filesystems with snapshots, or for files cloned with `cp --reflink`, punched blocks stay
referenced elsewhere and no space comes back until those references go. The file still becomes
`online-only` correctly.

"Free Up Space" for the whole folder ([desktop.md](desktop.md) §2.3) runs this per file and skips,
without waiting, any file that is open or whose per-inode lock is held.

## 9. Recovery

At startup, and again after every reconnect to the helper, the daemon first re-registers its root
with the helper and **then** walks it. The order matters: a file left `dehydrating` may still carry
its ignore mark (the daemon died between the state write and `ClearIgnore`), and clearing it needs
the helper, which acts only for a user with a registered root.

The walk runs from the root's own directory descriptor. Every entry is opened with `openat` from
the directory it was listed in (`O_NOFOLLOW`) and accepted only if it is a regular file or a
directory on the root's own device; one file is open at a time (holding them all open once left
half of 2000 interrupted files unrecovered at the default descriptor limit), and the walk stops at
128 levels. For each file in `hydrating` or `dehydrating`:

1. try the per-inode lock **without waiting**; if it is held, the file is being filled or freed up
   right now — by a fill from the previous connection, say — and is counted `busy` and left to it;
2. reopen the file writable through its descriptor (the same inode, whatever its name leads to by
   now) and close the read-only one;
3. clear the way by M3's local rule, on that descriptor;
4. take the write lease (a refusal is retried for about 75 ms, long enough for a copy of the
   descriptor inherited by a process being spawned to go) and **read the state again**: a file
   finished meanwhile is left as it is;
5. punch it (past a usable checkpoint for a `hydrating` file, §7.4, else entirely), restore the
   time it had, `fsync`, set `state=online-only`, remove the stamp.

A refusal anywhere — the helper's, the lease's, the punch's — leaves the file exactly as it was
found, to be retried at the next start. A refused lease means something has the file open, most
often the very open that will fill it, so it counts as `busy`, not a failure. A folder registered
without interception is recovered by the same rule: when a helper runs that this daemon has no link
to, its files are left as found and counted `deferred`, and recovered the moment a link exists.

The report distinguishes `scanned`, `reset`, `failed`, `skipped` (a subtree on another filesystem),
`busy` and `deferred`, so that "nothing needed fixing" and "nothing could be fixed" do not read
alike. `failed > 0` publishes `RootState = error`; the other counters put a note in `LastError`.

## 10. The helper–daemon link

### 10.1 Transport and messages

`/run/konedrive/helper.sock`, a `SOCK_SEQPACKET` socket. The helper learns the peer's uid from
`SO_PEERCRED`, never from a message. Messages are small, versioned, binary
(`crates/konedrive-proto`); descriptors travel with `SCM_RIGHTS`.

| Direction | Message |
|---|---|
| daemon → helper | `Hello{version}` (its first call); `RegisterRoot{root_id}` + directory descriptor; `UnregisterRoot{root_id}`; `MarkDir` / `UnmarkDir` + directory descriptor; `MarkFile` + file descriptor; `ClearIgnore` + file descriptor; `HydrateDone{req_id, errno}` |
| helper → daemon | `Welcome{version}` (unprompted, on accept); `Ack{errno}` — exactly one per daemon message, in order; `HydrateRequest{req_id}` + the event descriptor |

### 10.2 Connections

No handshake gates anything: `SO_PEERCRED` is the authority, and a greeting could only carry a
version the peer might lie about. The helper greets first; the daemon sends `Hello` as its first
ordinary call, so the version check runs both ways.

Replies are paired by order — every daemon message gets exactly one `Ack`, and `HydrateRequest` is
told apart by type — so a daemon call is bounded (30 s; 120 s for `RegisterRoot` and
`UnregisterRoot`, which walk the tree) and a timeout ends the connection: a reply dropped on the
floor would pair every later one with the wrong call. After losing the link the daemon publishes the
loss at once and reconnects with a backoff from 1 s doubling to 30 s; fills already running finish
on their own, and a request from a connection that has ended is not filled (its opener was answered
when the connection went).

Every job, suspended open and pid exemption belongs to one **(uid, connection)**; a connection
ending denies only its own openers `EIO`. Each uid has a **stack** of live connections: fill
requests and the pid exemption go to the top one, and a disconnect removes its connection wherever
it sits, handing control back to the next one. So a second process of the same user that connects
and hangs up cannot evict the live daemon. No uid may hold more than **16** connections: each costs
two threads and a few descriptors, and the socket is open to every local user.

Because requests go to one connection per uid, a daemon keeps one link for all of its accounts,
and decides itself which account's folder a request's file is in ([accounts.md](accounts.md)
§3.3, §3.4). The helper knows users, not accounts.

### 10.3 Flow control

At most **64** `HydrateRequest`s are outstanding on a connection
(`konedrive_proto::MAX_OUTSTANDING_HYDRATIONS`), and the daemon's request queue is exactly that
deep: a contract between the two ends. Beyond it a request is enrolled in the helper with its
openers suspended, and each `HydrateDone` hands its credit to the oldest one waiting. Without the
credit, the two bounded queues deadlocked under a burst: each fill slot waited for the `Ack` to its
`HydrateDone`, which sat behind requests the daemon had stopped reading (*kernel* §11.4). Refusing
beyond the credit instead of queuing gave `EAGAIN` to most of 3000 opens — a thumbnailer opening a
folder of photos would fail most of them.

Everything the helper sends goes through a per-connection outbox with its own writer thread, so no
worker ever blocks on a socket the peer controls. The outbox keeps room for the helper's requests
apart from room for `Ack`s (128), so replying never costs a live connection. A blocked send is not
evidence of a wedged daemon; silence is: the connection ends only when a send is blocked *and* the
daemon has sent nothing for 60 s.

## 11. What the helper allows each user

One helper serves every user, and its socket is open to all of them (mode `0666`). Every request is
authorised by the peer's uid and by who owns the object it is about; the request carries a
descriptor, not a path.

- **`RegisterRoot`** — the directory is owned by the peer, on a filesystem the helper accepts
  (§14.2), and neither inside nor containing another registered root, whatever that root's id. A
  root id that another uid registered is refused `EPERM` (ids are chosen by the client, so without
  this one user could replace another's registration). A path that is not valid UTF-8 is refused.
  The whole tree is walked and marked, and the ignore mark of every file walked is cleared.
- **`UnregisterRoot`** — only a root the peer's uid owns, from any of its connections (roots
  outlive connections). It removes the marks the helper placed on the tree, including files' ignore
  marks, as well as the entry: removing the entry alone would leave the tree intercepted with no
  daemon to ask, and every placeholder would answer `EIO`. The walk is best effort: what it cannot
  unmark is logged, and the answer is still success (limitations log Z7).
- **`ClearIgnore`** — a regular file the peer owns, anywhere. Removing an ignore mark can only send
  the file's next open back to the helper; and a folder registered without interception, which must
  ask before every punch (M3), may hold no root with the helper at all.
- **`MarkDir`, `UnmarkDir`, `MarkFile`** — the object is owned by the peer and lives on the device
  of one of that peer's roots. Without the owner check, a user could make the helper intercept
  opens of files they can read but do not own (say `/etc/passwd`) and stall system processes. The
  check is scoped to the device, not the root: keeping every operation inside the root is the
  daemon's job, and the helper is a coarse backstop.
- **The pid exemption** (§5.1) — only to a connection that owns a registered root, only for files
  of that uid, and only to that uid's top connection.
- **`HydrateRequest`** descriptors go only to the daemon of the file's owner, so the `O_RDWR`
  descriptor grants nothing that user could not already open.

The VM suite runs a second, unprivileged uid against the shipped helper: holding nothing but the
socket, it can neither force-allow another user's suspended opens by guessing request ids, nor
unregister their root by id, nor unmark their directories.

## 12. Helper startup and persistence

Registered roots live in `/var/lib/konedrive/roots.json` (uid, path, device, inode, root id; mode
`0600`). A corrupt file is moved aside, and one that cannot be read at all leaves the helper
starting with no roots rather than not starting: with no interception every placeholder reads
zeros, so not starting is worse.

On startup the helper first builds its worker pool, binds its socket and prepares its accept
thread — everything that can fail and end the process — and only then marks anything, because
exiting over marked trees would release every open suspended meanwhile. It then re-opens each root
and checks it again: ownership, the filesystem type, and the nesting rule (in a stable order, so the
same root wins every boot). It walks each tree **depth-first**, marking each directory before
descending into it and clearing the ignore mark of each file by name, to at most 128 levels. Files
are never opened. Measured: **1.03 s for 10 000 directories and 100 000 files** on a cold cache
(*kernel* §11.7).

The walk resolves each component beneath a held `/` descriptor with
`RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, and below the root additionally with
`RESOLVE_BENEATH | RESOLVE_NO_XDEV`. `RESOLVE_NO_XDEV` is deliberately not applied on the way *to*
the root: the sync folder normally lives under `/home`, a separate mount or subvolume. The
directory reached must also be the registered device and inode, but that check is not trusted
alone, because ext4 reuses a deleted directory's inode number. A directory that cannot be opened or
marked never ends a walk: everything reachable is marked, each failure is logged, and the root is
flagged degraded in the helper's log (limitations log F10). Measured against 57 766 directory
renames racing the walk: the helper never followed a symlink out of the root and marked every
directory that stood still.

The systemd unit (`packaging/systemd/konedrive-helper.service`) starts the helper before the
display manager, restarts it always, gives it `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH` only, no
network, a read-only view of the system and of `/home` with write access only to
`/var/lib/konedrive` and `/run/konedrive`, a system-call deny-list that includes the mount calls,
and `LimitNOFILE=65536`. `ProtectHome=read-only` does not stop fills: the daemon writes through the
event descriptor, which belongs to the opener's mount (*kernel* §11.6). `SECURITY.md` lists every
setting and what it does and does not prevent.

## 13. The helper must not die

When the helper's fanotify group closes — the process exits, is killed or crashes — **the kernel
answers every suspended open with "allow"**, and every opener reads whatever the placeholder holds:
zeros. Measured: 200 opens suspended on a slow source, `SIGKILL`, all 200 released within 525 ms,
every one reading zeros (*kernel* §11.6). Nothing the helper does can change what the kernel does
on close, and queuing beyond the credit (§10.3) means every open in flight is exposed. So the
requirement is that the helper does not exit. What defends it, each item proven in the VM suite:

- a bounded worker pool, so running out of threads is `EAGAIN`, not a panic in the event loop;
- a panic on a worker is caught, its opener denied `EIO`, and the pool kept at strength; a panic on
  a connection runs that connection's clean-up, which denies its openers `EIO`;
- running out of descriptors is survivable: the event loop keeps the group and retries every 50 ms,
  the accept loop backs off, and openers the kernel could not hand over are denied (`EPERM` by the
  kernel, `EIO` by the helper), never allowed;
- an event whose descriptor the kernel could not open — through a read-only mount (`EROFS`), of a
  running executable (`ETXTBSY`) — is that one event's failure, already answered `EPERM` by the
  kernel; only an error of the group's own descriptor (`EBADF`, `EINVAL`, `EFAULT`) ends the loop;
- a daemon disconnecting, dying or wedging costs only its own connection, and no uid holds more than
  16 connections;
- no lock is held across a blocking call on a socket a peer controls;
- the fault-injection hooks the suite uses to prove the unwind paths exist only in builds with the
  `fault-injection` cargo feature; the installer refuses a helper that contains them.

The remaining exposure is recorded as limitations log Z1: updating or stopping the helper releases
every open waiting at that moment.

## 14. Registration and modes

### 14.1 Registering a folder

`RegisterRoot(path)` binds an **empty** directory to the account's drive. It is refused, with a
named error, when the account is not signed in (`NotSignedIn`), when it already has a folder
(`AlreadyRegistered` — each account keeps one), when no helper is connected (`NoHelper`: a
placeholder nobody intercepts reads as zeros), when the folder is, is inside, or contains another
account's (`Overlaps`), or when the folder fails the checks below (`NotEmpty`, `Unsupported`).
"Empty" applies to a first registration only: a folder that already carries a well-formed root id
is one this daemon claimed before, and restarts rely on it — unless it also carries the drive of
another account, which is refused `NotEmpty` ([accounts.md](accounts.md) §6.3).

The registration is written to `config.toml` *before* the helper is told, and refused if it cannot
be; a failure after the helper may have stored it is undone at the helper and in `config.toml` —
or, when the helper cannot confirm it let go, kept intercepted with `RootState = error`, because a
folder the helper may hold must never be one the daemon treats as unintercepted. A `config.toml`
that exists but cannot be read is never overwritten with defaults. Calls that change the
registration take turns under a lifecycle lock.

At startup, and after every reconnect, the daemon re-registers the root with the helper and then
recovers it (§9). A restored intercepted root is held as registered before the helper is back —
`RootState = error`, saying the helper is not connected — so it answers `AlreadyRegistered` to a
second registration and `NoHelper` to a Forget.

### 14.2 Filesystem requirements and the probe

Required: sparse files with `FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE`, `user.*` extended
attributes, `O_TMPFILE` (every placeholder is built with it) and file leases (dehydration and
recovery cannot empty a file safely without one). Supported and tested: **Btrfs, ext4, XFS**.
Refused: FAT and exFAT (no sparse files, no attributes), and network and FUSE filesystems — NFS,
SMB/CIFS, FUSE, AFS, Ceph — where files can change on another machine, bypassing interception.

At registration the daemon creates a nameless `O_TMPFILE` file in the folder and exercises each
feature: size, hole punching, a `user.*` attribute, a write lease. A failure names the missing
feature. A named probe file would outlive a crash and make the folder permanently "not empty", so
the probe never has a name. The helper checks the filesystem type with `fstatfs`; its own write
probe can be refused by its sandbox and is then skipped. A symbolic link is refused as a root, and
`konedrivectl` resolves only the directory a path is in, never its last component, so a link given
on the command line reaches the daemon as a link and is refused. A folder that already carries its
root id is not probed again when it is brought back up: a OneDrive folder is locked read-only, so
the probe's write would fail (limitations log F31).

### 14.3 Without interception (developer's mode)

`RegisterRootWithoutInterception(path)` makes a folder with the same checks and the same
placeholders, but **nothing intercepts opens**: files read as zeros until they are downloaded by
hand with `Hydrate`. It needs no helper and no sign-in, and it always makes a *local* folder, filled
from a local directory with `PopulateFromDirectory` — never one that shows OneDrive
([sync.md](sync.md) §3). It exists for development and for the test suites on machines where no
helper runs. The window does not offer it; `konedrivectl sync register-without-interception` is the
only way in, and `sync status` repeats the cost on a line of its own for as long as it lasts.

Even here, emptying a file follows M3's local rule: a helper may be running and may have marked the
file while it belonged to an intercepted folder.

### 14.4 When the helper arrives later

A folder registered without interception *because no helper was connected* records
`upgrade_when_helper = true`. When a helper connects, such a folder is switched under the lifecycle
lock: its sync stops, the helper registers the root (its walk marks every directory), the switch is
written to `config.toml`, recovery runs, and the sync starts again, intercepted. The same call made
while a helper *was* connected was a deliberate choice and is never switched. A OneDrive folder left
without interception by an older version is switched whatever it recorded. A switch that fails
leaves the folder as it was and says why in `LastError` — except when the helper may still hold the
root, in which case the folder stays intercepted with its sync stopped until the next connect
(limitations log F30).

### 14.5 Forget

`UnregisterRoot` forgets the folder and leaves every file exactly as it is. An intercepted folder is
forgotten **through the helper or not at all**: with no link it is refused `NoHelper`, because
dropping it locally while the helper keeps its marks would leave placeholders that answer `EIO` with
no daemon to fill them. Forget also takes the read-only lock off ([sync.md](sync.md) §11), drops the
tree store, and removes the Baloo exclusion the daemon added ([desktop.md](desktop.md) §9).

## 15. Error summary

| Situation | What the application sees | Recovery |
|---|---|---|
| Network failure, download fails after retries | `EIO` | the file is `online-only` again; the next open retries |
| The daemon crashes during a fill | `EIO` for its waiters | recovery at the next start (§9) |
| A fill panics | `EIO` | the file is left `hydrating`; the next open fills it, recovery clears it |
| The daemon is not running | a wait of up to 30 s, then `EIO` (§5.2) | — |
| More than 64 fills in flight for one daemon | the open waits in the helper | sent as credits return, oldest first |
| The helper's connection is lost, the helper still runs | opens wait for a daemon as above | the daemon publishes `error` at once, reconnects, re-registers and recovers |
| The helper crashes | every suspended open is released and reads zeros | systemd restarts it and it marks everything again; the defence is §13 |
| The helper is not running yet (early boot) | placeholders read as zeros | the helper starts before the display manager |
| The disk fills up during a fill | `ENOSPC` | rolled back; fills normally once there is room |
| A managed file with an unreadable or unknown state | `EIO`, logged | by hand |
| The file is renamed or deleted during a fill | unaffected (work goes through the descriptor) | a deleted file's download completes into the unlinked inode |
| A placeholder moved out of the folder | reads zeros (M4 is not upheld yet) | — |
| The waiting program is killed | its open is abandoned | the fill continues; other waiters are unaffected |

## 16. Known gaps

- **The notification watcher is not built.** The design gives the daemon a second, unprivileged
  fanotify group (`FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME | FAN_REPORT_TARGET_FID`, watching
  `FAN_CREATE | FAN_MOVED_TO | FAN_MOVED_FROM`) so that a directory created by any program is marked
  at once and a file moved out of the root gets its `MarkFile`. Until it exists, M1 holds only for
  directories the daemon and the helper's walks create or reach, and M4 does not hold: a placeholder
  moved into a directory some other program just made, or out of the folder, reads zeros
  (limitations log Z2, Z3). It belongs to the write phase, which needs change tracking anyway.
- **Fail-open windows.** The helper not yet running, and the moment it dies (Z1). The blast radius
  is the sync folder only.
- **Eager hydration.** Anything that opens a placeholder downloads it — thumbnailers, indexers,
  `open(O_TRUNC)`; there is no way to tell them from a person (P6).
- **Coverage follows names.** A hardlink to a placeholder in an unmarked directory escapes
  interception and reads zeros; a second bind mount of the same filesystem is intercepted, because a
  mark is on the inode (*kernel* §1).
- **A tool that re-sparsifies a downloaded file** behind the daemon's back leaves it ignore-marked,
  reading zeros until the inode is evicted (Z4).
- **Provisional numbers.** The 32-waiter global cap and the 60 s liveness window are chosen, not
  measured; nothing in the suite reaches either (limitations log §5).

Three claims were measured false during development and must not return as reasons: that starting
a process makes `F_SETLEASE` fail (0 failures in 5000 spawns, *kernel* §12.2); that the daemon must
close its copy of the event descriptor before answering (not needed once the ignore mark carries
`SURV_MODIFY`); and that a `MAP_SHARED` mapping whose descriptor was closed does not refuse a lease
(it does, *kernel* §12.1).

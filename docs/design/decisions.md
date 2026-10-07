# Design decisions

The decisions that shape konedrive, each with the reason for it and what it costs. An entry
describes the code as it is; where the code no longer follows a decision, the entry says what
replaced it. An earlier approach is told where its failure is the reason for the current one.

Entries are grouped: [architecture](#architecture), [interception and
hydration](#interception-and-hydration), [the sync](#the-sync), [uploads](#uploads), [account and
security](#account-and-security), [the desktop](#the-desktop), [testing](#testing).

## Architecture

### No FUSE, no custom filesystem, no kernel module

- **Decision.** The sync folder is a plain directory on the user's own filesystem, not a mount of
  any kind. Files that are not downloaded are sparse placeholders; interception uses fanotify
  permission events.
- **Why.** Every tool sees ordinary files, a downloaded file has native performance, and renames
  within `/home` stay renames. A FUSE mount puts a userspace process in the path of every read, and
  behaves differently from a local filesystem in ways programs notice. A mount of a hidden directory
  would scope interception perfectly, but the real files would live somewhere else.
- **Trade-off.** A placeholder opened without interception reads as zeros: every gap in coverage is
  a way to read zeros. A filesystem must support sparse files, `user.*` attributes, `O_TMPFILE` and
  leases; network filesystems, FUSE and FAT are refused by name.

### Three processes, and the root side is minimal

- **Decision.** A root helper that intercepts, an unprivileged daemon that does everything else, and
  clients (window, CLI, Dolphin plugins) that talk to the daemon over D-Bus. The helper has no
  network, no tokens and no content logic; it decides an open with an `fstat`, an attribute read and
  a table lookup. Its one other service is to open an object of the user's by file handle ("Deleted
  or moved out: the object decides", below).
- **Why.** Holding an open until content exists needs a fanotify group of class
  `FAN_CLASS_PRE_CONTENT`, which needs `CAP_SYS_ADMIN`. The less code runs with it, the less can go
  wrong as root, and the smaller the attack surface offered to every local user through the helper's
  socket. The helper is also the one process whose death hands zeros to waiting programs.
- **Trade-off.** A protocol between helper and daemon, with its own flow control, timeouts and
  ownership rules ([hydration.md](hydration.md) §10–§11), and a round trip for every open that needs
  a download.

### Rust for the helper, daemon and CLI; C++/QML for the window and plugins

- **Decision.** The helper, daemon and CLI are Rust; the window is C++/QML on Kirigami; the Dolphin
  plugins are C++ against KIO.
- **Why.** Memory safety matters most in the root helper and in the daemon that parses network
  input. The window and the plugins use KDE's own stack, which is C++.
- **Trade-off.** Two toolchains, and sentences that must say the same thing in both: most are
  generated from one Rust crate, and a test keeps the skip reasons in step (issue #232).

### Whole-file hydration

- **Decision.** Opening a placeholder downloads the whole file before the open returns.
- **Why.** Every access path starts with an open, so intercepting the open covers `read`, `mmap`,
  `sendfile`, `copy_file_range`, reflink and io_uring by construction. Partial hydration on access
  (`FAN_PRE_ACCESS`) needs a kernel hook on every access path, and a missing hook means a program
  reads zeros; it was not evaluated.
- **Trade-off.** Opening a large file waits for all of it; anything that opens a placeholder, even
  briefly — a thumbnailer, an indexer, an IDE — downloads it whole (limitations log P6).

### Extended attributes are the truth; SQLite is a rebuildable map

- **Decision.** Each file's state lives in its own `user.konedrive.*` attributes. The tree store
  (SQLite) maps the drive and holds the delta link; what a file is and holds is never only there,
  and losing the store costs a full listing.
- **Why.** State stored with the file travels with it through renames and survives any crash that
  the file survives; there is no second copy to fall out of step.
- **Trade-off.** Tools that drop extended attributes make a file unmanaged. Attribute writes need
  write permission on the inode, which the read-only lock has to open a window for
  ([sync.md](sync.md) §11). The activity log, the conflict list and the outbox are lost with the
  store: a delete not sent yet is forgotten.

### Read before write

- **Decision.** Reading is the default: a new account is read-only, and uploads happen only for an
  account switched to read-write ([writes.md](writes.md)).
- **Why.** Writing depends on the real item ids that only reading provides, and a read-only client
  is safe to run against a live account: a bug cannot damage the cloud copy.
- **Trade-off.** A read-only account's folder must be locked (below), and its local edits are not
  uploaded.

### Several accounts in one daemon, sharing one helper link

- **Decision.** One daemon serves every account of a user. Each account has its own sign-in, folder,
  tree store, poller, transfer pool and D-Bus objects; the link to the helper, the per-inode locks
  and the admission of opens belong to the daemon and are shared.
- **Why.** The helper sends a user's opens to that user's newest connection only, so a second
  daemon, or a second link, would take every request away from the first. The helper already held
  several folders per user, refused folders that nest, and authorised by uid; keeping accounts out
  of it keeps the root side small.
- **Trade-off.** The accounts share the helper's credit of 64 requests, so many opens waiting in one
  account hold back opens in another, and a reconnect brings every folder up in turn before any fill
  is served (limitations log F43). The helper holds at most 32 folders for a user.

### An intercepted open finds its account by device, verified path, then item id

- **Decision.** The daemon decides which account a fill request belongs to from its descriptor
  alone: the accounts whose folder is on the file's filesystem; then the kernel's name for the file,
  opened beneath a candidate folder and compared by inode; then the file's item id in each
  candidate's tree store. One step comes first: a file moved out of an account's folder is that
  account's by its item id ([accounts.md](accounts.md) §3.4). With no answer, the open is denied
  `EIO`.
- **Why.** The helper's request carries no account, and adding one would put account knowledge in
  the root helper. A wrong answer would fill a file from another account's drive, so where two
  accounts could be meant a step either proves its answer or passes.
- **Trade-off.** A file renamed or unlinked while its open waits, on a filesystem that holds two
  folders, and known to no tree store, is denied `EIO`; the next open retries.

### Per-file calls go to one interface, routed by path

- **Decision.** `Hydrate`, `Dehydrate`, `ItemState`, `Pin`, `Unpin`, `FreeUp`, `WebUrl` and `Menu`
  are on the daemon-wide `org.konedrive.Files`, which finds each path's account by its folder. Each
  account's own objects keep what concerns its folder as a whole.
- **Why.** The Dolphin plugin and `konedrivectl` know a path, not an account, and a selection in
  Dolphin may span two accounts' folders.
- **Trade-off.** One path in no account's folder refuses a whole `Pin`, `Unpin` or `FreeUp` call.

### No alias for the single-account D-Bus object

- **Decision.** `/org/konedrive/Daemon` is not served. The window, the CLI and the Dolphin plugins
  use `/org/konedrive/Accounts` and the per-account objects.
- **Why.** konedrive has no outside clients, and its clients ship in the same packages as the
  daemon. An alias would need a meaning for "the first account" that changes when that account is
  removed, and tests of its own, to serve no one.
- **Trade-off.** None.

## Interception and hydration

### One fanotify mark per directory, plus evictable ignore marks

- **Decision.** The helper marks every directory inside the sync root with
  `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`, and places an evictable ignore mark on each downloaded file
  the first time it is opened.
- **Why.** Kernel memory grows with the number of folders, not files: 1658 bytes per directory mark,
  so about 16.6 MB for 10 000 folders. A mark per placeholder costs the same per file — about 330 MB
  for 200 000 files. A single filesystem-wide mark with a path filter would cost almost nothing, but
  would put the helper in the path of every open in `/home`, so a stalled helper would freeze the
  desktop. The kernel has no recursive or subtree marks.
- **Trade-off.** Every directory must be walked and marked at registration and at every helper start
  (1.03 s for 10 000 directories and 100 000 files, cold); a directory created by another program is
  covered only once something marks it (M1's gap); the first open of each downloaded file costs one
  round trip to the helper, and again after the kernel reclaims its ignore mark.

### `FAN_OPEN_PERM`, not `FAN_PRE_ACCESS`

- **Decision.** Interception is on open.
- **Why.** See "Whole-file hydration": coverage by construction. The design would switch only if
  `FAN_PRE_ACCESS` were shown to cover every access path.
- **Trade-off.** No lazy or partial hydration.

### Ignore marks survive modification, and nothing is emptied under one

- **Decision.** Ignore marks carry `FAN_MARK_IGNORED_SURV_MODIFY`. A file that may carry one — one
  that read `hydrated` — is emptied only after `dehydrating` or `hydrating` is durable and the mark
  is cleared (invariant M3; [hydration.md](hydration.md) §3 has each case). While a helper runs and
  the daemon has no link to it, the punch is refused. A file found `online-only` or `hydrating` is
  punched without a clearance: the helper marks only what reads `hydrated`.
- **Why.** Without `SURV_MODIFY` the kernel silently creates no ignore mark on a file anyone holds
  open for writing — and the helper always marks through an `O_RDWR` descriptor. With it, a punch no
  longer clears the mark, and a file emptied under its mark reads zeros forever. The rule is applied
  where the file is emptied because an earlier argument — that a folder without interception could
  carry no stale mark — was disproved three times, each time by an unforeseen race.
- **Trade-off.** Freeing up space and some fills are refused `NoHelper` while a helper runs that the
  daemon cannot reach; recovery leaves such a file for the next connect.

### The helper marks a file only after reading `hydrated`, and reads it back

- **Decision.** An ignore mark is placed only on a file the helper reads `hydrated`, and kept only
  if a second read after placing it still says `hydrated`. A file with no konedrive attributes is
  never marked.
- **Why.** The `O_TMPFILE` open that starts building a placeholder is itself intercepted before any
  attribute exists; an unconditional mark would blind the placeholder under construction. And a mark
  placed just after a free-up read `hydrated` but before it wrote `dehydrating` would otherwise
  survive the punch.
- **Trade-off.** One extra attribute read per marking.

### The commit point is `state=hydrated`, written last; a roll-back demotes first

- **Decision.** A fill writes the size, the time, the data sync and the stamp before it writes
  `state=hydrated`. The second half is replaced: a failed fill, a free-up and recovery punch first
  and write `online-only` last ([hydration.md](hydration.md) §6.3).
- **Why.** `hydrated` is what makes the helper allow and ignore-mark the file. Written earlier, a
  full or failing disk produced a file marked `hydrated` holding zeros. In the roll-back, only a
  file that reads `hydrating` or `dehydrating` is emptied, and those states never read as content:
  the next open fills the file and recovery resets it. Changed last, the state is never `hydrated`
  or `online-only` while the file holds something else.
- **Trade-off.** No step of a fill's commit may be best-effort, except setting the time.

### A resumed stream is written where it starts, or not at all

- **Decision.** Every content source reports the offset its stream actually starts at, and a fill
  refuses a stream that starts anywhere but where it asked. A declared size of 0 for a placeholder
  with a non-zero size is refused.
- **Why.** An HTTP server may answer a `Range` request with `200` and the whole body. Written at the
  resume offset, the file's beginning lands in its middle while every byte is accounted for. A size
  of 0 would truncate the file on one unconfirmed answer that nothing could retry.
- **Trade-off.** A file genuinely emptied in the cloud is left to the metadata sync.

### Only errnos the kernel accepts reach the opener

- **Decision.** The daemon reports `EIO` for every failure to produce content (a missing item, a
  dropped connection, a timeout), and a local error as itself only when the kernel accepts it, as it
  does the `ENOSPC` and `EDQUOT` of a full disk; the helper clamps to the accepted set and falls
  back to a plain deny if the kernel still refuses.
- **Why.** `FAN_DENY` accepts exactly `EPERM, EIO, EAGAIN, EBUSY, ETXTBSY, ENOSPC, EDQUOT`. Any
  other value makes the response fail with `EINVAL` and leaves the opener suspended until the group
  closes — and `ENOENT`, `ECONNRESET` and `ETIMEDOUT` are exactly what a network source produces.
- **Trade-off.** Programs see `EIO` for every network problem.

### Event descriptors are non-blocking

- **Decision.** The helper's event descriptors are opened `O_NONBLOCK`.
- **Why.** The kernel opens each event's descriptor inside the helper's `read()`. Opening a file
  that anyone holds a write lease on then waits for the lease — which stopped every intercepted open
  on the machine for as long as any local user held one. With `O_NONBLOCK` the kernel answers that
  one event itself.
- **Trade-off.** An open of a leased file — a free-up's punch lasting a few milliseconds, or a file
  another program leases — gets `EPERM` at once instead of waiting (limitations log P2).

### Queue beyond the credit, and make the helper not die

- **Decision.** At most 64 fill requests are outstanding per daemon connection; beyond that, opens
  wait in the helper rather than being refused. The helper is built so that it does not exit: a
  bounded worker pool, caught panics, survivable descriptor exhaustion, per-event handling of
  descriptors the kernel cannot open, and per-uid caps, past which an open is refused `EAGAIN`.
- **Why.** Without a credit, two bounded queues deadlocked under a burst. Refusing beyond it gave
  `EAGAIN` to most of 3000 concurrent opens. Queuing means every suspended open is exposed if the
  helper dies, because the kernel then allows them all onto unfilled files; the answer to that is a
  helper that does not die, not a smaller queue, which would only bring the refusals back.
- **Trade-off.** A helper that crashes or is killed hands zeros to every open in flight; an ordinary
  stop, as at an update, fails each with an error instead, and a file opened while no helper runs
  reads zeros (limitations log Z1). Measured with 3000 concurrent opens: all filled, none refused.

### Free-up runs on one descriptor, under a write lease

- **Decision.** Dehydration opens the file once by name, beneath the root, and does everything — the
  checks, `dehydrating`, `ClearIgnore`, the lease, the punch, restoring the time — through that one
  object, under the per-inode lock keyed by device and inode; the punch is made under a write lease.
- **Why.** An earlier version opened the path four times and punched the fourth: an editor's atomic
  save landing in between had the guard pass on the old inode and the punch empty the new one. A
  lease is granted only when nobody else has the file open or mapped, so nothing is punched under a
  reader. A lock keyed by path did not serialise two names for one file.
- **Trade-off.** A file open or mapped anywhere cannot be freed up ("in use"). `SIGIO` must be
  ignored, since a broken lease signals the holder and the default action kills it.

### "Download now" fills directly

- **Decision.** `Hydrate` opens the file beneath the root with
  `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`, then takes the per-inode
  lock, then reads the state, then fills — it does not open the file and let interception do the
  work.
- **Why.** Without interception an open fills nothing. With it, taking the lock before the open
  deadlocked against the fill the open triggered. And checking a canonical path, waiting, then
  opening the name let a directory rename send the fill outside the root.
- **Trade-off.** Two paths into one fill routine, which must both look at the state under the lock.

### Recovery runs after re-registration, one file at a time, without waiting

- **Decision.** When an intercepted folder comes up, the daemon registers its root with the helper
  and then recovers interrupted files; it takes such a file's per-inode lock without waiting,
  counting a busy file rather than blocking on it, and reads the state again under the lease.
- **Why.** A file left `dehydrating` may still carry its ignore mark, and only the helper can clear
  it. Holding every file open exhausted descriptors. Waiting for a fill from the previous connection
  would hold the reconnect behind a download, and a fill that finished in between must not be
  punched.
- **Trade-off.** A busy file is recovered at the next pass, not now.

### A registration is recorded before the helper hears of it; Forget goes through the helper

- **Decision.** A new intercepted registration is written to `config.toml` before the helper is
  told, and undone in both on failure — or kept intercepted when the helper cannot confirm it let
  go. An intercepted folder is forgotten (`Folder.Unregister`) through the helper or not at all; the
  exceptions are a folder the helper says it does not hold, and one whose root id can be read
  nowhere. An unreadable `config.toml` is never overwritten.
- **Why.** A folder the helper may hold must never be one the daemon treats as unintercepted: its
  ignore marks would stay, and a later free-up could punch under one. A Forget with no link would
  leave the tree intercepted with no daemon to answer, and every placeholder would fail `EIO`.
- **Trade-off.** Forget is refused `NoHelper` while the helper is unreachable; a lost or hand-edited
  `config.toml` can still leave the helper holding a folder the daemon forgot (issue #231).

### `ClearIgnore` is authorised by owning the file

- **Decision.** The helper clears an ignore mark on any regular file the asking user owns, whether
  or not that user has a registered root.
- **Why.** A folder registered without interception must still clear marks before a punch, and holds
  no root with the helper. Removing a mark can only send the file's next open back to the helper —
  one extra interception, never zeros.
- **Trade-off.** A user can clear marks on their own files.

### Per-user limits on the helper's socket

- **Decision.** The socket is open to every local user, and every request is authorised by the
  peer's uid and, where it names an object, the object's owner. Each uid may hold 16 connections, 32
  roots, and 8192 opens of its files waiting for its daemons; at most 8 workers wait for one user's
  absent daemon and 32 for all; a root id has the form of a version 4 UUID, and one another user
  registered is refused; mark requests are limited to objects the user owns on the device of one of
  their roots.
- **Why.** Any local user can connect. Without these, one user could exhaust the helper's
  descriptors, park every worker by opening another user's placeholders while that user's daemon is
  down, make the helper intercept files they can read but do not own, or replace another user's
  registration.
- **Trade-off.** The 32-waiter cap is chosen, not measured; containment inside a root is the
  daemon's job, with the helper only checking the device (limitations log F208).

## The sync

### Staging, then swap; the delta link moves only when the folder matches

- **Decision.** A cycle stages the changes, reconciles the folder against the staged tree, and only
  then, in one transaction, swaps the tree in with the new delta link ([sync.md](sync.md) §6.1). A
  changed download is replaced after the swap.
- **Why.** A crash anywhere before the swap leaves the old link, and the next cycle asks for the
  same changes again. The reconcile is idempotent, so doing it twice is safe; skipping a change is
  not.
- **Trade-off.** A delta of changes is held in memory whole before it is staged.

### A holding directory keyed by item id, and two reconcile scopes

- **Decision.** Items not where the new tree wants them move to `.konedrive-holding/<item id>`,
  deepest first; the tree is then placed top down. What is left in holding is gone from OneDrive: a
  read-only folder removes it or, if it holds local work, rescues it; a read-write folder puts back
  what OneDrive did not remove. A Full reconcile walks the whole folder; a Changed one only the
  delta's items, turning Full as soon as the folder disagrees with the stored tree or an operation
  on it fails.
- **Why.** The first design renamed each side of a name collision to a swap name resolved at the end
  of a batch. It had no clean answer for a cycle of three or more renames, or for a crash that left
  some swap names and not others. Keying everything in transit by item id resolves all of them the
  same way. Full keeps the folder honest, at every start of the sync, after a failure and for a
  delta of more than 5000 changes ([sync.md](sync.md) §6.2); Changed keeps an ordinary cycle cheap.
- **Trade-off.** A Full reconcile scans the whole folder; a file stuck mid-fill makes every cycle
  Full until it settles. The 5000-change threshold is a guess.

### The first listing is placed page by page

- **Decision.** A folder's first listing — nothing in the tree store, and nothing in the folder that
  carries an item id — places each page as it arrives and commits it with the link to the next page;
  every later listing is staged whole.
- **Why.** Staged whole, the first listing of a large drive left the folder empty for minutes. Page
  by page, the folder fills as the drive is listed and a stopped listing resumes where it stopped.
  Later listings keep staging, because part-way through a listing an item not listed yet cannot be
  told from one that is gone.
- **Trade-off.** The first page after each interruption is reconciled Full, and a partial download
  that an uncommitted page placed is fetched again.

### Replace a changed download beside the old one, then rename

- **Decision.** A downloaded file that changed in the cloud is replaced by downloading the new
  version into an `O_TMPFILE` in the same directory, verifying it, and renaming it over the old one
  — only if the disk has room for the new version beside the old one, with 64 MiB to spare.
- **Why.** Writing over the old version in place would show a reader a mix of both. A rename is
  atomic: a reader keeps the old version, the next open gets the new one.
- **Trade-off.** When both versions do not fit, the old one stays and the status says why. A failed
  replacement is tried again after each cycle that succeeds, without forcing a Full reconcile —
  forcing one turned every cycle into a whole-folder scan exactly when the disk was too full. Only a
  replacement that found nothing left to do forces one ([sync.md](sync.md) §9).

### Verify every download against `quickXorHash`

- **Decision.** A file is marked `hydrated` only if its bytes match the `quickXorHash` Graph
  reported; a mismatch or a mid-download version change restarts the download, once in all for one
  fill.
- **Why.** It is the one hash Graph provides for personal and business drives alike. Verification
  covers the whole file, including a prefix resumed from disk, so nobody is handed wrong bytes.
- **Trade-off.** A file Graph hands out without a hash is downloaded unverified: an anomaly, not a
  standing status (limitations log F34). It is logged only when the item had a cTag.

### Keep partial downloads, and check them when resuming

- **Decision.** Every 16 MiB a fill of a version that has a cTag and a hash makes its progress
  durable (`user.konedrive.progress`). A fill that fails for the network or the disk, and crash
  recovery, keep that prefix. When a fill resumes, a checkpoint for another version, or one the file
  does not bear out, is discarded; a hash mismatch keeps nothing.
- **Why.** Without it, a dropped connection or a restart during a multi-gigabyte download started it
  over from zero. Keeping the prefix is safe because the resumed file is verified whole.
- **Trade-off.** A file shown as `online-only` may hold part of its blocks, which stay until the
  download finishes or the cloud version changes: free-up answers that the file is not downloaded
  (issue #221). The 16 MiB interval is a guess.

### Judge a placeholder's content by cTag and size, never by its time

- **Decision.** A reconcile compares a placeholder's cTag and size with the tree's. A time that
  differs alone only gets the cloud's time back.
- **Why.** A fill's writes move the file's time to now. Taking that for a new version punched a
  partial download away at the Full reconcile every restart begins with.
- **Trade-off.** None.

### A read-only account's folder is locked; the guarantee is the stamp check

- **Decision.** Files `0444`, directories `0555`. The daemon lifts the owner's write bit only for
  its own attribute writes and directory changes. Before any change from the cloud is applied, the
  file is checked against its stamp and rescued if it changed.
- **Why.** With nothing uploaded, a local edit could only diverge from the cloud. Directories are
  locked too, because editors save by writing a new file and renaming it. `user.*` attribute writes
  need inode write permission even for the owner, hence the per-operation window.
- **Trade-off.** A program that opens a file for writing inside a window keeps a writable
  descriptor, so the lock is the rule a person sees, not the guarantee. A read-write account's
  folder has no lock ([writes.md](writes.md) §2.2).

### A rescue is one rename, never a copy

- **Decision.** A file holding local work is moved with one `RENAME_NOREPLACE` into a rescue
  directory on the folder's own filesystem; across filesystems the rescue fails and the file stays
  where it was. A file of konedrive's own that holds no local work is removed, not rescued.
- **Why.** A copy followed by a delete could delete bytes it never finished copying, would leave the
  subtree unlocked for the duration, and needs its own crash protocol, for a rare layout. A rename
  cannot lose bytes.
- **Trade-off.** A folder that is itself a mount point, or whose parent cannot be written, cannot be
  rescued into, and a cycle that needs a rescue fails until the folder is moved (limitations
  log F16).

### No OneDrive sync without the helper

- **Decision.** A OneDrive folder is registered only with the helper connected (`Folder.Register`
  answers `NoHelper` otherwise), and its cycles run only while the folder is intercepted and linked;
  `Folder.RegisterWithoutInterception` always makes a local folder. The daemon publishes the
  helper's state (`Accounts.HelperState`), and a folder that waits for the helper says what to do.
- **Why.** Placeholders that nothing intercepts read as zeros; a folder full of them is worse than
  an empty one. Earlier, a OneDrive folder could be registered without interception and kept in
  step, and the only sign of a missing helper was "not connected".
- **Trade-off.** Without the helper, nothing syncs: a recorded folder reads `waiting`, and `error`
  with the instruction once the helper is known to be missing or was lost ([sync.md](sync.md) §3).

### A folder made without the helper because none was there switches when it arrives

- **Decision.** A registration made without interception because no helper was connected records
  that, and switches to interception when the helper connects. One made while a helper was connected
  was a choice and stays. Today this serves a local folder made from the command line, and a
  OneDrive folder an older version left without interception, which switches whatever was recorded.
- **Why.** A folder registered before the helper was installed otherwise stayed unintercepted —
  every open read zeros — until it was forgotten and registered again.
- **Trade-off.** The switch walks and marks the whole tree under the lifecycle lock. A switch whose
  outcome the helper cannot confirm keeps the folder intercepted and down, in `error`, until the
  next connect or a `Refresh`.

### The lifecycle lock is held only from reconcile to swap

- **Decision.** A cycle's Graph phase runs without the lock that guards the registration.
- **Why.** Holding it across a Graph fetch blocked a helper reconnect — and so every fill — behind
  the listing. The Graph phase writes only the staging tables and `meta`, which a Forget does not
  read before it stops the poller.
- **Trade-off.** A stop cancels the cycle and then waits for the lock, so it waits for the step a
  reconcile is in.

### Check the account every cycle

- **Decision.** Each cycle asks which drive the token reaches and compares it with the drive the
  folder was built from: the tree store's, or, while the store has none, the account's drive in
  `config.toml` as read when the sync started. A mismatch blocks the cycle, and a read-write account
  turns read-only.
- **Why.** A sign-out followed by a different sign-in between two polls would defeat a check made
  only at sign-in, and the result would be another account's files listed over this folder. Keeping
  the id in `config.toml` too covers a store rebuilt empty.
- **Trade-off.** One `GET /me/drive` per cycle.

### A folder belongs to one account, and remembers which

- **Decision.** Two accounts' folders never nest: a registration that would is refused `Overlaps`,
  naming the other account. A OneDrive folder's root carries its account's drive
  (`user.konedrive.drive`), and a registration of a folder that carries another drive and is not
  empty is refused (`NotEmpty`); an empty one is taken, and the old drive comes off.
- **Why.** The helper refuses nested roots anyway; checking in the daemon names the refusal, and
  covers folders without interception, which the helper never sees. A forgotten folder keeps its
  root id and may be registered again without being empty, so without the drive attribute another
  account could adopt it and reconcile its own drive over the first account's files.
- **Trade-off.** A folder that carries no drive — one forgotten before there were several accounts —
  can still be adopted by any account.

### Skip what cannot be a local file, visibly

- **Decision.** Names over 255 bytes, the Personal Vault, shared folders added to the drive, OneNote
  notebooks, names with the `.konedrive-` prefix and items konedrive cannot place are skipped,
  recorded with their reason and listed ([sync.md](sync.md) §7.5).
- **Why.** Each either cannot exist as a local file or does not belong to this drive; a truncated or
  renamed copy would be a different file with a misleading name. A skip nobody can see looks like
  data loss.
- **Trade-off.** Those items are not available locally (shared folders: issue #51).

### Take the download URL from the item's metadata

- **Decision.** A fill asks for the item's metadata and streams from its
  `@microsoft.graph.downloadUrl`, falling back to `/content` only when there is none.
- **Why.** One request gives size, cTag, hash and URL together, and fresh metadata on every attempt
  also replaces an expired URL.
- **Trade-off.** None.

### Whole-drive listing, even when only one folder matters

- **Decision.** The daemon lists the drive from its root.
- **Why.** Graph's delta for a personal drive, and for OneDrive for Business, is available only from
  the drive root; one delta link per sync root.
- **Trade-off.** A real-account test run lists the whole drive even though it downloads only in one
  named folder.

## Uploads

These describe a read-write account's folder ([writes.md](writes.md)). Where Windows' OneDrive
client has an answer a user already knows, it is followed.

### Local changes come from a second fanotify group, in the daemon

- **Decision.** A read-write folder's directories carry a second fanotify mark, in an unprivileged
  notification group the daemon owns (`FAN_REPORT_DFID_NAME_TARGET`: creations, deletions, renames,
  closes after writing, attribute changes). Events only mark directories dirty; a quiet batch is
  then examined against the base. A Full local scan at bring-up and after anything that can lose
  events is the safety net.
- **Why.** The kernel refuses file-handle reporting, which directory-entry events need, in the
  helper's pre-content group. A group of the daemon's own needs no privilege and keeps the helper as
  it was. Events as hints, with the disk as the answer, make a merged, lost or overflowed event cost
  a scan, never a missed change.
- **Trade-off.** A second mark per directory, from a per-user budget every account shares, and a
  queue that a large unpack can overflow. An edit that kept both size and time while nothing watched
  is not found.

### An item is its id and its inode, never its path

- **Decision.** A file on disk is matched to its item by the `user.konedrive.item-id` it carries;
  two inodes with one id are told apart by the file handle recorded when the item was placed. A
  rename is an id under a new name; an editor's save by rename is a new inode taking over an id, so
  the item keeps its version history and sharing links.
- **Why.** Paths are what changes; ids travel with the inode through every rename, and a handle is
  unique per filesystem. Guessing identity from names would turn every save into a delete and a
  create.
- **Trade-off.** A copy that kept the attributes needs the recorded handle to be told from the
  original; a rebuilt store has none, and falls back to the item's place (limitations log F53).

### The disk is the truth about local changes; the outbox is only intent

- **Decision.** The outbox, rows in the tree store, records what is to be sent for an item and how
  far it got. Every row can be found again by comparing the disk with the base, and a rebuilt base
  produces no deletes.
- **Why.** A lost or corrupted store then costs a listing and restarted uploads, never a local byte
  and never a wrong delete in OneDrive.
- **Trade-off.** A delete not sent before the store was lost is forgotten, and the item comes back
  from OneDrive.

### Small files go up in an upload session

- **Decision.** Every non-empty file is uploaded through an upload session, one request's body up to
  10 MiB and 10 MiB fragments above that. Only an empty file, which a session cannot carry, uses the
  plain `PUT`.
- **Why.** Microsoft documents `If-Match` and `conflictBehavior` for the session, and it carries the
  file's time (`fileSystemInfo`). For the plain `PUT` its current page documents neither guard, and
  the time would take a second request anyway. 10 MiB is Microsoft's own boundary for resumable
  transfers.
- **Trade-off.** Two requests for every small file, and a guard on the empty `PUT` that is assumed
  ([writes.md](writes.md) §13). The session is not told the file's size, which a personal drive
  refuses, so a full drive shows itself only when a fragment is refused ([writes.md](writes.md)
  §6.1, §6.4).

### Every write is guarded, and a failed guard is settled by reading again

- **Decision.** `If-Match` on every change, `conflictBehavior=fail` on everything new. A `412` or
  `409` is followed by reading the item and deciding: the same hash is adopted, a change of metadata
  only is sent again with the fresh tag, anything else is a conflict. Nothing is sent without its
  guard — except a folder's delete, which carries none at all (next entry).
- **Why.** It is the only way two writers cannot overwrite each other, and it makes every step
  replayable after a crash: "did my request land?" is answered by the content hash.
- **Trade-off.** An extra read on every refused guard; and a session checks `If-Match` when it is
  created, not when it completes, which leaves a window of one fragment (limitations log F80).

### A folder delete is the whole folder, as on Windows; the recycle bin is the safety net

- **Decision.** A folder deleted here — outright, or by a move to the Trash — is one `DELETE` of the
  folder itself in OneDrive, with no `If-Match` and no reads or deletes of what is inside it first,
  whatever changed there meanwhile. `404` on the `DELETE` means it was already gone: that is success
  too.
- **Why.** This is what OneDrive on Windows does, so the result is what a user expects. The earlier
  design deleted file by file and then the folder under a guard on its cTag; the worker's own
  deletes changed that cTag first, so the folder's `DELETE` always lost with `412`, and the emptied
  folder was placed again locally. OneDrive's recycle bin is the safety net a whole, unguarded
  delete needs.
- **Trade-off.** Something added to the folder in OneDrive before the request reaches it goes with
  it, to the recycle bin; restoring it is a recycle-bin restore, not a resync. The mass-delete guard
  ([writes.md](writes.md) §4.5) still holds a large removal for confirmation before any request is
  sent.

### Changed on both sides: keep both, named after the machine

- **Decision.** The version in OneDrive keeps the name; this computer's is renamed beside it to
  `<name>-<machine>.<ext>` and uploaded as a new file, and the pair is listed as a conflict. A file
  deleted here and edited in OneDrive comes back; a file deleted in OneDrive and edited here is
  uploaded again, as a new item. For renames, the first to reach OneDrive wins.
- **Why.** It is what Windows does, so the result is what a user expects, and neither side's work is
  lost. The machine name says where the copy came from.
- **Trade-off.** Copies accumulate until the user merges them; a rename made here can be undone by
  one made first in OneDrive. A folder's delete takes what was edited in OneDrive with it (the entry
  above).

### Names OneDrive refuses are listed, not substituted

- **Decision.** A file whose name OneDrive refuses (`" * : < > ? \ |`, a leading or trailing space,
  a reserved name) is not uploaded: its change is blocked and listed under "Not Uploaded", with the
  reason, until the user renames it. No look-alike character is put in its place.
- **Why.** Windows does the same. A substituted name would differ between the folder and OneDrive
  for good, and every other device would see a name nobody chose.
- **Trade-off.** A `:` or `?` in a Linux file name is common, and each such file stays local until
  renamed by hand.

### A move out of the folder downloads first

- **Decision.** A file or folder moved out of the folder is deleted in OneDrive only once its
  content is on this computer: a placeholder that left is marked again, downloaded where it went,
  stripped of konedrive's attributes, and only then deleted. Sent to the desktop Trash instead, a
  placeholder is removed without a download.
- **Why.** Windows does the same. A move out is a delete for OneDrive, and the user still holds the
  file; deleting it first would leave them an empty placeholder where they expect their file. In the
  Trash the user asked for a delete, and OneDrive keeps the item in its recycle bin.
- **Trade-off.** A large cloud-only folder moved out is a large download, with no prompt; until it
  is done the item stays in OneDrive, and other devices still see it (limitations log F121).

### Deleted or moved out: the object decides, by its file handle

- **Decision.** An item missing from the folder is asked after by the file handle recorded for it,
  through the helper's `OpenByHandle`: gone, with nothing of its own at its place, is a delete;
  alive outside the folder is a move out; any other answer decides nothing, and the item is asked
  after again.
- **Why.** Events cannot tell: one can be lost, and a move made while the daemon was not running
  raises none that it sees. An unprivileged daemon cannot open a handle, and the helper already
  holds the capability for its walks, so the helper gained one narrow message rather than the daemon
  a privilege.
- **Trade-off.** New root code reachable by every local user, answering only for the user's own
  object carrying konedrive's attribute (SECURITY.md). A delete waits while the helper is not
  connected (limitations log F54).

### Nested subvolumes and other devices are not uploaded

- **Decision.** Anything on another device than the folder's root — a nested Btrfs subvolume, a
  filesystem mounted inside the folder — is neither watched nor uploaded, and is listed under "Not
  Uploaded" as `other-device`.
- **Why.** The helper cannot mark a directory on a device where the user holds no folder, so a
  placeholder there could never be protected. Windows treats a mount point inside OneDrive the same
  way.
- **Trade-off.** Files in such a place stay on this computer only (limitations log F72).

### A large delete waits for the user

- **Decision.** Removals of more than 500 items, or of at least 10 items that are more than 20 % of
  what the folder holds, are held until the user confirms them or asks for the items back
  ([writes.md](writes.md) §4.5).
- **Why.** Windows asks too. `rm -rf` of the wrong directory, or a folder swapped for an empty one,
  should not empty OneDrive before anyone looks.
- **Trade-off.** Nothing of such a delete reaches OneDrive until someone answers; the thresholds are
  provisional.

### A stale change from OneDrive is read again, not dropped

- **Decision.** A change the delta feed fetched before an upload committed, about the item it
  committed, is not trusted unless it is the commit itself: the item is read again, under the tree
  lock. OneDrive's change to an item with local work waiting is kept in the store until the disk
  takes it. A removal is the exception, decided on 2026-10-04 in place of "deleted there means
  deleted, whole": what OneDrive removed is removed here in the same cycle, whatever waits for it,
  and what only this computer has — a file made here, a download changed here — stays as the user's
  own and goes up as new ([writes.md](writes.md) §9).
- **Why.** Dropping the stale entry would lose a change OneDrive made just after the commit, which
  the delta feed never sends twice; applying it would undo the upload. Keeping only what OneDrive
  never had loses nobody's work and still lets a delete made elsewhere be a delete here.
- **Trade-off.** One `GET` for each such entry, and a failed `GET` fails the cycle.

## Account and security

### Sign-in in the system browser, with PKCE and a loopback redirect

- **Decision.** OAuth authorization code with PKCE, the system browser, a single-use loopback
  listener, personal accounts only.
- **Why.** Microsoft's recommendation for desktop applications (RFC 8252): the password and second
  factor stay in the browser, which keeps the user's Microsoft session, and there is no embedded web
  view to trust.
- **Trade-off.** The consent screen calls the app unverified.

### konedrive ships its own client ID, like any desktop client

- **Decision.** konedrive signs in with its own Microsoft Entra application registration, built into
  the daemon (`DEFAULT_CLIENT_ID`). Nobody registers an app or enters a client ID to use konedrive.
  `config.toml`'s `client_id`, set with `konedrivectl set-client-id` or `Accounts.SetClientId` while
  no account is signed in, overrides it.
- **Why.** A public client's id is not a secret: it is sent in every sign-in URL, so there is
  nothing to protect by making each user register their own. Registering an app was a step with no
  security purpose, only friction between installing konedrive and signing in.
- **Trade-off.** Every install shares one Entra application's rate limits and "unverified publisher"
  consent screen. An override is taken back only by editing `config.toml`.

### The daemon owns the token; the refresh token lives only in the Secret Service

- **Decision.** The daemon holds the refresh token in KWallet (through the Secret Service), with no
  plaintext fallback, and the access token in memory only. The window never sees a token.
- **Why.** The daemon needs the token while the window is closed, and one owner of a secret is
  easier to reason about than two. A plaintext fallback would put the most valuable secret on disk.
- **Trade-off.** No Secret Service, no sign-in. A locked wallet prompts, and a refusal is a
  transient error.

### Scope `Files.Read`

- **Decision.** A read-only account asks for `Files.Read User.Read offline_access`, at the
  authorization, the code exchange and every refresh alike, and a new account's sign-in always asks
  for this scope. A read-write account asks for `Files.ReadWrite` instead: a refresh follows the
  mode the account runs in, its own sign-in the mode `config.toml` records
  ([accounts.md](accounts.md) §10).
- **Why.** Microsoft refuses any write made with a `Files.Read` token, so "nothing is written to the
  cloud" is enforced by the server, not by the client's discipline. Microsoft may still answer a
  read-only request with a token that can write, from consent it keeps: the daemon uses it to read
  only, hands it to nobody and says so in `LastError` (limitations log F66).
- **Trade-off.** Switching to read-write needs a sign-in of its own, for `Files.ReadWrite`.

### An access-token export for test runs, in development builds only

- **Decision.** `TokenExport.ReadOnly()` and `konedrivectl dev export-access-token` hand out an
  access token (an hour of read access at most), never the refresh token, written atomically to a
  `0600` file. It is read-only whatever the account's mode. `--read-write`
  (`TokenExport.ReadWrite()`) hands out one that can write, for the test-account harness, and only
  for an account that runs read-write and whose drive is in `write_test_drive_ids`.
- **Why.** A test run in the VM needs to speak to Graph without a sign-in of its own, and the
  refresh token must never leave the Secret Service. Only the tests need it, so it is built only
  with the cargo feature `dev-tools` (`scripts/dev-install.sh`, `scripts/build-rpm.sh --dev-tools`):
  the released package has neither the interface nor `konedrivectl dev`, and its `%build` fails if
  the daemon names `org.konedrive.TokenExport`.
- **Trade-off.** On a development install, any process of the same user on the session bus can
  obtain an hour of read access — no more than it has by opening files in the folder.

### An account is its drive

- **Decision.** An account's identity is its Graph drive id, recorded at its first sign-in (or at
  the first answer that names its drive) and not changed after. A sign-in that reaches another drive
  than the account's own is refused, and so is one that reaches a drive another account already has.
  The check and the record are one step under the configuration's lock, and the check is
  fail-closed.
- **Why.** An account's folder, tree store and rescues belong to one drive: signed in as someone
  else, it would reconcile a different drive over them. Two accounts of one drive would download
  everything twice. A check that let a sign-in through when Graph did not answer would leave nothing
  to compare later sign-ins against.
- **Trade-off.** A sign-in is refused when the daemon cannot ask which drive it reached, or cannot
  learn the drive of another account that has none recorded yet. Connecting a different Microsoft
  account means adding a new account.

### Account ids are random; labels are for people

- **Decision.** An account is known by 12 random hexadecimal characters, checked against the ids
  present, which name its D-Bus object and its directories. People see and type a label, 1 to 40
  characters with no `/`, unique among the labels, which can change at any time and names nothing on
  disk. A label may contain `@`: the daemon names a new account after its email where it can.
- **Why.** What appears in a D-Bus path and in file names must be valid in both and stable across
  renames. An email address is neither stable in meaning — the same address can be removed and added
  again — nor valid in a D-Bus path.
- **Trade-off.** Rescued files are grouped by account id, not by a name a person recognises
  (limitations log F47). `--account` takes an id, a label or an email and refuses a name that fits
  two accounts, which a label equal to another account's email does.

### `config.toml` has one owner, and a single-account file is migrated in place

- **Decision.** Every change to `config.toml` goes through one store that re-reads, changes and
  writes the file atomically under one lock, and that never overwrites a file it cannot read. At the
  first start with multiple accounts, a single-account file is copied to `config.toml.v1` and
  rewritten as version 2, its account — if it had anything to carry over — becoming "Personal"; the
  tree store and the cached account then move into the account's directory, idempotently, at each
  start until done; the wallet item moves at the account's first token load.
- **Why.** With several accounts every change touches the same file, and two independent writers had
  already been able to save over each other. The version-2 write is the one commit point, and
  idempotent moves let the next start finish whatever a crash interrupted. Moving the wallet item
  inside a token load that happens anyway adds no unlock prompt.
- **Trade-off.** No way back to a single-account version except copying `config.toml.v1` back by
  hand; a tree store that cannot be moved safely is rebuilt with one listing, losing its activity
  log and conflicts.

### Each account's token is a wallet item of a new kind

- **Decision.** Each account's refresh token is stored under `kind=account-refresh-token` and
  `account=<id>`, rather than under the single-account `kind=refresh-token` with an `account`
  attribute added.
- **Why.** A Secret Service search returns every item whose attributes include the ones asked for: a
  search for the single-account item would find every account's as well, and telling them apart
  would mean reading attributes that a locked wallet may not show.
- **Trade-off.** While a migrated account's old item has not moved yet, both kinds are looked for
  and both are deleted at sign-out.

### The mode, and the write gate

- **Decision.** Every account has a mode, `read-only` or `read-write`, stored in `config.toml`, a
  new account read-only. `Account.Mode` publishes the mode it *runs* in: read-write only while
  `config.toml` says so, and its last token was granted `Files.ReadWrite` and was seen to reach the
  drive recorded for the account. `Account.SetMode` switches it, and writes read-write only once a
  sign-in has granted that. The mode is the user's choice, for any signed-in account: no list of
  accounts decides it. The outbox's write gate asks all of this again before each row and between an
  upload's fragments, with `config.toml` read again each time ([writes.md](writes.md) §2.3).
- **Why.** Microsoft, not the client, then decides whether a write can happen: a token can write
  only after a sign-in that asked for it. Publishing the mode run in, not the one asked for, keeps
  "read-write" from meaning anything a token cannot do. While uploads were being developed,
  `write_test_drive_ids` also decided which accounts could be read-write at all; that gate is gone,
  and the list now serves only the token export (above). What keeps a stray click from making
  another account writable is the switch's pinned sign-in, which asks for the password again with
  the account's email filled in.
- **Trade-off.** Once an account is read-write, nothing but the user's own switch stands between a
  change in its folder and the real OneDrive. A read-write account that loses its grant turns
  read-only until it signs in for it again.

### Removing an account keeps the user's files

- **Decision.** Removing an account forgets its folder as a Forget does, signs it out, and deletes
  its refresh token, its cached name and quota and its tree store. The folder's files and the
  rescued files stay.
- **Why.** Nothing konedrive does deletes a user's file, and the Forget path is already crash-safe
  and goes through the helper.
- **Trade-off.** Files never downloaded stay as empty placeholders, which read as zeros (limitations
  log F47). Like a Forget, removing an account is refused while its intercepted folder cannot reach
  the helper, and while changes wait to be uploaded.

### Personal accounts only, for now

- **Decision.** Every account signs in through the `consumers` authority.
- **Why.** Work or school accounts need another authority, an app registration that allows them,
  often an administrator's consent, and testing against SharePoint-backed drives: a phase of its
  own.
- **Trade-off.** No Microsoft 365 or OneDrive for Business accounts (issue #12).

## The desktop

### Notifications come from the app

- **Decision.** The window app, which runs at login in the tray, sends KDE notifications; the daemon
  sends none.
- **Why.** KNotification gives the user KDE's per-event settings in System Settings, and the app is
  the process that belongs to the desktop session.
- **Trade-off.** With the app quit, nothing notifies, and events that happened meanwhile are never
  announced; the window's lists still show everything.

### Events have kinds; the app never parses wording

- **Decision.** A failed replacement is its own activity kind, `update-failed`, distinct from a
  failed download. A rescue is carried as a conflict (`Conflicts.List()`, `Conflicts.Count`,
  `conflict` events), not as a note in `LastError`. Refusals are D-Bus error names.
- **Why.** A heuristic on the daemon's wording breaks on any rewording. Carrying the rescue as text
  in `LastError` made the app filter it out by pattern so that it did not show as a sync error.
- **Trade-off.** The code does not follow this everywhere: a full disk is recognised by the detail
  "not enough disk space", and a cycle's shortened list of conflicts by the detail "and N more"
  (issue #232).

### The window is six pages, and Conflicts is always one

- **Decision.** The window now has seven pages in a sidebar: Status, Activity, Conflicts, Not in the
  Folder, Not Uploaded, Account and Settings; uploads added the fifth. Conflicts is always present,
  with a count badge while there are conflicts, and Not Uploaded likewise ([desktop.md](desktop.md)
  §4).
- **Why.** A page that appears only sometimes is hard to find and moves the others around; a badge
  says the same thing without that.
- **Trade-off.** None.

### An account switcher heads the sidebar

- **Decision.** The window shows one account at a time. A switcher at the top of the sidebar chooses
  it; the six account pages below show that account, and Settings, which holds only what is the
  whole app's, stays one page. The switcher is there with a single account too, and a warning sign
  on it says when another account needs attention.
- **Why.** The pages stay where they were and show the chosen account; KDE's multi-account
  applications, such as NeoChat and Tokodon, put the account selector in the sidebar or the drawer
  in the same way. With one account, the switcher names it and gives "Sign in…" a home.
- **Trade-off.** Two accounts cannot be seen side by side in the window; the tray is the
  overview.

### The tray shows each account, or the worst one; notifications and Places name the account

- **Decision.** The one tray icon shows the worst state across the accounts, and with several
  accounts its tooltip has a line per account. A setting, on by default, gives each account its own
  icon instead, so that which account is paused or in trouble shows without hovering. Notifications
  and transfer progress name the account once there are several. Every Places entry is named `OneDrive — <label>`, with a single account too.
- **Why.** One icon cannot show several states, and trouble must not hide behind an account that is
  fine. A single account's notifications stay as they were, and a second account renames no Places
  entry.
- **Trade-off.** In the tooltip's line, an account that needs attention shows why in place of its
  "checked N ago".

### The command line names the account, and a path decides it

- **Decision.** `konedrivectl` takes the account from `--account` (id, label or email), else from
  `KONEDRIVE_ACCOUNT`, else the only account there is; with several and none named, a command that
  acts on one stops with exit status 2 and lists them. The commands that take a path find the
  account from the path, and refuse `--account`, as do the commands about no one account. `login`
  with no account at all is refused and names `account add` ([desktop.md](desktop.md) §3).
- **Why.** Guessing among several accounts would act on the wrong one sooner or later, and a refusal
  that lists the labels costs one retry. A path already says whose folder it is in; an `--account`
  that disagreed with it would have to be either ignored or obeyed wrongly. An account is added in
  one way, by signing in ([accounts.md](accounts.md) §7.2), so `login` adds none.
- **Trade-off.** An email names an account only once the account has signed in, and
  `KONEDRIVE_ACCOUNT` is ignored by the commands that name no chosen account (issue #231).

### The window registers a folder only with the helper

- **Decision.** The window offers no way to register a folder without interception; that mode is a
  developer's, reached only from the command line.
- **Why.** A folder without interception silently reads zeros wherever a file is not downloaded. It
  exists for development and tests, not for use.
- **Trade-off.** Without the helper, the window can only say how to install it.

### Thumbnails from OneDrive, up to 512 px

- **Decision.** The daemon writes OneDrive's own thumbnails into the freedesktop cache at the sizes
  `normal`, `large` and `x-large`, from one `c512x512` request per image; `xx-large` is not filled.
- **Why.** Dolphin draws a correctly tagged cached thumbnail without opening the file — measured —
  so previews stop costing downloads. One request per image serves the three sizes; a 1024 px
  request would be four times the bytes for a size used only at maximum zoom, and upscaling would
  blur.
- **Trade-off.** At maximum zoom KIO makes its own thumbnail and downloads the file; thumbnails fill
  gradually, behind the account's other transfers ([desktop.md](desktop.md) §8); type detection of
  files without a telling extension still opens them (limitations log K1).

### Keep Baloo out of the folder, and touch only the daemon's own exclusion

- **Decision.** A OneDrive folder is excluded from Baloo. Whether it is already excluded is read
  from `baloofilerc` directly; the daemon records whether it added the exclusion, and Forget removes
  only that. Every call to `balooctl6` has a 10 s timeout.
- **Why.** Baloo reads every file to index it, which would download the whole drive.
  `balooctl6 config list` was observed to print an empty list while the settings file held
  exclusions, so relying on it would have re-added, and later removed, exclusions the user had set.
  A hung indexer must not block registration or Forget.
- **Trade-off.** No Baloo search inside the folder; reading `baloofilerc` by hand follows KConfig's
  format as Baloo writes it today (limitations log W14).

### Download progress as a Plasma job

- **Decision.** A download or an upload still running after 2 s shows in Plasma's notifications as a
  `KJob`, through `KUiServerV2JobTracker`; at most 5 at once for an account and a direction, and one
  more job that stands for the rest.
- **Why.** It is how Dolphin shows its own copy progress, so it looks and behaves like the rest of
  the desktop; short downloads would only flicker.
- **Trade-off.** The daemon's order of "transfer gone" and "transfer failed" is not fixed, so a job
  is held 1.5 s before it is called a success.

### Dolphin: a menu action starts the daemon; the plugins never open a file

- **Decision.** A context-menu action starts a stopped daemon through D-Bus activation; asking what
  the menu may offer does not, so with the daemon stopped the entries are not shown. Neither plugin
  opens a file in the sync folder; emblems come from `lgetxattr`. At most 1000 paths wait at once
  per window, with one `Pin`/`Unpin`/`FreeUp` call per action chosen.
- **Why.** A user clicking "Always Keep on This Device" expects a download, as with any KDE service.
  An open inside Dolphin would download whatever is shown. The cap bounds Dolphin's memory and its
  D-Bus reply budget against a daemon that never answers.
- **Trade-off.** A selection of more than 1000 paths still takes several clicks, and the dedupe is
  by path alone, so an action chosen right after another on an overlapping selection, before the
  first answers, leaves out the paths still waiting.

### Pinning: unchecking only unpins; "Free up space" works on any folder

- **Decision (D-A).** Unchecking "Always Keep on This Device" only removes the pin -- files already
  downloaded stay downloaded, exactly as on Windows. It calls the daemon's `Unpin`, never `FreeUp`;
  only "Free Up Space" frees anything.
- **Decision (D-B).** "Free Up Space" is offered for any folder inside the root, not only a pinned
  or already-downloaded one, again as on Windows -- `FreeUp` already recurses into a folder
  regardless.
- **Why.** Matches what a Windows user already expects of "Always Keep on This Device", and keeps
  the two actions' jobs separate: one manages the pin, the other frees space.
- **Trade-off.** Unpin is refused as one call for the whole selection if any path in it is kept
  pinned by a folder above it that is not unpinned in the same call; the checkbox is then shown
  locked. The daemon says so (`Files.Menu`), and the plugin shows it ([pinning.md](pinning.md) §5,
  §9).

## Testing

### Privileged tests only in a VM

- **Decision.** Everything that needs root — the helper, fanotify, mounting test filesystems — runs
  in a virtme-ng VM booted on the host's own kernel (`tests/vm/run.sh`). The routine run is one
  filesystem (Btrfs); all three (Btrfs, ext4, XFS) run as a separate, slower run, in parallel VMs
  where the host can hold three.
- **Why.** A stalled or crashing helper stays inside the VM, and the host needs no `sudo`. The
  filesystems behave differently in measured ways (ext4's `st_blocks`), so all three are covered,
  but the full matrix takes several minutes.
- **Trade-off.** A regression that shows only on ext4 or XFS surfaces only in the full run. The
  helper's systemd unit is run in the VM only by a mode of its own (`run.sh unit`).

### Real-account runs are scoped and capped

- **Decision.** A test run against a real account needs a short-lived exported access token, opens
  and downloads only inside one named folder (`--graph-folder`), caps each download
  (`--graph-max-bytes`, 32 MiB by default), fetches no thumbnails, and runs the dropped-connection
  and restart-resume checks only on request (`--graph-resume-checks`).
- **Why.** Nothing can be written with a `Files.Read` token, but an unscoped run can still download
  far more of a real drive than a test needs.
- **Trade-off.** The listing still covers the whole drive (above).

### Writes against a real account: only a test account, through a guard at the wire

- **Decision.** Uploads are tested against mock servers everywhere, and against a real account only
  in one harness (`tests/write-account/`), only for a separate test account. It refuses to start
  unless the drive both tokens reach is the one named and is in `write_test_drive_ids`, and looks
  like a test account (under 1 GiB used, under 1000 items; `--large-test-drive` lifts the two
  sizes). Every request to Graph, konedrive's own client's included, goes through a proxy whose
  guard admits a write only inside the run's own folder and within fixed caps, and ends the checks
  at its first refusal ([writes.md](writes.md) §12.1).
- **Why.** Some of what uploads rely on is what the service does, which no mock can say. A guard at
  the wire sees every request whatever code made it, so a bug in a check cannot write elsewhere, and
  the look of the drive refuses a real account even if its id were listed by mistake.
- **Trade-off.** It runs by hand, with exported tokens, and no result of a whole run is on record:
  what it checks is assumed until one is ([writes.md](writes.md) §13; issue #226).

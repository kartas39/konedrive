# Design decisions

The decisions that shape konedrive, each with the reason for it and what it costs. Every entry here
describes the code as it is; where a first design was changed by what measurement or testing showed,
the entry says what the earlier approach was and why it did not hold, because that is usually the
best argument for the current one.

Entries are grouped: [architecture](#architecture), [interception and hydration](#interception-and-hydration),
[the sync](#the-sync), [account and security](#account-and-security),
[the desktop](#the-desktop), [testing](#testing).

## Architecture

### No FUSE, no custom filesystem, no kernel module

**Decision.** The sync folder is a plain directory on the user's own filesystem, not a mount of any
kind. Files that are not downloaded are sparse placeholders; interception uses fanotify permission
events.

**Why.** Every tool sees an ordinary directory with ordinary files. A downloaded file is an ordinary
file on Btrfs, ext4 or XFS, with native performance, and renames within `/home` stay renames. A FUSE
mount puts a userspace process in the path of every read, including reads of downloaded files, and
behaves differently from a local filesystem in ways programs notice. A folder that is a mount point
of a hidden directory would make interception perfectly scoped, but the real files would live
somewhere else — the unnaturalness this project avoids.

**Trade-off.** Interception depends on marks placed per directory (next entries), and a placeholder
opened without interception reads as zeros: every gap in coverage is a way to read zeros, and much
of the design exists to close those gaps. Filesystems must support sparse files, `user.*`
attributes, `O_TMPFILE` and leases; network filesystems are refused.

### Three processes, and the root side is minimal

**Decision.** A root helper that only intercepts, an unprivileged daemon that does everything else,
and clients (window, CLI, Dolphin plugins) that talk to the daemon over D-Bus. The helper has no
network, no tokens and no content logic; it decides with an `fstat`, an `fgetxattr` and a table
lookup.

**Why.** Holding an open until content exists needs a fanotify group of class
`FAN_CLASS_PRE_CONTENT`, which needs `CAP_SYS_ADMIN` — one of the broadest capabilities Linux has.
The less code runs with it, the less can go wrong as root, and the smaller the attack surface
offered to every local user through the helper's socket. The helper is also the one process whose
death hands zeros to waiting programs, so the less it does, the less can kill it.

**Trade-off.** A protocol between helper and daemon, with its own flow control, timeouts and
ownership rules ([hydration.md](hydration.md) §10–§11); every open that needs a download costs a
round trip across it.

### Rust for the helper, daemon and CLI; C++/QML for the window and plugins

**Decision.** The helper, daemon and CLI are Rust; the window is C++/QML on Kirigami; the Dolphin
plugins are C++ against KIO.

**Why.** Memory safety matters most in the root helper and in the daemon that parses network
input. The window and the plugins use KDE's own stack, which is C++.

**Trade-off.** Two toolchains, and a few strings that must say the same thing in both (the
skip reasons, kept in step by a test; limitations log W12).

### Whole-file hydration

**Decision.** Opening a placeholder downloads the whole file before the open returns.

**Why.** Every access path starts with an open, so intercepting the open covers `read`, `mmap`,
`sendfile`, `copy_file_range`, reflink and io_uring by construction. Partial hydration on access
(`FAN_PRE_ACCESS`) needs a kernel hook on every access path, and a missing hook means a program reads
zeros; it was not evaluated.

**Trade-off.** Opening a large file waits for all of it; anything that opens a placeholder, even
briefly — a thumbnailer, an indexer, an IDE — downloads it whole (limitations log P6).

### Extended attributes are the truth; SQLite is a rebuildable map

**Decision.** Each file's state lives in its own `user.konedrive.*` attributes. The tree store
(SQLite) maps the drive and holds the delta link, but is never the only copy of anything: losing it
costs a full listing.

**Why.** State stored with the file travels with it through renames and survives any crash that
the file survives; there is no second copy to fall out of step. The item id is kept on files and
directories alike, plus a map of every item — the same model Microsoft's own client uses.

**Trade-off.** Tools that drop extended attributes (some copy and archive tools) make a file
unmanaged. Attribute writes need write permission on the inode, which the read-only lock has to
open a window for ([sync.md](sync.md) §11). The activity log and conflict list live in the store and
are lost with it.

### Read before write

**Decision.** The work is split into a read phase (this one), Dolphin integration, and a write
phase. Nothing is written to the cloud until the write phase.

**Why.** Writing depends on the real item ids that only reading provides, and a read-only client is
safe to run against a live account from the first day: a bug cannot damage the cloud copy.

**Trade-off.** The folder must be read-only in the meantime (below), and local edits are not
uploaded.

## Interception and hydration

### One fanotify mark per directory, plus evictable ignore marks

**Decision.** The helper marks every directory inside the sync root with
`FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`, and places an evictable ignore mark on each downloaded file
the first time it is opened.

**Why.** Kernel memory grows with the number of folders, not files: 1658 bytes per directory mark,
measured on a tree of 10 000 directories and 100 000 files, so about 16.6 MB for 10 000 folders.
A mark per placeholder costs the same per file — about 330 MB for 200 000 files. A single
filesystem-wide mark with a path filter would cost almost nothing, but would put the helper in the
path of every open in `/home`, so a stalled helper would freeze the desktop; with directory marks
only the sync folder is affected. The kernel has no recursive or subtree marks.

**Trade-off.** Every directory must be walked and marked at registration and at every helper start
(1.03 s for 10 000 directories and 100 000 files, cold; limitations log P3); a directory created by
another program is covered only once something marks it (M1's gap); the first open of each
downloaded file costs one round trip to the helper (about 0.1 ms), and again after the kernel
reclaims its ignore mark.

### `FAN_OPEN_PERM`, not `FAN_PRE_ACCESS`

**Decision.** Interception is on open.

**Why.** See "Whole-file hydration": coverage by construction. The design would switch only if
`FAN_PRE_ACCESS` were shown to cover every access path.

**Trade-off.** No lazy or partial hydration.

### Ignore marks survive modification, and nothing is emptied under one

**Decision.** Ignore marks carry `FAN_MARK_IGNORED_SURV_MODIFY`. Every place that empties a file —
free-up, recovery, the roll-back of a failed fill — first makes `dehydrating` or `hydrating`
durable, then follows one local rule: with a link, the helper clears the mark and any failure stops
the punch; with no helper bound to its socket, go ahead; with a helper bound and no link, refuse
and retry (invariant M3).

**Why.** Without `SURV_MODIFY` the kernel silently creates no ignore mark on a file anyone holds
open for writing — and the helper always marks through an `O_RDWR` descriptor. With it, a punch no
longer clears the mark, and a file emptied under its mark reads zeros forever. The rule is applied
at each punch because an earlier argument — that a folder registered without interception could
carry no stale mark — was disproved three times, each time by a race nobody had foreseen: an idle
file carried across a re-registration, a fill finishing after a Forget, a file renamed past the
registration walk. The local rule depends on nothing that happened before the punch.

**Trade-off.** Freeing up space, recovery and some fills need the helper, and are refused
`NoHelper` while a helper runs that the daemon cannot reach (limitations log F11). The rule rests on
the daemon seeing the helper's `/run` (F12).

### The helper marks a file only after reading `hydrated`, and reads it back

**Decision.** An ignore mark is placed only on a file the helper reads `hydrated`, and kept only if
a second read after placing it still says `hydrated`. A file with no konedrive attributes is never
marked.

**Why.** The `O_TMPFILE` open that starts building a placeholder is itself intercepted before any
attribute exists; an unconditional mark would blind the placeholder under construction, and every
later open would read zeros. And a mark placed just after a free-up read `hydrated` but before it
wrote `dehydrating` would otherwise survive the punch.

**Trade-off.** One extra attribute read per marking.

### The commit point is `state=hydrated`, written last; a roll-back demotes first

**Decision.** A fill writes the size, the time, the data sync and the stamp before it writes
`state=hydrated`. A failed fill writes `online-only` before it punches anything.

**Why.** `hydrated` is what makes the helper allow and ignore-mark the file. Written earlier, a full
or failing disk produced a file marked `hydrated` holding zeros: the helper denied one opener and
then allowed the next onto the empty file. In the roll-back, a crash between punch and demotion
would leave a state promising content just removed; demoting first makes the worst case an
`online-only` file with stale bytes, which the next fill overwrites.

**Trade-off.** None worth naming; no step of a fill may be best-effort.

### A resumed stream is written where it starts, or not at all

**Decision.** Every content source reports the offset its stream actually starts at, and a fill
refuses a stream that starts anywhere but where it asked. A declared size of 0 for a placeholder
with a non-zero size is refused.

**Why.** An HTTP server may answer a `Range` request with `200` and the whole body. Written at the
resume offset, the file's beginning lands in its middle while every byte is accounted for, and the
fill reports success. A size of 0 would truncate the file on one unconfirmed answer that nothing
could retry.

**Trade-off.** A server that ignores ranges costs a failed attempt; a file genuinely emptied in the
cloud is left to the metadata sync.

### Only errnos the kernel accepts reach the opener

**Decision.** The daemon reports `EIO` for every failure to produce content (a missing item, a
dropped connection, a timeout), `ENOSPC`/`EDQUOT` for a full disk, and nothing else; the helper
clamps to the accepted set and falls back to a plain deny if the kernel still refuses.

**Why.** `FAN_DENY` accepts exactly `EPERM, EIO, EAGAIN, EBUSY, ETXTBSY, ENOSPC, EDQUOT`. Any other
value makes the response fail with `EINVAL` and leaves the opener suspended until the group closes —
and `ENOENT`, `ECONNRESET` and `ETIMEDOUT` are exactly what a network source produces.

**Trade-off.** Programs see `EIO` for every network problem (limitations log P5).

### Event descriptors are non-blocking

**Decision.** The helper's event descriptors are opened `O_NONBLOCK`.

**Why.** The kernel opens each event's descriptor inside the helper's `read()`. Opening a file that
anyone holds a write lease on then waits for the lease — which stopped every intercepted open on
the machine for as long as the lease was held, and any local user can hold one. With `O_NONBLOCK`
the kernel answers that one event itself.

**Trade-off.** An open of a leased file — a free-up's punch lasting a few milliseconds, or a file
another program leases — gets `EPERM` at once instead of waiting (limitations log P2).

### Queue beyond the credit, and make the helper not die

**Decision.** At most 64 fill requests are outstanding per daemon connection; beyond that, opens
wait in the helper rather than being refused. The helper is built so that it does not exit: a
bounded worker pool, caught panics, survivable descriptor exhaustion, per-event handling of
descriptors the kernel cannot open, and per-uid caps.

**Why.** Without a credit, two bounded queues deadlocked under a burst. Refusing beyond it gave
`EAGAIN` to most of 3000 concurrent opens — a thumbnailer opening a folder of photos would fail most
of them. Queuing means every suspended open is exposed if the helper dies, because the kernel then
allows them all onto unfilled files; the answer to that is a helper that does not die, not a
smaller queue, which would only move the number and bring the refusals back.

**Trade-off.** A helper crash, update or stop hands zeros to every open in flight (limitations log
Z1). Measured with 3000 concurrent opens: all filled, none refused.

### Free-up runs on one descriptor, under a write lease

**Decision.** Dehydration opens the file once, beneath the root, and does everything — the checks,
`dehydrating`, `ClearIgnore`, the lease, the punch, restoring the time — through that one
descriptor, under a write lease and the per-inode lock keyed by device and inode.

**Why.** An earlier version opened the path four times and punched the fourth: an editor's atomic
save landing in between had the guard pass on the old inode and the punch empty the new one —
measured, 300 KiB of fresh data destroyed with success reported. A lease is granted only when nobody
else has the file open or mapped, so nothing is punched under a reader. A lock keyed by path did not
serialise two names for one file.

**Trade-off.** A file open or mapped anywhere cannot be freed up ("in use"). `SIGIO` must be ignored,
since a broken lease signals the holder and the default action kills it.

### "Download now" fills directly

**Decision.** `Hydrate` opens the file beneath the root with `openat2(RESOLVE_BENEATH |
RESOLVE_NO_SYMLINKS)`, then takes the per-inode lock, then reads the state again, then fills — it
does not open the file and let interception do the work.

**Why.** Without interception (the developer's mode) an open fills nothing. With it, taking the
lock before the open deadlocked against the fill the open triggered. And checking a canonical path,
waiting, then opening the name let a directory rename send the fill outside the root while it
reported success.

**Trade-off.** Two paths into one fill routine, which must both look again under the lock.

### Recovery runs after re-registration, one file at a time, without waiting

**Decision.** At startup and on every reconnect the daemon re-registers its root with the helper and
then recovers interrupted files; it takes each file's per-inode lock without waiting, counting a busy
file rather than blocking on it; it reads the state again under the lease.

**Why.** A file left `dehydrating` may still carry its ignore mark, and only the helper can clear it
for a user with a registered root. Holding every file open exhausted descriptors (half of 2000
interrupted files went unrecovered). Waiting for a fill from the previous connection would hold the
reconnect behind a download, and a fill that finished in between must not be punched.

**Trade-off.** A busy file is recovered at the next pass, not now (limitations log W10).

### A registration is recorded before the helper hears of it; Forget goes through the helper

**Decision.** An intercepted registration is written to `config.toml` before the helper is told, and
undone at the helper on failure — or kept intercepted when the helper cannot confirm it let go. An
intercepted folder is forgotten through the helper or not at all. An unreadable `config.toml` is
never overwritten.

**Why.** A folder the helper may hold must never be one the daemon treats as unintercepted: its
marks would stay, its ignore marks with them, and a later free-up could punch under one. A Forget
with no link would leave the tree intercepted with no daemon to answer, and every placeholder would
fail `EIO`.

**Trade-off.** Forget is refused `NoHelper` while the helper is unreachable; a lost or hand-edited
`config.toml` can still leave the helper holding a folder the daemon forgot (limitations log Z6).

### `ClearIgnore` is authorised by owning the file

**Decision.** The helper clears an ignore mark on any regular file the asking user owns, whether or
not that user has a registered root.

**Why.** A folder registered without interception must still clear marks before every punch, and
may hold no root with the helper. Removing a mark can only send the file's next open back to the
helper — one extra interception, never zeros.

**Trade-off.** A user can clear marks on their own files (limitations log W9).

### Per-user limits on the helper's socket

**Decision.** The socket is open to every local user, and every request is authorised by the peer's
uid and the object's owner. Each uid may hold 16 connections; at most 8 workers wait for one user's
absent daemon and 32 for all; mark requests are limited to objects the user owns on the device of
one of their roots; a root id another user registered is refused.

**Why.** Any local user can connect. Without these, one user could exhaust the helper's descriptors,
park every worker by opening another user's placeholders while that user's daemon is down, make the
helper intercept files they can read but do not own, or replace another user's registration.

**Trade-off.** The 32-waiter cap is chosen, not measured; containment inside a root is the daemon's
job, with the helper only checking the device.

## The sync

### Staging, then swap; the delta link moves only when the folder matches

**Decision.** A cycle stages the changes, reconciles the folder against the staged tree, and only
then, in one transaction, swaps the tree in with the new delta link.

**Why.** A crash anywhere before the swap leaves the old link, and the next cycle asks for the same
changes again. The reconcile is idempotent, so doing it twice is safe; skipping a change is not.

**Trade-off.** A delta is held in memory whole before it is staged (limitations log D11).

### A holding directory keyed by item id, and two reconcile scopes

**Decision.** Items not where the new tree wants them move to `.konedrive-holding/<item id>`,
deepest first; the tree is then placed top down; what is left in holding is gone. A Full reconcile
walks the whole folder; a Changed one only the delta's items, turning Full as soon as the folder
disagrees with the stored tree.

**Why.** The first design renamed each side of a name collision to a swap name resolved at the end
of a batch. It had no clean answer for a cycle of three or more renames, or for a crash that left
some swap names and not others. Keying everything in transit by item id resolves swaps, cycles of
any length and crashes the same way. Full at every start, after a failure and for large deltas
keeps the folder honest; Changed keeps an ordinary minute's cycle cheap.

**Trade-off.** A Full reconcile scans the whole folder; a file stuck mid-fill makes every cycle Full
until it settles (limitations log F13). The 5000-change threshold is a guess.

### The first listing is placed page by page

**Decision.** A folder's first listing places each page as it arrives and commits it with the link
to the next page; every later listing is staged whole.

**Why.** Staged whole, the first listing of a large drive left the folder empty for minutes. Page by
page, the folder fills as the drive is listed and a stopped listing resumes where it stopped. Later
listings keep staging, because part-way through a listing an item not listed yet cannot be told from
one that is gone.

**Trade-off.** Several corner cases in which a partial download is lost and fetched again, and the
first page after each interruption is reconciled Full (limitations log F35).

### Replace a changed download beside the old one, then rename

**Decision.** A downloaded file that changed in the cloud is replaced by downloading the new version
into an `O_TMPFILE` in the same directory, verifying it, and renaming it over the old one — only if
both versions fit on disk.

**Why.** Writing over the old version in place would show a reader a mix of both. A rename is
atomic: a reader keeps the old version, the next open gets the new one.

**Trade-off.** Both versions must fit at once; when they do not, the old version stays and the
status says why. A failed replacement is retried every cycle, without forcing a Full reconcile —
forcing one turned every cycle into a whole-folder scan exactly when the disk was too full.

### Verify every download against `quickXorHash`

**Decision.** A file is marked `hydrated` only if its bytes match the `quickXorHash` Graph reported;
a mismatch or a mid-download version change restarts the download once.

**Why.** It is the one hash Graph provides for personal and business drives alike. Verification
covers the whole file, including a prefix resumed from disk, so nobody is ever handed wrong bytes.

**Trade-off.** A file Graph hands out without a hash is downloaded unverified and only logged: it is
treated as an anomaly, not a standing status (limitations log F34).

### Keep partial downloads, and check them when resuming

**Decision.** Every 16 MiB a fill makes its progress durable (`user.konedrive.progress`). A failed
fill and crash recovery keep that prefix; the cTag is compared when a fill resumes, and a checkpoint
for another version, or for a version with no hash, is discarded then.

**Why.** Without it, a dropped connection or a restart during a multi-gigabyte download started it
over from zero. Keeping the prefix is safe because the resumed file is verified whole.

**Trade-off.** A file shown as `online-only` may hold part of its blocks, and free-up refuses it
until the download finishes or the cloud version changes (limitations log F33). The 16 MiB interval
is a guess.

### Judge a placeholder's content by cTag and size, never by its time

**Decision.** A reconcile compares a placeholder's cTag and size with the tree's. A time that
differs alone only gets the cloud's time back.

**Why.** A fill's writes move the file's time to now. Taking that for a new version punched a
partial download away at the Full reconcile every restart begins with.

**Trade-off.** None.

### The folder is read-only in this phase; the guarantee is the stamp check

**Decision.** Files `0444`, directories `0555`. The daemon lifts the owner's write bit only for the
moment of its own attribute writes and directory changes. Before any change from the cloud is
applied, the file is checked against its stamp and rescued if it changed.

**Why.** With nothing uploaded yet, a local edit could only diverge from the cloud. Directories are
locked too, because editors save by writing a new file and renaming it. `user.*` attribute writes
need inode write permission even for the owner, hence the per-operation window.

**Trade-off.** A program that opens a file for writing inside a window keeps a writable descriptor,
so the lock is the rule a person sees, not the guarantee (limitations log W2). The lock comes off
when uploads arrive.

### A rescue is one rename, never a copy

**Decision.** A file holding local work is moved with one `RENAME_NOREPLACE` into a rescue
directory on the folder's own filesystem; across filesystems the rescue fails safely.

**Why.** A copy followed by a delete could delete bytes it never finished copying, would leave the
subtree unlocked for the duration, and needs its own crash protocol — much more code, for a rare
layout, behind a guarantee that must not fail. A rename cannot lose bytes.

**Trade-off.** A folder that is itself a mount point, or whose parent cannot be written, cannot be
rescued into, and its cycles fail until it is moved (limitations log F16).

### No OneDrive sync without the helper

**Decision.** A OneDrive folder is registered only with the helper connected, and its cycles run
only while the folder is intercepted and linked; `RegisterRootWithoutInterception` always makes a
local folder. The daemon publishes the helper's state from systemd with instructions.

**Why.** Placeholders that nothing intercepts read as zeros; a folder full of them is worse than an
empty one. Earlier, a OneDrive folder could be registered without interception and kept in step,
and the only sign of a missing helper was "not connected".

**Trade-off.** Without the helper, nothing syncs; a folder waits in `error` with the instruction.

### A folder made without the helper because none was there switches when it arrives

**Decision.** A registration made without interception because no helper was connected records
that, and switches to interception when the helper connects. One made while a helper was connected
was a choice and stays.

**Why.** A folder registered before the helper was installed otherwise stayed unintercepted — every
open read zeros — until it was forgotten and registered again.

**Trade-off.** The switch walks and marks the whole tree under the lifecycle lock; a switch whose
outcome the helper cannot confirm keeps the folder intercepted with its sync stopped until the next
connect (limitations log F30).

### The lifecycle lock is held only from reconcile to swap

**Decision.** A cycle's Graph phase runs without the lock that guards the registration.

**Why.** Holding it across a Graph fetch blocked a helper reconnect — and so every fill — behind the
listing. The Graph phase writes only `staging` and `meta`, which a Forget does not read before it
stops the poller.

**Trade-off.** A stop waits for a reconcile already under way (limitations log F15).

### Check the account every cycle

**Decision.** Each cycle compares the drive id with the one the folder was built from, kept in the
tree store and in `config.toml`; a mismatch blocks the cycle.

**Why.** A sign-out followed by a different sign-in between two polls would defeat a check made only
at sign-in, and the result would be another account's files listed over this folder. Keeping the id
in `config.toml` too covers a store rebuilt empty.

**Trade-off.** One `GET /me/drive` per cycle.

### Skip what cannot be a local file, visibly

**Decision.** Names over 255 bytes, the Personal Vault, shared folders added to the drive, OneNote
notebooks, names with the `.konedrive-` prefix and malformed items are skipped, recorded with their
reason and listed.

**Why.** Each either cannot exist as a local file or does not belong to this drive; a truncated or
renamed copy would be a different file with a misleading name. A skip nobody can see looks like data
loss.

**Trade-off.** Those items are not available locally.

### Take the download URL from the item's metadata

**Decision.** A fill asks for the item's metadata and streams from its
`@microsoft.graph.downloadUrl`, falling back to `/content` only when there is none.

**Why.** One request gives size, cTag, hash and URL together, and fresh metadata on every attempt
also replaces an expired URL.

**Trade-off.** None.

### Whole-drive listing, even when only one folder matters

**Decision.** The daemon lists the drive from its root.

**Why.** Graph's delta for a personal drive, and for OneDrive for Business, is available only from
the drive root; one delta link per sync root.

**Trade-off.** A real-account test run lists the whole drive even though it downloads only in one
named folder (limitations log W15).

## Account and security

### Sign-in in the system browser, with PKCE and a loopback redirect

**Decision.** OAuth authorization code with PKCE, the system browser, a single-use loopback
listener, personal accounts only; each user registers their own Entra application.

**Why.** Microsoft's recommendation for desktop applications (RFC 8252): the password and second
factor stay in the browser, which keeps the user's Microsoft session, and there is no embedded web
view to trust.

**Trade-off.** A one-time app registration per user (the README has the steps); the consent screen
calls the app unverified.

### The daemon owns the token; the refresh token lives only in the Secret Service

**Decision.** The daemon holds the refresh token in KWallet (through the Secret Service), with no
plaintext fallback, and the access token in memory only. The window never sees a token.

**Why.** The daemon needs the token while the window is closed, and one owner of a secret is easier
to reason about than two. A plaintext fallback would put the most valuable secret on disk.

**Trade-off.** No Secret Service, no sign-in. A locked wallet prompts, and a refusal is a transient
error.

### Scope `Files.Read`

**Decision.** The daemon asks for `Files.Read User.Read offline_access`.

**Why.** Microsoft refuses any write made with that token, so "nothing is written to the cloud" is
enforced by the server, not by the client's discipline.

**Trade-off.** The write phase will need incremental consent for `Files.ReadWrite`.

### An access-token export for test runs, in every build

**Decision.** `Dev1.AccessToken()` and `konedrivectl dev export-access-token` hand out the current
access token (about an hour of read access), never the refresh token, written atomically to a
`0600` file.

**Why.** A test run in the VM needs to speak to Graph without a sign-in of its own, and the refresh
token must never leave the Secret Service. A per-user development install needs it, so it is not
gated behind a build flag.

**Trade-off.** Any process of the same user on the session bus can obtain an hour of read access —
no more than it has by opening files in the folder; a Flatpak app is filtered by its bus proxy
(limitations log W11).

## The desktop

### Notifications come from the app

**Decision.** The window app, which runs at login in the tray, sends KDE notifications; the daemon
sends none.

**Why.** KNotification gives the user KDE's per-event settings in System Settings, and the app is
the process that belongs to the desktop session.

**Trade-off.** With the app quit, nothing notifies, and events that happened meanwhile are never
announced; the window's lists still show everything (limitations log A1).

### Events have kinds; the app never parses wording

**Decision.** A failed replacement is its own activity kind, `update-failed`, distinct from a failed
download. A rescue is carried as a conflict (`Conflicts()`, `ConflictCount`, `conflict` events),
not as a note in `LastError`. Refusals are D-Bus error names.

**Why.** A heuristic on the daemon's wording breaks on any rewording. Carrying the rescue as text in
`LastError` made the app filter it out by pattern so that it did not show as a sync error.

**Trade-off.** A few readings still depend on exact strings: a full disk is recognised by the detail
"not enough disk space" (limitations log A2).

### The window is six pages, and Conflicts is always one

**Decision.** Status, Activity, Conflicts, Not in the Folder, Account and Settings, in a sidebar.
Conflicts is always present, with a count badge while there are conflicts.

**Why.** A page that appears only sometimes is hard to find and moves the others around; a badge
says the same thing without that.

**Trade-off.** None.

### The window registers a folder only with the helper

**Decision.** The window offers no way to register a folder without interception; that mode is a
developer's, reached only from the command line.

**Why.** A folder without interception silently reads zeros wherever a file is not downloaded. It
exists for development and tests, not for use.

**Trade-off.** Without the helper, the window can only say how to install it.

### Thumbnails from OneDrive, up to 512 px

**Decision.** The daemon writes OneDrive's own thumbnails into the freedesktop cache at the sizes
`normal`, `large` and `x-large`, from one `c512x512` request per image; `xx-large` is not filled.

**Why.** Dolphin draws a correctly tagged cached thumbnail without opening the file — measured — so
previews stop costing downloads. One request per image serves the three sizes; a 1024 px request
would be four times the bytes for a size used only at maximum zoom, and upscaling would blur.

**Trade-off.** At maximum zoom KIO makes its own thumbnail and downloads the file (limitations log
K15); thumbnails fill gradually, two a second (K16); type detection of files without a telling
extension still opens them (K1).

### Keep Baloo out of the folder, and touch only the daemon's own exclusion

**Decision.** A OneDrive folder is excluded from Baloo. Whether it is already excluded is read from
`baloofilerc` directly; the daemon records whether it added the exclusion, and Forget removes only
that. Every call to `balooctl6` has a 10 s timeout.

**Why.** Baloo reads every file to index it, which would download the whole drive. `balooctl6
config list` was observed to print an empty list while the settings file held exclusions, so relying
on it would have re-added, and later removed, exclusions the user had set. A hung indexer must not
block registration or Forget.

**Trade-off.** No Baloo search inside the folder; reading `baloofilerc` by hand follows KConfig's
format as Baloo writes it today (limitations log W14).

### Download progress as a Plasma job

**Decision.** A download still running after 2 s shows in Plasma's notifications as a `KJob`,
through `KUiServerV2JobTracker`; at most 5 at once.

**Why.** It is how Dolphin shows its own copy progress, so it looks and behaves like the rest of
the desktop; short downloads would only flicker.

**Trade-off.** The daemon's order of "transfer gone" and "transfer failed" is not fixed, so a job is
held 1.5 s before it is called a success (limitations log A11).

### Dolphin: "Download" starts the daemon; the plugins never open a file

**Decision.** The context-menu action starts a stopped daemon through D-Bus activation. Neither
plugin opens a file in the sync folder; emblems come from `lgetxattr`. At most 1000 calls wait at
once per window.

**Why.** A user clicking "Download" expects a download, as with any KDE service. An open inside
Dolphin would download whatever is shown. The cap bounds Dolphin's memory and its D-Bus reply budget
against a daemon that never answers.

**Trade-off.** Very large selections take several clicks until a batch method exists (limitations
log K6).

## Testing

### Privileged tests only in a VM

**Decision.** Everything that needs root — the helper, fanotify, mounting test filesystems — runs in
a virtme-ng VM booted on the host's own kernel (`tests/vm/run.sh`). The routine run is one
filesystem (Btrfs); all three (Btrfs, ext4, XFS) run in parallel VMs as a separate, slower run.

**Why.** A stalled or crashing helper stays inside the VM, and the host needs no `sudo`. The
filesystems behave differently in measured ways (ext4's `st_blocks`, inode reuse), so all three are
covered, but the full matrix takes several minutes.

**Trade-off.** A regression that shows only on ext4 or XFS surfaces only in the full run
(limitations log W13). The helper's systemd unit itself runs only in a real installation (W16).

### Real-account runs are scoped and capped

**Decision.** A test run against a real account needs a short-lived exported access token, opens and
downloads only inside one named folder (`--graph-folder`), caps each download (`--graph-max-bytes`,
32 MiB by default), fetches no thumbnails, and runs the dropped-connection and restart-resume checks
only on request.

**Why.** Nothing can be written with a `Files.Read` token, but an unscoped run can still download
far more of a real drive than a test needs.

**Trade-off.** The listing still covers the whole drive (above).

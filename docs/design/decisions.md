# Design decisions

The decisions that shape konedrive, each with the reason for it and what it costs. Every entry here
describes the code as it is; where a first design was changed by what measurement or testing showed,
the entry says what the earlier approach was and why it did not hold, because that is usually the
best argument for the current one.

Entries are grouped: [architecture](#architecture), [interception and hydration](#interception-and-hydration),
[the sync](#the-sync), [uploads](#uploads), [account and security](#account-and-security),
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

**Decision.** The work was split into a read phase, Dolphin integration, and a write phase, and
reading is still the default: a new account is read-only, and uploads happen only for an account
switched to read-write ([writes.md](writes.md)).

**Why.** Writing depends on the real item ids that only reading provides, and a read-only client is
safe to run against a live account from the first day: a bug cannot damage the cloud copy.

**Trade-off.** A read-only account's folder must be locked (below), and its local edits are not
uploaded.

### Several accounts in one daemon, sharing one helper link

**Decision.** One daemon serves every account of a user. Each account has its own sign-in, folder,
tree store and poller, and D-Bus objects of its own; the link to the helper, the per-inode locks and
the fill-on-open slots belong to the daemon and are shared. The helper did not change.

**Why.** The helper sends a user's opens to that user's newest connection only, so a second
connection — a second daemon, or a second link from the same one — would take every request away
from the first. The helper already held any number of folders per user, refused folders that nest,
and authorised by uid; keeping accounts out of it keeps the root side as small as it was.

**Trade-off.** The accounts share four fill slots and the helper's credit of 64 requests, so a large
pin in one slows opens in another, and a reconnect brings every folder up in turn before any fill
is served (limitations log F43).

### An intercepted open finds its account by device, verified path, then item id

**Decision.** The daemon decides which account a fill request belongs to from its descriptor alone:
the accounts whose folder is on the file's filesystem; then the kernel's name for the file, opened
beneath a candidate folder and compared by inode; then the file's item id in each candidate's tree
store. With no answer, the open is denied `EIO`.

**Why.** The helper's request carries no account, and adding one would put account knowledge in the
root helper. A wrong answer would fill a file from another account's drive, so each step either
proves its answer or passes; routing never guesses.

**Trade-off.** A file renamed or unlinked while its open waits, on a filesystem that holds two
folders, and known to no tree store, is denied `EIO`; the next open retries (limitations log F44).

### Per-file calls go to one interface, routed by path

**Decision.** `Hydrate`, `Dehydrate`, `ItemState`, `Pin`, `Unpin` and `FreeUp` are on the
daemon-wide `org.konedrive.Files1`, which finds each path's account by its folder. Each account's
`Sync1` keeps what concerns its folder as a whole.

**Why.** The Dolphin plugin and `konedrivectl` know a path, not an account. Making every client
find the account first would repeat the routing in each of them, and a selection in Dolphin may
span two accounts' folders.

**Trade-off.** One path in no account's folder refuses a whole `Pin`, `Unpin` or `FreeUp` call, as
one path outside the folder always did.

### No alias for the single-account D-Bus object

**Decision.** `/org/konedrive/Daemon` is no longer served. The window, the CLI and the Dolphin
plugins moved to `/org/konedrive/Accounts` and the per-account objects in the same change.

**Why.** konedrive has no outside clients, and its clients ship in the same packages as the daemon.
An alias would need a meaning for "the first account" that changes when that account is removed,
would double every `PropertiesChanged`, and would need tests of its own, to serve no one.

**Trade-off.** A window or a Dolphin running across the upgrade calls an object that is gone, until
it is restarted (limitations log F46).

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

### A read-only account's folder is locked; the guarantee is the stamp check

**Decision.** Files `0444`, directories `0555`. The daemon lifts the owner's write bit only for the
moment of its own attribute writes and directory changes. Before any change from the cloud is
applied, the file is checked against its stamp and rescued if it changed.

**Why.** With nothing uploaded yet, a local edit could only diverge from the cloud. Directories are
locked too, because editors save by writing a new file and renaming it. `user.*` attribute writes
need inode write permission even for the owner, hence the per-operation window.

**Trade-off.** A program that opens a file for writing inside a window keeps a writable descriptor,
so the lock is the rule a person sees, not the guarantee (limitations log W2). A read-write
account's folder has no lock: its changes are uploaded instead ([writes.md](writes.md) §2.2).

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
tree store and in `config.toml` (as the account's drive, which is also its identity, below); a
mismatch blocks the cycle.

**Why.** A sign-out followed by a different sign-in between two polls would defeat a check made only
at sign-in, and the result would be another account's files listed over this folder. Keeping the id
in `config.toml` too covers a store rebuilt empty.

**Trade-off.** One `GET /me/drive` per cycle.

### A folder belongs to one account, and remembers which

**Decision.** Two accounts' folders never nest: a registration that would is refused `Overlaps`,
naming the other account. A OneDrive folder's root carries its account's drive
(`user.konedrive.drive`), and a registration of a folder that carries another drive is refused.

**Why.** The helper refuses nested roots anyway; checking in the daemon names the refusal, and covers
folders without interception, which the helper never sees. A forgotten folder keeps its root id and
may be registered again without being empty, so without the drive attribute another account could
adopt it and reconcile its own drive over the first account's files.

**Trade-off.** A folder forgotten before multiple accounts carries no drive and can still be adopted
(limitations log F45).

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

## Uploads

These describe a read-write account's folder ([writes.md](writes.md)). Where Windows' OneDrive
client has an answer a user already knows, it is followed.

### Local changes come from a second fanotify group, in the daemon

**Decision.** A read-write folder's directories carry a second fanotify mark, in an unprivileged
notification group the daemon owns (`FAN_REPORT_DFID_NAME_TARGET`: creations, deletions, renames,
closes after writing, attribute changes). Events only mark directories dirty; a quiet batch is then
examined against the base. A Full local scan at bring-up and after anything that can lose events
is the safety net.

**Why.** The kernel refuses file-handle reporting, which directory-entry events need, in the
helper's pre-content group, so the helper's marks cannot carry them. A group of the daemon's own
needs no privilege and keeps the helper exactly as it was. Events as hints, with the disk as the
answer, make a merged, lost or overflowed event cost a scan, never a missed change.

**Trade-off.** A second mark per directory, from a per-user budget every account shares, and a
queue that a large unpack can overflow (limitations log F70, F71). An edit that kept both size and
time while nothing watched is not found (F52).

### An item is its id and its inode, never its path

**Decision.** A file on disk is matched to its item by the `user.konedrive.item-id` it carries; two
inodes with one id are told apart by the file handle recorded when the item was placed. A rename is
an id under a new name; an editor's save by rename is a new inode taking over an id, so the item
keeps its version history and sharing links.

**Why.** Paths are what changes; ids travel with the inode through every rename, and a handle is
unique per filesystem. Guessing identity from names would turn every save into a delete and a
create.

**Trade-off.** A copy that kept the attributes needs the recorded handle to be told from the
original; a rebuilt store has none, and falls back to the item's place (limitations log F53).

### The disk is the truth about local changes; the outbox is only intent

**Decision.** The outbox, one row per item in the tree store, records what is to be sent and how
far it got. Every row can be found again by comparing the disk with the base, and a rebuilt base
produces no deletes.

**Why.** A lost or corrupted store then costs a listing and restarted uploads, never a local byte
and never a wrong delete in OneDrive.

**Trade-off.** A delete not sent before the store was lost is forgotten, and the item comes back
from OneDrive.

### Small files go up in an upload session

**Decision.** Every non-empty file is uploaded through an upload session, one request's body up to
10 MiB and 10 MiB fragments above that. Only an empty file, which a session cannot carry, uses the
plain `PUT`.

**Why.** Microsoft documents `If-Match` and `conflictBehavior` for the session, and it carries the
file's time (`fileSystemInfo`) and its size, so a full drive refuses before a byte is sent. For the
plain `PUT` its current page documents neither guard, and the time would take a second request
anyway: the session costs nothing extra. 10 MiB is Microsoft's own boundary for resumable
transfers.

**Trade-off.** Two requests for every small file, and a guard on the empty `PUT` that is assumed
until the test-account run ([writes.md](writes.md) §13).

### Every write is guarded, and a failed guard is settled by reading again

**Decision.** `If-Match` on every change, `conflictBehavior=fail` on everything new, the folder's
cTag on a folder's delete. A `412` or `409` is followed by reading the item and deciding: the same
hash is adopted, a change of metadata only is sent again with the fresh tag, anything else is a
conflict. Nothing is ever sent without its guard.

**Why.** It is the only way two writers cannot overwrite each other, and it makes every step
replayable after a crash: "did my request land?" is answered by the content hash.

**Trade-off.** An extra read on every refused guard; and a session checks `If-Match` when it is
created, not when it completes, which leaves a window of one fragment (limitations log F80).

### Changed on both sides: keep both, named after the machine

**Decision.** The version in OneDrive keeps the name; this computer's is renamed beside it to
`<name>-<machine>.<ext>` and uploaded as a new file, and the pair is listed as a conflict. A delete
here of something edited in OneDrive is undone, and so is a delete in OneDrive of something edited
here. For renames, the first to reach OneDrive wins.

**Why.** It is what Windows does, so the result is what a user expects, and neither side's work is
lost. The machine name says where the copy came from.

**Trade-off.** Copies accumulate until the user merges them; a rename made here can be undone by
one made first in OneDrive.

### Names OneDrive refuses are listed, not substituted

**Decision.** A file whose name OneDrive refuses (`" * : < > ? \ |`, a leading or trailing space, a
reserved name) is not uploaded: its change is blocked and listed under "Not Uploaded", with the
reason, until the user renames it. No look-alike character is put in its place.

**Why.** Windows does the same. A substituted name would differ between the folder and OneDrive
for good, and every other device would see a name nobody chose.

**Trade-off.** A `:` or `?` in a Linux file name is common, and each such file stays local until
renamed by hand.

### A move out of the folder downloads first

**Decision.** A file or folder moved out of the folder is deleted in OneDrive only once its content
is on this computer: a placeholder that left is marked again, downloaded where it went, stripped of
konedrive's attributes, and only then deleted. Sent to the desktop Trash instead, a placeholder is
removed without a download.

**Why.** Windows does the same. A move out is a delete for OneDrive, and the user still holds the
file; deleting it first would leave them an empty placeholder where they expect their file. The
Trash is the exception because the user asked for a delete, and OneDrive keeps the item in its
recycle bin.

**Trade-off.** A large cloud-only folder moved out is a large download, with no prompt; until it
is done the item stays in OneDrive, and other devices still see it (limitations log F121).

### Deleted or moved out: the object decides, by its file handle

**Decision.** An item missing from the folder is asked after by the file handle recorded for it,
through the helper's `OpenByHandle`: gone is a delete, alive elsewhere is a move out, and any other
answer decides nothing.

**Why.** Events cannot tell: one can be lost, and a move made while the daemon was not running
raises none that it sees. The object can. An unprivileged daemon cannot open a handle, and the
helper already holds the capability for its walks, so the helper gained one narrow message rather
than the daemon a privilege.

**Trade-off.** New root code reachable by every local user, answering only for the user's own
object carrying konedrive's attribute (SECURITY.md; limitations log F90). A delete waits while the
helper is not connected (F54).

### Nested subvolumes and other devices are not uploaded

**Decision.** Anything on another device than the folder's root — a nested Btrfs subvolume, a
filesystem mounted inside the folder — is neither watched nor uploaded, and is listed under "Not
Uploaded" as `other-device`.

**Why.** The helper cannot mark a directory on a device where the user holds no folder, so a
placeholder there could never be protected. Windows treats a mount point inside OneDrive the same
way.

**Trade-off.** Files in such a place stay on this computer only (limitations log F72).

### A large delete waits for the user

**Decision.** A batch of removals of more than 500 items, or 20 % of the folder, is held until the
user confirms it or asks for the items back.

**Why.** Windows asks too. `rm -rf` of the wrong directory, or a folder swapped for an empty one,
should not empty OneDrive before anyone looks.

**Trade-off.** Nothing of such a delete reaches OneDrive until someone answers; the thresholds are
provisional.

### A stale change from OneDrive is read again, not dropped

**Decision.** A change the delta feed fetched before an upload committed, about the item it
committed, is not trusted unless it is the commit itself: the item is read again, under the tree
lock. OneDrive's change to an item with local work waiting is kept in the store until the disk takes
it.

**Why.** Dropping the stale entry would lose a change OneDrive made just after the commit, which
the delta feed never sends twice; applying it would undo the upload.

**Trade-off.** One `GET` for each such entry, and a failed `GET` fails the cycle (limitations log
F112, F113).

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

**Decision.** The daemon asks for `Files.Read User.Read offline_access`: the scope of a read-only
account, which every account is unless it is switched to read-write (below). It asks for it at the
authorization, the code exchange and every refresh alike.

**Why.** Microsoft refuses any write made with that token, so "nothing is written to the cloud" is
enforced by the server, not by the client's discipline. Asking for the read-only scope at every
refresh keeps a read-only account's tokens unable to write even if its grant were ever wider.

**Trade-off.** Switching to read-write needs a sign-in of its own, for `Files.ReadWrite`.

### An access-token export for test runs, in every build

**Decision.** `Dev1.AccessToken()` and `konedrivectl dev export-access-token` hand out an access
token (about an hour of read access), never the refresh token, written atomically to a `0600`
file. It is read-only whatever the account's mode: a read-write account's comes from a refresh that
asks for `Files.Read` only. `--read-write` (`Dev1.ReadWriteAccessToken()`) hands out one that can
write, for the test-account harness, and only for an account the write gate lets through.

**Why.** A test run in the VM needs to speak to Graph without a sign-in of its own, and the refresh
token must never leave the Secret Service. A per-user development install needs it, so it is not
gated behind a build flag. A read-write account's token can change the whole drive, so the export
never grants write access unless asked, and never for an account that is not a test account.

**Trade-off.** Any process of the same user on the session bus can obtain an hour of read access —
no more than it has by opening files in the folder; a Flatpak app is filtered by its bus proxy
(limitations log W11).

### An account is its drive

**Decision.** An account's identity is its Graph drive id, recorded at its first sign-in and never
changed. A sign-in that reaches another drive than the account's own is refused, and so is one that
reaches a drive another account already has. The check and the record are one step under the
configuration's lock, and the check is fail-closed.

**Why.** An account's folder, tree store and rescues belong to one drive: signed in as someone else,
it would reconcile a different drive over them. Two accounts of one drive would download everything
twice, into two folders. The per-cycle check already compared drive ids, so the drive id was the
natural identity. A check that let a sign-in through when Graph did not answer would leave nothing
to compare later sign-ins against.

**Trade-off.** A sign-in is refused when the daemon cannot ask which drive it reached, or cannot
learn the drive of another signed-in account that has none recorded yet (limitations log F42).
Connecting a different Microsoft account means adding a new account.

### Account ids are random; labels are for people

**Decision.** An account is known by 12 random hexadecimal characters, never reused, which name its
D-Bus object and its directories. People see and type a label, 1 to 40 characters with no `/` and
no `@`, which can change at any time and names nothing on disk.

**Why.** What appears in a D-Bus path and in file names must be valid in both and stable across
renames. An email address is neither stable in meaning — the same address can be removed and added
again — nor valid in a D-Bus path. Without `@`, a label is never mistaken for an email where either
can name an account.

**Trade-off.** Rescued files are grouped by account id, not by a name a person recognises
(limitations log F47).

### `config.toml` has one owner, and a single-account file is migrated in place

**Decision.** Every change to `config.toml` goes through one store that re-reads, changes and writes
the file atomically under one lock, and that never overwrites a file it cannot read. At the first
start with multiple accounts, a single-account file is copied to `config.toml.v1` and rewritten as
version 2, its account becoming "Personal"; the tree store and the cached account then move into
the account's directory, idempotently, at each start until done; the wallet item moves at the
account's first token load.

**Why.** With several accounts every change touches the same file, and two independent writers had
already been able to save over each other (limitations log F37, now closed). The version-2 write is
the one commit point, and idempotent moves let the next start finish whatever a crash interrupted.
Moving the wallet item inside a token load that happens anyway adds no unlock prompt.

**Trade-off.** No way back to a single-account version except copying `config.toml.v1` back by hand
(limitations log F41); a tree store that cannot be moved safely is rebuilt with one listing, losing
its activity log and conflicts (F40).

### Each account's token is a wallet item of a new kind

**Decision.** Each account's refresh token is stored under `kind=account-refresh-token` and
`account=<id>`, rather than under the single-account `kind=refresh-token` with an `account`
attribute added.

**Why.** A Secret Service search returns every item whose attributes include the ones asked for: a
search for the single-account item would find every account's as well, and telling them apart would
mean reading attributes that a locked wallet may not show.

**Trade-off.** While a migrated account's old item has not moved yet, both kinds are looked for and
both are deleted at sign-out.

### The mode, and the write gate

**Decision.** Every account has a mode, `read-only` or `read-write`, stored in `config.toml`, a new
account read-only. `Account1.Mode` publishes the mode it *runs* in: read-write only while
`config.toml` says so, the write gate lets its drive through, and its last token was granted
`Files.ReadWrite`. `Account1.SetMode` switches it, and writes read-write only once a sign-in has
granted that. While uploads are being developed, the gate — `write_test_drive_ids` in
`config.toml`, empty by default — refuses read-write for every account but the test account's. It
refuses `SetMode("read-write")` and the export of a token that can write, and it decides the mode
an account runs in, so a hand edit of `mode` cannot get past it; the file is read again each time,
so taking a drive off the list counts at once. Nothing in konedrive writes the list. The OAuth scope
follows the mode (above), and the folder follows it too: read-write lifts the lock. The window's
switch is "Upload changes made on this computer" on the Account page.

**Why.** Microsoft, not the client, then decides whether a write can happen: a token can write only
after a sign-in that asked for it. Publishing the mode run in, not the one asked for, keeps
"read-write" from meaning anything a token cannot do. The gate makes it impossible for an agent, a
script or a stray click to make the user's real account writable before uploads have been run
against a test account and released; the release removes it in a change of its own.

**Trade-off.** Nobody but a developer with a test account can upload in this version. A read-write
account that loses its grant turns read-only until it signs in for it again, and the first switch
to read-write takes a sign-in of its own (limitations log F60, F61).

### Removing an account keeps the user's files

**Decision.** Removing an account forgets its folder as a Forget does, signs it out, and deletes its
refresh token, its cached name and quota and its tree store. The folder's files and the rescued
files stay.

**Why.** Nothing konedrive does deletes a user's file, and the Forget path is already crash-safe and
goes through the helper.

**Trade-off.** Files never downloaded stay as empty placeholders, which read as zeros; and, like a
Forget, removing an account with an intercepted folder is refused while the helper is not connected
(limitations log F47).

### Personal accounts only, for now

**Decision.** Every account signs in through the `consumers` authority.

**Why.** Work or school accounts need another authority, an app registration that allows them,
often an administrator's consent, and testing against SharePoint-backed drives: a phase of its own.
Nothing in the design of accounts stands in its way; the authority would become a per-account field
beside the mode.

**Trade-off.** No Microsoft 365 or OneDrive for Business accounts (limitations log F49).

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

### An account switcher heads the sidebar

**Decision.** The window shows one account at a time. A switcher at the top of the sidebar chooses
it; the five account pages below show that account, and Settings, which holds only what is the
whole app's, stays one page. The switcher is there with a single account too, and a warning sign on
it says when another account needs attention. The folder moved from Settings to the Account page.

**Why.** The pages stay where they were and show the chosen account, instead of a list of accounts
standing in front of each of them; KDE's multi-account applications, such as NeoChat and Tokodon,
put the account selector in the sidebar or the drawer in the same way. With one account, the
switcher names it and gives "Add Account…" a home.

**Trade-off.** Two accounts cannot be seen side by side in the window; the tray's tooltip is the
overview (limitations log A13).

### The tray shows the worst account; notifications and Places name the account

**Decision.** The tray icon shows the worst state across the accounts, and its tooltip has a line
per account. Notifications and download progress name the account once there are several. Every
Places entry is named `OneDrive — <label>`, with a single account too.

**Why.** One icon cannot show several states, and trouble must not hide behind an account that is
fine. Naming the account only once there are several leaves a single account's notifications as
they were. Naming the Places entry after the account from the start means that a second account
renames nothing.

**Trade-off.** In the tooltip, an account that needs attention shows why in place of its "checked
N ago" (limitations log A17, A18, A19).

### The command line names the account, and a path decides it

**Decision.** `konedrivectl` takes the account from `--account` (id, label or email), else from
`KONEDRIVE_ACCOUNT`, else the only account there is; with several and none named, it stops with
exit status 2 and lists them. The commands that take a path find the account from the path, and
refuse `--account`, as do `account …` and `set-client-id`. `login` with no account at all adds one
called "Personal" first.

**Why.** Guessing among several accounts would act on the wrong one sooner or later, and a
refusal that lists the labels costs one retry. A path already says whose folder it is in; an
`--account` that disagreed with it would have to be either ignored or obeyed wrongly, so it is
refused. Adding "Personal" at `login` keeps the single-account setup — `set-client-id`, `login`,
`sync register` — working word for word.

**Trade-off.** An email names an account only once the account has signed in, and
`KONEDRIVE_ACCOUNT` is ignored by the commands that name no chosen account (limitations log F51).

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

### Dolphin: a menu action starts the daemon; the plugins never open a file

**Decision.** The context-menu action starts a stopped daemon through D-Bus activation. Neither
plugin opens a file in the sync folder; emblems come from `lgetxattr`. At most 1000 paths wait at
once per window, one `Pin`/`Unpin`/`FreeUp` call per action chosen (not one call per path).

**Why.** A user clicking "Always keep on this device" expects a download, as with any KDE
service. An open inside
Dolphin would download whatever is shown. The cap bounds Dolphin's memory and its D-Bus reply budget
against a daemon that never answers.

**Trade-off.** A selection of more than 1000 paths still takes several clicks, and the dedupe is
by path alone, so choosing one action right after another on an overlapping selection, before the
first answers, sends only the first (limitations log K6).

### Pinning: unchecking only unpins; "Free up space" works on any folder

**Decision (D-A).** Unchecking "Always keep on this device" only removes the pin -- files already
downloaded stay downloaded, exactly as on Windows. It calls the daemon's `Unpin`, never `FreeUp`;
only "Free up space" frees anything. **Decision (D-B).** "Free up space" is offered for any folder
inside the root, not only a pinned or already-downloaded one, again as on Windows -- `FreeUp`
already recurses into a folder regardless.

**Why.** Matches what a Windows user already expects of "Always keep on this device", and keeps
the two actions' jobs separate: one manages the pin, the other frees space.

**Trade-off.** Pin and Unpin are refused as one call for the whole selection if any path in it is
pinned only by an ancestor, so the checkbox is disabled in that case rather than partly acting on a
selection (`dolphin/src/filestate.cpp`, `menuState`).

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

### Writes against a real account: only a test account, through a guard at the wire

**Decision.** Uploads are tested against mock servers everywhere, and against a real account only
in one harness (`tests/write-account/`), only for a separate test account. It refuses to start
unless the drive both tokens reach is the one named and is on the write gate's list, and looks like
a test account (under 1 GiB used, under 1000 items). Every request, konedrive's own client's
included, goes through a proxy whose guard admits a write only inside the run's own folder and
within fixed caps, and stops the run at its first refusal ([writes.md](writes.md) §12.1).

**Why.** Some of what uploads rely on is what the service does, which no mock can say. A guard at
the wire sees every request whatever code made it, so a bug in a check cannot write elsewhere, and
the look of the drive refuses a real account even if its id were listed by mistake.

**Trade-off.** It runs by hand, with two exported tokens, and has not run yet: until it does, the
behaviour it checks is assumed (limitations log F130, F131).

# Sync

How the folder comes to show the user's OneDrive and stays in step with it: listing the drive,
following its changes, the tree store, reconciling the folder, replacing files that changed in the
cloud, rescuing local changes, the read-only lock, and the account. How a placeholder is filled
when opened is in [hydration.md](hydration.md).

## 1. Scope

This document describes a **read-only** account's sync, which every account's is unless it is
switched to read-write. It reads from OneDrive and never writes to it:

- the OAuth scope is `Files.Read User.Read offline_access`, so Microsoft refuses any write made
  with the token — "nothing is written to the cloud" does not rest on the client's discipline;
- the Graph client issues `GET` requests only;
- the folder is read-only, so nothing local can diverge (§11), and a local change forced past the
  lock is rescued, never overwritten (§10).

A **read-write** account's folder is unlocked, and what is changed in it goes up: the watcher, the
examination, the outbox, conflicts on write, and what its cycle does differently are in
[writes.md](writes.md). In this version only a test account can be read-write
([writes.md](writes.md) §2.3). Pinning is described in [pinning.md](pinning.md).

Everything here is per account: each account has its own folder, and its folder its own tree
store, poller, activity log and conflicts. How several accounts share one daemon — and one link to
the helper — is in [accounts.md](accounts.md).

## 2. Components

All of this runs in the daemon, as the user, once for each account.

| Component | Where | Responsibility |
|---|---|---|
| Graph client | `drive/` | `/me/drive/root/delta` with its pages, item metadata, content, thumbnails; `Retry-After`; for a read-write account, the guarded writes ([writes.md](writes.md) §6) |
| Tree store | `tree.rs` | SQLite: one row per file and folder, the delta link, the activity log, the conflicts (§5) |
| Listing and poller | `sync/listing.rs` | One folder's sync cycles: when to run, what to fetch, which scope to reconcile, when to commit (§4, §6) |
| Materializer | `sync/materialize.rs`, `sync/disk.rs` | Makes the folder match a tree (§7), rescues local work (§10), keeps the lock (§11) |
| Graph content source | `sync/graph_source.rs` | Serves fills from Graph ([hydration.md](hydration.md) §7) |
| Replacements | `sync/materialize.rs` | Brings a downloaded file up to a new version (§9) |
| Thumbnail filler | `sync/thumbs.rs` | Puts OneDrive's thumbnails into KDE's cache ([desktop.md](desktop.md) §8) |
| Baloo exclusion | `sync/baloo.rs` | Keeps the file indexer out of the folder ([desktop.md](desktop.md) §9) |
| Account | `account.rs`, `oauth.rs`, `token.rs`, `secret.rs` | Sign-in, tokens, the account's name and quota (§12) |
| Configuration | `config.rs` | `config.toml`: the client id, and each account with its folder ([accounts.md](accounts.md) §4.1) |
| Watcher, examination, outbox worker | `sync/watcher/`, `sync/local/`, `sync/upload/` | a read-write folder's local changes, found and sent ([writes.md](writes.md) §3–§8) |
| Read-write reconcile | `sync/listing/rw.rs`, `sync/materialize/rw.rs` | a read-write folder's cycle, keeping local work ([writes.md](writes.md) §9) |

## 3. Which folders sync

An account has at most one registered folder. It has a **source**, recorded in `config.toml` as
`source` in the account's `[accounts.root]`:

- **`onedrive`** — made by `RegisterRoot` while signed in and with the helper connected. It shows
  the drive and is kept in step with it.
- **`local`** — made by `RegisterRootWithoutInterception`, always, and by `RegisterRoot` while
  signed out. It is filled from a local directory with `PopulateFromDirectory` and never talks to
  Graph ([hydration.md](hydration.md) §14.3).

The source is decided at registration and kept: a local folder stays local after a sign-in, and a
OneDrive folder stays OneDrive after a sign-out (it then reports that it is signed out). Changing it
takes a Forget and a new registration (limitations log F20). `Sync1.RootSource` publishes it, so
clients need not infer it.

**No OneDrive sync without the helper.** A OneDrive folder's cycle runs only while the folder is
intercepted and the daemon holds a link to the helper; otherwise it is refused `NoHelper` before
Graph is asked, and nothing is placed or updated. Placing placeholders nobody intercepts would hand
zeros to whatever opens them. While a folder waits for the helper, `RootState` is `error` and
`LastError` begins with what to do: install the helper, start it, or look at why it failed
([desktop.md](desktop.md) §2.5).

## 4. Listing and changes

### 4.1 The delta feed

The daemon reads `GET /me/drive/root/delta` and follows `@odata.nextLink` pages until Graph hands
out an `@odata.deltaLink`. Without a stored link that is a full listing of the drive; with one it
is the changes since. The final link is stored in the tree store and advanced only when the folder
matches what it describes (§6). Graph can only list a drive from its root, so there is no
folder-scoped listing; one delta link per sync root is all a personal drive (and OneDrive for
Business) supports.

Each `DriveItem` is classified: a folder or a file to place, a deletion, or an item to skip, with a
reason (§7.5). Graph does not promise a parent before its children, so a tree is assembled by
`parentReference.id` before it is placed.

### 4.2 When a cycle runs

The poller runs a cycle at once when the folder's sync starts, then every **60 s**; at once on
`Refresh()`; and at once when NetworkManager reports global connectivity again (limitations log
F21). After a failed cycle it retries after 5, 15 and 30 s, then at the ordinary interval. A cycle
is also nudged when the account becomes signed in. Cycles of one folder never overlap. While the
account is paused (`Sync1.Pause`) no cycle runs; the pause is kept in the tree store and outlasts a
restart ([writes.md](writes.md) §11).

### 4.3 Throttling and errors

`429` and `503` are retried after their `Retry-After` (10 s when none is given, at most 300 s, up to
5 attempts). A `401` refreshes the access token once. A cycle that fails for any other reason —
no network, Graph unreachable, the store unusable — is logged, published in `LastError`, and retried
on the schedule above; downloads on open keep working.

### 4.4 An expired delta link

When Graph answers `410 Gone` (`resyncRequired`), the daemon lists the drive afresh and reconciles
Full: anything not in the new listing is a deletion. Without this, files deleted in the cloud while
the machine was off would stay forever. A read-write folder tells Graph's two variants apart, and
keeps downloaded files the new listing left out, to upload them again ([writes.md](writes.md) §9).

## 5. The tree store

### 5.1 Schema

```sql
CREATE TABLE items (                -- the tree the folder was last made to match
  id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL,
  kind TEXT NOT NULL,               -- 'file' | 'folder'
  size INTEGER NOT NULL DEFAULT 0,
  mtime INTEGER NOT NULL DEFAULT 0, -- fileSystemInfo.lastModifiedDateTime, Unix seconds
  etag TEXT, ctag TEXT,
  quickxor TEXT,                    -- hashes.quickXorHash, base64
  mime TEXT,                        -- thumbnails are fetched for images and videos only
  placement TEXT NOT NULL,          -- 'placed' | 'skipped:<reason>'
  thumb_key TEXT,                   -- the cTag, path and time a cached thumbnail was made for
  local_handle BLOB,                -- the file handle of the inode the item was placed as
  local_seq INTEGER NOT NULL DEFAULT 0);  -- the upload that last wrote the row
CREATE INDEX items_parent ON items(parent_id);
CREATE TABLE staging (…same columns…);   -- the tree a cycle is building
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
  -- schema_version, drive_id, root_item_id, delta_link, listing_next, last_checked, …
CREATE TABLE activity (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL,
                       path TEXT NOT NULL, detail TEXT NOT NULL);   -- the newest 200 events
CREATE TABLE conflicts (rescued TEXT PRIMARY KEY, at INTEGER NOT NULL, original TEXT NOT NULL,
                        kind TEXT NOT NULL DEFAULT 'rescued');       -- 'rescued' | 'copy'
-- and the outbox's tables, used by a read-write folder: writes.md §5.1
```

A local path is the chain of names from the root; it is computed, never stored, so renaming a
folder changes one row. The schema is created in one transaction. The file handle is recorded for
what the daemon places, so that a read-write folder can tell an item's own inode from a copy of it
([writes.md](writes.md) §4.1).

### 5.2 Where it lives

`$XDG_STATE_HOME/konedrive/accounts/<account id>/tree.sqlite`, one for each account, in WAL mode
with `synchronous=NORMAL`. Store calls run on blocking threads. A single-account installation's
store, `$XDG_STATE_HOME/konedrive/tree.sqlite`, is moved there once ([accounts.md](accounts.md)
§8.3).

### 5.3 A map, and rebuildable

The extended attributes on the files are the truth about each local file; the store is a map of
the drive. If it is missing, unreadable, or of another schema version (currently 3), it is rebuilt:
a full listing fills it and the folder is reconciled Full against it, finding what is already there
by item id. Losing it costs one listing, never data — though the activity log and the conflict list
go with it (limitations log F24). For a read-write folder it also costs the outbox: the local
changes are found again on disk, and uploads start again from zero, but a delete not sent yet is
forgotten, and that item comes back ([writes.md](writes.md) §5.1). A Forget, and removing the
account, are refused while it holds changes not sent (`PendingUploads`); a forced switch to
read-only drops them first ([writes.md](writes.md) §2.2).

## 6. A sync cycle

### 6.1 Stage, reconcile, swap

1. **Check the account** (§12.3).
2. **Fetch.** Changes since the stored delta link, or a full listing when there is none. The Graph
   phase writes only `staging` and `meta`.
3. **Stage.** The changes are applied to a copy of `items` in `staging`.
4. **Reconcile** the folder against `staging` (§7).
5. **Swap.** In one transaction, `staging` replaces `items` and the new delta link is stored.

A crash before the swap leaves `items` and the delta link as they were; the next cycle asks for the
same changes and reconciles again, and the reconcile is idempotent: an item already where the tree
wants it is left alone. After the swap the cycle publishes the counts, records `last_checked`,
drops conflicts whose rescued file is gone, and starts the replacements the reconcile queued (§9).

A read-write folder's cycle holds the tree lock its uploads commit under from staging to the swap,
reads again what an upload committed while the delta was fetched, and keeps OneDrive's change to an
item with local work waiting until the disk takes it ([writes.md](writes.md) §9).

### 6.2 Full and Changed

A reconcile has one of two scopes:

- **Full** walks the whole folder and makes all of it match the tree.
- **Changed** looks only at the items the delta named, and turns into Full the moment the folder
  disagrees with the stored tree (an item of ours in the way that the delta did not mention).

Full runs at every daemon start, after a first or `410` listing, after a failed cycle, when a delta
has more than **5000** changes (above that one scan is assumed cheaper than item by item), and after
a cycle that left files for later (`deferred`: files being filled or freed up at that moment, which a
Changed scope would never revisit). A cycle with no changes stores the new link and reconciles
nothing. A *failed replacement* does not force Full; it is retried on its own after every cycle (§9).

### 6.3 Locking and stopping

A cycle asks Graph without the lifecycle lock that guards the folder's registration, and takes it
(as a reader) only from reconcile to swap. So a helper reconnect, which takes the lock to re-register
and recover, never waits behind a listing, and a Forget racing a cycle's staging writes is safe:
stopping the poller cancels the Graph request first.

Stopping a folder's sync never waits for Graph or for the lifecycle lock. It waits only for a
reconcile already changing the folder, which checks for the stop between steps (limitations log
F15).

## 7. Reconcile

### 7.1 What a change in the cloud does locally

Because the folder is read-only, a local file cannot normally have changed, so every change from
the cloud can be applied. The one guard — "is this still what we put here?" — catches the case where
the lock was bypassed (§10). In a read-write folder local changes are the normal case, and the
reconcile leaves alone whatever has one waiting to upload ([writes.md](writes.md) §9).

| In the cloud | Locally |
|---|---|
| New folder | a directory with its item id, marked by the helper before anything is placed in it (§7.3) |
| New file | a placeholder with its true size and time, item id, cTag, `online-only` ([hydration.md](hydration.md) §2.4) |
| Rename or move, file or folder | one `rename(2)`; attributes travel with the inode, and a folder's contents move with it |
| Delete | the file or folder is removed; a program holding a file open keeps reading its copy. Anything holding local work is rescued first |
| New content, file `online-only` | the placeholder takes the new size, time and cTag, in place (a stale checkpoint goes with it) |
| New content, file downloaded | the new version is downloaded in the background and swapped in atomically (§9) |
| New content, file changed locally | the local file is rescued and a placeholder of the new version takes its place (§10) |

Every change goes through a directory descriptor opened beneath the root (`openat2` with
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`), never through a path string a rename could redirect. A
file being filled or freed up at that moment (its per-inode lock is held, or its state is
`hydrating` or `dehydrating`) is left for the next cycle and counted `deferred`.

### 7.2 The holding directory

Name collisions — two items swapping names, a cycle of three renames, a file replaced by a folder
of the same name — are resolved with a **holding directory**, `<root>/.konedrive-holding`, in three
phases:

1. Every item of ours that is not where the new tree wants it is moved to
   `.konedrive-holding/<item id>`, deepest items first, so that a move never changes the path of
   something still to be moved.
2. The new tree is placed top down: each item is found in place, taken back from the holding
   directory by its id, or created.
3. Whatever is left in the holding directory once the whole tree is placed is genuinely gone from
   the drive, and is removed — after rescuing any of it that holds local work.

Everything in the holding directory is keyed by item id, so a crash at any point is resolved the
same way: a Changed cycle that finds a holding directory left by an earlier run does not touch it,
it forces a Full reconcile, which resolves the whole tree from the ids. The holding directory is
marked (M1) the first time anything is sent to it in a run.

### 7.3 New folders

A new folder is created as `.konedrive-new-<item id>`, labelled with its item id, marked by the
helper, and only then renamed to its real name. So nothing can be placed in an unmarked directory
(M1), and a crash leaves a temporary directory the next attempt recognises and clears (after
rescuing anything someone put in it). A folder whose marking failed waits, unmarked, in the holding
directory, and is marked before its real name shows it.

### 7.4 A placeholder's content

A placeholder's content is judged by its **cTag and size**, never by its time. Every write of a
fill moves the file's time to now, and a fill that stopped part-way leaves a checkpoint worth
keeping ([hydration.md](hydration.md) §7.4); taking the time for a new version would punch that
partial download away at the Full reconcile every restart begins with. So a placeholder of the
tree's own version whose time differs only gets the cloud's time back, under its per-inode lock.
A checkpoint goes only with a new cTag.

### 7.5 What is skipped, visibly

Nothing is skipped silently. Each of these is stored with `placement = 'skipped:<reason>'` and
listed by `Skipped()`, `konedrivectl sync skipped` and the window's "Not in the Folder" page:

- **A name longer than 255 bytes.** Linux allows 255 bytes per name, OneDrive 255 characters, and
  a Cyrillic letter takes two bytes: 127 Cyrillic characters fit, 128 fail with `ENAMETOOLONG`.
  Such an item is never created under a truncated name.
- **The Personal Vault** (`specialFolder.name == "vault"`), which needs a separate unlock. It is
  recognised by that facet, not by its name, which any folder could have.
- **A shared folder added with "Add to my OneDrive"** (`remoteItem`), which lives on another drive.
- **A OneNote notebook** (any item with a `package` facet), which is not a file.
- **A reserved name**: anything named with the `.konedrive-` prefix would collide with the daemon's
  own working names (the holding directory, new folders, rescues).
- **Unsupported**: an item that is neither a file nor a folder, or whose id or name cannot be a
  name in a directory (empty, `.`, `..`, containing `/` or NUL).

Only the top of a skipped subtree is listed. `ItemsListed` counts every item in the drive, while
`ItemsPlaced` and `SkippedCount` count only what the tree reaches from the root through placed
folders, so the contents of a skipped folder make `ItemsPlaced + SkippedCount` smaller than
`ItemsListed` by design (limitations log F32). `LastChecked` moving is the signal that a cycle has
finished.

## 8. The first listing, page by page

A full listing of a large drive takes minutes. Staged whole, the folder would stay empty until one
reconcile at the end. So a folder's **first** listing — no delta link, an empty `items` table, and
nothing in the folder carrying an item id — is placed as it arrives:

- each delta page is reconciled as soon as it comes, by the same rules as every other reconcile
  (M1, the lock, skips, reserved names, rescues);
- after each page, one transaction commits the page's items into `items` and stores the page's
  `@odata.nextLink` as `listing_next`;
- an item whose parent has not arrived yet is committed as a row with no place, and placed when its
  folder comes — across a stop too;
- `ItemsListed` and `ItemsPlaced` rise page by page, and the lifecycle lock is taken per page, never
  across a Graph request.

A listing stopped part-way (a crash, a stop, a failure) resumes from `listing_next` and asks for no
committed page again; the first page each cycle places is reconciled Full, finding by id whatever
the interrupted page had placed. If Graph refuses the stored resume link (`410`, `404` or another
client error), the listing starts again from the beginning the ordinary way: what is placed stays
and is found by its id, and the rest appears at the end. A next-page link handed out in the same
cycle and then refused only fails that cycle. The account check (§12.3) still runs before the first
page is placed.

Every later cycle, and a listing into a folder that already shows the drive (a lost store, a Forget
and a new registration), keeps the staged path: part-way through a listing, an item of ours not
listed yet cannot be told from one that is gone. The costs of the page-by-page path — among them
that a folder deleted on a later page takes with it children a still-later page moves out, so a
download of such a child is repeated — are listed in limitations log F35.

## 9. Replacing a downloaded file that changed in the cloud

A new version is never written over the old one in place: a program reading it would see a mix.
Instead:

1. check that the disk can hold both versions at once (the new size plus a 64 MiB margin);
2. download into an `O_TMPFILE` in the same directory, through the Graph source, with the same
   verification as a fill ([hydration.md](hydration.md) §7.2) — without checkpoints, since an
   `O_TMPFILE` does not survive a crash;
3. give it the file's attributes (item id, cTag, stamp, `state=hydrated`) and mode;
4. under the old file's per-inode lock, check that the old file is still the one at its name and
   still unchanged, link the new one in as `.konedrive-new-<item id>`, and rename it over the old
   name.

A program already reading the old version keeps it to the end; the next open gets the new one. The
old file is not held open during the download, so freeing it up meanwhile still works; step 4 then
finds it changed and gives up. At most 2 replacements download at once, in the background, reported in `Transfers` like any download.
In a read-write folder, where a program may be writing the old file, step 4 first takes a write
lease on it, granted only while nobody has it open; a refusal leaves the replacement for a later
cycle (limitations log F110).
A replacement that finds nothing left to do — the file moved, was freed up, changed locally, or is
already this version — ends quietly, and the next cycle looks again. One that fails (not enough
disk space to hold both versions, a network error) leaves the old version in place, is recorded as
`update-failed` once per version and reason, says why in `LastError`, and is retried after every
cycle without forcing a Full reconcile — a disk too full to hold both would otherwise make every
cycle scan the whole folder (limitations log F13). A leftover `.konedrive-new-<id>` link from a
crash is recognised by its name and its id, and cleared by the next swap of the same file or by the
next Full reconcile (which rescues it instead if it holds local work).

## 10. Rescues and conflicts

### 10.1 What holds local work

Before the reconcile removes, replaces or overwrites a file, it asks whether doing so would lose
something only this machine has:

- a `hydrated` file whose stamp no longer matches its size and time (it was changed locally);
- a file of ours in a state nobody can vouch for (no state, or an unreadable one) that holds data;
- anything without an item id that stands where the tree wants one of its items — a file the user
  created, or one that got into the folder past the lock.

Such a file is **rescued**; anything else of ours is simply replaced or removed. An `online-only`
placeholder, or one mid-fill or mid-free-up, holds nothing only this machine has, and is never
rescued.

A read-write folder moves nothing it could upload out of the folder: where this section moves a
file out of the folder, the reconcile renames it to a conflict copy beside the original and
uploads it, and a folder OneDrive removed that holds local work stays where it is
([writes.md](writes.md) §7, §9). Only konedrive's own temporary names (a leftover
`.konedrive-new-<id>` that holds local work) are still rescued as §10.2 says.

### 10.2 How a rescue works

A rescue is **exactly one `renameat2(…, RENAME_NOREPLACE)`** into
`$XDG_DATA_HOME/konedrive/rescued/<account id>/<time>/<path in the folder>`: never a copy, never a
delete. (A single-account installation rescued into `rescued/<time>/`; those files stay there.) A
rename cannot lose bytes; a copy followed by a delete could delete something other than what was
copied, would leave the tree unlocked for the duration, and would need its own crash protocol. A
name already taken in the rescue directory sends the file on to `<name>.1`, `<name>.2`, …

A rename works only within one filesystem, so the rescue directory is the XDG one when it is on the
folder's device, and otherwise `.konedrive-rescued-<folder name>` beside the folder. When neither is
possible (the folder is itself a mount point, or its parent is not writable) the rescue fails with
`EXDEV`, the file is left exactly where it was, the cycle fails and says why, and nothing is lost
(limitations log F16).

Once out of the folder, the rescued file becomes the user's own: konedrive's attributes are removed
and it gets ordinary modes (`0644`, directories `0755`). A rescued directory keeps the user's files
inside it, while placeholders of ours inside it that hold no content are removed rather than left
as files of zeros — the cloud still has them.

### 10.3 Conflicts

Each rescue is recorded as a **conflict** — time, original path, rescued path — in the tree store,
and announced as a `conflict` activity event. `Conflicts()` lists them, `DismissConflict()` takes
one off the list without touching the file, and a conflict whose rescued file no longer exists
drops off by itself. `ConflictCount` feeds the tray's "needs attention" state. A conflict is
recorded when its reconcile commits; a reconcile that fails with an error records none of the
rescues it already made — those files are in the rescue directory and the daemon's log (limitations
log F28).

## 11. The read-only lock

A read-only account's OneDrive folder is locked: files `r--r--r--` (`0444`), directories
`r-xr-xr-x` (`0555`). Directories are locked too, because editors save by writing a new file and
renaming it over the old one, which needs write permission on the directory, not the file. The
window and the README say the folder is read-only.

The daemon lifts write permission only for the moment of its own operation. File content needs no
window: it is written through the event descriptor the kernel opened for the helper, and the owner's
`pwrite`, `ftruncate`, `fallocate` and `futimens` work on a `0444` file through a descriptor already
open. But a `user.*` attribute write needs write permission on the inode even for its owner
(measured on Btrfs: `setfattr` fails `EACCES` on `0444` and succeeds after `chmod 0644`), and
creating an entry in a `0555` directory fails `EACCES` too. So `konedrive-fs` lifts the owner's
write bit around each attribute write and puts it back; a file that must be opened writable
(dehydration, recovery, a replacement) is opened read-only and reopened through `/proc/self/fd/<n>`
— the same inode, whatever its name leads to by then — with the bit lifted for that one open; and
each directory gets the same window around each change.

**What data safety rests on.** A program that opens a file for writing inside such a window keeps a
writable descriptor (limitations log W2). The lock is therefore the rule a person sees, **not** the
guarantee. The guarantee is the check at the point of danger: before any change from the cloud is
applied to a file, the daemon verifies the file is still the one it placed or downloaded (§10.1),
and rescues it if not. No local byte is discarded.

A Forget takes the lock off the whole folder (files `0644`, directories `0755`); what the unlock
walk cannot reach is logged (limitations log F19). A local folder is never locked. Neither is a
read-write account's folder: the switch to read-write takes the lock off with the same walk, once
the watcher has marked every directory, and the switch back puts it on again (limitations log F62;
[writes.md](writes.md) §2.2).

## 12. The account

Each account signs in, holds its tokens and checks its drive on its own, as described here; what
keeps several accounts apart is in [accounts.md](accounts.md) §6.

### 12.1 Sign-in

Sign-in is OAuth 2.0 authorization code with PKCE in the **system browser**, redirected to a
loopback listener (`http://localhost:<ephemeral port>`, on `127.0.0.1` and `::1`), as recommended
for desktop applications (RFC 8252). Password and second factor stay in the browser; there is no
embedded web view. The authority is `login.microsoftonline.com/consumers`: personal accounts only
(limitations log F49). The listener accepts only `GET /` carrying `code` and the expected `state`,
answers anything else with 404, is single use, and closes after five minutes. Each user registers
their own Entra application and gives its client id to the daemon (`Accounts1.SetClientId`); every
account signs in with it, and the README has the steps.

The scope is the account's mode's: `Files.Read User.Read offline_access` for a read-only account,
`Files.ReadWrite User.Read offline_access` for a read-write one, asked for at the authorization, the
code exchange and every refresh alike ([accounts.md](accounts.md) §10). A refresh never asks for
more than the last token was granted: a read-write account whose token lost `Files.ReadWrite` asks
for `Files.Read`, and runs read-only until it signs in with that permission again.

Before the refresh token is stored, the daemon asks `GET /me/drive` with the new access token, and
refuses the sign-in if that drive is not this account's, or is already another account's
([accounts.md](accounts.md) §6.2). A sign-in that cannot make that check is refused too.

### 12.2 Tokens

- **The refresh token** is stored only in the Secret Service (KWallet), one item per account, under
  attributes `application=konedrive`, `kind=account-refresh-token`, `account=<account id>`, and
  labelled `KOneDrive: <email>` ([accounts.md](accounts.md) §4.3). There is no plaintext
  fallback: without a Secret Service, sign-in fails with a clear error. It never crosses D-Bus and
  is never logged. The single-account item (`kind=refresh-token`) is moved into the migrated
  account's own the first time its token is loaded ([accounts.md](accounts.md) §8.4).
- **The access token** lives in the daemon's memory only, one per account. It is refreshed when less
  than 5 minutes remain, by one refresh shared by all callers; a `401` invalidates it once.
  `invalid_grant` (consent revoked, session expired) deletes the account's refresh token and signs
  it out: `LastError` says to sign in again, a download on open in its folder fails `EIO`, and the
  folder reports that it is signed out.
- **For test runs only**, each account's `org.konedrive.Dev1.AccessToken()` hands out an access
  token of that account — about an hour of `Files.Read`, whatever its mode: a read-write account's
  comes from a refresh that asks for `Files.Read` only — never the refresh token;
  `konedrivectl dev export-access-token` writes the chosen account's (`--account`) atomically to a
  `0600` file. Any process of the same user on the session bus can obtain that hour of read access, which
  is no more than it has by opening files in the folder (limitations log W11). With `--read-write`
  (`Dev1.ReadWriteAccessToken()`) it hands out a token that can write, for the test-account harness
  only, and only for a read-write account the write gate lets through ([writes.md](writes.md)
  §12.1).

At startup, a refresh token found by attribute search (which does not unlock the wallet) means
signed in; the cached name and quota are shown at once and refreshed in the background.

### 12.3 The same account

Every cycle begins with `GET /me/drive` and compares the drive id with the one the folder was built
from, kept both in the tree store and in `config.toml` — as the account's `drive_id`, which is also
its identity ([accounts.md](accounts.md) §2) — so that a store rebuilt empty still has something to
compare against. A mismatch is a blocking error, never a re-listing of another account's files over
this folder. The check costs one request per cycle; a sign-out followed by a different sign-in
between two polls would defeat any scheme that only checked at sign-in. The sign-in itself is also
checked now (§12.1), and a OneDrive folder carries its drive, so that no other account can register
it ([accounts.md](accounts.md) §6.3).

### 12.4 What a signed-out folder does

A OneDrive folder whose account is signed out keeps its files. Its cycles fail at the account check
without changing anything, and `RootState` reads `error` with "signed out: sign in again to keep
this folder in step with OneDrive". Downloaded files keep working; placeholders fail `EIO` on open.
Signing in again nudges a cycle at once, which brings the folder up to date.

## 13. Crash safety of the cycle, in one place

| Interrupted during | What the next run finds | What it does |
|---|---|---|
| Fetching or staging | the old `items` and delta link | asks for the same changes again |
| Reconcile | a folder part-way between two trees, perhaps a holding directory | a Full reconcile at the next start places everything by item id |
| The swap | either the old tree and link, or the new ones — one transaction | continues from whichever it is |
| A page of the first listing | `items` and `listing_next` up to the last committed page | resumes from `listing_next`, reconciling the first page Full |
| Creating a folder | a `.konedrive-new-<id>` directory | clears it, rescuing anything in it |
| A replacement | at most a leftover `.konedrive-new-<id>` link; the old version in place | clears the link; the next cycle queues the replacement again |
| A rescue | the file in the folder or in the rescue directory — one rename | nothing to finish |
| A fill or free-up | a file in `hydrating` or `dehydrating` | recovery ([hydration.md](hydration.md) §9) |
| An upload, in a read-write folder | the outbox row, perhaps a session | a replay that settles by content hash ([writes.md](writes.md) §10) |

## 14. Known limits

The limitations log has the full list. The ones specific to this document: the whole drive is
listed even when only part of it matters, since Graph lists from the root (limitations log W15);
the activity log keeps 200 events and summarises large changes (F24, F25); a delta is held in memory
whole before it is staged (D11); and the numbers here — 60 s, the retry steps, 5000 changes,
16 MiB checkpoints, 2 replacements — are chosen, not measured (limitations log §5).

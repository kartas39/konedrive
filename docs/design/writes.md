# Uploads: changes made on this computer

How a read-write folder sends what is changed in it to OneDrive: the access mode, what of a
program's change is sent, the requests and their guards, upload sessions, conflicts, what holds
sending back, and what the bus shows of what waits. The model this rests on is
[sync.md](sync.md)'s: the item and its three versions (§2), who writes what (§3), the sync plan
(§6) and a step (§7). How the mount takes a program's change is [mount.md](mount.md) §4. This
document does not repeat them.

## 1. What the user sees

- **A folder is read-only until its account is switched.** "Upload changes made on this
  computer" on the Account page, or `konedrivectl account mode read-write`, signs in again in the
  browser to ask for permission to change files. Until then the folder refuses every change
  (`EROFS`).
- **Anything a program does in a read-write folder counts**: a file saved, a folder made, a
  rename, a move inside the folder, a removal. A file's content goes up once it has not changed
  for 2 seconds and no program holds it open for writing; ten saves of one document while offline
  are one upload of its last content.
- **Dolphin** shows the syncing emblem on what waits or goes up, and the error emblem on what
  needs the user. The window's Status page and Activity page show what waits and what went
  ([desktop.md](desktop.md)).
- **Changed on both sides**, both are kept, as on Windows: OneDrive's version keeps the name, and
  this computer's goes up beside it as `Report-<machine>.docx`, listed under Conflicts.
- **Names OneDrive refuses** (`a:b.txt`, a trailing space, `CON`, …) are not renamed for the user:
  they stay here and are listed under Not Uploaded until the user renames them. Names on the
  ignore list (editors' temporary files) stay here unlisted. Links, special files and the user's
  own extended attributes are refused by the mount (F300).
- **A removal is sent at once**, however much it removes; what was removed is in OneDrive's
  recycle bin, and nothing of it is kept here (F302). The folder has no trash of its own (K34).
- **A move out of the folder** is a copy and a removal, because the kernel refuses a rename from
  one mount to another: the program reads the file (which downloads it) and then removes it, and
  the removal is sent. A move into another account's folder is the same, with an upload there
  (F124).
- **Pause** stops uploads until resumed or for a time; opening a file still downloads it.
- **Back to read-only** never drops anything. With changes still unsent, the account keeps
  sending them and switches by itself when none is left; `--force` switches at once, and what is
  unsent stays here and goes up when the account is read-write again.

Every one of these has a command: `account mode`, `sync upload-queue`, `sync not-uploaded`,
`sync conflicts`, `sync ignore`, `sync pause`, `sync resume`, `sync anyway`
([desktop.md](desktop.md)).

## 2. The access mode

### 2.1 Two facts, two owners

The mode is per account: `mode` in its section of `config.toml`, `read-only` for a new account.
`Account.Mode` is the mode the account *runs* in: read-write only while `config.toml` says so,
the token carries `Files.ReadWrite` and the token was last seen to reach the drive `config.toml`
records ([accounts.md](accounts.md)). A read-only account asks for
`Files.Read User.Read offline_access` at every refresh, so that Microsoft itself refuses its
writes; a read-write one for `Files.ReadWrite User.Read offline_access`. A read-only request
answered with a token that can write is used to read only (F66).

What the mode means for the folder is two facts, each with one owner:

| Fact | Owner | What it does |
|---|---|---|
| Programs may change the folder | `AccessModeSwitch` (`mount/access_mode_switch.rs`) | Read-only: every change is refused `EROFS` and no write bit is shown. Asked when a request is taken and again on the write-operation thread, which is the answer that counts |
| What is unsent is sent | `SendingHolds` (`sync_parts/sending_holds.rs`), the hold `FolderReadOnly` | Read-only: no step towards OneDrive starts, and an upload stops after its fragment |

`sync_tasks/access_mode_change.rs` switches the two together; no task of the folder's sync is
stopped or started for a switch. Beside them, `account_folder/mode.rs` watches the account and
sets the hold `WritesNotAllowed` while a read-write account may not write after all: `config.toml`
cannot be read or records no drive, the token's drive is not seen yet or is another, or the
sign-in grants no writes (`decide_account_holds`).

### 2.2 To read-write

`Account.SetMode("read-write")` is refused `NotSignedIn` for an account that is not signed in.
An account with no drive recorded is asked for it first (`GET /me/drive`), and refused before any
sign-in URL when none can be recorded. The sign-in asks for `Files.ReadWrite` and is pinned to the
account: its password asked again, its email filled in, so that a browser signed in to another
account cannot consent for it. Only a token response that grants `Files.ReadWrite`, for this
account's own drive, writes `mode = "read-write"`; anything else changes nothing but `LastError`
(`account/mode.rs`). The folder then follows: the mount's switch first, then the hold off, which
wakes the plan.

### 2.3 To read-only

`decide_read_only_switch` has two cases:

- **Nothing unsent, or forced**: the switch is made at once.
- **Something unsent, not forced**: the switch is neither made nor refused. The account keeps the
  wish (`read_only_wanted` in `config.toml`, so it outlasts a restart; `Account.ReadOnlyWanted`,
  `UnsentBeforeReadOnly`), stays read-write, and goes on sending. When the plan's task finds
  nothing unsent, the account makes the switch by itself. `SetMode("read-write")` takes the wish
  back; `SetMode("read-only", force)` switches now.

The switch itself, in this order, so that no request that changes OneDrive leaves under a token
that cannot write and no 403 comes from a switch:

1. `mode = "read-only"` written to `config.toml`, and the account's mode worked out again;
2. the hold `FolderReadOnly` set: nothing towards OneDrive starts, and an upload stops after the
   fragment in flight with its session kept;
3. the mount's switch, made on the write-operation thread, so that no write operation is half done
   across it; a file still open for writing has its last close written afterwards;
4. the steps towards OneDrive still running are waited for, at most 30 seconds (the answer says
   when one still ran);
5. only then the token that can write is dropped; the next refresh asks for `Files.Read`, a subset
   of the grant, with no sign-in.

What was unsent stays unsent: its rows keep their versions and their stamp, the queue lists them
as `waiting` because the folder is read-only, and they are sent when the folder is read-write
again. An account that turns read-only for a cause of its own (a sign-out, a narrower grant,
`config.toml` edited) goes the same way. `Folder.Unregister` and `Accounts.Remove` are never
refused for unsent changes; the window and `konedrivectl` read `UploadQueue.UnsentCount` first
and warn that they will stay on this computer only.

## 3. What of a change is sent

A program's change is written into the item's local version by the mount, row first
([mount.md](mount.md) §4). An item whose local version is not its synced one is unsent and has
the stamp `unsent_since`; the plan sends the difference between the two versions, not the
history of changes ([sync.md](sync.md) §2, §6). The rules below decide what of an unsent item is
not sent (`sync_plan/sending_obstacle.rs`, `find_sending_obstacle`), the first that holds:

| Rule | Owner | What follows |
|---|---|---|
| Ignored: an item OneDrive does not have whose name, or a directory's above it, is on the ignore list | `UploadRules` (`mount/upload_rules.rs`) over `local/ignore.rs` | Never sent, listed nowhere |
| A name OneDrive cannot hold | `local/names.rs` (`refused`), given to the plan by `plan_input::find_name_refusal` | Not sent, not altered; blocked with its reason |
| A refusal the step memory keeps (for good, or for lack of space) | the step memory ([sync.md](sync.md) §7) | Not sent again until the user changes the item, or space is found |
| OneDrive is full | `OneDriveSpace` (§4.7) | No content is sent; moves, removals and new folders go on |
| The quiet time: the content changed less than 2 seconds ago | `plan_input::QUIET_TIME` | Sent when it ends, so that a program that saves in several writes is not sent half-way |
| A next try that is not due | the step memory | Sent at that time |

Besides, an item a program holds open for writing waits (`Reason::OpenForWriting`) until its last
close, and a step whose content changed between the plan and the read is not run
(`StepObstacle::ContentChanged`): `ItemContentFiles` (`mount/item_content_files.rs`) opens the
backing file by the path of the row's local version, and opens nothing whose size or time is not
the planned content's.

**The ignore list** is the account's: `ignore` in `config.toml`, `Folder.SetIgnorePatterns`,
`konedrivectl sync ignore`, the Account page. Shell globs match the name, not the path,
case-sensitively; by default editors' swap, lock and backup files, `~$*`, `*.part`,
`*.crdownload`, `*.tmp`, `.goutputstream-*`, `.fuse_hidden*`, `.nfs*`. The list applies only to
items with no binding: what came from OneDrive syncs whatever its name. Each row keeps whether
its own name is ignored (`is_name_ignored`), gone over once when the list changes
(`item_store/ignored_names.rs`).

**Names OneDrive cannot hold**: one of `" * : < > ? \ |`; a leading or trailing space; `.lock`,
`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, `desktop.ini`, `_vti_` anywhere, a `~$`
prefix; a name that is not UTF-8. Only what Microsoft documents is blocked here; a name it refuses
otherwise is answered `400`, kept as a refusal for good with OneDrive's message.

**Names no program may make**, refused `EPERM` by the mount's make and rename:

- the daemon's own names, all beginning `.konedrive-` (the holding directory, a new folder's,
  an entry set aside, a temporary name of an exchange: `folder/disk.rs`, `is_hidden_name`), which
  the mount also hides;
- a file manager's trash, a `.Trash*` directory in the folder's root (`UploadRules::is_trash`):
  a trash inside the folder would be uploaded.

## 4. The requests to OneDrive

The steps towards OneDrive are run by `OneDriveSteps` (`onedrive_steps/mod.rs`), one function per
case of `OneDriveStep`. Every step is conditional, sends what it was planned from, writes what
must outlive a crash before its request, sends with no lock held and commits under the tree lock
([sync.md](sync.md) §7). This section is what is sent and what the answers mean.

### 4.1 What is sent

| Step | Request | Guard |
|---|---|---|
| `MakeDirectory` | `POST /items/{parent}/children` | `conflictBehavior: fail` |
| `SendNewFile`, empty | `PUT /items/{parent}:/{name}:/content?@microsoft.graph.conflictBehavior=fail`, then a `PATCH` of its time | `conflictBehavior=fail` in the URL: a `PUT` replaces by default |
| `SendNewFile` | `POST /items/{parent}:/{name}:/createUploadSession` with `conflictBehavior: fail`, its name and `fileSystemInfo`, then the fragments | `conflictBehavior: fail` |
| `SendContent`, empty | `PUT /items/{id}/content`, then the `PATCH` | `If-Match` |
| `SendContent` | `POST /items/{id}/createUploadSession` with `conflictBehavior: replace` and `fileSystemInfo`, then the fragments | `If-Match` |
| `SendMove` | `PATCH /items/{id}` with only what differs: `name`, `parentReference.id` | `If-Match` |
| `SendFileRemoval` | `DELETE /items/{id}`, into the recycle bin | `If-Match` |
| `SendDirectoryRemoval` | `DELETE /items/{id}`, the directory whole, into the recycle bin | none |
| a session's status, its cancel | `GET`, `DELETE` of the upload URL | — |

A `SendMove` for which nothing differs from where the row says OneDrive has the item (an answer
lost, and the rows merged since) sends nothing: the item is read and committed as it is. Times are
sent in whole seconds. Requests to an upload URL never carry the account's token, and the URL,
a credential for its one file until it expires, is never logged. The requests are those of
`DriveClient` (`crates/konedrive-graph/src/drive/write.rs`, `upload.rs`).

### 4.2 The guards

What a request is guarded with comes from the versions the step was planned from
(`onedrive_steps/confirmed_item.rs`), never from the row as it is when the request leaves.
OneDrive takes either tag in `If-Match`.

- **A move or a removal** (`decide_guard`): the eTag of the OneDrive version planned from; else
  the cTag of the synced content, for an item whose eTag no listing told yet. An item with
  neither is not sent and waits for a listing.
- **A content** (`decide_content_guard`): the cTag of the content OneDrive has as the row knows
  it, else the synced content's, else the guard above. Only a change of the content breaks a
  cTag: a rename or a move made in OneDrive meanwhile does not hold the upload back, and neither
  does the eTag OneDrive raises when a session is opened with the file's time.
- **A new item**: `conflictBehavior: fail`, so a name that is taken is answered `409`.
- **A directory's removal** is unguarded: OneDrive removes it whole, whatever it holds by then. So
  it is sent only while the rows say that everything OneDrive has in it was shown here and
  removed here, none of it is a file whose content OneDrive changed since, no listing is under
  way and no request not yet settled is about any of it; this is asked by the plan, again under
  the tree lock just before the request, and again by the commit
  (`ItemStore::can_directory_removal_be_sent`, [sync.md](sync.md) §6). Otherwise the removal is
  of the folder alone, and a directory that holds in OneDrive what the folder never showed cannot
  be removed here (F299).

A `412` means the item changed in OneDrive: the step reads once whether its own earlier request
went through (§4.5), and otherwise ends `ItemChanged`, which asks for a listing. OneDrive moves an
eTag with no change of content, and a listing names such an item with its new eTag; the next
plan is made with it.

### 4.3 Upload sessions

Every file with content goes through an upload session (`onedrive_steps/content_upload.rs`); a
file up to 10 MiB is a session of one fragment. Fragments are 10 MiB (32 × 320 KiB). The session
request carries the guard and the file's time but no `fileSize`, which a personal drive refuses,
so a full OneDrive shows itself only when a fragment is refused (F39). An empty file, which a
session cannot carry, is one `PUT` and a `PATCH` of its time.

**The order with the step memory**, so that a crash at any point loses nothing and leaves nothing
unknown:

1. for a new file, the opening is kept before the request that opens the session: the place and
   the time (`upload_session_openings`), what tells the empty file the session leaves (§4.4);
2. the session's address, the content it sends and its next offset are kept before the first
   byte (`UploadSession` in the step memory);
3. after each fragment its progress is kept;
4. the commit forgets the session it was told ended.

**Between two fragments**, and before the first, the upload asks whether to stop
(`decide_stop_between_fragments`): while sending is held (§6) it stops as `SendingStopped`, while
OneDrive is full as `NoSpace`; either way the session is kept, and the next try goes on from
where OneDrive says it stands. A file that changed meanwhile ends the session; one a program
opened for writing keeps it.

**Going on with a session** is only for the very content it was opened for, change count
included (`decide_upload_start`); the bytes before the offset are read again, so that the hash
sent is that of the bytes read here. A session of a new file goes on at the place of its empty
file, wherever the file is here by now.

**A fragment refused for now** (`429`, `503`, a dropped connection, a timeout) goes again to the
same session, after `Retry-After` or 10 seconds and a look at where the session stands, five sends
in all (`DriveClient::upload_chunk`); refused still, the step ends with the session kept.

**A session that ended** (its URL answers `404`, `410`, `401` or `403`) is forgotten. When the
content sent is in OneDrive by its hash, the upload went through and is committed; when the
existing item changed, the step ends `ItemChanged`; otherwise `UploadSessionGone`, and the next
try opens a new session. OneDrive refuses the last fragment of a session into an existing item
whose content changed since the session opened, and takes it after a rename or a move; so the
item is not read before the last fragment.

**The answer is checked.** The last fragment's answer carries the item, whose hash must be that
of the bytes read for the fragments. Another hash is never committed (`ContentMismatch`): for a
new file the item made is remembered and removed in OneDrive (`mismatched_item_removals`), for an
existing item the fact that this upload left that content is kept (`own_upload_contents`), so
that a listing that brings it is no change of OneDrive's; the content is then sent again.

**A session given up** — the content changed, a fragment refused for good, the row gone — is
queued for its cancel (`upload_session_cancellations`). `SessionCancellations`
(`onedrive_steps/session_cancellations.rs`) sends the cancels, removes the mismatched items and
settles the request records (§4.5), a minute after a failure, doubling to an hour.

An upload takes an upload slot of the account's transfer pool for its whole length, sized by the
file: a file of 100 MiB and up also counts against the limit of large streams it shares with the
downloads. The plan's task starts a few more uploads than there are slots, which wait at the pool
(`sync_plan_task/step_choice.rs`).

### 4.4 The empty file a session leaves

Until a new file's session completes or is cancelled, OneDrive holds its name with an empty file,
and a second request for that name is answered `409`. Nothing in OneDrive says which device
opened a session, so `NameHolders` (`onedrive_steps/name_holders.rs`) decides from what this
folder kept, the first that holds (`decide_name_holder`):

| The holder | What is done |
|---|---|
| nothing, by now | the request is sent again |
| the empty file of this item's own session | the upload goes on in it |
| the empty file of another item's live session | the step waits (`NameHeldByUploadSession`) |
| the empty file of another item's session that ended: its expiry passed, or it names none and was first seen a day ago | that session is forgotten, and the request sent again |
| the empty file of a session nothing goes on with | the session is cancelled, and the request sent again |
| an empty file no listing told of, made within a window in which this folder was about to open a session there (5 minutes of slack for the clocks) | it is this folder's own: removed, guarded by its eTag, and the request sent again |
| a file no row is bound to that holds the content sent | this step's own earlier upload: adopted |
| an empty file no listing told of, of nobody known | the step waits; it is never removed and never copied |
| anything else | `NameTaken`: another item has the name (§4.6) |

A step sends its request again at most once. An opening is kept for 7 days after its latest try
whose outcome is not known. An empty file made at that name by someone else inside a recorded
window could be taken for this folder's own; that is the cost of never deleting a holder this
folder did not record (F172). What an upload of this folder left in OneDrive — such an empty file,
an item of another content — is not shown in the folder ([sync.md](sync.md) §6).

### 4.5 Lost answers

Every request of one answer — a directory made, a move, a removal, an empty file — is on record
before it leaves (`request_records`), and the record goes with the commit, with a refusal that
is certain, or once its fate was read. An answer that says nothing certain (the network, a
timeout, a `408`, a `5xx` other than `503`, a success that cannot be read) is followed by one read
of OneDrive (`LostAnswers::read_request_outcome`, `onedrive_steps/lost_answers.rs`):

| What OneDrive has | The fate |
|---|---|
| what the request asked: the place sent, the hash sent, the item gone | `WentThrough`: committed as the step would have |
| the item as it was planned from, or nothing at a new item's place | `DidNotGoThrough`: tried again |
| the item neither as planned nor as asked | `ItemChanged`: a listing brings it |
| another item at a new item's place | `NameHeld` |

When that read says nothing certain either, the step ends `Transient` and the record stands:
while it stands, its item and the directory it sends into are busy to the plan, and no removal
of that directory is sent. A record left by a death or a stop is settled the same way by
`SessionCancellations`, once sending is not held, which is after the first listing of the run.
An upload through a session writes no record: its opening and its session are its record, and an
answer lost to the opening or to a fragment is `Transient` at once.

A step whose commit finds the rows changed after it made a directory or a file
(`onedrive_steps/made_items.rs`): when the item's row is gone, what was made is removed in
OneDrive; when it is still there, a listing brings the made item and the plan compares the two.
After a restart nothing of this is done: a file uploaded and then removed here before its commit
comes back from OneDrive.

### 4.6 What the answers mean

`graph_refusal::decide_refusal` reads a refusal; `sync_plan_task/step_settlement.rs` writes what
follows, the one place that counts tries ([sync.md](sync.md) §7).

| Answer | Action |
|---|---|
| `200`, `201` | the commit |
| `409` | a new file: §4.4. A directory: an empty directory there that no row is bound to is this step's own and is adopted. Otherwise `NameTaken`: the name and its holder as OneDrive spells it are kept, a listing is asked for, and the plan decides between the two (§5) |
| `412` | §4.2 |
| `404` | the item is gone (`ItemGone`) or the directory sent into is (`DirectoryGone`): a listing is asked for. For a removal, the item is gone: done |
| `507`, `quotaLimitReached` | §4.7 |
| `413` | the file is larger than OneDrive takes: a refusal for good (`too-large`), its session given up |
| `400` | a refusal for good, with OneDrive's message |
| `403` | a refusal for good (`forbidden`) |
| `423` | locked (co-authoring): tried again later |
| `429`, `503` | a fragment: §4.3. Then, and for any other request, every step towards OneDrive of the folder waits (§6.2) |
| the account signed out | all sending held until the sign-in |
| any other `4xx` | tried again later |
| the network, a timeout, another `5xx` | §4.5, then tried again after 1 second, doubling to an hour |

A refusal for good is kept with the local version that was sent, and the item is not sent again
until that version changes; the activity log records `upload-failed` once per item and reason.
Nothing is ever dropped for failing.

OneDrive takes a file of 250 GiB and refuses one byte more, with `413` on the first fragment
(`crates/konedrive-graph/src/drive/error.rs`); nothing is checked before the request.

### 4.7 A full OneDrive

`OneDriveSpace` (`sync_parts/onedrive_space.rs`) is the one owner of "OneDrive is full": full when
the quota's state is `exceeded` or less than 1 MiB is left. A refusal for lack of space reads the
quota at once (a read of the last 10 seconds is used again) and decides:

- **space left**: only this file waits, as too big (`too-big:<bytes needed>:<bytes free>`), kept
  in the step memory, until a quota read shows it fits;
- **no space left**, or a quota that cannot be read: OneDrive is full. No content is sent, and an
  upload under way stops at its next fragment with its session kept. Moves, renames, removals and
  new folders go on; a `507` to one of those is a failed try, tried again.

The quota is read every 30 minutes while OneDrive is full or a file waits as too big, on
`Folder.Refresh` and on `Account.RefreshInfo`; the folder's start reads it once before the first
plan, since "full" is not kept across a restart. Every read decides again, and the bytes of each
committed upload come off what is left until the next read. The account has one quota, which the
window shows ([accounts.md](accounts.md)).

## 5. Conflicts

What the two sides' changes mean for one item is the plan's tables ([sync.md](sync.md) §6); a
binding that moves from one item to another is [sync.md](sync.md) §4. For a file the user sees:

| Here | In OneDrive | Outcome |
|---|---|---|
| content changed | content changed, to another content | a conflict copy: the item here gives up its binding, OneDrive's content keeps the name, and the content here is renamed to a conflict copy's name and sent as a new file |
| content changed | content changed, to the same | agreed; nothing is sent (hashed first where a hash is missing) |
| content changed | moved or renamed | the file follows OneDrive's place here, and its content is sent into the item |
| moved or renamed | moved or renamed | OneDrive's place stands, where the folder can take it |
| content changed | removed | sent again as a new item |
| moved, not changed | removed | sent again at its new place when its content is on disk; otherwise it goes |
| removed | content changed | the file comes back with OneDrive's content |
| removed | moved or renamed | the removal is sent |
| new | new, at the same name | two directories are one item, and so are two files of one content; otherwise the file here steps aside to a conflict copy's name |

A file made here at a name OneDrive gives another item meanwhile steps aside the same way
(`sync_plan/name_contest.rs`). A directory OneDrive removed that holds what only this computer has
stays with it, and is sent again (F255).

**The conflict copy's name** (`sync_plan/conflict_copy_name.rs`): this computer's name added
before the extension, the stem ending at the last dot — `Report.docx` → `Report-pc.docx`,
`archive.tar.gz` → `archive.tar-pc.gz`, `.bashrc` → `.bashrc-pc` — and from the second try a
number after it (`Report-pc-2.docx`), up to 100 names, the first free as OneDrive compares names.
The stem is cut, between two letters, so that the name fits a directory entry. The computer's name
is `machine_name` in the account's section of `config.toml`, else the host name; either is cut at
its first dot, a character OneDrive refuses replaced by `-`, at most 32 characters, `linux` when
nothing is left (`local/names.rs`, `machine_name`; `Conflicts.MachineName`).

The rename is a step towards the folder (`FolderStep::RenameToConflictCopy`), and its commit
writes the conflict's record in the same transaction (`item_store/step_record.rs`), so that no
copy is made that nothing knows of. `Conflicts.List()` and the window's Conflicts page list the
copies; `Dismiss` takes one off the list and leaves the file.

**The mass-removal guard**: there is none. A removal is sent as any other change, however many
items it takes; OneDrive's recycle bin is the one place it is got back from (F302).

## 6. What holds sending back

### 6.1 The holds

`SendingHolds` (`sync_parts/sending_holds.rs`) owns what holds all of a folder's sending, each
cause set and cleared by its own owner, every change waking the plan. In the order shown:

| Hold | Set by |
|---|---|
| `Stopping` | the folder's sync stopping |
| `SignedOut` | a step answered that the account is signed out; cleared by the sign-in |
| `WritesNotAllowed` | the account may not write (§2.1) |
| `FolderReadOnly` | the switch to read-only (§2.3) |
| `Paused` | the user's pause or the automatic hold (§6.3) |
| `Throttled` | OneDrive asked to wait (§6.2) |
| `FirstListingNotEnded` | the folder's start: nothing is sent on rows a stopped daemon left behind OneDrive until a listing of this run ended |

Every hold holds every step towards OneDrive. A download is held only by `Stopping`, `SignedOut`
and `Paused`; steps towards the folder and of the rows by none. The queue shows the first hold;
whether a step is held asks every hold.

### 6.2 Throttling

A `429` or `503` that a fragment did not get past holds every step towards OneDrive of the folder
until `Retry-After` (at most an hour). Without one, the wait is 10 seconds, doubling with each
throttle to an hour, and back to 10 seconds once a step goes through. While a wait is under way a
time OneDrive names takes the place of one chosen here, of two named times the later end stands,
and a refusal that names none adds nothing.

### 6.3 Pause and the automatic hold

What background work an account runs is decided in one place, `conditions/running.rs`, from the
user's pause, the automatic hold and the account's settings.

**The user's pause** (`Folder.Pause(seconds)`, `Resume`; `konedrivectl sync pause`, `resume`) is
kept with the account's settings (`paused_until` in its section of `config.toml`), so it outlasts
a restart, a timed one ends by itself, and it is set whether the folder's sync runs or not
(`account_folder/pause.rs`). It holds sending and the downloads of pinned files, the listing's
poll and its notification socket, and thumbnails. Reading a file and `Hydrate` go on, and
programs still change the folder: their changes wait. An upload stops after the fragment in flight, with its session
kept; a request in flight is never cut. Resume, or the end of a timed pause, ends the hold and
wakes the plan, and a kept session goes on from where OneDrive says it stands.

**The automatic hold** holds an account back by itself (`conditions/mod.rs`, one watcher of the
system bus for the daemon):

- on a **metered connection** — NetworkManager's `Metered` is `1` (yes) or `3` (guessed yes) —
  while `pause_on_metered` is on, the default;
- **on battery** — UPower's `OnBattery` — as `on_battery` says: `sync`, nothing; `power-saver`,
  the default, while the power profile (`ActiveProfile` of `org.freedesktop.UPower.PowerProfiles`,
  or of `net.hadess.PowerProfiles`) is `power-saver`; `pause`, always. On mains power the battery
  never holds.

A source that is missing or cannot be read is no reason to hold back (F175). The two settings
are the whole app's: top-level keys of `config.toml`, `Accounts.SetPauseOnMetered` and
`SetOnBattery`, which reach every account at once. `Folder.HeldBack` says why an account holds
back now: `metered`, `on-battery`, `power-saver`, or empty; with a network and a battery reason at
once, `metered`. The hold is never written anywhere and never shows as `Paused`; it stops what the
pause stops. `Folder.SyncAnyway()` (`konedrivectl sync anyway`) lifts it for that account until a
source or a setting changes; `sync anyway --all` and the tray's Sync Anyway lift it for every
account that holds back by itself and is not paused by the user.

## 7. On the bus and the command line

What the bus says of what waits is a pure function of three sources: the plan's last answer for
each unsent item (`PublishedPlan`), what the steps remember, and the holds (`upload_queue/mod.rs`).
Nothing there keeps a sum; the counts are made once per plan, not per call, and the lists are read
through the store's reading connection, which never waits for a writer.

`org.konedrive.UploadQueue`, per account:

- `Changes(limit)`: each unsent item, oldest first by its stamp, as
  `(seq, kind, path, state, sent, total, reason, next try)`. The kind is `mkdir`, `create`,
  `update`, `move` or `delete`, from the item's versions (`decide_unsent_kind`). The state, the
  first that holds (`decide_queue_state`): `blocked` (a refusal for good, or a wait only the user
  ends), `waiting` (the folder is read-only, its sync does not run, the check at start left it,
  the plan is at fault about it), `paused`, `running`, `retry` (a failed try waits for its time),
  `waiting` (the quiet time, open for writing, another item, a listing), `ready` (a planned step,
  a wait for space);
- `UnsentCount()`: every unsent item, whatever holds it, for a folder in any state;
- `NotUploaded()`, `NotUploadedSummary()`, `NotUploadedFiles(reason, limit)`: what needs the user,
  per file and grouped by reason;
- `PendingCount`, `PendingBytes`, `BlockedCount`, `QuotaFull`, `QuotaWaitingCount`,
  `QuotaWaitingBytes`, `TooBigCount`.

`Transfers.Uploads` lists the uploads under way with their bytes; `Folder.Skipped()` says of an
item OneDrive has that the folder cannot show what of it still waits to be sent here
(`uploads:<n>`). `Folder.Writable` is whether the folder takes changes now. The mount answers the
sync mark Dolphin's emblem reads (`user.konedrive.sync`: `pending`, `uploading`, `blocked`), by the
same rule as the queue's `blocked` (`mount/item_lookup.rs`, `decide_sync_mark`). The activity kinds
of uploads are `uploaded`, `cloud-moved`, `cloud-deleted`, `upload-failed` and `conflict`.

The commands: `konedrivectl account mode [read-only|read-write] [--force]`,
`sync upload-queue [--all]`, `sync not-uploaded`, `sync conflicts`, `sync dismiss`, `sync ignore`,
`sync pause [<time>]`, `sync resume`, `sync anyway [--all]`, `sync transfers`, `sync refresh`. The
members, the words and the window's pages are in [desktop.md](desktop.md).

## 8. Testing against the test account

The requests and their answers are tested against wiremock and against the fake OneDrive
(`crates/konedrived/src/tests/fake_onedrive/`), with no account: each module of
`onedrive_steps/` over the mount's fixture ([sync.md](sync.md) §10), and the VM suite over a
mounted folder. What OneDrive itself does, which those can only take as given, is checked by two
tools run by hand against a separate test account, never the user's own.

### 8.1 The guarded harness

`konedrive-write-test` (`tests/write-account/`) writes, so it refuses to start unless every guard
holds:

1. `--graph-test-drive` is the drive both tokens reach (`GET /me/drive`), and is listed in
   `write_test_drive_ids` in the `config.toml` given with `--daemon-config`;
2. the drive looks like a test account: less than 1 GiB in use and fewer than 1000 items, unless
   `--large-test-drive` says it holds more;
3. every write stays in `/konedrive-write-test/<run id>/`, which the run makes and puts into the
   recycle bin at the end. Every request goes through a proxy on `127.0.0.1` whose guard asserts
   that the item it names, or the parent of what it makes, is inside that folder; a request it
   refuses never leaves the machine, and after a refusal only the cleanup is sent;
4. at most 64 MiB per file, 200 MiB of content and 500 requests per run;
5. the write token comes from `konedrivectl dev export-access-token --read-write`, which the
   daemon hands out only for a drive on the same list (`TokenExport.ReadWrite`, a development
   build only: the `dev-tools` feature, `scripts/dev-install.sh`); both token files are `0600` and
   the user's.

`write_test_drive_ids` is empty by default and written by nothing in konedrive; the user adds the
test account's drive id to it by hand. It decides nothing about the mode. A run, within the hour
an access token lasts:

```
konedrivectl --account Test account mode read-write
konedrivectl --account Test dev export-access-token --read-write --out /tmp/kd-rw.token
konedrivectl --account Test account mode read-only
konedrivectl --account Test dev export-access-token --out /tmp/kd-ro.token
cargo run -p konedrive-write-test -- --graph-test-drive <id> \
    --graph-token /tmp/kd-rw.token --graph-read-only-token /tmp/kd-ro.token \
    --daemon-config ~/.config/konedrive/config.toml
rm /tmp/kd-rw.token /tmp/kd-ro.token
```

`--only placeholders` (what holds a session's empty file and what frees it) and
`--only notifications` (what a write sends on the notification socket) run alone, with the
read-write token only. Each check prints `PASS`, `FAIL` or `LOOK` (to be confirmed by hand); the
exit status is 0 when nothing failed, 1 when a check failed, 2 when a guard refused: `REFUSED`
before anything was written, `STOPPED` mid-run.

### 8.2 The stress run

`tests/stress/stress_uploads.py` makes, edits, moves and removes real files in a read-write test
account's folder, and after each scenario checks against Graph, read-only, that OneDrive holds
exactly what the disk has: the same paths, sizes and QuickXorHashes. It refuses to start unless
the account's drive is in `write_test_drive_ids` and the account is read-write
(`tests/stress/README.md`).

## 9. What is assumed of OneDrive

What the code relies on, and the check of §8.1 that asks it of OneDrive:

| Assumed | Checked by |
|---|---|
| `conflictBehavior=fail` in a `PUT`'s URL refuses a taken name | "an empty file: conflictBehavior=fail in the URL" |
| `If-Match` is honoured on a 0-byte `PUT` | "an empty file: If-Match" |
| a small file is one request of a session, with its guards, its time and its hash | "a small file" |
| a rename onto a taken name is refused `409`, and names collide whatever their case | "a rename to a name that is taken", "names collide" |
| 10 MiB fragments, and a resume from the session's status | "a large file" |
| an edit in OneDrive during a session makes its last fragment be refused | "an edit in OneDrive during a session" |
| a session opened with the file's time leaves the cTag as it was | "a session opened with the file's time" |
| a rename in OneDrive during a session does not stop its last fragment | "a rename in OneDrive during a session" |
| a content guarded by its cTag passes a rename and is refused after an edit | "a content guarded by its cTag" |
| a removal goes to the recycle bin | "a delete goes to the recycle bin" |
| a token refreshed after the switch back to read-only cannot write | "the token exported after the switch back" |
| the delta listing returns the daemon's own changes with the eTags they were answered with | "delta returns the run's own changes" |
| an open session holds its name with an empty file, and its cancel frees it | `--only placeholders` |

Measured and written where the code relies on it: a personal drive refuses `fileSize` in a
session request (F39); OneDrive takes 250 GiB in one file; OneDrive moves an eTag with no change
of content; `If-Match` takes a cTag as well as an eTag.

## 10. Known limits

The limits of uploads that cannot simply be removed, in [`../limitations/`](../limitations/):
consent to write that stays with Microsoft (F66); no size in a session request (F39); the empty
file an open session leaves (F172); no local trash and no confirmation of a removal (F302); a
directory that holds what the folder cannot show (F299); a change made on disk while the daemon
does not run, which is never a move or a removal in OneDrive (F298); a file that is never uploaded
and keeps a directory OneDrive removed (F255); a move between accounts (F124); no links or special
files (F300); the executable bit kept here only (F303); a trash with KDE Frameworks before 6.21
(K34); no automatic hold without NetworkManager, UPower or power-profiles-daemon (F175); a store
that is lost, with its unsent removals and moves and its open sessions (D36).

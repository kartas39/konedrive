# Desktop integration

What sits on top of the daemon: its D-Bus API, the command line, the KOneDrive window and tray icon,
notifications, download progress in Plasma, thumbnails, the Baloo exclusion, and the Dolphin
plugins. The daemon's own work is in [hydration.md](hydration.md) and [sync.md](sync.md).

## 1. Principles

- **Every client talks to the daemon.** The window, the tray, the CLI and the Dolphin menu call
  D-Bus methods and show what the daemon publishes. The window never touches the sync folder itself.
  The Dolphin emblems are the one exception, and they only read the marks on a file, by name
  (§10.1).
- **Every feature has a command-line path.** Anything the window can do, `konedrivectl` can do, so
  nothing depends on clicking through a UI, and scripts and tests can drive everything.
- **Refusals have names.** A refusal is a D-Bus error name; clients match the name, never the
  message, and turn it into a sentence about the user's file and what to do next.
- **The daemon sends codes, and their sentences are written once.** A reason, a refusal and the
  menu's `free-up-why` cross the bus as codes; the daemon does not translate them and sends no
  sentence for them. The words are in one catalogue that every client is built from (§2.10).
- **Nothing a client does opens a placeholder** (see the invariant in [README.md](README.md)).

## 2. The D-Bus API

### 2.1 Names

Session bus, service `org.konedrive.Daemon`, ten interfaces on two kinds of object, and two more in
a development build:

| Object | Interfaces |
|---|---|
| `/org/konedrive/Accounts` | `org.konedrive.Accounts` (the accounts, the client id, the helper), `org.konedrive.Files` (the per-file calls, routed by path), `org.freedesktop.DBus.ObjectManager`; in a development build, `org.konedrive.DevTools` |
| `/org/konedrive/Accounts/<id>`, one per account | `org.konedrive.Account` (its sign-in); its folder's `org.konedrive.Folder` (the folder itself), `org.konedrive.Transfers` (what moves now), `org.konedrive.UploadQueue` (what waits to go up), `org.konedrive.Conflicts`, `org.konedrive.LocalScan` and `org.konedrive.ActivityLog`; in a development build, `org.konedrive.TokenExport` |

No name carries a version: the daemon and its clients ship together in one package. A member does
not repeat its interface's name (`Conflicts.List`, not `Conflicts.Conflicts`). The definitions are
in `dbus/*.xml`, one file per interface, and a test keeps each in step with the live interface's
signatures. The daemon is D-Bus activated (`SystemdService=konedrived.service`), so the first call
from any client starts it, and every object is on the bus before the name is claimed. Nothing
answers at `/org/konedrive/Daemon`, the object of the single-account versions
([accounts.md](accounts.md) §5).

Properties change through `PropertiesChanged`, each under the interface that holds it: one change of
the state that touches several interfaces sends one signal for each. The counters,
`Conflicts.Count`, everything of `Transfers` and of `LocalScan` are coalesced: at most one signal
per interface per 250 ms, so a drive of hundreds of thousands of items cannot flood the bus. The
folder's state, its errors, its path, the pause and the hold, `Writable`, `LiveChanges` and
`QuotaFull` go at once, and so does `Folder.Overall`, which sends the coalesced ones ahead of itself
(§2.4).

The manager's interfaces, `Accounts` and `Files`, are in §2.8 and §2.9; each account's in §2.2 to
§2.7.

### 2.2 `Account`, per account

| Member | Meaning |
|---|---|
| `Id` (`s`) | the account's id, the last element of its object path: 12 lowercase hexadecimal characters |
| `Label` (`s`) | the account's name ([accounts.md](accounts.md) §2) |
| `Mode` (`s`) | the mode the account runs in, `read-only` or `read-write`: read-write only while `config.toml` says so, and its token carries `Files.ReadWrite` and reaches the account's recorded drive ([accounts.md](accounts.md) §10) |
| `State` (`s`) | `signed-out`, `signing-in` or `signed-in` |
| `LastError` (`s`) | the reason for the most recent failure, a sign-in refused as another account's included ([accounts.md](accounts.md) §6.2); with no failure, why a signed-in account runs read-only although `config.toml` says read-write, or that Microsoft answered a read-only request with a token that can write, which is used to read only (limitations log F66); empty otherwise |
| `DisplayName`, `Email` (`s`) | from `GET /me`; empty for a signed-out account |
| `QuotaUsed`, `QuotaTotal`, `QuotaRemaining` (`t`), `QuotaState` (`s`) | the account's one quota: bytes used and in all, Graph's `quota.remaining` (never `total - used`) and `quota.state` (`normal`, `nearing`, `critical`, `exceeded`), from `GET /me/drive`, whoever reads it — the account's info (a sign-in, `RefreshInfo`) or the uploads' space check (`Folder.Refresh`, a refused upload, the check every 30 minutes; [writes.md](writes.md) §6.4). Every read updates all four, and what it did not give keeps its last value; between reads the bytes uploaded come off `QuotaRemaining` and are added to `QuotaUsed`. 0 and empty until read, and again after a sign-out; kept in `account.json` across restarts |
| `BeginSignIn() → s url` | starts the loopback listener and returns the authorization URL; the caller opens it ([sync.md](sync.md) §12.1). The bus's `Failed` (§2.6) for an account that is signed in or signing in |
| `CancelSignIn()`, `SignOut()`, `RefreshInfo()` | as named; `SignOut` deletes the refresh token and the cached name and quota; `RefreshInfo` reads the name, the address and the quota again |
| `SetLabel(s)` | renames the account; `InvalidArgs` for a label the rules refuse |
| `SetMode(s mode, b force) → s sign_in_url` | switches to `read-only` or `read-write` ([accounts.md](accounts.md) §10); the URL of the sign-in a switch to read-write needs, empty when none is needed. Refused `NotSignedIn` (to read-write), `PendingUploads` unless `force` (to read-only), `InvalidArgs` for another mode, `Failed` when it cannot be done now. A switch to read-write ends in `Mode` turning `read-write`, or in `LastError` saying why not; cancelled, in neither |

Neither the refresh token nor the access token is ever exposed through `Account`; `TokenExport`
(§2.7) is a development build's.

### 2.3 The folder's methods, per account

`Folder`:

| Method | Does |
|---|---|
| `Register(s path)` | binds an empty folder to the account's drive, with the helper intercepting ([hydration.md](hydration.md) §14.1); refused `Overlaps` for a folder that is, is inside, or contains another account's, and `NotEmpty` for one that carries another account's drive ([accounts.md](accounts.md) §6.3); the other refusals are in §2.6 |
| `RegisterWithoutInterception(s path)` | the developer's local folder, with nothing intercepting ([hydration.md](hydration.md) §14.3); refused `Overlaps` and `NotEmpty` in the same way |
| `Unregister()` | Forget: leaves every file as it is ([hydration.md](hydration.md) §14.5); refused `NoHelper` for an intercepted folder with no helper, `PendingUploads` while changes wait to be uploaded |
| `PopulateFromDirectory(s source_dir) → t created` | fills a local folder with placeholders mirroring a directory; `Unsupported` on a OneDrive folder, and for a source inside or around the folder |
| `Refresh()` | runs a sync cycle now, tries the changes in backoff, and reads the quota again (which may end a full OneDrive); refused `NoHelper` while the folder has no helper link, `Unsupported` for a local folder |
| `Skipped() → a(ssss)` | (path, reason, what keeps it here, where it is here) for everything in OneDrive that the folder cannot hold ([sync.md](sync.md) §7.5); the third is empty for an item that is not on this computer, and says what an item that still is waits for ([writes.md](writes.md) §9) |
| `FreeUpSpace() → (u files, t bytes, u busy)` | frees up every downloaded file that is not in use and not kept by a pin; files open somewhere or busy with a download are skipped and counted, never waited for; a file whose change waits to be uploaded is left, and counted as busy |
| `Pause(u seconds)`, `Resume()` | pause the account — no upload, no poll, no thumbnails; fills on open, `Hydrate` and detection go on — for `seconds`, or until `Resume` when 0; the pause outlasts a daemon restart; `Unsupported` for a folder not connected to OneDrive |
| `SetIgnorePatterns(as)` | the names of the user's own files that are never uploaded (shell globs on a name); written to `config.toml`, then the whole folder is scanned again; `InvalidArgs` for a pattern that is empty or blank, holds "/" or a NUL, or is longer than 255 bytes |
| `SyncAnyway()` | lifts the automatic hold ([writes.md](writes.md) §11) of this account now, until a source or the app's `pause_on_metered` / `on_battery` (`Accounts.SetPauseOnMetered`, `SetOnBattery`, §2.8) changes; not kept across a restart; `Unsupported` for a folder not connected to OneDrive |
| `SetThumbnails(b)` | the account's own sync setting (§8): whether Graph's thumbnails are fetched. Written to the account's section of `config.toml` (`thumbnails`) and taken at once; `Unsupported` for a folder not connected to OneDrive. When the account holds back by itself is the whole app's setting, on `Accounts` (§2.8) |

`UploadQueue`, every method of which is refused `Unsupported` for a folder not connected to OneDrive
and `NotUp` for one that is down:

| Method | Does |
|---|---|
| `Changes(u limit) → a(tsssttsx)` | the changes waiting to be uploaded, oldest first (0: all): seq, kind (`create`, `mkdir`, `update`, `move`, `delete`, `move-out`), path, state (`waiting`, `ready`, `running`, `retry`, `blocked`, `held`; while the account is paused or holds back by itself, every row but a blocked or held one reads `paused`, with no reason and no next try), bytes sent, bytes in all, reason, next try |
| `ConfirmDeletes() → u`, `RestoreDeletes() → u` | the mass-delete guard's two answers: the held removals go ahead, or are dropped and the items placed again; how many rows |
| `NotUploaded() → a(ss)` | (path, reason) for what stays on this computer: what is never uploaded (`symlink`, `hard-link`, `not-downloaded`, …), what cannot be read (`unreadable`) or has damaged marks (`state-unreadable`), and every blocked change (`name-characters`, `forbidden`, …). A change whose reason is the one the list already gives for the same path is left out, here and in the two below: such a file is listed once |
| `NotUploadedSummary() → a(ssut)` | what is kept back, one row per reason: group, reason, count, bytes. Groups, in order: `one-action` (one action fixes every file: `waiting-for-space`, which while OneDrive is full also counts the waiting changes that send content and give no other reason; `too-big`, every `too-big:<needed>:<free>` summed as one; `forbidden`; and `quota-exceeded` from an older version, until a start converts it), `per-file` (a name OneDrive refuses, `too-large`, `unreadable`, `state-unreadable`, `refused` — every `refused: <message>` summed as one, as is every reason with a detail behind its key, `<key>: <detail>`; and every blocked change whose reason is not a one-action or a never one), `never` (symlinks, pipes, sockets, devices, `other-device`, `reserved-name`, `hard-link`), `waiting` (goes up by itself: open for writing, locked, …, and any reason the daemon does not know, of a change that is not blocked). Everything `NotUploaded` lists, plus the changes waiting or in backoff with a reason, and the `ready` ones waiting for space; held removals are not. The table is `crates/konedrive-reason/src/lib.rs`; `crates/konedrived/src/upload/kept_back.rs` adds the row's state |
| `NotUploadedFiles(s reason, u limit) → (a(ss) items, u total)` | the files of one reason as the summary names it, by path, at most `limit` (0: all): (path, reason as stored — a refused one keeps OneDrive's message); and how many there are |

`Conflicts`:

| Method | Does |
|---|---|
| `List() → a(xsss)` | (time, original path, path of the kept version, how it was kept: `rescued` or `copy`) ([sync.md](sync.md) §10.3) |
| `Dismiss(s rescued_path)` | takes one conflict off the list; the file stays where it is; `NoConflict` for a path that names none |

`ActivityLog`:

| Method | Does |
|---|---|
| `Recent(u limit) → a(xsss)` | (time, kind, path, detail), newest first |

### 2.4 The folder's properties and signals, per account

`Folder`:

| Property | Meaning |
|---|---|
| `Path` (`s`) | the account's folder, empty when it has none. A folder `config.toml` records has its path from the daemon's start, while `State` is still `waiting`: it is not brought up yet |
| `State` (`s`) | `none`, `waiting`, `listing`, `ready`, `no-interception` or `error` (§2.5) |
| `Source` (`s`) | `onedrive`, `local`, or empty ([sync.md](sync.md) §3) |
| `LastError` (`s`) | everything that needs attention, in words, joined: the helper's advice while the folder is known to wait for a helper that is down (§2.5), the no-interception warning, the registration's trouble, the sync's and the failed-update note, then the notes of the lock, the watcher and the outbox. For the log and for people: no client decides anything by its text |
| `Overall` (`(ss)`) | the state the account is in as a whole, and the reason for it; see "The account as a whole" below |
| `Trouble` (`s`) | the sentence of the trouble there is now, whatever the reason of `Overall` is; empty with none. While `State` is `error`: everything `LastError` says. While it is `ready` or `no-interception`: the problems that do not stop the folder, OneDrive out of reach among them, joined with ". ", without the no-interception warning and the failed-update note. Empty in every other `State` |
| `NotUpdated` (`s`) | the failed-update note alone ("N file(s) changed in OneDrive could not be updated here yet: …"), whenever there is one; empty otherwise |
| `ItemsListed`, `ItemsPlaced`, `SkippedCount` (`t`) | the listing's progress ([sync.md](sync.md) §8) and what the folder cannot hold ([sync.md](sync.md) §7.5) |
| `LastChecked` (`x`) | Unix time of the last successful cycle; 0 for never |
| `LocalBytes` (`t`) | the space the folder's files take on disk (`st_blocks × 512`), measured by a walk at bring-up, after a cycle, a download, a replacement or a free-up, at most every 5 s |
| `PinnedCount` (`u`) | how many files and folders carry a pin of their own ([pinning.md](pinning.md) §7) |
| `IgnorePatterns` (`as`) | the ignore list; read-only |
| `Paused` (`b`), `PausedUntil` (`x`) | whether the account is paused, and when the pause ends by itself (0: until `Resume`) |
| `HeldBack` (`s`) | why the account holds its background work back by itself: `metered`, `on-battery`, `power-saver`, or empty ([writes.md](writes.md) §11); never the user's pause, which `Paused` shows |
| `Writable` (`b`) | whether what is added, changed, moved or deleted in the folder is uploaded now: the folder shows OneDrive, its account's `Mode` is `read-write`, the folder is watched and the read-only lock is off it. False for a folder that runs read-only although its account is read-write (`LastError` says why), and while its sync is starting or not running |
| `LiveChanges` (`s`) | how changes made in OneDrive reach this computer ([sync.md](sync.md) §4.2): `connected` (at once, through Graph's notification socket; the poll runs every 5 minutes), `connecting` (trying, or waiting before the next try; the poll runs every minute), `off` (paused, held back, or not a OneDrive folder) |
| `Thumbnails` (`b`) | the account's own sync setting; `true` when absent from `config.toml` |

`Transfers`:

| Property | Meaning |
|---|---|
| `Downloads` (`a(stt)`) | each download under way as (path, bytes done, bytes total): fills on open, `Hydrate`, pinned downloads and replacements; not thumbnails |
| `Uploads` (`a(stt)`) | each upload under way, the same way |
| `DownloadSpeed` (`t`), `UploadSpeed` (`t`) | bytes a second each way, the average of the last 3 s |
| `ActiveDownloads` (`u`), `ActiveUploads` (`u`) | the files downloading and uploading now: the entries of `Downloads` and `Uploads`, each file once however many streams it runs (no thumbnail is a file downloading) — not slots |
| `PoolInUse` (`u`), `PoolSize` (`u`), `PoolCeiling` (`u`) | the account's transfer pool ([hydration.md](hydration.md) §6.4): every slot held now, of all four classes (downloads, uploads, metadata operations, files being opened) with the opens' reserve, so it may be above the size (an open's reserve; slots still held after a throttle halved the pool); the pool's size now; and its ceiling (`[transfers] max`) |
| `LargeFiles` (`u`), `LargeStreams` (`u`), `LargeStreamLimit` (`u`) | the large files (100 MiB and up) the sync moves now, each once however many streams it runs, files being opened left out; the streams of large sync transfers under way (a download in parts runs several; a file being opened is never one); and how many such streams may run at once (`[transfers] large`) |
| `RetryAfter` (`u`) | the seconds left of OneDrive's `Retry-After` wait, during which no transfer starts (0: none) |
| `DownloadLeftCount` (`u`), `DownloadLeftBytes` (`t`), `DownloadDoneBytes` (`t`), `DownloadTimeLeft` (`u`), and the same four for uploads | the queue totals: files left to download and changes left to upload (those waiting for space or too big are not counted as left), their bytes less what is moved of those under way, the bytes done since nothing was last left that way, and the seconds left at the speed of the last 30 s (0: unknown, as after 10 s without movement, during a `Retry-After`, and for uploads while paused or held back) |

The speeds, the pool, `RetryAfter` and the queue totals are updated once a second while anything
moves or a `Retry-After` runs; the file counts with the lists they count.

`UploadQueue`:

| Property | Meaning |
|---|---|
| `PendingCount` (`u`), `PendingBytes` (`t`) | the changes waiting to be uploaded, neither blocked nor held, and the size of what they send; those waiting for space or too big are among them |
| `BlockedCount` (`u`) | the changes that need the user before they can go up (see `NotUploaded`); removals the mass-delete guard holds are not counted |
| `HeldCount` (`u`) | the removals the mass-delete guard holds, waiting for `ConfirmDeletes` or `RestoreDeletes`; `held` rows in `Changes()` |
| `QuotaFull` (`b`) | OneDrive is full: no content goes up until a quota read finds space ([writes.md](writes.md) §6.4) |
| `QuotaWaitingCount` (`u`), `QuotaWaitingBytes` (`t`) | the changes that wait for space in OneDrive, and the size of their files |
| `TooBigCount` (`u`) | files refused as too big for the space left; each is `ready` in `Changes()` with reason `too-big:<needed>:<free>` |

`Conflicts`:

| Property | Meaning |
|---|---|
| `Count` (`u`) | how many conflicts are listed |
| `MachineName` (`s`) | the name copies of files changed on both sides are named after ("Report-`<MachineName>`.docx"): `machine_name` in `config.toml`, or the host's name, cleaned as [writes.md](writes.md) §7 says |

`LocalScan`:

| Property | Meaning |
|---|---|
| `State` (`s`), `Reason` (`s`), `Started` (`x`), `Directories` (`t`), `Files` (`t`), `Expected` (`t`), `Finished` (`x`), `Took` (`u`) | the Full local scan of a read-write folder ([writes.md](writes.md) §4.6): `running`, `idle`, or `none` for a read-only folder; why it runs (`start`, `read-write`, `helper-back`, `overflow`, `ignore-list`, `periodic`); when it started; the directories and the files (every entry that is not a directory) it has seen so far; about how many items it will see — the items the base had placed when it started, not the disk's count, so never a percentage; when the last one finished (0: none since the daemon started) and how long it took, in seconds. While idle, the reason, start and counts are the last scan's. The small examinations after each change are not reported. Updated at most once a second while a scan runs, and once when it ends |

**The account as a whole.** The daemon decides what state an account is in, and says why:
`Folder.Overall` is the state (`ok`, `syncing`, `warning`, `paused`, `offline`) and the reason. The
rule is one function over what the account and its folder publish
(`crates/konedrived/src/status/overall.rs`); the spellings are `konedrive_dbus::overall`'s. The
first row that holds decides, in this order:

| State | Reason | When |
|---|---|---|
| `offline` | `signing-in` | the account is signing in |
| `offline` | `signed-out` | the account is not signed in |
| `offline` | `no-folder` | no folder is registered |
| `syncing` | `starting` | the folder is recorded and not up yet, with nothing known to be wrong (`State` is `waiting`) |
| `warning` | `stopped` | the folder's syncing has stopped on an error (`State` is `error`) |
| `warning` | `deletes-held` | deletions wait for the user's decision |
| `warning` | `conflicts` | changed files were moved out of the way |
| `warning` | `quota-full` | OneDrive is full (`QuotaFull`) |
| `warning` | `too-big` | files are too big for the space left |
| `warning` | `blocked` | changes cannot be uploaded |
| `warning` | `not-updated` | files changed in OneDrive could not be updated here yet (`NotUpdated` says it) |
| `warning` | `helper-unavailable` | the helper is not connected, for a folder it intercepts; not for one registered without interception |
| `paused` | `paused` | the user paused the account |
| `paused` | `held-back` | the account holds back by itself (metered, battery, power-saver) |
| `offline` | `unreachable` | OneDrive cannot be reached, and nothing else is wrong |
| `warning` | `trouble` | any other trouble that does not stop the folder |
| `syncing` | `listing` | a listing of the whole drive is running: the first, or one from the start after a refused link |
| `syncing` | `transferring` | files are downloading or uploading, or changes wait to upload |
| `ok` | `up-to-date` | none of the above |

- Nothing is decided by a sentence. Out of reach is a kind the sync's trouble carries
  (`TroubleKind`), the no-interception warning and the failed-update note are facts of the folder;
  rewording a message changes no state.
- `unreachable` holds only when out of reach is all that is wrong: with another note beside it the
  account is `warning` for `trouble`. While a listing of the whole drive runs, trouble that does not
  stop the folder is not counted, and `Trouble` is empty.
- `Overall`, `Trouble` and `NotUpdated` are announced the moment what they say changes, each by
  itself, and never otherwise.
- A reason never arrives ahead of the count a client says with it: when `Overall` changes, the
  coalesced properties that changed and are not sent yet (§2.1, the conflicts' `Count` and
  `HeldCount` among them) are announced first, whatever is left of their 250 ms, and `Overall` after
  them.
- The one state no daemon can say is "the service is not running": a client shows `offline` for it
  by itself, and decides nothing else.

The signal `ActivityLog.Added(x time, s kind, s path, s detail)` announces each event as it is
recorded; a client that falls far behind can miss some, and `Recent` still has them. The kinds are
`downloaded`, `freed`, `added`, `updated`, `removed`, `moved`, `listed`, `conflict`, `failed` (a
download) and `update-failed` (a changed file could not be replaced here). For a full disk, the
detail is exactly "not enough disk space" in either failure kind. A read-write account adds
`uploaded`, `cloud-moved` (detail: where it was), `cloud-deleted`, `upload-failed` (detail: the
reason as stored, once per change and reason), `restored` and `not-uploaded` (made here and removed
here before its upload finished; detail: why); a copy of a file changed on both sides is a
`conflict` whose detail is the copy, beside the file.

The daemon keeps the newest **200** events in the tree store; a Forget or a rebuild drops them. The
log is a summary, not a record of every file: a first listing or a Full reconcile is one `listed`
event ("12345 items"), beside the removals and conflicts it made; a cycle logs at most 50 events of
each kind plus one "and N more" for its changes, and the same again for what it removed only in part
and for its conflicts; and a `FreeUpSpace` that freed anything is one `freed` event.

### 2.5 `Folder.State` and `HelperState`

`Folder.State` is computed, never stored — one state, one source of truth:

- `none` — no folder is registered;
- `waiting` — a folder is recorded and not up yet, with nothing known to be wrong: before its
  bring-up, or while it waits for a helper that is not known to be down;
- `error` — the registration or the sync is in trouble: signed out, another account than the store
  was built from, a drive that is another account's, an unusable store, a failed bring-up or
  recovery, a folder that was up and lost the helper, a folder waiting for a helper that is known to
  be down (`not-installed`, `stopped`, `failed`), or an account held back for colliding with another
  in `config.toml` ([accounts.md](accounts.md) §4.1); `LastError` says which;
- `listing` — a listing of the whole drive is under way (only ever in place of `ready`);
- `ready` or `no-interception` — the registration's own mode.

`HelperState` says what the daemon knows of the helper. It is on `Accounts`, not on each account,
because one link serves every account ([accounts.md](accounts.md) §3.3). While the daemon holds the
link, `connected`. Otherwise it asks systemd — read-only, over the system bus, with no privilege,
with 5 s to answer — for `konedrive-helper.service`: not found is `not-installed`; inactive or
stopping is `stopped`; failed, or a unit that cannot be loaded, is `failed`; no system bus, no
systemd, no answer, or a unit systemd says is running while the daemon has no link yet, is
`unknown`. It is `unknown` from the moment the link drops until systemd has answered, and is asked
again every 30 s while there is no link. The sentence for each state — how to install, start or
diagnose the helper — is `konedrive_dbus::HelperState::advice`, for the daemon's `LastError` and the
CLI; the window has sentences of its own for its helper card. Each account whose folder waits for a
helper that is known to be down, or was up and lost it, begins its `Folder.LastError` with the
advice.

### 2.6 Errors

A refusal is an error name under `org.konedrive.Error`: `NotSignedIn`, `AlreadyRegistered`,
`NoHelper`, `NotEmpty`, `Unsupported`, `NoRoot`, `NoSource`, `NotUp` (the folder is down, or its
store is not open), `OutsideRoot`, `NotManaged`, `NotHydrated`, `ModifiedLocally`, `InUse`,
`NoConflict`, `NotAllowed` (a free-up of something a pin keeps, [pinning.md](pinning.md) §5),
`NotUploaded` (a free-up of a file whose change waits to be uploaded; `WebUrl` of an item with no
id), `Unreachable` (`WebUrl`: OneDrive did not answer), `Overlaps` (a folder that is, is inside, or
contains another account's; the message names that account), `NoAccount` (`Remove` of a path that
names no account), `WritesNotAllowed` (`TokenExport.ReadWrite` only: the account's drive is not in
`write_test_drive_ids`, or its token reaches another drive), `ModeNotGranted` (the account does not
run read-write), `PendingUploads` (a switch to read-only, a Forget or a `Remove` while changes wait
to be uploaded), and `Failed` for everything without a name of its own (an I/O failure, an account
held back).

A registration first refuses an account that is held back or being removed (`Failed`), then in the
order `NotSignedIn`, `AlreadyRegistered`, `NoHelper`, `Overlaps`, then the folder checks;
`RegisterWithoutInterception` gives neither `NotSignedIn` nor `NoHelper`. An argument that will not
do — a label, a client id, a mode, an `on_battery` choice, an ignore pattern — is refused with the
bus's own `InvalidArgs`, and `Accounts` refuses with the bus's `Failed`
(`org.freedesktop.DBus.Error.Failed`) a call that is not possible now — a client id changed while an
account is signed in, or anything while `config.toml` cannot be read: nothing needs to tell those
reasons apart. `Account`'s `BeginSignIn`, `SignOut` and `SetLabel` refuse with the bus's `Failed`
too; `SetMode`'s `Failed` is `org.konedrive.Error.Failed`.

### 2.7 `TokenExport`, per account, in a development build

Served only by a daemon built with the `dev-tools` feature (`scripts/dev-install.sh`); the released
package has no such interface. `ReadOnly() → s` returns an access token of the account for a test
run in the VM — about an hour of `Files.Read` on that account's drive, whatever its mode, never the
refresh token ([sync.md](sync.md) §12.2). `ReadWrite() → s`, for the test-account harness only,
returns one that can change files: refused `WritesNotAllowed` for an account whose drive
`write_test_drive_ids` does not list or whose token reaches another drive, `ModeNotGranted` for one
that is not read-write. Both are refused `NotSignedIn`.

### 2.8 `Accounts`

| Member | Meaning |
|---|---|
| `List` (`ao`) | every account's object, in the order the accounts were added |
| `ClientId` (`s`) | the application id every account signs in with; konedrive's own built-in one unless `SetClientId` overrode it |
| `HelperState` (`s`) | `connected`, `not-installed`, `stopped`, `failed` or `unknown`: one helper serves every account (§2.5) |
| `LastError` (`s`) | trouble that belongs to no account: `config.toml` cannot be read or was written by a newer version, a migration step failed, an account could not be loaded; empty when none |
| `Version`, `Commit` (`s`) | the daemon's build, which never changes while it runs (§4, the version line) |
| `SignIn() → (u sign_in, s url)` | starts a sign-in for a new account, the only way to add one; returns the sign-in's number, not reused while the daemon runs, and the URL to open. The sign-in belongs to no account: nothing is made until it has succeeded. Refused, with nothing started, when `config.toml` cannot be written or the listener cannot be bound. One at a time: a newer `SignIn` ends the one before as `cancelled`, and so does `SetClientId`; either of them refused ends nothing ([accounts.md](accounts.md) §7.2) |
| `CancelSignIn(u sign_in) → (b cancelled)` | cancels that sign-in; never refused. `true` when this call ended it. `false`, and nothing changes, for a number that is not under way, which includes a sign-in whose account is being made at that moment: its own `SignInFinished` says how it ended |
| `SignInFinished(u sign_in, s outcome, s message, o account)` (signal) | how a sign-in ended, once for each unless the daemon stopped first: `signed-in` (the account was made and is in `List` already; `message` is its label and `account` its path), `cancelled` (`message` empty, `account` `/`), `already-added` (the drive is another account's; `message` is that account's label and `account` its path) or `failed` (`message` says why, `account` is `/`). Only `signed-in` made an account |
| `Remove(o account)` | forgets the account's folder as `Folder.Unregister` does, with its refusals, signs it out, deletes its refresh token, cached name and quota and tree store, and takes its object off the bus; the folder's files and the rescued files stay ([accounts.md](accounts.md) §7.3) |
| `SetClientId(s)` | overrides the built-in client id with one of the caller's own (a custom Entra registration); validates and stores it, `InvalidArgs` for a malformed one, and refused while any account is signing in or signed in. Once stored, it ends a sign-in for a new account under way as `cancelled` |
| `PauseOnMetered` (`b`), `OnBattery` (`s`) | when every account holds back by itself ([writes.md](writes.md) §11): on a metered connection or not; on battery `sync`, `power-saver` or `pause`. The top-level `pause_on_metered` and `on_battery` of `config.toml`; absent, `true` and `power-saver` (an `on_battery` the daemon does not know reads `power-saver`, with a warning in the log). Both announced with `PropertiesChanged` |
| `SetPauseOnMetered(b)`, `SetOnBattery(s)` | change them for every account at once: written to `config.toml` under its lock, taken by every account's hold, and every account's `SyncAnyway` ends; `InvalidArgs` for an `on_battery` choice other than the three |

The same object is an `org.freedesktop.DBus.ObjectManager`: `InterfacesAdded` when an account's
object is on the bus, `InterfacesRemoved` when it goes, and `GetManagedObjects` for tools. The
window and the CLI follow `List` instead.

In a development build (`dev-tools`) the same object also serves `org.konedrive.DevTools`, with one
method: `AddAccount(s label) → o` adds a signed-out, read-only account with no folder, which never
has to sign in, for a folder that shows a local directory; `InvalidArgs` for a label the rules
refuse. A release build has no way to add a signed-out account.

### 2.9 `Files`

The calls on one file or on chosen paths, each routed by path to the account whose folder holds it
([accounts.md](accounts.md) §3.5). A path in no account's folder is refused `OutsideRoot`.

| Method | Does |
|---|---|
| `Hydrate(s path)` | downloads one file now ([hydration.md](hydration.md) §6.5) |
| `Dehydrate(s path)` | frees one file up ([hydration.md](hydration.md) §8) |
| `ItemState(s path) → s` | `online-only`, `hydrating`, `hydrated`, `dehydrating` or `not-managed`, read from the attribute by name (`lgetxattr`), never by opening the file; `not-managed` also for a directory and for a path in no account's folder |
| `Pin(as paths) → u queued` | "Always keep on this device" ([pinning.md](pinning.md) §3) |
| `Unpin(as paths) → u unpinned` | takes each path's own pin off ([pinning.md](pinning.md) §5) |
| `FreeUp(as paths) → (u files, t bytes, u busy, u skipped_pinned)` | "Free up space" ([pinning.md](pinning.md) §5) |
| `WebUrl(s path) → s url` | "Open in OneDrive": the address of the item's page in OneDrive's web interface, asked from Graph each time (§10.2) |
| `Menu(as paths) → a{sv} menu` | what a file manager's context menu may offer for a selection (below, and §10.2) |

An account's folder itself is a path of that account: `Pin`, `Unpin` and `FreeUp` take it, `WebUrl`
answers it with the address of the drive's root, and only `Menu` leaves it out of `paths`
([pinning.md](pinning.md) §5).

**`Menu`** is the one place where it is decided what the menu offers: the rules of `Pin`, `Unpin`,
`FreeUp` and `WebUrl`, asked without doing anything. It changes nothing and opens no file — each
path is reached with `O_PATH` under the resolution rules `Pin` opens it with, and its marks are read
from that — so nothing is downloaded. It is never refused for a path it does not take: such a path
is not in `paths`.

A menu is waiting for the answer, so it waits for nothing that can take long: the selection is
looked at in one pass, and the account's tree store is asked at most one question for the whole
selection — whether a downloaded file in it has a change waiting to be uploaded — through the
store's read-only connection, which does not wait for a sync that is writing. That connection sees
what was last committed; `FreeUp` itself asks the writer, and is the one that refuses.

| Key | Type | Meaning |
|---|---|---|
| `paths` | `as` | the selected paths `Pin` takes, in the order given: what `Pin`, `Unpin` or `FreeUp` is then called with. Left out: a path in no account's folder, one that does not exist, a symbolic link or anything else that is neither a file nor a directory, a file of the user's own, a file whose marks cannot be read, a `.konedrive-*` name and an account's folder itself |
| `always-keep` | `s` | `hidden` (no path is taken); `off`; `on` (every path is pinned, by itself or by a folder above it); `on-locked` (the same, and `Unpin` of `paths` would be refused) |
| `free-up` | `s` | `hidden` (no path is a folder, a downloaded file or one with a pin of its own); `enabled`; `disabled` (`FreeUp` of `paths` would be refused before it changed anything: `free-up-why` says for what) |
| `free-up-why` | `s` | why `free-up` is `disabled`, empty otherwise: `no-helper` (the folder is intercepted and its helper is not connected); `pinned-above` (a folder above keeps a path pinned: `blocked-by`); `unknown` (the daemon cannot tell now whether a downloaded file among the paths has a change waiting to be uploaded: the file cannot be looked up, or the account's sync has not started, or its store cannot be read); `not-uploaded` (one has such a change). One reason, the first that holds, in that order; the last two only for a folder that shows OneDrive |
| `blocked-by` | `s` | the name of the folder above that keeps a path pinned, when that is why `always-keep` is `on-locked` or `free-up-why` is `pinned-above`; empty otherwise |
| `open-online` | `s` | `hidden` (not exactly one path selected, or one that is neither in `paths` nor an account's folder itself); `enabled`; `disabled` (the item has no id: it is not in OneDrive yet) |
| `open-online-path` | `s` | the path `WebUrl` is then called with; empty when hidden |

A selection may span several accounts' folders: each account answers for its own paths, as `Pin`,
`Unpin` and `FreeUp` ask each account before anything changes, and one refusal locks the entry for
the whole selection. Each account finds its own first reason, and `free-up-why` is that of the first
account that refuses, in the order the accounts' paths first come in the selection — not the reason
that comes first in the order of the table. The answer is about the moment it was asked, and covers
what the calls check before they change anything, not what a free-up finds file by file.

`Pin`, `Unpin` and `FreeUp` route every path before anything changes, and their counts are summed
over the accounts; `FreeUp`'s `busy` also counts the files changed here.

### 2.10 The words for the codes

What a code means to a person is written once, in English, in the crate `konedrive-text`
(`crates/konedrive-text`), which knows only the codes (`konedrive-reason`, `konedrive-dbus`):

| Module | The codes | Shown by |
|---|---|---|
| `reasons` | every key of `Reason` and `LocalSkip`: why a change is not uploaded | the window, `konedrivectl` |
| `waits` | every key of `WaitsFor`: what keeps an item on this computer that the folder cannot hold | the window, `konedrivectl` |
| `files` | every name of `Refusal`, for each of four operations on a file: keep on this device, unpin, free up, open in OneDrive; and what the Dolphin plugin says of a call the daemon never answered | the Dolphin plugin, `konedrivectl` |
| `menu` | every `free-up-why` of `Files.Menu`, and the two other tooltips of a disabled entry | the Dolphin plugin |

- **A sentence** is written as the window or the plugin shows it: a capital first letter, a full
  stop, and named places for what is filled in (`{detail}`, `{file}`, `{needs}`). There is one
  sentence for a code, unless the clients name their own controls or say different things: then the
  window or the plugin has one and the command line another. A code may have no sentence: such a
  reason is shown as the daemon stored it, and such a refusal is told as any failure, with the
  daemon's message.
- **`konedrivectl`** reads the catalogue directly and prints its sentences as they are written, but
  for two things of a terminal: of several paths none of which the refusal names it says the
  catalogue's list form (`{files}: one of these …`), and a reason printed inside brackets, behind a
  path, has no full stop.
- **The window and the Dolphin plugin** are built with C++ generated from the catalogue, every
  sentence a literal inside `i18n`, `i18nc` or `i18np`, so translation stays where KDE's tools
  expect it: `app/generated/reasontexts.{h,cpp}` and `dolphin/src/generated/refusaltexts.{h,cpp}`.
  The files are in git, and the C++ build does not run cargo. How a reason is cut into its key and
  what stands behind it is generated too, from `konedrive-reason`'s rule.
- **A new code gets its words or the tests fail.** The crate's tests hold the catalogue against the
  codes, and one test writes the generated files again and fails when one in git differs; the
  pull-request workflow and the release's run them. The codes of `free-up-why` are the daemon's, and
  a test there holds them against the catalogue. The steps are in `CONTRIBUTING.md`, "A new reason
  or refusal".

The catalogue does not hold every sentence a client says: the words for `Overall`'s reasons and for
the reasons of the Not in the Folder page are written in the window and in `konedrivectl`, each on
its own (issue #232).

## 3. The command line

`konedrivectl` talks to the same interfaces.

**Choosing the account.** A command that acts on one account takes the account from the global
option `--account <id | label | email>`, else from the environment variable `KONEDRIVE_ACCOUNT`
(empty counts as unset), else it is the only account there is. The name is matched as an id, as a
label and as an email (label and email in any case). A label is not shaped like an id, so a name
fits two accounts only when one account's label equals another's email, or a hand-edited
`config.toml` gives two accounts colliding names; such a name is refused with exit status 2, listing
each account it fits as `label (id)`, and never taken as the first. A name that fits no account, or
several accounts and none named, stops the command with exit status 2 ("Several accounts: choose one
with --account (Personal, Family)"); with no account at all, it stops with exit status 1 and says
how to add one. The commands marked "—" or "by path" in the table refuse `--account` with exit
status 2 rather than ignore it, and ignore `KONEDRIVE_ACCOUNT`, which is a default for a whole
shell. Every command the CLI suggests names its account with `--account` whenever there are several
accounts or `KONEDRIVE_ACCOUNT` is set, and with several accounts each success line starts with the
account's label.

| Command | Account | Does |
|---|---|---|
| `--version` | — | the command's own build and the daemon's, without starting the daemon |
| `account list` | — | a table of every account in account order: id, label, email, sign-in state, mode, and the folder with its `Folder.State` |
| `account add` | — | `Accounts.SignIn`: adds a new account by signing in. Prints the URL, opens the browser and waits for `SignInFinished` with its sign-in's number, six minutes at most; Ctrl-C and the six minutes cancel it (`Accounts.CancelSignIn`): answered `true`, it ends as cancelled or timed out; answered `false`, the sign-in had ended by itself or its account is being made, and it waits for that `SignInFinished`, with no limit, and says how it ended, so it never says "cancelled" for an account that is then added. A daemon that leaves the bus while it waits ends it at once. Says what the account is called (its email), or that this OneDrive account is already added and under which name, or why it failed; exit status 1 for all but the first |
| `account rename <account> <label>` | — | `Account.SetLabel` |
| `account remove <account>` | — | `Accounts.Remove`, without asking; then says what was deleted and what was kept |
| `account mode [read-only\|read-write] [--force]` | chosen | shows the mode (and `LastError`), or switches it with `Account.SetMode`: read-write opens the browser like `login` and waits, six minutes at most, until `Mode` is `read-write` or `LastError` says why not; read-only is refused while changes wait to be uploaded, unless `--force` |
| `set-client-id <id>` | — | `Accounts.SetClientId`, overriding the built-in client id for every account with the caller's own; a refusal names the accounts signed in or signing in |
| `settings on-metered [pause\|sync]`, `settings on-battery [sync\|power-saver\|pause]` | — | the whole app's hold settings: shows the choice, or changes it for every account (`Accounts.SetPauseOnMetered` — `pause` is on — and `SetOnBattery`) |
| `login` | chosen | signs an account that is there in again: `BeginSignIn`, opens the browser and waits. With no account at all it adds nothing: it is refused, and the refusal names `account add` |
| `logout` | chosen | signs the account out and deletes its token |
| `status` | chosen, or all | the account's sign-in state and mode, and an `Overall:` line: the state of the account as a whole as the daemon decided it (`Folder.Overall`, §2.4), in words by its reason ("warning — changes cannot be uploaded"); with several accounts and none named, every account under its label, the `Client ID:` line once above them |
| `sync register <path>` | chosen | registers a OneDrive folder (needs the helper) |
| `sync register-without-interception <path>` | chosen | the developer's local folder, named after its cost on purpose |
| `sync forget` | chosen | Forget (`Folder.Unregister`) |
| `sync populate-from <dir>` | chosen | fills a local folder from a directory |
| `sync hydrate <path>`, `sync dehydrate <path>`, `sync state <path>` | by path | one file, through `Files` |
| `sync pin`, `sync unpin`, `sync free` `<paths…>` | by path | pinning ([pinning.md](pinning.md) §8), through `Files` |
| `sync menu <paths…>` | by path | what the file manager's menu would offer for these paths together: the answer of `Files.Menu` (§2.9), one line per key (`always-keep: on-locked`); `paths` is its entries, each quoted |
| `sync open <path> [--print]` | by path | `Files.WebUrl`: prints the address of the item's page in OneDrive and, without `--print`, opens it (§10.2) |
| `sync status` | chosen, or all | the folder, the same `Overall:` line as `status`, its state and counts, "Last checked", "On this computer", whether opens are intercepted; for a OneDrive folder, "Local scan:" — `running — 1 234 folders and 45 678 files, of about 50 000 (2 min, after the switch to read-write)`, `last finished 5 min ago (took 40 s)`, `not yet since the daemon started`, or `none — read-only`; "Waiting to download: 1 234 files (48.2 GiB)" (`DownloadLeftCount`, `DownloadLeftBytes`) beside "Waiting to upload"; while the account holds back by itself, "Paused by itself: metered connection" (or "on battery", "power-saver mode") with how to `sync anyway`; with several accounts and none named, every account's folder under its label. The `Helper:` line, with what to do, is printed once, above them |
| `sync skipped` | chosen | what is not in the folder, and why |
| `sync refresh` | chosen | a cycle now |
| `sync activity [--limit N]`, `sync transfers` | chosen | recent events (20 unless told); for each way one line — how many files move now, what is left, its size and about how long, what this run has done, and how fast ("Downloading: 12 now, 1 234 files left (48.2 GiB, about 12 min), 3.1 GiB done, 8.4 MiB/s"; uploads are counted in changes; what is left and done only while anything is left, the time only when known) — the transfer pool ("Pool: 7 of 32 · large files: 1 (4 of 4 streams)": the slots in use of the pool's size — shown as it is when above it, "Pool: 18 of 16 · …" — then the large files and their streams of the limit; ending "— OneDrive asked to wait 30 s" during a `Retry-After`), and the downloads and uploads under way |
| `sync conflicts`, `sync dismiss <rescued path>` | chosen | the conflicts |
| `sync free-up-space` | chosen | frees up every downloaded file not in use |
| `sync outbox [--all]` | chosen | `sync transfers`'s "Uploading:" line first, then `UploadQueue.Changes`: the changes waiting to be uploaded, each with its state and why it waits; the first 50 without `--all` |
| `sync pause [--for <duration>]`, `sync resume` | chosen | `Pause` for `30m`, `2h`, `1d`, `1h30m`…, or until `sync resume`; `Resume` |
| `sync anyway [--all]` | chosen, or — | `SyncAnyway`: syncs now though the account holds back by itself, until the connection, the battery or the power profile changes; `--all` does it for every account that holds back by itself and is not paused by the user |
| `sync ignore [list\|add <pattern>\|remove <pattern>]` | chosen | shows the ignore list (`IgnorePatterns`), or changes it with `SetIgnorePatterns` |
| `sync thumbnails [on\|off]` | chosen | shows the account's thumbnail setting, or changes it (`SetThumbnails`) |
| `sync not-uploaded [--all]` | chosen | `NotUploadedSummary`: each group and its reasons with their counts and sizes, then (`NotUploadedFiles`) the files of the per-file reasons, the first 20 of each; `--all` lists every file of every reason |
| `sync deletes confirm\|restore` | chosen | `ConfirmDeletes` or `RestoreDeletes`: the mass-delete guard's two answers |
| `dev add-account <label>` | — | a development build's only (`dev-tools`); `DevTools.AddAccount`: a signed-out account with no folder, which never has to sign in; prints its id |
| `dev export-access-token --out <file> [--read-write]` | chosen | a development build's only (`dev-tools`); writes an access token of the account to a `0600` file, atomically, never through a symlink: a read-only one, or with `--read-write` one that can change files, which only a read-write test account listed in `write_test_drive_ids` gets |

A browser is opened only when stdout is a terminal and `KONEDRIVE_NO_BROWSER` is not set; the URL is
printed in any case.

The path commands go through `Files`, so the path decides the account. When one is refused, the CLI
reads every account's folder to say where the path is or is not, which is its own view of the
routing rule.

When the daemon refuses, the CLI says what that means for the user's file and what to do, chosen by
the error name. For `sync pin`, `unpin`, `free` and `open` the sentences are the catalogue's
(§2.10), the ones Dolphin shows where there is one for both; most of those of the other commands are
the CLI's own. `sync status` shows a folder in `error` as that, with the last error, and exits with
status 0; the commands that change a folder or a file exit non-zero when the folder is in `error`
afterwards, as `sync register` does when the folder it just bound was not fully recovered. The CLI
resolves only the directory a path is in, never its last component, so a symlink given as a folder
reaches the daemon as a symlink and is refused.

## 4. The window

KOneDrive is a Kirigami application. Its sidebar is headed by an **account switcher**, below which
are six pages about the account chosen there and, after a separator, Settings, which is the whole
app's:

| Page | Shows |
|---|---|
| **Status** | the status line and what needs attention, the folder, "On this computer: …", "Free Up Space…", "Refresh Now", "Open in File Manager", and a card with the helper's instruction while it is not `connected`. For a OneDrive folder: the mode ("Read-only", "Changes upload", or "Read-only for now" for a read-write account whose folder is not writable), the local scan in one line — "Checking local files: 1234 folders and 45678 files, of about 50000 — started 2 min ago, after the switch to read-write" while one runs, "Local files last checked 5 min ago (took 40 s)" once one has finished, "Local files not checked yet" before; no line for a read-only folder — "N changes waiting to upload" with the size to send (to the Activity page), "N changes cannot be uploaded" while any are blocked (to Not Uploaded), a full OneDrive with "Refresh", files too big for the space left, removals the mass-delete guard holds with "Restore Them" and "Delete in OneDrive Too", and "Pause Syncing…" (for 2, 8 or 24 hours, or until resumed) or, while paused, "Paused until 14:00" with "Resume"; while the account holds back by itself (`HeldBack`, [writes.md](writes.md) §11), "Paused: metered connection" (or "on battery", "power-saver mode") with **Sync Anyway** (`SyncAnyway`), beside the user's own pause |
| **Activity** | two mini cards side by side, "Downloading" and "Uploading": each the speed, "N files downloading" (or "uploading": `ActiveDownloads`, `ActiveUploads`, each file once, however many streams it runs), while anything is left that way a line "1 234 files left · 48.2 GiB · about 12 min" (the Uploading card counts changes: "6 changes left · …"; the time only when known) with, smaller, "3.1 GiB done" (`Transfers`' queue totals), and one chart of the last two minutes (one sample a second, kept by the window) with two lines on two scales — speed on the left axis, the files moving that way on the right — each in its own colour, with a small legend ("Speed", and "Files downloading" or "Files uploading"); dimmed with "no transfers" while idle (KQuickCharts); below both, the shared pool once, "Pool: 7 of 32 · large files: 1 (4 of 4 streams)" (`PoolInUse` of `PoolSize`, which it may exceed; `LargeFiles`; `LargeStreams` of `LargeStreamLimit`; the ceiling is not shown), adding "— OneDrive asked to wait 30 s" during a `Retry-After`; then "Downloading now" and "Uploading now" (each file with a progress bar and its size), "Waiting to upload" while the Not Uploaded page lists anything: only its link, "N changes kept back — see Not Uploaded" (N: what that page counts; what is left to upload is in the Uploading card; no row per file), and "Recent" (the newest 50 events; clicking one shows the file in Dolphin) |
| **Conflicts** | each rescued file: the file, where it was, where it is now, when; "Show in Folder" and "Dismiss". A file changed on both sides kept a copy beside it instead: which name is whose, "Show Both" (both files selected in Dolphin) and "Dismiss". The newest 200, then "and N more" naming `konedrivectl sync conflicts`; a changed list is taken in one step (one removal, one insertion and one change, or one reset), never row by row. Always present, with a count badge (the chosen account's) while there are conflicts, and "No conflicts" otherwise |
| **Not in the Folder** | the skipped items and why, in the words `sync skipped` uses: the first 200, then "and N more" naming `konedrivectl sync skipped`. Read when shown, and while shown at most once a second however often `SkippedCount` moves |
| **Not Uploaded** | what stays on this computer and why (`NotUploadedSummary()`), in four groups: "Needs You" (a reason one action fixes: its count, size and button — "Refresh" for a full OneDrive or a file too big for the space left, "Sign In Again" for a sign-in that does not allow writes), "Needs You for Each File" (each reason with its count; opened, its files — `NotUploadedFiles(reason, 20)`, asked only then — a file whose own reason says more than the group's with that reason, OneDrive's own words for a refused one; clicking one shows it in Dolphin; past 20, "and N more" names `konedrivectl sync not-uploaded --all`), "Never Uploaded" (a line per reason with its count) and "Waiting" (one line, "N changes wait and will go up by themselves", its reasons when opened). Read when shown and when a count moves while it is, at most once a second. A count badge while changes are blocked |
| **Account** | the account's name with "Rename…"; the switch "Upload changes made on this computer" (below); for a OneDrive folder, "Thumbnails" below it: the switch "Download thumbnails" (`SetThumbnails`, §8; while off, a line says that Dolphin, with its previews on, downloads a cloud-only file in full to make its preview), showing what the daemon says; sign in or out, the Microsoft account's name, email and quota, and the account's own `LastError`; the folder, with "Choose Folder…" and "Forget Folder"; for a OneDrive folder, "Uploading": this computer's name for copies (`MachineName`, read-only: `machine_name` in `config.toml`) and the ignore list, with "Add" and a remove button per pattern (`SetIgnorePatterns`); and "Remove Account…" |
| **Settings** | "App": "Start at login", "Show download and upload progress", "Show in Places", "Show a tray icon for each account" (§5); "Sync", for every account: the switch "Pause on metered connections" (`Accounts.SetPauseOnMetered`) and the combo box "On battery" — "Sync as usual", "Pause in power-saver mode", "Pause" (`Accounts.SetOnBattery`) — each showing what the daemon says and disabled while it is not running; "Quit KOneDrive" |

The Conflicts and Not in the Folder pages list their rows in a `ListView`, which builds only the
rows in sight, so thousands of entries cost a handful of delegates; both keep a fixed height, since
a page waiting in the window's hidden holder would otherwise take the whole list's height and build
every row.

**The switcher** shows the chosen account's initials, label and email, and opens a menu of every
account, each with its state's icon (the tray's five, §5), then "Sign in…". It is there with a
single account too: it names the account, and it is where "Sign in…" lives. When an account other
than the chosen one needs attention (its state is `warning`), a warning sign on the switcher says
so, so trouble elsewhere is never hidden; another account merely signed out, or without a folder,
does not count. The choice is remembered (`CurrentAccount=<id>` in `konedriverc`). With more than
one account, each page's title names the account ("Status · Personal"), since a narrow window folds
the sidebar away.

**The version line.** The foot of the sidebar, on every page, names the window's build, small and
dimmed: "Version 0.1.1-dev.57 · commit 5254595" (`docs/releasing.md`). When the daemon on the bus
runs another build — its `Version` or `Commit` on `org.konedrive.Accounts` differs — a second line
says "Service: 0.1.1-dev.55 · commit 1a2b3c4 — restart it to use this version" ("Service: an older
version — …" for a daemon without those properties). `konedrivectl --version` says the same from a
terminal.

**After an update.** A newer package replaces the window's program file and restarts the daemon,
but not the window, which would go on as the old program with libraries in memory that no longer
match what is installed. So when the daemon comes back as another build and the window's own program
file is no longer the one it started from, the window ends as "Quit" would and starts the installed
program in its place, in the same process (`SelfRestart`): hidden in the tray if it was hidden,
shown on the same account if it was shown. A window whose file was not replaced — one built by hand
and run against an installed daemon — stays, with the line above. An update of Qt or KDE Frameworks
alone restarts nothing.

**No account yet.** Of the account's pages only Status is available, and it shows "Connect your
OneDrive" with "Sign in…".

**Sign in…** is one call, `Accounts.SignIn`, whose URL opens in the browser with Microsoft's account
picker; there is no client-id field, since konedrive signs in with its own application registration.
The window then waits for `Accounts.SignInFinished` with the number that call answered
([accounts.md](accounts.md) §7.2). `signed-in` names the new account: the window chooses it and the
folder picker opens at once, since a sign-in exists to sync something. Any other outcome made
nothing: cancelled (Cancel calls `Accounts.CancelSignIn` with the number) shows no message, a
OneDrive account that is another account's already gives "This account is already added as …",
naming it, and a failure the daemon's reason; a daemon that leaves the bus in the middle ends the
adding with a message that says so. The waiting row, Cancel and these messages are on the Status
page's card for no account, so they are shown only while there is none.

**Rename…**, on the Account page, checks the name as the daemon checks it before asking, so the
dialog says at once why it will not do.

**Remove Account…** asks first — "Your files stay in `<folder>`. Files that were never downloaded
are left as empty placeholders." — and then calls `Accounts.Remove`. A removal refused for the
helper or for changes waiting to be uploaded is told in its own words.

**The helper** serves every account, so its card shows on the Status page of whichever account is
chosen. Trouble that belongs to no account (`Accounts.LastError`) shows above it.

**Upload changes made on this computer** is the account's mode: on is read-write, and it shows
`Account.Mode`, the mode the account runs in. Turned on, a dialog first says that the browser opens
for a sign-in allowing KOneDrive to change files, and what uploading means; then
`SetMode("read-write", false)`, whose sign-in URL opens as Sign In's does. While that sign-in waits,
the switch says so, with "Copy Sign-In Link" and "Cancel". Turned off,
`SetMode("read-only", false)`; if changes still wait to be uploaded (`PendingUploads`), a dialog
asks whether to turn off without uploading them (the files stay), and then forces it. Two refusals
are told by name — sign in first (`NotSignedIn`), sign in again and allow it (`ModeNotGranted`) —
and any other with the daemon's message.

**Held removals.** The mass-delete guard holds a large delete until the user decides. The window
follows `HeldCount` for the Status page, the tray and the `massDelete` notification; it shows how
many, not which.

**The state is the daemon's.** An account's state, its icon and what needs attention come from
`Folder.Overall` (§2.4): `AccountStatus` takes the state as it is given, and the reason chooses the
words — a fixed line, `Trouble` for `stopped`, or the attention text with its count. Nothing is
worked out from the other properties, and `Folder.LastError` is not read. Two things remain the
window's: "The KOneDrive service is not running", which no daemon can say, and the wording of the
status line.

The **status line** reads, for example, "Up to date · checked 20 s ago", "Listing your OneDrive: N
items so far", "Downloading 3 files", "3 changes waiting to upload", "Paused until 14:00", "Paused:
metered connection", "Signed out of OneDrive", "No OneDrive folder yet", or the error, refreshed
every 10 s while it ages. What needs attention ("1 changed file was moved out of the way") is said
beside it, under "Needs your attention". Trouble that does not stop the folder (`Trouble`) is what
the line says, also while a reason that ranks higher decides the state (deletions held, a pause):
"Cannot reach OneDrive (…); trying again · checked 2 h ago". While `LiveChanges` is `connected`, the
"· checked 20 s ago" suffix becomes "· live". A listing's line takes neither. The window does not
offer the no-interception mode: a folder is registered only through `Folder.Register`.

**Places.** Each account's folder has an entry in Dolphin's Places panel and in file dialogs, named
`OneDrive — <label>` — with one account too, so that a second account renames nothing. The entry is
found again by a tag of its own (`konedrive-account=<id>`), not by its URL: renaming the account
renames it, a new folder moves it, and forgetting the folder or removing the account removes it.
Nothing is added or changed until the daemon and every account have answered. "Show in Places" is
one switch for every account; off, the entries are removed.

**Free Up Space** asks for confirmation, then reports how much it freed and how many files it
skipped as busy. It has no D-Bus timeout, since freeing up a large folder can take long, and the
window shows "Freeing up space…" until the answer. It is disabled without a connected helper.

**One instance.** The app is single-instance through `KDBusService(Unique)`; a second launch shows
the running window. It starts at login, hidden in the tray, through an XDG autostart entry that the
"Start at login" switch writes or removes; the entry itself is the truth, so removing it in System
Settings turns the switch off. With a system tray, closing the window hides it; without one, closing
quits.

## 5. The tray icon

Each account has one of five states, decided by the daemon (`Folder.Overall`, §2.4). With several
accounts and the setting "Show a tray icon for each account" on (as it is unless turned off), each
account has **its own icon** with its own state. Otherwise — one account or none, or the setting
off — there is **one icon**, and it shows the **worst** state across the accounts, in this order.
Choosing the worst is the window's (`AppStatus::rank`): it compares states the daemon gave, across
accounts, which no single account's object knows.

| State | Icon | `Overall` | An account is in it when |
|---|---|---|---|
| needs attention | `state-warning` | `warning` | a sync error, removals the mass-delete guard holds, a conflict, a full OneDrive, a file too big for the space left, a change that cannot be uploaded, a failed update, the helper's trouble, other trouble that does not stop the folder |
| signed out | `state-offline` | `offline` | signing in, signed out, no folder yet, or OneDrive unreachable with nothing else wrong; and the service not running |
| paused | `media-playback-pause` | `paused` | the account is paused, or holds back by itself (a metered connection, the battery) |
| syncing | `state-sync` | `syncing` | a folder starting, a listing, a download or an upload under way, or changes waiting to be uploaded |
| synced | `state-ok` | `ok` | the folder is up to date |

The rows are the daemon's, in the order of §2.4: a paused account with other trouble that does not
stop the folder is paused. With no account at all, the icon is `state-offline`. The helper's trouble
counts against every account with an intercepted folder.

- **Tooltip.** With one account, the window's status line, and what needs attention on a second
  line. With several, one line per account, in account order: "Personal — Up to date · checked 20 s
  ago", "Family — Signed out of OneDrive". An account that needs attention shows why in place of its
  status line.
- **Menu.** "Open OneDrive Folder" with one account; with several, an "Open Folder" submenu of the
  accounts that have a folder. Then "Open KOneDrive", "Refresh Now" — every account whose folder
  shows OneDrive — "Pause Syncing" (for 2, 8 or 24 hours, or until resumed: every such account not
  paused yet), "Resume Syncing" while any account is paused by the user, "Sync Anyway" while any
  account holds back by itself and is not paused by the user — it calls `SyncAnyway` on each such
  account and no other, as `konedrivectl sync anyway --all` does ([writes.md](writes.md) §11) — and
  "Quit".
- **Click.** Shows the window, or hides it when it is the active one; on the account that needs
  attention when exactly one does, and otherwise on the account the window last showed.

An account's own icon is that account's alone:

- **Tooltip.** The account's label as the title, then its status line, and what needs attention on
  a second line. The icons look alike; the tooltip tells them apart.
- **Menu.** The same entries, acting on that account only: "Open OneDrive Folder", "Open KOneDrive"
  (on that account), "Refresh Now", "Pause Syncing", "Resume Syncing", "Sync Anyway" and "Quit".
- **Click.** Shows the window on that account. When the window is the active one, a click hides it
  if it shows that account, and turns it to that account if it shows another.

An account added or removed gains or loses its icon at once, and so does a change of the setting.
The setting is the window's, `TrayIconPerAccount` in `konedriverc`'s `[General]` group, beside "Show
in Places" on the Settings page (§4); the daemon knows nothing of it.

The tray decides nothing by itself: the state is `Overall`'s, and the words are the window's (§4).

## 6. Notifications

Notifications are sent by the **app**, not the daemon, through KNotification, with events defined in
`konedrive.notifyrc` so that each can be configured in System Settings:

| Event | When |
|---|---|
| `signedOut` | the account went from signed in to signed out by itself, and still is two seconds later; not after a sign-out asked for in the window |
| `diskFull` | a download or an update failed for want of disk space |
| `downloadFailed` | a download failed |
| `updateFailed` | a file changed in OneDrive could not be updated here |
| `uploadFailed` | a change made here cannot be uploaded (`upload-failed`): the reason in words — a name OneDrive refuses, a sign-in without the permission — with "Show in Folder"; and, once when `QuotaFull` turns true, "OneDrive is full" |
| `conflict` | "your changed version was moved to …", or for a file changed on both sides "Both are kept: your version as …", with "Show in Folder" |
| `massDelete` | removals the mass-delete guard holds appeared: "N items deleted in … are not deleted in OneDrive yet", with "Restore Them" (`RestoreDeletes`, also what a click on the notification does) and "Delete in OneDrive" (`ConfirmDeletes`) |

Ordinary downloads notify nothing. The first notification of an event notifies at once; more of the
same event within 10 s are sent as one summary ("2 more files could not be downloaded"). A
notification is chosen by the event's kind, not by its wording, with two exceptions: a full disk is
recognised by the exact detail "not enough disk space", and a cycle's "and N more" conflicts by that
detail.

Each account's events notify on their own, and the 10 s summaries are per account and event. With
more than one account a notification's title names the account ("Download failed — Family"); its
text is unchanged, and clicking it opens the window on that account.

The cost of sending them from the app: with the app quit, nothing notifies, and events that happen
meanwhile are never announced later; the window's lists still show everything.

## 7. Download and upload progress in Plasma

The app watches `Transfers.Downloads`. A transfer still running **2 s** after it first appears is
reported to Plasma as a `KJob` through `KUiServerV2JobTracker` — the mechanism Dolphin's own copy
progress uses — titled "Downloading from OneDrive", with the file name, bytes done of total, and
speed. Shorter transfers never show. At most **5** jobs are visible at once; the rest are summed
into one, "and N more files". Each account's `Downloads` is watched on its own: with several
accounts, the cap of 5 and the summary are per account, and a job's title names the account,
"Downloading from OneDrive — Family".

A transfer can leave `Downloads` before the event of its failure arrives. So a job whose transfer
leaves `Downloads` is held for a **1.5 s** grace window: a `failed` or `update-failed` event naming
the same path fails the job with the reason, and otherwise the job finishes as a success. A daemon
that leaves the bus ends every job with an error. The "Show download and upload progress" switch in
Settings is on by default.

**Uploads** are reported the same way, by a second watcher per account on `Uploads`: "Uploading to
OneDrive" (with the account's name when there are several), the same 2 s, cap of 5 and grace window,
and an `upload-failed` event naming the file fails its job with the reason in words.

## 8. Thumbnails

Dolphin draws a preview by opening the file, and opening a placeholder downloads it: scrolling past
a folder of photos would download them all. The Windows client avoids this with thumbnails from the
cloud, and so does konedrive.

**What KIO does** (`docs/kio-behavior.md`): a thumbnail is found by the MD5 of the file's fully
percent-encoded `file://` URI, in the thumbnail cache's `{normal,large,x-large,xx-large}` (128, 256,
512, 1024 px). A cached PNG tagged with `Thumb::URI` and a `Thumb::MTime` equal to the file's time
is drawn **without opening the file** — measured at `normal` and `large`, taken from the naming
scheme for the larger two; a stale time makes KIO discard it and open the file. Listing a folder
opens nothing.

**The filler** (`desktop/thumbs.rs`) runs in the daemon, in the background, as a part of the running
sync:

- for each placed image or video (a file with a cTag, by the item's MIME type) for which the store
  records no thumbnail of what it is now, it asks Graph for one thumbnail, `c512x512`, and writes it
  as a PNG to `x-large`, `large` and `normal`, scaled down where it is larger than the size and
  never up, each tagged with the file's URI and the placeholder's time. The cache itself is not
  looked at: a thumbnail deleted from it is not made again until the file changes;
- it runs after each cycle that succeeded, when thumbnails are turned on, and 10 minutes after it
  last ran, draining in batches of up to 200 items, each request in a background slot of the
  account's transfer pool ([hydration.md](hydration.md) §6.4), like any background download. The
  store picks the candidates in item id order, and a drain ends once every candidate has been looked
  at;
- `thumb_key` in the tree store records the cTag, the path in the folder and the time a thumbnail
  was made for, so a file is fetched again only when its content, its name or place, or its time
  changes (the cache is keyed by URI and checked against the time);
- every answer that settles whether the item has a usable thumbnail is recorded in `thumb_key`, and
  the item is not asked for again until the file changes: a thumbnail written; a 404; any other 4xx
  but `401`, `408` and `429`; a body over 8 MiB; an image that cannot be decoded, or is over 4096 ×
  4096 px or 64 MiB of decoder memory; a thumbnail that cannot be written into the cache here
  (issue #224);
- only a passing trouble is tried again at the next drain: no answer, `401` (a sign-in trouble, not
  the item's), `408`, `429` and `503` (after the pool's throttle wait) and the other 5xx;
- Graph refuses `c512x512` for some items with `406`: the item is then asked once for Graph's named
  size `large`, scaled down the same way, and that answer is final under the same rules. An item
  refused at both sizes has no thumbnail from the cloud.

`xx-large` (1024 px) is **not** filled: it would be a second request per image at roughly four times
the bytes. Where Dolphin asks for that size, KIO makes its own thumbnail, which downloads the file.

**The setting.** Thumbnails are fetched per account, on by default (`Folder.Thumbnails`,
`SetThumbnails`, `sync thumbnails`, the account page's "Download thumbnails"). Off, the filler asks
Graph for nothing while everything else runs, and Dolphin, with its previews on, downloads every
cloud-only image or video it previews in full, as it would any other file; the thumbnails already in
the cache stay. Turned on again, the filler asks at once for every item without a recorded
thumbnail. The filler also asks nothing while the account's background work stops — a pause, or a
hold ([writes.md](writes.md) §11) — and stops a drain under way between two requests; after a
`Resume` it starts again with the next cycle or the 10 minutes.

**What still opens a file.** Type detection is a separate problem that a thumbnail does not solve:
for a name with no extension, or an ambiguous one like `.bin`, KIO reads the first bytes to learn
the type, and it does not consult `user.mime_type`, so no attribute can prevent it (limitations
log K1). An image shown before its thumbnail is filled, a preview of any other kind of file, and the
`xx-large` zoom also open the file. Turning previews off for the folder (View → Show Previews)
avoids those downloads.

## 9. Baloo

KDE's file indexer reads every file's content to index it, and a read of a placeholder downloads it:
left alone, Baloo would download the whole drive in the background the first time it walked the
folder. So a OneDrive folder is **excluded from Baloo**:

- the exclusion is added with `balooctl6 config add excludeFolders` and removed on Forget, and on
  the account's removal, with `config rm`;
- before adding, the daemon checks whether the folder, or a directory above it, is excluded already,
  by reading Baloo's own settings file (`baloofilerc`, `[General]`, `exclude folders`), since
  `balooctl6 config list excludeFolders` was observed printing an empty list while the file held
  exclusions;
- whether *this daemon* added the exclusion is recorded in `config.toml` (`baloo_excluded` in the
  account's `[accounts.root]`), and Forget removes only an exclusion recorded so, never one the user
  had set;
- a registration, and every bring-up of a folder not recorded as excluded, runs the same
  check-then-add, which only ever adds, so an exclusion that failed or timed out, or a Baloo
  installed later, is caught up. One that was added but not recorded before the daemon stopped is
  found in the settings file at the next bring-up and taken for the user's own: a Forget leaves it.
  An exclusion recorded once is not checked again;
- every call of `balooctl6` runs under a 10 s timeout and is killed when it expires; a hung
  `balooctl6` behaves like a missing one, and a registration or a Forget waits for it no longer than
  that.

The cost: no Baloo search inside the folder — KRunner and Dolphin's search do not find files there
(limitations log W14).

## 10. Dolphin plugins

Two plugins in `dolphin/`, both running inside Dolphin, where a crash takes the file manager down
and any `open()` downloads what is shown. Both are tested to never open a file in the sync folder: a
test drives every code path under an inotify watch and fails on any open, and the tests run under a
poisoned allocator so that a use of freed memory cannot hide.

The emblems are read from the marks on the files, by the plugin itself: asking the daemon about
every file Dolphin draws would be too slow. The menu is the daemon's answer, one call per menu
(§10.2).

### 10.1 Emblems

The overlay plugin gives each file and folder an emblem from its marks, read with `lstat` and
`lgetxattr`. The upload mark `user.konedrive.sync` ([writes.md](writes.md) §5.4) is read first, and
decides alone when it is there, for a file and for a folder, pinned or not:

| `user.konedrive.sync` | Emblem |
|---|---|
| `pending` (a change waits to be uploaded), `uploading` | `state-sync` |
| `blocked` (the change cannot be uploaded) | `state-error` |
| absent, or another value | by the state and the pin, below |

Otherwise `user.konedrive.state` and `user.konedrive.pin` decide:

| State | Pinned? | Emblem |
|---|---|---|
| `online-only` | no | `cloudstatus` (a cloud) |
| `online-only` | yes (a pin the sweep has not filled yet) | `state-sync` |
| `hydrating`, `dehydrating` | either | `state-sync` |
| `hydrated` | no | `dialog-ok` (an outline check) |
| `hydrated` | yes | `emblem-checked` (a filled check) |
| a folder | yes (effectively) | `emblem-checked` |
| a folder | no | none |
| outside a root, or an unrecognised state | -- | none |

"Pinned" means effectively pinned: the item itself, or any ancestor up to the root, carries
`user.konedrive.pin`. A file is inside a root when an ancestor directory carries
`user.konedrive.root`; ancestors are resolved with `lstat` and `readlink` per component, never by
opening, so a symlink out of the root does not count as inside. The root answer is cached per
directory while an inotify watch on that directory stays in place, for the 256 most recently shown
directories; without inotify nothing is cached. The pin is read fresh on every call, its walk up
included. `IN_ATTRIB` on the directory reports a change of a child's marks, or of the directory's
own, so emblems update live for a watched directory. Emblems need no daemon: they work with it
stopped.

### 10.2 The context menu

The action plugin adds **Always Keep on This Device**, a checkable action, and **Free Up Space**,
for files and folders and any selection (see [pinning.md](pinning.md)), and **Open in OneDrive**.

**What is offered is the daemon's answer.** Each time a menu is built for a selection with something
in a sync folder, the plugin makes one `Files.Menu(paths)` call (§2.9) with the selection, and sets
its entries from the answer, value by value:

| Answer | Entry |
|---|---|
| `always-keep`: `off`, `on` | "Always Keep on This Device", unchecked or checked |
| `always-keep`: `on-locked` | checked and disabled, with the tooltip "Kept on this device because “`blocked-by`” is." |
| `free-up`: `enabled` | "Free Up Space" |
| `free-up`: `disabled` | disabled, with a tooltip for `free-up-why` (below); none for a reason the plugin does not know |
| `open-online`: `enabled`, `disabled` | "Open in OneDrive"; disabled with the tooltip "Not in OneDrive yet." |
| `hidden`, or a value the plugin does not know | the entry is hidden |

| `free-up-why` | Tooltip of the disabled "Free Up Space" |
|---|---|
| `pinned-above` | "Kept on this device because “`blocked-by`” is; unpin it first." |
| `no-helper` | "The konedrive helper is not connected. Try again once it is — it reconnects on its own." |
| `not-uploaded` | "Not uploaded yet: freeing it up would lose the changes made here." |
| `unknown` | "KOneDrive cannot tell yet whether a change here waits to be uploaded. Try again in a moment." |

The tooltips are written in the catalogue (§2.10, `menu`), each beside the name `FreeUp` would be
refused under where it has one, whose longer sentence is in the same crate.

The plugin decides nothing from the marks: which paths count, what a pin above means and what the
daemon would refuse are the daemon's rules, in one place (`sync/menu.rs`), beside the calls they
restate. Two things remain the plugin's:

- **A check before the call**, by the marks alone: at least one selected path lies in a sync folder,
  or is one (the nearest directory at or above it carries `user.konedrive.root`, resolved as for the
  emblems, §10.1). A right click anywhere else makes no call.
- **The wait, which is the entries' and not the menu's.** The host asks for the entries
  synchronously, and the plugin never blocks it: it hands over the section at once in a waiting
  state — the heading and the three entries, shown, disabled, unchecked — and sends the call
  asynchronously. When the answer comes, each entry is set from it. An error, no daemon, or no
  answer within two seconds hides the entries and the section with them (issue #232). The message
  carries no auto-start, so building a menu never starts the daemon. An answer that comes after its
  menu is gone is dropped with it, and touches nothing. A waiting entry cannot be triggered: it is
  disabled, and has no paths to call the daemon with until the answer gives them.

**What an entry does.** Checking "Always Keep" calls `Pin`; unchecking it calls `Unpin`, which only
removes the pin -- files stay downloaded, as on Windows (D-A in [pinning.md](pinning.md) §1). "Free
Up Space" calls `FreeUp`, for a folder as for a file (D-B there). Each is called with the `paths` of
the answer, not with the selection. A selection is one asynchronous D-Bus call on `Files` with no
reply timeout, since downloads can take minutes; the daemon finds each path's account, so a
selection may span the folders of several accounts ([accounts.md](accounts.md) §3.5). A click starts
a daemon that stopped after the menu was built, through D-Bus activation. A path already waiting (in
an earlier call not yet answered, whichever call it was for, `WebUrl` included) is never sent again,
and at most 1000 paths wait at once per window. Refusals are explained by their error name; a batch
refused because one path is pinned only by an ancestor is explained with the daemon's own words,
which name that path and folder. The actions can be switched off in Dolphin's context-menu settings.

**The section.** Whatever the plugin offers is one section of the menu itself, not a submenu: a
separator whose text is "OneDrive" (`konedrive_section`), the entries, and a closing separator
(`konedrive_section_end`). When the answer offers nothing, the section is hidden with its entries.
Whether the heading is drawn is the widget style's choice.

**Open in OneDrive** (`konedrive_open_online`) is the section's last entry. The daemon offers it
(`open-online`) for exactly one selected path, never for several:

- an item `Pin` takes: enabled when it carries `user.konedrive.item-id`, otherwise disabled with the
  tooltip "Not in OneDrive yet.";
- an account's folder itself: always enabled, and the section's only entry, since the other two are
  not offered there. It opens the root of the drive.

A click is one asynchronous `WebUrl(path)` call on `Files`, with the answer's `open-online-path`,
under the same rules as the other calls. The daemon finds the account, reads the item's id from the
path (opened as `Pin` opens it, so nothing is downloaded), asks Graph for the item, or for the
drive's root for an account's folder itself, and answers its `webUrl`. The address is stored nowhere
and asked for on every click. The daemon opens no browser and changes nothing; the plugin opens the
address with `QDesktopServices::openUrl`, and only an `https` address. Refusals are explained by
name: `NotUploaded` (no item id), `NotSignedIn`, `Unreachable` (OneDrive did not answer; the
sentence is followed by the daemon's message), `OutsideRoot`, `NotManaged`, `NoRoot`, and `Failed`
with the daemon's words (the item is gone from OneDrive, or the answer has no address).
`konedrivectl sync open <path> [--print]` makes the same call and prints the address; without
`--print` it also opens it, under the guard the sign-in page has (§3) and only an `https` address.

## 11. Known limits

Those the limitations log keeps: Dolphin still opens some files itself (K1), and there is no desktop
search inside the folder (W14). Others, in plain words: notifications need the app running (§6);
there are no emblems in search results or Recent Files, which do not use `file://` URLs; the Plasma
side — how the tray, the popups and the job tracker actually render — is not covered by the tests,
which run offscreen on private buses; the window shows one account at a time; and a window or a
Dolphin running across the upgrade to multiple accounts needs a restart. Where a client still
decides something for the daemon is issue #232.

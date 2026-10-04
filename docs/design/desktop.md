# Desktop integration

What sits on top of the daemon: its D-Bus API, the command line, the KOneDrive window and tray
icon, notifications, download progress in Plasma, thumbnails, the Baloo exclusion, and the Dolphin
plugins. The daemon's own work is in [hydration.md](hydration.md) and [sync.md](sync.md).

## 1. Principles

- **Every client talks to the daemon.** The window, the tray, the CLI and the Dolphin menu call
  D-Bus methods and show what the daemon publishes. The window never touches the sync folder
  itself. The Dolphin emblems are the one exception, and they only read an extended attribute.
- **Every feature has a command-line path.** Anything the window can do, `konedrivectl` can do, so
  nothing depends on clicking through a UI, and scripts and tests can drive everything.
- **Refusals have names.** Every refusal is a D-Bus error name; clients match the name, never the
  message, and turn it into a sentence about the user's file and what to do next.
- **Nothing a client does opens a placeholder** (see the invariant in [README.md](README.md)).

## 2. The D-Bus API

### 2.1 Names

Session bus, service `org.konedrive.Daemon`, eleven interfaces on two kinds of object:

| Object | Interfaces |
|---|---|
| `/org/konedrive/Accounts` | `org.konedrive.Accounts` (the accounts, the client id, the helper), `org.konedrive.Files` (the per-file calls, routed by path), `org.freedesktop.DBus.ObjectManager` |
| `/org/konedrive/Accounts/<id>`, one per account | `org.konedrive.Account` (its sign-in); its folder's `org.konedrive.Folder` (the folder itself), `org.konedrive.Transfers` (what moves now), `org.konedrive.UploadQueue` (what waits to go up), `org.konedrive.Conflicts`, `org.konedrive.LocalScan` and `org.konedrive.ActivityLog`; `org.konedrive.TokenExport` |

No name carries a version: the daemon, the window, the Dolphin plugin and `konedrivectl` ship
together in one package, and a member does not repeat its interface's name (`Conflicts.List`, not
`Conflicts.Conflicts`). The definitions are in `dbus/*.xml`, one file per interface, and a test
keeps each in step with the live interface. The daemon is D-Bus activated
(`SystemdService=konedrived.service`), so the first call from any client starts it. Every object is
on the bus before the name is claimed, so no client sees a half-registered daemon. Nothing answers
at `/org/konedrive/Daemon`, the object of the single-account versions ([accounts.md](accounts.md)
§5).

Properties change through `PropertiesChanged`, each under the interface that holds it: one change
of the state that touches several interfaces sends one signal for each. Counters, status and the
transfers are coalesced: at most one signal per interface per 250 ms, so a drive of hundreds of
thousands of items cannot flood the bus.

The manager's interfaces, `Accounts` and `Files`, are in §2.8 and §2.9; each account's in §2.2
to §2.7.

### 2.2 `Account`, per account

| Member | Meaning |
|---|---|
| `Id` (`s`) | the account's id, the last element of its object path: 12 lowercase hexadecimal characters |
| `Label` (`s`) | the account's name ([accounts.md](accounts.md) §2) |
| `Mode` (`s`) | the mode the account runs in, `read-only` or `read-write`: read-write only while `config.toml` says so, the write gate lets it through and its token carries `Files.ReadWrite` ([accounts.md](accounts.md) §10) |
| `State` (`s`) | `signed-out`, `signing-in` or `signed-in` |
| `LastError` (`s`) | the reason for the most recent failure, a sign-in refused as another account's included ([accounts.md](accounts.md) §6.2); empty when none |
| `DisplayName`, `Email` (`s`) | from `GET /me` |
| `QuotaUsed`, `QuotaTotal`, `QuotaRemaining` (`t`), `QuotaState` (`s`) | the account's one quota: bytes used and in all, Graph's `quota.remaining` (never `total - used`) and `quota.state` (`normal`, `nearing`, `critical`, `exceeded`), from `GET /me/drive`, whoever reads it — the account's info (a sign-in, `RefreshInfo`) or the uploads' space check (`Folder.Refresh`, a refused upload, the check every 30 minutes; [writes.md](writes.md) §6.4). Every read updates all four, and what it did not give keeps its last value; between reads the bytes uploaded come off `QuotaRemaining` and are added to `QuotaUsed`. 0 and empty until read; kept in `account.json` across restarts |
| `BeginSignIn() → s url` | starts the loopback listener and returns the authorization URL; the caller opens it ([sync.md](sync.md) §12.1) |
| `CancelSignIn()`, `SignOut()`, `RefreshInfo()` | as named; `SignOut` deletes the refresh token; `RefreshInfo` reads the name, the address and the quota again |
| `SetLabel(s)` | renames the account; `InvalidArgs` for a label the rules refuse |
| `SetMode(s mode, b force) → s sign_in_url` | switches to `read-only` or `read-write` ([accounts.md](accounts.md) §10); the URL of the sign-in a switch to read-write needs, empty when none is needed. Refused `WritesNotAllowed` by the write gate, `NotSignedIn`, `PendingUploads` unless `force`, `InvalidArgs` for another mode. A switch to read-write ends in `Mode` turning `read-write`, or in `LastError` saying why not (limitations log F64) |

Neither the refresh token nor the access token is ever exposed through `Account`; `TokenExport`
(§2.7) is a development build's.

### 2.3 The folder's methods, per account

`Folder`:

| Method | Does |
|---|---|
| `Register(s path)` | binds an empty folder to the account's drive, with the helper intercepting ([hydration.md](hydration.md) §14.1); refused `Overlaps` for a folder that is, is inside, or contains another account's, and `NotEmpty` for one that carries another account's drive ([accounts.md](accounts.md) §6.3) |
| `RegisterWithoutInterception(s path)` | the developer's local folder, with nothing intercepting ([hydration.md](hydration.md) §14.3); refused `Overlaps` in the same way |
| `Unregister()` | Forget: leaves every file as it is ([hydration.md](hydration.md) §14.5) |
| `PopulateFromDirectory(s source_dir) → t created` | fills a local folder with placeholders mirroring a directory; refused on a OneDrive folder |
| `Refresh()` | runs a sync cycle now, tries the changes in backoff, and reads the quota again (which may end a full OneDrive); refused `NoHelper` while the folder waits for the helper |
| `Skipped() → a(ss)` | (path, reason) for everything in OneDrive that is not in the folder ([sync.md](sync.md) §7.5) |
| `FreeUpSpace() → (u files, t bytes, u busy)` | frees up every downloaded file that is not in use; files open somewhere or busy with a download are skipped and counted, never waited for; a file whose change waits to be uploaded is left, and counted as busy |
| `Pause(u seconds)`, `Resume()` | pause the account — no upload, no poll, no thumbnails; fills on open, `Hydrate` and detection go on — for `seconds`, or until `Resume` when 0; the pause outlasts a daemon restart |
| `SetIgnorePatterns(as)` | the names of the user's own files that are never uploaded (shell globs on a name); written to `config.toml`, then the whole folder is scanned again; `InvalidArgs` for an empty pattern or one holding "/" |
| `SyncAnyway()` | lifts the automatic hold ([writes.md](writes.md) §11) of this account now, until a source or the app's `pause_on_metered` / `on_battery` (`Accounts.SetPauseOnMetered`, `SetOnBattery`, §2.8) changes; not kept across a restart; `Unsupported` for a folder not connected to OneDrive |
| `SetThumbnails(b)` | the account's own sync setting (§8): whether Graph's thumbnails are fetched. Written to the account's section of `config.toml` (`thumbnails`) and taken at once; `Unsupported` for a folder not connected to OneDrive. When the account holds back by itself is the whole app's setting, on `Accounts` (§2.8) |

`UploadQueue`:

| Method | Does |
|---|---|
| `Changes(u limit) → a(tsssttsx)` | the changes waiting to be uploaded, oldest first (0: all): seq, kind (`create`, `mkdir`, `update`, `move`, `delete`, `move-out`), path, state (`waiting`, `ready`, `running`, `retry`, `blocked`, `held`; while the account is paused, every row but a blocked or held one reads `paused`, with no reason and no next try), bytes sent, bytes in all, reason, next try; `Unsupported` for a folder not connected to OneDrive |
| `ConfirmDeletes() → u`, `RestoreDeletes() → u` | the mass-delete guard's two answers: the held removals go ahead, or are dropped and the items placed again; how many rows |
| `NotUploaded() → a(ss)` | (path, reason) for what stays on this computer: what is never uploaded (`symlink`, `hard-link`, `not-downloaded`, …) and every blocked change (`name-characters`, `forbidden`, …) |
| `NotUploadedSummary() → a(ssut)` | what is kept back, one row per reason: group, reason, count, bytes. Groups, in order: `one-action` (one action fixes every file: `waiting-for-space`, which while OneDrive is full also counts every change that sends content; `too-big`, every `too-big:<needed>:<free>` summed as one; `forbidden`; and `quota-exceeded` from an older version, until a start converts it), `per-file` (a name OneDrive refuses, `too-large`, `refused` — every `refused: <message>` summed as one, as is every reason with a detail behind its key, `<key>: <detail>`; and every blocked change whose reason belongs to no other group, whatever the reason), `never` (symlinks, pipes, sockets, devices, `other-device`, `reserved-name`, `hard-link`), `waiting` (goes up by itself: open for writing, locked, …, and any reason the daemon does not know, of a change that is not blocked). Everything `NotUploaded` lists, plus the changes waiting or in backoff with a reason, and the `ready` ones waiting for space; held removals are not. The table is `crates/konedrive-reason/src/lib.rs`; `crates/konedrived/src/upload/kept_back.rs` adds the row's state |
| `NotUploadedFiles(s reason, u limit) → (a(ss) items, u total)` | the files of one reason as the summary names it, by path, at most `limit` (0: all): (path, reason as stored — a refused one keeps OneDrive's message); and how many there are |

`Conflicts`:

| Method | Does |
|---|---|
| `List() → a(xsss)` | (time, original path, path of the kept version, how it was kept: `rescued` or `copy`) ([sync.md](sync.md) §10.3) |
| `Dismiss(s rescued_path)` | takes one conflict off the list; the file stays where it is |

`ActivityLog`:

| Method | Does |
|---|---|
| `Recent(u limit) → a(xsss)` | (time, kind, path, detail), newest first |

### 2.4 The folder's properties and signals, per account

`Folder`:

| Property | Meaning |
|---|---|
| `Path` (`s`) | the account's folder, empty when it has none. A folder `config.toml` records has its path from the daemon's start, while `State` may still be `none`: it is not brought up yet |
| `State` (`s`) | `none`, `listing`, `ready`, `no-interception` or `error` (§2.5) |
| `Source` (`s`) | `onedrive`, `local`, or empty ([sync.md](sync.md) §3) |
| `LastError` (`s`) | what needs attention, in words: the registration's trouble and the sync's, joined; while the folder waits for the helper, it begins with the helper's advice (§2.5) |
| `ItemsListed`, `ItemsPlaced`, `SkippedCount` (`t`) | the listing's progress ([sync.md](sync.md) §7.5) |
| `LastChecked` (`x`) | Unix time of the last successful cycle; 0 for never |
| `LocalBytes` (`t`) | the space the folder's files take on disk (`st_blocks × 512`), measured by a walk after each cycle and at most every 5 s after a download or free-up |
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
| `ActiveDownloads` (`u`), `ActiveUploads` (`u`) | the files downloading and uploading now: the entries of `Downloads` and `Uploads`, each file once however many streams it runs (no thumbnail is a file downloading) — not slots (issue #50) |
| `PoolInUse` (`u`), `PoolSize` (`u`), `PoolCeiling` (`u`) | the account's transfer pool ([hydration.md](hydration.md) §6.4): every slot held now, of all four classes (downloads, uploads, metadata operations, files being opened) with the opens' reserve, so it may be above the size (an open's reserve; slots still held after a throttle halved the pool); the pool's size now; and its ceiling (`[transfers] max`) |
| `LargeFiles` (`u`), `LargeStreams` (`u`), `LargeStreamLimit` (`u`) | the large files (100 MiB and up) the sync moves now, each once however many streams it runs, files being opened left out; the streams of large sync transfers under way (a download in parts runs several; a file being opened is never one); and how many such streams may run at once (`[transfers] large`) |
| `RetryAfter` (`u`) | the seconds left of OneDrive's `Retry-After` wait, during which no transfer starts (0: none) |

The speeds, the pool and `RetryAfter` are updated once a second while anything moves or a
`Retry-After` runs; the file counts with the lists they count.
| `DownloadLeftCount` (`u`), `DownloadLeftBytes` (`t`), `DownloadDoneBytes` (`t`), `DownloadTimeLeft` (`u`), and the same four for uploads | the queue totals (issue #16): files left to download and changes left to upload, their bytes less what is moved of those under way, the bytes done since nothing was last left that way, and the seconds left at the speed of the last 30 s (0: unknown) |

`UploadQueue`:

| Property | Meaning |
|---|---|
| `PendingCount` (`u`), `PendingBytes` (`t`) | the changes waiting to be uploaded, neither blocked nor held, and the size of what they send |
| `BlockedCount` (`u`) | the changes that need the user before they can go up (see `NotUploaded`); removals the mass-delete guard holds are not counted |
| `HeldCount` (`u`) | the removals the mass-delete guard holds, waiting for `ConfirmDeletes` or `RestoreDeletes`; `held` rows in `Changes()` |
| `QuotaFull` (`b`) | OneDrive is full: no content goes up until a quota read finds space ([writes.md](writes.md) §6.4) |
| `QuotaWaitingCount` (`u`), `QuotaWaitingBytes` (`t`) | while full, the changes that send content, and the size of their files |
| `TooBigCount` (`u`) | files refused as too big for the space left; each is `ready` in `Changes()` with reason `too-big:<needed>:<free>` |

`Conflicts`:

| Property | Meaning |
|---|---|
| `Count` (`u`) | how many conflicts are listed |
| `MachineName` (`s`) | the name copies of files changed on both sides are named after ("Report-`<MachineName>`.docx"): `machine_name` in `config.toml`, or the host's name |

`LocalScan`:

| Property | Meaning |
|---|---|
| `State` (`s`), `Reason` (`s`), `Started` (`x`), `Directories` (`t`), `Files` (`t`), `Expected` (`t`), `Finished` (`x`), `Took` (`u`) | the Full local scan of a read-write folder ([writes.md](writes.md) §3.1): `running`, `idle`, or `none` for a read-only folder; why it runs (`start`, `read-write`, `helper-back`, `overflow`, `ignore-list`, `periodic`); when it started; the directories and the files (every entry that is not a directory) it has seen so far; about how many items it will see — the items the base had placed when it started, not the disk's count, so never a percentage; when the last one finished (0: none since the daemon started) and how long it took, in seconds. While idle, the reason, start and counts are the last scan's. The small examinations after each change are not reported. Updated at most once a second while a scan runs, and once when it ends |

The signal `ActivityLog.Added(x time, s kind, s path, s detail)` announces each event as it is
recorded.
The kinds are `downloaded`, `freed`, `added`, `updated`, `removed`, `moved`, `listed`, `conflict`,
`failed` (a download) and `update-failed` (a changed file could not be replaced here). For a full
disk, the detail is exactly "not enough disk space" in either failure kind. A read-write account
adds `uploaded`, `cloud-moved` (detail: where it was), `cloud-deleted` (to OneDrive's recycle bin),
`upload-failed` (detail: the reason's code, once per change and reason), `restored` and
`not-uploaded` (made here and removed here before its upload finished; detail: why); a copy of a
file changed on both sides is a `conflict` whose detail is the copy, beside the file.

The daemon keeps the newest **200** events in the tree store; a Forget or a rebuild drops them. The
log is a summary, not a record of every file: a first listing or a Full reconcile is one `listed`
event ("12 345 items"), an incremental cycle logs at most 50 events of each kind plus one "and N
more", and `FreeUpSpace` is one `freed` event (limitations log F25).

### 2.5 `Folder.State` and `HelperState`

`Folder.State` is computed, never stored — one state, one source of truth:

- `none` — no folder is registered;
- `error` — the registration or the sync is in trouble: signed out, another account than the store
  was built from, an unusable store, a failed recovery, a folder waiting for the helper, or an
  account held back for colliding with another in `config.toml` ([accounts.md](accounts.md) §4.1);
  `LastError` says which;
- `listing` — a first or post-`410` listing is under way (only ever in place of `ready`);
- `ready` or `no-interception` — the registration's own mode.

`HelperState` says what the daemon knows of the helper. It is on `Accounts`, not on each account,
because one link serves every account ([accounts.md](accounts.md) §3.3). While the daemon holds the
link, `connected`. Otherwise it asks systemd — read-only, over the system bus, with no privilege —
for `konedrive-helper.service`: not found is `not-installed`; inactive is `stopped`; failed, or a
unit that cannot be loaded, is `failed`; no system bus, no systemd, or a unit systemd says is
running while the daemon has no link yet, is `unknown`. It is asked again when the link drops or
returns and every 30 s while there is none. The sentence for each state — how to install, start or
diagnose the helper — is written once, in `konedrive_dbus::HelperState::advice`, for the daemon's
`LastError`, the CLI and the window alike; each account whose folder waits for the helper begins
its `Folder.LastError` with it.

### 2.6 Errors

Every refusal is an error name under `org.konedrive.Error`: `NotSignedIn`, `AlreadyRegistered`,
`NoHelper`, `NotEmpty`, `Unsupported`, `NoRoot`, `NoSource`, `OutsideRoot`, `NotManaged`,
`NotHydrated`, `ModifiedLocally`, `InUse`, `NoConflict`, `NotAllowed` (a free-up of something a pin
keeps, [pinning.md](pinning.md) §5), `Overlaps` (a folder that is, is inside, or contains another
account's; the message names that account), `NoAccount` (`Remove` of a path that names no account),
`WritesNotAllowed` (the write gate refuses read-write for this account), `ModeNotGranted` (the
account's token does not carry `Files.ReadWrite`), `PendingUploads` (a switch to read-only while
changes wait to be uploaded), and `Failed` for everything without a name of its own (an I/O failure). Registration refusals come
in the order `NotSignedIn`, `AlreadyRegistered`, `NoHelper`, `Overlaps`, then the folder checks.
`Add`, `SetLabel` and `SetClientId` refuse a label or an id with the bus's own `InvalidArgs`, and
`Accounts` refuses with the bus's `Failed` a call that is not possible now — a client id changed
while an account is signed in, or anything while `config.toml` cannot be read: nothing needs to
tell those reasons apart.

### 2.7 `TokenExport`, per account, in a development build

Served only by a daemon built with the `dev-tools` feature (`scripts/dev-install.sh`); the
released package has no such interface (limitations log W11). `ReadOnly() → s` returns an access token of the account for a test run in the VM — about an
hour of `Files.Read` on that account's drive, whatever its mode, never the refresh token
([sync.md](sync.md) §12.2). `ReadWrite() → s`, for the test-account harness only, returns
one that can change files: refused `WritesNotAllowed` for an account the write gate does not let
through, `ModeNotGranted` for one that is not read-write.

### 2.8 `Accounts`

| Member | Meaning |
|---|---|
| `Accounts` (`ao`) | every account's object, in the order the accounts were added |
| `ClientId` (`s`) | the application id every account signs in with; konedrive's own built-in one unless `SetClientId` overrode it |
| `HelperState` (`s`) | `connected`, `not-installed`, `stopped`, `failed` or `unknown`: one helper serves every account (§2.5) |
| `LastError` (`s`) | trouble that belongs to no account: `config.toml` cannot be read or was written by a newer version, a migration step failed, an account could not be loaded; empty when none |
| `Add(s label) → o` | adds a signed-out, read-only account with no folder and returns its object ([accounts.md](accounts.md) §7.2); `InvalidArgs` for a label the rules refuse |
| `Remove(o account)` | forgets the account's folder as `Folder.Unregister` does, signs it out, deletes its refresh token, cached name and quota and tree store, and takes its object off the bus; the folder's files and the rescued files stay ([accounts.md](accounts.md) §7.3) |
| `SetClientId(s)` | overrides the built-in client id with one of the caller's own (a custom Entra registration); validates and stores it, `InvalidArgs` for a malformed one, and refused while any account is signing in or signed in |
| `PauseOnMetered` (`b`), `OnBattery` (`s`) | when every account holds back by itself ([writes.md](writes.md) §11): on a metered connection or not; on battery `sync`, `power-saver` or `pause`. The top-level `pause_on_metered` and `on_battery` of `config.toml`; absent, `true` and `power-saver` (an `on_battery` the daemon does not know reads `power-saver`, with a warning in the log). Both announced with `PropertiesChanged` |
| `SetPauseOnMetered(b)`, `SetOnBattery(s)` | change them for every account at once: written to `config.toml` under its lock, taken by every account's hold, and every account's `SyncAnyway` ends; `InvalidArgs` for an `on_battery` choice other than the three |

The same object is an `org.freedesktop.DBus.ObjectManager`: `InterfacesAdded` when an account's
object is on the bus, `InterfacesRemoved` when it goes, and `GetManagedObjects` for tools. The
window and the CLI follow `Accounts` instead.

### 2.9 `Files`

The calls on one file or on chosen paths, each routed by path to the account whose folder holds it
([accounts.md](accounts.md) §3.5). A path in no account's folder is refused `OutsideRoot`.

| Method | Does |
|---|---|
| `Hydrate(s path)` | downloads one file now ([hydration.md](hydration.md) §6.5) |
| `Dehydrate(s path)` | frees one file up ([hydration.md](hydration.md) §8) |
| `ItemState(s path) → s` | `online-only`, `hydrating`, `hydrated`, `dehydrating` or `not-managed`, read from the attribute by name (`lgetxattr`), never by opening the file; `not-managed` also for a path in no account's folder |
| `Pin(as paths) → u queued` | "Always keep on this device" ([pinning.md](pinning.md) §3) |
| `Unpin(as paths) → u unpinned` | takes each path's own pin off ([pinning.md](pinning.md) §5) |
| `FreeUp(as paths) → (u files, t bytes, u busy, u skipped_pinned)` | "Free up space" ([pinning.md](pinning.md) §5) |
| `WebUrl(s path) → s url` | "Open in OneDrive": the address of the item's page in OneDrive's web interface, asked from Graph each time (§10.2) |

`WebUrl` alone also answers for an account's folder itself, with the address of the drive's root;
every other method treats that path as in no account's folder.

`Pin`, `Unpin` and `FreeUp` route every path before anything changes, and their counts are summed
over the accounts.

## 3. The command line

`konedrivectl` talks to the same interfaces.

**Choosing the account.** A command that acts on one account takes the account from the global
option `--account <id | label | email>`, else from the environment variable `KONEDRIVE_ACCOUNT`,
else it is the only account there is. The name is matched as an id, as a label and as an email
(label and email in any case). A label is not shaped like an id, so a name fits two accounts only
when one account's label equals another's email — possible now that a label may equal an email —
or a hand-edited `config.toml` gives two accounts colliding labels; such a name is refused with
exit status 2, listing each account it fits as `label (id)`, and never taken as the first. With
several accounts and none named, the command stops with exit status 2 and lists the labels ("Several
accounts: choose one with --account (Personal, Family)"); with no account at all, it stops with
exit status 1 and says how to add one. The commands that name no chosen account — the path
commands, `account …` and `set-client-id` — refuse `--account` with exit status 2 rather than
ignore it, and ignore `KONEDRIVE_ACCOUNT`, which is a default for a whole shell (limitations log
F51).

| Command | Account | Does |
|---|---|---|
| `account list` | all | a table of every account in account order: id, label, email, sign-in state, mode, and the folder with its `Folder.State` |
| `account add <label>` | — | `Accounts.Add`: a signed-out account with no folder; prints its id |
| `account rename <account> <label>` | the argument | `Account.SetLabel` |
| `account remove <account>` | the argument | `Accounts.Remove`, without asking; then says what was deleted and what was kept |
| `account mode [read-only\|read-write] [--force]` | chosen | shows the mode (and `LastError`), or switches it with `Account.SetMode`: read-write opens the browser like `login` and waits until `Mode` is `read-write` or `LastError` says why not; read-only is refused while changes wait to be uploaded, unless `--force` |
| `set-client-id <id>` | — | `Accounts.SetClientId`, overriding the built-in client id for every account with the caller's own; a refusal names the accounts still signed in |
| `settings on-metered [pause\|sync]`, `settings on-battery [sync\|power-saver\|pause]` | — | the whole app's hold settings: shows the choice, or changes it for every account (`Accounts.SetPauseOnMetered` — `pause` is on — and `SetOnBattery`) |
| `login` | chosen | `BeginSignIn`, opens the browser and waits. With no account at all and none named, it first adds one called `Personal` |
| `logout` | chosen | signs the account out and deletes its token |
| `status` | chosen, or all | the account's sign-in state and mode; with several accounts and none named, every account under its label, the `Client ID:` line once above them |
| `sync register <path>` | chosen | registers a OneDrive folder (needs the helper) |
| `sync register-without-interception <path>` | chosen | the developer's local folder, named after its cost on purpose |
| `sync forget` | chosen | Forget |
| `sync populate-from <dir>` | chosen | fills a local folder from a directory |
| `sync hydrate <path>`, `sync dehydrate <path>`, `sync state <path>` | by path | one file, through `Files` |
| `sync pin`, `sync unpin`, `sync free` `<paths…>` | by path | pinning ([pinning.md](pinning.md) §8), through `Files` |
| `sync status` | chosen, or all | the folder, its state, source and counts, "Last checked", "On this computer", whether opens are intercepted; for a OneDrive folder, "Local scan:" — `running — 1 234 folders and 45 678 files, of about 50 000 (2 min, after the switch to read-write)`, `last finished 5 min ago (took 40 s)`, `not yet since the daemon started`, or `none — read-only`; "Waiting to download: 1 234 files (48.2 GiB)" (`DownloadLeftCount`, `DownloadLeftBytes`) beside "Waiting to upload"; while the account holds back by itself, "Paused by itself: metered connection" (or "on battery", "power-saver mode") with how to `sync anyway`; with several accounts and none named, every account's folder under its label. The `Helper:` line, with what to do, is printed once, above them |
| `sync skipped` | chosen | what is not in the folder, and why |
| `sync refresh` | chosen | a cycle now |
| `sync activity [--limit N]`, `sync transfers` | chosen | recent events; for each way one line — how many files move now, what is left, its size and about how long, what this run has done, and how fast ("Downloading: 12 now, 1 234 files left (48.2 GiB, about 12 min), 3.1 GiB done, 8.4 MiB/s"; uploads are counted in changes; what is left and done only while anything is left, the time only when known) — the transfer pool ("Pool: 7 of 32 · large files: 1 (4 of 4 streams)": the slots in use of the pool's size — shown as it is when above it, "Pool: 18 of 16 · …" — then the large files and their streams of the limit; ending "— OneDrive asked to wait 30 s" during a `Retry-After`), and the downloads and uploads under way |
| `sync conflicts`, `sync dismiss <rescued path>` | chosen | the conflicts |
| `sync free-up-space` | chosen | frees up every downloaded file not in use |
| `sync outbox [--all]` | chosen | `sync transfers`'s "Uploading:" line first, then `UploadQueue.Changes`: the changes waiting to be uploaded, each with its state and why it waits; the first 50 without `--all` |
| `sync pause [--for <duration>]`, `sync resume` | chosen | `Pause` for `30m`, `2h`, `1d`, `1h30m`…, or until `sync resume`; `Resume` |
| `sync anyway` | chosen | `SyncAnyway`: syncs now though the account holds back by itself, until the connection, the battery or the power profile changes |
| `sync ignore [list\|add <pattern>\|remove <pattern>]` | chosen | shows the ignore list (`IgnorePatterns`), or changes it with `SetIgnorePatterns` |
| `sync thumbnails [on\|off]` | chosen | shows the account's thumbnail setting, or changes it (`SetThumbnails`) |
| `sync not-uploaded [--all]` | chosen | `NotUploadedSummary`: each group and its reasons with their counts and sizes, then (`NotUploadedFiles`) the files of the per-file reasons, the first 20 of each; `--all` lists every file of every reason |
| `sync deletes confirm\|restore` | chosen | `ConfirmDeletes` or `RestoreDeletes`: the mass-delete guard's two answers |
| `dev export-access-token --out <file> [--read-write]` | chosen | a development build's only (`dev-tools`); writes an access token of the account to a `0600` file, atomically, never through a symlink: a read-only one, or with `--read-write` one that can change files, which only a test account the write gate lets through gets |

The path commands go through `Files`, so the path decides the account. When one is refused
`OutsideRoot`, the CLI reads every account's folder to say where the path is not, which is its own
view of the routing rule (limitations log F50).

When the daemon refuses, the CLI says what that means for the user's file and what to do, chosen by
the error name. `sync status` never prints `error` so that it looks like success, and
`sync register` exits non-zero if the folder it just bound was not fully recovered. The CLI resolves
only the directory a path is in, never its last component, so a symlink given as a folder reaches
the daemon as a symlink and is refused.

## 4. The window

KOneDrive is a Kirigami application. Its sidebar is headed by an **account switcher**, below which
are six pages about the account chosen there and, after a separator, Settings, which is the whole
app's:

| Page | Shows |
|---|---|
| **Status** | the status line, the folder and its item count, "On this computer: …", "Free Up Space…", "Refresh Now", "Open in File Manager", and a card with the helper's instruction while it is not `connected`. For a OneDrive folder: the mode ("Read-only" or "Changes upload"), the local scan in one line — "Checking local files: 1234 folders and 45678 files, of about 50000 — started 2 min ago, after the switch to read-write" while one runs, "Local files last checked 5 min ago (took 40 s)" once one has finished, "Local files not checked yet" before; no line for a read-only folder — "N changes waiting to upload" with the size to send (to the Activity page), "N changes cannot be uploaded" while any are blocked (to Not Uploaded), removals the mass-delete guard holds with "Restore Them" and "Delete in OneDrive Too", and "Pause Syncing…" (for 2, 8 or 24 hours, or until resumed) or, while paused, "Paused until 14:00" with "Resume"; while the account holds back by itself (`HeldBack`, [writes.md](writes.md) §11), "Paused: metered connection" (or "on battery", "power-saver mode") with **Sync Anyway** (`SyncAnyway`), beside the user's own pause, which is shown as before |
| **Activity** | two mini cards side by side, "Downloading" and "Uploading": each the speed, "N files downloading" (or "uploading": `ActiveDownloads`, `ActiveUploads`, each file once, however many streams it runs), while anything is left that way a line "1 234 files left · 48.2 GiB · about 12 min" (the Uploading card counts changes: "6 changes left · …"; the time only when known) with, smaller, "3.1 GiB done" (`Transfers`' queue totals, issue #16), and one chart of the last two minutes (one sample a second, kept by the window) with two lines on two scales — speed on the left axis, the files moving that way on the right — each in its own colour, with a small legend ("Speed", and "Files downloading" or "Files uploading"); dimmed with "no transfers" while idle (KQuickCharts); below both, the shared pool once, "Pool: 7 of 32 · large files: 1 (4 of 4 streams)" (`PoolInUse` of `PoolSize`, which it may exceed; `LargeFiles`; `LargeStreams` of `LargeStreamLimit`; the ceiling is not shown), adding "— OneDrive asked to wait 30 s" (counting down) during a `Retry-After`; then "Downloading now" and "Uploading now" (each file with a progress bar and its size), "Waiting to upload" while the Not Uploaded page lists anything: only its link, "N changes kept back — see Not Uploaded" (N: what that page counts; what is left to upload is in the Uploading card; no row per file), and "Recent" (the newest 50 events; clicking one shows the file in Dolphin) |
| **Conflicts** | each rescued file: the file, where it was, where it is now, when; "Show in Folder" and "Dismiss". A file changed on both sides kept a copy beside it instead: which name is whose, "Show Both" (both files selected in Dolphin) and "Dismiss". The newest 200, then "and N more" naming `konedrivectl sync conflicts`; a changed list is taken in one step (one removal, one insertion and one change, or one reset), never row by row. Always present, with a count badge (the chosen account's) while there are conflicts, and "No conflicts" otherwise |
| **Not in the Folder** | the skipped items and why, in the same words as `sync skipped` (a test keeps the two in step): the first 200, then "and N more" naming `konedrivectl sync skipped`. Read when shown, and while shown at most once a second however often `SkippedCount` moves |

The Conflicts and Not in the Folder pages list their rows in a `ListView`, which builds only the
rows in sight, so thousands of entries cost a handful of delegates; both keep a fixed height, since
a page waiting in the window's hidden holder would otherwise take the whole list's height and build
every row (issue #39, limitations log F168, F170).
| **Not Uploaded** | what stays on this computer and why (`NotUploadedSummary()`), in four groups: "Needs You" (a reason one action fixes: its count, size and button — "Refresh" for a full OneDrive, "Sign In Again" for a sign-in that does not allow writes), "Needs You for Each File" (each reason with its count; opened, its files — `NotUploadedFiles(reason, 20)`, asked only then — each with its reason, OneDrive's own words for a refused one; clicking one shows it in Dolphin; past 20, "and N more" names `konedrivectl sync not-uploaded --all`), "Never Uploaded" (a line per reason with its count) and "Waiting" (one line, "N changes wait and will go up by themselves", its reasons when opened). Read when shown and when a count moves while it is, at most once a second. A count badge while changes are blocked |
| **Account** | the account's name with "Rename…"; the switch "Upload changes made on this computer" (below); for a OneDrive folder, "Thumbnails" below it: the switch "Download thumbnails" (`SetThumbnails`, §8; while off, a line says that Dolphin, with its previews on, downloads a cloud-only file in full to make its preview), showing what the daemon says; sign in or out, the Microsoft account's name, email and quota; the folder, with "Choose Folder…" and "Forget Folder"; for a OneDrive folder, "Uploading": this computer's name for copies (`MachineName`, read-only: `machine_name` in `config.toml`) and the ignore list, with "Add" and a remove button per pattern (`SetIgnorePatterns`); and "Remove Account…" |
| **Settings** | "App": "Start at login", "Show download and upload progress", "Show in Places"; "Sync", for every account: the switch "Pause on metered connections" (`Accounts.SetPauseOnMetered`) and the combo box "On battery" — "Sync as usual", "Pause in power-saver mode", "Pause" (`Accounts.SetOnBattery`) — each showing what the daemon says and disabled while it is not running; "Quit KOneDrive" |

**The switcher** shows the chosen account's initials, label and email, and opens a menu of every
account, each with its state's icon (the tray's four, §5), then "Sign in…". It is there with a
single account too: it names the account, and it is where "Sign in…" lives. When an account
other than the chosen one needs attention, a warning sign on the switcher says so, so trouble
elsewhere is never hidden; another account merely signed out, or without a folder, does not count.
The choice is remembered (`CurrentAccount=<id>` in `konedriverc`). With more than one account,
each page's title names the account ("Status · Personal"), since a narrow window folds the sidebar
away (limitations log A13).

**The version line.** The foot of the sidebar, on every page, names the window's build, small and
dimmed: "Version 0.1.1-dev.57 · commit 5254595" (the commit's first 7 characters;
`docs/releasing.md`). When the daemon on the bus runs another build — its `Version` or `Commit`
on `org.konedrive.Accounts` differs, read once each time it appears on the bus — a second line in
the neutral (warning) colour says "Service: 0.1.1-dev.55 · commit 1a2b3c4 — restart it to use this
version" ("Service: an older version — …" for a daemon without those properties). No second line
while the daemon is not on the bus. Both lines can be selected and copied.
`konedrivectl --version` says the same from a terminal.

**No account yet.** Only the Status page is available, and it shows "Connect your OneDrive" with
"Sign in…".

**Sign in…** opens the Microsoft sign-in in the browser straight away, with the account picker:
there is no client-id field or dialog, since konedrive signs in with its own built-in application
registration. It makes the account, signed in and named by its own doing: `Add` with a temporary
label, `BeginSignIn`, whose URL opens in the browser, and, once the sign-in succeeds, `SetLabel`
with the account's email ([accounts.md](accounts.md) §7.2; limitations log A15). The account stays
out of the switcher, the tray, Places and notifications until then; if the sign-in is cancelled,
fails, the dialog is closed, or the email is already another account's, it is removed and nothing
is left — an "already added" account shows a message saying so, rather than being renamed. Once
named, it is chosen and the folder picker opens at once: a sign-in exists to sync something.

**Rename…**, on the Account page, checks the name as the daemon checks it before asking, so the
dialog says at once why it will not do (A14).

**Remove Account…** asks first — "Your files stay in `<folder>`. Files that were never downloaded
are left as empty placeholders." — and then calls `Accounts.Remove`.

**The helper** serves every account, so its card shows on the Status page of whichever account is
chosen. Trouble that belongs to no account (`Accounts.LastError`) shows above it.

**Upload changes made on this computer** is the account's mode: on is read-write, and it shows
`Account.Mode`, the mode the account runs in. Turned on, a dialog first says that the browser opens
for a sign-in allowing KOneDrive to change files, and what uploading means (new files, edits,
renames, moves and deletions go up; deleted files go to OneDrive's recycle bin); then
`SetMode("read-write", false)`, whose sign-in URL opens as Sign In's does. While that sign-in waits,
the switch says so, with "Copy Sign-In Link" and "Cancel". Turned off, `SetMode("read-only", false)`;
if changes still wait to be uploaded, a dialog asks whether to turn off without uploading them (the
files stay), and then forces it. Refusals are told by name: uploading is not available for this
account in this version (the write gate), sign in first, sign in again (limitations log A16).

**Held removals.** The mass-delete guard holds a large delete until the user decides. The window
follows `HeldCount` for the Status page, the tray and the `massDelete` notification, and reads
`Changes()` for the list when a count changes, when `Paused` changes, or when the Activity page is
shown (limitations log A20).

The **status line** reads, for example, "Up to date · checked 20 s ago", "Listing your OneDrive:
N items so far", "Downloading 3 files", "Uploading 1 file", "3 changes waiting to upload", "Paused
until 14:00", "Paused: metered connection" (the account's own hold, `HeldBack`, with the Status
page's texts), "1 changed file was moved out of the way", "Signed out of OneDrive", "No OneDrive
folder yet", or the error, refreshed every 10 s. While `LiveChanges` is `connected`, the "· checked
20 s ago" suffix becomes "· live", whatever comes before it ("Up to date · live"), and the line no
longer ages. The window does not offer
the no-interception mode: a folder is registered only through `Folder.Register`.

**Places.** Each account's folder has an entry in Dolphin's Places panel and in file dialogs, named
`OneDrive — <label>` — with one account too, so that a second account renames nothing. The entry is
found again by a tag of its own (`konedrive-account=<id>`), not by its URL: renaming the account
renames it, a new folder moves it, and forgetting the folder or removing the account removes it.
The single-account versions' entry is taken over in its place in the panel. Nothing is touched
until the daemon and every account have answered. "Show in Places" is one switch for every account
(limitations log A12, A19).

**Free Up Space** asks for confirmation, then reports how much it freed and how many files it
skipped as busy. It has no D-Bus timeout: freeing up a large folder can take longer than the
default 25 s, and the window shows "Freeing up space…" until the answer.

**One instance.** The app is single-instance through `KDBusService(Unique)`; a second launch shows
the running window. It starts at login, hidden in the tray, through an XDG autostart entry that the
"Start at login" switch writes or removes; the switch is on by default after the first run, and the
entry itself is the truth, so removing it in System Settings turns the switch off. With a system
tray, closing the window hides it; without one, closing quits, so no process lingers unseen.

## 5. The tray icon

Each account has one of five states, and the icon shows the **worst** of them across the accounts,
in this order:

| State | Icon | An account is in it when |
|---|---|---|
| needs attention | `state-warning` | a sync error, a conflict, a failed update, a change that cannot be uploaded, removals the mass-delete guard holds, trouble that does not stop the folder |
| signed out | `state-offline` | signed out, OneDrive unreachable, or no folder yet |
| paused | `media-playback-pause` | the account is paused (`Paused`), or holds back by itself (`HeldBack`: a metered connection, the battery) |
| syncing | `state-sync` | a listing, a download or an upload is under way, or changes wait to be uploaded |
| synced | `state-ok` | the folder is up to date |

With no account at all, the icon is `state-offline`. The helper's trouble counts against every
account with an intercepted folder.

- **Tooltip.** With one account, the window's status line. With several, one line per account, in
  account order: "Personal — Up to date · checked 20 s ago", "Family — Signed out of OneDrive". An
  account that needs attention shows why in place of its status line.
- **Menu.** "Open OneDrive Folder" with one account; with several, an "Open Folder" submenu of the
  accounts that have a folder. Then "Open KOneDrive", "Refresh Now" — every account whose folder
  shows OneDrive — "Pause Syncing" (for 2, 8 or 24 hours, or until resumed: every such account not
  paused yet), "Resume Syncing" while any account is paused by the user, "Sync Anyway" while any
  account holds back by itself and is not paused by the user — it calls `SyncAnyway` on each such
  account and no other, as `konedrivectl sync anyway --all` does ([writes.md](writes.md) §11) —
  and "Quit".
- **Click.** Opens the window; on the account that needs attention when exactly one does, and
  otherwise on the account the window last showed.

The tray reads each account's state from its `Folder.State`, `Conflicts.Count`, `Folder.LastError`,
`Folder.Paused` and `Folder.HeldBack` and from
`HelperState`; a few of those readings still depend on exact wording from the daemon (limitations
log A2, A17).

## 6. Notifications

Notifications are sent by the **app**, not the daemon, through KNotification, with events defined
in `konedrive.notifyrc` so that each can be configured in System Settings:

| Event | When |
|---|---|
| `signedOut` | the account is signed out or needs signing in again |
| `diskFull` | a download or an update failed for want of disk space |
| `downloadFailed` | a download failed |
| `updateFailed` | a file changed in OneDrive could not be updated here |
| `uploadFailed` | a change made here cannot be uploaded (`upload-failed`): the reason in words — a name OneDrive refuses, a sign-in without the permission — and, for a full OneDrive, the title "OneDrive is full"; with "Show in Folder" |
| `conflict` | "your changed version was moved to …", or for a file changed on both sides "Both are kept: your version as …", with "Show in Folder" |
| `massDelete` | removals the mass-delete guard holds appeared: "N items deleted in … are not deleted in OneDrive yet", with "Restore Them" (`RestoreDeletes`, also what a click on the notification does) and "Delete in OneDrive" (`ConfirmDeletes`) |

Ordinary downloads notify nothing. The first event of a kind notifies at once; more of the same kind
within 10 s are sent as one summary ("2 more files could not be downloaded"). A notification is
chosen by the event's kind, never by its wording, except that a full disk is recognised by the
exact detail "not enough disk space".

Each account's events notify on their own, and the 10 s summaries are per account and kind. With
more than one account a notification's title names the account ("Download failed — Family"); its
text is unchanged, and clicking it opens the window on that account (limitations log A18).

The cost of sending them from the app: with the app quit, nothing notifies, and events that happen
meanwhile are never announced later; the window's lists still show everything (limitations log
A1). The app runs at login in the tray, so that is the exception.

## 7. Download and upload progress in Plasma

The app watches `Transfers.Downloads`. A transfer still running **2 s** after it first appears is reported to
Plasma as a `KJob` through `KUiServerV2JobTracker` — the mechanism Dolphin's own copy progress
uses — titled "Downloading from OneDrive", with the file name, bytes done of total, and speed.
Shorter transfers never show. At most **5** jobs are visible at once; the rest are summed into one,
"and N more files". Each account's `Downloads` is watched on its own: with several accounts, the
cap of 5 and the summary are per account, and a job's title names the account, "Downloading from
OneDrive — Family" (limitations log A18).

The daemon removes a transfer from `Downloads` before it signals the transfer's failure, and the
coalesced property can also arrive after the failure. So a job whose transfer leaves `Downloads` is
held for a **1.5 s** grace window: a `failed` or `update-failed` event naming the same path inside
it fails the job with the reason, and otherwise the job finishes as a success (limitations log A11).
A daemon restart ends every job with an error. The "Show download and upload progress" switch in
Settings is on by default.

**Uploads** are reported the same way, by a second watcher per account on `Uploads`: "Uploading to
OneDrive" (with the account's name when there are several), the same 2 s, cap of 5 and grace
window, and an `upload-failed` event naming the file fails its job with the reason in words
(limitations log A21).

## 8. Thumbnails

Dolphin draws a preview by opening the file, and opening a placeholder downloads it: scrolling past
a folder of photos would download them all. The Windows client avoids this with thumbnails from the
cloud, and so does konedrive.

**What KIO does, measured** (`docs/kio-behavior.md`): a thumbnail is found by the MD5 of the file's
fully percent-encoded `file://` URI, in `~/.cache/thumbnails/{normal,large,x-large,xx-large}`
(128, 256, 512, 1024 px). A cached PNG tagged with `Thumb::URI` and a `Thumb::MTime` equal to the
file's time is drawn **without opening the file**; a stale time makes KIO discard it and open the
file. Listing a folder opens nothing.

**The filler** (`desktop/thumbs.rs`) runs in the daemon, in the background:

- for each placed image or video (by the item's MIME type) with no current thumbnail, it asks Graph
  for one thumbnail, `c512x512`, and writes it to `x-large` as it is and scaled down to `large`
  and `normal`, each tagged with the file's URI and the placeholder's time;
- it runs after each listing cycle and every 10 minutes regardless, draining in batches of up to
  200 items, each request in a background slot of the account's transfer pool
  ([hydration.md](hydration.md) §6.4), like any background download. The store picks the
  candidates in SQL — placed images and videos whose `thumb_key` is missing or not the one for what
  they are now — in item id order, 500 ids a query with each page's paths found in one recursive
  query, at most 5 000 ids a call; each batch goes on from where the last one stopped, and a drain
  ends once every candidate has been looked at, so the next drain starts from the beginning again
  (issue #39, limitations log F166);
- `thumb_key` in the tree store records the cTag, path and time a thumbnail was made for, so a file
  is fetched again only when its content, name or time changes (a rename needs a new cache entry,
  because the cache is keyed by URI and checked against the time);
- every answer that settles whether the item has a usable thumbnail is recorded in `thumb_key`, and
  the item is not asked for again until the file changes: a thumbnail written; a 404; any other 4xx
  but `401`, `408` and `429`; a body over 8 MiB; an image over 4096 × 4096 px or 64 MiB of decoder
  memory; a thumbnail that cannot be written into the cache here. A final refusal is logged once,
  at `info`, with its status;
- only a passing trouble is tried again at the next drain: no answer, `401` (a sign-in trouble, not
  the item's), `408`, `429` (after the pool's throttle wait) and 5xx (issue #80);
- Graph refuses `c512x512` for some items with `406`: the item is then asked once for Graph's
  named size `large` (up to 800 px), scaled down the same way, and that answer is final under the
  same rules. An item refused at both sizes has no thumbnail from the cloud (limitations log K27).

`xx-large` (1024 px) is **not** filled: it would be a second request per image at roughly four
times the bytes, for a size Dolphin asks for only at maximum zoom on a HiDPI screen, and upscaling
the 512 px answer would look blurred. At that zoom KIO makes its own thumbnail, which downloads the
file (limitations log K15).

**The setting.** Thumbnails are fetched per account, on by default (`Folder.Thumbnails`,
`SetThumbnails`, `sync thumbnails`, the account page's "Download thumbnails"). Off, the filler asks
Graph for nothing while everything else runs, and Dolphin, with its previews on, downloads every
cloud-only image or video it previews in full, as it would any other file; the thumbnails already
in the cache stay. Turned on again, the filler asks at once for every item without a recorded
thumbnail. The filler also asks nothing while the account's background work stops — a pause, or
a hold ([writes.md](writes.md) §11) — and stops a drain under way between two requests.

**What still opens a file.** Type detection is a separate problem that a thumbnail does not solve:
for a name with no extension, or an ambiguous one like `.bin`, KIO reads the first bytes to learn
the type, and it does not consult `user.mime_type`, so no attribute can prevent it (limitations log
K1). An image shown before its thumbnail is filled, a preview of any other kind of file, and the
`xx-large` zoom also open the file. Turning previews off for the folder (View → Show Previews)
avoids those downloads.

## 9. Baloo

KDE's file indexer reads every file's content to index it, and a read of a placeholder downloads
it: left alone, Baloo would download the whole drive in the background the first time it walked the
folder. So a OneDrive folder is **excluded from Baloo**:

- the exclusion is added with `balooctl6 config add excludeFolders` and removed on Forget with
  `config rm`;
- before adding, the daemon checks whether the folder, or a directory above it, is excluded
  already, by reading Baloo's own settings file (`${XDG_CONFIG_HOME:-~/.config}/baloofilerc`,
  `[General]`, `exclude folders`, with or without `[$e]`, comma-separated, variables expanded,
  compared by path components) — `balooctl6 config list excludeFolders` was observed printing an
  empty list while the file held exclusions;
- whether *this daemon* added the exclusion is recorded in `config.toml` (`baloo_excluded` in the
  account's `[accounts.root]`), and Forget removes only an exclusion the daemon added, never one the
  user had set;
- every bring-up of a folder not recorded as excluded runs the same check-then-add, which only ever
  adds, so an exclusion that failed, timed out or was interrupted, or a Baloo installed later, is
  caught up;
- every call runs under a 10 s timeout and is killed when it expires; a hung or missing `balooctl6`
  behaves like a missing one and never blocks registration or Forget.

The cost: no Baloo search inside the folder — KRunner and Dolphin's search do not find files there
(limitations log W14).

## 10. Dolphin plugins

Two plugins in `dolphin/`, both running inside Dolphin, where a crash takes the file manager down
and any `open()` downloads what is shown. Both are tested to never open a file in the sync folder:
a test drives every code path under an inotify watch and fails on any open, and the tests run under
a poisoned allocator so that a use of freed memory cannot hide.

### 10.1 Emblems

The overlay plugin gives each file (and, for the pinned case, each folder) an emblem from its
`user.konedrive.state` and `user.konedrive.pin`, read with `lstat` and `lgetxattr`:

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
opening, so a symlink out of the root does not count as inside, and the same resolution is used to
walk up for a pin. The root answer is cached per directory while an inotify watch on that directory
stays in place, for the 256 most recently shown directories; the pin is read fresh on every call
instead, since a directory Dolphin has only passed through, not browsed on its own, never gets a
watch of its own (limitations log K22). `IN_ATTRIB` on the directory reports a child's state or pin
change, so emblems update live for a watched directory. Emblems need no daemon: they work with it
stopped. Reading the attributes on Dolphin's UI thread costs about 7 µs per file inside the folder;
the pin's ancestor walk is not cached (K5), and a directory's own pin bit is not cached either (K15).

### 10.2 The context menu

The action plugin adds **Always keep on this device**, a checkable action, and **Free up space**,
for files and folders and any selection (see [pinning.md](pinning.md)). "Always keep" is checked
when the selection is effectively pinned; while checked, it is disabled if anything in the
selection is pinned only by a folder above it (unchecking it then would refuse the whole call).
Checking it calls `Pin`; unchecking it calls `Unpin`, which only removes the pin -- files stay
downloaded, as on Windows (D-A). "Free up space" is shown for any folder in the root, or a file
that is downloaded or explicitly pinned, and disabled for a selection with anything pinned only by
a folder above it; it calls `FreeUp` (D-B). A selection is one asynchronous D-Bus call (`Pin`,
`Unpin` or `FreeUp`, on `Files` at `/org/konedrive/Accounts`) with no reply timeout, since
downloads can take minutes; the daemon finds each path's account, so a selection may span the
folders of several accounts ([accounts.md](accounts.md) §3.5). A click starts a
stopped daemon through D-Bus activation, as any KDE service would, rather than reporting that it is
not running. A path already waiting (in an earlier call not yet answered, whichever of the three it
was for) is never sent again, and at most 1000 paths wait at once per window (limitations log K6).
Refusals are explained by their error name; a batch refused because one path is pinned only by an
ancestor is explained with the daemon's own words, which name that path and folder, not the first
path of the selection. The actions can be switched off in Dolphin's context-menu settings.

**The section.** Whatever the plugin offers is one section of the menu itself, not a submenu: a
separator whose text is "OneDrive" (`konedrive_section`), the entries, and a closing separator
(`konedrive_section_end`). With nothing to offer it adds nothing. Whether the heading is drawn is
the widget style's choice (limitations log K30).

**Open in OneDrive** (`konedrive_open_online`, issue #53) is the section's last entry. It is
offered for exactly one selected path, never for several:

- an item the other two entries are offered for: enabled when it carries
  `user.konedrive.item-id` (read with `lgetxattr`; the plugin still opens nothing), otherwise
  disabled with the tooltip "Not in OneDrive yet.";
- an account's folder itself — a directory that carries `user.konedrive.root` and lies in no
  other account's folder: always enabled, and the section's only entry, since the other two are
  not offered there. It opens the root of the drive.

A click is one asynchronous `WebUrl(path)` call on `Files`, under the same rules as the other
calls: no reply timeout, a stopped daemon is started, a path already waiting is not asked again.
The daemon finds the account, reads the item's id from the path (opened as `Pin` opens it, so
nothing is downloaded) and asks Graph for the item — `GET me/drive/items/{id}`, or
`GET me/drive/root` for an account's folder itself — and answers its `webUrl`. The address is
stored nowhere and asked for on every click (K28). The daemon opens no browser and changes
nothing; the plugin opens the address with `QDesktopServices::openUrl`, and only an `https`
address. Refusals are explained by name: `NotUploaded` (no item id), `NotSignedIn`,
`Unreachable` (OneDrive did not answer; the sentence is followed by the daemon's message, the
cause), `OutsideRoot`, `NotManaged`, and `Failed` with the
daemon's words (the item is gone from OneDrive, or the answer has no address).
`konedrivectl sync open <path> [--print]` makes the same call and prints the address; without
`--print` it also opens it, under the guard the sign-in page has (`KONEDRIVE_NO_BROWSER` unset and
stdout a terminal).

## 11. Known limits

The limitations log's sections 7 and 8 list them. The main ones: Dolphin still opens some files
itself (K1); notifications need the app running (A1); no emblems in search results or Recent Files,
which do not use `file://` URLs (K2); and the Plasma side — how the tray, the popups and the job
tracker actually render — is not covered by the tests, which run offscreen on private buses (A7).
With several accounts: the window shows one at a time (A13), Sign In is several calls rather than
one transaction (A15), and a window or a Dolphin running across the upgrade to multiple accounts
needs a restart (F46).

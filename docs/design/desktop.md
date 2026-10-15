# Desktop integration

What sits on top of the daemon: its D-Bus API, the command line, the KOneDrive window and tray
icon, notifications, progress in Plasma, thumbnails, the Baloo exclusion, KDE's directory
notifications, and the Dolphin plugins. The daemon's own work is in [sync.md](sync.md),
[mount.md](mount.md) and [writes.md](writes.md).

## 1. Principles

- **Every client talks to the daemon.** The window, the tray, the command line and the Dolphin menu
  call D-Bus methods and show what the daemon publishes. The window never touches the folder. The
  Dolphin emblems are the one exception: they read the attributes the mount answers, by name
  (§11.1).
- **Every feature has a command-line path.** Anything the window can do, `konedrivectl` can do.
- **The daemon decides, the clients say it.** The state of an account as a whole is one function in
  the daemon (`Folder.Overall`, §2.4); a client turns its reason into words and an icon, and decides
  nothing from a sentence.
- **Refusals have names.** A refusal is a D-Bus error name; clients match the name, never the
  message, and turn it into a sentence about the user's file and what to do next (§2.10).
- **Codes cross the bus, and their sentences are written once** (§2.12).
- **No client opens a file in the folder.** Opening a file that is not on disk reads it from
  OneDrive; the plugins and the daemon's queries read attributes by name instead.

## 2. The D-Bus API

### 2.1 Names and objects

Session bus, service `org.konedrive.Daemon`. The objects are in [accounts.md](accounts.md) §5:
`/org/konedrive/Accounts` with `Accounts`, `Files` and the `ObjectManager`, and one
`/org/konedrive/Accounts/<id>` per account with `Account`, `Folder`, `Transfers`, `UploadQueue`,
`Conflicts`, `LocalScan` and `ActivityLog`. A development build adds `DevTools` and `TokenExport`
(§2.11).

No name carries a version: the daemon and its clients ship in one package. A member does not
repeat its interface's name (`Conflicts.List`, not `Conflicts.Conflicts`). The definitions are
`dbus/*.xml`, one file per interface, with each member's meaning in its comment; a test
(`crates/konedrived/tests/dbus_api.rs`) holds each against the live interface's signatures. The
daemon is D-Bus activated (`SystemdService=konedrived.service`), so the first call from any client
starts it, and every object is on the bus before the name is claimed
([accounts.md](accounts.md) §3.2).

Properties change through `PropertiesChanged`, each under the interface that holds it. Which
property goes out how is one table (`dbus/properties.rs`), which the getters answer through too, so
a property cannot be read one way and announced another:

- **coalesced**: the counters, `Conflicts.Count` and what of `Transfers` changes often: at most one
  signal per interface per 250 ms (`dbus/signals.rs`), so a drive of hundreds of thousands of items
  cannot flood the bus;
- **at once**: the folder's state, path, errors, the pause and the hold, `Writable`, `LiveChanges`,
  `QuotaFull`;
- **decided**: `Overall`, `Trouble` and `NotUpdated`, each announced the moment what it says
  changes, and never otherwise. When `Overall` changes, the coalesced properties that changed and
  are not sent yet go first, so a reason never arrives ahead of the count a client says with it.

`IgnorePatterns`, `FreeSpaceReserve` and `Thumbnails` are announced by the call that sets them.

### 2.2 `Account`, per account

| Member | Meaning |
|---|---|
| `Id` (`s`), `Label` (`s`) | the account's id, the last element of its object path, and its name ([accounts.md](accounts.md) §2) |
| `Mode` (`s`) | the access mode the account runs in, `read-only` or `read-write` ([accounts.md](accounts.md) §10) |
| `State` (`s`) | `signed-out`, `signing-in` or `signed-in` |
| `LastError` (`s`) | the reason for the last failure, a sign-in refused as another account's included; with none, why an account `config.toml` says is read-write runs read-only, or that Microsoft answered a read-only request with a token that can write (F66); empty otherwise |
| `ReadOnlyWanted` (`b`), `UnsentBeforeReadOnly` (`t`) | a switch to read-only that waits for the unsent changes, and how many are still to send ([writes.md](writes.md) §2.3) |
| `DisplayName`, `Email` (`s`) | from `GET /me`; empty for a signed-out account |
| `QuotaUsed`, `QuotaTotal`, `QuotaRemaining` (`t`), `QuotaState` (`s`) | the account's one quota, whoever reads it: the account's info (a sign-in, `RefreshInfo`) or the uploads' space check. `QuotaRemaining` is Graph's `quota.remaining`, never `total - used`; between reads the bytes uploaded come off it. Kept in `account.json` across restarts |
| `BeginSignIn() → s url` | starts a sign-in of this account and answers the URL to open; the bus's `Failed` for an account that is signed in or signing in |
| `CancelSignIn()`, `SignOut()`, `RefreshInfo()` | as named; `SignOut` deletes the refresh token and the cached profile and quota |
| `SetLabel(s)` | renames the account; `InvalidArgs` for a label the rules refuse, with the rule |
| `SetMode(s mode, b force) → s sign_in_url` | switches the access mode ([accounts.md](accounts.md) §10); the URL of the sign-in a switch to read-write needs, empty when none. Refused `NotSignedIn` (to read-write), `InvalidArgs` for another mode, `Failed` when it cannot be done now. A switch to read-write ends in `Mode` turning `read-write`, or in `LastError` saying why not; cancelled, in neither |

No token is ever exposed through `Account`.

### 2.3 `Folder`: the methods, per account

| Method | Does |
|---|---|
| `Register(s path)` | binds a folder to the account and brings it up; the refusals and their order are in [accounts.md](accounts.md) §7.1 |
| `Unregister()` | Forget: the mount comes off, and the folder is a plain directory with what it holds on this computer ([accounts.md](accounts.md) §7.2). Never refused for unsent changes; a client warns first with `UploadQueue.UnsentCount` |
| `Refresh()` | asks OneDrive for changes now, tries the changes in backoff now, and reads the quota again. A folder that is down after a failure is brought up first, and one whose sync could not start has it started. Refused `NotUp`, with why, when that fails and for a folder that only waits (`State` `waiting`) |
| `Skipped() → a(ssss)` | everything OneDrive has that the folder cannot hold: (path in OneDrive, reason, what keeps it here, where it is here) ([sync.md](sync.md) §5, §6) |
| `FreeUpSpace() → (u files, t bytes, u busy)` | frees up every downloaded file that nothing holds ([mount.md](mount.md) §6): the files freed and their bytes. A file a pin keeps, a download is filling, or that holds an unsent change is left, and is not counted in the answer: `busy` is 0 |
| `Pause(u seconds)`, `Resume()` | the user's pause, for `seconds` or until `Resume` when 0 ([writes.md](writes.md) §6.3); `NoRoot` with no folder |
| `SyncAnyway()` | lifts this account's automatic hold until a source or the hold's settings change ([writes.md](writes.md) §6.3) |
| `SetIgnorePatterns(as)` | the names of the user's own files that are never uploaded ([writes.md](writes.md) §3); `InvalidArgs` for an empty pattern, one holding `/` or a NUL, or one longer than 255 bytes |
| `SetThumbnails(b)` | whether Graph's thumbnails are fetched for this account (§8) |
| `SetFreeSpaceReserve(t gigabytes)` | the free space the downloads of pinned files leave on the folder's disk ([pinning.md](pinning.md) §3) |

Each setting is written to the account's section of `config.toml` and taken at once.

### 2.4 `Folder`: the properties, per account

| Property | Meaning |
|---|---|
| `Path` (`s`) | the account's folder, empty when it has none; a folder `config.toml` records has its path from the daemon's start, while `State` is `waiting` |
| `State` (`s`) | `none` (no folder), `waiting` (recorded and being brought up, nothing known to be wrong), `ready`, `listing` (the folder is up and a full listing runs, in place of `ready`), or `error` (it could not be brought up, its sync stopped on an error, or its account is held back at load); computed (`status::snapshot::published_state`), never stored |
| `Source` (`s`) | `onedrive`, or empty with no folder |
| `LastError` (`s`) | every current problem, joined with ". ": the registration's, the sync's, the upload queue's note, the pinned files that wait for disk space. For the log and for people; no client decides anything by it |
| `Overall` (`(ss)`) | the state the account is in as a whole, and the reason (below) |
| `Trouble` (`s`) | the sentence of the trouble there is now, whatever `Overall`'s reason is: while `State` is `error`, what `LastError` says; while it is `ready`, the problems that do not stop the folder, OneDrive out of reach among them; empty otherwise |
| `NotUpdated` (`s`) | empty: a file changed in OneDrive has its downloaded copy removed and is read from OneDrive again, so no update fails and waits |
| `ItemsListed`, `ItemsPlaced`, `SkippedCount` (`t`) | items in OneDrive, items the folder shows, and what it cannot hold |
| `LastChecked` (`x`) | unix time of the last successful check with OneDrive; 0 for never |
| `LocalBytes` (`t`) | the space the folder's files take in the backing directory (`st_blocks × 512`): a file that is not on disk takes none |
| `PinnedCount` (`u`) | how many files and folders carry a pin of their own |
| `IgnorePatterns` (`as`), `Thumbnails` (`b`), `FreeSpaceReserve` (`t`) | the settings of §2.3 |
| `Paused` (`b`), `PausedUntil` (`x`) | the user's pause, and when it ends by itself (0: until `Resume`) |
| `HeldBack` (`s`) | why the account holds back by itself: `metered`, `on-battery`, `power-saver`, or empty |
| `Writable` (`b`) | the folder's mount is up and switched to read-write, which follows `Account.Mode` |
| `LiveChanges` (`s`) | `connected` (Graph's notification socket is up; the poll runs every 5 minutes), `connecting` (the poll runs every minute), or `off` (paused, held back, or no sync) ([sync.md](sync.md) §5) |

**The account as a whole.** `Folder.Overall` is a state — `ok`, `syncing`, `warning`, `paused` or
`offline` — and a reason. The rule is one function over what the account and its folder publish
(`crates/konedrived/src/status/overall.rs`); the spellings are `konedrive_dbus::overall`'s. The
first row that holds decides:

| State | Reason | When |
|---|---|---|
| `offline` | `signing-in` | the account is signing in |
| `offline` | `signed-out` | the account is not signed in |
| `offline` | `no-folder` | no folder is registered |
| `syncing` | `starting` | `State` is `waiting` |
| `warning` | `stopped` | `State` is `error` |
| `warning` | `conflicts` | conflict copies are listed |
| `warning` | `quota-full` | OneDrive is full |
| `warning` | `too-big` | files are too big for the space left |
| `warning` | `blocked` | changes cannot be uploaded |
| `paused` | `paused` | the user paused the account |
| `paused` | `held-back` | the account holds back by itself |
| `paused` | `throttled` | OneDrive asked the account to wait (`Transfers.RetryAfter`) |
| `offline` | `unreachable` | OneDrive cannot be reached, and nothing else is wrong |
| `warning` | `trouble` | any other trouble that does not stop the folder |
| `syncing` | `listing` | `State` is `listing` |
| `syncing` | `transferring` | files are downloading or uploading, or changes wait to upload |
| `ok` | `up-to-date` | none of the above |

- Nothing is decided by a sentence: out of reach is a kind the sync's trouble carries
  (`TroubleKind`), never a wording.
- While a full listing runs, trouble that does not stop the folder is not counted.
- `konedrive_dbus::overall` also spells `not-updated`, which the daemon never decides.
- The one state no daemon can say is "the service is not running": a client shows `offline` for it
  by itself.

### 2.5 `Transfers`, per account

| Property | Meaning |
|---|---|
| `Downloads`, `Uploads` (`a(stt)`) | each transfer under way as (path, bytes done, bytes total): a download for as long as it holds its slot of the pool, until the file landed or the try failed; no thumbnail is one |
| `DownloadSpeed`, `UploadSpeed` (`t`) | bytes a second each way, the average of the last 3 s |
| `ActiveDownloads`, `ActiveUploads` (`u`) | the files moving now, each once however many streams it runs |
| `PoolInUse`, `PoolSize`, `PoolCeiling` (`u`) | the account's transfer pool: every slot held now (it may be above the size), how many requests it has in flight at most now, which adapts to OneDrive's throttling, and its ceiling (`[transfers] max`) |
| `LargeFiles`, `LargeStreams`, `LargeStreamLimit` (`u`) | the large files (100 MiB and up) the sync moves now, their streams, and how many may run at once (`[transfers] large`); a file being opened is never one |
| `RetryAfter` (`u`) | the seconds left of OneDrive's `Retry-After`, during which no transfer starts |
| `RetryAfterRefusal` (`s`) | the first file a program was refused during that wait: a read of content not on disk, an open for writing or a `Hydrate` fails at once with `EAGAIN` meanwhile ([mount.md](mount.md) §5) |
| `DownloadLeftCount`, `DownloadLeftBytes`, `DownloadDoneBytes`, `DownloadTimeLeft`, and the same four for uploads | the queue totals: the pinned files not on disk yet and each file being opened while its download runs, or the unsent changes; their bytes less what is moved; the bytes moved since nothing was last left; the seconds left at the speed of the last 30 s (0: unknown). What is kept back is not left |

They are published once a second while anything moves or a `Retry-After` runs.

### 2.6 `UploadQueue`, per account

What waits to go up and what is kept back. The meaning of each is in [writes.md](writes.md) §7
and in the comments of `dbus/org.konedrive.UploadQueue.xml`.

| Member | |
|---|---|
| `Changes(u limit) → a(tsssttsx)` | the unsent changes, oldest first: (seq, kind, path, state, bytes sent, bytes in all, reason, next try) |
| `UnsentCount() → t` | every unsent change, whatever holds it: what a client reads to warn before `Folder.Unregister` and `Accounts.Remove`; answered in any state of the folder |
| `NotUploaded() → a(ss)`, `NotUploadedSummary() → a(ssut)`, `NotUploadedFiles(s reason, u limit) → (a(ss) items, u total)` | what is kept back, by path, by reason, and the files of one reason |
| `PendingCount` (`u`), `PendingBytes` (`t`), `BlockedCount` (`u`), `QuotaFull` (`b`), `QuotaWaitingCount` (`u`), `QuotaWaitingBytes` (`t`), `TooBigCount` (`u`) | the counts of the window's Status page |

`Changes` and the three lists are refused `Unsupported` with no OneDrive folder, `NoRoot` with no
folder, and `NotUp`, with why, for a folder that is not up or whose sync has not started.

### 2.7 `Conflicts`, `ActivityLog` and `LocalScan`, per account

- **`Conflicts.List() → a(xsss)`**: (time, original path, path of the kept version, kind); the kind
  is `copy`, a conflict copy ([writes.md](writes.md) §5). `Dismiss(s kept_path)` takes one off the
  list, `NoConflict` for a path that names none; the file stays. `Count` (`u`) and `MachineName`
  (`s`), the name conflict copies are named after.
- **`ActivityLog.Recent(u limit) → a(xsss)`**: (time, kind, path, detail), newest first; the store
  keeps the newest 200. `Added(x, s, s, s)` announces each event as it is recorded; a client that
  falls behind can miss some, and `Recent` still has them. The kinds are `konedrive_tree`'s
  `ActivityKind` (`crates/konedrive-tree/src/activity.rs`): `downloaded`, `freed`, `added`,
  `updated`, `removed`, `moved`, `first-sync-began`, `first-sync-ended`, `conflict`,
  `not-removed-here`, `store-rebuilt`, `failed`, `update-failed`, and for a read-write account
  `uploaded`, `cloud-moved`, `cloud-deleted`, `upload-failed`, `restored`, `not-uploaded`. A full
  disk is the exact detail `not enough disk space`. The log is a summary: a first listing is two
  events of the folder, not one per item; at most 50 events of each kind of what OneDrive changed
  are logged between the ends of two listings; a `FreeUpSpace` is one `freed` event.
- **`LocalScan`**: the start-up check as the bus shows it ([mount.md](mount.md) §7). `State`
  (`running`, `idle`, or `none` before the first since the daemon started), `Reason` (`start`),
  `Started`, `Finished` and `Took`. Published when a check begins and when it ends.

### 2.8 `Accounts`

| Member | Meaning |
|---|---|
| `List` (`ao`) | every account's object, in account order |
| `ClientId` (`s`) | the application every account signs in with ([accounts.md](accounts.md) §8.1) |
| `LastError` (`s`) | trouble that belongs to no account: `config.toml` cannot be read or was written by a newer version, a migration step failed, an account could not be loaded |
| `Version`, `Commit` (`s`) | the daemon's build; never change while it runs (§4, the version line) |
| `PauseOnMetered` (`b`), `OnBattery` (`s`) | the automatic hold's settings, one pair for every account ([writes.md](writes.md) §6.3) |
| `SignIn() → (u sign_in, s url)`, `CancelSignIn(u) → b`, `SignInFinished(u, s, s, o)` (signal) | adding an account by signing in ([accounts.md](accounts.md) §8.2) |
| `Remove(o account)` | removing an account ([accounts.md](accounts.md) §8.3) |
| `SetClientId(s)` | overrides the built-in client id; `InvalidArgs` for a malformed one, the bus's `Failed` while any account is signing in or signed in |
| `SetPauseOnMetered(b)`, `SetOnBattery(s)` | change the hold's settings for every account at once, written to `config.toml`; each ends every account's `SyncAnyway`. `InvalidArgs` for an `on_battery` choice other than `sync`, `power-saver` or `pause` |

The same object is an `org.freedesktop.DBus.ObjectManager` ([accounts.md](accounts.md) §5).

### 2.9 `Files`

The calls on one file or on chosen paths, each routed by path to the account whose folder holds it
([accounts.md](accounts.md) §3.3). A path in no account's folder is refused `OutsideRoot`.

| Method | Does |
|---|---|
| `Hydrate(s path)` | downloads one file now ([mount.md](mount.md) §5) |
| `Dehydrate(s path)` | frees up one file ([mount.md](mount.md) §6): `NotUploaded` for one with an unsent change, `NotAllowed` for one a pin holds for, `NotHydrated` for one that is not on disk or is downloading, `InUse` for one that could not be freed after all |
| `ItemState(s path) → s` | `online-only`, `hydrating`, `hydrated`, or `not-managed` (a directory, a file of the user's own, a path in no folder): the folder's `user.konedrive.state`, read by name through the mount, never by opening the file |
| `Pin(as) → u`, `Unpin(as) → u`, `FreeUp(as) → (u files, t bytes, u busy, u skipped_pinned)` | "Always keep on this device", unpinning, and "Free up space" for chosen paths ([pinning.md](pinning.md) §4, §4.1) |
| `WebUrl(s path) → s url` | "Open in OneDrive": the address of the item's page in OneDrive's web interface, asked of Graph each time; an account's folder itself answers the drive's root. `NotUploaded` for an item OneDrive does not have yet, `NotSignedIn`, `Unreachable` when OneDrive does not answer, `NotManaged` for a file of the user's own, `Failed` otherwise. The caller opens the address (F295) |
| `Menu(as) → a{sv}` | what a file manager's context menu may offer for a selection ([pinning.md](pinning.md) §4.2): the keys `paths`, `always-keep` (`hidden`, `off`, `on`), `free-up` (`hidden`, `enabled`, `disabled`), `free-up-why` (`not-uploaded`), `open-online` (`hidden`, `enabled`, `disabled`) and `open-online-path` |

`Menu` decides in one place what the calls behind it would take and refuse
(`account_folder/menu.rs`). It changes nothing, opens no file, and waits for nothing that can take
long: each account's mount answers from its own reading connection to the store, which never waits
for the store's writer. Over several accounts' folders, `free-up-why` is that of the first account
that refuses, in the order the accounts' paths first come in the selection. The answer is about
the moment it was asked: a call made after it can still be refused.

### 2.10 Errors

A refusal is an error name under `org.konedrive.Error` (`konedrive_dbus::Refusal`). The daemon
answers: `NotSignedIn`, `AlreadyRegistered`, `NotEmpty`, `Overlaps`, `Unsupported`, `NoRoot`,
`NotUp` (the folder is not up, or its sync is not running), `OutsideRoot`, `NotManaged`,
`NotHydrated`, `InUse`, `NotAllowed` (a pin holds for it), `NotUploaded` (an unsent change, or an
item OneDrive does not have yet), `Unreachable`, `NoConflict`, `NoAccount`, `WritesNotAllowed` and
`ModeNotGranted` (the token export, §2.11), and `Failed` for everything without a name of its own
(an I/O failure, an account held back at load, a `Hydrate` during OneDrive's `Retry-After`).

An argument that will not do — a label, a client id, a mode, an `on_battery` choice, an ignore
pattern — is refused with the bus's own `InvalidArgs`. `Accounts` refuses with the bus's `Failed`
(`org.freedesktop.DBus.Error.Failed`) a call that is not possible now: a client id changed while
an account is signed in, anything while `config.toml` cannot be read. `Account`'s `BeginSignIn`,
`SignOut` and `SetLabel` refuse with the bus's `Failed` too; `SetMode`'s `Failed` is
`org.konedrive.Error.Failed`. The mapping is one file, `crates/konedrived/src/dbus/fault.rs`.

### 2.11 A development build

A daemon built with the `dev-tools` feature (`scripts/dev-install.sh`, `scripts/build-rpm.sh
--dev-tools`) serves two more interfaces; the released package has neither, and its build stops if
`konedrived` holds the token export ([packaging.md](packaging.md) §2).

- **`TokenExport`**, per account. `ReadOnly() → s`: an access token for a test run in the VM, about
  an hour of `Files.Read` on the account's drive, whatever its mode; never the refresh token.
  `ReadWrite() → s`, for the test account's harness only: refused `WritesNotAllowed` unless the
  account's drive is in `write_test_drive_ids`, `ModeNotGranted` unless the account is read-write.
  Both are refused `NotSignedIn` when there is no token.
- **`DevTools`**, on `/org/konedrive/Accounts`. `AddAccount(s label) → o`: a signed-out, read-only
  account with no folder, which never has to sign in; `InvalidArgs` for a label the rules refuse.

### 2.12 The words for the codes

What a code means to a person is written once, in English, in the crate `konedrive-text`, which
knows only the codes (`konedrive-reason`, `konedrive-dbus`):

| Module | The codes | Shown by |
|---|---|---|
| `reasons` | every key of `Reason` and `LocalSkip`: why a change is not uploaded | the window, `konedrivectl` |
| `waits` | every key of `WaitsFor`: what keeps an item here that the folder cannot hold | the window, `konedrivectl` |
| `files` | every name of `Refusal`, for each of four operations on a file: keep on this device, unpin, free up, open in OneDrive; and what the Dolphin plugin says of a call the daemon never answered | the Dolphin plugin, `konedrivectl` |
| `menu` | every `free-up-why` of `Files.Menu`, and the tooltip of a disabled "Open in OneDrive" | the Dolphin plugin |

- **A sentence** is written as the window or the plugin shows it: a capital first letter, a full
  stop, and named places for what is filled in. A code may have no sentence: such a reason is shown
  as the daemon stored it, and such a refusal with the daemon's message.
- **`konedrivectl`** reads the catalogue directly.
- **The window and the Dolphin plugin** are built with C++ generated from the catalogue, every
  sentence a literal inside `i18n`, `i18nc` or `i18np`, so that translation stays where KDE's tools
  expect it: `app/generated/reasontexts.{h,cpp}` and `dolphin/src/generated/refusaltexts.{h,cpp}`.
  The files are in git, and the C++ build does not run cargo.
- **A new code gets its words or the tests fail.** The crate's tests hold the catalogue against the
  codes, and `crates/konedrive-text/tests/generated.rs` fails when a generated file in git differs
  from what the generator writes now. The steps are in `CONTRIBUTING.md`, "A new reason or
  refusal".

The words for `Overall`'s reasons and for the reasons of the "Not in the Folder" page are written
in the window and in `konedrivectl`, each on its own.

## 3. The command line

`konedrivectl` talks to the same interfaces (`crates/konedrivectl/`). The user's list of commands
is `docs/command-line.md`; `konedrivectl <command> --help` has each in full.

**Choosing the account** (`choice.rs`). A command that acts on one account takes it from the
global option `--account <id | label | email>`, else from `KONEDRIVE_ACCOUNT` (empty counts as
unset), else it is the only account there is. The name is matched as an id, a label and an email,
label and email in any case. A name that fits no account, or several, or several accounts and none
named, stops the command with exit status 2 and lists them; with no account at all it stops with
status 1 and says how to add one. A name that fits two accounts — one's label is another's email,
or a hand-edited `config.toml` — is refused, never taken as the first. The commands that take a
path, and `account list`, `account add`, `account rename`, `account remove`, `set-client-id`,
`settings` and `sync anyway --all`, refuse `--account` with status 2 and ignore
`KONEDRIVE_ACCOUNT`. With several accounts each success line starts with the account's label.

| Command | Account | Calls |
|---|---|---|
| `--version` | — | the command's own build, then `Accounts.Version` and `Commit` |
| `account list` | — | every account's id, label, email, sign-in state, mode, and folder with its `Folder.State` |
| `account add` | — | `Accounts.SignIn`; waits for its `SignInFinished`, six minutes at most. Ctrl-C and the limit call `CancelSignIn`: on `true` it says "cancelled" or "timed out", on `false` it waits for the outcome, so it never says "cancelled" for an account that is then added. A daemon that leaves the bus ends the wait |
| `account rename <account> <label>` | — | `Account.SetLabel` |
| `account remove <account> [--yes]` | — | `UploadQueue.UnsentCount`, and with any unsent change asks first; then `Accounts.Remove`, and says what was deleted and what was kept |
| `account mode [read-only\|read-write] [--force]` | chosen | shows the mode, or `Account.SetMode`: read-write opens the browser and waits, six minutes at most, until `Mode` is `read-write` or `LastError` says why not |
| `set-client-id <id>` | — | `Accounts.SetClientId` |
| `settings on-metered [pause\|sync]`, `settings on-battery [sync\|power-saver\|pause]` | — | shows or changes the hold's settings |
| `login`, `logout` | chosen | `Account.BeginSignIn` and waits; `Account.SignOut`. With no account at all `login` is refused and names `account add` |
| `status` | chosen, or all | the sign-in state, the mode, and an `Overall:` line in words by its reason; every account under its label when several and none is named |
| `sync register <path>` | chosen | `Folder.Register` |
| `sync forget [--yes]` | chosen | `UploadQueue.UnsentCount`, and with any unsent change asks first; then `Folder.Unregister` |
| `sync status` | chosen, or all | the folder, its `Overall:` line, its state and counts, live changes, the mode, what waits each way, the pause or the hold, the space on disk, pins, conflicts, the last start-up check (`Local scan:`), and OneDrive's wait with the first file refused |
| `sync refresh`, `sync skipped`, `sync activity [--limit N]`, `sync transfers`, `sync conflicts`, `sync dismiss <kept path>`, `sync free-up-space` | chosen | the `Folder`, `Transfers`, `Conflicts` and `ActivityLog` members of the same name |
| `sync upload-queue [--all]`, `sync not-uploaded [--all]`, `sync ignore [list\|add\|remove]` | chosen | `UploadQueue` and `SetIgnorePatterns` |
| `sync pause [--for <duration>]`, `sync resume`, `sync anyway [--all]` | chosen; `--all` — | `Pause`, `Resume`, `SyncAnyway`; `--all` for every account that holds back by itself and is not paused by the user |
| `sync thumbnails [on\|off]`, `sync free-space [GiB]` | chosen | shows or changes `Thumbnails` and `FreeSpaceReserve` |
| `sync hydrate`, `dehydrate`, `state` `<path>`; `sync pin`, `unpin`, `free`, `menu` `<paths…>`; `sync open <path> [--print]` | by path | `Files`; `sync open` prints the address and, without `--print`, opens it, only an `https` one |
| `dev add-account <label>`, `dev export-access-token --out <file> [--read-write]` | —; chosen | a development build's only (§2.11); the token is written to a `0600` file atomically, never through a symbolic link |

A browser is opened only when stdout is a terminal and `KONEDRIVE_NO_BROWSER` is not set; the URL
is printed in any case. A command that asks a question (`sync forget`, `account remove`) fails when
its input is not a terminal, unless given `--yes`.

When the daemon refuses, the command says what that means for the user's file and what to do,
chosen by the error name (`text/refusals.rs`). For `sync pin`, `unpin`, `free` and `open` the
sentences are the catalogue's (§2.12), the ones Dolphin shows. When a path command is refused, the
command reads every account's folder to say where the path is or is not. It resolves only the
directory a path is in, never its last component, so a symbolic link given as a folder reaches the
daemon as a link and is refused.

## 4. The window

KOneDrive is a Kirigami application (`app/`). Its sidebar is headed by an **account switcher**,
below which are six pages about the account chosen there and, after a separator, Settings, which is
the whole app's:

| Page | Shows |
|---|---|
| **Status** | the status line and what needs attention; the folder and "On this computer"; "Free Up Space…", "Refresh Now", "Open in File Manager"; the mode ("Read-only", "Changes upload", "Read-only for now" while the folder of a read-write account is not writable, or "Switching to read-only" while a switch waits); the last start-up check; the changes waiting to upload and those that cannot be; a full OneDrive and files too big for the space left, with "Refresh"; the pins; "Pause Syncing…" (2, 8 or 24 hours, or until resumed) or "Resume"; while the account holds back by itself, why, with **Sync Anyway** |
| **Activity** | two cards, "Downloading" and "Uploading": the speed, the files moving, what is left with its size and time, what is done, and a chart of the last two minutes (KQuickCharts); the transfer pool once below them; the transfers under way with progress bars; the newest 50 events |
| **Conflicts** | each conflict copy, with "Show Both" and "Dismiss"; the newest 200, then "and N more" naming `konedrivectl sync conflicts`; a count badge while there are any |
| **Not in the Folder** | `Skipped()` and why, in the words `sync skipped` uses; the first 200 |
| **Not Uploaded** | `NotUploadedSummary()` in its groups, and the files of a reason when it is opened (`NotUploadedFiles(reason, 20)`); a count badge while changes are blocked |
| **Account** | the name with "Rename…"; the switch "Upload changes made on this computer" (below); "Download thumbnails" (§8); the free space to keep for pinned files; sign in or out, the Microsoft account's name, email and quota, and `Account.LastError`; the folder with "Choose Folder…" and "Forget Folder"; this computer's name for conflict copies and the ignore list; "Remove Account…" |
| **Settings** | "Start at login", "Show download and upload progress", "Show in Places", "Show a tray icon for each account"; for every account, "Pause on metered connections" and "On battery"; "Quit KOneDrive" |

The long lists are `ListView`s, which build only the rows in sight, with a fixed height.

**The switcher** shows the chosen account and opens a menu of every account, each with its state's
icon (§5), then "Sign in…". When another account than the chosen one is in `warning`, a sign on
the switcher says so. The choice is remembered (`CurrentAccount=<id>` in `konedriverc`). With more
than one account each page's title names the account.

**The state is the daemon's.** An account's state, its icon and what needs attention come from
`Folder.Overall` (`AccountStatus`): the reason chooses the words — a fixed line, `Trouble` for
`stopped`, the wait's time left for `throttled`, or the attention text with its count. Nothing is
worked out from the other properties, and `Folder.LastError` is not read. The window says by itself
only "The KOneDrive service is not running". The status line reads, for example, "Up to date ·
checked 20 s ago", "· live" while `LiveChanges` is `connected`, "Listing your OneDrive: N items so
far", "Downloading 3 files", "Paused until 14:00", or the trouble, refreshed while it ages.

**The version line.** The foot of the sidebar names the window's build: "Version 0.1.1-dev.57 ·
commit 5254595" (`docs/releasing.md`). When the daemon on the bus runs another build — its
`Version` or `Commit` differs — a second line says to restart it. `konedrivectl --version` says
the same.

**After an update** the package replaces the window's program file and restarts the daemon, not
the window. So when the daemon comes back as another build and the window's own program file is no
longer the one it started from, the window starts the installed program in its place, in the same
process (`SelfRestart`): hidden in the tray if it was hidden, on the same account if it was shown.
A window built by hand and run against an installed daemon stays, with the version line.

**Sign in…** is `Accounts.SignIn`; the window then waits for `SignInFinished` with that number
([accounts.md](accounts.md) §8.2). `signed-in` chooses the new account and opens the folder picker
at once. A cancel shows nothing, `already-added` names the account it is, a failure says the
daemon's reason, and a daemon that leaves the bus ends the adding with a message that says so.
With no account at all, the Status page shows "Connect your OneDrive" with "Sign in…".

**Forget Folder** and **Remove Account…** read `UploadQueue.UnsentCount` first and, with any, say
that those changes stay only on this computer. Remove Account says that the downloaded files and
the user's own stay in the folder.

**Upload changes made on this computer** is the access mode, and shows `Account.Mode`. Turned on,
a dialog says that the browser opens for a sign-in allowing KOneDrive to change files; then
`SetMode("read-write", false)`, whose URL opens as Sign In's does, and while that sign-in waits the
switch says so. Turned off while changes are unsent, the switch to read-only waits for them, and the
page says how many are still to send, with "Switch Now" (`force`) and "Cancel"
([writes.md](writes.md) §2).

**Places.** Each account's folder has an entry in Dolphin's Places panel and in file dialogs,
`OneDrive — <label>`, with one account too. The entry is found again by a tag of its own
(`konedrive-account=<id>`), not by its URL: renaming the account renames it, a new folder moves
it, and forgetting the folder or removing the account removes it. "Show in Places" is one switch
for every account.

**One instance.** The app is single-instance through `KDBusService(Unique)`; a second launch shows
the running window. It starts at login, hidden in the tray, through an XDG autostart entry that
"Start at login" writes or removes; the entry is the truth, so removing it in System Settings turns
the switch off. With a system tray, closing the window hides it; without one, closing quits.

## 5. The tray icon

Each account has one of five states, decided by the daemon (`Folder.Overall`). With several
accounts and "Show a tray icon for each account" on, as it is unless turned off, each account has
its own icon. Otherwise there is one icon, which shows the worst state across the accounts in this
order (`AppStatus::rank`): it compares states the daemon gave, across accounts, which no single
account's object knows.

| State | Icon | `Overall` |
|---|---|---|
| needs attention | `state-warning` | `warning` |
| signed out | `state-offline` | `offline`, and the service not running |
| paused | `media-playback-pause` | `paused` |
| syncing | `state-sync` | `syncing` |
| synced | `state-ok` | `ok` |

- **Tooltip.** With one account, its status line and what needs attention. With several, one line
  per account in account order, "Personal — Up to date · checked 20 s ago"; an account's own icon
  has its label as the title.
- **Menu.** "Open OneDrive Folder" (with several accounts on one icon, an "Open Folder" submenu),
  "Open KOneDrive", "Refresh Now", "Pause Syncing" (2, 8 or 24 hours, or until resumed), "Resume
  Syncing" while an account is paused by the user, "Sync Anyway" while an account holds back by
  itself and is not paused by the user, and "Quit". On the one icon each entry acts on every
  account it applies to; on an account's own icon, on that account only.
- **Click.** Shows the window, or hides it when it is the active one; on the account that needs
  attention when exactly one does, or on the icon's own account.

An account added or removed gains or loses its icon at once. The setting is the window's,
`TrayIconPerAccount` in `konedriverc`'s `[General]` group; the daemon knows nothing of it.

## 6. Notifications

Notifications are sent by the **window's process**, not the daemon, through KNotification, with
events defined in `app/konedrive.notifyrc` so that each can be configured in System Settings
(`app/notifier.cpp`):

| Event | When |
|---|---|
| `signedOut` | the account went from signed in to signed out by itself, and still is two seconds later; not after a sign-out asked for in the window |
| `diskFull` | a download failed for want of disk space (the detail `not enough disk space`) |
| `downloadFailed` | a download failed |
| `updateFailed` | an `update-failed` event |
| `uploadFailed` | a change cannot be uploaded (`upload-failed`): the reason in words, with "Show in Folder"; and once when `QuotaFull` turns true, "OneDrive is full" |
| `conflict` | a conflict copy: "Both are kept: your version as …", with "Show in Folder" |
| `notRemovedHere` | a directory could not be removed here because it holds in OneDrive what the folder cannot show (F299) |
| `throttled` | a program was refused a file during OneDrive's `Retry-After`: once per wait, naming the first file and the time left |

The first notification of an event is sent at once; more of the same event within 10 s are sent as
one summary. A notification is chosen by the event's kind, never by its wording, except that a full
disk is the exact detail `not enough disk space`. Each account's events notify on their own; with
more than one account a notification's title names the account, and clicking it opens the window
on that account. With the app quit nothing notifies, and what happened meanwhile is never announced
later; the window's lists still show it.

## 7. Download and upload progress in Plasma

The app watches `Transfers.Downloads` (`app/downloadprogresscontroller.cpp`). A transfer still
running 2 s after it first appears is reported to Plasma as a `KJob` through
`KUiServerV2JobTracker`, the mechanism Dolphin's own copy progress uses, titled "Downloading from
OneDrive", with the file name, bytes done of total, and speed. At most 5 jobs are visible at once
per account; the rest are summed into one, "and N more files". A job whose transfer leaves
`Downloads` is held for 1.5 s: a `failed` or `update-failed` event naming the same path fails it
with the reason, and otherwise it finishes as a success. A daemon that leaves the bus ends every job
with an error. **Uploads** are reported the same way from `Uploads`, "Uploading to OneDrive", and an
`upload-failed` event fails its job. With several accounts a job's title names the account. "Show
download and upload progress" in Settings is on by default.

## 8. Thumbnails

Dolphin draws a preview by reading the file, and reading a file that is not on disk fetches it:
scrolling past a folder of photos would download them. konedrive writes OneDrive's own thumbnails
into the cache KIO reads (`desktop/thumbs.rs`).

**What KIO does** (`docs/kio-behavior.md`): a thumbnail is found by the MD5 of the file's
percent-encoded `file://` URI in `~/.cache/thumbnails/{normal,large,x-large,xx-large}` (128, 256,
512, 1024 px). A cached PNG tagged with `Thumb::URI` and a `Thumb::MTime` equal to the file's time
is drawn without opening the file.

**The filler**, a task of the folder's sync ([sync.md](sync.md) §9):

- its candidates are the files the folder shows that OneDrive has, whose MIME type is an image's or
  a video's, and that have no thumbnail recorded for the content OneDrive has now at the path they
  are at (`ItemReader::list_thumbnail_candidates`, the store's `thumbnail_marks`);
- for each it asks Graph for one thumbnail, `c512x512`, and writes it as a PNG to `x-large`,
  `large` and `normal`, scaled down and never up, tagged with the file's URI and the content's time;
  a `406` is asked once more at Graph's named size `large`;
- every answer that settles whether the item has a usable thumbnail is recorded — a thumbnail
  written, a refusal, a body that will not decode or is over the decode limits (4096 × 4096 px, 64
  MiB), a thumbnail that cannot be written into the cache — and the item is not asked again until
  its content or its path changes. Only a passing trouble (no answer, `401`, `408`, `429`, a 5xx) is
  tried again at the next drain. The cache itself is not looked at;
- it runs after every cycle, when thumbnails are turned on again, and every ten minutes, draining
  200 candidates at a time until each has been looked at once;
- a thumbnail is a request to the Graph API, not a download, and takes no slot of the transfer pool.
  At most two run at once, and none starts before the folder's first listing ended, during a
  listing, during a `Retry-After`, while a file is being opened, or while anything waits for a slot
  of the pool (`desktop/thumbs/request_start.rs`).

`xx-large` is not filled: it would be a second request per image at about four times the bytes.
Where Dolphin asks for that size, KIO makes its own thumbnail, which reads the file.

**The setting.** Thumbnails are fetched per account, on by default (`Folder.Thumbnails`,
`SetThumbnails`, `sync thumbnails`, "Download thumbnails"). Off, the filler asks for nothing, and
Dolphin, with its previews on, reads every image or video it previews in full; the thumbnails in the
cache stay. The filler also asks for nothing while the account is paused or holds back by itself.

**What still reads a file.** For a name with no extension, or an ambiguous one, KIO reads the first
bytes to learn the type (K1). Those bytes are a range read, not a download: the file is downloaded
whole only once more than `[transfers] download_after` of it was read ([mount.md](mount.md) §5). A
preview of any other kind of file, and the `xx-large` zoom, read the file. Turning previews off for
the folder avoids those reads.

## 9. Baloo

KDE's file indexer reads every file's content, and reading a file that is not on disk fetches it:
left alone, Baloo would download the whole drive. So a OneDrive folder is excluded from Baloo
(`desktop/baloo.rs`):

- the exclusion is added with `balooctl6 config add excludeFolders` at registration and at every
  bring-up of a folder not recorded as excluded, and removed with `config rm` on Forget and on the
  account's removal;
- before adding, the daemon checks whether the folder, or a directory above it, is excluded already,
  by reading Baloo's own settings file (`baloofilerc`, `[General]`, `exclude folders`):
  `balooctl6 config list excludeFolders` was observed printing an empty list while the file held
  exclusions;
- whether this daemon added the exclusion is recorded in `config.toml` (`baloo_excluded` in the
  account's `[accounts.root]`), and a Forget removes only an exclusion recorded so, never one the
  user had set. An exclusion added but not recorded before the daemon stopped is found at the next
  bring-up and taken for the user's own;
- every call of `balooctl6` runs under a 10 s timeout and is killed when it expires.

The cost: no desktop search inside the folder (W14).

## 10. KDE's directory notifications

A program that lists a directory follows it with a watch of its own, and that watch ends when the
mount under it goes. So when a folder is mounted again, and when a stopping daemon or the tidy step
has taken its mount off, the daemon tells KDE's programs to list it again with
`org.kde.KDirNotify.FilesAdded` on the session bus (`desktop/dirnotify.rs`); a program that does
not show the directory ignores it. What tells a watcher of a change inside a mounted folder is the
mount's own inotify bridge ([mount.md](mount.md) §9).

## 11. Dolphin plugins

Two plugins in `dolphin/`, both running inside Dolphin, where a crash takes the file manager down
and any `open()` reads what is shown. Both are tested never to open a file in the folder
(`dolphin/tests/noopentest.cpp`, which drives every code path under an inotify watch), and the
tests run under a poisoned allocator.

### 11.1 Emblems

The overlay plugin gives each file and folder an emblem from attributes it reads by name with
`lstat` and `lgetxattr`, never opening anything (`dolphin/src/filestate.cpp`). Inside a mounted
folder the mount answers those attributes from the item's row ([mount.md](mount.md) §4). The upload
mark `user.konedrive.sync` is read first, and decides alone when it is there:

| `user.konedrive.sync` | Emblem |
|---|---|
| `pending`, `uploading` | `state-sync` |
| `blocked` | `state-error` |
| absent | by the state and the pin, below |

Otherwise `user.konedrive.state` and the pin decide:

| State | Pinned | Emblem |
|---|---|---|
| `online-only` | no | `cloudstatus` |
| `online-only` | yes (its download is queued) | `state-sync` |
| `hydrating` | either | `state-sync` |
| `hydrated` | no | `dialog-ok` (an outline check) |
| `hydrated` | yes | `emblem-checked` (a filled check) |
| a folder | yes | `emblem-checked` |
| a folder | no | none |
| outside a folder, or a state the plugin does not know | — | none |

"Pinned" is `user.konedrive.pin` on the item itself or on a directory above it up to the folder's
root, read fresh at every call. A file is inside a folder when a directory above it carries
`user.konedrive.root`; ancestors are resolved with `lstat` and `readlink` per component, so a
symbolic link out of the folder does not count as inside. The root answer is cached per directory
while an inotify watch on that directory stays in place, for the 256 most recently shown
directories. `IN_ATTRIB` on a watched directory reports a change of a child's attributes, which the
mount's announcements make ([mount.md](mount.md) §9), so the emblems update live. Emblems need no
call to the daemon. With the daemon stopped the folder is the backing directory, whose downloaded
copies carry the marks the mount's steps leave on them ([mount.md](mount.md) §3.5): they show as
downloaded, and the user's own files show none.

### 11.2 The context menu

The action plugin adds a section "OneDrive" with **Always Keep on This Device** (checkable),
**Free Up Space** and **Open in OneDrive** ([pinning.md](pinning.md) §4, §6).

**What is offered is the daemon's answer.** For a selection with something in a sync folder — by
the marks alone: the nearest directory at or above a path carries `user.konedrive.root` — the
plugin makes one `Files.Menu(paths)` call and sets the entries from the answer, value by value. A
right click anywhere else makes no call.

**The plugin never blocks Dolphin.** Dolphin asks for the entries synchronously, so the plugin hands
over the section at once in a waiting state — shown, disabled, unchecked — and sends the call
asynchronously. When the answer comes, each entry is set from it. An error, no daemon, or no answer
within two seconds hides the section. The call carries no auto-start, so building a menu never
starts the daemon. An answer that comes after its menu is gone is dropped.

| Answer | Entry |
|---|---|
| `always-keep`: `off`, `on` | "Always Keep on This Device", unchecked or checked |
| `free-up`: `enabled` | "Free Up Space" |
| `free-up`: `disabled` | disabled, with the catalogue's tooltip for `free-up-why` (§2.12) |
| `open-online`: `enabled`, `disabled` | "Open in OneDrive"; disabled with "Not in OneDrive yet." |
| `hidden`, or a value the plugin does not know | hidden |

**What an entry does.** Checking "Always Keep" calls `Pin`, unchecking it `Unpin`, and "Free Up
Space" `FreeUp`, each with the answer's `paths`, not with the selection. Each is one asynchronous
call on `Files` with no reply timeout, since downloads can take minutes; a click starts a daemon
that stopped after the menu was built. A path already waiting in an earlier call is never sent
again, and at most 1000 calls wait at once. "Open in OneDrive" calls `WebUrl` with the answer's
`open-online-path` and opens the address with `QDesktopServices::openUrl`, only an `https` one.
Refusals are told by their error name, in the catalogue's words. The actions can be switched off
in Dolphin's context-menu settings.

## 12. Known limits

In the limitations log: Dolphin still reads some files itself (K1); "Move to Trash" needs KDE
Frameworks 6.21 to delete without a download (K34); there is no desktop search inside the folder
(W14); "Open in OneDrive" opens under whichever personal account the browser is signed in to
(F295); with the daemon stopped the folder shows only what is on disk (F296). Others, in plain
words: notifications need the app running (§6); there are no emblems in search results or Recent
Files, which do not use `file://` URLs; and the Plasma side — how the tray, the popups and the job
tracker render — is not covered by the tests, which run offscreen on private buses.

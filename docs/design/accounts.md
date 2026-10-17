# Several accounts

konedrive holds several OneDrive accounts at once, each with its own sign-in and its own folder.
This document says what an account is, how one daemon serves several, where each account's files
are, what keeps two accounts apart, and how an account and its folder are added and removed. How
one folder follows its drive is in [sync.md](sync.md); the mount is in [mount.md](mount.md); the
access mode is in [writes.md](writes.md) §2; the D-Bus API, the window and the tray are in
[desktop.md](desktop.md).

## 1. What the user sees

- Several personal Microsoft accounts, each with its own folder, Places entry, activity, conflicts
  and "Not in the Folder" list. Work and school accounts are not supported.
- Every account is read-only until the user switches it to read-write (§10).
- The window shows one account at a time, chosen in a switcher at the top of its sidebar; the tray
  has an icon for each, or one for them all ([desktop.md](desktop.md) §4, §5).
- A Microsoft account is connected once. A sign-in of an account as another Microsoft account than
  its own is refused: that one is added as a new account.
- Two accounts cannot share a folder, and one account's folder cannot be inside another's.
- A file moved from one account's folder to another's is copied and removed, as between two disks
  (F124).
- Removing an account signs it out and forgets it on this computer. What its folder holds on this
  computer stays; nothing in OneDrive is touched.

## 2. An account

| Field | What it is |
|---|---|
| Id | 12 random lowercase hexadecimal characters, none of another account's. It names the account's D-Bus object, its state directory and its Places entry; nobody types it |
| Label | The name people see and type. Set to the account's email when it is added (§8.2), and renamed at will. Trimmed; 1 to 40 characters; no `/`, no control character; not 12 hexadecimal digits in any case, so that a label is never taken for an id; unique among the labels whatever the case. Nothing on disk is named after it |
| Drive | The Graph drive id of the Microsoft account: the account's identity (§6). Recorded at the first sign-in or the first `GET /me/drive`, then never changed |
| Mode | `read-only`, the default, or `read-write` (§10) |
| Origin | `added`, or `migrated` for the account a version-1 configuration became (§9). A missing or unknown value reads as `migrated` |
| Folder | At most one registered folder (§7) |

Accounts keep the order in which they were added: the order of the window's switcher, the tray's
tooltip and every list of accounts.

## 3. One daemon, several accounts

### 3.1 The parts

```text
konedrived
 ├─ ConfigStore ────── config.toml: one owner, one lock, every change through it
 ├─ Registry ───────── the accounts' folders as they see each other; the hold's settings
 │                     and the machine's conditions, told to every account
 ├─ AccountManager ─── /org/konedrive/Accounts: Accounts, Files, ObjectManager
 │    └─ Account <id> ── /org/konedrive/Accounts/<id>: Account and the folder's interfaces
 │         ├─ AccountService  sign-in, tokens, its wallet item, its drive, its mode
 │         └─ SyncService     its folder: registration, mount, sync, pins, thumbnails, Baloo
 └─ watchers ───────── the network, and power and metering
```

| Part | Where | State it owns, and who changes it |
|---|---|---|
| `ConfigStore` | `config/store.rs` | `config.toml` as last read or written. Every change goes through `ConfigStore::update` (§4.2) |
| `AccountManager` | `daemon/manager.rs`, `daemon/manager/sign_in.rs` | The ordered list of accounts and the sign-in for a new account under way. It alone adds and removes accounts, and it alone writes the `Registry`'s list. One lock (`changing`) runs `SignIn`'s making of an account, `Remove`, `SetClientId` and the hold's two settings one at a time |
| `Account` | `daemon/manager.rs` | Holds one account's `AccountService`, `SyncService` and paths, and the tasks that turn their state into `PropertiesChanged`; nothing else |
| `AccountService` | `account/` | One account's sign-in state, tokens, cached profile and quota, wallet item, drive and mode. The order of its locks is in `account/mod.rs` |
| `SyncService` | `account_folder/` | One account's folder and everything that runs for it. Its state changes only inside `SyncService::change` (`account_folder/folder.rs`) |
| `Registry` | `account_folder/registry.rs` | The list of every account's `SyncService`, written only by `AccountManager`; the lock every registration holds from its overlap check to its end (§6.3); what every account was last told of the hold's settings and the machine's conditions |

Each account has its own mount, store, transfer pool, listing, plan's task and thumbnail filler.
One account's downloads do not take another's slots, and one account's fault takes down only its
own folder ([sync.md](sync.md) §9).

### 3.2 The start

Every object a client may call is on the bus before the bus name is claimed
(`daemon/startup.rs`):

1. The daemon stops if `org.konedrive.Daemon` already has an owner. It takes `config.toml.lock`
   for the life of the process, and stops if another daemon holds it: two daemons on one
   configuration could each migrate it under an account id of its own.
2. `config.toml` is loaded, and migrated if it is version 1 (§9).
3. The files a migration still has to move are moved, and an account's own hold settings are moved
   to the top-level keys (§4.1), before anything opens them.
4. Each account is loaded in file order: its session restored from the wallet and its cached
   profile, and the folder `config.toml` records for it taken up, down, so that its path
   is published while `Folder.State` reads `waiting`.
5. `/org/konedrive/Accounts` and every account's object are exported, and only then is
   `org.konedrive.Daemon` claimed.
6. Every account's folder is brought up, one after another in account order: mounted, and its sync
   started. A panic in one account's bring-up leaves that folder down, saying so, and the next is
   brought up all the same.
7. `main` starts the watchers of the network and of power and metering. When one of the tasks the
   daemon needs ends, it stops as on a signal and exits with a failure, and systemd starts it again
   (`Restart=on-failure`, five seconds later).

The connection is built with the `ObjectManager` of `/org/konedrive/Accounts` already on it. A
connection built with an interface is one whose object server listens before the first message is
read; a call read before the server listens is dropped with no reply, and the call that started the
daemon through D-Bus activation must not be.

### 3.3 Which account a path belongs to

The calls of `org.konedrive.Files` ([desktop.md](desktop.md) §2.9) take a path, because Dolphin
and `konedrivectl` know a path, not an account (`AccountManager::route`). The account is the one
whose folder is a component prefix of the path with its directory part resolved — through `..`
and through a symbolic link such as `/home` to `/var/home` — or, only when that cannot be resolved,
of the path as given, if it has no `.` or `..` in it. The last component is not resolved, unless it
is `..`, and routing never opens the file. Only an account with a folder is found. A path in no
account's folder is refused `OutsideRoot`; `ItemState` answers `not-managed`, and `Menu` leaves the
path out.

`Pin`, `Unpin` and `FreeUp` take many paths. Every path is routed before anything changes, so one
path in no account's folder refuses the whole call. Every account's share is then checked as that
account checks it before any account acts ([pinning.md](pinning.md) §4), so a refusal the checks can
give is given for the whole call. The accounts then act one after another: a failure in a later
account leaves what the earlier ones did. The counts in the answer are summed.

## 4. The configuration and each account's files

### 4.1 `config.toml`

```toml
config_version = 2
client_id = ""                  # empty: konedrive's own application registration (§8.1)

[[accounts]]
id = "3f9a1c0e5b7d"
label = "ann@outlook.com"
mode = "read-only"
origin = "added"
drive_id = "D1A2B3C4"
login_hint = "ann@outlook.com"

[accounts.root]                 # absent while the account has no folder
path = "/home/ann/OneDrive"
id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d"   # the folder's user.konedrive.root
source = "onedrive"
baloo_excluded = true
```

Keys the example leaves out, each absent until it is set (`config/model.rs`):

- at the top: `pause_on_metered` (absent: `true`) and `on_battery` (absent or unknown:
  `power-saver`), the automatic hold's settings for every account ([writes.md](writes.md) §6.3);
  `[transfers]` with `max`, `large` and `download_after` ([mount.md](mount.md) §5);
  `write_test_drive_ids`, the drives a development build may export a read-write token for
  (§10), which decides no account's mode;
- in an account: `ignore` and `machine_name` ([writes.md](writes.md) §3, §5), `thumbnails` (absent:
  on, [desktop.md](desktop.md) §8), `free_space_reserve_gb` (absent: 10, [pinning.md](pinning.md) §3),
  `paused_until` (the user's pause, which outlasts a restart), `read_only_wanted` (a switch to
  read-only that waits, [writes.md](writes.md) §2.3), and `legacy_token` and `migrate_files` while a
  migration is not finished (§9);
- in `[accounts.root]`: a missing `source` reads as `local`. A folder whose `source` is not
  `onedrive` is never brought up; it is kept as recorded, for a Forget.

An account's own `pause_on_metered` or `on_battery`, where a file has one, is read only to be moved
to the top-level key at the next start, the strictest value winning (`move_hold_settings`).

The file is written whole, with mode `0600`: a key the daemon does not know, and every comment,
is gone at the next write.

### 4.2 One owner

**Every change through one call.** `ConfigStore::update` re-reads the file, applies the change and
writes the result atomically (a temporary file, `fsync`, rename, `fsync` of the directory), holding
one lock across the three. A change that is refused, or changes nothing, writes nothing. A check
and the write it allows are one step: the identity guard relies on it (§6.2). Readers that need
the file as it is now, such as the mode's gate, read it again without the lock; every write
replaces the file in one step, so a reading is always a whole file.

**What cannot be read is never overwritten.** A file that cannot be read or parsed, whose
`config_version` is not a whole number above zero, or that a newer version wrote, *poisons* the
store for the life of the process: no account is loaded, every write is refused, and
`Accounts.LastError` names the file and the reason. So does a migration whose own writes failed.
A later start with the file fixed loads it then.

**Validation at load.** Accounts are checked in file order, and nothing is rewritten
(`Config::holds`):

- an account whose id is not 12 lowercase hexadecimal characters, or repeats an earlier account's
  id, is not loaded at all: it could name neither an object nor a directory;
- an account whose label (in any case), drive, folder root id or folder path collides with an
  earlier account's — a folder that is, is inside, or contains an earlier one, by the paths as
  written — is loaded but *held back at load*: its folder is not brought up, `Folder.State` is
  `error`, `LastError` names the collision, and a registration is refused `Failed`, saying to
  correct `config.toml` and start konedrived again;
- a `mode` that is a string other than `read-only` or `read-write` loads as `read-only`, is
  logged, and is written back as `read-only` with the next change.

### 4.3 Where each account's files are

| What | Where |
|---|---|
| Configuration | `~/.config/konedrive/config.toml`, one for every account; `config.toml.lock` beside it while a daemon runs; `config.toml.v1` after a migration (§9) |
| The store: the rows, the step memory, the pins, the activity log, the conflicts | `$XDG_STATE_HOME/konedrive/accounts/<id>/fuse.sqlite` |
| Cached name, email and quota, and the scopes and the drive of the last token | `$XDG_STATE_HOME/konedrive/accounts/<id>/account.json` |
| Refresh token | the Secret Service, one item per account (§4.4) |
| The folder's root id and drive | `user.konedrive.root` and `user.konedrive.drive` on the folder's own directory in the backing directory (§6.3) |
| Thumbnails | the freedesktop cache, shared by every account: it is keyed by file URI |

`accounts/<id>/` is created with mode `0700`, and everything in it goes when the account is
removed. The store can be lost without losing a file; what goes with it is in D36.

### 4.4 Refresh tokens

Each account's refresh token is a Secret Service item of its own, with the attributes
`application=konedrive`, `kind=account-refresh-token` and `account=<id>` (`account/secret.rs`). It
is labelled `KOneDrive: <email>` once the daemon knows the account's email, and
`KOneDrive refresh token` before. The version-1 item is `application=konedrive`,
`kind=refresh-token`. The two `kind`s differ, rather than an `account` attribute being added
alone, because a Secret Service search matches every item whose attributes *include* the ones
asked for: a search for the version-1 item would otherwise find every account's too. The access
token stays in the daemon's memory, one per account.

## 5. On the bus

| Object | Interfaces |
|---|---|
| `/org/konedrive/Accounts` | `org.konedrive.Accounts`, `org.konedrive.Files`, `org.freedesktop.DBus.ObjectManager`; in a development build, `org.konedrive.DevTools` |
| `/org/konedrive/Accounts/<id>` | `org.konedrive.Account`, `org.konedrive.Folder`, `org.konedrive.Transfers`, `org.konedrive.UploadQueue`, `org.konedrive.Conflicts`, `org.konedrive.LocalScan`, `org.konedrive.ActivityLog`; in a development build, `org.konedrive.TokenExport` |

`Accounts.List` lists the account objects in account order and changes with `PropertiesChanged`.
The `ObjectManager` announces each account object with `InterfacesAdded` once it is on the bus and
`InterfacesRemoved` when it goes, for generic tools (`busctl`, D-Spy); konedrive's own clients
follow `Accounts.List`. The members are in [desktop.md](desktop.md) §2.

## 6. Keeping accounts apart

Three checks, each where the wrong account could otherwise act.

### 6.1 Every listing cycle: the folder's drive is still the account's

Each cycle of the listing begins with `GET /me/drive` on the account's own token
(`ListingCycle::check_account`, `listing/listing_cycle.rs`), and compares the drive id with the one
the store records, or, when the store records none, with the account's `drive_id` in
`config.toml`. A mismatch stops the folder ("this folder was listed from another OneDrive
account"), and the account records the drive its token reaches, so that a read-write account
turns read-only. Nothing is listed.

An account with no drive recorded records it from its first successful `GET /me/drive`:
`RefreshInfo`, or a cycle. A drive another account has already is never recorded a second time:
the cycle then stops the folder, naming that account ("signed in to the Microsoft account already
connected as …"), and nothing is listed into it.

### 6.2 At sign-in: an account is one drive, and a drive is one account

After the code exchange, and before the refresh token is stored, the daemon asks `GET /me/drive`
(and `GET /me`, for the email) with the new access token (`account/attempt.rs`). Every other
account that has no drive recorded yet and may be signed in to one — signed in, or holding a
stored refresh token it has not used — is first asked for its own (`settle`). Then, inside one
`ConfigStore::update`, so that two sign-ins cannot both pass (`account/sign_in.rs`):

- this account has a drive and it is another one: refused — "This account is `<email>`. You signed
  in as a different Microsoft account; to connect that one, add a new account.";
- another account has this drive: refused — "This Microsoft account is already connected as
  '`<label>`'.";
- an account that may be signed in to this drive still has none recorded: refused — "Could not
  check which account this is; try again.";
- otherwise the drive is recorded if the account had none, the refresh token is stored, and the
  account is signed in. A token that cannot be stored — the wallet locked, its prompt refused —
  fails the sign-in, and the drive recorded for it is taken back.

A refused sign-in stores nothing: its tokens are dropped, `State` returns to `signed-out`, and
`LastError` says why; the browser's page says it too. The check fails closed: a `GET /me/drive`
that fails refuses the sign-in, and so does an answer with no refresh token. A failed `GET /me`
does not; the wallet item is then labelled with the email the daemon last knew.

### 6.3 At registration: a folder belongs to one account

- **No nesting.** `Folder.Register` refuses, with `Overlaps`, a folder that is, is inside, or
  contains another account's folder — up, held back at load, or only recorded in `config.toml` —
  comparing the resolved paths by component and the directories by device and inode
  (`Registry::overlapping`). The message names the other account. Every registration, whichever
  account makes it, holds the registry's `registering` lock from this check to its end, so two
  accounts cannot both pass it with folders that nest.
- **A folder remembers its drive.** The folder's own directory carries `user.konedrive.drive`, the
  drive of the account it shows, written once the drive is known and only where the folder carries
  none (`folder::root::mark_drive`). A folder that carries a root id may be registered again without
  being empty (§7.1); that would let one account take over another's forgotten folder. So a folder
  that carries another drive than the account's — any drive, for an account that has none recorded
  — is refused `NotEmpty`: it holds another account's files. An empty one holds nothing to take
  over: its stale drive is taken off, and the registration goes ahead, so Remove, then Add, on the
  same folder works once the folder is emptied.
- **A hand-edited configuration** whose folders collide is held back at load (§4.2).

## 7. An account's folder

### 7.1 Register

`Folder.Register(path)` binds a folder to the account (`account_folder/bring_up.rs`). It is
refused, before anything is touched, in this order:

1. an account held back at load, or being removed: `Failed`;
2. an account that is not signed in: `NotSignedIn`;
3. an account that has a folder, up or not (one `config.toml` records counts from the daemon's
   start): `AlreadyRegistered`;
4. the folder overlaps another account's: `Overlaps` (§6.3);
5. the folder carries another account's drive: `NotEmpty` (§6.3);
6. the folder's own checks (`folder::root::register_root`): `Unsupported` for what is no
   directory, a symbolic link, or on a filesystem that fails the probe; `NotEmpty` for a folder
   that is not empty and carries no root id of ours.

**The probe** (`konedrive_fs::probe::probe_dir`) makes one nameless file in the folder
(`O_TMPFILE`) and checks every feature the backing directory needs: a sparse size and a `user.*`
extended attribute. A missing feature is named in the refusal. A folder that already carries a
root id of ours is not probed again: it was probed when it was first registered.

**The root id** is a random UUID in `user.konedrive.root` on the folder's directory. One the folder
carries is used again, so that a registration made again after a failure, a Forget or a restart
names the same folder; a value that does not have the shape of one we mint is no root id. A store
left by a folder forgotten earlier is removed first: it describes another folder.

The folder then comes up: it is kept out of Baloo ([desktop.md](desktop.md) §9), marked with its
account's drive, recorded in `config.toml`, and its sync started, which mounts it
([sync.md](sync.md) §9). A registration that fails leaves nothing recorded.

### 7.2 Bring-up, Refresh and Forget

- **At the start** the folder `config.toml` records is brought up without the sign-in check: the
  mount shows its names whoever is signed in, and its sync says what it lacks. A folder whose
  bring-up fails is down and says why in `LastError`; it is tried again at the next start, and by
  `Folder.Refresh`.
- **`Folder.Unregister`** (Forget) stops the sync, takes the mount off, gives the backing directory
  its write bits back, removes the store, and takes off the Baloo exclusion this daemon added. The
  folder is a plain directory again, with the files that were downloaded and the user's own. It is
  never refused for unsent changes: a client warns first with `UploadQueue.UnsentCount`, and those
  files stay only on this computer.

## 8. Adding and removing

### 8.1 The client id

Every account signs in with one Entra application, `Accounts.ClientId`. konedrive ships its own
(`DEFAULT_CLIENT_ID`), so signing in needs nothing from the user; `config.toml`'s `client_id`
overrides it for anyone who registers their own. `SetClientId` trims what it is given, accepts the
canonical GUID form only, and is refused while any account is signing in or signed in.

### 8.2 Add

An account is added only by signing in. `Accounts.SignIn() → (u sign_in, s url)` starts a sign-in
for a new account and answers its number and the URL to open; the window's **Sign in…** and
`konedrivectl account add` both call it.

The sign-in belongs to no account (`daemon/manager/sign_in.rs`). The manager runs it: the loopback
listener, the authorization URL (read-only scopes, Microsoft's account picker), the wait for the
browser (five minutes), the exchange, and the question of which drive and email this is. The steps
of the attempt are the ones an account's own sign-in runs, in one piece of code
(`account/attempt.rs`). Until all of that has succeeded and the drive is known not to be another
account's, nothing exists for it: no id, no entry in `config.toml`, no `accounts/<id>/`, no stored
token, no object on the bus.

`SignIn` is refused, with nothing started, when `config.toml` cannot be read (the account could not
be written at the end), no client id can be had, or the listener cannot be bound. There is one
sign-in for a new account at a time: a `SignIn` while one is under way ends that one as
`cancelled`, but only once nothing can refuse the new one. Numbers are not reused while the daemon
runs.

`Accounts.SignInFinished(u sign_in, s outcome, s message, o account)` is sent once for every
sign-in `SignIn` answered, unless the daemon stopped first:

| Outcome | When | `message` | `account` |
|---|---|---|---|
| `signed-in` | the account was made and is in `List` | its label | its path |
| `cancelled` | `CancelSignIn`, a newer `SignIn`, or `SetClientId` | empty | `/` |
| `already-added` | the drive is another account's (§6.2) | that account's label | that account's path |
| `failed` | anything else, the timeout included | why | `/` |

**The account is made** once the browser sign-in has succeeded. The other accounts' drives are
settled first, which asks Graph, before the manager's `changing` lock is taken; the rest runs under
that lock:

1. the identity guard (§6.2) and the entry it allows — the label, the drive and the `login_hint` —
   in one write of `config.toml`;
2. the account built (`accounts/<id>/`), its token stored and its session signed in; the wallet is
   asked with the lock held (A31);
3. the account listed and put on the bus, and `SignInFinished` sent. Its name and quota are read
   on their own and may arrive after it.

A step that fails takes back what the steps before it made, and the outcome is `failed`. The entry
is written before the token is stored, so that a daemon that stops in between leaves a signed-out
account anybody can see, sign in or remove — never a token in the wallet that no account owns.

**The label** is the account's email, or the first free one of `<email> 2`, `<email> 3`, … when
another account has it as its label. With no email, or one the rules refuse (longer than 40
characters), it is the first free one of `Personal`, `Personal 2`, …. A label never decides
whether an account is added already; only the drive does.

**A cancel**, `Accounts.CancelSignIn(u sign_in) → (b cancelled)`, is never refused and waits for
nothing. `true`: this call ended the sign-in, and `cancelled` follows. `false`, changing nothing:
the number is not under way — ended already, or its account being made — and its own
`SignInFinished` says how it ended. So `konedrivectl account add`, after Ctrl-C or its time limit,
says "cancelled" or "timed out" on `true`, and on `false` waits for the outcome.

**A client that goes away** leaves its sign-in under way: a sign-in finished in the browser after
that still adds the account. **A daemon that goes away** sends no signal: the window and
`konedrivectl account add` end the adding when the daemon's name leaves the bus.

`Account.BeginSignIn`, `Account.CancelSignIn` and `Account.SetLabel` are for an account that
exists: signing a signed-out account in again (`konedrivectl login`) and renaming. A development
build can also add a signed-out account under a chosen label, with no folder
(`org.konedrive.DevTools.AddAccount`, `konedrivectl dev add-account`).

### 8.3 Remove

`Accounts.Remove(account)` (`AccountManager::remove`), under the manager's `changing` lock:

1. the folder is forgotten exactly as `Folder.Unregister` forgets it (§7.2), its store with it, and
   in the same change the account's folder side is retired: no registration or bring-up is made for
   it from then on. An account held back at load is forgotten the same way;
2. the account is signed out and retired: a sign-in under way is given up, and the refresh token
   (and the version-1 item, while `legacy_token` is set) and the cached profile and quota are
   deleted;
3. the account is taken out of `config.toml`;
4. `accounts/<id>/` is deleted with what is left in it;
5. the account's objects are taken off the bus and it leaves both lists.

It is never refused for unsent changes: the caller warns first, with `UploadQueue.UnsentCount`.
What stays: what the folder holds on this computer, as a Forget leaves it. A path that names no
account is refused `NoAccount`, and any removal `Failed` while `config.toml` cannot be read.

A removal that fails at step 2 (the token cannot be deleted) or step 3 (`config.toml` cannot be
written) leaves the account as an account again: listed, taking a folder, a sign-in and a mode,
and removable again. What the steps before the failure did is not taken back: the folder stays
forgotten, and after step 2 the account is signed out. The refusal says what failed and what was
done.

## 9. From a version-1 configuration

A `config.toml` with no `config_version`, or with `config_version = 1`, is a single-account file
(`config/migrate.rs`). It is migrated as it is loaded, before anything is exported:

1. A version-1 file that cannot be read is not migrated, and nothing is written: the store is
   poisoned (§4.2).
2. Is there an account to carry over? Yes if the file names a folder, or `account.json` or a store
   is at the version-1 places (directly under `$XDG_STATE_HOME/konedrive/`). Only when none of
   those says yes is the wallet asked whether the version-1 refresh token is there, through the
   presence check that needs no unlock, with 10 s to answer; no answer counts as yes.
3. Yes: one account, `Personal`, read-only, `migrated`, with the drive the file recorded, both
   migration flags set, and the folder from the `sync_root*` keys. No: version 2 with the client
   id and no account.
4. The version-1 file is copied to `config.toml.v1` (mode `0600`), then version 2 is written
   atomically: the commit point. If either write fails, the store is poisoned for this run.

At every start, for each account whose `migrate_files` is set, before its services open anything,
`account.json` and a store at the version-1 place are moved into `accounts/<id>/` and the flag is
cleared. The store moved is the file of the store's present name, `fuse.sqlite`; a version-1
`tree.sqlite` is never read, so a migrated account lists its drive into a new store (D36). A store
that cannot be opened, whose write-ahead log survives its close, or whose place is taken, is left
where it is and logged. An error of the moves themselves keeps the flag for the next start and
shows in `Accounts.LastError`. Every step is idempotent, so the next start finishes whatever a
crash interrupted.

The refresh token moves lazily, on the account's first token load: the account's own item is used
if it exists; otherwise the version-1 item is read, stored as the account's own, and deleted;
`legacy_token` is cleared unless the version-1 item could not be deleted. No unlock prompt is added.

There is no way back but `config.toml.v1`, by hand.

## 10. The mode

Every account has a mode, `read-only` or `read-write`, stored in `config.toml`; a new account is
read-only. What the mode changes in the folder, the switch to read-only that waits for the unsent
changes, and the folder's side of the switch are in [writes.md](writes.md) §2. The
account's side (`account/mode.rs`, `account/state.rs`):

- **The mode it runs in.** `Account.Mode` is `read-write` only while `config.toml` says so and
  records a drive for the account, the drive its token was last seen to reach is that drive, and
  the scopes its last token response granted include `Files.ReadWrite`; the scopes and the drive
  are kept in `account.json`. Otherwise it is `read-only`, and when `config.toml` says read-write,
  `LastError` says why.
- **The scope.** Read-only asks for `Files.Read User.Read offline_access`, read-write for
  `Files.ReadWrite User.Read offline_access`. A refresh asks by the mode the account runs in; the
  sign-in that adds an account asks read-only. Microsoft may answer a read-only request with a token
  that can write; it is used to read only (F66).
- **To read-write**, `Account.SetMode("read-write", false)` answers a sign-in URL — empty when the
  account runs read-write already — whose sign-in asks for `Files.ReadWrite`, pinned to the
  account: its password asked for again, its email filled in. Only when the token response grants
  that, for this account's own drive, are the refresh token stored and `mode = "read-write"`
  written. A cancelled, refused or failed sign-in changes nothing but `LastError`; the account stays
  signed in.
- **To read-only** needs no sign-in: the next refresh asks for `Files.Read`. While changes are
  unsent the switch waits for them unless forced (`ReadOnlyWanted`, `UnsentBeforeReadOnly`); nothing
  is dropped.
- **The mode is the user's choice.** Any signed-in account can be switched to read-write, whatever
  its drive. `write_test_drive_ids` only limits a development build's `TokenExport.ReadWrite`.

## 11. The clients

- **The window** shows one account at a time, chosen in the account switcher; Settings is the whole
  app's ([desktop.md](desktop.md) §4).
- **The tray** has an icon per account, or one that shows the worst state of them all
  ([desktop.md](desktop.md) §5).
- **Notifications and download progress** name the account once there are several
  ([desktop.md](desktop.md) §6, §7).
- **Places** has one entry per account's folder, `OneDrive — <label>` ([desktop.md](desktop.md)
  §4).
- **The Dolphin plugins** read the attributes the mount answers, which need no account, and call
  `Files`, which finds the account by path ([desktop.md](desktop.md) §11).
- **The command line** chooses the account with `--account <id | label | email>` or
  `KONEDRIVE_ACCOUNT`; with one account it needs neither. The path commands go through `Files`,
  where the path decides the account. The rules are in [desktop.md](desktop.md) §3.

```text
$ konedrivectl account list
ID            LABEL            EMAIL            STATE       MODE       FOLDER
3f9a1c0e5b7d  ann@outlook.com  ann@outlook.com  signed-in   read-only  /home/ann/OneDrive (ready)
8c21d07a44e1  Family           —                signed-out  read-only  —
$ konedrivectl --account family login
```

## 12. Known limits

Recorded in [`../limitations/`](../limitations/README.md):

- a move between two accounts' folders is a download, a removal and an upload (F124);
- "Open in OneDrive" opens under whichever personal account the browser is signed in to (F295);
- while a wallet prompt is open at the end of a sign-in, the other changes of the accounts wait
  (A31);
- consent to write stays with Microsoft after an account turns read-only (F66).

Said where it happens: a sign-in refused when the drive cannot be checked (§6.2), an account held
back at load (§4.2), and a migrated account's store, which is made anew (§9).

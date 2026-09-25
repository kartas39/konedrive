# Multiple accounts

konedrive holds several OneDrive accounts at once, each with its own sign-in and its own folder.
This document describes what an account is, how one daemon serves several, how a path or an
intercepted open finds its account, where each account's data lives, what keeps two accounts
apart, how accounts are added and removed, and how a single-account installation becomes the first
account. How one folder follows its drive is in [sync.md](sync.md); how a file is filled is in
[hydration.md](hydration.md); the D-Bus API, the window and the tray are in
[desktop.md](desktop.md).

## 1. What the user sees

- Any number of personal Microsoft accounts, each with its own folder, Places entry, activity,
  conflicts and "Not in the Folder" list. Work or school accounts are not supported yet
  (limitations log F49).
- Every account is read-only unless it is switched to read-write, which only a test account can be
  while uploads are being developed (§10).
- The window shows one account at a time, chosen in a switcher at the top of its sidebar; the tray
  icon sums them all up ([desktop.md](desktop.md) §4, §5).
- A Microsoft account can be connected once. An account signed in again as a different Microsoft
  account than its own is refused: that one is added as a new account.
- Two accounts cannot share a folder, and one account's folder cannot be inside another's.
- Removing an account signs it out and forgets it on this computer. Its folder's files stay where
  they are, and so do the files rescued from it; nothing in OneDrive is touched.
- An installation from before multiple accounts becomes one account, named "Personal", at the first
  start of the new daemon, with its folder, sign-in and history (§8).

## 2. An account

| Field | What it is |
|---|---|
| Id | 12 random lowercase hexadecimal characters (48 bits), checked against the ids present and never reused. It names the account's D-Bus object, its state and rescue directories and its Places entry; nobody has to type it |
| Label | The name people see and type ("Personal", "Family"). Trimmed; 1 to 40 characters; no `/`, no `@`, no control character; not 12 hexadecimal digits in any case; unique regardless of case. With no `@` and not shaped like an id, a label is never mistaken for an email address or an id where any of them can name an account. It can be changed at any time, and nothing on disk is named after it |
| Drive | The Graph drive id of the Microsoft account: the account's identity (§6). Empty until the first sign-in or the first `GET /me/drive`, then never changed |
| Mode | `read-only`, the default, or `read-write` (§10) |
| Origin | `migrated` for the account carried over from a single-account installation (§8), `added` for every other. A missing or unknown value reads as `migrated` |
| Folder | At most one registered folder, with its source (`onedrive` or `local`) and its registration mode ([sync.md](sync.md) §3, [hydration.md](hydration.md) §14) |

Accounts keep the order in which they were added: the order of the window's switcher, of the
tray's tooltip and of every list of accounts. They cannot be reordered.

## 3. One daemon, several accounts

### 3.1 Components

```text
konedrived
 ├─ ConfigStore ────── config.toml: one owner, one lock, every write through it
 ├─ HelperHub ──────── the one link to konedrive-helper, its supervisor, HelperState,
 │                      the per-inode locks, the fill-on-open loop and its router
 ├─ AccountManager ─── /org/konedrive/Accounts: Accounts1, Files1, ObjectManager
 │    └─ Account <id> ── /org/konedrive/Accounts/<id>: Account1, Sync1, Dev1
 │         ├─ AccountService  sign-in, tokens, its wallet item, its drive
 │         └─ SyncService     its folder: registration, listing, tree store, activity,
 │                            conflicts, pins, replacements, thumbnails, Baloo
 └─ network watcher ── one NetworkManager watcher, nudging every account's sync
```

| Component | Where | Responsibility |
|---|---|---|
| `ConfigStore` | `config.rs`, `migrate.rs` | Loads `config.toml`, migrates a single-account file (§8), validates it, and runs every change as re-read, change and atomic write under one lock (§4.1) |
| `AccountManager` | `accounts.rs` | The ordered list of accounts; startup; `Add`, `Remove` and `SetClientId`; putting each account's objects on the bus and taking them off; routing per-file calls by path (§3.5) |
| `Account` | `accounts.rs` | One account: its `AccountService`, its `SyncService`, its paths, and the tasks that turn their state into `PropertiesChanged` |
| `AccountService` | `account.rs`, `secret.rs`, `oauth.rs` | Per account: sign-in, tokens, the account's own wallet item (§4.3), its drive, and the identity check at sign-in (§6.2) |
| `SyncService` | `sync/` | Per account: everything [sync.md](sync.md), [hydration.md](hydration.md) and [pinning.md](pinning.md) describe for one folder, on the hub's link |
| `HelperHub` | `sync/hub.rs` | The link to the helper and what goes with it, for every account (§3.3); which account an intercepted open belongs to (§3.4); the overlap check across accounts (§6.3) |

### 3.2 Startup

Every object a client may call is on the bus before the name is claimed, as before:

1. The daemon asks the bus whether `org.konedrive.Daemon` already has an owner, and stops if it
   has: a daemon of an older version still running could write a single-account configuration over
   the migrated one (limitations log F41). It then takes `config.toml.lock` for the life of the
   process, and stops if another daemon holds it: two daemons started at once would both migrate,
   each under an account id of its own.
2. `config.toml` is loaded, and migrated if it is a single-account file (§8).
3. The files a migration still has to move are moved, before anything opens them (§8.3).
4. Each account is brought up in order: its session restored from the wallet's presence check and
   its cached name and quota, and an intercepted folder held as registered until the helper is
   back.
5. `/org/konedrive/Accounts` and every account's object are exported, and only then is
   `org.konedrive.Daemon` claimed.
6. Every folder that needs no helper is brought up; the hub's supervisor, the `HelperState` watcher
   and the network watcher start.

### 3.3 One helper link for every account

The helper sends each intercepted open to the newest connection of the file owner's uid
([hydration.md](hydration.md) §10.2). A second connection from the same daemon would take every
request away from the first, so the daemon keeps one link and every account shares it. On connect
the hub brings every account's intercepted folder up again, one after another in account order —
each re-registers its root, whose walk the helper performs, and recovers it — and only then serves
fill requests. On loss it tells every account at once.

Shared by every account of the daemon:

- the link, and `HelperState`, which is published once, on `Accounts1`; each account's
  `Sync1.LastError` still begins with the helper's advice while its folder waits for the helper;
- the per-inode lock table: an inode belongs to one account only, so one table serves them all;
- the four slots of the fill-on-open loop, and the helper's credit of 64 requests per connection.

Per account: the four slots of pinned downloads ([pinning.md](pinning.md) §4), the two slots of
replacements ([sync.md](sync.md) §9), the thumbnail filler, the poller, the tree store and the
Baloo exclusion.

What sharing costs — a large pin in one account slows opens in another, and a reconnect walks every
folder in turn before any fill is served — is limitations log F43.

### 3.4 Which account an intercepted open belongs to

A fill request from the helper carries a request id and the event's descriptor, and nothing about
accounts. The hub decides, in this order, and stops at the first answer:

1. **By device.** `fstat` the descriptor. The accounts whose folder is on that filesystem — as it
   was when the folder was registered, never looked up per request — are the candidates. None
   means no account; exactly one is the answer — the common case of folders on different
   filesystems, and of a file moved out of its folder — unless some account has a folder whose
   device is not known (held back, or written down by a registration still under way), which could
   be the file's: then even one candidate is verified by the next steps.
2. **By path, verified.** The name the kernel has for the descriptor (`/proc/self/fd/<n>`) is
   looked up beneath each candidate folder that is a component prefix of it — opened with
   `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` and `O_PATH | O_NOFOLLOW`, so nothing is
   followed or filled — and is the answer only if its device and inode are the descriptor's.
3. **By item id.** The file's `user.konedrive.item-id` is looked up in each candidate's tree store,
   `items` then `staging`; item ids are unique across drives. This covers a file renamed or
   unlinked while its open was waiting. A tree store in use by a registration or a Forget at that
   moment is passed over, not waited for.
4. **None.** The request is answered `EIO`, and the next open tries again (limitations log F44).

Routing never guesses. Its answer selects the account's content source and its report
(`Transfers`, activity, `LocalBytes`).

### 3.5 Which account a path belongs to

The per-file calls — `Hydrate`, `Dehydrate`, `ItemState`, `Pin`, `Unpin` and `FreeUp` — are on the
daemon-wide `org.konedrive.Files1`, because the Dolphin plugin and `konedrivectl` know a path, not
an account. The account is the one whose folder is a component prefix of the path with its
directory part resolved — through `..`, and through a symbolic link such as `/home` to `/var/home`
or a link from one account's folder into another's — or, only when that cannot be resolved, of the
path as given, if it has no `.` or `..` in it. The last component is never resolved, and routing
never opens the file.
The account's folder then does what the call always did, beneath its own root. A path in no
account's folder is refused `OutsideRoot`; `ItemState` answers `not-managed`.

`Pin`, `Unpin` and `FreeUp` take many paths. Every path is routed before anything changes, so one
path in no account's folder refuses the whole call. Every account's paths are then checked as that
account would check them before any account acts — each is in its folder and one of ours; for
`Unpin` and `FreeUp`, no folder above it pins it; for `FreeUp`, a folder with interception has its
helper — so a call that spans accounts is refused as a whole or not at all. The counts in the
answer are summed over the accounts.

### 3.6 The helper is unchanged

The helper already held several roots per uid, and nothing in it changed:

- roots are keyed by root id, each with its uid, and a uid may hold any number of them;
- a root inside or containing another registered root is refused, whoever owns it, so two
  accounts' intercepted folders cannot nest even without the daemon's own check (§6.3);
- the limits of 16 connections and 8 waiting workers are per uid, and the credit of 64 requests
  per connection; a user with several accounts is still one uid, with one daemon and one
  connection;
- unregistering one account's folder briefly withholds ignore marks in the user's other folders
  too, since that guard is kept per uid: an extra interception, never zeros.

The helper knows users, not accounts, so [SECURITY.md](../../SECURITY.md) and
[hydration.md](hydration.md) §11 hold as written.

## 4. Configuration and files

### 4.1 `config.toml`

```toml
config_version = 2
client_id = "0f8fad5b-d9cb-469f-a165-70867728950e"   # one Entra application for every account

[[accounts]]
id = "3f9a1c0e5b7d"
label = "Personal"
mode = "read-only"
origin = "migrated"
drive_id = "D1A2B3C4"
legacy_token = true          # only until the single-account wallet item is moved (§8.4)
migrate_files = true         # only until account.json and tree.sqlite are moved (§8.3)

[accounts.root]              # absent while the account has no folder
path = "/home/ann/OneDrive"
id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d"          # the folder's user.konedrive.root
intercepted = true
source = "onedrive"
baloo_excluded = true

[[accounts]]
id = "8c21d07a44e1"
label = "Family"
mode = "read-only"
origin = "added"
drive_id = "E5F6A7B8"
```

The fields of `[accounts.root]` are the single-account file's `sync_root_*` fields, with the same
defaults: a missing `intercepted` reads as `true`, a missing `source` as `local`, and
`upgrade_when_helper` is written only when it was decided ([hydration.md](hydration.md) §14.4).

**One owner.** Every change — the client id, a label, a drive, the migration's flags, a folder
registered or forgotten — goes through `ConfigStore::update`, which re-reads the file, applies the
change and writes the result atomically (a temporary file, `fsync`, rename, `fsync` of the
directory), holding one lock across the three. A change that is refused, or changes nothing, writes
nothing. A check and the write it allows are one step, which the identity check at sign-in relies on
(§6.2). Before this, the account and the sync each rewrote the file on their own, and one could
save over the other's change (limitations log F37, closed).

**What cannot be read is never overwritten.** A file that cannot be parsed, or that a newer version
wrote (`config_version` above 2), *poisons* the store for the life of the process: no account is
loaded, every write is refused, and `Accounts1.LastError` names the file and the reason. A later
start with the file fixed loads it — or migrates it — then.

**Validation at load.** Accounts are checked in file order, and nothing is rewritten:

- an account whose id is not 12 lowercase hexadecimal characters, or repeats an earlier account's
  id, is not loaded at all: it could name neither an object nor a directory. `Accounts1.LastError`
  says so;
- an account whose label (in any case), drive, folder root id or folder path collides with an
  earlier account's — a folder that is, is inside, or contains an earlier one — is loaded and shown
  but *held back*: its folder is not brought up, its `RootState` is `error`, its `LastError` names
  the collision, and a registration is refused (limitations log F48);
- a `mode` other than `read-only` or `read-write` loads as `read-only`, is logged, and is written
  back as `read-only` with the next change; a `read-write` the write gate does not let through
  loads as written, and the account runs read-only (§10).

`write_test_drive_ids`, at the top of the file, is the write gate's list (§10): absent, as it is
unless the developer install writes it by hand, no account can be read-write.

### 4.2 Where each account's data lives

| What | Where |
|---|---|
| Configuration | `~/.config/konedrive/config.toml`, one for every account; `config.toml.v1` keeps the single-account file after the migration |
| Tree store, activity log, conflicts | `$XDG_STATE_HOME/konedrive/accounts/<id>/tree.sqlite` |
| Cached name and quota | `$XDG_STATE_HOME/konedrive/accounts/<id>/account.json` |
| Refresh token | the Secret Service, one item per account (§4.3) |
| Rescued local changes | `$XDG_DATA_HOME/konedrive/rescued/<id>/<time>/…`, or `.konedrive-rescued-<folder name>` beside the folder, which is one per folder already |
| The folder's drive | `user.konedrive.drive` on the folder's root directory (§6.3) |
| The helper's registrations | one entry per intercepted folder in `/var/lib/konedrive/roots.json`, in the same format as before |
| Thumbnails | the freedesktop cache, shared by every account: it is keyed by file URI |

`accounts/<id>/` is created with mode `0700`, and everything in it goes when the account is
removed. The rescue directory is outside it on purpose: rescued files are the user's, and stay
(§7.3).

### 4.3 Refresh tokens

Each account's refresh token is a Secret Service item of its own, with the attributes
`application=konedrive`, `kind=account-refresh-token` and `account=<id>`, labelled
`KOneDrive: <email>` once the email is known and `KOneDrive refresh token` before. The
single-account item was `application=konedrive`, `kind=refresh-token`. The `kind` differs, rather
than an `account` attribute being added alone, because a Secret Service search matches every item
whose attributes *include* the ones asked for: a search for the old item would otherwise find every
account's too, and telling them apart would mean reading attributes that a locked wallet may not
show. The access token stays in the daemon's memory, one per account ([sync.md](sync.md) §12.2).

## 5. On the bus

| Object | Interfaces |
|---|---|
| `/org/konedrive/Accounts` | `org.konedrive.Accounts1`, `org.konedrive.Files1`, `org.freedesktop.DBus.ObjectManager` |
| `/org/konedrive/Accounts/<id>` | `org.konedrive.Account1`, `org.konedrive.Sync1`, `org.konedrive.Dev1` |

`Accounts1.Accounts` lists the account objects in account order and changes with
`PropertiesChanged`. The `ObjectManager` announces each account object with `InterfacesAdded` once
it is on the bus and `InterfacesRemoved` when it goes, which is what generic tools (`busctl`,
D-Spy) understand; konedrive's own clients follow `Accounts`. The members are in
[desktop.md](desktop.md) §2.

Nothing answers any more at `/org/konedrive/Daemon`, the single-account object, and there is no
alias for it. Every client of the daemon changed with it and ships in the same packages; an alias
would need a meaning for "the first account" that changes when that account is removed, and would
double every signal. A window or a Dolphin that was running across the upgrade has to be
restarted (limitations log F46).

## 6. Keeping accounts apart

Three checks, each where the wrong account could otherwise act.

### 6.1 Every cycle: the folder's drive is still the account's

As before ([sync.md](sync.md) §12.3), each cycle of a OneDrive folder begins with `GET /me/drive`
on its own account's token, and compares the drive id with the account's `drive_id` in
`config.toml` and with the copy in its tree store. A mismatch is a blocking error, never a listing.
An account with no drive recorded yet — a migrated one whose folder never recorded it — records it
from its first successful `GET /me/drive`: its first cycle, or `RefreshAccountInfo`, which reads
the drive id and the quota from the same request. A drive another account has already is never
recorded a second time: the cycle is then a blocking error naming that account, and nothing is
listed into the folder.

### 6.2 At sign-in: an account is one drive, and a drive is one account

After the code exchange, and before the refresh token is stored, the daemon asks `GET /me/drive`
(and `GET /me`, for the email) with the new access token. Every other account that has no drive
recorded yet and may be signed in to one — signed in, or holding a stored refresh token it has not
used, as a wallet that did not answer at startup leaves it — is first asked for its own. Then,
inside one `ConfigStore` update, so that two sign-ins cannot both pass:

- this account has a drive and it is another one: refused — "This account is `<email>`. You
  signed in as a different Microsoft account; to connect that one, add a new account.";
- another account has this drive: refused — "This Microsoft account is already connected as
  '`<label>`'.";
- such an account still has no drive recorded: refused — "Could not check which account this is;
  try again.";
- otherwise the drive is recorded if the account had none, the refresh token is stored, and the
  account is signed in. A token that cannot be stored — the wallet locked, its prompt refused —
  fails the sign-in, and the drive recorded for it is taken back.

A refused sign-in stores nothing: its tokens are dropped, `State` returns to `signed-out`, and
`LastError` says why. The browser round trip means that the refusal cannot be an error of
`BeginSignIn` itself. The check is fail-closed: a `GET /me/drive` that fails refuses the sign-in
too (limitations log F42). A failed `GET /me` does not; the wallet item then keeps its generic
label.

### 6.3 At registration: a folder belongs to one account

- **No nesting.** `RegisterRoot` and `RegisterRootWithoutInterception` refuse, with `Overlaps`, a
  folder that is, is inside, or contains another account's folder — registered, held back, or only
  recorded in `config.toml` — comparing the resolved paths by component and the directories by
  device and inode. The message names the other account. The helper would refuse the intercepted
  case anyway (`EINVAL`); checking first gives the refusal a name, and covers a folder without
  interception, which the helper never sees. Every registration, whichever account makes it, holds
  one lock from this check to its end, so two accounts cannot both pass it with folders that nest.
- **A folder remembers its drive.** A OneDrive folder's root directory carries
  `user.konedrive.drive`, the drive id of the account it shows, written — in the read-only lock's
  write window, like `user.konedrive.root` — once the drive is known: at registration or at the
  cycle that first learns it, and at the first bring-up of a folder registered before multiple
  accounts. A folder that already carries a root id may be registered again without being empty
  ([hydration.md](hydration.md) §14.1); with several accounts, that would let one account adopt
  another's forgotten folder and reconcile its own drive over it. So a registration of a folder that
  carries another drive is refused `NotEmpty`: "this folder holds another OneDrive account's files;
  choose an empty folder". An empty folder holds nothing to adopt: its stale drive is taken off,
  and the registration goes ahead — Remove, then Add, on the same folder works once the folder is
  emptied. A local folder carries no drive. Neither does a folder forgotten before multiple
  accounts, which any account can therefore still adopt (limitations log F45).
- **A hand-edited configuration** whose folders collide is held back at load (§4.1).

## 7. Adding and removing

### 7.1 The client id

Every account signs in with one Entra application, `Accounts1.ClientId`. `SetClientId` accepts the
canonical GUID form only, and is refused while any account is signing in or signed in, as it was
for the single account.

### 7.2 Add

`Accounts1.Add(label)` adds a signed-out, read-only account with no folder and no drive, after every
other, and answers its object path; the object is on the bus by the time the call answers, and
`Accounts` changes. A label the rules refuse (§2) is `InvalidArgs`, with the reason. Signing in
(`Account1.BeginSignIn`) and choosing a folder (`Sync1.RegisterRoot`) are separate calls, made on the
account's own object as they were for the single account.

The window's **Add Account** dialog makes the first three calls in a row: `SetClientId` when no
client id is set yet, `Add`, then `BeginSignIn` on the new account, whose URL it opens in the
browser. They are three calls, not one transaction: a failure part way keeps what succeeded
(limitations log A15).

### 7.3 Remove

`Accounts1.Remove(account)`:

1. forgets the account's folder exactly as `Sync1.UnregisterRoot` does — so it is refused
   `NoHelper`, before anything changes, for an intercepted folder while no helper is connected
   ([hydration.md](hydration.md) §14.5). That holds for an account held back at load (§4.1) too:
   its folder was never brought up, but one it registered with interception in an earlier session
   is still the helper's, and is forgotten through the helper by the root id `config.toml`
   records. Under the same lock, the account is retired: from then on no folder is registered or
   brought up for it;
2. signs the account out, which cancels a sign-in under way and deletes its refresh token (both
   items, while the single-account one has not been moved yet, §8.4). The account is retired
   here too: no sign-in is begun, and none under way is stored, so no call on the account's own
   objects can slip in between the steps;
3. takes the account out of `config.toml`;
4. deletes `accounts/<id>/`: the cached name and quota, the tree store, the activity log and the
   conflict list;
5. takes the account's object off the bus.

What stays: the folder's files, as a Forget leaves them — unlocked, and a file that was never
downloaded left as an empty placeholder, which reads as zeros — and the rescued files, in
`rescued/<id>/` (limitations log F47). A path that names no account is refused `NoAccount`.

`Add`, `Remove` and `SetClientId` run one at a time.

## 8. From one account to several

### 8.1 When

At the first start of a daemon with multiple accounts, a `config.toml` with no `config_version` is
a single-account file, version 1. It is migrated as it is loaded, before anything is exported or
registered. The helper is not involved: root ids do not change, so its `roots.json` stays valid.

### 8.2 Steps

1. A version-1 file that cannot be read is not migrated, and nothing is written: the store is
   poisoned (§4.1).
2. Is there an account to carry over? Yes if there is a registered folder, the cached
   `account.json` or a `tree.sqlite` at the single-account paths, or the single-account refresh
   token in the wallet — asked through the presence check that needs no unlock, with 10 s to
   answer; a wallet that does not answer counts as yes. No: version 2 with the client id and no
   account.
3. Yes: the account "Personal" — a fresh id, `read-only`, `migrated`, the drive the folder
   recorded (possibly none) as its drive, both migration flags set, and the folder from the
   `sync_root_*` fields.
4. The version-1 file is copied to `config.toml.v1` (mode `0600`), then version 2 is written
   atomically. That write is the commit point: before it, the old layout is untouched; after it, the
   configuration names files that may still be at their old paths, which the next step moves.

### 8.3 Moving the files

At every start, for each account whose `migrate_files` is set, before its services open anything:

- `account.json` is renamed into `accounts/<id>/`, unless a file is there already;
- `tree.sqlite` is opened and closed once — the last connection's close folds the write-ahead log
  into the database and removes it — then renamed into `accounts/<id>/`, and a leftover `-shm`
  removed;
- the flag is cleared.

A store whose write-ahead log survives the close (another process has it open), or whose place is
taken, is left where it is and logged: the account lists its drive again into a new store, and the
activity log and conflict list of before are lost (limitations log F40). An unexpected error — a
directory that cannot be created, a refused rename — keeps the flag for the next start and shows in
`Accounts1.LastError`.

Every step is idempotent — a source that is gone means the step is done — so the next start
finishes whatever a crash interrupted:

| Crash | The next start finds | And does |
|---|---|---|
| before the version-2 write | version 1, the old files | migrates from the start, under a new id; nothing had moved |
| after it, before the moves | version 2 with `migrate_files`, the old files | moves them |
| between the two moves | one file moved, one not | moves the other |
| before the flag is cleared | both files moved | nothing to move; clears the flag |

### 8.4 The wallet item

Moving the refresh token means reading it, and reading it may ask to unlock the wallet. So it moves
lazily, on the account's first token load, which its first token refresh makes anyway:

1. the account's own item is used if it exists;
2. otherwise the single-account item is read, stored as the account's own, and deleted;
3. `legacy_token` is cleared.

While `legacy_token` is set, the presence check at startup sees either item, and a sign-out deletes
both. A crash between storing the new item and deleting the old leaves both; the next load prefers
the new one and deletes the old. No unlock prompt is added.

### 8.5 What the user sees

The window's one account is now "Personal". Its Places entry, "OneDrive", becomes
"OneDrive — Personal" in the same place in the panel. The tray and the notifications read as they
did with one account. Files rescued before stay in `rescued/<time>/`, and their conflicts, which
record absolute paths, still find them.

There is no way back: an older daemon reads version 2 as a configuration with no folder, and its
next write of the file drops every account. `config.toml.v1` is the way back, by hand (limitations
log F41).

## 9. The clients

- **The window** shows one account at a time. An account switcher heads the sidebar; Status,
  Activity, Conflicts, Not in the Folder and Account show the account chosen there, and Settings is
  the whole app's ([desktop.md](desktop.md) §4).
- **The tray icon** shows the worst state across the accounts, and its tooltip has a line per
  account ([desktop.md](desktop.md) §5).
- **Notifications and download progress** name the account once there are several
  ([desktop.md](desktop.md) §6, §7).
- **Places** has one entry per account folder, named `OneDrive — <label>`
  ([desktop.md](desktop.md) §4).
- **The Dolphin plugins** call `Files1`, which finds the account by path; the emblems read each
  file's attributes, as before, and need no account at all ([desktop.md](desktop.md) §10).
- **The command line** chooses the account with the global option `--account <id | label |
  email>`, or `KONEDRIVE_ACCOUNT`; with one account, it needs neither. It has `account list`,
  `account add`, `account rename` and `account remove`; `status` and `sync status` show every
  account when none is chosen; and the path commands (`sync hydrate`, `dehydrate`, `state`, `pin`,
  `unpin`, `free`) go through `Files1`, where the path decides the account
  ([desktop.md](desktop.md) §3; limitations log F50, F51). `login` with no account at all first
  adds one called `Personal`, so the single-account setup — `set-client-id`, `login`,
  `sync register` — works as it did. A name that fits more than one account (possible only in a
  hand-edited `config.toml`) is refused with exit status 2, never taken as the first. Every
  command the CLI suggests names its account with `--account` whenever there are several or
  `KONEDRIVE_ACCOUNT` is set, and with several accounts each success line starts with the
  account's label.

```text
$ konedrivectl account list
ID            LABEL     EMAIL            STATE       MODE       FOLDER
3f9a1c0e5b7d  Personal  ann@outlook.com  signed-in   read-only  /home/ann/OneDrive (ready)
8c21d07a44e1  Family    —                signed-out  read-only  —
$ konedrivectl --account family login
```

## 10. The mode

Every account has a mode, `read-only` or `read-write`, stored in `config.toml`; a new account is
read-only. [writes.md](writes.md) §2 has what the mode changes in the folder; in short:

- **The mode it runs in.** `Account1.Mode` is `read-write` only while `config.toml` says so, the
  write gate lets the account's drive through, the drive its token was last seen to reach is that
  drive, and the scopes its last token response granted — the scopes and the drive kept in
  `account.json` — include `Files.ReadWrite`. Otherwise it is `read-only`, and when `config.toml`
  says read-write, `LastError` says why (limitations log F61).
- **The scope follows it.** Read-only asks for `Files.Read User.Read offline_access`, read-write
  for `Files.ReadWrite User.Read offline_access`, at the sign-in and at every refresh
  ([sync.md](sync.md) §12.1). A read-only account keeps asking for `Files.Read`, a subset of any
  grant, so Microsoft keeps refusing its writes even when its grant is wider.
- **To read-write**, `Account1.SetMode("read-write", false)` answers a sign-in URL, and the sign-in
  asks for `Files.ReadWrite`, pinned to the account (its password asked for again, its email filled
  in). Only when its token response grants that, for this account's own drive, are the new refresh
  token stored and `mode = "read-write"` written; a cancelled, refused or foreign sign-in changes
  nothing. Consent given in the browser stays with Microsoft (limitations log F66). **To read-only**, no sign-in: the mode is written, and the
  next refresh asks for `Files.Read`. It is refused `PendingUploads` while changes wait to be
  uploaded, unless forced.
- **The folder follows the mode.** A read-write folder is kept without the read-only lock; the
  switch takes it off, or puts it back, with a walk of the folder (limitations log F62, F65).
- **The write gate.** While uploads are being developed, only an account whose drive id is in
  `write_test_drive_ids` can be read-write: `SetMode("read-write")` is refused `WritesNotAllowed`
  for any other (limitations log F60). The release removes the gate.

`konedrivectl account mode [read-only|read-write] [--force]` shows or switches the mode. In the
window it is the Account page's switch "Upload changes made on this computer", which explains the
sign-in before it starts it and asks before it drops changes waiting to upload
([desktop.md](desktop.md) §4; limitations log A16).

## 11. Costs and limits

Recorded in [`../limitations-and-workarounds.md`](../limitations-and-workarounds.md):

- moving the single-account tree store can give up, and then the account lists its drive again and
  loses its earlier activity and conflicts (F40); there is no way back to a single-account version
  but `config.toml.v1` (F41);
- a sign-in is refused when the daemon cannot check which drive it reached (F42);
- every account shares one helper link, its fill slots and its credit, and a reconnect walks every
  folder in turn (F43);
- an intercepted open that cannot be matched to an account is refused `EIO` (F44);
- a folder forgotten before multiple accounts can be adopted by another account (F45);
- nothing answers at `/org/konedrive/Daemon`, so a client running across the upgrade needs a
  restart (F46);
- removing an account leaves its folder's placeholders as empty files, and keeps its rescued files
  by account id, not label (F47);
- an account that collides with an earlier one in a hand-edited `config.toml` is held back (F48);
- personal Microsoft accounts only (F49);
- only a test account can be read-write while uploads are being developed (F60); an account is
  read-write only while its token carries `Files.ReadWrite` (F61); the lock walks of a switch
  (F62), what a read-write folder's cycle keeps waiting for local changes (F112), the switch's
  outcome read from `Mode` and `LastError` (F64), file modes lost across a round trip (F65), and
  consent to write that stays with Microsoft (F66);
- a folder that the helper holds for an account taken out of `config.toml` by hand stays with the
  helper (Z6);
- in the window: one account at a time (A13), label rules checked by a copy of the daemon's (A14),
  Add Account as three calls (A15), the upload switch's own wait for its sign-in and one client id
  for all (A16), the tray's summary (A17), the account named in notifications and download progress
  (A18), one Places entry per account folder (A19), and the mass-delete notification (A20).

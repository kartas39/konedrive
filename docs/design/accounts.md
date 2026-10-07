# Multiple accounts

konedrive holds several OneDrive accounts at once, each with its own sign-in and its own folder.
This document describes what an account is, how one daemon serves several, how a path or an
intercepted open finds its account, where each account's data lives, what keeps two accounts apart,
how accounts are added and removed, and how a single-account installation becomes the first account.
How one folder follows its drive is in [sync.md](sync.md); how a file is filled is in
[hydration.md](hydration.md); the D-Bus API, the window and the tray are in
[desktop.md](desktop.md).

## 1. What the user sees

- Several personal Microsoft accounts, each with its own folder, Places entry, activity, conflicts
  and "Not in the Folder" list. Work or school accounts are not supported (issue #12). The helper
  holds at most 32 intercepted folders for one user (§3.6).
- Every account is read-only until the user switches it to read-write, which any signed-in account
  can be (§10).
- The window shows one account at a time, chosen in a switcher at the top of its sidebar; the tray
  has an icon for each, or one that sums them all up ([desktop.md](desktop.md) §4, §5).
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
| Id | 12 random lowercase hexadecimal characters (48 bits), checked against the ids present. It names the account's D-Bus object, its state and rescue directories and its Places entry; nobody has to type it |
| Label | The name people see and type. The daemon sets it when the account is added by signing in, to the account's email where it can (§7.2), and it can be renamed to anything the rules allow ("Personal", "Family"). Trimmed; 1 to 40 characters; no `/`, no control character; not 12 hexadecimal digits in any case, so a label is never taken for an id; unique among the labels regardless of case. It is not compared with the other accounts' emails. It can be changed at any time, and nothing on disk is named after it |
| Drive | The Graph drive id of the Microsoft account: the account's identity (§6). Empty until the first sign-in or the first `GET /me/drive`, then never changed |
| Mode | `read-only`, the default, or `read-write` (§10) |
| Origin | `migrated` for the account carried over from a single-account installation (§8), `added` for every other. A missing or unknown value reads as `migrated` |
| Folder | At most one registered folder, with its source (`onedrive` or `local`) and its registration mode ([sync.md](sync.md) §3, [hydration.md](hydration.md) §14) |

Accounts keep the order in which they were added: the order of the window's switcher, of the one
tray icon's tooltip and of every list of accounts. They cannot be reordered.

## 3. One daemon, several accounts

### 3.1 Components

```text
konedrived
 ├─ ConfigStore ────── config.toml: one owner, one lock, every change through it
 ├─ Registry ───────── the accounts as their folders see each other: the per-inode locks,
 │    │                 the fill-on-open loop and its router, the overlap check, the hold
 │    └─ HelperHub ──── the one link to konedrive-helper, its supervisor, HelperState
 ├─ AccountManager ─── /org/konedrive/Accounts: Accounts, Files, ObjectManager
 │    └─ Account <id> ── /org/konedrive/Accounts/<id>: Account, Folder, Transfers, UploadQueue,
 │         │                                           Conflicts, LocalScan, ActivityLog
 │         ├─ AccountService  sign-in, tokens, its wallet item, its drive
 │         └─ SyncService     its folder: registration, listing, tree store, activity,
 │                            conflicts, pins, replacements, thumbnails, Baloo
 └─ watchers ───────── the network, and power and metering, for every account's sync
```

| Component | Where | Responsibility |
|---|---|---|
| `ConfigStore` | `config/store.rs`, `config/migrate.rs` | Loads `config.toml`, migrates a single-account file (§8), and runs every change as re-read, change and atomic write under one lock (§4.1) |
| `AccountManager` | `daemon/manager.rs`, `daemon/startup.rs`, `dbus/accounts.rs` | The ordered list of accounts; startup, with the holding back of accounts that collide (§4.1); adding by sign-in, `Remove`, `SetClientId` and the hold's two settings; putting each account's objects on the bus and taking them off; routing per-file calls by path (§3.5) |
| `Account` | `daemon/manager.rs` | One account: its `AccountService`, its `SyncService`, its paths, and the tasks that turn their state into `PropertiesChanged` |
| `AccountService` | `account/`, `konedrive-graph/src/oauth.rs` | Per account: sign-in, tokens, the account's own wallet item (§4.3), its drive, and the identity check at sign-in (§6.2) |
| `SyncService` | `sync/` | Per account: everything [sync.md](sync.md), [hydration.md](hydration.md) and [pinning.md](pinning.md) describe for one folder, on the hub's link |
| `HelperHub` | `helper/hub.rs` | The link to the helper, its supervisor and `HelperState`, for every account (§3.3). It knows no account: it tells the registry as the link comes and goes |
| `Registry` | `sync/registry.rs` | The list of the accounts' folders, written only by `AccountManager` with its own list; which account an intercepted open belongs to (§3.4); the overlap check across accounts (§6.3); what every account is told alike (the hold's settings and the machine's conditions) |

### 3.2 Startup

Every object a client may call is on the bus before the name is claimed:

1. The daemon asks the bus whether `org.konedrive.Daemon` already has an owner, and stops if it has:
   a daemon of an older version still running could write a single-account configuration over the
   migrated one. It then takes `config.toml.lock` for the life of the process, and stops if another
   daemon holds it: two daemons started at once would both migrate, each under an account id of its
   own.
2. `config.toml` is loaded, and migrated if it is a single-account file (§8).
3. The files a migration still has to move are moved, before anything opens them (§8.3).
4. Each account is brought up in order: its session restored from the wallet's presence check and
   its cached name and quota, and an intercepted folder held as registered until the helper is back.
5. `/org/konedrive/Accounts` and every account's object are exported, and only then is
   `org.konedrive.Daemon` claimed.
6. Every folder that needs no helper is brought up; the hub's supervisor, the `HelperState` watcher
   and the watchers of the network and of power and metering start. `main` keeps the four: when the
   supervisor or the `HelperState` watcher ends, or any of them panics, the daemon stops as on a
   signal and exits with a failure, and systemd starts it again after five seconds
   (`Restart=on-failure`).

When the helper connects, a panic in one account's bring-up is caught: that folder is down and says
so, and the others come up. The bring-up of step 6 has no such guard.

The daemon's connection is built with the `ObjectManager` of `/org/konedrive/Accounts` already on
it. This is what makes the connection answer calls before it reads its first message: a connection
that gets its first object later starts answering a moment after it starts reading, and a call read
in between is dropped with no reply. The call that started the daemon over D-Bus is delivered as
soon as the name is claimed, so it must not be lost.

### 3.3 One helper link for every account

The helper sends each intercepted open to the newest connection of the file owner's uid
([hydration.md](hydration.md) §10.2). A second connection from the same daemon would take every
request away from the first, so the daemon keeps one link and every account shares it. On connect
the hub brings every account's intercepted folder up again, one after another in account order —
each re-registers its root, whose walk the helper performs, and recovers it — and only then serves
fill requests. On loss it tells every account at once.

Shared by every account of the daemon:

- the link, and `HelperState`, which is published once, on `Accounts`; each account's
  `Folder.LastError` still begins with the helper's advice while its folder is known to wait for a
  helper that is down;
- the per-inode lock table: an inode belongs to one account only, so one table serves them all;
- the fill-on-open loop, which takes at most 64 requests at once, the helper's credit for one
  connection.

Per account: the transfer pool, whose slots that account's fills, pinned downloads, replacements and
thumbnails take ([hydration.md](hydration.md) §6.4), the eight workers of replacements
([sync.md](sync.md) §9), the poller, the tree store and the Baloo exclusion. A request the loop has
taken waits for a slot of its own account's pool, so one account's downloads do not take another's
slots.

What sharing costs: a reconnect walks every folder in turn before any fill is served, and the 64
requests are one budget for all accounts (limitations log F43).

### 3.4 Which account an intercepted open belongs to

A fill request from the helper carries a request id and the event's descriptor, and nothing about
accounts. The registry of the accounts' folders (`sync/registry.rs`) decides, in this order, and
stops at the first answer:

1. **By a move out.** While any account has objects that left its folder and are not settled yet
   ([writes.md](writes.md) §8), the file's `user.konedrive.item-id` is read: an id among them is
   that account's, whatever folder the file is in now.
2. **By device.** `fstat` the descriptor. The accounts whose folder is on that filesystem — as it
   was when the folder was registered, never looked up per request — are the candidates. None means
   no account; exactly one is the answer — the common case of folders on different filesystems —
   unless some account has a folder whose device is not known (held back, or written down by a
   registration still under way), which could be the file's: then even one candidate is verified by
   the next steps.
3. **By path, verified.** The name the kernel has for the descriptor (`/proc/self/fd/<n>`) is looked
   up beneath each candidate folder that is a component prefix of it and still carries its root id —
   opened beneath the folder with no symbolic link followed, `O_PATH | O_NOFOLLOW`, so nothing is
   filled — and is the answer only if its device and inode are the descriptor's.
4. **By item id.** The file's `user.konedrive.item-id` is looked up in each candidate's tree store,
   `items` then `staging`; item ids are unique across drives. This covers a file renamed or unlinked
   while its open was waiting. A tree store in use by a registration or a Forget at that moment is
   passed over, not waited for.
5. **None.** The request is answered `EIO`, and the next open tries again.

Routing never guesses. Its answer selects the account's content source, its transfer pool and its
report (`Transfers.Downloads`, activity, `LocalBytes`).

### 3.5 Which account a path belongs to

The calls of the daemon-wide `org.konedrive.Files` ([desktop.md](desktop.md) §2.9) take a path,
because the Dolphin plugin and `konedrivectl` know a path, not an account. The account is the one
whose folder is a component prefix of the path with its directory part resolved — through `..`, and
through a symbolic link such as `/home` to `/var/home` or a link from one account's folder into
another's — or, only when that cannot be resolved, of the path as given, if it has no `.` or `..` in
it. The last component is not resolved (a path that ends in `..` is resolved whole), and routing
never opens the file. Only an account whose folder is registered is found. The account's folder then
does what the call does, beneath its own root. A path in no account's folder is refused
`OutsideRoot`; `ItemState` answers `not-managed`, and `Menu` leaves the path out.

`Pin`, `Unpin` and `FreeUp` take many paths. Every path is routed before anything changes, so one
path in no account's folder refuses the whole call. Every account's paths are then checked as that
account would check them before any account acts ([pinning.md](pinning.md) §3, §5), so a refusal the
checks can give is given for the whole call, before anything changed. The accounts then act one
after another: a failure in a later account leaves what the earlier ones did. The counts in the
answer are summed over the accounts.

### 3.6 The helper is unchanged

The helper already held several roots per uid, and nothing in it changed:

- roots are keyed by root id, each with its uid, and a uid may hold up to 32 of them; one more is
  refused `EDQUOT`;
- a root inside or containing another registered root is refused, whoever owns it, so two accounts'
  intercepted folders cannot nest even without the daemon's own check (§6.3);
- the limits of 16 connections and 8 waiting workers are per uid, and the credit of 64 requests per
  connection; a user with several accounts is still one uid, with one daemon and one connection;
- unregistering one account's folder briefly withholds ignore marks in the user's other folders too,
  since that guard is kept per uid: an extra interception, never zeros.

The helper knows users, not accounts, so [SECURITY.md](../../SECURITY.md) and
[hydration.md](hydration.md) §11 hold as written.

## 4. Configuration and files

### 4.1 `config.toml`

```toml
config_version = 2
client_id = ""   # empty: konedrive signs in with its own built-in application; set to override it

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

Keys the example leaves out, each absent until it is set:

- at the top: `pause_on_metered` (absent: `true`) and `on_battery` (absent or unknown:
  `power-saver`), the hold's settings for every account ([writes.md](writes.md) §11); `[transfers]`
  with `max` and `large` ([hydration.md](hydration.md) §6.4); `write_test_drive_ids`, the drives a
  development build may export a read-write token for (§10): it decides no account's mode;
- in an account: `login_hint` (the email its sign-in is pinned to, §7.2, §10), `ignore` and
  `machine_name` ([writes.md](writes.md) §4.2, §7), `thumbnails` (absent: on,
  [desktop.md](desktop.md) §8) and `paused_until` (the user's pause, which outlasts a restart).

The file is written whole from what the daemon read, with mode `0600`: a key the daemon does not
know, and every comment, is gone at the next write.

**One owner.** Every change after the migration's own first write (§8.2) — the client id, a label, a
drive, the migration's flags, a folder registered or forgotten — goes through `ConfigStore::update`,
which re-reads the file, applies the change and writes the result atomically (a temporary file,
`fsync`, rename, `fsync` of the directory), holding one lock across the three. A change that is
refused, or changes nothing, writes nothing. A check and the write it allows are one step, which the
identity check at sign-in relies on (§6.2).

**What cannot be read is never overwritten.** A file that cannot be read or parsed, whose
`config_version` is not a whole number above zero, or that a newer version wrote (`config_version`
above 2), *poisons* the store for the life of the process: no account is loaded, every write is
refused, and `Accounts.LastError` names the file and the reason. So does a migration whose own
writes failed (§8.2). A later start with the file fixed loads it — or migrates it — then.

**Validation at load.** Accounts are checked in file order, and nothing is rewritten:

- an account whose id is not 12 lowercase hexadecimal characters, or repeats that of an account
  already loaded, is not loaded at all: it could name neither an object nor a directory.
  `Accounts.LastError` says so;
- an account whose label (in any case), drive, folder root id or folder path collides with an
  earlier account's — a folder that is, is inside, or contains an earlier one, by the paths as
  written — is loaded and shown but *held back*: its folder is not brought up, its `Folder.State` is
  `error`, its `LastError` names the collision, and a registration is refused `Failed`, saying to
  correct `config.toml` and start konedrived again;
- a `mode` that is a string other than `read-only` or `read-write` loads as `read-only`, is logged,
  and is written back as `read-only` with the next change; a `read-write` the account's token does
  not grant loads as written, and the account runs read-only until it does (§10).

### 4.2 Where each account's data lives

| What | Where |
|---|---|
| Configuration | `~/.config/konedrive/config.toml`, one for every account; `config.toml.v1` keeps the single-account file after the migration |
| Tree store, activity log, conflicts | `$XDG_STATE_HOME/konedrive/accounts/<id>/tree.sqlite` |
| Cached name, email and quota, and the scopes and the drive of the last token | `$XDG_STATE_HOME/konedrive/accounts/<id>/account.json` |
| Refresh token | the Secret Service, one item per account (§4.3) |
| Rescued local changes | `$XDG_DATA_HOME/konedrive/rescued/<id>/<time>/…`, or `.konedrive-rescued-<folder name>` beside the folder, which is one per folder already |
| The folder's drive | `user.konedrive.drive` on the folder's root directory (§6.3) |
| The helper's registrations | one entry per intercepted folder in `/var/lib/konedrive/roots.json` |
| Thumbnails | the freedesktop cache, shared by every account: it is keyed by file URI |

`accounts/<id>/` is created with mode `0700`, and everything in it goes when the account is removed.
The rescue directory is outside it on purpose: rescued files are the user's, and stay (§7.3).

### 4.3 Refresh tokens

Each account's refresh token is a Secret Service item of its own, with the attributes
`application=konedrive`, `kind=account-refresh-token` and `account=<id>`. It is labelled when it is
stored: `KOneDrive: <email>` once the daemon has learned an email for the account,
`KOneDrive refresh token` before. The single-account item was `application=konedrive`,
`kind=refresh-token`. The `kind` differs, rather than an `account` attribute being added alone,
because a Secret Service search matches every item whose attributes *include* the ones asked for: a
search for the old item would otherwise find every account's too, and telling them apart would mean
reading attributes that a locked wallet may not show. The access token stays in the daemon's memory,
one per account ([sync.md](sync.md) §12.2).

## 5. On the bus

| Object | Interfaces |
|---|---|
| `/org/konedrive/Accounts` | `org.konedrive.Accounts`, `org.konedrive.Files`, `org.freedesktop.DBus.ObjectManager`; in a development build, `org.konedrive.DevTools` |
| `/org/konedrive/Accounts/<id>` | `org.konedrive.Account`, `org.konedrive.Folder`, `org.konedrive.Transfers`, `org.konedrive.UploadQueue`, `org.konedrive.Conflicts`, `org.konedrive.LocalScan`, `org.konedrive.ActivityLog`; in a development build, `org.konedrive.TokenExport` |

`Accounts.List` lists the account objects in account order and changes with `PropertiesChanged`. The
`ObjectManager` announces each account object with `InterfacesAdded` once it is on the bus and
`InterfacesRemoved` when it goes, which is what generic tools (`busctl`, D-Spy) understand;
konedrive's own clients follow `Accounts.List`. The members are in [desktop.md](desktop.md) §2.

Nothing answers at `/org/konedrive/Daemon`, the single-account object, and there is no alias for it.
Every client of the daemon changed with it and ships in the same packages; an alias would need a
meaning for "the first account" that changes when that account is removed, and would double every
signal. A window or a Dolphin that was running across the upgrade has to be restarted.

## 6. Keeping accounts apart

Three checks, each where the wrong account could otherwise act.

### 6.1 Every cycle: the folder's drive is still the account's

Each cycle of a OneDrive folder begins with `GET /me/drive` on its own account's token
([sync.md](sync.md) §12.3), and compares the drive id with the one the folder's tree store records —
or, when the store records none, with the account's `drive_id` in `config.toml` as it was when the
sync started. A mismatch is a blocking error, never a listing.

An account with no drive recorded yet — a migrated one whose folder never recorded it — records it
from its first successful `GET /me/drive`: `RefreshInfo`, which reads the drive id and the quota
from the same request, or a cycle, whose reconcile writes it. A drive another account has already is
never recorded a second time: the cycle is then a blocking error naming that account, and nothing is
listed into the folder.

### 6.2 At sign-in: an account is one drive, and a drive is one account

After the code exchange, and before the refresh token is stored, the daemon asks `GET /me/drive`
(and `GET /me`, for the email) with the new access token. Every other account that has no drive
recorded yet and may be signed in to one — signed in, or holding a stored refresh token it has not
used, as a wallet that did not answer at startup leaves it — is first asked for its own. Then,
inside one `ConfigStore` update, so that two sign-ins cannot both pass:

- this account has a drive and it is another one: refused — "This account is `<email>`. You signed
  in as a different Microsoft account; to connect that one, add a new account.", with the account's
  label in quotes where the daemon holds no email for it, as after a sign-out;
- another account has this drive: refused — "This Microsoft account is already connected as
  '`<label>`'.";
- such an account still has no drive recorded: refused — "Could not check which account this is; try
  again.";
- otherwise the drive is recorded if the account had none, the refresh token is stored, and the
  account is signed in. A token that cannot be stored — the wallet locked, its prompt refused —
  fails the sign-in, and the drive recorded for it is taken back.

A refused sign-in stores nothing: its tokens are dropped, `State` returns to `signed-out`, and
`LastError` says why. The browser round trip means that the refusal cannot be an error of
`BeginSignIn` itself. The check is fail-closed: a `GET /me/drive` that fails refuses the sign-in
too, and so does an answer with no refresh token. A failed `GET /me` does not; the wallet item is
then labelled with the email the daemon last learned for the account, or generically.

### 6.3 At registration: a folder belongs to one account

- **No nesting.** `Folder.Register` and `RegisterWithoutInterception` refuse, with `Overlaps`, a
  folder that is, is inside, or contains another account's folder — registered, held back, or only
  recorded in `config.toml` — comparing the resolved paths by component and the directories by
  device and inode. The message names the other account. The helper would refuse the intercepted
  case anyway; checking first gives the refusal a name, and covers a folder without interception,
  which the helper never sees. Every registration, whichever account makes it, holds one lock from
  this check to its end, so two accounts cannot both pass it with folders that nest.
- **A folder remembers its drive.** A OneDrive folder's root directory carries
  `user.konedrive.drive`, the drive id of the account it shows, written once the drive is known and
  only where the folder carries none: at registration or at the cycle that first learns it, and at
  the first bring-up of a folder registered before multiple accounts. A folder that already carries
  a root id may be registered again without being empty ([hydration.md](hydration.md) §14.1); with
  several accounts, that would let one account adopt another's forgotten folder and reconcile its
  own drive over it. So a registration of either kind, of a folder that carries another drive than
  the account's — any drive, for an account that has none recorded — is refused `NotEmpty`: "this
  folder holds another OneDrive account's files; choose an empty folder". An empty folder holds
  nothing to adopt: its stale drive is taken off, and the registration goes ahead — Remove, then
  Add, on the same folder works once the folder is emptied. Where the drive cannot be taken off, the
  folder is registered all the same and keeps the other account's drive. A local folder carries no
  drive. Neither does a folder forgotten before multiple accounts, which any account can therefore
  still adopt.
- **A hand-edited configuration** whose folders collide is held back at load (§4.1).

## 7. Adding and removing

### 7.1 The client id

Every account signs in with one Entra application, `Accounts.ClientId`. konedrive ships its own
(`DEFAULT_CLIENT_ID`), so signing in needs nothing from the user; `config.toml`'s `client_id`
overrides it for anyone who registers their own. `SetClientId` trims what it is given and accepts
the canonical GUID form only, and is refused while any account is signing in or signed in.

### 7.2 Add

An account is added only by signing in. **`Accounts.SignIn() → (u sign_in, s url)`** starts a
sign-in for a new account and answers its number and the URL to open in a browser. The window's
**Sign in…** and `konedrivectl account add` both call it, and nothing else.

The sign-in belongs to no account. The manager runs it: the loopback listener, the authorization URL
(read-only scopes, Microsoft's account picker), the wait for the browser (five minutes), the
exchange, and the question of which drive and email this is. Until all of that has succeeded and the
drive is known not to be another account's, nothing exists for it: no account id, no entry in
`config.toml`, no `accounts/<id>/`, no stored token, no object on the bus. The steps of the attempt
are the ones an account's own sign-in runs (`Account.BeginSignIn`, the switch to read-write): one
piece of code, `account/attempt.rs`.

A client holds only the sign-in's number: to cancel it
(`Accounts.CancelSignIn(u sign_in) → (b cancelled)`) and to know which `SignInFinished` is its own.
Numbers are not reused while the daemon runs. `SignIn` is refused `Failed`, with nothing started,
when `config.toml` cannot be written (the account could not be written at the end) or the listener
cannot be bound. There is one sign-in for a new account at a time: a `SignIn` while one is under way
ends that one as `cancelled` first, so a client that died in the middle never blocks the next
attempt. A refused call ends nothing: `SignIn` ends the one under way only once nothing can refuse
the new one any more.

One signal, `Accounts.SignInFinished(u sign_in, s outcome, s message, o account)`, is sent for every
sign-in that `SignIn` answered, unless the daemon stopped first:

| Outcome | When | `message` | `account` |
|---|---|---|---|
| `signed-in` | the account was made and is in `List` | its label | its path |
| `cancelled` | `CancelSignIn`, a newer `SignIn`, or `SetClientId` | empty | `/` |
| `already-added` | the drive is another account's (§6.2) | that account's label | that account's path |
| `failed` | anything else, the timeout included | why | `/` |

Only `signed-in` made an account.

- **The account is made, already signed in,** once the browser sign-in has succeeded. The other
  accounts' drives are settled first (§6.2), which asks Graph, before the manager's lock is taken;
  the rest runs with that lock held (the one `Remove` and `SetClientId` take). First the identity
  guard is asked as an account's own sign-in asks it (§6.2): a drive that is another account's gives
  `already-added`, any other refusal `failed`, and nothing was made. The check and the entry it
  allows — the label, the drive and the `login_hint` — are one write of `config.toml`, which reads
  the file again, so a drive recorded since the settling is seen. Then the account is built
  (`accounts/<id>/`), its token stored and its session signed in; the wallet is asked with the lock
  held (limitations log A31). Last it is put on the bus and announced in `Accounts.List`, and
  `SignInFinished` is sent.
- **A step that fails** takes back what the steps before it made — the token, the entry, the
  directory, the interfaces — and the outcome is `failed`. If `config.toml` cannot be written at
  that moment the entry stays, and comes up as a signed-out account at the next start. The account
  comes onto the bus and into `List` only signed in and under its final label; its name and quota
  are read on their own and may arrive after `signed-in`.
- **The label** is the account's email. A name never decides whether an account is already added;
  only the drive does. When another account already has the email as its label (compared without
  case), the label is the first free one of `<email> 2`, `<email> 3`, and so on. When there is no
  email, or the rules of §2 refuse it (longer than 40 characters, a `/` in it), it is the first free
  one of `Personal`, `Personal 2`, `Personal 3`, and so on.
- **A cancel** is never refused and waits for nothing. It answers whether it cancelled: `true` when
  this call ended the sign-in, and `cancelled` follows. A sign-in can be cancelled until its account
  starts being made. For a number that is not under way — ended already, or its account being made —
  it answers `false` and changes nothing: the sign-in's own `SignInFinished` says how it ended,
  `signed-in` when the account was made. So `konedrivectl account add`, after Ctrl-C or its time
  limit, says "cancelled" or "timed out" on `true`; on `false` it waits for the outcome and prints
  by it.
- **`SetClientId`** ends a sign-in under way as `cancelled` only after its own write has succeeded:
  refused, it ends nothing. A sign-in under way does not refuse it, being no account (§7.1).
- **A client that goes away** (the window closed, `konedrivectl` killed) leaves its sign-in under
  way: one finished in the browser after that still adds the account, and otherwise it ends at the
  timeout or at the next `SignIn`.
- **A daemon that goes away** sends no signal. A client does not wait for one: the window and
  `konedrivectl account add` each end the adding when the daemon's name leaves the bus, with an
  error that says the daemon stopped. What the daemon had made by then stays: the entry is written
  before the token is stored, so what can be left is a signed-out account under its final label, or
  the whole account, never a token no account owns (issue #231).

`Account.BeginSignIn`, `Account.CancelSignIn` and `Account.SetLabel` are for an account that exists:
signing a signed-out account in again (`konedrivectl login`) and renaming use them. Choosing a
folder (`Folder.Register`) is a separate call on the account's own object.

There is no adding a signed-out account under a chosen name. Only a development build (`dev-tools`)
can still make one, for a folder that shows a local directory (`docs/developing.md`, "A folder without OneDrive or
the helper"): `org.konedrive.DevTools.AddAccount(s label) → o`, which
`konedrivectl dev add-account <label>` calls. A label the rules refuse (§2) is `InvalidArgs`, with
the reason. An account whose objects cannot be put on the bus is refused `Failed`, and what was made
is taken back as above.

### 7.3 Remove

`Accounts.Remove(account)`:

1. forgets the account's folder exactly as `Folder.Unregister` does — so it is refused, before
   anything changes, `NoHelper` for an intercepted folder while no helper is connected and
   `PendingUploads` while changes wait to be uploaded ([hydration.md](hydration.md) §14.5). That
   holds for an account held back at load (§4.1) too: its folder was never brought up, but one it
   registered with interception in an earlier session is still the helper's, and is forgotten
   through the helper by the root id `config.toml` records. A OneDrive folder's tree store goes
   here. Under the same lock, the account is retired: from then on no folder is registered or
   brought up for it;
2. signs the account out, which cancels a sign-in under way, deletes its refresh token (and the
   single-account item, while that has not been moved yet, §8.4) and its cached name and quota. The
   account is retired here too: no sign-in is begun, and none under way is stored, so no call on the
   account's own objects can slip in between the steps;
3. takes the account out of `config.toml`;
4. deletes `accounts/<id>/` with what is left in it;
5. takes the account's object off the bus.

What stays: the folder's files, as a Forget leaves them — unlocked, and a file that was never
downloaded left as an empty placeholder, which reads as zeros — and the rescued files, in
`rescued/<id>/` (limitations log F47). A path that names no account is refused `NoAccount`, and any
removal `Failed` while `config.toml` cannot be read (§4.1).

A removal that fails at step 2 (the account's refresh token cannot be deleted) or at step 3 (the
account cannot be taken out of `config.toml`) is refused under the name the failure has (`Failed`;
`NoAccount` when `config.toml` no longer holds the account), and the account stays, no longer
retired: it is listed, takes a folder, a sign-in and a mode as before, and can be removed again. (An
account `config.toml` no longer holds is the exception: it goes only when konedrived starts again,
and the refusal says that.) What the steps before the failure did is not taken back: a folder that
was forgotten stays forgotten, a sign-in under way is given up, and after step 2 the account is
signed out. The refusal says what failed and what was done: whether the account is signed out, and
whether its folder is no longer registered, was forgotten while `config.toml` still records it, or
was left as it was.

Whatever changes the accounts or the settings they share — the start of a `SignIn`, the making of
its account, `Remove`, `SetClientId`, the hold's two settings — runs one at a time.

## 8. From one account to several

### 8.1 When

At the first start of a daemon with multiple accounts, a `config.toml` with no `config_version`, or
with `config_version = 1`, is a single-account file. It is migrated as it is loaded, before anything
is exported or registered. The helper is not involved: root ids do not change, so its `roots.json`
stays valid.

### 8.2 Steps

1. A version-1 file that cannot be read is not migrated, and nothing is written: the store is
   poisoned (§4.1).
2. Is there an account to carry over? Yes if there is a registered folder, or the cached
   `account.json` or a `tree.sqlite` at the single-account paths (or their presence cannot be
   checked). Only when none of those says yes is the wallet asked for the single-account refresh
   token, through the presence check that needs no unlock, with 10 s to answer; a wallet that does
   not answer, or answers with an error, counts as yes. No: version 2 with the client id and no
   account.
3. Yes: the account "Personal" — a fresh id, `read-only`, `migrated`, the drive the folder recorded
   (possibly none) as its drive, both migration flags set, and the folder from the `sync_root_*`
   fields.
4. The version-1 file is copied to `config.toml.v1` (mode `0600`), then version 2 is written
   atomically. That write is the commit point: before it, the old layout is untouched; after it, the
   configuration names files that may still be at their old paths, which the next step moves. If
   either write fails, the store is poisoned for this run.

### 8.3 Moving the files

At every start, for each account whose `migrate_files` is set, before its services open anything:

- `account.json` is renamed into `accounts/<id>/`, unless a file is there already;
- `tree.sqlite` is opened, its write-ahead log folded into the database, and closed, then renamed
  into `accounts/<id>/`, and a leftover `-shm` removed;
- the flag is cleared.

A store that cannot be opened or folded, whose write-ahead log survives the close (another process
has it open), or whose place is taken, is left where it is and logged, and the flag is cleared all
the same: the account lists its drive again into a new store, and the activity log and conflict list
of before are lost. An error of the moves themselves — a directory that cannot be created, a refused
rename — keeps the flag for the next start and shows in `Accounts.LastError`.

Every step is idempotent — a source that is gone means the step is done — so the next start finishes
whatever a crash interrupted:

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
3. `legacy_token` is cleared, unless the old item could not be deleted.

While `legacy_token` is set, the presence check at startup sees either item, and a sign-out deletes
both. A crash between storing the new item and deleting the old leaves both; the next load prefers
the new one and deletes the old. No unlock prompt is added.

### 8.5 What the user sees

The window's one account is now "Personal". Its Places entry, "OneDrive", becomes "OneDrive —
Personal" in the same place in the panel. Files rescued before stay in `rescued/<time>/`, where
their conflicts recorded them.

There is no way back: an older daemon does not know the version-2 file's accounts. `config.toml.v1`
is the way back, by hand.

## 9. The clients

- **The window** shows one account at a time. An account switcher heads the sidebar; its account
  pages show the account chosen there, and Settings is the whole app's ([desktop.md](desktop.md)
  §4).
- **The tray** has an icon per account, each with its account's state and menu; with the setting
  for that off, one icon shows the worst state across the accounts and its tooltip has a line per
  account ([desktop.md](desktop.md) §5).
- **Notifications and download progress** name the account once there are several
  ([desktop.md](desktop.md) §6, §7).
- **Places** has one entry per account folder, named `OneDrive — <label>` ([desktop.md](desktop.md)
  §4).
- **The Dolphin plugins** call `Files`, which finds the account by path; the emblems read each
  file's attributes and need no account at all ([desktop.md](desktop.md) §10).
- **The command line** chooses the account with the global option `--account <id | label | email>`,
  or `KONEDRIVE_ACCOUNT`; with one account, it needs neither. It has `account list`, `account add`,
  `account rename`, `account remove` and `account mode`; `status` and `sync status` show every
  account when none is chosen; and the path commands go through `Files`, where the path decides the
  account. The rules and every command are in [desktop.md](desktop.md) §3. `login` only signs an
  account that is there in again; with no account at all it is refused and names `account add`. The
  first setup is `account add`, then `sync register`; `set-client-id` is needed only to override the
  built-in client id. A name that fits more than one account — a label that equals another account's
  email, or a hand-edited `config.toml` — is refused with exit status 2, never taken as the first.

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

- **The mode it runs in.** `Account.Mode` is `read-write` only while `config.toml` says so and
  records a drive for the account, the drive its token was last seen to reach is that drive, and the
  scopes its last token response granted — the scopes and the drive kept in `account.json` — include
  `Files.ReadWrite`. Otherwise it is `read-only`, and when `config.toml` says read-write and the
  account is signed in, `LastError` says why.
- **The scope.** Read-only asks for `Files.Read User.Read offline_access`, read-write for
  `Files.ReadWrite User.Read offline_access` ([sync.md](sync.md) §12.1). A refresh asks by the mode
  the account runs in. A sign-in of an account that exists asks by what `config.toml` records:
  read-write when it says so and records a drive. The sign-in that adds an account asks read-only. A
  read-only account keeps asking for `Files.Read`, a subset of any grant, and uses what it gets to
  read only (limitations log F66).
- **To read-write**, `Account.SetMode("read-write", false)` answers a sign-in URL — empty when the
  account already runs read-write — and the sign-in asks for `Files.ReadWrite`, pinned to the
  account (its password asked for again, its email filled in). Only when its token response grants
  that, for this account's own drive, are the new refresh token stored and `mode = "read-write"`
  written. A cancelled sign-in changes nothing; a refused or foreign one changes only `LastError`.
  Consent given in the browser stays with Microsoft (F66).
- **To read-only**, no sign-in: the mode is written, and the next refresh asks for `Files.Read`. It
  is refused `PendingUploads` while changes wait to be uploaded, unless forced; forced, the waiting
  changes are dropped and the files stay.
- **The folder follows the mode.** A read-write folder is kept without the read-only lock; the
  switch takes it off, or puts it back, with a walk of the folder ([writes.md](writes.md) §2.2).
  File modes are not carried to OneDrive and back (issue #216).
- **The mode is the user's choice.** Any signed-in account can be switched to read-write, whatever
  its drive; once it is, nothing but that switch stands between a change in its folder and OneDrive.
  `write_test_drive_ids` only limits a development build's `TokenExport.ReadWrite`, refused
  `WritesNotAllowed` for a drive not on it.

`konedrivectl account mode [read-only|read-write] [--force]` shows or switches the mode. In the
window it is the Account page's switch "Upload changes made on this computer", which explains the
sign-in before it starts it and asks before it drops changes waiting to upload
([desktop.md](desktop.md) §4).

## 11. Costs and limits

Recorded in [`../limitations/`](../limitations/):

- every account shares one helper link and its credit, and a reconnect walks every folder in turn
  (F43);
- removing an account leaves its folder's placeholders as empty files, and keeps its rescued files
  by account id, not label (F47);
- while a wallet prompt is open at the end of a sign-in, the other changes of the accounts wait
  (A31);
- consent to write stays with Microsoft after an account turns read-only (F66).

What else costs something is said where it happens: a sign-in refused when the drive cannot be
checked (§6.2), an open that finds no account (§3.4), a folder forgotten before multiple accounts
(§6.3), an account held back at load (§4.1), the tree store a migration could not move (§8.3), and a
folder the helper still holds for an account taken out of `config.toml` by hand (issue #231).

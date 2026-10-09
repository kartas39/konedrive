# Code map

Where everything is: every crate, directory and file with one line each, where its tests are,
how each suite is run, and which design document explains it. The rules this structure is kept
by are in [`CONTRIBUTING.md`](../CONTRIBUTING.md), "The structure of the code".

A change that adds, moves or removes a file changes its line here.

## How to read it

- A heading names a directory; the files under it are named from that directory.
- `[tests]` after a Rust file `x.rs` means its unit tests are in `x/tests.rs`, or in `x/tests/`
  by topic. After `mod.rs`, `lib.rs` or `main.rs` it means `tests.rs` or `tests/` in the same
  directory. A `tests.rs` has no line of its own; the files of a `tests/` directory have.
- "Design" names the document in [`docs/design/`](design/README.md) that explains the area.

## The parts

| Part | What it is |
|---|---|
| `crates/konedrived` | The user's daemon: sign-in, the sync, the folder as a FUSE mount, the D-Bus API |
| `crates/konedrivectl` | The command line: everything the window can do |
| `crates/konedrive-graph` | Sign-in with Microsoft and everything that talks to Microsoft Graph |
| `crates/konedrive-tree` | The tree store (SQLite): items, staging, the outbox |
| `crates/konedrive-fs` | A folder's files on the local filesystem: konedrive's extended attributes and modes, leases, handles, the feature probe |
| `crates/konedrive-reason` | Why a change is kept back: the reasons of outbox rows and local skips, their spellings and groups |
| `crates/konedrive-dbus` | D-Bus names and client proxies of the daemon, for `konedrivectl` and tests |
| `crates/konedrive-text` | What the codes mean, in words: the one catalogue of sentences, and the C++ generated from it |
| `dbus/` | The D-Bus interfaces as XML: the contract between the daemon and its clients |
| `app/` | The window and the tray icon (Qt 6, Kirigami) |
| `dolphin/` | The Dolphin plugins: overlay emblems and the context menu |
| `tests/fuse-probe` | The suite of the mount, run in a virtme-ng VM |
| `tests/write-account` | The guarded harness for the test account |
| `tests/stress`, `tests/kio` | Tools run by hand: the upload stress run, the KIO probe |
| `scripts/`, `packaging/`, `.github/` | Building, installing, the RPM packages, the release workflow |

Which crate uses which, lowest first: `konedrive-fs` and `konedrive-reason`
(which use nothing); `konedrive-graph`; `konedrive-tree` (uses `konedrive-fs`, and re-exports
`konedrive-reason` in `outbox`; it does not know `konedrive-graph`); `konedrive-dbus`;
`konedrive-text` (uses `konedrive-reason` and `konedrive-dbus`); then
`konedrived` (which uses `konedrive-text` in its tests only) and `konedrivectl`.

## How each suite is run

| Suite | Command | What it covers |
|---|---|---|
| The Rust workspace | `cargo test --workspace` | The unit tests of every crate and the integration tests in `crates/*/tests/`, on private buses, temporary directories and wiremock |
| One crate, or one test binary | `cargo test -p konedrived --lib`, `cargo test -p konedrivectl --test sync_cli` | The same, narrowed while working on one part |
| The development build's tests | `cargo test -p konedrived -p konedrivectl --features konedrived/dev-tools,konedrivectl/dev-tools` | The token export and the signed-out account (`dbus/token_export.rs`, `dbus/dev_tools.rs`, `konedrivectl dev`) |
| The window | `cmake -S app -B build/app -DBUILD_TESTING=ON && cmake --build build/app && ctest --test-dir build/app --output-on-failure` | `app/tests/` |
| The Dolphin plugins | `cmake -S dolphin -B build/dolphin -DBUILD_TESTING=ON && cmake --build build/dolphin && ctest --test-dir build/dolphin --output-on-failure` | `dolphin/tests/` |
| The VM suite | `tests/fuse-probe/run.sh` (`service`, `cli`) | The daemon's service and `konedrivectl` over a folder that is mounted for real, on ext4 in a virtme-ng VM: nothing is mounted on the host |
| The test account | `konedrive-write-test` (`tests/write-account`) | Writes to OneDrive itself, through the guards; rare, never the user's real account |
| The structure | `scripts/check-structure.sh` | The files over the advised sizes (listed only), tests outside source files, the daemon's layer order |

## `crates/konedrived`: the daemon

Design: [`README.md`](design/README.md) for the processes; each directory below names its own.

### `crates/konedrived/`

- `Cargo.toml` — the crate; the features `dev-tools` (the token export) and `fault-injection`
  (the fake OneDrive and the test support of `sync/`, for the VM suite).

### `crates/konedrived/src/`

The directories are in layer order: a directory uses only the directories before it, and
`remote/` does not use `upload/`.

- `main.rs` — the program: reads the configuration, starts the daemon on the session bus,
  and stops when a task it needs is gone; `--tidy`, what the unit runs after the daemon's end.
- `lib.rs` — the list of the directories below.
- `clock.rs` — the wall clock, read in one place (`unix_now`).
- `panic.rs` — what a caught panic said, and a lock taken whether or not a holder of it
  panicked (`lock`, `read`, `write`), for the places that go on after a panic. `[tests]`
- `tests/fake_onedrive/mod.rs` — the module `fake_onedrive`: a fake OneDrive on wiremock, for
  the tests of every area that talks to OneDrive and for the VM scenarios
  (built for tests and with `fault-injection` only).
- `tests/fake_onedrive/sockets.rs` — the fake's notification socket.

### `crates/konedrived/src/config/`

`config.toml`, file locations, the version 1 migration. Design: `accounts.md`.

- `mod.rs` — what the module gives the rest of the daemon. `[tests]`
- `paths.rs` — where the daemon's files are, and each account's.
- `ids.rs` — `AccountId` and `DriveId`: the two ids of an account, as types.
- `model.rs` — what `config.toml` holds (version 2), and the rules of labels, client ids and held accounts.
- `store.rs` — `ConfigStore`, the one owner of the file; `WriteStanding`.
- `atomic.rs` — one file replaced in one step.
- `migrate.rs` — version 1 of `config.toml` and its migration to version 2. `[tests]`

### `crates/konedrived/src/account/`

One account: its sign-in, mode, state, quota, cached profile, stored secret. Design: `sync.md`
(the account), `accounts.md`, `writes.md` §2 (the mode).

- `mod.rs` — `AccountService`: the sign-in state machine of one Microsoft account; the order of its locks.
- `sign_in.rs` — starting, finishing and cancelling a sign-in; sign-out; the account's drive.
- `attempt.rs` — one browser sign-in, up to "these tokens, this drive, this email": an account's own, and a new account's.
- `mode.rs` — `Account.SetMode`: the switch between read-only and read-write.
- `state.rs` — the observable account state, and the mode's note in `LastError`. `[tests]`
- `quota.rs` — the quota of the account's drive, and its figures (`QuotaFigures`). `[tests]`
- `cache.rs` — the cached profile and quota (`account.json`), shown offline. `[tests]`
- `secret.rs` — the refresh token's storage in the Secret Service, an item for each account. `[tests]`
- `testing.rs` — test support, also for `konedrivectl`'s tests: a wallet in
  memory, and one account with no accounts manager. Built only under the `testing` feature.

### `crates/konedrived/src/folder/`

The folder on disk: the root and its registration, descriptor-based changes, the per-file
locks, and which item of the drive has a place in it. Design: `sync.md`.

- `mod.rs` — the list of the modules.
- `root.rs` — `SyncRoot`: checking and registering a root, and opening it — the folder under
  the mount for a mounted one; `RegisterError`; the drive a folder remembers. `[tests]`
- `classify.rs` — what a Graph item becomes in the tree, and whether it has a place in the
  folder. `[tests]`
- `disk.rs` — the upload worker's hands on the folder, by descriptor; `Modes`, a folder's
  lock on its directories' modes. `[tests]`
- `locks.rs` — `InodeLocks`: one reader or writer of a file's upload per inode at a time.
  `[tests]`
- `walk.rs` — a walk over the folder's files that opens none of them; the names konedrive keeps
  for itself.

### `crates/konedrived/src/conditions/`

When an account may work: the pause, the automatic hold, metered, battery, the network.
Design: `writes.md` §11.

- `mod.rs` — the automatic hold's sources; the trait `Accounts`. `[tests]`
- `running.rs` — what background work an account runs now, and the pause. `[tests]`
- `network.rs` — noticing that the network came back. `[tests]`

### `crates/konedrived/src/status/`

What the sync reports. Design: `desktop.md`.

- `mod.rs` — the list of the modules.
- `activity.rs` — `Activity`: the activity log and the conflicts; the words of an event.
  `[tests]`
- `transfers.rs` — `Transfers`: the downloads under way. `[tests]`
- `space.rs` — `LocalSpace`: the space the folder's files take, measured again when asked.
  `[tests]`
- `report.rs` — `Report`: the three of them, as everything that reports holds them.
- `totals.rs` — the queue totals: how much is left to download and to upload. `[tests]`
- `snapshot.rs` — `SyncSnapshot`, in six groups by who writes them, and the published states:
  the root, the local scan, live changes; `LastError` and its notes. `[tests]`
- `overall.rs` — the state an account is in as a whole and the reason for it (`Folder.Overall`),
  with `Trouble` and `NotUpdated`: the one rule, which the window and `konedrivectl` follow.
  `[tests]`

### `crates/konedrived/src/hydration/`

Where a download gets its bytes from, and the download of a whole file. Design:
`hydration.md`.

- `mod.rs` — the list of the modules.
- `source.rs` — `ContentSource`: where a download gets its bytes from.
- `source/download.rs` — a download in one stream, into a file that has no name yet.
- `source/parts.rs` — a large pinned download in parallel parts.
- `source/guards.rs` — what both downloads check: the source's answer, the breaks, the
  checkpoint a download continues from, the hash, the one start over; `Tuning`.
- `source/target.rs` — the file a download writes, and its blocking sections. `[tests]`
- `graph_source.rs` — the content source of a folder: OneDrive.
- `tracked.rs` — `Tracked`: a content source that reports what it fetches to `Transfers`.

### `crates/konedrived/src/local/`

What the user changed in the folder, on its way to the outbox: the folder's mount records
each change (`fuse/`); here is what the recording goes by. Design: `writes.md` §4.

- `mod.rs` — the list of the modules, and the limits they share.
- `ignore.rs` — the ignore list: names that stay local. `[tests]`
- `names.rs` — the names OneDrive refuses, and the name of a kept copy. `[tests]`
- `record.rs` — a local change recorded in the outbox: the mass-delete guard, the order of
  the rows, the one transaction. `[tests]`
- `held.rs` — `Rw`: what the outbox's rows hold of the folder while they wait — the items a
  cycle of the listing leaves as they are, and the ones being removed.

### `crates/konedrived/src/fuse/`

A folder served as a FUSE mount laid over the folder itself: every name from the tree store,
the folder on disk holding only what is downloaded and what the user made. The one way the
daemon shows OneDrive.

- `mod.rs` — the list of the modules, and what the daemon sees of a mount.
- `mount.rs` — `Mount`: made, served, taken off; `Parts`, what it is made with.
- `shared.rs` — `Shared`: the parts of a mount held together, and what is under way;
  `Writing`, what a folder that takes changes shares
  with its sync.
- `under.rs` — `Under`: the folder under the mount, by the descriptor opened on it before the
  mount; it knows the disk and nothing else. Opening the folder, a path of it, its space, an
  attribute.
  `[tests]`
- `under/objects.rs` — `Object`: what stands at a place, its file handle, the item id it
  carries. `[tests]`
- `under/copies.rs` — an item's downloaded copy: whether a place holds it whole, marking one,
  removing one only when it is that item's. `[tests]`
- `under/files.rs` — the files and directories a change makes, renames and removes. `[tests]`
- `under/backing.rs` — the directories down to a place, each marked with the id of its item,
  which the caller tells name by name. `[tests]`
- `under/landing.rs` — a finished download landed: stamped as its item's copy and given its
  name, by what stands at its place (`Stands`, `Step`, `Landed`). `[tests]`
- `under/lock.rs` — the folder on disk locked while no mount lies over it; a dead mount taken
  off it; `tidy`, what follows the daemon's end. `[tests]`
- `under/older.rs` — a folder that holds an older konedrive's placeholders: it is not opened,
  so no mount is put over it. `[tests]`
- `mode.rs` — whether programs may change the folder: `Mount::read_write`, `read_only`,
  `is_writable`.
- `requests.rs` — the mount's requests (`Filesystem`), each handed on.
- `requests/look.rs` — the requests that look: a name, a node, a directory's content,
  attributes.
- `requests/read.rs` — the requests that open and read a file.
- `requests/change.rs` — the requests that change the folder, each made on the recorder's
  thread.
- `nodes.rs` — the node ids the kernel knows, each an item, or an object without one by its
  file handle, and its place. `[tests]`
- `attr.rs` — what the mount says of an item: kind, size, times, modes.
- `view.rs` — `View`: the folder as the mount shows it; it owns the mount's reading connection
  to the store and the overlay as last read, and uses `Under`. An item's row, its parent, the
  files below it. `[tests]`
- `view/entries.rs` — what a directory holds (`Entry`, `Local`, `Dir`): the store's items as
  the overlay leaves them, and the files and directories OneDrive has no item for yet.
- `view/places.rs` — where the folder shows an item, what stands at a path, and the
  directories down to a place marked with the view's items.
- `local.rs` — a node resolved: the entry of the folder it is now.
- `overlay.rs` — `Overlay`: what the outbox's rows say of the folder, a function of the store
  alone: the items moved, removed or changed here. `[tests]`
- `read.rs` — a file that is not downloaded, read as ranges; the file being read ahead of the
  background downloads.
- `fill.rs` — the download of a whole file into a file without a name, which then lands; the
  state of a file: downloaded, being downloaded, or neither.
- `pins.rs` — the pins of the store, and the sweep that downloads what they keep.
- `asked.rs` — what the daemon asks of a mount for a path: download, free up, look.
- `recorder.rs` — the one thread that makes and records each change a program asks for,
  under the folder's tree lock.
- `changes.rs` — what a change means for the outbox: the detections.
- `make.rs` — a file or a directory a program makes.
- `remove.rs` — a file or a directory a program removes.
- `rename.rs` — a file or a directory a program renames or moves inside the folder.
- `write.rs` — a file a program writes, in its downloaded copy.
- `writers.rs` — the files held open for writing: a file is sent once its last writer closed
  it. `[tests]`
- `follow.rs` — the folder under the mount made to follow a cycle of the listing (`Step`).
- `conflict.rs` — where a change from OneDrive meets something the user made here: what is
  kept, and under which name.
- `announce.rs` — changes told to the kernel through the mount, for whoever watches.

### `crates/konedrived/src/upload/`

The outbox worker: sends the recorded changes to OneDrive; what is kept back. Design:
`writes.md` §5, §6, §10.

- `mod.rs` — the worker and its host.
- `engine.rs` — the worker: what it works with, whether it may send, its status, its life.
- `engine/drain.rs` — one drain: which row is taken next, and what the worker waits for.
- `engine/outcome.rs` — how a row's run can end, and the outcome of an answer no step settled.
- `engine/settle.rs` — what each outcome does to its row and to the worker.
- `engine/state.rs` — the worker's own state in parts: the throttle, its trouble, the cycle it waits for, the rows in flight. `[tests]`
- `engine/marks.rs` — the `user.konedrive.sync` attribute of the rows' files.
- `steps.rs` — one row, one step: which step a row's kind takes. `[tests]`
- `steps/meta.rs` — the rows that send no content: `mkdir`, `move` and `delete`.
- `steps/shared.rs` — what the steps share: the row's object, a name that is taken, the guard, the conflict copy.
- `steps/sections.rs` — the blocking sections a step's file calls run in, and the worker's count of them.
- `content.rs` — a file's content going up, a `create` or an `update`: what is decided before it is sent and by its answer.
- `content/session.rs` — how the content gets there: the read, and the one upload session every file with content goes through, step by step.
- `local.rs` — the worker's hands on the folder: finding a row's local object.
- `space.rs` — a full OneDrive: what waits for space. `[tests]`
- `kept_back.rs` — what is kept back from OneDrive, grouped by what the user can do about it.
  `[tests]`

### `crates/konedrived/src/remote/`

What changed in OneDrive, brought into the folder. Design: `sync.md`, `writes.md` §9 (the
reconcile in read-write mode).

- `mod.rs` — the list of the modules.
- `mode.rs` — `Mode<W>`: read-only, or read-write with what a level needs for it (the folder's
  `Writes`, a cycle's `RwCycle`, what the outbox holds).
- `applied.rs` — what a cycle's reconcile did to the folder: the numbers, the copies it
  kept, each change by name (`Applied`, `OnDisk`, `Counts`, `Changed`, `Copied`).
- `live.rs` — changes from OneDrive at once: the notification socket's task.
- `listing.rs` — `Listing`: one folder's cycle in three steps (fetch, stage and reconcile, what
  follows a cycle that went through).
- `listing/fetch.rs` — `fetch`: a first listing shown page by page, a whole listing, and the
  changes since the last one.
- `listing/lease.rs` — the folder's lease a cycle holds while it changes the folder; `sync/` hands it in.
- `listing/poller.rs` — when a cycle runs.
- `listing/reconcile.rs` — the reconcile of one cycle, in either mode: the folder's mount
  follows the new tree (`follow_mount`), `commit_cycle` (what waits), `after_commit`.
- `listing/rw.rs` — `Writes`: what a read-write folder's cycle shares with its outbox worker.
- `listing/stage.rs` — `stage`: what was fetched put into `staging`, and the reconcile it asks
  for (commit, mode); a read-write folder's tree lock, stale-delta guard and what is
  staged again.

### `crates/konedrived/src/desktop/`

Baloo and thumbnails. Design: `desktop.md`.

- `mod.rs` — the list of the modules.
- `baloo.rs` — keeps KDE's file indexer out of a OneDrive folder. `[tests]`
- `dirnotify.rs` — telling KDE's programs to read directories again, after a mount came or
  went or the modes it shows changed.
- `thumbs.rs` — thumbnails from OneDrive. `[tests]`

### `crates/konedrived/src/sync/`

`SyncService`, one per account's folder, a file per responsibility, and the registry. Every
`impl SyncService` is here. Design: `sync.md`, `accounts.md`.

- `mod.rs` — `SyncService` and what it holds; `SyncError`.
- `wiring.rs` — `Wiring`: what a service is made with, given once to its constructor.
- `testing.rs` — test support, also for the scenarios outside the workspace: a `Wiring` of
  fakes (the account, a temporary `config.toml`, the content source, a clock moved by hand).
- `registry.rs` — `Registry`: the folders of every account, listed by the account manager;
  the overlap check, what every account is told alike.
- `folder.rs` — what the folder is, as a type; `change`, the one way to change it, which
  also says to the desktop when the folder's mount shows other modes; the view the readers read.
- `running_sync.rs` — the running sync of a folder as one object in that state: its parts
  (the poller, the outbox worker of a read-write folder), the handles a reader may hold, and
  how it stops (dropped, then waited for by the next change).
- `publish.rs` — what the bus shows of the folder's state, worked out in one place.
- `persisted.rs` — the folder as `config.toml` records it.
- `bring_up.rs` — a new registration, the folder taken up and brought up at startup; the
  two calls of the bus that are refused now (`RegisterWithoutInterception`,
  `PopulateFromDirectory`).
- `take_down.rs` — forgetting a folder; retiring an account's folder.
- `start_stop.rs` — starting the sync (prepare with the mount, build, run: the mount takes
  changes while a read-write sync runs) and asking it for a cycle.
- `hydrate.rs` — `Hydrate`: a file downloaded now, by the mount; the mount a call needs.
- `free_up.rs` — freeing up files and whole folders, by the mount.
- `pins.rs` — putting pins on and taking them off, by the mount.
- `menu.rs` — what the context menu may offer for a selection: one account's share of it, and
  the answer of `Files.Menu` put together.
- `queries.rs` — what the bus reads: skipped items, activity, conflicts, transfers, states.
- `mode.rs` — the folder's side of the account's mode, and its write gate.
- `outbox.rs` — the outbox worker built and woken; the rows dropped; the outbox as
  `org.konedrive.UploadQueue` shows it. `[tests]`
- `pause.rs` — the pause, the hold, Sync Anyway, and `PauseClock`, which ends a timed pause. `[tests]`
- `settings.rs` — the account's settings in `config.toml`: thumbnails, the ignore list, the machine name.

### `crates/konedrived/src/daemon/`

The daemon as a whole. Design: `accounts.md`.

- `mod.rs` — the list of the modules.
- `manager.rs` — the account manager; the trait `Bus`: how its objects get on the bus.
- `manager/sign_in.rs` — `Accounts.SignIn`: the sign-in that belongs to no account, how it ends, and the account made when it succeeds.
- `startup.rs` — `Daemon`: the configuration's lock, the migration, the start.
- `stop.rs` — the stop on SIGTERM or SIGINT; `Tasks`, the tasks whose end stops the daemon. `[tests]`

### `crates/konedrived/src/dbus/`

The D-Bus interfaces, one file each, named like the XML in `dbus/`. Design: `desktop.md`.

- `mod.rs` — the interfaces of one account's folder, as types.
- `accounts.rs` — `org.konedrive.Accounts`.
- `account.rs` — `org.konedrive.Account`.
- `folder.rs` — `org.konedrive.Folder`.
- `files.rs` — `org.konedrive.Files`.
- `transfers.rs` — `org.konedrive.Transfers`.
- `upload_queue.rs` — `org.konedrive.UploadQueue`.
- `conflicts.rs` — `org.konedrive.Conflicts`.
- `activity_log.rs` — `org.konedrive.ActivityLog`.
- `local_scan.rs` — `org.konedrive.LocalScan`.
- `token_export.rs` — `org.konedrive.TokenExport`, only in a development build.
- `dev_tools.rs` — `org.konedrive.DevTools`, only in a development build.
- `export.rs` — putting the objects on the bus and taking them off; `OnBus`.
- `properties.rs` — the table of the properties the daemon announces by itself. `[tests]`
- `signals.rs` — what the daemon announces by itself: `PropertiesChanged`, `ActivityLog.Added`.
  `[tests]`
- `fault.rs` — `Fault`: every refusal under the name of a `Refusal`. `[tests]`

### `crates/konedrived/tests/`

Integration tests: the daemon over a private bus, Microsoft as wiremock.

- `common/mod.rs` — what they share: the fake Microsoft, the started daemon.
- `account_flow.rs` — the sign-in, from the client id to signed in.
- `sign_in.rs` — `Accounts.SignIn`: nothing made before it succeeds, its outcomes, its label.
- `accounts.rs` — several accounts.
- `dbus_api.rs` — `org.konedrive.Account`, and the introspection against the XML.
- `mode.rs` — the account's mode.

## `crates/konedrivectl`: the command line

Design: `desktop.md`.

### `crates/konedrivectl/`

- `Cargo.toml` — the crate; the feature `dev-tools` (`konedrivectl dev`).

### `crates/konedrivectl/src/`

The program is `main.rs`, `cli.rs`, `commands/`, `daemon.rs`, `read.rs` and `wait.rs`: it talks
to the daemon and prints. The library is `lib.rs`, `choice.rs`, `secret_file.rs` and `text/`:
what is decided and what is said, with nothing read from the daemon.

- `main.rs` — the program: parses the command line and runs the command.
- `cli.rs` — the commands as `clap` describes them; `sync` is `status`, `FolderCmd` and `PathCmd`.
- `daemon.rs` — the daemon on the bus, the account a command chose, and every account's folder.
- `read.rs` — what is read of the daemon into the types `text/` prints.
- `wait.rs` — the waits for a sign-in to end.
- `lib.rs` — the library: its modules by name, the environment variables, whether a browser opens. `[tests]`
- `choice.rs` — the choice of an account. `[tests]`
- `secret_file.rs` — a secret written to a file, atomically and privately. `[tests]`

### `crates/konedrivectl/src/commands/`

- `mod.rs` — the list of the command groups.
- `account.rs` — `account`: list, remove, rename, mode.
- `add.rs` — `account add`: a new account, by signing in.
- `login.rs` — `login`: an account there already signs in again.
- `browser.rs` — opening the sign-in page.
- `status.rs` — `status`.
- `settings.rs` — `settings`.
- `sync/mod.rs` — `sync`: `status`, `anyway --all`, and which of the two groups a command is in.
- `sync/folder.rs` — the `sync` commands on the chosen account's folder.
- `sync/path.rs` — the `sync` commands that take a path.
- `sync/explain.rs` — what they share: a path made absolute, a refusal explained, the check
  that a folder does not need attention.
- `dev.rs` — `dev`, only in a development build.
- `version.rs` — `--version`: this build's, and the running daemon's.

### `crates/konedrivectl/src/text/`

What is printed, by topic. Pure: each function is given what was read and returns the text.

- `mod.rs` — the list of the topics.
- `status.rs` — `status` and `sync status`: what was read, and its lines. `[tests]`
- `folder.rs` — what the commands on a folder say when they are done; `sync activity`. `[tests]`
- `settings.rs` — `settings`.
- `version.rs` — `--version`.
- `uploads.rs` — the upload queue and what is not uploaded; a reason's sentence is
  `konedrive-text`'s. `[tests]`
- `transfers.rs` — transfers and the queue totals. `[tests]`
- `files.rs` — skipped items, conflicts, pins, free-up. `[tests]`
- `refusals.rs` — a refusal explained: the actions, and what the CLI read after the refusal. `[tests]`
- `refusals/folder.rs` — the sentences of a refused folder call, by the group of the action;
  those of `pin`, `unpin`, `free` and `open` are taken from `konedrive-text`.
- `refusals/account.rs` — the sentences of a refused call on the accounts and the token export.
- `accounts.rs` — `account list`.
- `formats.rs` — sizes, times, durations, shell words. `[tests]`

### `crates/konedrivectl/tests/`

Integration tests: the compiled binary against a daemon on a private bus.

- `common/mod.rs` — what they share: the daemon, started as `konedrived` starts it.
- `accounts_cli.rs` — several accounts.
- `login.rs` — `login` when the sign-in is cancelled elsewhere.
- `status.rs` — `status`.
- `mode_cli.rs` — `account mode`, and the token export's `--read-write`.
- `version_cli.rs` — `--version`.
- `sync_cli/main.rs` — the test binary `sync_cli`: its harness.
- `sync_cli/registration.rs` — what `sync register` refuses, and a path that does not exist.
- `sync_cli/status.rs` — `sync status`, skipped items.
- `sync_cli/activity.rs` — transfers, conflicts, `sync open`, refresh.
- `sync_cli/refusals.rs` — the named refusals, explained.
- `sync_cli/settings.rs` — pause, the ignore list, the settings.
- `sync_cli/fuse.rs` — a mounted folder driven from the command line; run in the VM only
  (`tests/fuse-probe/run.sh cli`).
- `sync_cli/token_export.rs` — `dev export-access-token`.
- `sync_cli/wording.rs` — the wording shared with the window.

## `crates/konedrive-graph`: Microsoft Graph

Design: `sync.md`; `writes.md` for the writes and the upload sessions.

### `crates/konedrive-graph/`

- `Cargo.toml` — the crate; the feature `testing`, for the daemon's tests.

### `crates/konedrive-graph/src/`

- `lib.rs` — the list of the modules, and what each offers; `lock`, a lock taken whether or
  not a holder of it panicked.
- `oauth.rs` — the authorization URL and the token endpoint; the scopes. `[tests]`
- `pkce.rs` — PKCE and random `state` values. `[tests]`
- `loopback.rs` — the one-shot listener for the OAuth redirect. `[tests]`
- `token.rs` — access tokens: cached, refreshed on demand, one refresh at a time. `[tests]`
- `secret.rs` — the refresh token's store, as the token manager sees it.
- `pool.rs` — the transfer pool: how many requests an account has in flight. `[tests]`
- `quickxor.rs` — QuickXorHash. `[tests]`
- `drive/mod.rs` — the drive API, reading: the delta feed, one item, content. `[tests]`
- `drive/error.rs` — an answer's status, what it and Graph's error code mean, a read's error.
  `[tests]`
- `drive/send.rs` — the one way a request leaves the client: who it is authorised as, and
  what a `429` or a `503` does to it.
- `drive/account.rs` — the two calls the account page needs: the profile and the drive.
  `[tests]`
- `drive/item.rs` — a `driveItem`. `[tests]`
- `drive/write.rs` — the drive API, writing: a new folder, a move, a delete. `[tests]`
- `drive/upload.rs` — upload sessions. `[tests]`
- `drive/socket.rs` — change notifications over Socket.IO. `[tests]`

## `crates/konedrive-tree`: the tree store

Design: `sync.md` (the tree store), `writes.md` §5 (the outbox).

### `crates/konedrive-tree/`

- `Cargo.toml` — the crate; the feature `testing`, for the daemon's tests.

### `crates/konedrive-tree/src/`

- `lib.rs` — the tree store: the list of the modules, `TreeStore`, `TreeError`. `[tests]`
- `model.rs` — a row, a delta entry, the stored words, and the one place a row is read and
  written. `[tests]`
- `schema.rs` — the schema: its version, what a new store is created with, when a store is
  rebuilt. `[tests]`
- `schema/migrations.rs` — the numbered steps that bring a store of an earlier version to
  today's schema: none yet.
- `meta.rs` — what the store keeps one of: every key of `meta`, and its typed accessors.
- `query.rs` — reading the tree: a row, what is below it, where it is, the counts.
- `read.rs` — `ReadStore`: the store as its read-only connection gives it.
- `placing.rs` — whether the base places a row: itself, and every folder above it.
- `shared.rs` — the store shared by the tasks of one folder.
- `source.rs` — where the rows of a tree are, and the queries that walk it.
- `staging.rs` — the new tree a cycle builds, and its swap into `items`.
- `reconcile.rs` — what a read-write folder's cycle keeps from one cycle to the next.
  `[tests]`
- `activity.rs` — the activity log, as the store keeps it; `ActivityKind`, what an event records.
- `conflicts.rs` — the local versions kept, on record.
- `thumbs.rs` — the thumbnails still to make, and what each cached one was made for.
- `outbox.rs` — the outbox: what the folder holds that OneDrive does not have yet. `[tests]`
- `outbox/stored.rs` — a row in the database: read, written and removed in one place.
- `outbox/row.rs` — a row, a detection, and what a recording and a commit hand the store.
- `outbox/encoded.rs` — a row's snapshot, as the row has it and as its columns keep it, and the
  temporary name in its `target_name`. `[tests]`
- `outbox/record.rs` — a detection recorded.
- `outbox/pick.rs` — which rows run next. `[tests]`
- `outbox/dependencies.rs` — every row's blockers at once; test code.
- `outbox/worker.rs` — the worker's own transactions.
- `outbox/sums.rs` — what the outbox holds, summed.
- `outbox/changes.rs` — telling those who wait that the outbox changed.

## `crates/konedrive-fs`: a folder's files

Design: `hydration.md`.

### `crates/konedrive-fs/`

- `Cargo.toml` — the crate.

### `crates/konedrive-fs/src/`

- `lib.rs` — the list of the modules, the limits the crates share (`MAX_DEPTH`, `NAME_MAX`,
  `RESERVED_PREFIX`), and `proc_path`, the path of an open descriptor.
- `placeholder.rs` — what konedrive keeps on a file or a directory: its extended attributes
  and its modes. `[tests]`
- `lease.rs` — a write lease: proof that no other process has the file open. `[tests]`
- `handle.rs` — file handles: the kernel's name for an inode, and what a file or directory
  without an item is known by. `[tests]`
- `probe.rs` — whether a directory's filesystem can be a sync folder's: sparse files, `user.*`
  attributes, leases, file handles. `[tests]`

## `crates/konedrive-reason`: why a change is kept back

Design: `writes.md` §5 (the outbox), `desktop.md` (`NotUploadedSummary()`).

### `crates/konedrive-reason/`

- `Cargo.toml` — the crate; it has no dependencies.
- `src/lib.rs` — a row's reason, a local skip, their stored spellings, details and groups. `[tests]`

## `crates/konedrive-dbus`: names and proxies

Design: `desktop.md`.

### `crates/konedrive-dbus/`

- `Cargo.toml` — the crate; the feature `testing` (`testing.rs`).
- `build.rs` — the version and the commit a build shows.
- `src/lib.rs` — the D-Bus names. `[tests]`
- `src/accounts.rs` — the proxies of the accounts' interfaces. `[tests]`
- `src/rows.rs` — the rows the daemon answers with, by name. `[tests]`
- `src/overall.rs` — `Folder.Overall`: the states and the reasons, as they are spelled. `[tests]`
- `src/refusal.rs` — `Refusal`: every name a call is refused under. `[tests]`
- `src/version.rs` — the version line, the same in every program. `[tests]`
- `src/testing.rs` — a private session bus for tests; only with the feature `testing`.
- `tests/test_bus.rs` — tests: the private bus starts no program; a test's connection has the method timeout.
- `tests/version_script.rs` — tests: `scripts/version.sh`.

## `dbus/`: the interfaces

Each has its file in `crates/konedrived/src/dbus/`. Design: `desktop.md`.

### `dbus/`

- `org.konedrive.Accounts.xml` — the manager: the accounts, the shared settings, `HelperState`
  (which reads `connected`), the version.
- `org.konedrive.Account.xml` — one account: sign-in, label, mode, profile, quota.
- `org.konedrive.Folder.xml` — one account's folder: registration, refresh, pause, settings.
- `org.konedrive.Files.xml` — the per-file calls, by path, for any account.
- `org.konedrive.Transfers.xml` — downloads and uploads under way, speeds, the pool.
- `org.konedrive.UploadQueue.xml` — the changes waiting to be uploaded, and what is not.
- `org.konedrive.Conflicts.xml` — the local versions kept.
- `org.konedrive.ActivityLog.xml` — the activity log.
- `org.konedrive.LocalScan.xml` — the Full local scan.
- `org.konedrive.TokenExport.xml` — the token export of a development build.
- `org.konedrive.DevTools.xml` — a development build's way to add a signed-out account.

## `crates/konedrive-text`: the words for the codes

Design: `desktop.md` §2.10. A new code gets its words here: `CONTRIBUTING.md`, "A new reason or
refusal".

### `crates/konedrive-text/`

- `Cargo.toml` — the crate; it uses `konedrive-reason` and `konedrive-dbus` only.
- `src/lib.rs` — a sentence, its places, and who shows it. `[tests]`
- `src/reasons.rs` — why a change is not uploaded: every key of `Reason` and `LocalSkip`.
- `src/waits.rs` — what keeps an item on this computer: every key of `WaitsFor`.
- `src/files.rs` — a file not kept, unpinned, freed up or opened in OneDrive: every name of
  `Refusal`, and the Dolphin plugin's own sentences. `[tests]`
- `src/menu.rs` — why an entry of Dolphin's menu is disabled: the tooltips. `[tests]`
- `src/cpp.rs` — the generator of the C++ files, and the window's (`app/generated/`).
- `src/cpp/plugin.rs` — the Dolphin plugin's (`dolphin/src/generated/`).
- `tests/generated.rs` — tests: the generated files in git are what the generator writes now;
  with `KONEDRIVE_UPDATE_GENERATED=1` it rewrites them.

## `app/`: the window

Design: `desktop.md`. Each `x.h` and `x.cpp` is one class.

### `app/`

- `CMakeLists.txt` — the build; reads the version from `Cargo.toml`.
- `main.cpp` — the program: single instance, the window, the tray.
- `Main.qml` — the window: pages chosen from a sidebar.
- `qmlregistration.h` — what the QML finds in `org.konedrive.app`.
- `daemoncontroller.h`, `daemoncontroller.cpp` — the manager object, `org.konedrive.Accounts`.
- `accountsmodel.h`, `accountsmodel.cpp` — the accounts, each with its controllers; Sign In.
- `currentaccount.h`, `currentaccount.cpp` — the account the window shows.
- `accountcontroller.h`, `accountcontroller.cpp` — one account's `org.konedrive.Account`.
- `synccontroller.h`, `synccontroller.cpp` — one account's folder, for QML.
- `synctypes.h` — the structured types of the folder's interfaces.
- `skippeditem.h` — one entry of `Skipped()` and of `NotUploaded()`.
- `accountstatus.h`, `accountstatus.cpp` — one summary of an account and its folder: the state
  the daemon decided (`Folder.Overall`), in words by its reason.
- `appstatus.h`, `appstatus.cpp` — the whole app's state, for the tray: the worst of the accounts'.
- `activitymodel.h`, `activitymodel.cpp` — the activity list.
- `conflictmodel.h`, `conflictmodel.cpp` — the "Conflicts" list.
- `transfermodel.h`, `transfermodel.cpp` — the downloads under way.
- `uploadreasons.h`, `uploadreasons.cpp` — why a change is not uploaded, in words: the entry
  functions; the sentences are generated.
- `generated/reasontexts.h`, `generated/reasontexts.cpp` — generated from `konedrive-text`, not
  edited: the window's sentence for every reason, and how a reason is cut into its parts.
- `notifier.h`, `notifier.cpp` — the notifications.
- `konedrive.notifyrc` — the notification events.
- `trayicon.h`, `trayicon.cpp` — the app in the tray: one icon for every account, or one each.
- `trayitem.h`, `trayitem.cpp` — one tray icon and its menu.
- `traysettings.h`, `traysettings.cpp` — the setting for an icon per account.
- `downloadprogresscontroller.h`, `downloadprogresscontroller.cpp` — long downloads reported
  to Plasma as jobs.
- `downloadjob.h`, `downloadjob.cpp` — one such job.
- `downloadjobtracker.h`, `downloadjobtracker.cpp` — where a job is registered.
- `downloadprogresssettings.h`, `downloadprogresssettings.cpp` — the setting for it.
- `placescontroller.h`, `placescontroller.cpp` — one entry per folder in KDE's Places.
- `placessettings.h`, `placessettings.cpp` — the setting for it.
- `autostart.h`, `autostart.cpp` — start at login.
- `selfrestart.h`, `selfrestart.cpp` — the window restarts as the installed program after an update.
- `org.konedrive.KOneDrive.desktop.in` — the desktop entry.

### `app/qml/`

- `AccountSwitcher.qml` — the top of the sidebar: the account the pages show.
- `StatusPage.qml` — the start page: how the folder is doing, and what to do.
- `AccountPage.qml` — the account: its name, its mode, its sign-in.
- `ActivityPage.qml` — speeds, what is left, what happened.
- `ConflictsPage.qml` — the local versions the sync moved out of the way.
- `NotUploadedPage.qml` — what stays on this computer and is not uploaded.
- `SkippedPage.qml` — what is in OneDrive but not in the folder.
- `SettingsPage.qml` — the whole app's settings.

### `app/tests/`

- `fakedaemon.h` — a stand-in for `konedrived` on the tests' private bus.
- `session-bus.conf` — that bus.
- `accountcontrollertest.cpp` — `AccountController`.
- `accountsmodeltest.cpp` — `DaemonController`, `AccountsModel`, `CurrentAccount`.
- `appstatustest.cpp` — `AccountStatus`, `AppStatus`.
- `synccontrollertest.cpp` — `SyncController`.
- `reasontextstest.cpp` — every reason reads as it did before its sentence was generated.
- `modelstest.cpp` — the transfer, activity and conflict models.
- `notifiertest.cpp` — `Notifier`.
- `dialogstest.cpp` — the window's confirmations.
- `downloadprogresscontrollertest.cpp` — `DownloadProgressController`.
- `downloadprogresssettingstest.cpp` — `DownloadProgressSettings`.
- `placescontrollertest.cpp` — `PlacesController`.
- `autostarttest.cpp` — `Autostart`.
- `singleinstancetest.cpp` — a second launch shows the first one's window.
- `selfrestarttest.cpp` — the window restarts once its program file is replaced and the daemon is back.
- `qmlcachetest.cpp` — the QML runs from the binary, never from Qt's disk cache.
- `CacheProbe.qml` — a file of that test.

## `dolphin/`: the Dolphin plugins

Design: `desktop.md`; `docs/kio-behavior.md` for what Dolphin opens.

### `dolphin/`

- `CMakeLists.txt` — the build.

### `dolphin/src/`

- `overlayplugin.cpp` — the overlay plugin: emblems on files.
- `konedriveoverlay.json` — its description.
- `overlayengine.h`, `overlayengine.cpp` — its logic: which emblem, and the cache of roots.
- `actionplugin.cpp` — the context menu plugin: the OneDrive section, handed over waiting and
  set from the daemon's answer (`Files.Menu`) when it comes.
- `konedriveactions.json` — its description.
- `filestate.h`, `filestate.cpp` — what a file in the folder is, from its extended attributes
  alone: what the emblems show, and whether a selection is in a sync folder at all.
- `syncclient.h`, `syncclient.cpp` — the calls to the daemon, none of them waited for: the
  menu's question and the entries' calls.
- `refusaltext.h`, `refusaltext.cpp` — what to tell a person when the daemon refused: which
  sentence a failure gets, and how several are summed up; the sentences are generated.
- `generated/refusaltexts.h`, `generated/refusaltexts.cpp` — generated from `konedrive-text`,
  not edited: the plugin's sentence for every operation and refusal, and the menu's tooltips.

### `dolphin/tests/`

- `CMakeLists.txt` — the tests' build, and their private bus.
- `testsupport.h` — what the tests share: folders built from real files, and a stand-in for the
  daemon on a thread of its own.
- `session-bus.conf.in`, `activating-bus.conf.in`, `failing-daemon.service.in` — the tests'
  buses, and a daemon that fails to start.
- `overlayenginetest.cpp` — the overlay logic on its own.
- `overlayplugintest.cpp` — the overlay plugin as Dolphin loads it.
- `actionplugintest.cpp` — the context menu plugin as `KFileItemActions` loads it.
- `noopentest.cpp` — the plugins never open a file inside the folder.
- `refusaltextstest.cpp`, `refusaltexts.expected` — every operation and refusal reads as it did
  before its sentence was generated: the strings of the hand-written table, a line each.

## `tests/fuse-probe`: the VM suite of the mount

A crate of its own, outside the workspace. A folder is mounted for real here, in a virtme-ng
VM, never on the host.

### `tests/fuse-probe/`

- `Cargo.toml` — the crate and its two programs.
- `run.sh` — builds the programs, boots the VM and runs `guest.sh` in it.
- `guest.sh` — inside the VM: an ext4 disk, and the tests as an ordinary user. `service`: the
  daemon's service over the fake OneDrive, command by command. `cli`: `konedrivectl`'s test
  of a mounted folder (`crates/konedrivectl/tests/sync_cli/fuse.rs`).
- `try-on-host.sh` — a trial of this tree's daemon on the developer's machine, beside the
  installed one, with a configuration and a state of its own.
- `src/bin/fuse-service.rs` — `fuse-service`: the daemon's service for one account, taking a
  command a line.
- `src/bin/watch-client.rs` — `watch-client`: an inotify watch on a directory, every event printed.

## `tests/write-account`: the test account's harness

The program `konedrive-write-test`, a member of the workspace. Design: `writes.md` §12.

### `tests/write-account/`

- `Cargo.toml` — the crate.
- `src/main.rs` — the entry.
- `src/args.rs` — the command line: the token files and the daemon's `config.toml`.
- `src/harness.rs` — one run: the guards that must hold before anything is written.
- `src/guard.rs` — what every request must pass before it is forwarded.
- `src/proxy.rs` — the one way out to OneDrive: a forwarder on `127.0.0.1`.
- `src/checks.rs` — what the run checks against OneDrive itself.
- `src/checks/notifications.rs` — the change notifications.
- `src/checks/placeholders.rs` — an upload session's placeholder.
- `src/tests.rs` — tests: each guard shown to refuse, against wiremock.

## `tests/stress` and `tests/kio`: tools run by hand

### `tests/stress/`

- `README.md` — what the stress run does, and how to run it.
- `stress_uploads.py` — the run: real files made, changed, moved and deleted in a test
  account's folder.
- `graph_check.py` — the read-only check against Graph.
- `konedrivectl_wrap.py` — `konedrivectl`, called for one account.
- `quickxor.py` — QuickXorHash in Python.

### `tests/kio/`

What `docs/kio-behavior.md` was measured with.

- `CMakeLists.txt` — the build; never installed.
- `kio_probe.cpp` — measures what KIO does to files in a folder Dolphin shows.
- `run.sh` — runs it on a private bus.

## Scripts, packaging and CI

Design: `packaging.md`; `docs/releasing.md`.

### `scripts/`

- `build-rpm.sh` — builds the RPM packages from the committed tree.
- `version.sh` — the version a build carries.
- `check-structure.sh` — the guard of the structure rules.
- `dev-install.sh` — builds and installs for the current user: a development install.
- `dev-uninstall.sh` — removes it.

### `packaging/`

- `rpm/konedrive.spec` — the two packages.
- `systemd/konedrived.service` — the daemon's user unit.
- `dbus/org.konedrive.Daemon.service` — the daemon started by the bus.

### `.github/workflows/`

- `release.yml` — a release on every push to `main`: the RPMs for each Fedora, each tried in a
  clean container, then the tag and the release.
- `tests.yml` — the tests of a pull request into `dev`: the daemon's unit tests, the window's and
  the Dolphin plugins'.
- `structure.yml` — on every pull request: the guard, the check of the links in the doc comments,
  and the generated C++ against the catalogue of sentences (`cargo test -p konedrive-text`).

## Documents

- `README.md` — what KOneDrive is and does, in short, and where to read more.
- `docs/user-guide.md` — installing and using it: the window, the tray, Dolphin.
- `docs/command-line.md` — the same from a terminal, with `konedrivectl`.
- `docs/developing.md` — building, the development install, the tests.
- `CONTRIBUTING.md` — building, the tests, the structure rules, the limitations log.
- `SECURITY.md` — the helper's security model.
- `docs/design/` — how the system works and why; its `README.md` is the index.
- `docs/limitations/` — every limit, workaround, fragile spot and shortcut, one file each;
  its `README.md` is the index.
- `docs/kernel-behavior-7.2/` — what fanotify, leases and the filesystems were measured to
  do, by topic; its `README.md` is the index.
- `docs/kio-behavior.md` — what KIO and Dolphin open.
- `docs/releasing.md` — the version, and how a release is made.
- `docs/code-map.md` — this file.

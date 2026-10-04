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
| `crates/konedrived` | The user's daemon: sign-in, the sync, files on demand, the D-Bus API |
| `crates/konedrive-helper` | The privileged helper: fanotify permission events, and nothing about OneDrive |
| `crates/konedrivectl` | The command line: everything the window can do |
| `crates/konedrive-graph` | Sign-in with Microsoft and everything that talks to Microsoft Graph |
| `crates/konedrive-tree` | The tree store (SQLite): items, staging, the outbox |
| `crates/konedrive-fs` | Placeholders on the local filesystem: extended attributes, sparse files, leases, handles |
| `crates/konedrive-proto` | The messages between the helper and the daemon |
| `crates/konedrive-reason` | Why a change is kept back: the reasons of outbox rows and local skips, their spellings and groups |
| `crates/konedrive-dbus` | D-Bus names and client proxies of the daemon, for `konedrivectl` and tests |
| `dbus/` | The D-Bus interfaces as XML: the contract between the daemon and its clients |
| `app/` | The window and the tray icon (Qt 6, Kirigami) |
| `dolphin/` | The Dolphin plugins: overlay emblems and the context menu |
| `tests/vm` | The suite that needs root and a real kernel, run in a virtme-ng VM |
| `tests/write-account` | The guarded harness for the test account |
| `tests/stress`, `tests/kio` | Tools run by hand: the upload stress run, the KIO probe |
| `scripts/`, `packaging/`, `.github/` | Building, installing, the RPM packages, the release workflow |

Which crate uses which, lowest first: `konedrive-proto`, `konedrive-fs` and `konedrive-reason`
(which use nothing); `konedrive-graph`; `konedrive-tree` (uses `konedrive-fs`, and re-exports
`konedrive-reason` in `outbox`; it does not know `konedrive-graph`); `konedrive-dbus`; then
`konedrive-helper`, `konedrived` and `konedrivectl`.

## How each suite is run

| Suite | Command | What it covers |
|---|---|---|
| The Rust workspace | `cargo test --workspace` | The unit tests of every crate and the integration tests in `crates/*/tests/`, on private buses, temporary directories and wiremock |
| One crate, or one test binary | `cargo test -p konedrived --lib`, `cargo test -p konedrivectl --test sync_cli` | The same, narrowed while working on one part |
| The development build's tests | `cargo test -p konedrived -p konedrivectl --features konedrived/dev-tools,konedrivectl/dev-tools` | The token export (`dbus/token_export.rs`, `konedrivectl dev`) |
| The outbox at scale | `cargo test -p konedrived --release --lib bench:: -- --ignored --nocapture --test-threads 1` | `crates/konedrived/src/tests/bench.rs`: ignored tests, run by hand |
| The window | `cmake -S app -B build/app -DBUILD_TESTING=ON && cmake --build build/app && ctest --test-dir build/app --output-on-failure` | `app/tests/` |
| The Dolphin plugins | `cmake -S dolphin -B build/dolphin -DBUILD_TESTING=ON && cmake --build build/dolphin && ctest --test-dir build/dolphin --output-on-failure` | `dolphin/tests/` |
| The VM suite | `tests/vm/run.sh quick` (btrfs); `tests/vm/run.sh full` (btrfs, ext4, xfs) | `tests/vm/scenarios/`: the real helper as root. Only when a change touches the helper path |
| The helper's unit | `tests/vm/run.sh unit` | `tests/vm/helper_unit_test.sh`: the shipped systemd unit, under systemd in the VM |
| The helper's installer | `tests/vm/run.sh tests/vm/install_helper_test.sh` | `scripts/install-helper.sh`, as root in the VM |
| The kernel measurements | `tests/vm/run.sh measure`; `tests/vm/run.sh <binary>` for the built `poc-marks`, `vm-ignore-mark` and `watch-probe` | The probes behind `docs/kernel-behavior-7.2/` |
| The test account | `konedrive-write-test` (`tests/write-account`) | Writes to OneDrive itself, through the guards; rare, never the user's real account |
| The structure | `scripts/check-structure.sh` | The size limits, tests outside source files, the daemon's layer order |

## `crates/konedrived`: the daemon

Design: [`README.md`](design/README.md) for the processes; each directory below names its own.

### `crates/konedrived/`

- `Cargo.toml` — the crate; the features `dev-tools` (the token export) and `fault-injection`
  (a stall the VM suite arms).

### `crates/konedrived/src/`

The directories are in layer order: a directory uses only the directories before it, and
`remote/` does not use `upload/`.

- `main.rs` — the program: reads the configuration, starts the daemon on the session bus,
  keeps the helper link up.
- `lib.rs` — the list of the directories below.
- `tests/bench.rs` — the module `bench`: the outbox and the cloud side at scale, ignored
  tests run by hand in release.

### `crates/konedrived/src/config/`

`config.toml`, file locations, the version 1 migration. Design: `accounts.md`.

- `mod.rs` — the configuration (version 2) and where each account's files are. `[tests]`
- `migrate.rs` — version 1 of `config.toml` and its migration to version 2. `[tests]`

### `crates/konedrived/src/account/`

One account: its sign-in, mode, state, quota, cached profile, stored secret. Design: `sync.md`
(the account), `accounts.md`, `writes.md` §2 (the mode).

- `mod.rs` — `AccountService`: the sign-in state machine of one Microsoft account.
- `sign_in.rs` — starting, finishing and cancelling a sign-in; sign-out; the account's drive.
- `mode.rs` — `Account.SetMode`: the switch between read-only and read-write.
- `state.rs` — the observable account state. `[tests]`
- `quota.rs` — the quota of the account's drive. `[tests]`
- `cache.rs` — the cached profile and quota (`account.json`), shown offline. `[tests]`
- `secret.rs` — the refresh token's storage: the Secret Service, or memory in tests. `[tests]`

### `crates/konedrived/src/helper/`

The daemon's end of the helper socket, and the helper's state. Design: `hydration.md`.

- `mod.rs` — the link: a blocking thread that owns the socket; `LinkCell`. `[tests]`
- `linked.rs` — `Helper` and `Linked`: what the daemon asks of the helper beyond the fills, as
  a trait.
- `status.rs` — the helper as the daemon sees it: `Accounts.HelperState`. `[tests]`

### `crates/konedrived/src/folder/`

The folder on disk: the root and its registration, descriptor-based changes, the per-file
locks, and which item of the drive has a place in it. Design: `hydration.md`, `sync.md`.

- `mod.rs` — the list of the modules.
- `root.rs` — `SyncRoot`: opening, checking and registering a root; `DehydrateError`. `[tests]`
- `classify.rs` — what a Graph item becomes in the tree, and whether it has a place in the
  folder. `[tests]`
- `disk.rs` — every change made to the folder, by descriptor. `[tests]`
- `locks.rs` — `InodeLocks`: one fill or free-up per inode at a time. `[tests]`

### `crates/konedrived/src/conditions/`

When an account may work: the pause, the automatic hold, metered, battery, the network.
Design: `writes.md` §11.

- `mod.rs` — the automatic hold's sources; the trait `Accounts`. `[tests]`
- `running.rs` — what background work an account runs now, and the pause. `[tests]`
- `network.rs` — noticing that the network came back. `[tests]`

### `crates/konedrived/src/status/`

What the sync reports. Design: `desktop.md`.

- `mod.rs` — the list of the modules.
- `activity.rs` — the activity log, conflicts and transfers. `[tests]`
- `totals.rs` — the queue totals: how much is left to download and to upload. `[tests]`
- `snapshot.rs` — `SyncSnapshot` and the published states: the root, the local scan, live
  changes. `[tests]`

### `crates/konedrived/src/hydration/`

Files on demand: answering opens, content sources, the fill, freeing up, startup recovery,
pins. Design: `hydration.md`, `pinning.md`.

- `mod.rs` — the list of the modules.
- `server.rs` — takes hydration requests off the helper's queue and fills them; the trait
  `Router`. `[tests]`
- `source.rs` — `ContentSource`: where hydration gets its bytes from.
- `source/fill.rs` — the loop that fills a placeholder in place from a content source.
  `[tests]`
- `source/parts.rs` — a large pinned download in parallel parts. `[tests]`
- `graph_source.rs` — the content source of a folder that shows OneDrive. `[tests]`
- `tracked.rs` — `Tracked`: a content source that reports what it fetches to `Transfers`.
- `dehydrate.rs` — freeing up a file: back to a placeholder. `[tests]`
- `recovery.rs` — startup recovery: what an interrupted fill or free-up left behind. `[tests]`
- `pin.rs` — "Always keep on this device": which items are pinned, and their downloads.
  `[tests]`

### `crates/konedrived/src/local/`

What the user changed on disk: the watcher that notices it and the examination that records it
in the outbox. Design: `writes.md` §3 (the watcher), §4 (the examination).

- `mod.rs` — local changes, from the disk to the outbox. `[tests]`
- `batch.rs` — a batch: the places a quiet spell of events made dirty. `[tests]`
- `entry.rs` — one directory entry as the examination sees it.
- `examine.rs` — the examination: from a batch to the detections recorded in the outbox.
- `examine/run.rs` — a run's reads of the store, and what it expects where.
- `examine/list.rs` — reading every place the batch names.
- `examine/classify.rs` — which entry is which item; strangers and backups.
- `examine/found.rs` — an item found: where it is, and its content.
- `examine/missing.rs` — an item not found: removed, moved away, or leaving.
- `examine/finish.rs` — the end of a run: the skipped list, the counts, the order of the rows.
- `ignore.rs` — the ignore list: names that stay local. `[tests]`
- `names.rs` — the names OneDrive refuses, and the name of a kept copy. `[tests]`
- `liveness.rs` — whether a missing object is still there, and where.
- `scan.rs` — how the Full local scan goes, for `org.konedrive.LocalScan`. `[tests]`

### `crates/konedrived/src/local/watcher/`

- `mod.rs` — the watcher: the daemon's own unprivileged fanotify group on the folder.
  `[tests]`
- `fan.rs` — the notification group itself: `fanotify_init`, `fanotify_mark`, reading events.
  `[tests]`
- `map.rs` — the directory map: which directory a file handle names. `[tests]`
- `dirt.rs` — what the events since the last hand-over made dirty.
- `reader.rs` — the reader thread: walks the folder once, then drains the events.
- `service.rs` — the watcher in the daemon: hands each batch to the examination.

### `crates/konedrived/src/upload/`

The outbox worker: sends the recorded changes to OneDrive; what is kept back. Design:
`writes.md` §5, §6, §8 (moves out), §10.

- `mod.rs` — the worker and its host. `[tests]`
- `engine.rs` — the worker's loop: which rows run now, and how many at once.
- `engine/drain.rs` — running rows until none can run now.
- `engine/outcome.rs` — what a step's outcome does to its row.
- `steps.rs` — one row, one step: `mkdir`, `move` and `delete`.
- `content.rs` — a file's content going up: a `create` or an `update`.
- `local.rs` — the worker's hands on the folder: finding a row's local object.
- `space.rs` — a full OneDrive: what waits for space. `[tests]`
- `kept_back.rs` — what is kept back from OneDrive, grouped by what the user can do about it.
  `[tests]`
- `move_out.rs` — a move out of the folder: a `move-out` row's step.
- `move_out/place.rs` — where a moved-out object is now, proved.
- `move_out/trash.rs` — the Trash and its entries.
- `move_out/walk.rs` — walking a moved-out folder.
- `move_out/cases.rs` — the four cases: a file or a folder, to the Trash or elsewhere.
- `move_out/tidy.rs` — removing what a move-out left behind.
- `fake.rs` — a fake OneDrive, for the worker's tests and the VM suite's write scenarios.
- `fake/harness.rs` — the tests' worker around the fake.
- `fake/sockets.rs` — the fake's notification socket.

### `crates/konedrived/src/upload/tests/`

The tests of the worker, by topic.

- `mod.rs` — the worker's steps, its order, its crash points; what the topics share.
- `move_out.rs` — moves out of the folder, on the host.
- `removed.rs` — a file or folder removed before its upload finished.
- `sessions.rs` — upload sessions and their placeholders.

### `crates/konedrived/src/remote/`

What changed in OneDrive, brought into the folder. Design: `sync.md`, `writes.md` §9 (the
reconcile in read-write mode).

- `mod.rs` — the list of the modules.
- `live.rs` — changes from OneDrive at once: the notification socket's task. `[tests]`
- `listing.rs` — `Listing`: one folder's cycle, from the delta feed to the reconcile.
  `[tests]`
- `listing/fetch.rs` — a full listing and the changes since the last one, page by page.
  `[tests]`
- `listing/poller.rs` — when a cycle runs. `[tests]`
- `listing/replacements.rs` — replacing changed files, several at once. `[tests]`
- `listing/rw.rs` — a read-write folder's cycle. `[tests]`
- `materialize.rs` — `Materializer`: makes the folder match the tree. `[tests]`
- `materialize/file.rs` — a file already in place, and what its content needs.
- `materialize/holding.rs` — deleting what OneDrive no longer has; rescues.
- `materialize/replace.rs` — replacing one changed file. `[tests]`
- `materialize/rw.rs` — the reconcile in read-write mode. `[tests]`
- `materialize/rw/leaving.rs` — what is leaving: an item that stays in OneDrive but is no
  longer placed here.
- `materialize/rw/removal.rs` — what OneDrive removed, and what is kept of it.
- `materialize/rw/holding.rs` — putting back what was held.

### `crates/konedrived/src/remote/listing/rw/tests/`

The tests of a read-write folder's cycle.

- `mod.rs` — the cycle with the outbox and the examination; what the topics share.
- `stale.rs` — what the daemon takes off the disk itself is never deleted or moved in
  OneDrive.

### `crates/konedrived/src/desktop/`

Baloo and thumbnails. Design: `desktop.md`.

- `mod.rs` — the list of the modules.
- `baloo.rs` — keeps KDE's file indexer out of a OneDrive folder. `[tests]`
- `thumbs.rs` — thumbnails from OneDrive. `[tests]`

### `crates/konedrived/src/sync/`

`SyncService`, one per account's folder, a file per responsibility, and the hub. Every
`impl SyncService` is here. Design: `hydration.md`, `sync.md`, `accounts.md`.

- `mod.rs` — `SyncService` and what it holds. `[tests]`
- `hub.rs` — the one link to the helper, shared by every account. `[tests]`
- `registration.rs` — binding a folder to the account.
- `forget.rs` — forgetting a folder; retiring an account's folder.
- `start_stop.rs` — starting, nudging and stopping the sync.
- `resume.rs` — bringing a folder back at startup, and the switch to interception.
- `populate.rs` — a folder of placeholders made from a local directory.
- `hydrate.rs` — filling a placeholder now; `SyncService` as a content source.
- `free_up.rs` — freeing up files and whole folders.
- `pins.rs` — putting pins on and taking them off.
- `queries.rs` — what the bus reads: skipped items, activity, conflicts, transfers, states.
- `write_mode.rs` — the folder's side of the account's mode.
- `outbox_api.rs` — the outbox as `org.konedrive.UploadQueue` shows it. `[tests]`
- `move_outs.rs` — the fills of moved-out objects.
- `run_settings.rs` — the settings that decide what runs; Sync Anyway.
- `watching.rs` — the watcher of a read-write folder.

### `crates/konedrived/src/sync/tests/`

The tests of `SyncService`, by topic.

- `mod.rs` — what they share: the fake helper, the services, the placeholders.
- `registration.rs` — registering a folder.
- `hydrate.rs` — fills, and one fill per inode.
- `pins.rs` — "Always keep on this device".
- `mode.rs` — with and without interception.
- `startup.rs` — startup and the helper's supervisor.
- `reports.rs` — what a download or a free-up reports.
- `guards.rs` — guards over behaviour found correct.
- `onedrive/mod.rs` — what the tests of a folder that shows OneDrive share.
- `onedrive/folder.rs` — the folder: `WebUrl`, Baloo, its drive.
- `onedrive/read_write.rs` — a read-write folder.
- `onedrive/settings.rs` — the settings, the hold and the pause.

### `crates/konedrived/src/daemon/`

The daemon as a whole. Design: `accounts.md`.

- `mod.rs` — the list of the modules.
- `manager.rs` — the account manager; the traits `Bus` and `HelperStateSignal`.
- `startup.rs` — `Daemon`: the configuration's lock, the migration, the start.
- `stop.rs` — the stop on SIGTERM or SIGINT. `[tests]`

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
- `export.rs` — putting the objects on the bus and taking them off; `OnBus`.
- `signals.rs` — `PropertiesChanged`, coalesced. `[tests]`
- `fault.rs` — every refusal as a D-Bus error name.

### `crates/konedrived/tests/`

Integration tests: the daemon over a private bus, Microsoft as wiremock.

- `common/mod.rs` — what they share: the fake Microsoft, the started daemon.
- `account_flow.rs` — the sign-in, from the client id to signed in.
- `accounts.rs` — several accounts.
- `dbus_api.rs` — `org.konedrive.Account`, and the introspection against the XML.
- `mode.rs` — the account's mode.
- `sync_dbus.rs` — the interfaces of an account's folder.

## `crates/konedrive-helper`: the privileged helper

Design: `hydration.md`; [`SECURITY.md`](../SECURITY.md). Its end-to-end tests are the VM suite.

### `crates/konedrive-helper/`

- `Cargo.toml` — the crate; the feature `fault-injection`, for the VM suite only.

### `crates/konedrive-helper/src/`

- `main.rs` — the startup.
- `lib.rs` — the modules worth exercising on their own.
- `shared.rs` — what the helper's threads share, by subject; the hydrations in hand.
- `shared/registrations.rs` — the registered roots behind their two locks; the count of
  unregistrations. `[tests]`
- `shared/daemons.rs` — the connected daemons, and waiting for one. `[tests]`
- `shared/slots.rs` — a bounded number of places per uid. `[tests]`
- `shared/refusals.rs` — the throttled log of refusals. `[tests]`
- `events.rs` — the loop that reads the fanotify group. `[tests]`
- `events/decision.rs` — what an intercepted open is answered, and carrying it out. `[tests]`
- `events/hydration.rs` — asking the owner's daemon, and passing its answer on.
- `connection.rs` — accepting connections, and the connection with one daemon. `[tests]`
- `registration.rs` — registering and unregistering a root, with its filesystem check.
  `[tests]`
- `marks.rs` — the fanotify permission group: marks on directories, ignore marks on files.
  `[tests]`
- `roots.rs` — the registered roots, and the rules for what the helper acts on. `[tests]`
- `jobs.rs` — coalescing: many opens of one file wait on one hydration. `[tests]`
- `outbox.rs` — everything sent to one daemon, and the thread that sends it. `[tests]`
- `pool.rs` — a bounded pool of threads that answer permission events.
- `by_handle.rs` — `OpenByHandle`: a descriptor for an object of the folder. `[tests]`

## `crates/konedrivectl`: the command line

Design: `desktop.md`.

### `crates/konedrivectl/`

- `Cargo.toml` — the crate; the feature `dev-tools` (`konedrivectl dev`).

### `crates/konedrivectl/src/`

The program is `main.rs`, `cli.rs`, `commands/` and `daemon.rs`; the library is `lib.rs`,
`text/` and `choice.rs`.

- `main.rs` — the program: parses the command line and runs the command.
- `cli.rs` — the commands as `clap` describes them.
- `daemon.rs` — the daemon on the bus, and the account a command chose.
- `lib.rs` — the testable part; re-exports `text/` and `choice.rs`. `[tests]`
- `choice.rs` — the choice of an account. `[tests]`

### `crates/konedrivectl/src/commands/`

- `mod.rs` — the list of the command groups.
- `account.rs` — `account`: list, add, remove, rename, mode.
- `login.rs` — `login`.
- `browser.rs` — opening the sign-in page.
- `status.rs` — `status`.
- `settings.rs` — `settings`.
- `sync.rs` — `sync`: everything about a folder and its files.
- `dev.rs` — `dev`, only in a development build.
- `version.rs` — `--version`: this build's, and the running daemon's.

### `crates/konedrivectl/src/text/`

What is printed, by topic.

- `mod.rs` — the list of the topics.
- `status.rs` — `status` and `sync status`. `[tests]`
- `uploads.rs` — the upload queue and what is not uploaded. `[tests]`
- `transfers.rs` — transfers and the queue totals. `[tests]`
- `files.rs` — skipped items, conflicts, pins, free-up. `[tests]`
- `refusals.rs` — a refusal explained. `[tests]`
- `accounts.rs` — `account list`.
- `formats.rs` — sizes, times, durations, shell words. `[tests]`

### `crates/konedrivectl/tests/`

Integration tests: the compiled binary against a daemon on a private bus.

- `common/mod.rs` — what they share: the daemon, started as `konedrived` starts it.
- `accounts_cli.rs` — several accounts.
- `login.rs` — the wait for a sign-in.
- `status.rs` — `status`.
- `mode_cli.rs` — `account mode`, and the token export's `--read-write`.
- `version_cli.rs` — `--version`.
- `sync_cli/main.rs` — the test binary `sync_cli`: its harness.
- `sync_cli/registration.rs` — register, populate, hydrate, dehydrate.
- `sync_cli/status.rs` — `sync status`, skipped items.
- `sync_cli/activity.rs` — activity, transfers, conflicts, free-up.
- `sync_cli/refusals.rs` — the named refusals, explained.
- `sync_cli/settings.rs` — pause, the ignore list, the settings, the upload queue.
- `sync_cli/token_export.rs` — `dev export-access-token`.
- `sync_cli/wording.rs` — the wording shared with the window.

## `crates/konedrive-graph`: Microsoft Graph

Design: `sync.md`; `writes.md` for the writes and the upload sessions.

### `crates/konedrive-graph/`

- `Cargo.toml` — the crate; the feature `testing`, for the daemon's tests.

### `crates/konedrive-graph/src/`

- `lib.rs` — the list of the modules, and what each offers.
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
- `schema.rs` — the schema: its version, what an open creates and upgrades, when a store is
  rebuilt.
- `query.rs` — reading the tree: a row, what is below it, where it is, the counts.
- `forget.rs` — forgetting the local objects of a subtree.
- `shared.rs` — the store shared by the tasks of one folder.
- `source.rs` — where the rows of a tree are, and the queries that walk it.
- `staging.rs` — the new tree a cycle builds, and its swap into `items`.
- `reconcile.rs` — what a read-write folder's cycle keeps from one cycle to the next.
  `[tests]`
- `activity.rs` — the activity log, as the store keeps it.
- `conflicts.rs` — the local versions kept, on record.
- `thumbs.rs` — the thumbnails still to make.
- `outbox.rs` — the outbox: what the folder holds that OneDrive does not have yet. `[tests]`
- `outbox/schema.rs` — the write phase's tables and indexes.
- `outbox/row.rs` — a row, a detection, and what an examination and a commit hand the store.
- `outbox/encoded.rs` — what a row's `snapshot` and `target_name` hold, read and written. `[tests]`
- `outbox/record.rs` — a detection recorded.
- `outbox/pick.rs` — which rows run next. `[tests]`
- `outbox/dependencies.rs` — every row's blockers at once; test code.
- `outbox/worker.rs` — the worker's own transactions.
- `outbox/sums.rs` — what the outbox holds, summed.
- `outbox/handles.rs` — an item's local object.
- `outbox/changes.rs` — telling those who wait that the outbox changed.

## `crates/konedrive-fs`: placeholders

Design: `hydration.md`.

### `crates/konedrive-fs/`

- `Cargo.toml` — the crate.

### `crates/konedrive-fs/src/`

- `lib.rs` — the list of the modules, and the limits the crates share: `MAX_DEPTH`, `NAME_MAX`,
  `RESERVED_PREFIX`.
- `placeholder.rs` — a placeholder: a sparse file whose state is in its extended attributes.
  `[tests]`
- `lease.rs` — a write lease: proof that no other process has the file open. `[tests]`
- `handle.rs` — file handles: the kernel's name for an inode. `[tests]`
- `probe.rs` — whether a directory's filesystem can host placeholders. `[tests]`

## `crates/konedrive-proto`: the helper's protocol

Design: `hydration.md` (the helper–daemon protocol).

### `crates/konedrive-proto/`

- `Cargo.toml` — the crate.
- `src/lib.rs` — the messages between the helper and the daemon, and their framing. `[tests]`

## `crates/konedrive-reason`: why a change is kept back

Design: `writes.md` §5 (the outbox), `desktop.md` (`NotUploadedSummary()`).

### `crates/konedrive-reason/`

- `Cargo.toml` — the crate; it has no dependencies.
- `src/lib.rs` — a row's reason, a local skip, their stored spellings, details and groups. `[tests]`

## `crates/konedrive-dbus`: names and proxies

Design: `desktop.md`.

### `crates/konedrive-dbus/`

- `Cargo.toml` — the crate.
- `build.rs` — the version and the commit a build shows.
- `src/lib.rs` — the D-Bus names and the client proxies. `[tests]`
- `src/accounts.rs` — the proxies of the accounts' interfaces. `[tests]`
- `src/version.rs` — the version line, the same in every program. `[tests]`
- `src/testing.rs` — a private session bus for tests.
- `tests/test_bus.rs` — tests: the private bus starts no program; a test's connection has the method timeout.
- `tests/version_script.rs` — tests: `scripts/version.sh`.

## `dbus/`: the interfaces

Each has its file in `crates/konedrived/src/dbus/`. Design: `desktop.md`.

### `dbus/`

- `org.konedrive.Accounts.xml` — the manager: the accounts, the shared settings, the helper's
  state, the version.
- `org.konedrive.Account.xml` — one account: sign-in, label, mode, profile, quota.
- `org.konedrive.Folder.xml` — one account's folder: registration, refresh, pause, settings.
- `org.konedrive.Files.xml` — the per-file calls, by path, for any account.
- `org.konedrive.Transfers.xml` — downloads and uploads under way, speeds, the pool.
- `org.konedrive.UploadQueue.xml` — the changes waiting to be uploaded, and what is not.
- `org.konedrive.Conflicts.xml` — the local versions kept.
- `org.konedrive.ActivityLog.xml` — the activity log.
- `org.konedrive.LocalScan.xml` — the Full local scan.
- `org.konedrive.TokenExport.xml` — the token export of a development build.

## `app/`: the window

Design: `desktop.md`. Each `x.h` and `x.cpp` is one class.

### `app/`

- `CMakeLists.txt` — the build; reads the version from `Cargo.toml`.
- `main.cpp` — the program: single instance, the window, the tray.
- `Main.qml` — the window: pages chosen from a sidebar.
- `qmlregistration.h` — what the QML finds in `org.konedrive.app`.
- `daemoncontroller.h`, `daemoncontroller.cpp` — the manager object, `org.konedrive.Accounts`.
- `accountsmodel.h`, `accountsmodel.cpp` — the accounts, each with its controllers.
- `currentaccount.h`, `currentaccount.cpp` — the account the window shows.
- `accountcontroller.h`, `accountcontroller.cpp` — one account's `org.konedrive.Account`.
- `synccontroller.h`, `synccontroller.cpp` — one account's folder, for QML.
- `synctypes.h` — the structured types of the folder's interfaces.
- `skippeditem.h` — one entry of `Skipped()` and of `NotUploaded()`.
- `accountstatus.h`, `accountstatus.cpp` — one summary of an account and its folder.
- `appstatus.h`, `appstatus.cpp` — the whole app's state, for the tray.
- `activitymodel.h`, `activitymodel.cpp` — the activity list.
- `conflictmodel.h`, `conflictmodel.cpp` — the "Conflicts" list.
- `transfermodel.h`, `transfermodel.cpp` — the downloads under way.
- `uploadreasons.h`, `uploadreasons.cpp` — why a change is not uploaded, in words.
- `notifier.h`, `notifier.cpp` — the notifications.
- `konedrive.notifyrc` — the notification events.
- `trayicon.h`, `trayicon.cpp` — the tray icon.
- `downloadprogresscontroller.h`, `downloadprogresscontroller.cpp` — long downloads reported
  to Plasma as jobs.
- `downloadjob.h`, `downloadjob.cpp` — one such job.
- `downloadjobtracker.h`, `downloadjobtracker.cpp` — where a job is registered.
- `downloadprogresssettings.h`, `downloadprogresssettings.cpp` — the setting for it.
- `placescontroller.h`, `placescontroller.cpp` — one entry per folder in KDE's Places.
- `placessettings.h`, `placessettings.cpp` — the setting for it.
- `autostart.h`, `autostart.cpp` — start at login.
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
- `modelstest.cpp` — the transfer, activity and conflict models.
- `notifiertest.cpp` — `Notifier`.
- `dialogstest.cpp` — the window's confirmations.
- `downloadprogresscontrollertest.cpp` — `DownloadProgressController`.
- `downloadprogresssettingstest.cpp` — `DownloadProgressSettings`.
- `placescontrollertest.cpp` — `PlacesController`.
- `autostarttest.cpp` — `Autostart`.
- `singleinstancetest.cpp` — a second launch shows the first one's window.
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
- `actionplugin.cpp` — the context menu plugin: the OneDrive section.
- `konedriveactions.json` — its description.
- `filestate.h`, `filestate.cpp` — what a file in the folder is, from its extended attributes
  alone.
- `syncclient.h`, `syncclient.cpp` — the calls to the daemon.
- `refusaltext.h`, `refusaltext.cpp` — what to tell a person when the daemon refused.

### `dolphin/tests/`

- `CMakeLists.txt` — the tests' build, and their private bus.
- `testsupport.h` — what the tests share: folders built from real files.
- `session-bus.conf.in`, `activating-bus.conf.in`, `failing-daemon.service.in` — the tests'
  buses, and a daemon that fails to start.
- `overlayenginetest.cpp` — the overlay logic on its own.
- `overlayplugintest.cpp` — the overlay plugin as Dolphin loads it.
- `actionplugintest.cpp` — the context menu plugin as `KFileItemActions` loads it.
- `noopentest.cpp` — the plugins never open a file inside the folder.

## `tests/vm`: the VM suite

A crate of its own, outside the workspace. Everything that needs root runs here, in a
virtme-ng VM, never on the host. Design: `hydration.md`; `docs/kernel-behavior-7.2/` for what
the probes measured.

### `tests/vm/`

- `Cargo.toml` — the crate and its four programs.
- `run.sh` — boots the VM and runs a program in it as root; `quick`, `full`, `unit`,
  `measure`.
- `helper_unit_test.sh` — the helper as systemd starts it from the shipped unit.
- `install_helper_test.sh` — `scripts/install-helper.sh`, as root.
- `poc_marks.rs` — `poc-marks`: the proof of concept of the interception.
- `ignore_mark.rs` — `vm-ignore-mark`: the ignore mark the helper ships really lands.

### `tests/vm/scenarios/`

The program `vm-scenarios`: the real helper binary, end to end.

- `main.rs` — the entry and the suite.
- `harness.rs` — the harness: the helper started, its marks read, the scenarios' tools.
- `child.rs` — the child modes: the processes that open, hold and attack.
- `kernel_facts.rs` — the kernel facts the design rests on.
- `measure.rs` — the measurement mode.
- `fills.rs` — an open fills the file.
- `dehydrate.rs` — freeing up, and the ignore mark after it.
- `registration.rs` — registering and forgetting a folder.
- `coverage.rs` — what the marks cover: new directories, moves, hard links.
- `lifecycle.rs` — the daemon or the helper dying and coming back.
- `clients.rs` — hostile and too many clients.
- `burst.rs` — a burst of opens, and the caps on waiters.
- `faults.rs` — out of descriptors, panics, a full disk.
- `races.rs` — races between fills, forgets and marks.
- `punch_rule.rs` — what must never be punched or denied.
- `walk.rs` — the startup walk and its hazards.
- `accounts.rs` — two accounts on one link.
- `open_by_handle.rs` — `OpenByHandle`.
- `move_out.rs` — moves out of the folder.
- `watch.rs` — the write phase's watcher.
- `writes.rs` — the write phase end to end, with the fake OneDrive.
- `graph.rs` — the real account: one folder, read only.
- `unit.rs` — the daemon's side of `run.sh unit`.

### `tests/vm/watch_probe/`

The program `watch-probe`: what the watcher's fanotify group reports.

- `main.rs` — the entry.
- `root.rs` — the part run as root.
- `child.rs` — the part run as the user.
- `events.rs` — parsing fanotify events.
- `sys.rs` — the system calls.

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
- `install-helper.sh` — installs the helper, as root.

### `packaging/`

- `rpm/konedrive.spec` — the two packages.
- `systemd/konedrived.service` — the daemon's user unit.
- `systemd/konedrive-helper.service` — the helper's system unit.
- `dbus/org.konedrive.Daemon.service` — the daemon started by the bus.

### `.github/workflows/`

- `release.yml` — a release on every push to `main`: the tests, the RPMs, the tag.
- `structure.yml` — the guard, on every pull request.

## Documents

- `README.md` — what KOneDrive is, how to install and use it.
- `CONTRIBUTING.md` — building, the tests, the structure rules, the limitations log.
- `SECURITY.md` — the helper's security model.
- `docs/design/` — how the system works and why; its `README.md` is the index.
- `docs/limitations/` — every limit, workaround, fragile spot and shortcut, one file each;
  its `README.md` is the index.
- `docs/kernel-behavior-7.2/` — what fanotify, leases and the filesystems were measured to
  do, by topic; its `README.md` is the index.
- `docs/history/original-proposal.md` — the original proposal for the whole client.
- `docs/kio-behavior.md` — what KIO and Dolphin open.
- `docs/releasing.md` — the version, and how a release is made.
- `docs/acceptance-check.md` — a manual check of a build against a real account.
- `docs/code-map.md` — this file.

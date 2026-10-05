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
  keeps the helper link up, and stops when a task it needs is gone.
- `lib.rs` — the list of the directories below.
- `clock.rs` — the wall clock, read in one place (`unix_now`).
- `panic.rs` — what a caught panic said, and a lock taken whether or not a holder of it
  panicked (`lock`, `read`, `write`), for the places that go on after a panic. `[tests]`
- `tests/bench.rs` — the module `bench`: the outbox and the cloud side at scale, ignored
  tests run by hand in release.
- `tests/fake_onedrive/mod.rs` — the module `fake_onedrive`: a fake OneDrive on wiremock, for
  the tests of every area that talks to OneDrive and for the VM suite's write scenarios
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
- `mode.rs` — `Account.SetMode`: the switch between read-only and read-write.
- `state.rs` — the observable account state, and the mode's note in `LastError`. `[tests]`
- `quota.rs` — the quota of the account's drive, and its figures (`QuotaFigures`). `[tests]`
- `cache.rs` — the cached profile and quota (`account.json`), shown offline. `[tests]`
- `secret.rs` — the refresh token's storage in the Secret Service, an item for each account. `[tests]`
- `testing.rs` — test support, also for `konedrivectl`'s tests and the VM suite: a wallet in
  memory, and one account with no accounts manager. Built only under the `testing` feature.

### `crates/konedrived/src/helper/`

The daemon's end of the helper socket, and the helper's state. Design: `hydration.md`.

- `mod.rs` — the list of the modules; `HelperError`.
- `link.rs` — `HelperLink`: the two blocking threads that own the socket, the calls and
  their bounds; `LinkCell`, where an account keeps the link. `[tests]`
- `clearance.rs` — `Clearance`: the rule every punch clears a file's ignore mark by.
- `presence.rs` — whether a helper has its socket bound, told without connecting. `[tests]`
- `testing.rs` — test support: the stand-ins for the helper's end of the socket, over one accept
  loop; `FakeHelper`, the one the daemon's tests share: it records, refuses, holds an answer
  until released, and finds an object by its handle.
- `hub.rs` — `HelperHub`: the one link every account shares, its supervisor and `HelperState`;
  `Served`, whom it tells as the link comes and goes.
- `linked.rs` — `Helper` and `Linked`: what the daemon asks of the helper beyond the fills, as
  a trait.
- `status.rs` — the helper as the daemon sees it: how systemd says its unit stands. `[tests]`

### `crates/konedrived/src/folder/`

The folder on disk: the root and its registration, descriptor-based changes, the per-file
locks, and which item of the drive has a place in it. Design: `hydration.md`, `sync.md`.

- `mod.rs` — the list of the modules.
- `root.rs` — `SyncRoot`: opening, checking and registering a root; `OpenError`,
  `RegisterError`; the drive a folder remembers. `[tests]`
- `classify.rs` — what a Graph item becomes in the tree, and whether it has a place in the
  folder. `[tests]`
- `disk.rs` — every change made to the folder, by descriptor; `Modes`, a folder's lock on
  its directories' modes. `[tests]`
- `locks.rs` — `InodeLocks`: one fill or free-up per inode at a time. `[tests]`
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

### `crates/konedrived/src/hydration/`

Files on demand: answering opens, content sources, the fill, freeing up, startup recovery,
pins. Design: `hydration.md`, `pinning.md`.

- `mod.rs` — the list of the modules.
- `server.rs` — takes hydration requests off the helper's queue and fills them; the trait
  `Router`. `[tests]`
- `source.rs` — `ContentSource`: where hydration gets its bytes from.
- `source/fill.rs` — a fill of a placeholder in place: the state it is found in, the clearing
  of its ignore mark, the commit, and the roll-back of a fill that failed. `[tests]`
- `source/download.rs` — a download in one stream; `download_into` for a replacement.
- `source/parts.rs` — a large pinned download in parallel parts. `[tests]`
- `source/guards.rs` — what both downloads check: the source's answer, the breaks, the
  checkpoint a download continues from, the hash, the one start over; `Tuning`.
- `source/target.rs` — the file a fill writes, and its blocking sections. `[tests]`
- `source/local_dir.rs` — `LocalDir`: the content source of a folder filled from a directory.
- `demote.rs` — turning a file back into a placeholder: the one function a failed fill, a
  stopped download, a free-up and startup recovery empty a file with. `[tests]`
- `testing.rs` — test support: `Faulty` (a source that counts, waits and breaks) and the
  area's fixture.
- `graph_source.rs` — the content source of a folder that shows OneDrive. `[tests]`
- `tracked.rs` — `Tracked`: a content source that reports what it fetches to `Transfers`.
- `dehydrate.rs` — freeing up a file: the checks, `dehydrating`, the clearing, the lease.
  `[tests]`
- `recovery.rs` — startup recovery: what an interrupted fill or free-up left behind. `[tests]`
- `pin.rs` — "Always keep on this device": which items are pinned, and their downloads.
  `[tests]`

### `crates/konedrived/src/local/`

What the user changed on disk: the watcher that notices it and the examination that records it
in the outbox. Design: `writes.md` §3 (the watcher), §4 (the examination).

- `mod.rs` — local changes, from the disk to the outbox. `[tests]`
- `batch.rs` — a batch: the places a quiet spell of events made dirty. `[tests]`
- `entry.rs` — one directory entry as the examination sees it.
- `examine.rs` — the examination: from a batch to the detections recorded in the outbox; a run's four parts and the order of the rules.
- `examine/listing.rs` — what a run read of the disk: every place the batch names, read-only once built; whether a place was looked at.
- `examine/facts.rs` — the store as a run reads it: the live rows, and each item's row, recorded object and expected place.
- `examine/decisions.rs` — who is who in a run: which entry is which item, what is settled, what is spoken for.
- `examine/identity.rs` — which entry is the item: one function of facts and entries, and carrying its decision out. `[tests]`
- `examine/hands.rs` — the one type through which a run opens a listed entry and writes to the disk while deciding.
- `examine/copies.rs` — what becomes of an entry with an id that is not its own: stripped, listed, or removed when empty.
- `examine/found.rs` — an item found: where it is, and its content, by the stamp first.
- `examine/missing.rs` — an item not found: removed, moved away, or moved out of the folder.
- `examine/new.rs` — an entry with no id: a new file or folder, unless it stays local.
- `examine/place.rs` — where an object is now, asked by its handle, and the proof that it is absent.
- `examine/detect.rs` — the detections a run records, each shape made once; whether a file is ready to be read.
- `examine/finish.rs` — the end of a run: the skipped list, the counts, the order of the rows, the one transaction.
- `ignore.rs` — the ignore list: names that stay local. `[tests]`
- `names.rs` — the names OneDrive refuses, and the name of a kept copy. `[tests]`
- `liveness.rs` — whether a missing object is still there, and where; the proof that it is absent from a place.
- `handles.rs` — which filesystem the recorded file handles belong to, and taking them again when it changed; the record of the object a replacement swapped in.
- `testing.rs` — tests: the one fixture of this area's tests, and of the others' that run an
  examination (`Folder`: a temporary folder, its store, the listing placed in it, the watcher on
  it), the sinks of the watcher's tests, and the stand-in for the helper's answer about an object
  (`FakeLiveness`).
- `scan.rs` — how the Full local scan goes, for `org.konedrive.LocalScan`. `[tests]`

### `crates/konedrived/src/local/watcher/`

- `mod.rs` — the watcher: the daemon's own unprivileged fanotify group on the folder.
  `[tests]`
- `fan.rs` — the notification group itself: `fanotify_init`, `fanotify_mark`, reading events.
  `[tests]`
- `map.rs` — the directory map: which directory a file handle names. `[tests]`
- `dirt.rs` — what the events since the last hand-over made dirty.
- `reader.rs` — the reader thread: walks the folder once, then drains the events and settles them.
  - `reader/tree.rs` — the folder's directories as the reader knows them: the map, what left, what waits.
  - `reader/marks.rs` — the marks on them: the notification groups and their budget, the helper's `MarkDir`.
  - `reader/timers.rs` — when the folder is walked again and the helper asked again. `[tests]`
  - `reader/walk.rs` — a walk of directories, and how each kind of walk treats what it finds.
- `examiner.rs` — the examiner thread: hands each batch to the sink and says what came of it.
- `schedule.rs` — what the examiner examines next, and when: retries, rechecks, the periodic scan. `[tests]`
- `service.rs` — the watcher in the daemon: hands each batch to the examination.

### `crates/konedrived/src/upload/`

The outbox worker: sends the recorded changes to OneDrive; what is kept back. Design:
`writes.md` §5, §6, §8 (moves out), §10.

- `mod.rs` — the worker and its host. `[tests]`
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
- `content.rs` — a file's content going up, a `create` or an `update`: the one upload session every file with content goes through, step by step.
- `local.rs` — the worker's hands on the folder: finding a row's local object.
- `space.rs` — a full OneDrive: what waits for space. `[tests]`
- `kept_back.rs` — what is kept back from OneDrive, grouped by what the user can do about it.
  `[tests]`
- `move_out.rs` — a move out of the folder: what the worker needs for it, and where a `move-out` row's step begins.
- `move_out/row.rs` — one `move-out` row's step: what its cases work with (`MoveOut`), and which case the helper's answer and the object's place make it.
- `move_out/reach.rs` — the helper's answer for an object asked for by its handle (`Reach`): the one place its errnos are read.
- `move_out/cases.rs` — the cases: a file or a folder, to the Trash or elsewhere; gone; kept for another account.
- `move_out/place.rs` — where a moved-out object is now, proved.
- `move_out/trash.rs` — the Trash and its entries.
- `move_out/walk.rs` — walking a moved-out folder.
- `move_out/tidy.rs` — what becomes of an object outside that nothing is downloaded into: the one rule of the Trash case and of dropped rows (`Fate`, `tidy_dirs`).
- `move_out/dropped.rs` — `move-out` rows dropped before they ran: what they left outside is tidied.
- `move_out/protect.rs` — the re-marking of what left, and the routing of its fills.

### `crates/konedrived/src/upload/tests/`

The tests of the worker, by topic.

- `mod.rs` — the worker's steps, its order, its crash points; what the topics share: the one fixture (`World`: the folder of `local/testing.rs`, the harness and the fake helper).
- `harness.rs` — the tests' worker around the fake OneDrive, driven by hand; also used by the bench.
- `candidates.rs` — a `403` on one row, and a new file OneDrive holds with other content.
- `foreign_parent.rs` — a directory that carries another folder's id: nothing is sent into that folder.
- `move_out.rs` — moves out of the folder, on the host: each case once, with the fake helper, behind a real link, finding what left beside the folder.
- `removed.rs` — a file or folder removed before its upload finished.
- `replaced_folder.rs` — a folder replaced offline by a new one that keeps one of its files: the examination's rows and the worker's side of the name rule.
- `sessions.rs` — upload sessions: a crash, a refusal, a changed file or an ended session at each step; and their placeholders.
- `stops.rs` — a worker stopped while a section changes the folder and records it.
- `worker.rs` — the worker's loop: when it waits, and which row goes next.

### `crates/konedrived/src/remote/`

What changed in OneDrive, brought into the folder. Design: `sync.md`, `writes.md` §9 (the
reconcile in read-write mode).

- `mod.rs` — the list of the modules.
- `mode.rs` — `Mode<W>`: read-only, or read-write with what a level needs for it (the folder's
  `Writes`, a cycle's `RwCycle`, a pass's `Rw`).
- `testing.rs` — the one fixture of this area's tests (`World`): a temporary folder, the store, the
  fake OneDrive and the fake helper (`helper/testing.rs`); a real cycle, or the cycle's staging and its
  reconcile in two steps. The one place a test builds a materializer by itself.
- `live.rs` — changes from OneDrive at once: the notification socket's task. `[tests]`
- `listing.rs` — `Listing`: one folder's cycle in three steps (fetch, stage and reconcile, what
  follows a cycle that went through). `[tests]`
- `listing/fetch.rs` — `fetch`: a full listing and the changes since the last one, page by page.
  `[tests]`
- `listing/lease.rs` — the folder's lease a cycle holds while it changes the folder; `sync/` hands it in.
- `listing/poller.rs` — when a cycle runs. `[tests]`
- `listing/reconcile.rs` — the reconcile of one cycle, in either mode: `apply_with_handover`,
  `commit_cycle` (what waits), `after_commit`; what a failed cycle still hands over and records.
- `listing/replacements.rs` — `Replacements`: which changed files are being replaced, wait or
  failed; the workers that replace them, several at once. `[tests]`
- `listing/rw.rs` — `Writes`: what a read-write folder's cycle shares with its outbox worker and
  watcher. `[tests]`
- `listing/stage.rs` — `stage`: what was fetched put into `staging`, and the reconcile it asks
  for (scope, commit, mode); a read-write folder's tree lock, stale-delta guard and what is
  staged again.
- `materialize.rs` — `Materializer`: makes the folder match the tree; the Full and the Changed
  pass, each once for both modes, and `place`. `[tests]`
- `materialize/answers.rs` — what the mode answers: every question a pass asks where a read-only
  and a read-write folder differ.
- `materialize/applied.rs` — what a pass did, left for later, and how it failed (`Applied`,
  `OnDisk`, `Pending`, `ApplyError`).
- `materialize/file.rs` — a file already in place, and what its content needs.
- `materialize/holding.rs` — the holding directory; rescues.
- `materialize/removal.rs` — `take_off`: the one way a managed object is taken off the disk
  (survey, forget, remove, settle), what is kept of it, and what an item that can no longer be
  placed waits for.
- `materialize/replace.rs` — replacing one changed file. `[tests]`
- `materialize/rw.rs` — `Rw`: a read-write folder's rules, read once per reconcile, and what
  they say of a name, a missing item and a local version in the way. `[tests]`
- `materialize/rw/holding.rs` — putting back what was held.
- `materialize/rw/sort.rs` — phase 1 in a read-write folder: what is done with each object, by
  the base, the new tree and what a local change holds.
- `materialize/rw/unplaced.rs` — what can no longer be placed: `after_placement` (it goes or
  waits whole), the step aside.

### `crates/konedrived/src/remote/listing/rw/tests/`

The tests of a read-write folder's cycle.

- `mod.rs` — the cycle with the outbox and the examination; what the topics share.
- `stale.rs` — what the daemon takes off the disk itself is never deleted or moved in
  OneDrive.
- `stale/names.rs` — names taken and given in one listing: the enumeration of small listings.
- `stale/read_only.rs` — the same listings in a read-only folder.

### `crates/konedrived/src/desktop/`

Baloo and thumbnails. Design: `desktop.md`.

- `mod.rs` — the list of the modules.
- `baloo.rs` — keeps KDE's file indexer out of a OneDrive folder. `[tests]`
- `thumbs.rs` — thumbnails from OneDrive. `[tests]`

### `crates/konedrived/src/sync/`

`SyncService`, one per account's folder, a file per responsibility, and the registry. Every
`impl SyncService` is here. Design: `hydration.md`, `sync.md`, `accounts.md`.

- `mod.rs` — `SyncService` and what it holds. `[tests]`
- `wiring.rs` — `Wiring`: what a service is made with, given once to its constructor.
- `testing.rs` — test support, also for the VM suite: a `Wiring` of fakes (the account, a
  temporary `config.toml`, the content sources, the watcher, a clock moved by hand).
- `registry.rs` — `Registry`: the folders of every account, listed by the account manager; whose
  folder an open is in, the overlap check, the claims, what every account is told alike. `[tests]`
- `folder.rs` — what the folder is, as a type; `change`, the one way to change it, and the view the readers read.
- `running_sync.rs` — the running sync of a OneDrive folder as one object in that state: its parts,
  the handles a reader may hold, and how it stops (dropped, then waited for by the next change).
- `publish.rs` — what the bus shows of the folder's state, worked out in one place. `[tests]`
- `persisted.rs` — the folder as `config.toml` records it.
- `bring_up.rs` — a new registration, the folder taken up and brought up at startup and at the
  helper's connect, and the switch to interception.
- `take_down.rs` — forgetting a folder; retiring an account's folder; a folder moved away.
- `start_stop.rs` — starting the sync (prepare, build, run) and asking it for a cycle.
- `populate.rs` — a folder of placeholders made from a local directory.
- `hydrate.rs` — filling a placeholder now.
- `free_up.rs` — freeing up files and whole folders.
- `pins.rs` — putting pins on and taking them off.
- `queries.rs` — what the bus reads: skipped items, activity, conflicts, transfers, states.
- `mode.rs` — the folder's side of the account's mode, and its write gate.
- `outbox.rs` — the outbox worker built and woken; the rows dropped; the outbox as
  `org.konedrive.UploadQueue` shows it. `[tests]`
- `pause.rs` — the pause, the hold, Sync Anyway, and `PauseClock`, which ends a timed pause. `[tests]`
- `move_outs.rs` — the fills of moved-out objects.
- `settings.rs` — the account's settings in `config.toml`: thumbnails, the ignore list, the machine name.
- `watcher.rs` — the watcher of a read-write folder: started, flushed, and what it tells.

### `crates/konedrived/src/sync/tests/`

The tests of `SyncService`, by topic.

- `mod.rs` — what they share: the fake helper, the services, the placeholders.
- `registration.rs` — registering a folder.
- `hydrate.rs` — fills through the service, and one fill per inode.
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
- `manager.rs` — the account manager; the trait `Bus`: how its objects get on the bus.
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
- `export.rs` — putting the objects on the bus and taking them off; `OnBus`.
- `properties.rs` — the table of the properties the daemon announces by itself. `[tests]`
- `signals.rs` — what the daemon announces by itself: `PropertiesChanged`, `ActivityLog.Added`,
  `Accounts.HelperState`. `[tests]`
- `fault.rs` — `Fault`: every refusal under the name of a `Refusal`. `[tests]`

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
- `errno.rs` — `Errno`: the errno of a refusal, and the integer a message carries for it. `[tests]`

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
- `account.rs` — `account`: list, add, remove, rename, mode.
- `login.rs` — `login`.
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
- `uploads.rs` — the upload queue and what is not uploaded. `[tests]`
- `transfers.rs` — transfers and the queue totals. `[tests]`
- `files.rs` — skipped items, conflicts, pins, free-up. `[tests]`
- `refusals.rs` — a refusal explained: the actions, and what the CLI read after the refusal. `[tests]`
- `refusals/folder.rs` — the sentences of a refused folder call, by the group of the action.
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
- `schema/migrations.rs` — the numbered steps that bring an older store to today's schema.
- `meta.rs` — what the store keeps one of: every key of `meta`, and its typed accessors.
- `query.rs` — reading the tree: a row, what is below it, where it is, the counts.
- `plan.rs` — the plan of a reconcile, item by item: the base's row and place, the new tree's.
  `[tests]`
- `read.rs` — `ReadStore`: the store as its read-only connection gives it.
- `forget.rs` — forgetting local objects: the one walk of a subtree, and the rule that a row the
  base does not place records none.
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
- `outbox/row.rs` — a row, a detection, and what an examination and a commit hand the store.
- `outbox/encoded.rs` — a row's snapshot, as the row has it and as its columns keep it, and the
  forms of its `target_name`. `[tests]`
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

- `lib.rs` — the list of the modules, the limits the crates share (`MAX_DEPTH`, `NAME_MAX`,
  `RESERVED_PREFIX`), and `proc_path`, the path of an open descriptor.
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

- `Cargo.toml` — the crate; the feature `testing` (`testing.rs`).
- `build.rs` — the version and the commit a build shows.
- `src/lib.rs` — the D-Bus names. `[tests]`
- `src/accounts.rs` — the proxies of the accounts' interfaces. `[tests]`
- `src/rows.rs` — the rows the daemon answers with, by name. `[tests]`
- `src/refusal.rs` — `Refusal`: every name a call is refused under. `[tests]`
- `src/helper.rs` — `HelperState`: `Accounts.HelperState`, and what to say in each state. `[tests]`
- `src/version.rs` — the version line, the same in every program. `[tests]`
- `src/testing.rs` — a private session bus for tests; only with the feature `testing`.
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
- `structure.yml` — the guard, and the check of the links in the doc comments, on every pull request.

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

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
| `crates/konedrive-tree` | The tree store (SQLite): the items, the tree as OneDrive has it, staging, the outbox |
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
- `clock.rs` — the wall clock, read in one place (`unix_now`), and Unix seconds as a time (`to_system_time`).
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
- `ids.rs` — `AccountId`, the id of an account as a type, and how `config.toml` holds a `DriveId` (the tree store's type).
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
- `source/download.rs` — a download in one stream, into a file that has no name yet; what it
  produced, with the version it was checked as (`Downloaded`).
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
- `mount.rs` — `Mount`: made, served, taken off. It holds the fuser session, the
  `FuseConnection` and the parts, and calls the parts in order three times: at the start, at
  the stop, and at a switch of the access mode — to read-only on the write-operation thread,
  and waited for. It answers for no part: the access mode switch, the cycle step, the
  `Files` interface and the backing directory are handed out (`access_mode_switch()`,
  `cycle_step()`, `files_interface()`, `backing_directory()`), and a cycle and the folder's
  sync ask them. `[tests]`
- `mount/build_order.rs` — `Mount::build`: a stale mount taken off the folder, then the parts
  built in the order they depend on each other, each given only what stands before it;
  nothing is mounted.
- `mount/mount_setup.rs` — `MountSetup`, what a mount is made with, and `WriteSetup`, what a
  folder that takes changes shares with its sync.
- `fuse_connection.rs` — `FuseConnection`: the mount's connection to the kernel by its
  number, and its abort through `/sys`. `[tests]`
- `backing_directory.rs` — `BackingDirectory`: the directory on disk the mount lies over, by
  the descriptor opened on it before the mount; it knows the disk and nothing else. Opening
  it, a path of it and one that names the directory itself, its space, an extended attribute
  of an entry and of the folder. `[tests]`
- `backing_directory/file_handles.rs` — `DiskEntry`, `DiskMetadata`, `EntryKind`: the file or
  directory at a path, with its file handle and what the filesystem says of it; the OneDrive
  id an entry is marked with, whether it is marked with one, and the one rule of an
  unmarked entry (`find_unmarked_disk_entry`), which a walk and a cycle go by. `[tests]`
- `backing_directory/downloaded_copies.rs` — `DiskMarks`: what a file's marks say, with the
  one test of a downloaded copy (marked with the file's OneDrive id, and whole), which the
  lookup, the landing and the legacy check read through; a file made marked with an id and
  not whole (`make_partial_file`), marking a copy, taking its marks off, removing one by its
  OneDrive id alone. `[tests]`
- `backing_directory/file_operations.rs` — the files and directories a change makes, renames
  and removes, and a file opened to read or to write. `[tests]`
- `backing_directory/directory_marks.rs` — the directories down to a path, each marked with
  its OneDrive id, which the caller tells name by name; a file in a directory's way is an
  error. `[tests]`
- `backing_directory/download_landing.rs` — a finished download landed: marked as a downloaded
  copy and given its name, by what is at its path (`TargetContents`, `LandingAction`,
  `LandingOutcome`). `[tests]`
- `backing_directory/write_bits.rs` — the write bits of the folder on disk: removed while no
  mount lies over it (`remove_write_bits`), restored for the daemon under its mount and for
  a folder that is the user's again (`restore_write_bits`); the one table of the modes of a
  directory and a file with and without them (`to_mode_bits`, `WriteBits`). `[tests]`
- `backing_directory/legacy_placeholders.rs` — a directory that holds an older konedrive's
  placeholders: it is not opened, so no mount is put over it. `[tests]`
- `stale_mount.rs` — the mount a daemon that died left on a folder, taken off
  (`unmount_stale_mount`).
- `tidy_step.rs` — what follows the daemon's end (`konedrived --tidy`): a folder's stale mount
  taken off and its write bits removed (`tidy_folder`), for every folder the daemon's
  configuration records as showing OneDrive (`tidy_served_folders`); the directories a file
  manager is told to read again (`list_directories`). `[tests]`
- `directory_reference.rs` — `DirectoryReference`: a reference to a directory of the mount, by
  its OneDrive id and its path. `[tests]`
- `entry_place.rs` — `EntryPlace`: a name in a directory.
- `local_only_entry.rs` — `LocalOnlyEntry`: a file or directory made here that is not in
  OneDrive yet, on disk and at its place; the one place that puts the upload rules together
  with the backing directory's marks: whether what is at a place is uploaded from there
  (`decide_upload`, `is_in_ignored_directory`), and the walk that collects the unmarked
  entries at and below a place. `[tests]`
- `unsent_changes.rs` — `UnsentChanges`: what was moved, removed or changed here and is not
  sent yet, a function of the store's outbox alone. `[tests]`
- `directory_contents.rs` — `DirectoryContents`: what each directory of the mount holds; it
  owns the mount's reading connection to the store and the unsent changes as last read, uses
  `BackingDirectory`, is the one place that works out a sync state and the one part that
  says a file is on disk, and is asked only named questions. A row, the row of the directory
  it is in, the files below it with their states, whether a path holds a file's downloaded
  copy, the path the store has an id at, whether an id is removed here. `[tests]`
- `directory_contents/sync_states.rs` — `FileSyncState`, `DirectorySyncState`, `EntryState`
  and `DirectoryEntry`: a name with its state, each state carrying what exists in it.
  `[tests]`
- `directory_contents/directory_entries.rs` — the entry a directory holds under a name, and
  the names it lists (`ListedName`): the store's rows as the unsent changes leave them, read
  without the backing directory, and the local-only entries, by the one rule of what the
  mount shows as local-only (`find_local_only_at`), which the node resolver and the scan
  for unrecorded entries ask too. The entry made from a listed name, with its sync state;
  the entry a disk entry is marked as. `[tests]`
- `directory_contents/entry_paths.rs` — the path the mount shows a OneDrive id at, the entry
  at a path, and the directories down to a path marked with the ids the mount shows there.
- `node_table.rs` — `NodeTable`: the node ids (`NodeId`) given to the kernel, each with a
  `NodeRecord`: its `NodeKey` — in OneDrive by its id, or local only by its file handle —
  the node id of the directory it is in, and its name. Its maps are its own; the place of a
  node it has is changed by a move alone. A node id is the kernel's inode number: the pair
  of conversions is here. `[tests]`
- `node_resolver.rs` — `NodeResolver`: over the directory contents and the backing directory,
  and owning the node table: the entry a node id stands for now and the node id of an entry
  or of a listed name; a local-only entry that got into OneDrive keeps its node id. A lookup
  moves a node only to where the mount shows it now, and a listed name never moves one; the
  node of a copy a cycle moved aside under its writer goes with that file. `[tests]`
- `attributes.rs` — the attributes the mount gives the kernel for a name, a function of a
  `DirectoryEntry` and the `AccessMode`: kind, size, times, modes. `[tests]`
- `access_mode_switch.rs` — `AccessModeSwitch`: the one holder of whether programs may change
  the folder, answered as an `AccessMode` (`ReadOnly`, `ReadWrite`); a write request asks it
  before it queues its operation, the operation asks it again on the write-operation thread
  — twice by design, and the second answer counts — and the mount switches it to read-only
  on that thread, so nothing is recorded after that switch; to read-write it is switched at
  once. The folder's sync asks it whether the folder takes changes. `[tests]`
- `open_handles.rs` — `OpenHandles`: the files (`OpenFile`: read while not downloaded, read
  from the backing directory, or written) and the directories programs hold open, by the
  kernel's `fh` (`Fh`); it alone counts the writers of a node, says of one it adds whether it
  is the node's first (`WriterRank`), remembers whether one wrote,
  says what closing a file that was open for writing left (`ClosedFile`: the node it wrote
  and the `CloseOutcome`) and whether a node is open for writing — which OneDrive file a
  handle writes is the node table's to say, by the node; a writer is closed on the
  write-operation thread, and counts until then;
  the names an open directory listed when it was first read.
  `[tests]`
- `state_attribute.rs` — `to_state_attribute`: what is shown of a file — online-only, being
  downloaded, downloaded — from its sync state and whether it is being downloaded; the value
  of the `user.konedrive.state` attribute and of the bus. `[tests]`
- `kernel_requests.rs` — `KernelRequests`: the one implementation of the kernel's interface
  (`Filesystem`); it holds the five groups and the inotify bridge, asks whether a request
  is the bridge's own, and hands each request on.
- `kernel_requests/kernel_numbers.rs` — fuser's numbers of a request turned into the layer's
  own (`Fh`, `RequestId`).
- `kernel_requests/lookup_requests.rs` — `LookupRequests`: a name looked up, the attributes
  of a node, `access`, `statfs`, and the answers the inotify bridge's own requests get; each
  answer is found as a value, and the handler replies with it. `[tests]`
- `kernel_requests/directory_requests.rs` — `DirectoryRequests`: a directory opened, read and
  closed: `readdir` from the listed names alone, `readdirplus` making the entries it answers
  with; neither changes the place of a node. `[tests]`
- `kernel_requests/xattr_requests.rs` — `XattrRequests`: the extended attributes the mount
  shows, and their names. `[tests]`
- `kernel_requests/xattr_value.rs` — `to_xattr_value`: the value of an attribute of what is
  in OneDrive, from the entry, its pin and whether it is being downloaded. `[tests]`
- `kernel_requests/read_requests.rs` — `ReadRequests`: a file opened to be read, and read:
  where a read's bytes come from is found as a value (`find_read_origin`) — the file a
  handle was opened from, or its downloaded copy once there is one — and the read is
  answered from it. `[tests]`
- `kernel_requests/read_origin.rs` — `ReadOrigin`: where a read's bytes come from, each case
  carrying what it reads from (the backing file, the file of a running download, the row
  and range of OneDrive, nothing); `choose_read_origin` decides it for a file that is not
  downloaded, from its row and a running download. `[tests]`
- `kernel_requests/write_requests.rs` — `WriteRequests`: everything that changes the folder,
  each made by a write operation on the write-operation thread; here are the type and how a
  request gets onto the thread — a read-only folder's is refused before it is put there.
  `[tests]`
- `kernel_requests/write_requests/name_requests.rs` — the requests that change a name: one
  made, removed, renamed. `[tests]`
- `kernel_requests/write_requests/content_requests.rs` — the requests that change a file's
  content or its time: opened for writing, truncated, the time set, and the close of a writer,
  which is answered at once and made on the thread; `write` and `fsync` go into the file a
  write operation opened, and are answered on the mount's own thread. `[tests]`
- `kernel_requests/interruptible_reads.rs` — `InterruptibleReads`: the reads that wait for a
  range from OneDrive, by the kernel's request number (`RequestId`), which an `interrupt` or the mount's
  end gives up. `[tests]`
- `errno.rs` — the errno of a disk error: as a request is answered with it, and as a number
  for a download.
- `range_readers.rs` — `RangeReaders`: the files being read as ranges, each holding its slot
  of the pool until a moment after its last read (`ActiveRangeRead`, one read in flight), so
  that what is read goes before the background downloads. `[tests]`
- `range_fetcher.rs` — `RangeFetcher`: a range of a file fetched from the source, over the
  range readers; the ranges the source remembered of a file dropped. `[tests]`
- `download_states.rs` — `DownloadStates`: one `DownloadState` per file that has a download
  — running or failed — with who waits for it, the file a running one writes into and how
  far it is written (`RunningDownloadFile`), and the unfinished file a failed one left; and, beside the states, the count of the bytes of each
  file read as ranges, which decides when the file is downloaded whole; no I/O. `[tests]`
- `file_downloader.rs` — `FileDownloader`: the download of a whole file into a file without a
  name, which then lands, in one job on the write-operation thread: where the file is now
  and which version the store has of it are asked there and the file is given its name
  there, so no rename and no cycle runs in between; it
  keeps no state, reports to `DownloadStates` and announces through the inotify bridge.
  `FileToDownload`: a file's row and its path, as whoever asks for a download gives it.
  `[tests]`
- `file_downloader/landing_attempt.rs` — `to_download_result`: what a download reports, from
  how its landing went (`LandingAttempt`); `has_downloaded_version`: whether the store's row
  is of the version that was downloaded. `[tests]`
- `file_downloader/watermarked_source.rs` — the download's source, telling how far the file
  is written as its one stream is read. `[tests]`
- `pins.rs` — `Pins`: the OneDrive ids that carry a pin, changed with the store, which it
  writes, and what keeps a file; it starts nothing. `[tests]`
- `pinned_file_downloader.rs` — `PinnedFileDownloader`: every pinned file that is not
  downloaded, downloaded, over the pins, the directory contents and the file downloader.
  `[tests]`
- `free_up_decision.rs` — `decide_free_up`: whether a downloaded copy is freed
  (`FreeUpDecision`), from the sync state, the pin, whether it is being downloaded and
  whether a program holds it open for writing; the one decision of a free-up. `[tests]`
- `files_interface.rs` — `FilesInterface`: what the daemon's bus interface `Files` asks of a
  mount for a path, over exactly the parts it uses (the node resolver among them, for the
  node whose writers are a OneDrive file's): download, free up (`FreedSpace`), pin,
  how the path stands; the space the downloaded copies below a path take (`DownloadedSpace`); why it
  refuses (`FilesRefusal`). A free-up lists the copies off the write-operation thread and
  frees each in a job of its own on it, where it reads how the file stands, decides and
  removes the copy; the facts of a path are gathered once, as its `PathStatus`. `[tests]`
- `files_interface/path_status.rs` — `PathStatus`: a path's id and pin, and its sync state,
  whether it is being downloaded and whether it is open for writing, which what is shown of
  it and what a free-up does with it are asked of. `[tests]`
- `upload_rules.rs` — `UploadRules`: what of the folder is uploaded at all; it owns the
  account's ignore list and touches nothing. What is uploaded and what is ignored (`UploadDecision`), the names no
  program may make. `[tests]`
- `detections.rs` — what a change in the folder is recorded as: a decision from a typed event
  (`FolderEvent`) and the store's facts (`StoreFacts`), with no disk or mount in it; the
  detections of a local-only entry or of something of OneDrive at a place, built by the
  `detect_…` functions, in an `OutboxBatch`; whether a program holds a file open for
  writing (`WriteState`). `[tests]`
- `detections/entry_created.rs` — `EntryCreated`: a file or directory a program made, also
  where a OneDrive file was just removed. `[tests]`
- `detections/entry_removed.rs` — `EntryRemoved`: something of OneDrive or a local-only entry
  removed. `[tests]`
- `detections/entry_renamed.rs` — `EntryRenamed`: a move of something of OneDrive, a
  local-only file or a local-only directory, over what it displaced (`MovedEntry`,
  `DisplacedEntry`); a local-only file saved over a OneDrive file. `[tests]`
- `detections/file_written.rs` — `FileWritten`: a file opened for writing, closed changed or
  untouched, or truncated (`OneDriveFileWrite`, `LocalOnlyFileWrite`). `[tests]`
- `detections/unrecorded_entries.rs` — `UnrecordedEntries`: the local-only entries a walk
  found that the outbox has no row for. `[tests]`
- `detections/entry_kept_aside.rs` — `EntryKeptAside`: a local-only entry kept beside what
  came to its name from OneDrive; one a program still writes waits for its writer. `[tests]`
- `write_operation_thread.rs` — `WriteOperationThread`: the one thread the write operations,
  the switch to read-only, the scan for unrecorded entries, the freeing of each
  downloaded copy and the landing of a finished download run on, each job under
  the folder's tree lock, which a cycle's reconcile holds in either access mode. Stopped, it runs nothing any more: the stop waits for the job in
  flight, drops the jobs that wait, and never waits for the tree lock. `[tests]`
- `outbox_recorder.rs` — `OutboxRecorder`: the one writer of detections; it owns the store's
  writer and what wakes the outbox's worker (`OutboxWaker`), records an event in one
  transaction, and a cycle's batch inside the cycle's own; it does not ask the access mode.
  `[tests]`
- `conflict_copies.rs` — `ConflictCopies`: the user's own moved aside, to a conflict copy's
  name, where something of OneDrive comes — and the downloaded copy a program writes when a
  change takes its file's content or the file, unmarked and moved aside the same way — and a
  directory removed from OneDrive kept while it holds the user's own, in either access mode; recorded for the outbox in a read-write
  folder alone. It owns the machine's name. `CycleConflicts`, what a cycle's conflicts
  left, `ConflictCopy`, one entry moved aside, `MoveAsideOutcome`, what became of the
  user's own at a path, and `RemovedDirectoryOutcome`, what became of a removed directory's
  directory on disk: left for another directory, removed, or kept as local-only. `[tests]`
- `write_operations.rs` — `WriteOperations`: the operations that change the folder, each the
  access mode asked, what stands at the places looked up, a decision, a step on disk, a
  record and the node ids; it holds the parts they use and owns nothing. A file it opens for
  writing it puts into the open handles, and hands back the `fh`. `[tests]`
- `write_operations/make.rs` — a file or a directory a program makes. `[tests]`
- `write_operations/make_check.rs` — `check_make`: whether an entry (`EntryToMake`) may be
  made at a place, or the errno; `choose_made_entry_record`: what one that was made is
  recorded as. `[tests]`
- `write_operations/remove.rs` — a file or a directory a program removes: recorded, then
  removed; a local-only entry that cannot be removed is recorded again. `[tests]`
- `write_operations/remove_plan.rs` — `plan_remove`: what a remove takes out of the folder
  (`RemovePlan`), or the errno. `[tests]`
- `write_operations/rename.rs` — a file or a directory a program renames or moves inside the
  folder: the lookups, the backing directory renamed — what it held at the new place of what
  the mount shows there kept at the old one until the rename is recorded, and put back when
  it cannot be; anything else there refuses the rename — the record, the node. `[tests]`
- `write_operations/rename_plan.rs` — the decisions of a rename: `plan_rename`, what it
  moves where and over what (`RenamePlan`, `RenameMode`), or the errno;
  `BackingDirectoryMove`, whether the backing directory held anything to move;
  `describe_rename`, what it is recorded as. `[tests]`
- `write_operations/write.rs` — a file a program writes, in its downloaded copy: opened with
  its content or to be overwritten, closed, truncated, and the time it was last modified set
  — recorded before its content changes and before a copy made for it is marked whole;
  what was done to the file (`FileWrite`: opened, closed, truncated), and what a write and
  the last close are recorded as from it (`describe_file_write`, `describe_last_close`).
  `[tests]`
- `unrecorded_entry_scan.rs` — `UnrecordedEntryScan`: the local-only entries the outbox has
  no row for, found by one walk and recorded once the folder takes changes. `[tests]`
- `cycle_step.rs` — `CycleStep`: the step of a cycle of the delta listing over the backing
  directory; it owns its parts and what the cycle now being committed leaves to be done
  once it is. A cycle's changes are read, decided on and performed (`CycleOutcome`) — a copy
  a program holds open for writing is never removed, and is moved aside instead — what it
  kept of the user's own is recorded through the outbox recorder, and once committed it is
  announced. `[tests]`
- `cycle_step/onedrive_change.rs` — `OneDriveChange`: what happened to a name of OneDrive in
  the folder (`Appeared`, `Vanished`, `Moved`, `Stayed`). `[tests]`
- `cycle_step/cycle_actions.rs` — `list_cycle_actions`: what one change does to the backing
  directory (`CycleAction`), from the change and what the disk holds at its paths
  (`BackingDirectoryFacts`); the directories gone from OneDrive, deepest first. `[tests]`
- `cycle_step/uncommitted_cycle.rs` — `UncommittedCycle`: the announcements and the removed
  ids a cycle leaves until it is committed.
- `inotify_bridge.rs` — `InotifyBridge`: a change from OneDrive (`Announcement`) brought to
  whoever watches the folder, made by path through the mount on the bridge's own thread; the
  announcement in progress, and what its name was before the change
  (`AnnouncementInProgress`, `NameBeforeChange`). `[tests]`

### `crates/konedrived/src/upload/`

The outbox worker: sends the recorded changes to OneDrive; what is kept back. Design:
`writes.md` §5, §6, §10.

- `mod.rs` — the worker and its host. `[tests]`: the worker end to end against the fake
  OneDrive, in a folder that stands for the backing directory (`tests/world.rs`,
  `tests/write_operations.rs`, `tests/harness.rs`), one file for each thing it does.
- `engine.rs` — the worker: what it works with, whether it may send, its status, its life.
- `engine/drain.rs` — one drain: which row is taken next, and what the worker waits for.
- `engine/outcome.rs` — how a row's run can end, and the outcome of an answer no step settled. `[tests]`
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
- `listing/reconcile.rs` — the reconcile of one cycle, in either mode, under the folder's
  lease and its tree lock: the folder's mount follows the new tree (`follow_mount`),
  `commit_cycle` (what waits), `after_commit`. `[tests]`
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
  written; an item as `items` holds it (`Item` with its kind, its ids, `ItemVersion`,
  `ItemPlace`, `ContentVersion`, `LocalVersionChange`), the sync state it gives a file, and
  `DriveId`. `[tests]`
- `item_store.rs` — `ItemStore`: the rows of `items`, one per item by local item id; finding
  an item, its path and whether it is left here; applying a change of its local version,
  each a case; the rules that hold of the rows, and who holds each. `[tests]`
- `item_store/item_row.rs` — a row of `items` in the database: where it is read, written and
  deleted, and where a name is checked. Reachable from `item_store` alone.
- `item_store/bindings.rs` — the binding: given first (`bind_item`, `make_onedrive_item`,
  the root's), and moved from one item to another (`take_over_binding`, done with the change
  that says so; `give_up_binding`; `merge_items`). `[tests]`
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
- `staging.rs` — the new tree a cycle builds, and its swap into `onedrive_items`.
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

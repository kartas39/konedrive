# Code quality: `sync/`, `daemon/`, `dbus/`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `sync/start_stop.rs`, `sync/registration.rs`, `sync/outbox_api.rs`, `sync/write_mode.rs`
2; `sync/mod.rs`, `sync/resume.rs`, `sync/forget.rs`, `sync/free_up.rs`, `sync/move_outs.rs`,
`sync/watching.rs`, `sync/hub.rs`, `daemon/manager.rs`, `dbus/fault.rs`, `dbus/signals.rs` 3; the
rest 4; `daemon/stop.rs` 5.

**Is `SyncService` to be taken apart?** By the plan's own test: one part already changes another
part's state (`start_stop.rs:259–262`, `:131–140`, `write_mode.rs:334`, `resume.rs:280`, `SY1`);
tests that need the whole service are not yet a problem. Do not take it apart wholesale. Clusters
of its 33 fields that come out cleanly: the pause clock (`pause_timer`, `pause_shown`, `running`),
the mode switch flags (`mode`, `switched_to_read_write`, `mode_check`; `drop_at_read_only` went
with the fix of `SY1`), and
the running sync (`syncing`, `store`, `source`, `tree_lock`), which is the valuable one and the
risky one.

**First in this area:** `SY1` with `SY2`; then `SY4` with `SY5`; then `SY12` with the pause clock.

## SY1. Two lock orders between `lifecycle` and the tree lock — **defect?**

- **Where:** a read-write cycle takes the tree lock, then `lifecycle` for reading
  (`remote/listing/rw.rs:131`, `:150`, then `:206`). A forced switch to read-only takes
  `lifecycle.read()` (`sync/write_mode.rs:593`), then the tree lock in `drop_outbox` (`:426`).
- **What:** tokio's `RwLock` is fair, so a writer queued in between completes the cycle. Writers
  that queue without stopping the poller: `sync/resume.rs:63`, `sync/watching.rs:82`,
  `sync/registration.rs:33`, `:84`. If it happens, `resume()` never returns, and
  `sync/hub.rs:458–460` resumes accounts one after another before serving, so fills stop for
  every account. The same shape with the inode lock in `free_one` (`sync/free_up.rs:255`, `:261`)
  against `remote/materialize/replace.rs:210`, not traced to the end.
- **Fix:** take the tree lock first in the read-write branch, or stop the tasks as the read-only
  branch does; write the order (tree, lifecycle, inode, `syncing`) into `docs/design/writes.md`.
- **Size:** S. **Risk:** low.
- **Verified 2026-10-03: confirmed, by a test.**
  `sync::tests::onedrive::read_write::a_cycle_a_forced_switch_and_a_bring_up_at_once_all_end`
  (`sync/tests/onedrive/read_write.rs`, branch `verify-sync-remote`, ignored): the forced switch
  and the bring-up never end within 15 s; the same arrangement without the queued writer ends in
  5.6 s, so the hang needs all three parties.
  - **What starts it:** `Account.SetMode read-only` with force on a read-write account
    (`account/mode.rs:228`), while a cycle that reconciles is between its tree lock and
    `remote/listing/rw.rs:206` (a window that includes Graph requests), and a writer that does
    not stop the poller arrives: a helper reconnect (`sync/hub.rs:458–460`), `RegisterRoot` or
    `RegisterWithoutInterception`, or the watcher's "root gone" hook. `resume.rs:115` (`restore`)
    queues the same way.
  - **Effect:** the `SetMode` call never answers and the folder's cycles stop. If the writer is
    the reconnect's `resume()`, `supervise` never reaches `serve_routed`, so intercepted opens are
    not filled for every account. It lasts until something calls `stop_tasks` (a Forget, a mode
    change from another cause) or the daemon restarts. Likelihood: low.
  - **A fix must:** keep one order for the tree lock and `lifecycle` everywhere; mind that
    `drop_outbox` is also called under `lifecycle.write()` after the tasks are stopped
    (`write_mode.rs:96, 604`), that `restore_deletes` takes the tree lock with no `lifecycle`
    (`outbox_api.rs:376`), and that replacements take the tree lock, then the inode lock.
  - **The `free_one` variant**, by reading only: a chain of four (a replacement holding the tree
    lock and waiting for the inode lock; `free_one` holding the inode lock and waiting for
    `lifecycle.read` behind a queued writer; the writer waiting for `drop_pending_uploads`, which
    holds the read and waits for the tree lock). Not tested.
- **Fixed 2026-10-03** in `e3d9f89` (#135): a forced drop stops the tasks, takes `lifecycle` for
  writing, then the tree lock, and turns the folder read-only itself; the only holder of the tree
  lock that waits for `lifecycle` is the cycle, which a stop cancels. The `free_one` variant goes
  with it, by reading. The order is in `docs/design/writes.md` §9; that it is kept by hand, and
  what a forced switch now costs, in `docs/limitations/F198.md`.

## SY2. The lifecycle protocol is kept by comments

- **Where:** "called with `lifecycle` held for writing" in the docs of `start_sync`
  (`start_stop.rs:20`), `let_go_of_activity` (`:259`), `restore_locked` (`resume.rs:119`),
  `upgrade` (`:149`), `retire_locked` (`forget.rs:27`), `forget_locked` (`:203`).
- **What:** the sequence "stop tasks, take the lock, stop again, let go of activity" is copied at
  `forget.rs:140–145`, `write_mode.rs:78–83`, `:598–603`. `stop_tasks` takes the `Syncing` out
  first (`start_stop.rs:222`) and then awaits four stops; dropped part-way, the watcher and the
  thumbnail task run on with nothing to stop them.
- **Fix:** a `WriteHeld<'_>` token those functions take; one `stop_for_change()` that returns it.
- **Size:** M. **Risk:** low (the compiler checks it).
- **Fixed in part 2026-10-04** in `be2e9c9` (#169): the folder's state is behind the lock that guards it
  and is reached only through `change()` → `Stopped`. The running parts are still fields of
  `SyncService` (`D38`); part 4 of B5 moves them.

## SY3. The running sync is not an object; `start_sync` does too much

- **Where:** `start_stop.rs:22–177` (about eleven jobs). The chain
  `self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref())` is written eight times
  (`write_mode.rs:281, 316, 408`; `outbox_api.rs:65, 73, 102`; `move_outs.rs:85`;
  `start_stop.rs:200`). `hub.rs:131, 415, 416` read another service's fields.
- **Fix:** a `RunningSync` owning `Syncing`, `store`, `source`, with `start`, `stop`,
  `outbox(|o| …)`, `watcher(|w| …)`; `start_sync` split into prepare, build, publish.
- **Size:** L. **Risk:** the highest in the area. After `SY2`.
- **Fixed 2026-10-04** in `a0d0f1e` (#172): `RunningSync` owns the parts and lives in the folder's state;
  the parts are linked by handles; a change waits for what it stopped. Three links still go through
  the service by a weak reference (`D42`).

## SY4. A registration's state is four bools; the register sequence is written twice

- **Where:** `sync/mod.rs:374–408`; `resume.rs:68–102`; literals at `registration.rs:446, 535`,
  `resume.rs:256, 316`, `forget.rs:58`; calls like `bind(path, true, true)`
  (`registration.rs:41`) and `commit(root, true, source, fresh, false, report)` (`:322`).
- **What:** copies: `bind`'s intercepted branch (`registration.rs:295–331`) and
  `switch_to_interception` (`resume.rs:204–226`); `abandon` (`registration.rs:513–552`) and
  `undo_switch` (`resume.rs:235–273`); the root-id lookup (`resume.rs:297–314`,
  `forget.rs:45–57`); the unlock walk (`write_mode.rs:158–170`, `forget.rs:261–272`). `commit`
  (`registration.rs:354–492`) does six things in 139 lines.
- **Fix:** an enum for the standing and one for interception; one `register_with_helper`; split
  `commit`. **Size:** M. **Risk:** medium; the VM suite covers the helper path.
- **Fixed 2026-10-04** in `be2e9c9` (#169): `Folder { standing, wanted, is: Absent | Down | Up }`, one
  `with_helper` sequence, one pure `publish`. A bring-up takes only the folder recorded, by its root id.

## SY5. `held` means two things; `Accounts.Remove` is not atomic — **defect?**

- **Where:** `forget.rs:29`, `:41`; `registration.rs:101`; `daemon/manager.rs:323–325`, `:302`.
- **What:** `sync.retire()` forgets the folder and deletes the tree store; if `account.retire()`
  or `config.remove_account` then fails, the account stays listed and refuses every registration
  until the daemon restarts. When `export` fails in `add`, the config entry and the `siblings`
  entry stay.
- **Fix:** an enum (`Active`, `HeldBack(why)`, `Retiring`) and an undo on failure. **Size:** S.
- **Verified 2026-10-03: confirmed, by a test.**
  `an_account_whose_removal_failed_half_way_still_takes_a_folder` (`konedrived/tests/accounts.rs`,
  branch `verify-sync-remote`, ignored), with a wallet whose delete fails: the account stays
  listed, without its folder, and answers "this account is being removed" to Register, sign-in
  and `SetMode`. The same state by trace when `config.remove_account` fails (`manager.rs:325`),
  with the sign-in already deleted. The `add` whose `export` fails is traced too.
  - **Effect:** a OneDrive folder's tree store is deleted and its lock taken off; the files stay.
    Likelihood: low (the Secret Service must refuse the delete, or `config.toml` be unwritable).
  - **A fix must know:** there are two flags (`SyncService::held`, `AccountService::retired`), and
    the folder is already forgotten at the helper when the later steps fail, so an undo cannot
    simply restore it.
  - **Correction:** not only a restart ends it: a second `Remove` that succeeds does too
    (`forget.rs:160–163`).
- **Fixed 2026-10-03** in `57216c9` (#139): a removal that fails at its second or third step takes both
  retirements back, and its refusal says what failed and what became of the folder; the folder's
  retirement is its own flag, `SyncService::retiring`; an `Add` whose export fails takes its
  objects, its entry and its directory back. What is still left behind is in
  `docs/limitations/F205.md`. The standing enum of the "Fix" line is not started.

## SY6. States and refusals as strings — **defect?** in part

- **Where:** `SyncError::Io(String)` at `registration.rs:111`, `start_stop.rs:297, 306`,
  `outbox_api.rs:459`; `Result<_, String>` at `registration.rs:561`, `watching.rs:18`,
  `write_mode.rs:330`, `resume.rs:15`; `populate.rs:69`.
- **What:** **defect?** `RootSource::parse` (`sync/mod.rs:72–78`) turns any unknown value into
  `Local`: a typo in `config.toml` makes a OneDrive folder local and its sync never starts.
- **Fix:** `parse` returning `Result`; more `SyncError` variants; typed note slots (`X1`).
- **Size:** M. **Risk:** D-Bus error names would change.
- **Verified 2026-10-03 (`RootSource::parse`): confirmed, by a test.**
  `sync::tests::registration::a_source_that_config_toml_misspells_is_not_taken_for_local_in_silence`
  (`sync/tests/registration.rs`, branch `verify-sync-remote`, ignored): `source = "OneDrive"`
  comes up as a local folder, ready, with nothing in `LastError`. `config/mod.rs:342` does not
  validate the value.
  - **Effect:** the folder is never listed or kept in step, and files not downloaded cannot be
    filled. If the entry had `baloo_excluded = true`, the entry is rewritten as `source =
    "local"`: the typo becomes permanent and a later Forget leaves the Baloo exclusion on.
    Likelihood: very low; only a hand edit produces another value.
  - **A fix must:** let a Forget of such a folder still reach the helper (`hold`, `bind` and
    `recorded_for_forget` all read `persisted_root()`).
- **Fixed 2026-10-03** in `2cdf5dd` (#144): a `source` that is neither word refuses the bring-up and
  is said in `LastError`; a folder held for it never comes up on a guess. What a folder in that
  state can and cannot do is in `docs/limitations/F211.md`.
- **Fixed 2026-10-04** in `4a0919c` (#157), the notes: the note of a failed switch and the outbox's note are slots of
  the folder's snapshot (`status/snapshot.rs`). `SyncError::Io(String)` and the
  `Result<_, String>` stay.
- **Kept as it was, a candidate defect:** a refused ignore pattern goes out under
  `org.freedesktop.zbus.Error`, its message starting with
  `org.freedesktop.DBus.Error.InvalidArgs: `, while `dbus/org.konedrive.Folder.xml` and the
  proxy's comment promise `InvalidArgs` (`dbus/fault.rs`, `to_fault`). `konedrivectl sync ignore`
  prints it as "changing the ignore list failed: …".

## SY7. Wiring by setters; a test-only surface on the production type

- **Where:** nine `Mutex<Option<_>>` fields set after construction in an order that matters
  (`sync/mod.rs:520–612`, `daemon/manager.rs:221–259`).
- **What:** test-only or dead on `SyncService`: `new` (`mod.rs:457`), `replace_content_source`
  (`:604`), `set_schedule` (`:610`), `watch_helper` (`:741`), `check_helper` (`:545`),
  `supervise_helper` (`:731`), `watch_helper_every`, `set_helper_unit`, `set_helper_socket`,
  `set_link`. `start_watcher` is `#[cfg(test)]` (`write_mode.rs:200`), so the real function is
  `start_watcher_scanned`. `FAIL_WATCHER` (`write_mode.rs:212, 499`). `Options.onedrive`
  (`manager.rs:36`) is a test switch.
- **Fix:** a wiring struct passed to the constructor; test helpers in a test-support module.
- **Size:** M. **Risk:** low; touches every test fixture.
- **Fixed 2026-10-04** in `865ceb4` (#166): `SyncService::new(Wiring)` is the only constructor
  (`sync/wiring.rs`); the setters and the test-only methods are gone; the account is the trait
  `account::FolderAccount`. `sync/testing.rs` (tests and the VM suite only) gives a helper that
  holds an answer until released, a fake account, a clock moved by hand and a builder. One clock
  for the pause (`conditions::running::Clock`). Left for parts 3 and 4 of `B5`: a dozen tests
  still reach `lifecycle`, `syncing`, `store` or a private method, listed in the pull request;
  the upload engine's retry and throttle times are still on the system clock
  (`docs/limitations/F100.md`, `D37.md`).

## SY8. `HelperHub` has two jobs; the daemon keeps two account lists

- **Where:** `sync/hub.rs:40–79`; `daemon/manager.rs:119, 332–333`.
- **What:** `set_conditions` and `set_hold_settings` (`hub.rs:262–299`) are one function twice,
  and deliver outside the lock. Blocking syscalls on the runtime: `hub.rs:149–150` (where
  `:409` uses `spawn_blocking` for the same read), `:386–399`, `:317–318`, `:526`.
- **Fix:** split the registry from the link. **Size:** M. Part of `X2`.
- **Fixed in part 2026-10-04** in `8a4a9a6` (#153), the blocking calls: `by_moved_out`, `by_path`,
  `overlapping` and `device_of` are blocking sections. The split of the registry from the link,
  the two setters and the two account lists are `B5`'s.
- **Fixed 2026-10-04** in `b55039e` (#180): `helper/hub.rs` is the link only; `sync/registry.rs` is the list
  of folders with one writer, the account manager. Registry entries are weak (`D47`).

## SY9. The traits for calling upward

- **What:** `conditions::Accounts` and `hydration::server::Router` are honest. `daemon::manager::
  Bus` and `HelperStateSignal` (`manager.rs:45–68`) have one implementation and zbus types in
  their signatures: they exist because exporting objects and the `HelperState` pump are `dbus/`
  work placed in `daemon/`.
- **Fix:** narrow `Bus` to `export` and `unexport`; move the pump into `dbus/signals.rs`.
- **Size:** M. **Risk:** low. Low priority.

## SY10. Repetition in the D-Bus layer

- **Where:** each coalesced property in three places that must agree (the getter,
  `dbus/signals.rs:95–150`, `:154–279`); `.map_err(to_fault)` 31 times (`dbus/fault.rs:67`);
  two hand-written `DBusError` impls (`fault.rs:144–173`, `184–213`); three error conventions;
  wire-shaped tuples built in `sync/` (`outbox_api.rs:29`, `queries.rs:38, 79`).
- **What:** `status::totals::run` is started by the signal code (`signals.rs:55`), so the totals
  exist only while the folder is exported.
- **Fix:** `From<SyncError>`; one property table; start the totals task with the service.
- **Size:** M. **Risk:** low.

## SY11. Tasks nobody watches

- **Where:** `main.rs:70–81` spawns the supervisor and three watchers and drops the handles.
- **What:** a panic in `supervise` ends helper supervision for good while the daemon says ready.
- **Fix:** select on the handles in `main` and exit non-zero. **Size:** S.

## SY12. Files cut by accident

- `outbox_api.rs` holds the pause clock (`:107–233`), quota (`:78–105`), outbox queries, the
  ignore list (`:392–434`), `machine_name` (`:438`), the free-up guard (`:453`) and `Host`.
  Watcher wiring is split across `write_mode.rs` and `watching.rs`. Three spellings of one
  visibility. Mangled or stale comments: `queries.rs:132`, `resume.rs:174`, `:294–295`,
  `registration.rs:65, 169, 215`, `dbus/fault.rs:8`, `dbus/signals.rs:36, 40`, `populate.rs:83–112`,
  `hydrate.rs:220–227`, `:236–242`, `dbus/mod.rs:1–13`, `sync/mod.rs:198–220`.
- **Fix:** `pause.rs`, `outbox.rs`, one `watcher.rs`; repair the comments. **Size:** S to M.
- **Fixed in part 2026-10-04** in `789dcf0` (#163): `sync/pause.rs` with a `PauseClock`, `sync/outbox.rs`,
  one `sync/watcher.rs`, `sync/settings.rs`; one spelling of the visibility; the comments of
  `queries.rs` and `populate.rs`. Fixed with it: a shorter pause set over a longer one was shown
  as paused up to a minute after it ended. Left for the later parts of `B5`: the comments in
  `resume.rs`, `registration.rs`, `hydrate.rs`, `sync/mod.rs` and `dbus/`. Only `sync/` takes
  its time from a function (`docs/limitations/F100.md`).

## SY13. The daemon's first call can be lost at start — **defect?**

- **Where:** `daemon/startup.rs:91–92`, `:108` (`start_on`, `serve`, `request_name`);
  `dbus/export.rs:87–88`.
- **What:** `bus.serve` makes the first `connection.object_server()` call, and zbus then only
  spawns the object-server task, which still has to register its match rule before it gets
  method calls. Nothing waits for it. If a call reaches the daemon's socket before that task has
  run, the socket reader finds no channel for method calls and drops it, with no reply and no
  error. The name is claimed right after, so a call that activated the daemon over D-Bus arrives
  in exactly that window; neither `konedrivectl` nor the window sets a method timeout, so the
  caller waits for ever.
- **Fix:** let the object server exist before the socket is read: build the connection with
  `serve_at(ACCOUNTS_PATH, ObjectManager)` (zbus then waits for the server to listen), and take
  that line out of `OnBus::serve`. **Size:** S. **Risk:** low.
- **Found 2026-10-04, by reading, after a test hung:** `set_client_id_validates_and_notifies`
  (`tests/dbus_api.rs`) ran for 57 minutes in one whole-workspace run, with every thread parked
  and none blocked on a mutex. A trace of `Accounts.SetClientId`, `Accounts.Add` and the property
  reads found no lock held across a wait and no inversion, and no commit of 2026-10-03 on that
  path; `startup.rs` is as at `4aeefb9`. The lost first call fits the stacks (the test's `setup`
  calls `Accounts.Add` with no timeout). **Status: open; suspected, not reproduced.** Which wait
  was pending is not known. What would settle it: a method timeout on the tests' connection
  (`konedrive-dbus/src/testing.rs`, `TestBus::connect`), so that a lost reply fails by name.
- **Fixed 2026-10-04** in `8fb6ac2` (#146): the connection is built with the `ObjectManager` of
  `/org/konedrive/Accounts`, so the object server listens before the socket is read. What was
  reproduced is a call that arrives before the first export: it was dropped with no reply
  (the test `a_call_that_reaches_the_daemon_before_its_objects_is_answered` failed on `dev`
  with `TimedOut`). The window named above, after the name is claimed, was not reproduced, and
  by the code of zbus 5.19 it should not exist; whether a lost call is what hung the test is
  still only supposed (`docs/limitations/D31.md`). The tests' connection has a method timeout
  of 120 s now; `konedrivectl` and the window still have none (`docs/limitations/D32.md`).

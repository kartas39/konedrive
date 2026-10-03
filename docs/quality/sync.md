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
the mode switch flags (`mode`, `drop_at_read_only`, `switched_to_read_write`, `mode_check`), and
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

## SY3. The running sync is not an object; `start_sync` does too much

- **Where:** `start_stop.rs:22–177` (about eleven jobs). The chain
  `self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref())` is written eight times
  (`write_mode.rs:281, 316, 408`; `outbox_api.rs:65, 73, 102`; `move_outs.rs:85`;
  `start_stop.rs:200`). `hub.rs:131, 415, 416` read another service's fields.
- **Fix:** a `RunningSync` owning `Syncing`, `store`, `source`, with `start`, `stop`,
  `outbox(|o| …)`, `watcher(|w| …)`; `start_sync` split into prepare, build, publish.
- **Size:** L. **Risk:** the highest in the area. After `SY2`.

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

## SY5. `held` means two things; `Accounts.Remove` is not atomic — **defect?**

- **Where:** `forget.rs:29`, `:41`; `registration.rs:101`; `daemon/manager.rs:323–325`, `:302`.
- **What:** `sync.retire()` forgets the folder and deletes the tree store; if `account.retire()`
  or `config.remove_account` then fails, the account stays listed and refuses every registration
  until the daemon restarts. When `export` fails in `add`, the config entry and the `siblings`
  entry stay.
- **Fix:** an enum (`Active`, `HeldBack(why)`, `Retiring`) and an undo on failure. **Size:** S.

## SY6. States and refusals as strings — **defect?** in part

- **Where:** `SyncError::Io(String)` at `registration.rs:111`, `start_stop.rs:297, 306`,
  `outbox_api.rs:459`; `Result<_, String>` at `registration.rs:561`, `watching.rs:18`,
  `write_mode.rs:330`, `resume.rs:15`; `populate.rs:69`.
- **What:** **defect?** `RootSource::parse` (`sync/mod.rs:72–78`) turns any unknown value into
  `Local`: a typo in `config.toml` makes a OneDrive folder local and its sync never starts.
- **Fix:** `parse` returning `Result`; more `SyncError` variants; typed note slots (`X1`).
- **Size:** M. **Risk:** D-Bus error names would change.

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

## SY8. `HelperHub` has two jobs; the daemon keeps two account lists

- **Where:** `sync/hub.rs:40–79`; `daemon/manager.rs:119, 332–333`.
- **What:** `set_conditions` and `set_hold_settings` (`hub.rs:262–299`) are one function twice,
  and deliver outside the lock. Blocking syscalls on the runtime: `hub.rs:149–150` (where
  `:409` uses `spawn_blocking` for the same read), `:386–399`, `:317–318`, `:526`.
- **Fix:** split the registry from the link. **Size:** M. Part of `X2`.

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

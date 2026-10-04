# Code quality: `local/`, `conditions/`, `desktop/`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `local/examine/classify.rs`, `local/examine/missing.rs`, `local/watcher/reader.rs` 2;
`local/mod.rs`, `local/examine.rs`, `local/examine/run.rs`, `local/examine/found.rs`,
`local/liveness.rs`, `local/watcher/mod.rs` 3; `local/ignore.rs`, `local/watcher/fan.rs`,
`map.rs`, `dirt.rs`, `conditions/network.rs` 5; the rest 4.

**First in this area:** `LO3` with `LO4`; then `LO8`, then `X1`; then `LO1` with `LO9` and `LO2`
inside it. `LO5` after these.

## LO1. `Run` is a god-object and its `impl` is cut across six files

- **Where:** `local/examine.rs:280–335` (29 fields), built at `:190–220`; `impl Run` in
  `examine/run.rs`, `list.rs`, `classify.rs`, `found.rs`, `missing.rs`, `finish.rs`.
- **What:** every step takes `&mut self`, so an entry is cloned whole per item
  (`classify.rs:144, 172, 275, 294, 317, 360, 381, 420`; `found.rs:22`). Three untyped `usize`
  index spaces. A decision's progress is five loose collections (`chosen`, `decided`, `deferred`,
  `consumed`, `fresh`). Helpers landed in the wrong file by the cut (`missing.rs:303–336`,
  `classify.rs:380–416`).
- **Fix:** a read-only `Listing`, a `BaseCache`, a `Decisions` type with named transitions, the
  output; newtypes for the indexes. **Size:** L. **Risk:** medium; well covered.
- **Fixed 2026-10-05** in `e6d9db9` (#186): `Run` holds `Listing`, `Facts`, `Decisions` and `Outcome` (10 fields);
  entries are read by `EntryIx`, none is cloned; identity is the pure `identity::identify`. Not done: the
  steps are still methods of `Run` (`docs/limitations/D53.md`).

## LO2. The examination writes to the store and the disk while it is still deciding

- **Where:** `classify.rs:40, 55, 331, 340, 363`; `found.rs:187, 233–234`; `examine.rs:172, 181`.
  The module doc says everything is applied in one transaction (`examine.rs:32–33`).
- **Fix:** collect these as effects applied in `finish`, or state which effects are immediate
  and why a re-examination converges. **Size:** M. **Risk:** medium to high.
- **Fixed 2026-10-05** in `e6d9db9` (#186): the immediate writes go through one type, `Hands`
  (`examine/hands.rs`), which states for each why a re-examination converges.

## LO3. One file's I/O error aborts the whole examination — **defect?**

- **Where:** `found.rs:200–204`; `classify.rs:340`, `:362–363`.
- **What:** a downloaded file with a changed mtime and mode 000 gives `EACCES`, then
  `ExamineError::Io`, then `Handled::Failed`; the batch is retried with backoff
  (`watcher/mod.rs:605–611`) and meets the same file. F55 covers unreadable directories only.
- **Fix:** one per-entry policy (gone means skip; denied means `mark_unreadable` and a recheck) at
  every open in the examination. **Size:** S to M. **Risk:** low.
- **Verified 2026-10-03: refuted as written; a narrower form confirmed by a traced path.**
  - **The mode-000 case does not happen.** `entry::read` reads the attributes by name
    (`local/entry.rs:132`), which gives `EACCES` first; `Run::read_entry`
    (`local/examine/run.rs:141–144`) takes `denied`, calls `mark_unreadable` and goes on. Shown by
    two passing tests in `local/tests.rs` (branch `verify-local`):
    `an_unreadable_downloaded_file_does_not_stop_the_examination` and
    `a_read_only_copy_that_kept_its_attributes_is_stripped_and_uploaded_as_new`.
  - **What holds:** the sites have no per-entry policy (`found.rs:195–204`;
    `classify.rs:318–322, 331, 340, 362–363`): any error that is not `gone` becomes
    `ExamineError::Io`, the batch is merged back and retried with a backoff of 5 s to 600 s.
    Persistent causes need root or bad hardware: a readable file owned by another user that
    carries konedrive attributes and must be stripped, a read error in `hash` (`found.rs:178`),
    `restore` on a foreign-owned placeholder (`found.rs:227–234`).
  - **Effect:** local changes of the whole account stop being uploaded while the file is there;
    the only sign is a log warning, nothing reaches `WatchStatus` or `LastError`. Likelihood: low.
  - **A fix must:** apply one policy at every open, strip and read; not count a passed-over entry
    as missing; not upload a stranger that could not be stripped; bring a batch that keeps failing
    to `LastError`.
  - **Corrections:** `read_entry` already covers every entry, files included; F55's text mentions
    only directories. `Examined::unreadable` is read by nothing but tests, so the user is never
    told a file is not uploaded.
  - **Found beside it, traced and not run:** `entry::read` reads a directory's attributes before
    `classify.rs:80` checks its device, and the `xattr` crate maps only `ENODATA` to "none". A
    filesystem without user attributes mounted inside the folder (vfat, some FUSE mounts) should
    give `EOPNOTSUPP` at `entry.rs:144` and abort every examination, since each Full scan lists
    the mount point. F72 expects such a mount to be listed as `other-device`. The VM suite could
    confirm it with a vfat mount.
- **Fixed 2026-10-04** in `5af7191` (#143), the narrow form: one policy (`Run::entry_io`) for an entry
  that cannot be opened, stripped or read; an entry's own error passes it over, any other fails
  the batch, and a batch that keeps failing reaches `LastError`. What a passed-over entry costs is
  in `docs/limitations/F210.md`.

## LO4. The examiner thread can die unnoticed — **defect?**

- **Where:** `watcher/mod.rs:544–641`; the reader has an `Ending` guard
  (`watcher/reader.rs:265–284`), the examiner none. Panic candidates: `classify.rs:108, 464`,
  `watcher/service.rs:126`.
- **What:** the reader then sends into a closed channel (`reader.rs:405`), `WatchStatus::stopped`
  is never set. On `Handled::RootGone` the examiner returns (`mod.rs:613–619`) while the reader
  keeps walking.
- **Fix:** the same drop guard on the examiner; its exit sets `stop`. **Size:** S.
- **Verified 2026-10-03: confirmed for a panic, by a test; the `RootGone` part refuted.**
  `local::watcher::tests::an_examiner_that_dies_says_the_watcher_stopped`
  (`local/watcher/tests.rs`, branch `verify-local`, ignored): after the sink panics the examiner
  thread is gone, the reader keeps handing over into a closed channel, and `stopped` stays false,
  so `LastError` says nothing. `WatchHandle::flush` does return `false` at once.
  - **`RootGone` is not a defect:** `watcher/mod.rs:613–619` calls `shared.root_gone()`, and the
    status hook (`sync/watching.rs:70–84`) sets the folder to error and stops the sync.
  - **Effect:** nothing is shown; local changes are no longer examined or uploaded until the sync
    restarts. Likelihood: low. No panic reachable today was found: `classify.rs:108` and `:464`
    cannot fire. What remains is a poisoned mutex after a panic on another thread
    (`watcher/service.rs:126`, `sync/write_mode.rs:407`), `runtime.block_on` on a runtime being
    shut down (`service.rs:133`), and any future bug in the examination.
  - **A fix must:** put a guard on the examiner like the reader's `Ending`; the guard runs during
    unwinding, so it takes poisoned locks and must not panic itself; wake the reader so it ends.
- **Fixed 2026-10-03** in `eba0828` (#132): the examiner has the guard `ExaminerEnding`; what an
  unasked end leaves is in `docs/limitations/F74.md` (11).

## LO5. `Reader::visit` and the settle path are hard to change safely

- **Where:** `watcher/reader.rs:117–153` (24 fields), `:505–548`, `:635–711`.
- **What:** behaviour is chosen by a mode enum combined with three booleans (`:671`, `:688`,
  `:708`); `try_settle(parent, name, foreign: bool, deleted: bool)`; the same `match` four times
  (`:541, 648, 675, 729`).
- **Fix:** split `Reader` into walk state, mark bookkeeping and timers; a small policy struct per
  `Walk`. **Size:** M to L. **Risk:** medium; the `MarkDir` paths are covered only by the VM suite.
- **Fixed 2026-10-04** in `0d0d059` (#179): `Reader` is `Tree`, `Marks` and `Timers`; one `WalkPolicy`; one
  `Tree::place_or_rewalk` in place of four copies.

## LO7. `removal` and its callers: invariants by convention, recursion by side effect

- **Where:** `missing.rs:164` (six parameters), `:168`, `:170`, `:201`, `:222`, `:234`, `:282`;
  `run.rs:249` (`hold_back(id, settle, report: bool)`); `examine.rs:497`.
- **What:** `removal`, `left_before` and `missing_item` call each other; termination rests on
  `decided.insert` at `:168` coming before any recursion.
- **Fix:** `enum Leaves { Deleted, MovedOut { object, to } }`; the termination rule stated next to
  `decided`. **Size:** M. **Risk:** medium.

## LO8. The same small logic written several times

- "Same object": `examine.rs:407–410, 464–469`, `found.rs:206–209`, beside `Inode::same_object`.
  "Waiting on a writer": `found.rs:52–57, 108–112, 252`. `Detection` literals of 12 fields:
  `classify.rs:397, 430, 471`; `missing.rs:95, 226`. `record_replaced` and its async twin
  (`local/mod.rs:91–120`); `handles_current` and its async twin (`liveness.rs:189–206`).
- **Fix:** `Detection::new`, a `Readiness` type, `Entry::snapshot()`. **Size:** S each.
- **Fixed 2026-10-04** in `1eee48d` (#184): `Inode::same_object` everywhere, the `Detection` constructors in
  `examine/detect.rs`, one `handles::record_replaced`, `daemon_owned` and `gone` once in `folder/disk.rs`.
  A copy is stripped through the descriptor compared with what was listed (`F275`).

## LO9. Leaving objects are four parallel structures

- **Where:** `examine.rs:183–186, 299–307`; `classify.rs:32–56`; `run.rs:102–113`.
- **Fix:** one `Leaving` type. **Size:** S to M. Goes with `RE1`.
- **Fixed 2026-10-04** in `69d7cd6` (#178): the leaving code of the examination is removed with the mechanism.

## LO10. `liveness.rs` holds three subjects, one with a gap

- **Where:** `liveness.rs:35–127, 129–244, 246–272`.
- **What:** `renew_handles` (`:221–226`) looks up an absolute path following symlinks, where
  `same_place` (`:120–127`) and `absent_at` (`:265–272`) use `openat2` with
  `RESOLVE_NO_SYMLINKS`. `handles_current` is named as a predicate and records `HANDLES_ON`
  (`:192`). `HelperLiveness::whereabouts` blocks up to 45 s per missing item (`:87–91`) while the
  tree lock is held (`watcher/service.rs:65`), with nothing to short-circuit after a first
  timeout.
- **Fix:** `liveness.rs` and `handles.rs`; one path-opening helper; a stuck flag per run.
- **Size:** M.
- **Fixed 2026-10-04** in `f49e23f` (#177): `local/handles.rs` and `local/liveness.rs`, one
  `open_no_symlinks`; a helper that times out is asked once in an examination (`F260`).

## LO11. Test hooks and test doubles

- `watcher/mod.rs:216–217, 252–256, 314–319, 520–530`; `watcher/reader.rs:150–152, 206, 311–315,
  781–785`; `desktop/thumbs.rs:273–309`; `liveness.rs:274–326` (`FakeLiveness`);
  `examine.rs:229`.
- **Fixed in part 2026-10-04** in `0d0d059` (#179): the examiner loop is `Schedule`, tested with `now` as a
  parameter; the watcher's test hooks are gone. The `desktop/` lines are left to B14.

## LO12. Stale comments and double export paths

- `local/mod.rs:16–17` ("Nothing here runs from the daemon yet"), `:90–112`;
  `desktop/baloo.rs:17, 24, 73`; `desktop/thumbs.rs:97–98`; `examine.rs:9–26`. Items reachable
  by two paths (`local/mod.rs:35–40`).
- **Fixed 2026-10-04** in `1eee48d` (#184) for `local/`: stale comments and module paths. The `desktop/` lines
  are left to B14.

## LO13. A mount without user attributes inside the folder aborts every examination — **defect?**

- **Found on 2026-10-03 while verifying `LO3`; traced, not run** (it needs a mount).
- **Where:** `local/entry.rs:144`, `local/examine/run.rs:145`, `local/examine/classify.rs:80`.
- **What:** `entry::read` reads a directory's attributes before `classify.rs:80` checks its
  device, and the `xattr` crate maps only `ENODATA` to "none". A filesystem without user
  attributes mounted inside the folder (vfat, some FUSE mounts) should give `EOPNOTSUPP`, which
  passes `run.rs:145` as an error and aborts the examination; each Full scan lists the mount
  point again. F72 expects such a mount to be listed as `other-device`.
- **To confirm:** a VM scenario with a vfat mount inside the folder.
- **Fix:** the per-entry policy of `LO3`, with `EOPNOTSUPP` on a directory read as "not ours".
- **Verified 2026-10-04: confirmed, in the VM** (a vfat mount inside the folder: a file beside it
  was never uploaded, `LastError` read "Operation not supported"). **Fixed** in `5af7191` (#143):
  `EOPNOTSUPP` reads as "no attribute" where an attribute is read by name, and the mount is
  listed as `other-device`. What the safety of that rests on is in `docs/limitations/F210.md`.


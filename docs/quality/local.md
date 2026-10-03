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

## LO2. The examination writes to the store and the disk while it is still deciding

- **Where:** `classify.rs:40, 55, 331, 340, 363`; `found.rs:187, 233–234`; `examine.rs:172, 181`.
  The module doc says everything is applied in one transaction (`examine.rs:32–33`).
- **Fix:** collect these as effects applied in `finish`, or state which effects are immediate
  and why a re-examination converges. **Size:** M. **Risk:** medium to high.

## LO3. One file's I/O error aborts the whole examination — **defect?**

- **Where:** `found.rs:200–204`; `classify.rs:340`, `:362–363`.
- **What:** a downloaded file with a changed mtime and mode 000 gives `EACCES`, then
  `ExamineError::Io`, then `Handled::Failed`; the batch is retried with backoff
  (`watcher/mod.rs:605–611`) and meets the same file. F55 covers unreadable directories only.
- **Fix:** one per-entry policy (gone means skip; denied means `mark_unreadable` and a recheck) at
  every open in the examination. **Size:** S to M. **Risk:** low.

## LO4. The examiner thread can die unnoticed — **defect?**

- **Where:** `watcher/mod.rs:544–641`; the reader has an `Ending` guard
  (`watcher/reader.rs:265–284`), the examiner none. Panic candidates: `classify.rs:108, 464`,
  `watcher/service.rs:126`.
- **What:** the reader then sends into a closed channel (`reader.rs:405`), `WatchStatus::stopped`
  is never set. On `Handled::RootGone` the examiner returns (`mod.rs:613–619`) while the reader
  keeps walking.
- **Fix:** the same drop guard on the examiner; its exit sets `stop`. **Size:** S.

## LO5. `Reader::visit` and the settle path are hard to change safely

- **Where:** `watcher/reader.rs:117–153` (24 fields), `:505–548`, `:635–711`.
- **What:** behaviour is chosen by a mode enum combined with three booleans (`:671`, `:688`,
  `:708`); `try_settle(parent, name, foreign: bool, deleted: bool)`; the same `match` four times
  (`:541, 648, 675, 729`).
- **Fix:** split `Reader` into walk state, mark bookkeeping and timers; a small policy struct per
  `Walk`. **Size:** M to L. **Risk:** medium; the `MarkDir` paths are covered only by the VM suite.

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

## LO9. Leaving objects are four parallel structures

- **Where:** `examine.rs:183–186, 299–307`; `classify.rs:32–56`; `run.rs:102–113`.
- **Fix:** one `Leaving` type. **Size:** S to M. Goes with `RE1`.

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

## LO11. Test hooks and test doubles

- `watcher/mod.rs:216–217, 252–256, 314–319, 520–530`; `watcher/reader.rs:150–152, 206, 311–315,
  781–785`; `desktop/thumbs.rs:273–309`; `liveness.rs:274–326` (`FakeLiveness`);
  `examine.rs:229`.

## LO12. Stale comments and double export paths

- `local/mod.rs:16–17` ("Nothing here runs from the daemon yet"), `:90–112`;
  `desktop/baloo.rs:17, 24, 73`; `desktop/thumbs.rs:97–98`; `examine.rs:9–26`. Items reachable
  by two paths (`local/mod.rs:35–40`).

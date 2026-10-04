# Code quality: `remote/` and `status/`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `materialize/rw/leaving.rs` 1.5; `materialize/rw.rs` 2; `listing/rw.rs` 2; `listing.rs`
2.5; `listing/fetch.rs`, `listing/replacements.rs`, `materialize.rs`, `materialize/holding.rs`,
`materialize/rw/removal.rs`, `status/activity.rs` 3; `materialize/file.rs`,
`materialize/replace.rs`, `materialize/rw/holding.rs`, `status/snapshot.rs` 3.5;
`listing/poller.rs` 4; `live.rs`, `status/totals.rs` 4.5.

**First in this area:** `RE3`, then `RE2`; then `RE8` with `RE9`; then `RE1` on top of them, with
the #104 test gaps filled first. `RE4` waits for these.

## RE1. The leaving mechanism (issue #104) — **defect?** in part

- **Where:** `materialize/rw.rs:398–446` (`where_it_was`), `materialize/rw/leaving.rs:43–134`
  (`leaving_rw`), `:139–182` (`leaving_at`), `:187–226` (`leaving_by_handle`), `:277–311`
  (`keeps_leaving`); `local/examine/classify.rs:40, 55`.
- **What:**
  - "Is this object the leaving one" is written four times (the three functions above and the
    examination); the copies already differ (the hard-link count is checked in some).
  - `where_it_was` is named as a question and writes the store (`leaving_set_handle` at
    `rw.rs:437`, `leaving_set_rel` at `:443`); it reads the whole `leaving` table per misplaced
    entry (`:406`) and asks `locate(Staging, id)` three times (`:393, 396, 440`).
  - The row's path is rebased by hand at every rename: `materialize/holding.rs:49`,
    `materialize.rs:565`, `materialize/rw/holding.rs:124`. **defect?** `copy_aside` renames a
    directory (`rw.rs:651`) and rebases the outbox (`:669`) but not `leaving`; only the handle
    walk would recover it, and not on a filesystem without handles. Not checked against the
    store's code.
  - `leaving_rw` is a 90-line loop body with a four-deep match (`:56–85`), four `leaving_drop`
    sites and eight `continue`s.
  - The whole local examination runs inside the reconcile, under the tree lock, and then
    `local_skipped()` is filtered by reason strings (`leaving.rs:306`).
  - Store errors become `io::Error::other(e.to_string())` (`leaving.rs:150, 159, 174`), logged and
    skipped (`:58–61`), where elsewhere a `TreeError` fails the cycle.
  - "unplaced" means four things: `Rw::unplaced` (`rw.rs:88`), `Run::unplaced`
    (`materialize.rs:213`), `Was::Unplaced`, `Materializer::unplace`.
  - `Was` has seven variants handled by two matches in `full_rw` with different mappings
    (`rw.rs:310–320`, `:326–344`). Dead code at `leaving.rs:93–94`.
- **Fix:** one `Leaving` type that owns the identity rule (a pure `find(id)`) and the row's
  lifecycle; one store call that returns an entry's facts, so that `where_it_was` is a pure
  function of a struct; `leaving_rw` split into locate, decide (an enum) and act; renames through
  one `Materializer::rename_tracked` that rebases. Shares `LO9` and `TR2`.
- **Size:** L. **Risk:** high: the delete-safety path (F193, F194, F196); the #104 test gaps are
  open; `listing/rw/tests/stale.rs` is the net.
- **Verified 2026-10-03 (only the `copy_aside` claim): refuted, by a passing test.**
  `remote::materialize::rw::tests::a_directory_kept_aside_takes_what_is_leaving_in_it_along`
  (`remote/materialize/rw/tests.rs`, branch `verify-sync-remote`; since B4-2 the store's half is
  `outbox::tests::a_directory_kept_aside_takes_what_is_leaving_in_it_along` in `konedrive-tree`,
  until part 6 removes the leaving rows): `copy_aside` applies
  `OutboxOp::Rebase` (`rw.rs:669–670`), and the store's `rebase` ends in `rebase_leaving`
  (`konedrive-tree/src/outbox.rs:381–390`), which moves the `leaving` rows at and below the
  directory. The three hand-written sites are renames that apply no `Rebase` op. The rest of this
  finding is about structure and stands.
- **Fixed 2026-10-04** in `69d7cd6` (#178): the leaving mechanism is gone. What can no longer be placed stays
  placed and waits as a deferred change; it yields its name by stepping aside to its copy name
  (`F188`). One `take_off` under policy `Unplaced`. Store schema 8.

## RE2. `reconcile` and `reconcile_rw` are two copies

- **Where:** `listing.rs:613–706`, `listing/rw.rs:205–366`.
- **What:** identical in both: the lifecycle lock and link check, the `pending_drive` write, the
  "no root yet" branch (`listing.rs:639–650` and `rw.rs:232–240`), building the `Materializer`,
  the Changed-to-Full hand-over, the `Said` choice. `reconcile_rw` is one 140-line blocking
  closure that adds seven steps inline and a test hook.
- **Fix:** one `reconcile(mode)` with the shared skeleton; the read-write additions as named
  functions (`plan_deferred`, `settle_outbox_after_swap`, `hand_to_watcher`).
- **Size:** M. **Risk:** medium: the order of steps around the swap matters (F190).
- **Fixed 2026-10-04** in `48cc690` (#170): one `Listing::reconcile` over `Mode`, `commit_cycle`, one commit
  tail; `before_swap` is gone; the tests of `remote/` run on one `World` (`remote/testing.rs`).

## RE3. `Applied` is merged by hand in four places, one of them exhaustive

- **Where:** `materialize.rs:63–107` (15 fields); `listing.rs:275–311` (`Reconciled::add`,
  exhaustive); `materialize.rs:242–249`, `:259–271`; `listing/rw.rs:269–277`.
- **What:** three sites carry over a hand-picked subset. A field added later that must survive a
  failed first pass is silently dropped there.
- **Fix:** split `Applied` into `Counts`, `OnDisk` and `Pending`; one `OnDisk::absorb`.
- **Size:** S to M. **Risk:** low.
- **Fixed 2026-10-04** in `df8c5a1` (#162): `Applied` is three parts with one merge.

## RE4. The read-only and read-write passes are forked by copy; the mode is passed twice

- **Where:** `materialize.rs:400–469` (`changed`) and `materialize/rw.rs:480–592` (`changed_rw`);
  `materialize.rs:345–388` (`full`) and `rw.rs:239–381` (`full_rw`); `self.rw` tested inline at
  `materialize.rs:277, 501, 521, 527, 538, 579` and `materialize/file.rs:36, 48, 76, 84, 94`.
- **What:** the mode lives in `Materializer::rw: Option<Rw>` and as a `rw: &Rw` parameter
  (`materialize.rs:317–329`). The plan is cloned whole (`rw.rs:251`).
- **Fix:** a policy (trait or enum) with the few decisions that differ, and one skeleton for
  `changed` and for `full`.
- **Size:** L. **Risk:** high for behaviour; both modes have tests. After `RE1`–`RE3`.

## RE5. Blocking file I/O in replacements

- **Where:** `materialize/replace.rs:121–260` (`replace_inner`): `sync_data` (`:194`), `sync_all`
  (`:201`), `swap_in` (`:249`), up to `REPLACE_WORKERS` at once, with the tree lock and the inode
  lock held across the swap (`:206–210`).
- **Fix:** the pre-check and the swap as two `spawn_blocking` sections around the async download.
- **Size:** M. **Risk:** medium (F14 is nearby). Part of `X2`. Not found in the limitations log.
- **Fixed 2026-10-04** in `cc0ee68` (#152): three blocking sections (the look before the download, the
  sealing after it, the swap under both locks); a stop ends a replacement only at its waits, so
  `Poller::stop` still returns when no replacement code runs. `record_replaced_async`'s file
  calls are still on the runtime thread (`docs/limitations/F232.md`).

## RE6. One store failure is blocking or not by the line — **defect?**

- **Where:** `listing.rs:639` (`CycleError::Apply`, retried quietly) against `:645, 689, 697`
  (`CycleError::Store`, which turns the folder to `error`); `listing/rw.rs:232` against
  `:235, 241`.
- **What:** also `CycleError::Store(String)`, `Apply(String)`, `Offline(String)`
  (`listing.rs:151–158`) and `ApplyError::Io(String)` (`materialize.rs:148–158`) discard the
  source error.
- **Fix:** keep `#[source]` errors; one mapping function. **Size:** S. **Risk:** low.
- **Verified 2026-10-03: confirmed as stated, by a test of the two mappings; small as a defect.**
  `remote::listing::tests::a_store_failure_is_the_same_trouble_wherever_a_reconcile_meets_it`
  (`remote/listing/tests.rs`, branch `verify-sync-remote`, ignored).
  - **What differs:** blocking trouble makes `RootState` read `error` (`status/snapshot.rs:276`)
    and closes the write gate (`sync/write_mode.rs:379`); the other kind is only a line in
    `LastError`. The poller retries both on the same schedule, and both are logged.
  - **Wider than the lines named:** `ApplyError::Tree` (`materialize.rs:150`) sends every store
    failure inside the materializer through `applying` (`listing.rs:198`) to the non-blocking
    `Apply`.
  - **A fix must:** decide which of the two a store failure is; `CycleError::blocking`
    (`listing.rs:168`) is the only place that says.
- **Fixed 2026-10-03** in `2cdf5dd` (#144): a store failure while a listing is applied is
  `CycleError::Store`, blocking like the one at the commit, and the outbox worker is woken when
  blocking trouble clears. The store failures that are still passed over or only warned about
  are in `docs/limitations/F212.md`.

## RE7. `sync_once` does everything

- **Where:** `listing.rs:417–542`; read-write logic inline in `listing/fetch.rs:150–178`.
- **What:** about nine jobs in 125 lines. The mode is told by `fetch_seq: Option<i64>`;
  `Fetched::Placed` is handled at `:453` and again, unreachably, at `rw.rs:129`.
- **Fix:** `fetch`, `reconcile_fetched(mode)`, `after_cycle`. **Size:** M. **Risk:** low to
  medium.

## RE8. Store traffic per item in the Changed scope

- **Where:** `materialize.rs:411, 412, 423, 438, 449, 450`; `materialize/rw.rs:508, 509, 530, 535`.
- **What:** about seven `call_blocking` hops per id, several repeated, for up to 5,000 ids.
- **Fix:** one store call that returns the plan of every item. **Size:** M. **Risk:** low.
- **Fixed 2026-10-04** in `2e21d56` (#171): `TreeStore::plan(ids)` is read once per Changed pass, and
  `where_it_was` is a pure function of it. Not timed (`F244`).

## RE9. The forget, remove, settle protocol is kept by convention

- **Where:** written out at `materialize/rw/removal.rs:90–96`, `rw/leaving.rs:124–129`,
  `:245–252`, `materialize/holding.rs:65–72`.
- **What:** the read-only drain passes the survey through a field (`materialize.rs:210`) and
  rebuilds it (`holding.rs:94`) with helpers that exist only to get round privacy after the cut.
  Flag parameters `forget_before_removing(.., by_id: bool)` and `remove_whole(keep: Option<&Rw>)`.
  The same-device and link-count checks are written several times.
- **Fix:** one `removing(dir, name, by_id, f)` that owns the protocol; move `Survey` and these
  functions out of `rw/` into `materialize/removal.rs`. **Size:** S to M. **Risk:** low.
- **Fixed in part 2026-10-04** in `df8c5a1` (#162): one `Materializer::take_off(dir, name, policy)`
  removes every managed object; under policy `Removed` local work stays and goes up as new (`F238`,
  `F243`). The leaving code still has its own path until the later parts of B4.
- **Fixed 2026-10-04** in `69d7cd6` (#178), the rest: the leaving code's own removal path is gone with it.

## RE10. Replacement bookkeeping: four mutexes, a tuple, string comparison — **defect?** in part

- **Where:** `listing.rs:231–239`; `listing/replacements.rs:101, 204–207, 230`.
- **What:** one state machine over four `std::sync::Mutex`es; a failure is "news" when its message
  text differs; `needs_full` is set from six places in four files. **defect?** a worker cancelled
  between `swap_in` (`replace.rs:249`) and `record_replaced_async` (`replacements.rs:180`) leaves
  the new inode unrecorded; whether the next cycle repairs it was not traced.
- **Fix:** a `Replacements` struct with one mutex and a typed failure reason;
  `Listing::request_full()`. **Size:** M. **Risk:** medium.
- **Verified 2026-10-03 (only the cancelled replacement): refuted, by a passing test and a trace.**
  `remote::materialize::replace::tests::a_leased_replacement_stopped_right_after_its_swap_has_recorded_the_new_inode`
  (`remote/materialize/replace/tests.rs`, branch `verify-sync-remote`). In read-write mode the only
  await between the swap and `record_replaced_async` is `land_deferred` (`replace.rs:255`), whose
  job is already sent and records the handle itself (`konedrive-tree/src/reconcile.rs:300–305`).
  In read-only mode there is no await between the swap and the sending of `set_local_handle`.
  What is left is a cancel while the send waits on a full store queue of 1,024 jobs; the next
  examination that sees the file repairs it (`local/examine/found.rs:34–37`). The rest of this
  finding is about structure and stands.
- **Fixed 2026-10-04** in `5252892` (#173): one `Replacements` state behind one lock, `Failure { reason, text }`,
  a typed `ReplacementNote`, `Listing::request_full()`. A cut wait for the workers loses none.

## RE11. `status/activity.rs` holds four things

- **Where:** `status/activity.rs`; the doc comment of `Activity`'s locking (`:215–235`) is
  attached to `PRUNE_BATCH` (`:238`); `Event.kind` is a `String` (`:141`).
- **What:** `Activity` holds its mutex across SQLite calls by design; correct only while every
  caller is on a blocking thread, which nothing enforces.
- **Fix:** `activity.rs`, `transfers.rs`, `space.rs`, `report.rs`; the file walk to `folder/`.
- **Size:** S. **Risk:** very low.
- **Fixed 2026-10-04** in `5252892` (#173): `status/` is `activity.rs`, `transfers.rs`, `space.rs`, `report.rs`;
  the walk is `folder/walk.rs`; the kind is `konedrive_tree::ActivityKind` (`F247`).

## RE12. Test hooks and leftovers

- `Writes::before_swap` (`listing/rw.rs:76–77, 222–223, 296–299`; F190): gone with `RE2` (B4-2). `Poller::live_up`
  (`listing/poller.rs:89`) is public for one test. `Listing::writes()` panics on a read-only
  folder (`rw.rs:110`), guarded by convention. Stale doc paths in `status/totals.rs:9` and
  `status/snapshot.rs:117, 137, 142, 155, 175, 177`. Meta keys as literals
  (`listing.rs:480, 500, 569, 588`; `rw.rs:156`). `SyncSnapshot` is a flat bag of 35 public
  fields.
- **Fixed 2026-10-04** in `5252892` (#173): `Poller::live_up` is gone, the cycle carries its `&Writes`,
  `SyncSnapshot` is six structs. `locked` beside `writes` in `ListingContext` is left to `RE4`.


# Code quality: `konedrive-tree`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `lib.rs`, `reconcile.rs`, `outbox/schema.rs` 2; `staging.rs`, `thumbs.rs`, `outbox.rs`,
`outbox/pick.rs`, `outbox/worker.rs` 3; the rest 4.

**First in this crate:** `TR1` with `TR2`; then `TR3` with `TR6`, and `TR7` with them.

## TR1. `commit_staging_deferring` is two transactions — **defect?**

- **Where:** `reconcile.rs:216–274` (a first transaction committed at `:271`, then
  `commit_staging` at `:273` opens a second); `staging.rs:44–97`. The doc (`:204–215`) says one.
- **What:** a crash or an SQLite error between them leaves the consumed deferrals deleted and
  never swapped into `items`; the next `stage_rw` wipes `staging`, and the delta cursor does not
  send those entries again. Whether the daemon heals was not traced. Also: `outbox_record_opening`
  (`outbox/worker.rs:215–240`) is two autocommit statements, and `outbox_settle_not_found`
  (`outbox.rs:545–562`) a loop of them, under a doc that says each is one transaction.
- **Fix:** extract the swap into `fn swap(tx, whole, delta_link)`, called inside one transaction.
- **Size:** S. **Risk:** low.
- **Verified 2026-10-03: confirmed, by a test.**
  `reconcile::tests::a_swap_that_fails_keeps_the_deferred_changes_it_consumed`
  (`konedrive-tree/src/reconcile/tests.rs`, branch `verify-tree-graph`, ignored): a swap made to
  fail leaves `items` and the link as they were, and the consumed `deferred` row already gone.
  The daemon calls it at `remote/listing/rw.rs:314`; the next cycle's `stage_rw` reads an empty
  `deferred` and wipes `staging`; the consumed deferrals came from earlier cycles whose link is
  past them, so they never come back.
  - **Effect:** the disk already matches the consumed change and `items` keeps the old row for
    good. The next Full reconcile makes the disk match the old row again: an online-only
    placeholder gets the old size, time and cTag back, a downloaded file is queued for
    replacement to the old cTag, a rename or move is undone, a consumed delete is made a
    placeholder again. A likely follow-on, not traced: an upload from the stale base ends as a
    conflict copy. No user content is deleted as far as traced. It heals when the item changes
    again in OneDrive, an outbox commit rewrites it, or a full listing runs.
  - **Likelihood:** low: a read-write folder, a cycle that settles a deferred change, and a crash
    between two adjacent commits or an SQLite error in the second (full disk, I/O error). Silent
    and lasting when it happens.
  - **A fix must:** one transaction for both parts; set `self.whole = false` only after the
    commit; roll the `outbox_gone` prune back with the rest; keep `commit_staging` usable alone
    for the read-only path.
  - **Corrections:** `outbox_record_opening` has two statements only in its "recorded at another
    place" branch (`outbox/worker.rs:226, 234`). Neither `outbox_settle_not_found`'s doc nor its
    module's says "one transaction", and a partial loop is settled again by the next cycle.
- **Fixed 2026-10-03** in `eff1fd2` (#133): `swap` runs in the caller's transaction. The two
  autocommit statements of `outbox_record_opening` stay as they are: a crash between them leaves
  at worst a second `upload_openings_left` row, which `left_at` prunes later.

## TR2. "Forget the local objects below X" three times, with different reach — **defect?**

- **Where:** `lib.rs:710–766` (`forget_subtrees`, walks `items` and `staging`);
  `outbox/worker.rs:41–57` and `outbox.rs:634–649` (walk only `items`).
- **What:** a row only the staged tree has below the folder keeps its handle. The recursive
  `below` CTE is written seven times (`source.rs:54`, `lib.rs:321, 753`, `staging.rs:180`,
  `outbox.rs:640`, `outbox/pick.rs:256`, `outbox/worker.rs:47`).
- **Fix:** one `forget_subtrees`; every subtree walk through `source::below_sql`.
- **Size:** S. **Risk:** more is forgotten, the safe direction.
- **Verified 2026-10-03: the difference is real; refuted as a delete-safety defect.**
  `outbox::tests::dropping_a_row_forgets_what_the_new_tree_has_below_its_item`
  (`konedrive-tree/src/outbox/tests.rs`, branch `verify-tree-graph`, ignored) shows a row that is
  below the folder only by the staged tree keeping its handle.
  - **Why it does no harm today:** every caller that forgets holds the tree lock
    (`upload/steps.rs:272, 630`, `upload/move_out/cases.rs:380`, `sync/outbox_api.rs:376`,
    `sync/write_mode.rs:426`, the examiner), and a read-write delta cycle holds that lock from
    before `begin_staging` to after the swap, so `staging` is empty when these run. In the
    remaining cases (a failed cycle's leftover, a read-write full listing, a read-only cycle) a
    row below the folder only by the staged tree has its object, if any, outside the folder's
    directory on disk. `sync/move_outs.rs:114` is the one caller without the lock; the store is
    dropped right after.
  - **For the merge into one function:** "more is forgotten, the safe direction" is not
    automatic. An item OneDrive moved into the folder would lose the handle of an object that
    still exists elsewhere, and the reconcile must then find it by its id rather than place a
    second one: that needs a materializer test. Today's safety rests on the tree lock, not on the
    SQL; the comments at `outbox/worker.rs:42` and `outbox.rs:635` describe a state the lock no
    longer allows.
  - **Correction:** the walk is over `items` only, but the `UPDATE` runs on both tables by id.

## TR3. The schema version does not describe the schema

- **Where:** `lib.rs:57, 424–486`; `reconcile.rs:30–58`; `outbox/schema.rs:33–106`.
- **What:** `SCHEMA_VERSION = "4"`, and after the version check five more upgrade steps run on
  every open, outside any transaction. Columns are found with `pragma_table_info`. A `DROP
  TRIGGER` and a backfill run at every start. The trigger `upload_openings_left_behind`
  (`schema.rs:66–71`) reads the wall clock. The `CREATE TABLE outbox` (`schema.rs:11`) lacks
  `size`. Paths have two encodings (`outbox.rs:87`, `reconcile.rs:376`).
- **Fix:** an ordered list of migrations by version, each in a transaction, in one `schema.rs`;
  the trigger's work into the Rust functions that delete outbox rows.
- **Size:** M. **Risk:** medium; needs fixture stores of the old shapes.

## TR4. Outbox row writes are duplicated; `reason` and `snapshot` are strings with meaning

- **Where:** `outbox.rs:303–381` (`insert` and `rewrite`, 22 columns each);
  `outbox/pick.rs:518–520`; `outbox/sums.rs:16–17`; `outbox.rs:209`.
- **Fix:** one `bind(row)`; the `Reason` enum of `X1`; `snapshot_size` and `snapshot_mtime`
  columns. **Size:** M. **Risk:** medium; goes with `TR3`.

## TR5. The public API is the whole `TreeStore`

- **Where:** about 160 `pub fn`, about 149 call sites in the daemon.
- **What:** `meta` and `set_meta` take raw keys, which the daemon writes as literals;
  `TreeError::Sql(rusqlite::Error)` is public; `OutboxRow` is all-`pub` and
  `outbox_amend(seq, impl FnOnce(&mut OutboxRow))` (`outbox/worker.rs:82`) lets a caller rewrite
  any column; the read-only connection is the same type (`lib.rs:410–416`); `OutboxRow` derives
  `Debug` over `session_url` (`outbox/row.rs:108, 128`).
- **Fix, in steps:** a `SessionUrl` newtype with a redacting `Debug` (S); typed meta accessors
  (S); a `ReadStore` type (M); facades by topic (L, after the #104 and #111 test gaps).

## TR6. `lib.rs` holds six things

- **Where:** `lib.rs:69–83, 105–243, 245–310, 317–335, 545–664, 710–791`.
- **What:** the literal `'placed'` 17 times; `Placement::decode` reads any unknown text as
  `Placed` (`:179–184`) and `row_from` any unknown kind as `File` (`:681`), where the outbox fails
  closed; `deferred_change` (`reconcile.rs:73–96`) is `row_from` again; rows decoded by position.
- **Fix:** `schema.rs`, `model.rs`, `query.rs`, `forget.rs`; one row decoder per table; unknown
  placement and kind fail closed. **Size:** M. **Risk:** low for the split.

## TR7. The dependency on `konedrive-graph` is the wrong way round

- **Where:** `lib.rs:30, 245–310` (`classify`, `skip_reason`);
  `konedrive-graph/src/drive/item.rs:6–11` (`NAME_MAX`, `RESERVED_PREFIX`).
- **Fix:** `classify` and `skip_reason` to the daemon, below `upload/` and `remote/`; the two
  constants to `konedrive-fs`. **Size:** S. **Risk:** low.

## TR10. Test hooks and test-only API

- `outbox/pick.rs:344–345` (a `#[cfg(test)] assert_eq!` in a production function, against
  `outbox/dependencies.rs`); public and used only by tests: `outbox_runnable`, `outbox_blockers`,
  `outbox_record`, `outbox_under`, `upload_opening_at`, `conflict_kind`, `in_memory`. D24 records
  four more.

## TR12. Tuples and bare strings where a type is missing

- `commit_staging_deferring(delta_link, consumed, defer, content, seq)` takes three `&[String]`
  in a row; tuple returns (`reconcile.rs:41, 390`, `thumbs.rs:16`); `outbox_copied` with seven
  parameters (`outbox/worker.rs:422`); `begin_staging(copy_items: bool)` whose `true` now means
  the opposite of its name (`staging.rs:11`). `thumbs.rs`: the `thumb_key` format is written in
  SQL (`:38`) and in the daemon (`desktop/thumbs.rs:67`).

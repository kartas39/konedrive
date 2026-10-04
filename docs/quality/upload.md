# Code quality: `upload/`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `engine/drain.rs`, `move_out/cases.rs` 2; `mod.rs`, `engine.rs`, `steps.rs`, `content.rs`,
`space.rs`, `kept_back.rs`, `move_out.rs`, `move_out/tidy.rs`, `fake.rs` 3; `engine/outcome.rs`,
`local.rs`, `move_out/walk.rs`, `move_out/place.rs` 4; `move_out/trash.rs` 5.

**First in this area:** `UP2` with `UP3`; then `UP4` with `UP1`; then `UP5` with `UP9`. `UP6` and
`UP7` ride along.

## UP1. The sign-in latch and four facade methods have no production caller — **defect?**

- **Where:** `engine/drain.rs:247, 255` (set), `engine.rs:402` (read), `engine.rs:307–317`
  (`signed_in`, the only place it is cleared, and the only caller of `outbox_unblock(&[FORBIDDEN])`);
  `mod.rs:533–610`.
- **What:** nothing outside `upload/` and tests calls `OutboxWorker::signed_in`, `set_online`,
  `pause`, `resume`, `status` or `subscribe`. One 401 or 403 then stops the worker until it is
  rebuilt (`sync/write_mode.rs:244`). Whether a sign-in always rebuilds it was not traced.
- **Fix:** wire `signed_in` from the account state, or delete the latch and rely on
  `OutboxHost::may_write`; delete the dead methods and `online`. **Size:** S after M of
  investigation.
- **Verified 2026-10-03: confirmed for a `403`, by a test and a trace; the `401` part refuted.**
  `upload::tests::candidates::a_row_blocked_by_403_goes_again_with_the_worker_a_sign_in_builds`
  (`upload/tests/candidates.rs`, branch `verify-upload`, ignored): after a `403` a newly built
  worker leaves the row `blocked` with `forbidden`.
  - **The path:** a `403` on any write becomes `WriteError::Forbidden`
    (`konedrive-graph/src/drive/write.rs:253`), then `Outcome::Forbidden`; `settle` sets
    `needs_sign_in` and blocks the row (`engine/drain.rs:252–262`); `may_start` is false from
    then on (`engine.rs:402`). No caller of `OutboxWorker::signed_in` or of `outbox_unblock`
    exists outside `upload/` and tests. `Refresh()` only calls `retry_outbox`
    (`sync/start_stop.rs:288–291`). The host drops the worker's `needs_sign_in` and `last_error`
    (`sync/outbox_api.rs:523–540`), so nothing tells the user to sign in.
  - **Does a sign-in rebuild the worker? Yes, always:** a sign-in is possible only from
    `SignedOut`, every sign-out sets the mode to read-only, which stops the sync with its worker,
    and the return to read-write starts a new one (`sync/write_mode.rs:74–121, 240`). So the
    latch goes with a sign-out and sign-in, a restart, a mode switch or a Forget. The `forbidden`
    row does not: it stays blocked until the file changes again.
  - **Effect:** one `403` on one item (it can be item-specific) stops every upload of the account
    silently, with no `LastError`. `docs/design/writes.md:568` promises that `LastError` says to
    sign in again and that a new sign-in releases the rows. Likelihood: low to moderate.
  - **A fix must:** decide whether one `403` should stop the whole account at all, given
    `OutboxHost::may_write` already gates on the granted scope; if the latch stays, wire
    `signed_in` from the account state and carry `needs_sign_in` and `last_error` through the
    host's status; release `forbidden` rows when a worker starts after a sign-in.
  - **Corrections:** a `401` does not latch: `send_write` drops the token and retries once
    (`write.rs:182–186`), and a second `401` is `WriteError::Failed`. `Outcome::SignedOut` comes
    only from the token source, where the account signs out and the worker is dropped.
- **Fixed 2026-10-03** in `a4ce8ee` (#136): the latch is gone for a `403`, which blocks only its
  own row; a worker that begins, and may send, releases the `forbidden` rows; the dead facade
  methods and `online` are gone. `LastError` still says nothing of a `403`:
  `docs/design/writes.md` §6.2 was changed to say so. What the user sees beyond this finding
  (a request, an event and a notification for each row when OneDrive refuses everything; the
  release at every worker start) the user accepted on 2026-10-03; `docs/limitations/F197.md`.

## UP2. Row string columns are overloaded as control state — **defect?**

- **Where:** `row.reason`: `hash-mismatch:<item id>` (`content.rs:41, 209, 852`),
  `too-big:<needs>:<free>` (`space.rs:54–67`), `gone-once` (`move_out.rs:399`). `row.snapshot`:
  `<size> <mtime_ns>` parsed at `steps.rs:745` and `space.rs:138–146`, or a marker
  (`move_out.rs:87–90`). `row.target_name`: a name, or an absolute path (`move_out.rs:183–194`).
- **What:** **defect?** after `finish` stores `hash-mismatch:<id>`, a failure in `clear_bad_item`
  (`content.rs:210–213`) makes `settle` overwrite the reason with `network`. The id is lost; the
  next create gets `409`, and the user's file becomes a conflict copy against the worker's own
  bad upload. `content.rs:856` drops the delete error without logging it.
- **Fix:** typed accessors in `konedrive-tree` (`TR4`, `X1`). **Size:** M. **Risk:** touches the
  store's encoding.
- **Verified 2026-10-03: confirmed, by a test.**
  `upload::tests::candidates::a_bad_upload_whose_delete_fails_twice_is_still_deleted_before_the_file_goes_again`
  (`upload/tests/candidates.rs`, branch `verify-upload`, ignored): the file ends as
  `a-<machine>.txt` with a conflict event, and the bad item stays in OneDrive as `a.txt`.
  - **Effect:** a conflict the user did not cause; no content of theirs is lost, but the name
    holds content they never wrote, and the next cycle places it in the folder. Likelihood: very
    low (OneDrive must answer an upload with another hash, the delete must fail, and the next
    run must be disturbed).
  - **The second disturbance is wider than stated:** anything that settles the row before
    `clear_bad_item` succeeds overwrites the reason: the waits at the start of `content::run`
    (`content.rs:50, 55, 62, 98`), a read failure in `hash()` (`:215`), `Throttled` and
    `SignedOut` (`engine/drain.rs:242, 250`), an examination's merge. A file that is open for
    writing at the next try is enough. The failing delete at `content.rs:219–221` does it too.
  - **A fix must:** keep the bad item's id outside `reason`, so that it survives every settle and
    the examination's merge (`konedrive-tree/src/outbox/record.rs`); clear it exactly when the
    item is deleted, found gone or adopted; decide what `copy()` and `upload_as_new()` do with it.
- **Fixed 2026-10-03** in `1bb7df2` (#137), the lost id: the bad item is kept beside the row
  (`BadItem`, three columns of the outbox), survives every settle and the merge, and is deleted
  only while its content tag is still the upload's. The rest of this finding (strings as control
  state) is done in `695fe1a` (#150): `too-big` and `gone-once` are variants of `Reason`, and the
  snapshot and the target name have typed accessors; the fields themselves are still strings
  the store's code sets (`docs/limitations/D34.md`). What is left is in `docs/limitations/F200.md`.

## UP3. `kept_back::known_group` does not know the reasons the worker writes — **defect?**

- **Where:** `kept_back.rs:64–81` against `mod.rs:148–158`.
- **What:** missing constants `PAUSED`, `SESSION_OPEN`, `NAME_HELD`; missing literals `"no-name"`
  (`steps.rs:50`), `"no-item"`, `"no-guard"` (`steps.rs:454, 459, 600`; `content.rs:357, 361`),
  `"no-handle"`, `"bad-handle"`, `"another-item"` (`move_out.rs:386, 424, 433`), and the sentences
  at `steps.rs:659`, `engine/drain.rs:212`, `content.rs:56, 171, 678`; suffixed forms never match
  (`move_out.rs:227, 256, 264, 425`). All fall to `Group::Waiting` plus a warning.
- **Fix:** the `Reason` enum of `X1`. **Size:** S for the table. D20 covers four keys only.
- **Verified 2026-10-03: confirmed, by two tests** (`upload/kept_back/tests.rs`, branch
  `verify-upload`, ignored).
  - `a_blocked_row_is_never_shown_as_going_up_by_itself` is the real harm: rows in state
    `blocked` with `another-item`, `bad-handle`, `no-guard`, `no-handle`, `no-item`, `no-name`, an
    unreadable state, or the bare `blocked` of `kept_back.rs:110` are grouped as waiting, which
    the window and `konedrivectl` word as "these go up by themselves". `BlockedCount` still counts
    them, so the two disagree. These reasons are rare.
  - `every_reason_the_worker_writes_is_in_the_table`: `paused`, `upload-session-open`,
    `name-held-by-an-upload`, the sentences and the suffixed forms are not in the table. For
    these `Waiting` is the right group; the cost is a wrong warning and summary lines keyed by a
    suffixed string, one per errno. These reasons are ordinary.
  - **A fix must:** make the group follow the row's state as well as its reason; give suffixed
    reasons a key, as `refused: …` and `too-big:…` have; add a sentence for each new key in
    `app/uploadreasons.cpp` and `konedrivectl/src/text/uploads.rs`.
- **Fixed 2026-10-03** in `1bb7df2` (#137): a blocked row is listed per file, never as waiting;
  every reason the worker writes has a key and a sentence in both clients.

## UP4. `drain` and `settle` are the hardest functions to change safely

- **Where:** `engine/drain.rs:24–161`, `:186–277`; `engine/outcome.rs:32`.
- **What:** `Outcome::Again { state, reason, next_try, backoff, detail }` allows combinations that
  mean nothing; `settle` decodes them by convention (`drain.rs:203–214`). `Outcome::NoSpace` in
  `settle` (`:271`) is unreachable.
- **Fix:** `take_row`, `wait_for_event`, one `settle_*` per outcome; `Again` replaced by variants.
- **Size:** M. **Risk:** the scheduling order is subtle.

## UP5. `content.rs` models control flow through error channels and duplicates the upload loop

- **Where:** `content.rs:606–622`, `:630`, `:758`, `:789–797`; `send_small` (`:546–600`) and
  `send_large` (`:728–835`).
- **Fix:** one `send_session` with a small step enum. **Size:** M. **Risk:** medium; this is the
  replay-critical code, well covered by `tests/sessions.rs`.

## UP6. Blocking filesystem calls on runtime threads, some under the tree lock

- **Where:** `steps.rs:272–281, 355, 375–381, 565–573, 115`; `content.rs:58–64`;
  `move_out.rs:153, 190, 437`. A `blocking()` helper exists (`steps.rs:43`).
- **Size:** S to M. Part of `X2`.
- **Fixed 2026-10-04** in `e9b716a` (#155): every listed place is a blocking section, and the worker's stop
  waits for the sections under way. Left on runtime threads, outside the task's files:
  `upload/engine/drain.rs` (`Disk::open`, `local::size_at`), `upload/space.rs` `release_fitting`,
  `handles_current_async` (`docs/limitations/F233.md`).

## UP7. Two `strip` functions with different crash guarantees

- **Where:** `move_out/walk.rs:43–51` (the id first, fsync, then the rest) and
  `local.rs:265–268` (listing order, then fsync), used at `steps.rs:279–281, 378–380`.
- **What:** the weaker one can leave an id with no state after a crash, "the one combination the
  helper refuses" (`local.rs:241–243`).
- **Fix:** one id-first strip in `konedrive-fs`. **Size:** S.

## UP8. The move-out Trash case is written twice

- **Where:** `move_out/cases.rs:219–249` and `move_out/tidy.rs:171–199`; `cases.rs:158–175` and
  `tidy.rs:140–166`; the ids inside a folder at `cases.rs:256–261`, `tidy.rs:134–136`,
  `move_out.rs:286–291`; the `open_by_handle` errno ladder four times.
- **Fix:** a `MoveOut` context struct like `content::Job`; a shared `tidy_dirs`; a `Reach` enum.
- **Size:** M. **Risk:** medium to high: the delete-in-OneDrive path.

## UP9. `steps.rs` is two modules; the 409 and guard ladders are copied

- **Where:** the five-arm `match taken(…)` at `steps.rs:420–428, 482–491`, `content.rs:237–245,
  369–375`; the guard expression eight times, five of them `unwrap_or_default()` that would send
  an empty `If-Match` (`steps.rs:652, 734`; `content.rs:218, 330, 853`).
- **Fix:** `steps/shared.rs` and `steps/meta.rs`; `Guard::of`; `OutboxRow::reset_for_resend()`.
- **Size:** S to M. **Risk:** low.

## UP10. `Engine` is one type over four files, with `cfg` open to all

- **Where:** `engine.rs:78–107, 148, 355–368, 411–430`; `engine/drain.rs:17`; `space.rs:148`;
  `move_out.rs:203`. `cfg` is read directly 85 times.
- **What:** `last_error` is one string with four writers, cleared by a prefix test, so the
  throttle text stays after the throttle ends. `gate_open` logs and mutates but reads as a
  predicate. `publish` computes the status outside the watch's lock.
- **Fix:** sub-states with their own types; accessors instead of `cfg`. **Size:** M.

## UP11. Test machinery in production code

- `Engine::fault` (`engine.rs:213–221`) is always compiled and locks a mutex at 17 call sites;
  `detach` (`mod.rs:266–276`) builds a runtime "on a plain thread (tests)"; `fake.rs` (865 lines)
  is a general OneDrive fake used by `remote/` and `local/` tests, living in `upload/`.

## UP12. A panic on a convention; errors swallowed or mislabelled

- `move_out.rs:205` `expect("move-out rows run only with MoveOuts")`. Swallowed errors:
  `move_out.rs:283, 289, 301`, `space.rs:117`, `engine.rs:546`, `move_out/tidy.rs:135`.
  `answer_row` wraps a bad Graph answer as `Fail::Io` (`steps.rs:133`), stored as `local-error`.
  Stale comments: `space.rs:203`, `content.rs:49`, `mod.rs:61–68`.

## UP13. A crash between a conflict copy's rename and its record leaves the copy never uploaded — **defect?**

- **Where:** `upload/steps.rs` `copy`: the rename and the strip of the conflict copy, then
  `outbox_copied` (as of `e9b716a`: the rename, the strip and the record are one blocking section). The same shape in
  `upload_as_new`: the strip, then `outbox_orphan`.
- **What:** if the daemon dies after the rename and before the store has the record, an `update`
  row (an edit against an edit) is left `running` at the old name while the file is at the copy
  name with no attributes and no conflict recorded. At the next start the scan records a
  follow-up row with no item id; the replay's `locate` finds nothing at the row's place and
  `content::removed` drops the row ("the file was removed here"); the follow-up ends
  `blocked(NoItem)`, and again after every examination. The user's content stays on disk under
  the copy name, is never uploaded, and no conflict is said. Nothing is deleted in OneDrive by
  this path. For `create`, `mkdir` and `move` rows the replay converges (a second copy name; the
  first rename has no conflict record).
- **Found 2026-10-04, by reading, in the review of `B3c`** (#155), where a stop could have
  produced the same state; there the record is being moved into the same blocking section as the
  rename, which narrows the crash window to the store's own write and does not close it.
  **Status: open; by reading, not reproduced.** Not traced: what the read-write reconcile does
  with the item meanwhile; what a later delete of the copy by the user does (the base still
  records that inode as the item's object, so a `delete` of the item could be recorded).
- **Fix:** make the replay of a `running` row recognise its own conflict copy (the row's inode at
  another name with no attributes), or record the intent before the rename. **Size:** S to M.

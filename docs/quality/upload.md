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

## UP3. `kept_back::known_group` does not know the reasons the worker writes — **defect?**

- **Where:** `kept_back.rs:64–81` against `mod.rs:148–158`.
- **What:** missing constants `PAUSED`, `SESSION_OPEN`, `NAME_HELD`; missing literals `"no-name"`
  (`steps.rs:50`), `"no-item"`, `"no-guard"` (`steps.rs:454, 459, 600`; `content.rs:357, 361`),
  `"no-handle"`, `"bad-handle"`, `"another-item"` (`move_out.rs:386, 424, 433`), and the sentences
  at `steps.rs:659`, `engine/drain.rs:212`, `content.rs:56, 171, 678`; suffixed forms never match
  (`move_out.rs:227, 256, 264, 425`). All fall to `Group::Waiting` plus a warning.
- **Fix:** the `Reason` enum of `X1`. **Size:** S for the table. D20 covers four keys only.

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

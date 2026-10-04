# Code quality: `hydration/`, `folder/`, `helper/` (the daemon's side)

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `hydration/source/fill.rs`, `hydration/pin.rs`, `helper/mod.rs` 2.5;
`hydration/server.rs`, `hydration/source.rs`, `hydration/source/parts.rs`, `folder/root.rs`,
`folder/disk.rs` 3; `hydration/dehydrate.rs`, `hydration/recovery.rs` 3.5; `folder/locks.rs`,
`helper/linked.rs`, `helper/status.rs` 4; `hydration/graph_source.rs`, `hydration/tracked.rs` 4.5.

**First in this area:** `HY1`; then `HY2`, then `HY3`; then `HY6` with `HY5`. `HY8` and `HY7` are
hour-sized.

## HY1. The clearance rule is split between caller and callee — **defect?**

- **Where:** `hydration/source/fill.rs:196–249, 103–137`; `sync/hydrate.rs:100–116, 201–233`;
  `helper/linked.rs:22`.
- **What:** `fill_file` takes `clearance: Option<&Clearance>` and clears only when the state is
  not `OnlineOnly` and a clearance was given (`fill.rs:237–238`). `None` means three things: the
  caller knows it is online-only, tests, and no link at all. A file found `hydrating` or
  `dehydrating` with `None` is filled without clearing, and `roll_back` (`fill.rs:351`) punches
  on the strength of a doc comment (`:338–341`). The state is classified twice.
  `read_state(&file).ok().flatten()` (`:230`) turns an unreadable state into `None`; if clearance
  then fails, `put_back(None)` removes the state attribute (`:291–295`).
- **Fix:** `enum Clearing { KnownUnmarked, By(Clearance) }` built in one place; `fill_file` alone
  decides from the state it reads. **Size:** S to M. **Risk:** the zeros path.
- **Verified 2026-10-03: refuted as something the daemon does today; confirmed as a hole in
  `fill_file` that no caller reaches.**
  - **Every production caller passes `None` only for a file it read `online-only` itself**, under
    the per-inode lock it holds until the fill ends: `hydration/server.rs:250` through
    `answer_request` (always `Some(&link)`); `sync/hydrate.rs:100–115` (`None` only for
    `Fill::Needed { may_be_marked: false }`); `upload/move_out.rs:241–252` (`hydrating` without a
    link backs off, `dehydrating` waits). Existing tests show the feared case handled:
    `hydration::server::tests::a_file_left_dehydrating_is_refilled_only_after_its_ignore_mark_is_cleared`,
    `…::a_refill_whose_ignore_mark_cannot_be_cleared_touches_nothing`,
    `sync::tests::hydrate::hydrate_now_clears_the_ignore_mark_before_refilling_a_file_that_may_carry_one`.
  - **The unreadable-state claim is unreachable too:** each caller refuses before any fill.
  - **What is real:** `fill_file` does both things when called that way. Two tests in
    `hydration/source/fill/tests.rs` (branch `verify-hydration`, ignored) show it:
    `a_file_found_dehydrating_is_not_emptied_by_a_fill_given_no_clearance` and
    `a_refused_fill_does_not_take_off_a_state_it_could_not_read`. They assert the contract the
    fix proposes; if that fix is not wanted they are dropped, not kept ignored.
  - **The risk that remains:** the state is classified by caller and callee, so a new caller that
    passes `None` wrongly gets no refusal. The severity is "latent", not "the zeros path".
- **Fixed 2026-10-03** in `6d9cdb6` (#141), the latent hole: `fill_file` decides from the state it
  reads; a file that is not `online-only` is cleared first, and with no clearance it is refused
  before it is touched (`NotCleared::NoWay`).

## HY2. "Turn a file back into a placeholder" is written four times

- **Where:** `fill.rs:351–399`, `:262–288`; `hydration/dehydrate.rs:247–263`;
  `hydration/recovery.rs:680–686`. Checkpoint usability at `fill.rs:484–489` and
  `recovery.rs:630–634`. The mtime set at `fill.rs:808` and `dehydrate.rs:99`.
- **Fix:** one module with one function taking "keep a prefix or nothing" and "lease held or
  not". **Size:** M. **Risk:** medium: the order is the safety argument.
- **Fixed 2026-10-04** in `cf2c622` (#181): `hydration/demote.rs` is the one way back to a placeholder:
  punch, size, times, `fsync`, then `online-only`.

## HY3. The download guards are duplicated and have started to drift

- **Where:** `fill.rs:536–712` and `hydration/source/parts.rs:256–320, 324–339, 432–488`.
- **What:** wrong-offset refusal three times; size-0 refusal twice; checkpoint acceptance twice,
  and only the parts copy checks `progress.bytes <= fetched.size` (`parts.rs:292`); the break
  counter four times; start-over once as a macro (`fill.rs:555–566`), once as an enum.
- **Fix:** shared `check_answer`, `Breaks`, `Resume::accept`. **Size:** M to extract, L to unify.
- **Fixed 2026-10-04** in `cf2c622` (#181): `source/guards.rs` holds the checks both downloads share; the
  read-and-write loop is still written twice (`F270`).

## HY4. Blocking file I/O on tokio workers in the fill

- **Where:** `fill.rs:222–236, 665, 673, 724–763`; `parts.rs:471, 495–507` (an fsync under
  `Mutex<State>`); `hydration/server.rs:199, 294`. Up to 64 fills (`server.rs:19`).
- **Size:** M. Part of `X2`. Not found in the limitations log.
- **Fixed 2026-10-04** in `a9779f8` (#149): each run of a fill's file calls is one blocking section
  (`hydration/source/target.rs`), single stream and in parts, and the checkpoint of a fill in
  parts is not under the state's mutex. The two `fstat`s and the `readlink` in `server.rs` stay
  on the runtime thread. A section that has begun runs to its end; what that changes when a fill
  is dropped is in `docs/limitations/F230.md`.

## HY5. `server.rs::serve`: one closure, an undeliverable errno, stale docs — **defect?**

- **Where:** `hydration/server.rs:138–289`.
- **What:** it answers `libc::ENOENT` (`:257`), which is not in `ACCEPTED_DENY_ERRNOS`
  (`konedrive-proto/src/lib.rs:74–82`); the helper clamps it to `EIO`
  (`konedrive-helper/src/events.rs:802`), so the comment at `:248–249` is false. One exit never
  calls `hydrate_done` (`:202–205`). After a caught panic the file stays `hydrating`
  (`:265–271`). Stale docs at `:50, 53–78, 62, 155, 277`.
- **Fix:** `async fn answer_one(...) -> (DenyErrno, Option<Event>)` with a `DenyErrno` newtype;
  `struct Filler`. **Size:** S to M. **Risk:** low.
- **Verified 2026-10-03: confirmed for the errno, by a test; the missing `hydrate_done` refuted.**
  `hydration::server::tests::a_fill_stopped_by_a_removal_answers_an_errno_the_kernel_delivers`
  (`hydration/server/tests.rs`, branch `verify-hydration`, ignored): the opener of a removed file
  is answered errno 2, which the helper clamps to `EIO`.
  - **Effect:** an application opening a file OneDrive removed during its download gets
    "Input/output error" instead of "No such file"; no hang; one helper warning per event.
  - **A fix must know:** the kernel delivers no errno that means "gone", so the comment at
    `server.rs:247–249` cannot be made true; pick an accepted errno and correct the comment.
  - **The exit at `:202–205` is not a defect:** it is taken only when the link is closed, and the
    helper's disconnect guard has already answered every opener of that connection.
  - **After a caught panic the file stays `hydrating`:** true by trace (`fill.rs:235`, a panic in
    `fetch` at `:569`, caught at `server.rs:250`). It heals at the next open or at startup
    recovery; not zeros.
- **Fixed 2026-10-03** in `6d9cdb6` (#141), the errno: a fill stopped by a removal answers `EIO`,
  and the comment says what is true. The stale module docs and the file left `hydrating` after a
  caught panic are as they were.

## HY6. `helper/mod.rs` holds five things; its reader drops messages silently

- **Where:** `helper/mod.rs:162–508, 519, 629, 680–720, 721`; `connect_with_timeout` (`:216–390`,
  175 lines).
- **What:** dropped without a log line: an `Ack` with nobody waiting (`:321–323`), a
  `HydrateRequest` without a descriptor (`:333–338`), the receive error (`:339`). `LinkCell` is a
  bare `Arc<Mutex<Option<HelperLink>>>` used from three areas.
- **Fix:** `link.rs`, `clearance.rs`, `presence.rs`; `LinkCell` as a small type; log every drop.
- **Size:** M. **Risk:** low.
- **Fixed 2026-10-04** in `e4e2717` (#183): `helper/` is `link.rs`, `clearance.rs`, `presence.rs`; `LinkCell`
  is a type; every message the reader passes over is logged.

## HY7. `pin.rs` is four modules in one, with a slip in the worker — **defect?**

- **Where:** `hydration/pin.rs:36–101, 103–193, 195–243, 262–670`.
- **What:** **defect?** a download cancelled by a Forget becomes `Filled::Done` (`:642`), and
  `permit.succeeded()` is called (`:644–646`). 14 `lock().unwrap()`; one panic under a lock
  leaves `working == true` and no pinned download starts again. `refusal()` (`:99`) is a sentence
  the CLI parses back.
- **Fix:** `pin/{marks,walk,order,queue}.rs`; `Filled::Cancelled`. **Size:** M. `pin/tests.rs` is
  170 lines.
- **Verified 2026-10-03: confirmed, by a test; the impact is low.**
  `hydration::pin::tests::a_download_cancelled_by_a_forget_is_not_a_success_for_the_pool`
  (`hydration/pin/tests.rs`, branch `verify-hydration`, ignored): the pool grows by a slot.
  - **Effect:** none directly; the account's pool is one slot larger than its transfers earned,
    up to the ceiling, so a throttle may come slightly sooner. On every Forget during pinned
    downloads with other work queued.
  - **A fix must:** still send a cancelled download through `finished`; and note that
    `Filled::Done` also covers "found downloaded already", "no longer pinned" and "no
    registration" (`sync/pins.rs:195–202, 209`), which are counted as pool successes too with no
    transfer made: a `Filled::Cancelled` alone does not cover those.
- **Fixed 2026-10-03** in `6d9cdb6` (#141), the slip in the worker: `Filled::Skipped` for a
  download that was cancelled or transferred nothing; only `Filled::Done` is a pool success.

## HY8. `dehydrate.rs`: an error path skips the roll-back — **defect?**

- **Where:** `hydration/dehydrate.rs:220`.
- **What:** `WriteLease::take(file).map_err(io_error)?` returns on `Err` and leaves the file
  `dehydrating` with full content and its mark cleared; the doc at `:132–139` argues this must
  be rolled back. Not zeros: the file is downloaded again or punched at the next start. The
  module doc (`:1–32`) is stale; `recovery.rs` has none.
- **Fix:** roll back on the `Err` arm too. **Size:** S. **Risk:** low.
- **Verified 2026-10-03: confirmed, by a test.**
  `hydration::dehydrate::tests::a_lease_that_cannot_be_asked_for_rolls_the_state_back_like_a_refused_one`
  (`hydration/dehydrate/tests.rs`, branch `verify-hydration`, ignored): the file is left
  `dehydrating` with all its content.
  - **What starts it:** `F_SETLEASE` failing with anything but `EAGAIN`
    (`konedrive-fs/src/lease.rs:125–133`): leases switched off after registration, a file owned by
    another uid, `ENOLCK`, `ENOMEM`. A filesystem without leases is refused at registration.
  - **Effect:** "Free up space" reports an I/O error. The next open downloads the whole file
    again; if that download fails (offline), the roll-back punches a complete local copy and the
    open gets `EIO`; the next startup recovery punches it. Never zeros. Likelihood: very low.
  - **A fix must:** roll back on the `Err` arm as on `None`. The same gap exists if the
    `spawn_blocking` at `:386–388` fails to join.
- **Fixed 2026-10-03** in `6d9cdb6` (#141), the error path: a lease that cannot be asked for rolls
  the state back to `hydrated`. What can still leave a file `dehydrating` is in
  `docs/limitations/F203.md`.

## HY9. `disk.rs`: a process-global lock kept by convention; walks that disagree on errors

- **Where:** `folder/disk.rs:64–87, 301–309, 472, 499–502, 527–538, 449–460, 580–600`.
- **What:** `DIR_MODES` is one static for every account's folder, re-entrant by a thread-local
  counter, held for whole-tree walks. `release(.., inside=false)` strips the state off a managed
  placeholder, which would leave a file of zeros in the rescue directory; only a caller's check
  prevents it (`remote/materialize/holding.rs:107–112`).
- **Fix:** a lock per `Disk`; one error policy per walk; `release` refuses a placeholder at any
  depth. **Size:** M. **Risk:** medium; `disk/tests.rs` is 170 lines.
- **Fixed 2026-10-04** in `e4e2717` (#183): a lock per sync folder on its directories' modes (`F271`); one
  error rule per walk; a directory that cannot be listed fails the scan and is named (`F272`).

## HY10. `root.rs`: error types that do not say what happened

- **Where:** `folder/root.rs:478, 544, 613` (`DehydrateError` as the error of every open);
  `:247, 361, 377, 410` (`RegisterError::Unsupported(String)` as a catch-all); `:228` (erases
  `HelperError::Timeout`); `:273–298` (`drive_allows`, a predicate that removes an attribute).
- **Fix:** `OpenError` and `DehydrateError`; `RegisterError::Helper(HelperError)`. **Size:** S to
  M. D-Bus error names must stay.
- **Fixed 2026-10-04** in `e4e2717` (#183): `OpenError` apart from `DehydrateError`; `RegisterError::Helper`
  keeps the `HelperError`; `drive_of` and `forget_drive`.

## HY11. Cut artifacts and test code in `source.rs` and `fill.rs`

- `source.rs:16–21` re-exports nine items; `LocalDir` is documented as a test source and is the
  production source of `PopulateFromDirectory`, carrying fault knobs; 13 `cfg(test)` sites in
  `fill.rs`, with thread-local hooks that fire only on a single-threaded runtime
  (`:407–428, 757–806`); a stale `#[allow(dead_code)]` (`:476`).
- **Fixed 2026-10-04** in `cf2c622` (#181): the fault knobs are `hydration::testing::Faulty`, the hooks a
  `Tuning` value; one test hook is left (`D49`).


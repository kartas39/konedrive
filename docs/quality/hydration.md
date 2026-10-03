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

## HY2. "Turn a file back into a placeholder" is written four times

- **Where:** `fill.rs:351–399`, `:262–288`; `hydration/dehydrate.rs:247–263`;
  `hydration/recovery.rs:680–686`. Checkpoint usability at `fill.rs:484–489` and
  `recovery.rs:630–634`. The mtime set at `fill.rs:808` and `dehydrate.rs:99`.
- **Fix:** one module with one function taking "keep a prefix or nothing" and "lease held or
  not". **Size:** M. **Risk:** medium: the order is the safety argument.

## HY3. The download guards are duplicated and have started to drift

- **Where:** `fill.rs:536–712` and `hydration/source/parts.rs:256–320, 324–339, 432–488`.
- **What:** wrong-offset refusal three times; size-0 refusal twice; checkpoint acceptance twice,
  and only the parts copy checks `progress.bytes <= fetched.size` (`parts.rs:292`); the break
  counter four times; start-over once as a macro (`fill.rs:555–566`), once as an enum.
- **Fix:** shared `check_answer`, `Breaks`, `Resume::accept`. **Size:** M to extract, L to unify.

## HY4. Blocking file I/O on tokio workers in the fill

- **Where:** `fill.rs:222–236, 665, 673, 724–763`; `parts.rs:471, 495–507` (an fsync under
  `Mutex<State>`); `hydration/server.rs:199, 294`. Up to 64 fills (`server.rs:19`).
- **Size:** M. Part of `X2`. Not found in the limitations log.

## HY5. `server.rs::serve`: one closure, an undeliverable errno, stale docs — **defect?**

- **Where:** `hydration/server.rs:138–289`.
- **What:** it answers `libc::ENOENT` (`:257`), which is not in `ACCEPTED_DENY_ERRNOS`
  (`konedrive-proto/src/lib.rs:74–82`); the helper clamps it to `EIO`
  (`konedrive-helper/src/events.rs:802`), so the comment at `:248–249` is false. One exit never
  calls `hydrate_done` (`:202–205`). After a caught panic the file stays `hydrating`
  (`:265–271`). Stale docs at `:50, 53–78, 62, 155, 277`.
- **Fix:** `async fn answer_one(...) -> (DenyErrno, Option<Event>)` with a `DenyErrno` newtype;
  `struct Filler`. **Size:** S to M. **Risk:** low.

## HY6. `helper/mod.rs` holds five things; its reader drops messages silently

- **Where:** `helper/mod.rs:162–508, 519, 629, 680–720, 721`; `connect_with_timeout` (`:216–390`,
  175 lines).
- **What:** dropped without a log line: an `Ack` with nobody waiting (`:321–323`), a
  `HydrateRequest` without a descriptor (`:333–338`), the receive error (`:339`). `LinkCell` is a
  bare `Arc<Mutex<Option<HelperLink>>>` used from three areas.
- **Fix:** `link.rs`, `clearance.rs`, `presence.rs`; `LinkCell` as a small type; log every drop.
- **Size:** M. **Risk:** low.

## HY7. `pin.rs` is four modules in one, with a slip in the worker — **defect?**

- **Where:** `hydration/pin.rs:36–101, 103–193, 195–243, 262–670`.
- **What:** **defect?** a download cancelled by a Forget becomes `Filled::Done` (`:642`), and
  `permit.succeeded()` is called (`:644–646`). 14 `lock().unwrap()`; one panic under a lock
  leaves `working == true` and no pinned download starts again. `refusal()` (`:99`) is a sentence
  the CLI parses back.
- **Fix:** `pin/{marks,walk,order,queue}.rs`; `Filled::Cancelled`. **Size:** M. `pin/tests.rs` is
  170 lines.

## HY8. `dehydrate.rs`: an error path skips the roll-back — **defect?**

- **Where:** `hydration/dehydrate.rs:220`.
- **What:** `WriteLease::take(file).map_err(io_error)?` returns on `Err` and leaves the file
  `dehydrating` with full content and its mark cleared; the doc at `:132–139` argues this must
  be rolled back. Not zeros: the file is downloaded again or punched at the next start. The
  module doc (`:1–32`) is stale; `recovery.rs` has none.
- **Fix:** roll back on the `Err` arm too. **Size:** S. **Risk:** low.

## HY9. `disk.rs`: a process-global lock kept by convention; walks that disagree on errors

- **Where:** `folder/disk.rs:64–87, 301–309, 472, 499–502, 527–538, 449–460, 580–600`.
- **What:** `DIR_MODES` is one static for every account's folder, re-entrant by a thread-local
  counter, held for whole-tree walks. `release(.., inside=false)` strips the state off a managed
  placeholder, which would leave a file of zeros in the rescue directory; only a caller's check
  prevents it (`remote/materialize/holding.rs:107–112`).
- **Fix:** a lock per `Disk`; one error policy per walk; `release` refuses a placeholder at any
  depth. **Size:** M. **Risk:** medium; `disk/tests.rs` is 170 lines.

## HY10. `root.rs`: error types that do not say what happened

- **Where:** `folder/root.rs:478, 544, 613` (`DehydrateError` as the error of every open);
  `:247, 361, 377, 410` (`RegisterError::Unsupported(String)` as a catch-all); `:228` (erases
  `HelperError::Timeout`); `:273–298` (`drive_allows`, a predicate that removes an attribute).
- **Fix:** `OpenError` and `DehydrateError`; `RegisterError::Helper(HelperError)`. **Size:** S to
  M. D-Bus error names must stay.

## HY11. Cut artifacts and test code in `source.rs` and `fill.rs`

- `source.rs:16–21` re-exports nine items; `LocalDir` is documented as a test source and is the
  production source of `PopulateFromDirectory`, carrying fault knobs; 13 `cfg(test)` sites in
  `fill.rs`, with thread-local hooks that fire only on a single-threaded runtime
  (`:407–428, 757–806`); a stale `#[allow(dead_code)]` (`:476`).

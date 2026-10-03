# Code quality: Across areas

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

## X1. Reasons and states are strings shared by convention

- **Where:**
  - Outbox row reasons: literals and constants in `upload/` (`UP2`, `UP3`), parsed again in
    `konedrive-tree/src/outbox/pick.rs:518–520` and `konedrivectl/src/text/uploads.rs:41–73`.
  - Local skip reasons: `local/examine/classify.rs:65, 97, 296, 337, 342`, `local/entry.rs:34–37`,
    `local/examine.rs:62–75`; matched in `upload/kept_back.rs:75` and
    `konedrivectl/src/text/uploads.rs:43–53`; `"reserved-name"` is also `SkipReason::ReservedName`
    in `konedrive-tree/src/lib.rs:153`.
  - D-Bus refusal names: 21 names as literals, 51 uses in `konedrivectl/src/text/refusals.rs`,
    again in `dolphin/src/refusaltext.cpp` and `app/synccontroller.cpp:404`. `konedrive-dbus`
    exports only `ERROR_PREFIX`.
  - Notes in `LastError` and `outbox_note` owned by prefix matching: `account/mod.rs:123–127`,
    `sync/write_mode.rs:334`, `sync/resume.rs:280`.
  - Data inside English sentences that the CLI parses back: `hydration/pin.rs:100`,
    `sync/mod.rs:147` (see `CL3`).
- **What is wrong:** a new reason can be forgotten in one of the places and nothing fails to
  compile; a reworded message silently changes what the user is told.
- **Fix:** one `Reason` enum for outbox rows and one `LocalSkip` enum, next to the outbox types in
  `konedrive-tree`, each with `key()`, `detail()` and the group; a `Refusal` enum with
  `from_error` in `konedrive-dbus`; typed slots in the snapshots for the mode, gate and switch
  notes. The spellings stored in the database and sent over D-Bus stay identical.
- **Size:** M for each enum, L for carrying structured data on the wire. **Risk:** low to medium;
  the C++ tables must follow.
- **Recorded in part:** D20, F50, A22, W12.

## X2. Blocking file I/O on async threads

- **Where:** the fill (`HY4`), the replacement (`RE5`), the upload steps (`UP6`), the hub
  (`SY8`), the configuration read on the write path (`CF1`, `AC6`).
- **What is wrong:** an fsync on a slow disk stalls runtime workers, some of it under the tree
  lock that a sync cycle holds. Other code in the same areas uses `spawn_blocking` and says why.
- **Fix:** run each of these as one `spawn_blocking` section; keep sections under the tree lock
  to store calls and one blocking hop. Or record it in the limitations log.
- **Size:** M in all. **Risk:** low to medium; lock guards must move into the blocking section,
  and a dropped fill must still stop only where it stops today.

## X3. One small thing written several times across areas

- `proc_path`: `local/entry.rs:89`, `folder/root.rs:75`, `folder/disk.rs:93`,
  `upload/local.rs:69`, `upload/move_out/place.rs:20`; ad-hoc copies at
  `hydration/server.rs:294`, `hydration/source.rs:197`, `helper/mod.rs:528`.
- `unix_now`: `conditions/running.rs:241`, `local/scan.rs:18`, inline at
  `local/watcher/service.rs:59`, `status/activity.rs:135`, `account/mod.rs:617` (`u64`),
  `account/quota.rs:103` (`i64`).
- `beneath` and the `RESOLVE_BENEATH` `OpenHow`: `local/watcher/reader.rs:163`,
  `folder/disk.rs:89`, `folder/root.rs:485, 510, 569`.
- `daemon_owned` and `gone`: `local/examine.rs:476–482` and `local/watcher/reader.rs:155–161`.
- A file walk: `status/activity.rs` `walk_files` and `hydration/pin.rs:118–150`.
- `errno_of`: `konedrive-helper/src/registration.rs:128` and `by_handle.rs:167`.
- **Fix:** one of each, in the lowest layer that uses it (`folder/disk.rs`, `konedrive-fs`).
  **Size:** S each. **Risk:** low.

## X4. Poisoned locks handled three ways

- `lock().unwrap()` about 110 times in `sync/`, `daemon/`, `dbus/`; 14 in `hydration/pin.rs`;
  10 in `account/mod.rs`; 6 in `konedrive-graph/src/token.rs`. Recovery from poison in
  `status/activity.rs:279, 432, 643`, `konedrive-graph/src/pool.rs:268`, `config/mod.rs:688`,
  the helper's `lock()`.
- **Fix:** one `lock()` helper per crate and one policy. **Size:** S. **Risk:** none.

## X5. Test hooks and test doubles in production code

- See `UP11`, `LO11`, `HY11`, `TR10`, `SY7`, `AC8`, `RE12`, `HE12`. Rule 2 of `CONTRIBUTING.md`
  ("no source file holds tests") is kept in the letter and not in the spirit.
- **Fix:** test-support modules behind `#[cfg(test)]` or the crates' `testing` features; hooks
  injected through configuration instead of globals.

## X6. Comments damaged or made stale by the move

- Sentences cut where a finding id was stripped, doc comments attached to the wrong item, links
  to items that moved, history narration. Listed per area: `RE12`, `LO12`, `HY5`, `HY8`,
  `SY12`, `HE12`, `AC8`, `UP12`.
- **Size:** S. **Risk:** none.

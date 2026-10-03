# Code quality: `konedrivectl` and `konedrive-dbus`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `konedrivectl/src/lib.rs`, `commands/sync.rs`, `text/refusals.rs` 2; `daemon.rs`,
`commands/account.rs`, `commands/login.rs`, `text/status.rs`, `text/files.rs`, `text/uploads.rs`,
`konedrive-dbus/src/lib.rs`, `konedrive-dbus/src/testing.rs` 3; `choice.rs`,
`text/transfers.rs`, `konedrive-dbus/src/version.rs` 5; the rest 4.

**First here:** `CL2` with `CL3` as far as the enum, and `CL5`; then `CL6`.

A correction to an earlier measurement: there is no 350-line `commit` in
`konedrive-dbus/src/accounts.rs`. It is a one-line property declaration; the file is declarative.

## CL2. `refusal_text_as` gives the wrong sentence for pairs nobody wrote — **defect?**

- **Where:** `konedrivectl/src/text/refusals.rs:246–485`; wildcard arms at `:277`, `:366`,
  `:406`; a comment at `:359–360` records that this already happened once.
- **Fix:** parse the name once into a `Refusal` enum; dispatch by action group, each an
  exhaustive match; every unwritten pair falls to a generic sentence. Fold the four copies of the
  detail extraction (`:125–129, 171–175, 494–498, 556–560`).
- **Size:** M. **Risk:** low; wording is pinned by tests.

## CL3. Refusal data travels inside English sentences

- **Where:** `text/files.rs:103–109, 123`; `text/refusals.rs:407, 467`; `text/uploads.rs:61–73`.
- **Fix:** the `Refusal` enum of `X1` in `konedrive-dbus`; later, the refused path and the
  pinning folder as structured data. **Size:** S for the enum, L for the wire.

## CL5. The label rule is stated in five places; four contradict the code — **defect?**

- **Where:** the code allows `@` (`konedrived/src/config/mod.rs:445–446`). Still saying "no @":
  `konedrivectl/src/cli.rs:80`, `text/refusals.rs:585–586`, `choice.rs:17`,
  `dbus/org.konedrive.Accounts.xml:17`; `app/accountsmodel.h:150` relies on the old rule. Also
  stale: `cli.rs:13–16`.
- **Fix:** one sentence, owned by the daemon's refusal message. **Size:** S.

## CL6. `folder_command` and `sync` split one enum across two functions

- **Where:** `commands/sync.rs:76–157, 161–395`; `unreachable!` at `:385–392`; five path commands
  repeat one sequence (`:80–138`); sentences inline and repeated (`:59, 180, 191, 289, 351, 358`).
- **Fix:** split `SyncCmd` into `PathCmd` and `FolderCmd`; one `path_command` helper; sentences
  into `text/`. **Size:** M. **Risk:** low.

## CL8. The library's boundary is accidental; "text" functions that do I/O

- `lib.rs:15–16` and `text/mod.rs:9–15` glob-re-export about 70 names; `lib.rs` holds a secret
  file writer, two polling loops and `is_gone`. `text/status.rs:16–46, 79–163`: about 30 awaited
  property reads inside a text function; `text/transfers.rs:43–68` shows the right shape.
  `daemon.rs`: `shown()` returns `(Vec, bool, bool)`.
- **Fix:** named modules; read into a struct, then render purely. **Size:** S to M.

## DB1. `konedrive-dbus`

- `lib.rs` exports `pub mod testing` unconditionally (`:19`) and holds user-facing prose
  (`helper_advice`); row types in `accounts.rs` are anonymous tuples up to eight wide.

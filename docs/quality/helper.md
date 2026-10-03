# Code quality: `konedrive-helper`, `konedrive-fs`, `konedrive-proto`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `events.rs`, `shared.rs`, `connection.rs`, `registration.rs`, `roots.rs`,
`konedrive-proto/src/lib.rs`, `konedrive-fs/src/placeholder.rs` 3; `by_handle.rs`,
`konedrive-fs/src/lib.rs` 5; the rest 4.

**First in these crates:** `HE1` with `HE2`; then `HE3` with `HE4`; then `HE5` with `HE6`.

## HE1. No bound per uid on suspended openers — **defect?**

- **Where:** `konedrive-helper/src/jobs.rs:211, 238`.
- **What:** every suspended open holds an event fd in the helper; one uid whose daemon never
  answers can hold them without limit, up to the process's `LimitNOFILE`. Every other user's
  opens are then denied (`EPERM` or `EIO`, never zeros). `SECURITY.md:92` says per-uid bounds
  exist.
- **Fix:** count waiters per uid in `Jobs`; past a cap, `EAGAIN`. **Size:** S. **Risk:** a cap
  too low refuses a legitimate burst (the 3,000-open burst is the reference).

## HE2. Roots unbounded per uid; `root_id` not validated — **defect?**

- **Where:** `connection.rs:285`; `registration.rs:122–124, 159, 223–225, 231–344, 318, 327`;
  `roots.rs:52, 118, 190`; `main.rs:126–134`.
- **What:** `root_id` is any string up to the 64 KiB datagram, stored in `roots.json`.
  `roots.save()` (two fsyncs) runs under the mutex workers take. Startup overlap checking is
  O(n²). Re-registering an id onto another directory drops the old entry without unmarking its
  tree. Walk failures are logged unthrottled.
- **Fix:** validate and cap at entry; save outside the lock; unwalk a displaced root.
- **Size:** S to M. **Risk:** check the daemon's id format first.

## HE3. "Every open is answered exactly once" is held by convention

- **Where:** `events.rs:225, 290–292, 313, 361, 391, 409, 576`; `pool.rs:74–90`;
  `jobs.rs:104–109`; `connection.rs:156–160`.
- **What:** an `OwnedFd` dropped unanswered leaves its opener suspended until the helper exits.
  Five `expect`s in production.
- **Fix:** a `PendingOpen` type whose `Drop` denies `EIO`, with consuming `allow` and `deny`.
- **Size:** M. **Risk:** touches every answer path.

## HE4. Every stat and xattr read duplicates the event fd

- **Where:** `events.rs:862–875`, forced by `konedrive-fs/src/placeholder.rs:131, 163, 177`
  taking `&File`.
- **What:** two to four `dup` and `close` pairs per intercepted open; the cause of F7.
- **Fix:** the readers take `impl AsFd`. **Size:** S. **Risk:** low.

## HE5. `events.rs` mixes three responsibilities

- **Where:** `event_loop` (`:146–277`), `handle_open` (`:312–439`), `:488`, `:523–530`.
- **Fix:** three files (the loop, the decision, hydration); the decision as a pure function to a
  `Decision` enum. **Size:** M. **Risk:** the most critical path; after `HE3` and `HE4`.

## HE6. `Shared` is one bag of ten fields

- **Where:** `shared.rs:284–312`; `degraded_roots` is written in six places and never read
  (F10); `ConnectionSlot` and `WaiterSlot` (`:378–447`) are one guard twice.
- **Fix:** split by subject; methods instead of exposed mutexes. **Size:** M. **Risk:** low.

## HE7. The helper's write probe is privileged code the shipped unit always refuses

- **Where:** `registration.rs:261–298, 404–433`. The daemon runs the same probe unprivileged
  (`konedrived/src/folder/root.rs:173`).
- **Fix:** remove it from the helper; keep `check_filesystem_type`. **Size:** S. **Risk:** VM
  scenarios that expect the helper's refusal would change.

## HE8. Only workers and connection readers contain a panic

- **Where:** `outbox.rs:296–301`; `connection.rs:24`; `main.rs:48`; `events.rs:154, 225–274`.
- **What:** a dead accept thread leaves a helper that never takes another daemon; a panic in the
  event loop is a Z1 event.
- **Fix:** a `Drop` guard in the writer; `catch_unwind` per batch with the remaining events
  denied; a restart loop around accept. **Size:** S.

## PR1. The protocol: an advisory version, unbounded fields, one long unsafe function

- **Where:** `konedrive-proto/src/lib.rs:13, 193–295`; `konedrive-helper/src/connection.rs:199–202,
  283–284`.
- **What:** the helper answers a wrong `Hello` with `Ack{EPROTO}` and keeps serving; `Hello` is
  optional. `recvmsg` with flags 0 (no `MSG_CMSG_CLOEXEC`; `MSG_TRUNC` unchecked). A `cmsghdr`
  reference into a `Vec<u8>` (`:196, 242`), sound only by the allocator's alignment.
  `docs/design/hydration.md:599` calls the messages binary; they are JSON.
- **Fix:** `recv` split into a raw receive and a parse; the two flags; `ToHelper::validate()`;
  close on a version mismatch. **Size:** S to M.

## HE10. Error types are inconsistent

- Bare `i32` with 0 for success, `Result<(), i32>`, `Result<(), String>`, `anyhow`. `apply`'s
  catch-all `EPERM` (`connection.rs:325`); `MarkDir` and `MarkFile` do not check the object's
  type (`:287–295`). **Fix:** an `Errno` newtype.

## FS1. `konedrive-fs/src/placeholder.rs` — **defect?** in part

- **defect?** `set_mtime` (`:229–231`) rejects a time before 1970. `with_owner_write`
  (`:102–112`) is a chmod window with no exclusion. `create_placeholder` (`:195`) and
  `create_placeholder_with` (`:392`) duplicate each other.

## HE12. Comments and dead code

- 23 comment fragments broken where finding ids were stripped (`events.rs:281, 697`,
  `connection.rs:107, 248`, `jobs.rs:4`, `shared.rs:75`, `main.rs:152`, `outbox.rs:62`); stale
  references (`marks.rs:59, 560`, `events.rs:861`). Test-only in production: `jobs.rs:249`,
  `outbox.rs:353`. Unreachable: `konedrive-fs/src/lease.rs:66–69`.

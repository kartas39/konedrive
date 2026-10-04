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
- **Verified 2026-10-03: confirmed, by a test.**
  `jobs::tests::one_uid_cannot_take_every_descriptor_the_helper_has`
  (`konedrive-helper/src/jobs/tests.rs`, branch `verify-helper`, ignored): one uid enrolls 65,536
  opens, the unit's `LimitNOFILE`, and nobody is refused. `jobs.rs:231` is a third place that keeps
  an fd. Nothing upstream bounds it: a worker enrolls and returns, and the liveness rule ends only
  a daemon whose send is blocked, not one that reads its requests and never answers.
  - **Effect:** every other user's opens in sync folders fail with `EPERM` or `EIO`, new
    connections and registrations fail; no zeros, the helper stays up. Needs a hostile local user,
    so it matters on a multi-user machine only.
  - **A fix must:** count per uid, not per connection (a uid may hold 16); count joined waiters
    (`:212`); decide whose budget an open of another user's readable placeholder is charged to;
    keep the 3,000-open burst and the test
    `beyond_the_credit_a_new_hydration_waits_instead_of_being_refused` passing; hand a refused fd
    back to be answered, never drop it.
  - **Correction:** `SECURITY.md:92–95` promises per-uid bounds in general and lists four; the
    general promise is the one not kept.
- **Fixed 2026-10-04** in `d6bd569` (#138): each uid's waiting opens are counted over all its
  connections and jobs, and past `MAX_SUSPENDED_OPENS_PER_UID` an open is answered `EAGAIN`.
  An open is charged to the file's owner. What is still open is in `docs/limitations/F208.md`.

## HE2. Roots unbounded per uid; `root_id` not validated — **defect?**

- **Where:** `connection.rs:285`; `registration.rs:122–124, 159, 223–225, 231–344, 318, 327`;
  `roots.rs:52, 118, 190`; `main.rs:126–134`.
- **What:** `root_id` is any string up to the 64 KiB datagram, stored in `roots.json`.
  `roots.save()` (two fsyncs) runs under the mutex workers take. Startup overlap checking is
  O(n²). Re-registering an id onto another directory drops the old entry without unmarking its
  tree. Walk failures are logged unthrottled.
- **Fix:** validate and cap at entry; save outside the lock; unwalk a displaced root.
- **Size:** S to M. **Risk:** check the daemon's id format first.
- **Verified 2026-10-03: confirmed, by a traced path** (a test would need fanotify, so root, or a
  production change that splits the acceptance decision out of `register_root`). Every sub-claim
  holds: `root_id` goes from `connection.rs:285` to `register_root` unchecked and is stored twice
  in `roots.json`; nothing counts roots; the save runs under the lock (`registration.rs:302`,
  `:327`, `:154–159`); the overlap check clones every covered root per root (`main.rs:77–97`,
  `:126–130`); a re-registration onto another inode walks only the new directory (`:318–342`),
  and `walk_and_unmark` is reached only from `unregister_root`.
  - **Who can start it:** not the shipped daemon (it mints and checks a UUID v4,
    `konedrived/src/folder/root.rs:372–433`, one root per account); a hostile local client on the
    socket can. The displaced root can come from the real daemon, rarely: a folder replaced at its
    path by a copy that keeps xattrs (`cp -a`, `rsync -X`, a restore).
  - **Effect:** `roots.json` and the helper's memory grow by up to about 128 KiB per
    registration; saves slow down under the lock; at the next helper start the walk loop runs
    before the event loop, so opens hang and trees not yet covered read zeros. A displaced root's
    placeholders fail with `EIO` until the helper restarts, and read zeros after.
  - **A fix must:** validate against the daemon's format, and still let a root with an older id be
    unregistered; leave room for several accounts per user; keep the roll-back correct when the
    save moves outside the lock; unwalk a displaced root only when `(dev, ino)` differs.
  - **Corrections:** the steady contenders for the roots lock are the connection threads
    (`connection.rs:264`, `:316`), workers only in two cases (`events.rs:649–650`, `:722`). More
    unthrottled log lines print a peer-chosen id raw: `registration.rs:156, 250, 309`.
- **Fixed 2026-10-04** in `d6bd569` (#138): a root id is a version 4 UUID or is refused; 32 roots for a
  uid; the list is decided on a copy and saved outside the `roots` lock; a displaced root is
  unmarked, and an id is not moved onto a directory that overlaps its old one; the refusals a
  peer can cause are throttled and its paths and ids printed escaped and cut. The limits left
  are in `docs/limitations/F208.md`.

## HE3. "Every open is answered exactly once" is held by convention

- **Where:** `events.rs:225, 290–292, 313, 361, 391, 409, 576`; `pool.rs:74–90`;
  `jobs.rs:104–109`; `connection.rs:156–160`.
- **What:** an `OwnedFd` dropped unanswered leaves its opener suspended until the helper exits.
  Five `expect`s in production.
- **Fix:** a `PendingOpen` type whose `Drop` denies `EIO`, with consuming `allow` and `deny`.
- **Size:** M. **Risk:** touches every answer path.
- **Fixed 2026-10-04** in `b0ee394` (#147): a suspended open is a `PendingOpen` (`pending.rs`) that owns
  the event fd, is answered by a consuming `allow` or `deny`, and denies `EIO` when dropped
  unanswered; the five `expect`s are gone.

## HE4. Every stat and xattr read duplicates the event fd

- **Where:** `events.rs:862–875`, forced by `konedrive-fs/src/placeholder.rs:131, 163, 177`
  taking `&File`.
- **What:** two to four `dup` and `close` pairs per intercepted open; the cause of F7.
- **Fix:** the readers take `impl AsFd`. **Size:** S. **Risk:** low.
- **Fixed 2026-10-04** in `b0ee394` (#147): the readers take `&impl AsFd`, and the helper decides an open
  on the event fd itself; `docs/limitations/F7.md` is closed.

## HE5. `events.rs` mixes three responsibilities

- **Where:** `event_loop` (`:146–277`), `handle_open` (`:312–439`), `:488`, `:523–530`.
- **Fix:** three files (the loop, the decision, hydration); the decision as a pure function to a
  `Decision` enum. **Size:** M. **Risk:** the most critical path; after `HE3` and `HE4`.
- **Fixed 2026-10-04** in `a4391d6` (#151): `events.rs` keeps the loop; `events/decision.rs` has the
  decision as a pure function over a `Facts` trait, with tests that need no fanotify;
  `events/hydration.rs` has the hydration.

## HE6. `Shared` is one bag of ten fields

- **Where:** `shared.rs:284–312`; `degraded_roots` is written in six places and never read
  (F10); `ConnectionSlot` and `WaiterSlot` (`:378–447`) are one guard twice.
- **Fix:** split by subject; methods instead of exposed mutexes. **Size:** M. **Risk:** low.
- **Fixed 2026-10-04** in `a4391d6` (#151): `Registrations`, `Daemons`, `Hydrations`, `UidSlots` and
  `Refusals` under `shared/`, each with methods and its own lock; `degraded_roots` is gone
  (`docs/limitations/F10.md`); one guard, `UidSlot`, for what were two.

## HE7. The helper's write probe is privileged code the shipped unit always refuses

- **Where:** `registration.rs:261–298, 404–433`. The daemon runs the same probe unprivileged
  (`konedrived/src/folder/root.rs:173`).
- **Fix:** remove it from the helper; keep `check_filesystem_type`. **Size:** S. **Risk:** VM
  scenarios that expect the helper's refusal would change.
- **Fixed 2026-10-04** in `e00366c` (#154): the probe is removed from the helper; `check_filesystem_type`
  stays. Under the unit the probe's write was refused almost everywhere, not everywhere (a
  filesystem mounted after the helper started): `docs/limitations/F234.md`.

## HE8. Only workers and connection readers contain a panic

- **Where:** `outbox.rs:296–301`; `connection.rs:24`; `main.rs:48`; `events.rs:154, 225–274`.
- **What:** a dead accept thread leaves a helper that never takes another daemon; a panic in the
  event loop is a Z1 event.
- **Fix:** a `Drop` guard in the writer; `catch_unwind` per batch with the remaining events
  denied; a restart loop around accept. **Size:** S.
- **Fixed 2026-10-04** in `e00366c` (#154): a panic over a batch denies that open and the rest of the
  batch `EIO` and the loop reads on; the accept thread starts again; a writer thread ends its
  connection however it stops. A panic outside the batch still ends the process
  (`docs/limitations/F234.md`).

## PR1. The protocol: an advisory version, unbounded fields, one long unsafe function

- **Where:** `konedrive-proto/src/lib.rs:13, 193–295`; `konedrive-helper/src/connection.rs:199–202,
  283–284`.
- **What:** the helper answers a wrong `Hello` with `Ack{EPROTO}` and keeps serving; `Hello` is
  optional. `recvmsg` with flags 0 (no `MSG_CMSG_CLOEXEC`; `MSG_TRUNC` unchecked). A `cmsghdr`
  reference into a `Vec<u8>` (`:196, 242`), sound only by the allocator's alignment.
  `docs/design/hydration.md:599` calls the messages binary; they are JSON.
- **Fix:** `recv` split into a raw receive and a parse; the two flags; `ToHelper::validate()`;
  close on a version mismatch. **Size:** S to M.
- **Fixed 2026-10-04** in `e00366c` (#154): a raw receive and a parse, an aligned control buffer,
  `MSG_CMSG_CLOEXEC`, `MSG_TRUNC` checked, `ToHelper::validate()`, a `Hello` with another version
  closes the connection, the design document says JSON. `Hello` stays optional
  (`docs/limitations/F234.md`).

## HE10. Error types are inconsistent

- Bare `i32` with 0 for success, `Result<(), i32>`, `Result<(), String>`, `anyhow`. `apply`'s
  catch-all `EPERM` (`connection.rs:325`); `MarkDir` and `MarkFile` do not check the object's
  type (`:287–295`). **Fix:** an `Errno` newtype.

## FS1. `konedrive-fs/src/placeholder.rs` — **defect?** in part

- **defect?** `set_mtime` (`:229–231`) rejects a time before 1970. `with_owner_write`
  (`:102–112`) is a chmod window with no exclusion. `create_placeholder` (`:195`) and
  `create_placeholder_with` (`:392`) duplicate each other.
- **Verified 2026-10-03 (only `set_mtime`): refuted for a OneDrive item; confirmed for the
  function.**
  - **Every caller for OneDrive cuts the time to 1970 first:**
    `konedrived/src/remote/materialize/file.rs:173–175`, `hydration/graph_source.rs` `mtime_of`,
    `local/examine/found.rs:234`; the two fill-side calls treat a failure as non-fatal. Shown by
    the passing test
    `remote::materialize::tests::an_item_dated_before_1970_gets_a_placeholder_dated_1970`
    (branch `verify-hydration`). The cost is that such a file shows 1970-01-01 locally.
  - **The function does refuse:** `placeholder::tests::a_placeholder_can_carry_a_time_before_1970`
    (`konedrive-fs/src/placeholder/tests.rs`, same branch, ignored), although `futimens` accepts
    such a time. The one caller that passes an uncut time is `sync/populate.rs:261`
    (`PopulateFromDirectory`, the local test source): one source file dated before 1970 fails the
    whole populate.
  - **A fix must:** change both copies of `set_mtime` (`placeholder.rs:228`, the daemon's
    `hydration/source/fill.rs:808`); if real earlier times are wanted, drop the three clamps too.
- **Fixed 2026-10-03** in `b5cffd9` (#142), the function: `set_mtime` takes a time before 1970, and the
  daemon's copy is gone. The three clamps to 1970 for OneDrive items stay
  (`docs/limitations/F220.md`).

## HE12. Comments and dead code

- 23 comment fragments broken where finding ids were stripped (`events.rs:281, 697`,
  `connection.rs:107, 248`, `jobs.rs:4`, `shared.rs:75`, `main.rs:152`, `outbox.rs:62`); stale
  references (`marks.rs:59, 560`, `events.rs:861`). Test-only in production: `jobs.rs:249`,
  `outbox.rs:353`. Unreachable: `konedrive-fs/src/lease.rs:66–69`.

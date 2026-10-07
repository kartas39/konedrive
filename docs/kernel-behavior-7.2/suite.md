# The end-to-end suite (§11)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

## 11. The end-to-end suite: what running the whole mechanism showed

`tests/vm/run.sh scenarios` starts the shipped `konedrive-helper` binary, a harness
that plays the daemon with `konedrived`'s own `sync` module over a `LocalDir`
source, and drives every intercepted open from a **child process**. The child
process is not a detail: the helper exempts the owning daemon's pid from
interception (`docs/design/hydration.md` §5.1), and the harness *is* that daemon, so
an open from one of its own threads is allowed straight through and proves nothing.

**Figures are from Btrfs unless a row says otherwise.** The suite runs all three
filesystems; the first run of 2026-09-23 completed Btrfs and ext4 and was cut
off part-way through XFS, which had agreed with Btrfs on everything it reached.
Later runs completed all three (the watchdog now measures each
step rather than the whole run), and §11.4's burst figures are from those.

### 11.1 Two results that had no committed programme now have one

Both run before the helper starts, against a fanotify group of the suite's own,
and both passed:

- **A permission response is matched by descriptor number** (§5.1). Answering
  with a `dup()` of the event fd fails with **`ENOENT`** and the opener stays
  blocked; answering with the original *number* succeeds **after that number has
  been closed**. This is what makes `PendingOpen`'s ownership of the fd load-bearing
  rather than tidy: a closed number is immediately reusable, so a response
  naming a remembered number can answer somebody else's event.
- **An ignore mark without `FAN_MARK_IGNORED_SURV_MODIFY` is silently refused
  while any ordinary `O_RDWR` descriptor is open** (§2.1's decisive row). The
  check holds a writable descriptor opened by another thread — no event fd
  anywhere near the mark — adds the mark by `(dirfd, name)`, and finds nothing
  in fdinfo; adding `SURV_MODIFY` makes the same mark appear. The control
  (nothing open at all) lands, so the row is a difference and not a broken
  check.

Two more of §10's open items were re-measured in passing, through the shipped
helper rather than directly: a `SURV_MODIFY` ignore mark reports **`mflags:640`**
in fdinfo, and it is **gone after `sync` + `echo 3 > drop_caches`**, which is
what §8's "free in the steady state" argument depends on.

### 11.2 The errno sweep measures the clamp, not the kernel's raw set

Every value in `0..=133`, plus `256`, `512`, `4095`, `-1`, `i32::MAX` and
`i32::MIN`, was reported by the daemon as the result of a hydration against a
live suspended opener — 140 values, one fresh placeholder each. **Not one left
an opener unanswered**, and not one let an open through onto an unfilled file.

The values that reached the opener unchanged were exactly

```
EPERM(1), EIO(5), EAGAIN(11), EBUSY(16), ETXTBSY(26), ENOSPC(28), EDQUOT(122)
```

which is `konedrive_proto::ACCEPTED_DENY_ERRNOS` minus `0`; everything else
arrived as `EIO`.

**Read this for what it is.** `Marks::deny` clamps before it writes, so the kernel
never saw an unacceptable value: this sweep proves the property that matters end to
end — *no errno a daemon can report leaves an opener suspended* — but it does
**not** re-measure the kernel's raw accepted set. That set is still resting on the
throwaway programme of the note at the top. Re-measuring it raw needs a check that
writes `FAN_DENY | (errno << 24)` directly, deliberately bypassing the clamp, and is
a piece of work this suite has not done.

### 11.3 What the helper costs under load

| load | outcome |
| --- | --- |
| 3000 concurrent opens of distinct placeholders (before §11.4's bound) | 3000 answered, 0 read zeros, **662 `EIO`**; peak **68 threads**, peak **3984 KiB RSS**, 10.3 s |
| the same, with `MAX_OUTSTANDING_HYDRATIONS` refusing beyond it (Btrfs / ext4 / XFS) | 3000 answered, 0 read zeros, **0 `EIO`**, 307–481 filled and the rest `EAGAIN`; peak 68 threads, ~3.0–3.3 MiB RSS, 0.23–0.36 s |
| the same, with the credit-gated queue (Btrfs / ext4 / XFS) | 3000 answered, **3000 filled**, 0 read zeros, **nothing refused**; peak 69 threads, 3.6–3.8 MiB RSS, 5.6 / 4.6 / 4.6 s |
| 400 concurrent opens with `RLIMIT_NOFILE=128` | 400 answered, 0 read zeros; helper survived; peak 68 threads, 3812 KiB RSS |
| 64 opens with the daemon gone and the root still registered | 8 waited the full 30 s, 56 refused at once; peak 66 threads |

The thread count is the pool (64) plus the main thread, the accept thread, the log
flusher and a connection's reader and writer — it does **not** follow the number of
opens in flight, which is the whole point of the bounded pool, and it is now
measured rather than argued. Neither does the credit-gated queue add threads: a
hydration waiting for credit holds only its suspended openers' event descriptors.

### 11.4 Which bound actually binds — and a circular wait that looked like a slow daemon

At 3000 concurrent opens the helper's own queue was **never** full: the
`EVENT_WORKERS = 64` / `EVENT_QUEUE_DEPTH = 1024` pair was not the constraint
even once. What refused work in the first runs was the **per-connection
outbox**: about 2300 of the 3000 opens were denied `EAGAIN` because
`OUTBOX_DEPTH = 256` was full. That part is designed behaviour — `EAGAIN` means
"try that again", and a retry of a refused opener succeeded.

The part that was not: the remaining **662** opens were denied **`EIO`**. The first
diagnosis was that `SEND_TIMEOUT` (10 s) had given up on a daemon that was slow
rather than wedged, and the timeout was replaced with a 60 s window of *silence*.
Measured after that change, identically on all three filesystems:

| | with the 10 s send timeout | with the 60 s silence window alone |
| --- | --- | --- |
| openers denied `EIO` | 662 | 662 |
| run time | 10.3–10.6 s | 30.1–30.2 s |
| files filled, **instant** source | 31 | 11–37 |
| what ended the connection | the helper's writer, at 10 s | the daemon, at 30 s |

A daemon filling 4 KiB files from a local directory with no delay does not take
thirty seconds to fill twenty of them. It was not slow; it had **stopped**, and
the helper's log shows the only thing that happened afterwards was the daemon
hanging up. The mechanism is a circular wait between the two processes:

1. each of the daemon's four fill slots is held until the `Ack` for
   that fill's `HydrateDone` arrives;
2. that `Ack` travels on the same socket, in order, behind every request the
   helper had already queued;
3. the daemon's reader thread stops reading when its 64-deep request queue is
   full.

With more than about 69 requests in flight (64 queued, one in the loop's hand,
four filling), the reader stops with requests in the socket ahead of an `Ack`;
the slot waiting for that `Ack` never frees; the queue never drains; the reader
never resumes. Thirty seconds later the daemon's own call timeout ends the
connection and the helper's disconnect guard denies every enrolled opener `EIO`.
(That it is the daemon's 30 s call timeout is read off the timing: the helper's
log is silent for 29 s after the last refusal and then reports the *peer*
closing, and its own liveness window — 60 s — never fired.) `SEND_TIMEOUT` had
only ever been firing first. Nothing about it is specific to a test: by
construction, any user with about 70 concurrent opens of distinct placeholders
reaches it in production. The harness reached it a little later, because its
forwarder added a second 64-deep queue in front of `serve_hydrations` — see
the end of this section for what running without it measured.

`konedrive_proto::MAX_OUTSTANDING_HYDRATIONS` (64) now makes the depth a
contract: the helper never has more than 64 requests out on a connection, and
the daemon's request queue is exactly that deep, so every request in flight fits
and the reader always reaches the next `Ack`. At first the helper enforced it by
refusing the 65th concurrent new hydration `EAGAIN`, as a full outbox had.
Measured that way:

| filesystem | answered | `EIO` | read zeros | filled | `EAGAIN` | run time | connection lost |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Btrfs | 3000 | 0 | 0 | 307 | 2693 | 0.26 s | no |
| ext4 | 3000 | 0 | 0 | 452 | 2548 | 0.30 s | no |
| XFS | 3000 | 0 | 0 | 481 | 2519 | 0.23 s | no |

(The full run at `bbcbb10`; an earlier subset run gave 330 / 472 / 589 filled in
0.28–0.36 s.)

Every fill the daemon was asked to start, it completed; the outbox was never
full once, so `OUTBOX_DEPTH` is no longer what binds — the 64-hydration bound
is.

That cured the wedge and moved the failure onto users: nine openers in ten refused,
where a desktop thumbnailing a folder expects every file. Since then the bound is a
**credit**: a hydration beyond it is enrolled and held back in the helper, its
openers suspended like any other's, and each `HydrateDone` hands its credit to the
oldest one waiting, in the same step. The same burst, full run at `3b8ae01`:

| filesystem | answered | filled | refused | `EIO` | read zeros | run time | median / max wait | connection lost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Btrfs | 3000 | 3000 | 0 | 0 | 0 | 5.60 s | 3.86 / 5.41 s | no |
| ext4 | 3000 | 3000 | 0 | 0 | 0 | 4.63 s | 3.10 / 4.16 s | no |
| XFS | 3000 | 3000 | 0 | 0 | 0 | 4.59 s | 3.18 / 4.13 s | no |

The run is longer because it does all the work: 3000 fills at four at a time,
about 650 a second from an instant local source. Nothing refused anything — not
the worker pool, not the outbox, not the credit. Two related changes ride on the
same socket: the outbox now keeps the helper's requests (at most
the credit, plus the greeting) and the `Ack`s for the daemon's own calls in
separate compartments, so neither can crowd out the other, and an `Ack` past its
128-deep reserve makes the connection's reader wait rather than ending the
connection. A rootless peer that sends 2000 requests before reading a single
reply now gets all 2000 `Ack`s and keeps its connection, on all three
filesystems; before, the helper reset it after 667 with none delivered.

Every burst above ran through the harness's forwarding task, which had a
64-deep channel of its own in front of the daemon's request queue — roughly
twice production's buffering. The forwarder is gone:
the errno sweep that needed it now answers through a second connection of
its own uid, and the daemon reads the link's own queue directly. The same
burst at production depth, in a later full suite run:

| filesystem | answered | filled | refused | `EIO` | read zeros | run time | median / p95 / max wait |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Btrfs | 3000 | 3000 | 0 | 0 | 0 | 5.49 s | 3.75 / 5.18 / 5.20 s |
| ext4 | 3000 | 3000 | 0 | 0 | 0 | 4.70 s | 3.16 / 4.08 / 4.10 s |
| XFS | 3000 | 3000 | 0 | 0 | 0 | 4.67 s | 3.24 / 4.30 / 4.34 s |

The burst does not pin the depth itself: with the daemon's queue cut to 62 it
still passed on Btrfs, because the exact worst case — four fills waiting for
the `Ack`s of their `HydrateDone`s, queued behind a whole credit of new
requests — needs a timing a burst does not reliably produce. The host test
`the_reader_reaches_acks_queued_behind_every_request_the_helper_may_send`
builds it deliberately, and passes at 64 and 63 and fails at 62.

### 11.5 `st_blocks` is not zero on ext4 for a file that holds nothing

A placeholder that has been punched — by a failed hydration's roll-back or by a
completed dehydration — reports

| filesystem | `st_blocks` for a punched placeholder |
| --- | --- |
| Btrfs | **0** |
| XFS | **0** |
| ext4 | **8** (4 KiB) |

The four kilobytes are ext4's **extended-attribute block**. A placeholder
carries `user.konedrive.item-id`, `user.konedrive.state` and, while hydrated,
`user.konedrive.stamp`; they do not fit in the inode's inline space, so ext4
allocates a block for them, and `st_blocks` counts it. Btrfs and XFS keep xattrs
somewhere `st_blocks` does not see.

Anything that reads "this file occupies no space" off `st_blocks == 0` is wrong
on ext4 — including a "space saved" figure shown to a user, and including a test.
Allow one filesystem block of slack and the discrimination is still enormous: a
file that kept its content shows its whole size.

### 11.6 Behaviour nothing had measured before

- **A full disk denies `ENOSPC` and never commits.** With a 400 MiB image filled to
  within 2 MiB and an 8 MiB placeholder, the open was denied **`ENOSPC`** and the
  file was left `online-only`, at its true size, with **0 blocks** and no stamp —
  the state the commit-point ordering (`docs/design/hydration.md` §6.2) exists to
  guarantee. It filled correctly once space was freed.
- **The event fd is opened against the *opener's* mount.** With the helper
  restarted inside a private mount namespace in which the whole filesystem is a
  read-only bind mount — which is what `ProtectHome=read-only` is — a hydration
  through the event fd still worked. The helper's own view of the filesystem
  does not constrain what the daemon may write through the descriptor it is
  handed.
- **Killing the helper hands every suspended open an unfilled placeholder.** With
  200 opens suspended on a delayed source, `SIGKILL` on the helper released all 200
  within 525 ms and **every one of them read zeros** — 200 filled: 0, read-wrong:
  200. (While §11.4's bound refused beyond 64, at most 64 opens per connection were
  ever suspended and the same scenario showed 64 reading zeros and 136 refused
  `EAGAIN` up front. The credit-gated queue suspends every opener again, so it is
  back to all 200, on all three filesystems: waiting instead of failing is also more
  applications for a helper death to hand an unfilled file.) `fanotify(7)`'s *"Upon
  close(2), outstanding permission events will be set to allowed"* is now observed
  rather than quoted, and it is the whole reason the worker pool is bounded and a
  panicking worker is caught rather than allowed to unwind: **the helper exiting is
  silent data loss, and the helper denying is not.** Identical on Btrfs and ext4.
- **Stopping the helper with `SIGTERM` denies every suspended open instead.** The
  same 200 opens suspended on the same delayed source, and `SIGTERM` in place of
  `SIGKILL` (2026-10-07, Btrfs only): **all 200 openers got `EIO`** — filled: 0,
  read-wrong: 0, errno 5 × 200 — each within 529 ms of its open, as in the kill.
  The helper said it had answered 200 opens in 17.8 ms and was gone, with exit
  status 0, 19.3 ms after the signal. It reads the signal from a descriptor in its
  event loop and answers what it holds before the group closes
  (`docs/design/hydration.md` §13); what the kernel does at the close is unchanged,
  there is only nothing left for it to allow. A crash or a `SIGKILL` runs none of
  this, and the bullet above still describes them.
- **So does a stop that finds the opens parked with no daemon connected.** Eight
  opens — what one uid may park — each in a worker's hands, waiting up to 30 s for
  a daemon to connect, and `SIGTERM` 1.5 s later (2026-10-07, Btrfs only): **all
  8 openers got `EIO`** — filled: 0, read-wrong: 0, errno 5 × 8 — each 1491 ms
  after its open, not after the 30 s. Nothing is in the table of hydrations here:
  the stop wakes the workers and waits for each to write its answer. The helper
  said it had answered 8 opens in 2.1 ms and was gone, with exit status 0, 4.3 ms
  after the signal.
- **`SO_PEERCRED` on `SOCK_SEQPACKET` reports the pid the event reports.**
  Observable because the exemption is: the harness's own open of an
  `online-only` placeholder went straight through with no fetch and left the
  file `online-only`, while the same open from any other process was intercepted
  and filled.
- **The startup walk survives a tree changing under it.** 57 766 directory
  renames ran concurrently with the walk; the helper survived, never followed a
  symlink out of the root (neither an escaping one nor one pointing back inside),
  and every directory that stood still was marked.
- **`DT_UNKNOWN` is handled.** An ext4 image built with `-O ^filetype` really
  does report `DT_UNKNOWN` for every entry, and the walk still marked a
  directory three levels down — `openat2(O_DIRECTORY)` settles what `d_type`
  would not.
- **A real `EMFILE` does not end the helper.** With `RLIMIT_NOFILE=128` and 400
  concurrent opens, every opener was answered and the helper stayed up. 38 of
  them came back **`EPERM`**, which is the kernel denying the events it could not
  copy a descriptor out for — so the openers caught in that window are answered
  rather than left hanging, exactly as the design assumed. With the credit-gated
  queue (which keeps every enrolled opener's descriptor instead of refusing past
  64) the same run gives about 152 filled, 93 `EPERM` and 155 `EIO` on each
  filesystem, and no `EAGAIN`. Read off the helper's log on Btrfs, the 153
  `EIO`s there were: 141 opens the helper could not `dup` to inspect, 7 requests
  whose descriptor for the daemon could not be duplicated, and **5 files that
  had been filled** but whose state could not be re-read for want of a
  descriptor — denied rather than allowed unverified. None read zeros.
- **A cross-device subtree is counted, not silently skipped.** With a tmpfs
  mounted inside the sync root and one interrupted file under it, `recover`
  reported `skipped > 0` and left the file untouched. The host test for this
  skips itself whenever unprivileged user namespaces are unavailable; here it
  needs no namespace at all.
- **No live watcher covers a new directory.** A directory created behind the
  daemon's back (`mkdir` by anybody else) is **not** covered until the daemon
  marks it or the helper restarts. This is a property of the current design, not
  a kernel behaviour, and it is recorded here because the suite is where it
  became visible.
### 11.7 The realistic tree: `tests/vm/run.sh measure`

Run once, on Btrfs, at `579ae73` plus the registration fix: **10 000
directories, 100 000 placeholders** (10 per directory, 4 KiB each), built before
the helper knew anything about them, then registered through the shipped
helper's own walk. Totals are `/proc/slabinfo` deltas (`active_objs × objsize`
over every cache) after `sync` + `drop_caches`, as in §8.

| measurement | result |
| --- | --- |
| building the tree (100 000 `O_TMPFILE` + `linkat` placeholders) | 4.6 s |
| **the startup walk** — `RegisterRoot` on the existing tree, `openat2` per directory | **461 ms** for 10 000 directories (~46 µs each) |
| the same walk once it also clears each file's ignore mark by name, run again later, cold cache | **1.03 s** for 10 000 directories and 100 000 files — ~5.7 µs more per file |
| slab per directory mark, that run | 1686.5 B — unchanged within the noise: the file lookups pin nothing once `drop_caches` has run |
| marks in the group afterwards (`/proc/<helper>/fdinfo`) | 10 001 (every directory plus the root) |
| slab per directory mark, total delta | **1658 B** |
| first open of a placeholder (intercepted, 4 KiB hydrated from a local source) | 9.8 ms |
| second open of it (ignore-marked) | 6.3 ms |
| open of an ordinary file outside the root | 6.9 ms |
| ignore marks present after 2000 hydrations | 2000 |
| slab delta across those 2000 hydrations, per hydration | 3177 B |

How to read it:

- **The walk is cheap.** Half a second for ten thousand directories is what a helper
  restart costs a large sync folder before interception is complete; a second, now
  that every registration walk also takes the ignore mark off each of 100 000 files
  (`docs/design/hydration.md` §12), a lookup of each name on a cold cache.
- **The per-directory figure is higher than §8's 1148 B**, and this programme
  cannot say why: it records only the total, not §8's per-cache breakdown, and
  what it measures is the shipped walk over a tree that was just written rather
  than a loop marking idle directories. Until a per-cache run attributes the
  extra ~500 B, the design figure is the measured one — **1658 B per directory,
  ~16.6 MB for a 10 000-folder tree** (§13), not §8's ~11 MB. It does not change
  the conclusion that folders win on the count.
- **The open latencies are dominated by starting the reader process** (each is
  a fresh child, as the exemption requires — §11). What they do show is the
  ordering: an ignore-marked open costs the same as an open outside the root
  (6.3 vs 6.9 ms, inside the spawn noise), and a first open that hydrates 4 KiB
  costs about 3 ms more.
- **The 3177 B per hydration is not the cost of an ignore mark.** It is
  everything 2000 hydrations left pinned — stamps and state xattrs, the filled
  extents' metadata, the ignore marks themselves — divided by 2000, and the mark
  count was taken before `drop_caches`, not after. §8's "evictable ignore marks
  are ≈0 after reclaim" is neither confirmed nor contradicted by it. A run that
  counts the marks again after `drop_caches` and breaks the delta down by cache
  would settle it.

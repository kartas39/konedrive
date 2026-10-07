# What the kernel actually does: fanotify on Linux 7.2 (measured on 7.2.5 and 7.2.7)

Everything stated as a result below, except where §10 says otherwise, was
measured by one of the three committed programmes

- `tests/vm/poc_marks.rs` — the original proof of concept (§§1–8);
- `tests/vm/ignore_mark.rs` — the ignore-mark behaviour of §2.1, asserted
  against the code `konedrive-helper` actually ships;
- `tests/vm/scenarios/` — the end-to-end suite (§11): the shipped helper
  binary in one process, the daemon's own `sync` module in another, and every
  intercepted open driven from a child process;
- `tests/vm/watch_probe/` — the write phase's unprivileged notification
  group (§14): what it may set up, and which events each local change raises;
- `tests/vm/scenarios/open_by_handle.rs` — `open_by_handle_at` inside the helper's
  sandbox, and the helper's own opens (§15), run by `tests/vm/run.sh unit`;

which run as root inside a virtme-ng VM booted on the host kernel
(`tests/vm/run.sh`), against three loop-mounted filesystems created fresh for
the run: Btrfs at `/mnt/btrfs`, ext4 at `/mnt/ext4`, XFS at `/mnt/xfs`.
Where a statement is an inference rather than a measurement it says so.

**§10 is the honest list** — what was never tested, and which results were
measured by a programme that no longer exists and so cannot be reproduced with
one command today. Read it before relying on anything here.

**§13 is the memory verdict** the design asked for: what a mark costs, what a drive
of a given size costs, and whether marking directories holds up. §12 is about
leases, which dehydration (`docs/design/hydration.md` §8) depends on and which none
of the programmes above touch.

§5.1 and the "ordinary `O_RDWR` descriptor" row of §2.1 used to have no committed
programme: they came from throwaway code written during development, whose results
were recorded but whose code was not kept. `tests/vm/scenarios/kernel_facts.rs` now re-measures
both directly, before it starts the helper, against a fanotify group of its own —
see §11.1. The accepted-errno set of §5 is a different case and is still not
re-measured raw; §11.2 says exactly what the suite establishes instead.

- Kernel: `7.2.5-200.fc44.x86_64` (Fedora 44), the host's own kernel. SELinux is
  in the picture (see §8); a kernel without an LSM will show slightly smaller
  per-inode figures.
- Group: `FAN_CLASS_PRE_CONTENT | FAN_CLOEXEC | FAN_UNLIMITED_QUEUE |
  FAN_UNLIMITED_MARKS | FAN_NONBLOCK`, event fds `O_RDWR | O_LARGEFILE | O_CLOEXEC`
  — and, in the current helper, `O_NONBLOCK` as well (§12.4). Every result
  measured before §12.4 was measured without it.
- Directory marks: `FAN_MARK_ADD` with `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`, no `FAN_ONDIR`.
- Ignore marks on files: `FAN_MARK_ADD | FAN_MARK_IGNORE | FAN_MARK_IGNORED_SURV_MODIFY | FAN_MARK_EVICTABLE`
  with `FAN_OPEN_PERM`. The `SURV_MODIFY` flag is not optional — §2.1 is about why
  the combination without it silently creates no mark at all — and it has a
  consequence of its own, in §2.2.

Each filesystem is checked with `statfs` before anything else runs: `run.sh`
mounts a tmpfs over `/mnt`, so a mount that silently failed would otherwise leave
a writable directory behind and every result below would be a result about
tmpfs. The run fails loudly instead.

Reproduce with:

```
cargo build --release --manifest-path tests/vm/Cargo.toml
tests/vm/run.sh tests/vm/target/release/poc-marks
tests/vm/run.sh tests/vm/target/release/poc-marks --measure 10000
tests/vm/run.sh tests/vm/target/release/vm-ignore-mark
tests/vm/run.sh quick        # the suite on ext4 only: the normal run
tests/vm/run.sh full         # btrfs, ext4 and xfs in three VMs at once: the full, slower run
tests/vm/run.sh scenarios    # all three in one VM, in sequence
tests/vm/run.sh measure
cargo build --release --manifest-path tests/vm/Cargo.toml --bin watch-probe
tests/vm/run.sh tests/vm/target/release/watch-probe --fs btrfs   # §14; also --fs ext4
tests/vm/run.sh unit                                              # §15, under the helper's own unit
```

The last four build the helper and the suite themselves and hand the suite the
helper's path, because half of what it asserts is about the helper dying,
restarting, or running out of descriptors. Every step of the suite prints how
long it took, and a run ends with its ten slowest steps.

## Where each section is

The section numbers are the ones the rest of the repository cites (*kernel* §n); they did not
change when the document was divided by topic.

- [`interception.md`](interception.md) — marks, ignore marks and answers to opens (§§1–7):
  - §1 A directory mark intercepts opens of the files inside it
  - §2 An ignore mark on a file suppresses the event its parent would raise — THE GATE
    - §2.1 The gate does not build itself: an ignore mark the kernel silently refuses
    - §2.2 What that second effect costs: `ClearIgnore` becomes safety-critical
  - §3 Files created after the mark are covered
  - §4 Opening the directory itself is never intercepted
  - §5 A denial carries our errno
  - §5.1 A response is matched by file descriptor *number*
  - §6 Writing through the event fd is silent
  - §7 Traps for the helper: our own opens are events too
- [`memory.md`](memory.md) — what a mark costs, and the memory verdict (§8, §13):
  - §8 What a mark costs
  - §13 The memory verdict
- [`vm-tests.md`](vm-tests.md) — running privileged tests (§9):
  - §9 Running privileged tests: what it took
- [`not-covered.md`](not-covered.md) — what this does not cover (§10):
  - §10 What this does not cover
- [`suite.md`](suite.md) — the end-to-end suite (§11):
  - §11 The end-to-end suite: what running the whole mechanism showed
    - §11.1 Two results that had no committed programme now have one
    - §11.2 The errno sweep measures the clamp, not the kernel's raw set
    - §11.3 What the helper costs under load
    - §11.4 Which bound actually binds — and a circular wait that looked like a slow daemon
    - §11.5 `st_blocks` is not zero on ext4 for a file that holds nothing
    - §11.6 Behaviour nothing had measured before
    - §11.7 The realistic tree: `tests/vm/run.sh measure`
- [`leases.md`](leases.md) — leases (§12):
  - §12 Leases: what a refused `F_SETLEASE` means
    - §12.1 A mapping counts as open, even after its descriptor is closed
    - §12.2 Starting a process does not refuse a lease
    - §12.3 A broken lease kills a holder that has not handled `SIGIO`
    - §12.4 A lease in a marked directory stops the listener's `read()` — unless the event descriptors are `O_NONBLOCK`
    - §12.5 An opener suspended in a permission wait already refuses a write lease
    - §12.6 An event whose descriptor cannot be opened `O_RDWR` at all
- [`notification.md`](notification.md) — notification events for the write phase (§14):
  - §14 Notification events for the write phase
    - §14.1 What an unprivileged process may set up
    - §14.2 The limits: queue, groups, marks
    - §14.3 Each operation and the events it raises
    - §14.4 The pid rule, and a pre-content group on the same directories
    - §14.5 What writes through a pre-content event descriptor raise
    - §14.6 From an event to a path, without privilege
    - §14.7 What §14 does not cover
- [`open-by-handle.md`](open-by-handle.md) — `open_by_handle_at` from the helper's sandbox (§15):
  - §15 `open_by_handle_at` from the helper's sandbox, and the helper's own opens
    - §15.1 What §15 does not cover

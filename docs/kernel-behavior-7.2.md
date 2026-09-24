# What the kernel actually does: fanotify on Linux 7.2 (measured on 7.2.5 and 7.2.7)

Everything stated as a result below, except where §10 says otherwise, was
measured by one of the three committed programmes

- `tests/vm/poc_marks.rs` — the original proof of concept (§§1–8);
- `tests/vm/ignore_mark.rs` — the ignore-mark behaviour of §2.1, asserted
  against the code `konedrive-helper` actually ships;
- `tests/vm/scenarios.rs` — the end-to-end suite (§11): the shipped helper
  binary in one process, the daemon's own `sync` module in another, and every
  intercepted open driven from a child process;
- `tests/vm/watch_probe.rs` — the write phase's unprivileged notification
  group (§14): what it may set up, and which events each local change raises;

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
were recorded but whose code was not kept. `tests/vm/scenarios.rs` now re-measures
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
tests/vm/run.sh quick        # the suite on btrfs only: the normal run
tests/vm/run.sh full         # btrfs, ext4 and xfs in three VMs at once: the full, slower run
tests/vm/run.sh scenarios    # all three in one VM, in sequence
tests/vm/run.sh measure
cargo build --release --manifest-path tests/vm/Cargo.toml --bin watch-probe
tests/vm/run.sh tests/vm/target/release/watch-probe --fs btrfs   # §14; also --fs ext4
```

The last four build the helper and the suite themselves and hand the suite the
helper's path, because half of what it asserts is about the helper dying,
restarting, or running out of descriptors. Every step of the suite prints how
long it took, and a run ends with its ten slowest steps.

## 1. A directory mark intercepts opens of the files inside it

Marking a directory with `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD` produces a
permission event when a file in that directory is opened **by a path that goes
through that directory**. The event carries `FAN_OPEN_PERM` and an fd for the
file; after `FAN_ALLOW` the opener gets the content. Identical on Btrfs, ext4 and
XFS.

This is the load-bearing behaviour: one mark per folder covers the files in it.

### Where that stops — do not generalise this

A parent's mark is consulted during path resolution through that parent. It is
not a property of the file's inode. So it does **not** follow that every open of
a managed file reaches the listener. Not demonstrated here, and expected to
bypass the parent's mark:

- an open through a **hardlink** to the file that lives in an unmarked directory;
- an open through a **second mount** of the same filesystem, or through a bind
  mount whose path does not traverse the marked directory;
- an open after the file has been **renamed out** of the marked tree.

These are the cases the design's invariant M4 (a managed file that has left the
root carries an individual inode mark) exists to cover.

**Measured now, by `tests/vm/scenarios.rs` (Btrfs only so far):**

| case | intercepted? |
| --- | --- |
| a hardlink to a placeholder, in a directory nobody marked | **no** — the open succeeded and read zeros |
| a placeholder renamed out of the marked tree, with no `MarkFile` | **no** — the open succeeded and read zeros |
| the same placeholder after `MarkFile`, renamed out of the tree | **yes** — intercepted and filled, one fetch |
| a **second (bind) mount** of the same filesystem | **yes** — intercepted and filled, one fetch |

The first two confirm the boundary as stated: a parent's mark is consulted
during resolution *through that parent*, so a name that does not go through it
escapes, and the file reads as zeros. That is the failure this project exists to
prevent, and `MarkFile` is what prevents it — the third row is that working.

The fourth row is the one that was not obvious. A second mount shares the
superblock, so it shares the *inodes*, and the mark is on the parent directory's
inode; resolution through the other mount still passes through the same marked
inode. A mark is therefore not per-mount. A bind mount whose path does **not**
traverse the marked directory (a bind of a subdirectory straight onto somewhere
else) is a different case and is still not measured.

## 2. An ignore mark on a file suppresses the event its parent would raise — THE GATE

With the parent directory marked and `FAN_MARK_ADD | FAN_MARK_IGNORE |
FAN_MARK_EVICTABLE` (mask `FAN_OPEN_PERM`) placed on one file inside it, opening
that file produces **no event at all** and does not block. The `fanotify_mark`
call itself returns success on all three filesystems.

The strategy therefore holds: hydrated files can be made invisible to the helper
without removing the directory's mark, so the steady-state cost is one mark per
folder, not one per file.

### 2.1 The gate does not build itself: an ignore mark the kernel silently refuses

§2 above places the ignore mark on an idle file, and that works. The helper does
not have an idle file. It places the mark on a file somebody is in the middle of
opening, through the `O_RDWR` descriptor the kernel handed it with the permission
event, while the daemon holds an `SCM_RIGHTS` copy of that same open file
description. In that situation

> `fanotify_mark(FAN_MARK_ADD | FAN_MARK_IGNORE | FAN_MARK_EVICTABLE,
> FAN_OPEN_PERM, …)` **returns 0 and creates no mark at all.**

`/proc/self/fdinfo/<group>` shows no line for the inode, and the next open of the
file raises a permission event exactly as if nothing had been asked for. The
return value is 0, with `errno` untouched. This is the one call in the whole
interface whose success cannot be read from its result, so **assert on
`/proc/self/fdinfo/<group>`, never on the return value.**

**The mechanism is `inode_is_open_for_write()`, and it is not about event fds.**
`fs/notify/fanotify/fanotify_user.c`'s `fanotify_add_inode_mark()` contains

```c
/*
 * If some other task has this inode open for write we should not add
 * an ignore mask, unless that ignore mask is supposed to survive
 * modification changes anyway.
 */
if ((flags & FANOTIFY_MARK_IGNORE_BITS) &&
    !(flags & FAN_MARK_IGNORED_SURV_MODIFY) &&
    inode_is_open_for_write(inode))
        return 0;
```

Measured, on Btrfs, ext4 and XFS alike, with the ignore mark always added by
`(dirfd, name)` so that path resolution is held constant and the only variable is
what descriptor happens to be open:

| what is open on the inode while the mark is added | mark appears in fdinfo |
| --- | --- |
| nothing | **yes** |
| an ordinary `O_RDONLY` descriptor | **yes** |
| an ordinary `O_RDWR` descriptor, no fanotify event anywhere | **no** |
| the permission event's `O_RDWR` fd | **no** |
| only the daemon's `SCM_RIGHTS`/`dup` copy, after the helper answered and closed its own | **no** |

The third row is the decisive one: no event fd is involved in it at all. The
kernel is not treating event descriptors specially; it is refusing to add an
ignore mask to an inode that anyone has open for writing.

**Adding `FAN_MARK_IGNORED_SURV_MODIFY` removes the refusal**, and every row
above becomes "yes" — including the last, which matters most: answering the event
and closing the helper's own descriptor first is *not* sufficient on its own,
because the daemon still holds its copy when it reports the hydration done.
Marking through the event fd itself, before the response is even written, works
once the flag is set, which is what the helper now does — the event fd is the
exact inode the opener will get, so there is no path to resolve and nothing to
re-validate.

The flag has a second effect that is wanted for its own sake. Measured: with a
plain ignore mask, writing a single byte to the file sets its `ignored_mask` back
to `0` and the next open raises an event again; with `SURV_MODIFY` the mask
survives and the open stays suppressed. Without it, every save of a hydrated file
would send its next open back through the helper.

### 2.2 What that second effect costs: `ClearIgnore` becomes safety-critical

The behaviour just described is also a safety net being removed, and it is worth
stating on its own because the loss is invisible at the point where it bites.

Before `SURV_MODIFY`, *any* modification cleared the ignored mask. A dehydration
that punched a file's blocks without first removing its ignore mark therefore
repaired itself: the punch cleared the mask, the next open was intercepted, and
the file re-hydrated. Nobody designed that, but it was doing real work.

With `SURV_MODIFY` it is gone. A file that is dehydrated while still carrying an
ignore mark is **empty and invisible at the same time**: every subsequent open is
suppressed, so the helper never sees it, never hydrates it, and the application
reads zeros. Nothing detects this and nothing recovers from it — the mark only
goes away when the kernel evicts the inode.

So the dehydration's ordering (`docs/design/hydration.md` §8: clear the ignore mark,
take the write lease, punch) is no longer about saving a round trip; it is the only
thing between a dehydration and silent data loss:

> **Never punch a hole in a file whose `ClearIgnore` did not succeed.**

`ENOENT` from the removal is *not* a failure for this purpose — an evictable mark
is designed to vanish, and "there is no mark" is the state the caller wanted. Any
other errno must abort the dehydration.

The same hazard from outside our own code is in §10: a third-party tool that
re-sparsifies a managed file leaves the mask in place. That one cannot be
prevented from here; this one can.

`FAN_MARK_EVICTABLE` and `FAN_MARK_IGNORED_SURV_MODIFY` do not conflict, and
`SURV_MODIFY` does **not** make the mark permanent: after `sync` +
`echo 3 > /proc/sys/vm/drop_caches` the mark is gone from fdinfo and the next
open raises an event again, exactly as in the section below. The memory
argument in §8 is unaffected.

Removal is unaffected too: a plain `FAN_MARK_REMOVE | FAN_MARK_IGNORE` clears an
ignored mask that was added with `SURV_MODIFY` (passing `SURV_MODIFY` on the
removal as well behaves identically), and the very next open is intercepted
again. On a file that carries no mark, removal returns **`ENOENT`** — which is a
routine outcome, not a failure, because an evictable mark is designed to vanish.

In fdinfo, a `SURV_MODIFY` mark is distinguishable: `mflags:640` rather than
`mflags:600`.

**Consequence for anything built on this:** "the ignore mark is in place" is
never established by a successful `fanotify_mark`. The helper logs the syscall's
error when there is one and otherwise assumes nothing; the assertion that the
mark really exists lives in `tests/vm/ignore_mark.rs`, which reads fdinfo.

### What happens when the evictable mark is reclaimed

`FAN_MARK_EVICTABLE` means exactly what it says. After
`echo 3 > /proc/sys/vm/drop_caches`, the check asserts — not merely prints — that
the same open raises `FAN_OPEN_PERM` again: the ignore mark is gone with the
inode, while the directory's (non-evictable) mark survives. The failure direction
is the safe one: a reclaimed ignore mark means more interception, never a file
read past a missing body. The helper must be ready to see events for files it has
already hydrated, re-add the ignore mark and allow. "This file has an ignore
mark" is not a durable fact the helper may cache.

## 3. Files created after the mark are covered

A file created in a marked directory after the mark was placed is intercepted on
its first open, on all three filesystems. `FAN_EVENT_ON_CHILD` covers the
directory's contents, not a snapshot of them.

## 4. Opening the directory itself is never intercepted

Without `FAN_ONDIR`, `read_dir()` on a marked directory produces no event and
never waits for us. Listing a folder — which is what a file manager does
constantly — stays at native speed and cannot be blocked by a stuck helper.

## 5. A denial carries our errno

`FAN_DENY | (EIO << 24)` written as the response surfaces as `EIO` ("Input/output
error") to the opener, on all three filesystems. A separate control check denies
with a plain `FAN_DENY` and asserts the opener gets `EPERM` — that is the
fallback if a future kernel drops `FAN_DENY_ERRNO`, and it is measured rather
than assumed.

The nix crate's `Response` bitflags only know `FAN_ALLOW` and `FAN_DENY`, so the
value is built by hand:
`Response::from_bits_retain(Response::FAN_DENY.bits() | ((errno as u32) << 24))`.

### Only some errnos are accepted, and the rest hang the opener

*(Measured on all three filesystems by a programme that is no longer on disk — see
the note at the top. The end-to-end suite re-establishes the property that depends
on it, not the raw set (§11.2); `konedrive_proto::ACCEPTED_DENY_ERRNOS` encodes the
result and is unit-tested against it.)*

`FAN_DENY | (errno << 24)` is accepted for

```
0, EPERM, EIO, EAGAIN, EBUSY, ETXTBSY, ENOSPC, EDQUOT
```

and for those only. `ENOENT`, `EACCES`, `ECONNRESET`, `ENETDOWN`, `ETIMEDOUT`
and `ECANCELED` were each swept and each made `write()` on the group fail with
**`EINVAL`** — and a permission event whose `write()` failed has not been
answered, so **the opener stays suspended**, until the group fd closes.

This is a live hazard rather than a curiosity, because the errnos outside the set
are precisely the ones a network-backed hydrator reports: a deleted item is
`ENOENT`, a dropped connection is `ECONNRESET`, a slow server is `ETIMEDOUT`. A
helper that forwards the daemon's errno unfiltered hangs every waiting opener on
the first ordinary failure.

Writing a plain `FAN_DENY` afterwards **does** rescue such an event: the opener
gets `EPERM` and proceeds. So the safe shape is clamp first, and fall back to a
bare `FAN_DENY` if the write is refused anyway.

## 5.1 A response is matched by file descriptor *number*

*(Measured on all three filesystems by a programme that is no longer on disk — see
the note at the top; §11.1 measures it again.)*

Answering a permission event with a `dup()` of its event fd fails: `write()`
returns **`ENOENT`** and the opener stays blocked. Answering with the original fd
*number* succeeds **even after that number has been closed**.

So the kernel matches a response against the number it handed out in
`fanotify_event_metadata.fd`, not against the open file description behind it.
Two things follow, and the second is the dangerous one:

- anything that must answer an event later has to keep that exact descriptor
  alive and answer with it — a duplicate will not do;
- a closed descriptor number is immediately reusable, so a response naming a
  number that has since been recycled would be matched against whatever event now
  holds it. Answering "the fd we remembered" after closing it is not merely
  ineffective; it can answer somebody else's event.

`SCM_RIGHTS` is unaffected: what the daemon receives is a descriptor for the same
open file description whether the helper sends the event fd or a duplicate of it,
because nothing on that path is matched by number.

## 6. Writing through the event fd is silent

The event fd (opened `O_RDWR`) can be written and `fsync`ed while the opener is
still blocked, and the opener then reads the new content — which is what makes
filling a placeholder in place possible.

The check is built so that silence means something. The directory is marked with
`FAN_MODIFY` as well as `FAN_OPEN_PERM`, and a positive control writes to the
same file through an **ordinary** descriptor on the same group: that raises
`[FAN_OPEN_PERM, FAN_MODIFY]`, while the write through the event fd raises
nothing. The silence is `FMODE_NONOTIFY` on the descriptor the kernel hands out,
not a dead mask.

## 7. Traps for the helper: our own opens are events too

fanotify does not exempt the listening process. Anything the helper does to a
file inside a directory it has marked generates an event aimed at itself, and a
single-threaded helper that blocks in `open()` waits forever for an answer only
it could give. Measured, inside a directory marked `FAN_OPEN_PERM |
FAN_EVENT_ON_CHILD`:

| operation by the helper itself | events raised |
| --- | --- |
| `open()` of a file (e.g. to get an fd to mark) | 1 × `FAN_OPEN_PERM` — **deadlocks** a single-threaded helper |
| `fs::write()` creating a new file | 1 × `FAN_OPEN_PERM` |
| `O_TMPFILE` open on the directory | 1 × `FAN_OPEN_PERM` |
| `O_TMPFILE` + `linkat` together | 1 × `FAN_OPEN_PERM` (the `linkat` adds none) |
| `open()` of a directory | none |
| `fanotify_mark` by (dirfd, name) | none |

Consequences for the helper:

- **Mark files by name, never by descriptor.** `fanotify_mark` with a directory
  fd and a relative name resolves the path without opening the file, so no event
  is raised. Opening the file first deadlocks; opening it `O_PATH` to dodge the
  event does not work either — a control check asserts that `fanotify_mark`
  rejects an `O_PATH` descriptor with `EBADF` on all three filesystems.
- **Directories are safe to open.** Without `FAN_ONDIR` a directory open raises
  nothing, so holding directory fds and working relative to them is free.
- **Placeholder construction is intercepted.** Building a placeholder with
  `O_TMPFILE` inside a marked directory raises one `FAN_OPEN_PERM` for the
  helper's own open, even though the file has no name yet. The event loop must
  keep answering while any such work is in flight (the design's "never block in
  the event loop" rule is not optional; it is what stops the helper deadlocking
  against itself).

## 8. What a mark costs

**Open item: these figures are Btrfs only.** Per-mark cost on ext4 and XFS is not
yet measured; `ext4_inode_cache` is 1072 B per object against `btrfs_inode`'s
944 B, so expect a somewhat higher figure there. The conclusion — folders win on
the count, not on the unit cost — does not turn on it.

*Preliminary, and never repeated:* before the measurements on the other two
filesystems were stopped, one pair of post-reclaim runs at N = 10 000 gave ext4
1282.4 / 1284.3 B per directory mark and XFS 1200.6 / 1195.0 B, against Btrfs 1149.1
/ 1150.9 B on the same runs — ext4 **~135 B** and XFS **~45 B** per mark dearer than
Btrfs, the order the object sizes predict. File marks came out the same as directory
marks on both. The code that produced these was discarded, so they are recorded from
the notes of that run, not reproducible here. XFS frees inodes from a background
worker and needs a two-pass quiesce (`sync` + `drop_caches` twice, ~400 ms apart) or
identical runs differ by ~100 B per mark.

`--measure 10000` on Btrfs, marking 10 000 objects in one group. Figures are
`/proc/slabinfo` deltas (`active_objs × objsize`, summed over every cache) across
the marking loop, taken after `sync` + `drop_caches` so they reflect what is
**pinned** rather than what happens to be cached.

**Headline: a mark costs ~1.15 KB, and directory marks and file marks are
indistinguishable — the difference between them is smaller than the measurement
noise.**

| mark | per mark, resident | per mark, after `drop_caches` | across runs |
| --- | --- | --- | --- |
| directory, `FAN_OPEN_PERM\|FAN_EVENT_ON_CHILD` | ~1330 B | **~1148 B** | 1147.7–1149.8 (N=10 000, eight runs); 1167.0–1168.9 (N=20 000) |
| file, `FAN_OPEN_PERM` | ~1320 B | **~1140 B** | 1127.5–1146.7 (N=10 000, nine runs); 1163.9 (N=20 000) |
| file, evictable ignore mark | ~1320 B | **≈0** | fanotify structures left behind 0.2–1.9 B per mark; the raw total swings between −78 and +4.6 B per mark |

The dir-minus-file gap across runs ranges from about +1 to +41 B with no stable
sign, against a noise floor of roughly **±80 B per mark (~7%)** on the
total-slab baseline — visible directly in the ignore-mark row, where the true
answer is zero and identical runs return anything from −776 KB to +46 KB for
10 000 marks. Treat the totals as "~1.15 KB, same for both".

### Where those bytes go — the strong evidence

The per-cache deltas are far steadier than the total, and they add up:

| cache | objsize | per directory mark |
| --- | --- | --- |
| `btrfs_inode` | 944 B | 914.5 B |
| `lsm_inode_cache` | 112 B | 112.3 B |
| `fanotify_mark` | 80 B | 80.2 B |
| `fsnotify_inode_mark_connector` | 40 B | 40.1 B |
| everything else (bio, kmalloc, maple_node…) | — | ±3 B |
| **total** | | **1147.7–1149.7 B** |

- **The mark itself is 120 B**: an 80-byte `fanotify_mark` plus a 40-byte
  `fsnotify_inode_mark_connector` (one connector per inode, holding that inode's
  marks). This is the same for directory marks, file marks and ignore marks.
  Runs sometimes report 113 B instead of 120 B for the same structures — that is
  SLUB undercounting `active_objs` for objects parked in per-cpu partial slabs,
  not a real difference between mark kinds.
- **The rest is the inode the mark pins**: 944 B of `btrfs_inode` plus 112 B of
  `lsm_inode_cache` (the LSM's per-inode blob — SELinux is enabled on this host;
  expect this line to vanish on a kernel without an LSM). Neither can be
  reclaimed while a non-evictable mark holds the inode.
- The ~180 B/mark of `dentry` in the resident column is reclaimable and is gone
  after `drop_caches`, marked or not.

### What this means for the design

- A directory mark and a file mark cost the same, ~1.15 KB, because both are
  dominated by the pinned inode. The saving is entirely in the **count**: a tree
  of 200 000 files in 10 000 folders costs **~11 MB** with directory marks and
  would have cost **~230 MB** with a mark per file. That is the whole argument
  for marking folders, and it is now measured rather than estimated.
  *(Superseded as a design figure by §11.7's realistic tree, which measured
  1658 B per directory mark, and by the verdict in §13: ~16.6 MB against
  ~330 MB. The figures above stay as what this loop measured.)*
- The evictable ignore marks on hydrated files are **free in the steady state**:
  after reclaim their cost is indistinguishable from zero, because the kernel
  drops them along with the inodes. They cost ~1.15 KB each only while their
  inode is in cache anyway — memory the kernel would be using for that inode
  regardless.
- Kernel memory therefore scales with the number of *folders* and with how much
  of the tree is being touched right now, not with the number of files.

## 9. Running privileged tests: what it took

`vng` (virtme-ng 1.41) boots the host kernel with the host filesystem shared over
virtiofs. Three things were not obvious and are baked into `tests/vm/run.sh`:

- **`--memory 2G` hangs the boot.** The guest stops early (right after
  `ACPI: Core revision`) and never reaches init, at exactly 2048 MiB, under vng's
  `microvm` machine type. 1 G, 1536 M, 3 G and 4 G all boot in ~2 s. The runner
  uses 4 G.
- **`/mnt` is not writable by guest root.** The guest's root filesystem is the
  host's, exported by a virtiofsd that runs as the unprivileged host user, so
  guest root gets `EACCES` on `mkdir /mnt/btrfs`. The runner mounts a tmpfs over
  `/mnt` first, and keeps the disk images on it — a loop device cannot be backed
  by a file on the overlayfs vng mounts over `/tmp`.
- **The loop driver is not loaded.** No `/dev/loop*` exists until `modprobe loop`
  runs, and `mount -o loop` fails before it starts.
- **`/var/lib` is not writable by guest root either**, for the same virtiofs
  reason as `/mnt`, and the helper's state file lives at
  `/var/lib/konedrive/roots.json`. Without a tmpfs over `/var/lib` every
  `RegisterRoot` fails on the state-file write and no end-to-end scenario can
  start. `/run` *is* already a tmpfs, so the control socket needs nothing.
- **The guest's `RLIMIT_NOFILE` is 1024 soft / 4096 hard**, and the helper holds
  roughly 1.3 descriptors per suspended open. A burst scenario left at the
  default measures the rlimit rather than the worker pool; guest root may raise
  the hard limit, so `run.sh` does (`ulimit -n 1048576`).
- **Creating a file inside a marked directory deadlocks the creator** if the
  creator is also the only thread that could answer the event (§7). This bites
  test code, not just the helper: a check that marks a directory and *then*
  writes a file into it from its main thread hangs the whole VM run with no
  output. Create the files first, mark afterwards.
- **`--network user` needs the guest's DNS pointed at QEMU's own resolver by hand.**
  Fedora's `/etc/resolv.conf` is a symlink into `/run`, which is a fresh tmpfs on
  every guest boot, so with `vng --network user` and nothing else, `getent hosts
  graph.microsoft.com` fails outright (confirmed: exit 2, no address) — `ip addr` in
  the guest shows the `10.0.2.x` user-net interface is up and has DHCP'd an address,
  but `cat /etc/resolv.conf` is empty because nothing ever wrote the stub file it
  points at (`/run/systemd/resolve/stub-resolv.conf`). `tests/vm/run.sh` now writes
  that file itself — `nameserver 10.0.2.3`, QEMU user-mode networking's own resolver
  — as the first thing `inner.sh` does when `VM_NETWORK` is set, and only then. With
  it, `getent hosts graph.microsoft.com` resolves (three AAAA records, via
  `graph.microsoft.com`'s traffic-manager CNAME) and an anonymous `curl` to
  `https://graph.microsoft.com/v1.0/me` gets back a real `401 Unauthorized` from
  Microsoft's servers, not a connection failure — so the guest's outbound TLS path
  works end to end before any token is involved. `VM_NETWORK` is unset by default:
  every scenario except the real-account `--graph-token` mode runs with no network
  at all, on purpose.

Also: `vng` must be given `< /dev/null` when run from a non-interactive session,
and it does not propagate the environment into the guest, so the runner writes
the binary path and its arguments into the guest script. `run.sh` exits with the
guest binary's own status, or 125 if the guest never reported one.

## 10. What this does not cover

- **In §§1–8 the opener is another thread of the same process**, not a separate
  process. fanotify does not exempt the listening process (§7 is the evidence),
  so this is representative. §11's suite drives every intercepted open from a
  real child process, so the cross-process case is now covered there.
- **Only `FAN_OPEN_PERM`** was exercised, plus `FAN_MODIFY` as the control in §6.
  `FAN_ACCESS_PERM` and the pre-content events (`FAN_PRE_ACCESS`) were not.
- **§5's accepted-errno set is still not re-measured raw.** §5.1's fd matching
  and §2.1's `O_RDWR` row now have a committed programme (§11.1), as do the
  `drop_caches` and `mflags:640` observations (§11.1). The errno set does not:
  `Marks::deny` clamps before it writes, so the suite's sweep (§11.2) measures
  the clamp end to end and never hands the kernel an unacceptable value. A check
  that writes `FAN_DENY | (errno << 24)` directly, bypassing the clamp, is what
  would close this, and it has not been written. Until it is, the *set itself*
  rests on the throwaway programme of the note at the top — though the property that
  depends on it, "no daemon-reported errno leaves an opener suspended", is now
  measured.
- ~~Nothing measures what happens when the helper dies.~~ **Measured** — see
  §11.6. It is exactly as bad as `fanotify(7)` says.
- **`FAN_MARK_IGNORED_SURV_MODIFY` was not tested against an external writer.**
  §2.1 establishes that the ignore mask now survives modification, which is what
  the design wants, on the stated assumption that only konedrive's own daemon
  ever empties a managed file (and it clears the mark first). A third-party tool
  that re-sparsified a hydrated file behind the helper's back would leave the
  mask in place and the file would read as zeros. Nothing measures that, because
  nothing currently does it.
- **Queue overflow (`FAN_Q_OVERFLOW`), mark limits, and behaviour across unmount**
  were not tested at all for the helper's group. For the write phase's
  unprivileged notification group, the overflow and the mark and group limits
  are measured in §14.2.
- **Hardlinks, second mounts and renames out of the tree** are now measured on
  Btrfs (§1's table). A bind mount whose path does *not* traverse the marked
  directory is still not measured, and neither is any of it on ext4 or XFS.
- **§11's figures are Btrfs and ext4.** The suite runs all three filesystems;
  the run that produced §11 was cut off part-way through XFS, which had agreed
  with Btrfs on everything it reached.
- Measurements were taken on **Btrfs only**; the checks run on all three
  filesystems but `--measure` does not. ext4 and XFS are an open item: with
  `ext4_inode_cache` at 1072 B against `btrfs_inode`'s 944 B, the per-mark figure
  on ext4 should come out somewhat higher.
- The per-inode part of the cost depends on the host's **LSM policy**: 112 B of
  it is `lsm_inode_cache` (see §8), so a machine without SELinux will measure
  about that much less per mark.
- **§8's ext4 and XFS figures are preliminary**: one pair of runs each, made by code
  that was discarded. They are in §8 for their order of magnitude only.
- **§12's lease results come from throwaway programmes**, none of them committed:
  the `SIGIO` and `fork`/`posix_spawn` results (C, on 7.2.5), and the mapping result
  (C, on **7.2.7**; the programme is reproduced in §12.1 so it can be run again).
  All three are unprivileged and ran on the host, on tmpfs and Btrfs — not in the
  VM, and not on ext4 or XFS.

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
  been closed**. This is what makes `take_fd`'s ownership rule load-bearing
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

## 12. Leases: what a refused `F_SETLEASE` means

Dehydration (`docs/design/hydration.md` §8) empties a file only under a write lease
(`F_SETLEASE`, `F_WRLCK`), on the strength of one kernel promise: the lease is
refused while anybody else has the file. Three things about that promise were
measured on the host, none of them by a committed programme (§10 says which ran
where); §12.4 and §12.5, about how leases and the permission wait meet, are measured
by the VM suite.

### 12.1 A mapping counts as open, even after its descriptor is closed

A program that `mmap`s a file and then closes the descriptor still holds the
file: the mapping keeps a reference to the open file description
(`vma->vm_file`), and `check_conflicting_open()` compares the inode's
`i_writecount` and `i_readcount` — which each open file description raises once,
from the open until its last reference goes — against the lease-taker's own.
Measured on tmpfs and Btrfs, kernel 7.2.7 — 20 runs on each filesystem (the
cross-process row: 5 on each), identical every time:

| what else holds the file while a fresh `O_RDWR` descriptor asks for `F_WRLCK` | lease |
| --- | --- |
| nothing (control) | granted |
| a second `O_RDONLY` descriptor, held open (control) | **`EAGAIN`** |
| a `MAP_SHARED` read/write mapping from an `O_RDWR` descriptor, descriptor closed | **`EAGAIN`** |
| a `MAP_SHARED` read-only mapping from an `O_RDONLY` descriptor, descriptor closed | **`EAGAIN`** |
| a `MAP_PRIVATE` read-only mapping, descriptor closed | **`EAGAIN`** |
| each of the three, after `munmap` | granted |
| another **process** holding only a `MAP_SHARED` mapping, descriptor closed | **`EAGAIN`**; granted once it exited |

So "the file is not open anywhere" is exactly as strong as it reads, and a little
stronger: a viewer that mapped a document and closed its descriptor makes "free up
space" answer "in use" until the mapping goes. A claim that circulated during
development — that `F_SETLEASE` does *not* see a mapping whose descriptor was closed
— is false, and must not be designed around.

The programme, small enough to keep here since it is not committed anywhere:

```c
/* gcc -O2 -o lease_mmap lease_mmap.c && ./lease_mmap ./probe.bin */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static const char *path;
static int try_lease(void) {            /* 0, or the errno F_SETLEASE gave */
    int fd = open(path, O_RDWR), err = 0;
    if (fcntl(fd, F_SETLEASE, F_WRLCK) != 0) err = errno;
    else fcntl(fd, F_SETLEASE, F_UNLCK);
    close(fd);
    return err;
}
int main(int argc, char **argv) {
    path = argv[1];
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    ftruncate(fd, 1 << 20);
    close(fd);
    printf("nothing else open: %s\n", strerror(try_lease()));
    int mfd = open(path, O_RDONLY);
    void *map = mmap(NULL, 1 << 20, PROT_READ, MAP_SHARED, mfd, 0);
    close(mfd);                          /* only the mapping is left */
    printf("mapped, fd closed: %s\n", strerror(try_lease()));
    munmap(map, 1 << 20);
    printf("after munmap:      %s\n", strerror(try_lease()));
    unlink(path);
    return 0;
}
```

(`strerror(0)` prints "Success", i.e. granted.)

### 12.2 Starting a process does not refuse a lease

A `fork` duplicates the descriptor *table*, but every duplicate points at the same
open file description, raising only its reference count (`f_count`), which the lease
check never reads. Measured on 7.2.5: **0 of 2000** `F_SETLEASE` calls failed after
`posix_spawn`, **0 of 3000** after `fork` + `_exit`, **0 of 3000** after `fork` +
`exec`, against a control in which a genuine second `open()` gave `EAGAIN` at once
(which withdrew an earlier, opposite claim). A refusal therefore always means
somebody else really has the file — never "the daemon happened to start a process".

### 12.3 A broken lease kills a holder that has not handled `SIGIO`

The kernel tells the lease holder that somebody wants the file with `SIGIO`, whose
default action is to terminate. Measured: a process holding a lease with no handler
died with exit status **157** (128 + 29, `SIGIO`) the moment another process opened
the file. The daemon therefore sets `SIGIO` to be ignored, once and process-wide,
before its first lease — only if the disposition is still the default, so a process
with a handler of its own keeps it (`konedrive_fs::lease`).

### 12.4 A lease in a marked directory stops the listener's `read()` — unless the event descriptors are `O_NONBLOCK`

fanotify creates an event's descriptor inside the listener's `read()`, by opening
the file with the group's `event_f_flags`, and opening a file that somebody holds a
write lease on breaks the lease: the open waits until the holder lets go, or until
`lease-break-time` (45 s by default) runs out. Measured twice — by a standalone C
programme run as root in the VM on all three filesystems (no longer on disk), and
since by the suite's "a lease on one file does not stall the opens of others", which
takes a write lease on an `online-only` placeholder F from the exempt daemon, opens
F from reader B, and 200 ms later opens another placeholder G, in the same folder,
from reader C:

| event descriptors | the group's `read()` | B, the opener of the leased file | C, another file's opener |
| --- | --- | --- | --- |
| `O_RDWR \| O_LARGEFILE \| O_CLOEXEC` (the helper up to `28f43c0`) | blocked as long as the lease was held: 4.8 s for a 5 s lease; 3.0 s at `lease-break-time=3` with a holder that never let go (C programme) | waited, and was filled once the lease went: 2.7 s (suite) | **waited behind it: 2.5 s**, the whole time the lease was held (suite, Btrfs, ext4 and XFS) |
| the same plus `O_NONBLOCK` | does not block (C programme); returned nothing once, with the group readable (suite, the helper's log) | **denied `EPERM` by the kernel, at once: 7–8 ms** (suite) | answered at once: 10 ms (suite) |

So without `O_NONBLOCK` the helper's whole event loop — every intercepted open on
the machine — stops for as long as any process holds a lease on any file in any
marked directory. The helper's own dehydration holds exactly such a lease across its
punch and `fsync` (`docs/design/hydration.md` §8), and any local user can take one
on a file of their own and hold it for 45 s at a time.

**What the kernel does with an event whose descriptor cannot be created.**
Read in `fanotify_read()` and `copy_event_to_user()` (an inference from the
source; the outcome below is what the suite measured): without
`FAN_REPORT_FD_ERROR`, a failed descriptor creation ends that event's copy
with the error, and a permission event is finished right there with
`FAN_DENY` — the opener gets `EPERM`, and the event never reaches the access
list, so nothing is left for the listener to answer and nothing is allowed.
`read()` returns the events before it, or, when it was the first, the error
itself: `EAGAIN` for a lease, which the helper's loop treats as a drained
queue and goes back to `poll()` — whatever is still queued makes the group
readable again at once. Descriptor exhaustion takes the same path with
`EMFILE`/`ENFILE` (§11.6, the `EPERM`s there), and so does a descriptor that
cannot be opened at all, with that open's own errno (§12.6). The helper logs, throttled,
each time its first read after `poll()` finds nothing, which is a lower bound
on how many opens were refused this way.

The cost of the flag is that an open landing while a file is leased — during
a dehydration's punch, or of a file some other program (Samba, say) holds a
lease on — is refused `EPERM` at once instead of waiting for the lease to
break. `FAN_REPORT_FD_ERROR` would deliver such an event to the helper with
the error in place of the descriptor; whether the helper could then answer
it, with `EAGAIN` for instance, was not measured.

### 12.5 An opener suspended in a permission wait already refuses a write lease

A read-only open is counted against a write lease (`i_readcount`) before the
permission hook runs, not once the open has completed: measured by the suite's
kernel-fact check "a read-only opener suspended in a permission wait already
refuses a write lease", on Btrfs, ext4 and XFS. With this process's own group
marking the directory and nobody reading its events — so no event descriptor
exists — a thread's `open(O_RDONLY)` of the file is suspended, and while it is,
`F_SETLEASE` `F_WRLCK` on the file is refused `EAGAIN`; with nothing else open,
the same lease is granted (the control).

What that rules out: an opener the helper lets through onto a `hydrated` file
being overtaken by a dehydration's lease before its own `break_lease()` — after
the helper has closed its event descriptor, which it does the moment the
answer is written — and waking, once the lease goes, onto the punched file.
The opener itself refuses the lease from the moment it is suspended.

### 12.6 An event whose descriptor cannot be opened `O_RDWR` at all

§12.4's descriptor is opened against the **opener's** path — its mount and its
dentry — with the group's `event_f_flags`, `O_RDWR`. Two ordinary kinds of open make
that open fail outright, whatever `O_NONBLOCK` says. Measured by a standalone C
programme in the VM, and since by two suite scenarios — "an open through a read-only
mount is refused by the kernel, and the helper keeps running" and "a second open of
a running executable is refused by the kernel, and the helper keeps running" — on
Btrfs, ext4 and XFS, kernel `7.2.7-200.fc44`:

| the open | the group's `read()` | its opener | the helper up to `bbbdecb` | the helper since |
| --- | --- | --- | --- | --- |
| of an `online-only` placeholder through a read-only bind mount of the root | fails **`EROFS`** (C programme) | **`EPERM` from the kernel, 7–8 ms, nothing fetched** — not let through onto the placeholder | exited (`Error: EROFS`); a reader suspended on a slow hydration of another file got **65 536 zero bytes** | reads on, logs the event once; the suspended reader gets its file; the same placeholder through the ordinary path fills after one fetch |
| a second open — a read, or a second `exec` — of an executable, copied into the folder, while it runs | fails **`ETXTBSY`** (C programme:  `deny_write_access` holds `i_writecount` negative) | **`EPERM`**, 7–8 ms; a second `exec` fails `EPERM` too | exited (`Error: ETXTBSY`); the same zeros | reads on; the suspended reader gets its file |

So the kernel treats these exactly like a leased file (§12.4): the event is
finished `FAN_DENY` inside `read()`, its opener sees `EPERM`, and it is never
allowed and never left suspended. Only what `read()` returns differs — the
failed open's own errno instead of `EAGAIN` — and a listener that takes that
errno for a broken group, as the helper did, exits and hands every other
suspended open to the kernel's release-as-allowed (§11.6). The helper now ends
its loop only for `EBADF`, `EINVAL` and `EFAULT`, the errnos its group
descriptor itself can report.

Consequences, which no listener can change while its event descriptors are
`O_RDWR`:

- **Any open through a read-only mount of a file the helper intercepts is
  refused `EPERM`** — a Flatpak or bubblewrap sandbox with `home:ro`, a
  service with `ProtectHome=read-only`, a filesystem that remounted itself
  read-only after an error. That covers a placeholder, which could not be
  filled through such an open anyway (the daemon writes through that very
  descriptor), but also a hydrated file whose ignore mark is not in place and
  every file konedrive does not manage (never ignore-marked). Only a file
  whose ignore mark is in place opens.
- **While an executable in the folder runs, every open of it that reaches
  the helper is refused `EPERM`**, a second `exec` included. A managed,
  hydrated executable escapes this only while its ignore mark is in place.

`FAN_REPORT_FD_ERROR` would hand such an event to the listener with the error
in place of a descriptor; whether it could then be answered — or its opener
served through a descriptor opened some other way — was not measured.

## 13. The memory verdict

What the design asked this document to settle: what the marks
cost, what a whole drive costs, and whether marking **directories** holds up.

| mark | measured | source |
| --- | --- | --- |
| directory mark, isolated loop over idle directories, Btrfs | **~1148 B** pinned: the inode (944 B `btrfs_inode` + 112 B SELinux blob) plus the mark itself (80 B + 40 B connector) | §8 |
| directory mark, the shipped helper's walk over a realistic tree (10 000 directories, 100 000 placeholders), Btrfs | **1658 B** — ~500 B above the loop, not yet attributed | §11.7 |
| file mark (the rejected per-file strategy) | the same as a directory mark, within noise: both are the pinned inode | §8 |
| evictable ignore mark on a hydrated file | the mark's own 120 B while its inode is in cache anyway; **≈0 after reclaim**, since it goes with the inode — including with `SURV_MODIFY` | §8, §2.2, §11.1 |
| ext4 / XFS, relative to Btrfs | ~+135 B / ~+45 B per mark — preliminary, never repeated | §8 |
| a kernel without SELinux | ~−112 B per mark | §8 |

**Projection.** The design figure is the larger, measured one. A drive of *D*
folders and *F* files pins about **D × 1658 B**, whatever *F* is; ignore marks
add 120 B for each hydrated file whose inode the kernel happens to be caching,
and the kernel takes that back along with the inode. The rejected per-file
strategy would have pinned about 1658 B for every online-only file — right
after the first fill, every file:

| drive | marking directories | marking every online-only file |
| --- | --- | --- |
| 1 000 folders, 20 000 files | ~1.7 MB | ~33 MB |
| 10 000 folders, 200 000 files | **~16.6 MB** | **~330 MB** |
| 50 000 folders, 1 000 000 files | ~83 MB | ~1.66 GB |

(§11.7's 3177 B per hydration is not an ignore-mark figure: it is everything
2000 hydrations left pinned before reclaim — xattrs, extent metadata, the marks
— divided by 2000.)

**Verdict: confirmed.** Marking directories holds, needs no hybrid fallback, and
does not have to change. The cost follows the number of folders at ~1.66 KB each,
and the saving over per-file marks is the files-per-folder ratio — twenty times on
the drive shapes above — because a mark's cost is the inode it pins, not the mark.
Two things remain open and neither can overturn the verdict: the ~500 B by which the
realistic tree exceeds the loop is unattributed (a per-cache run would settle it),
and no real drive's cost has been measured through the Graph listing yet. At the
measured figure a drive of 100 000 folders would pin ~166 MB.

## 14. Notification events for the write phase

The write phase (`writes/design.md` §3.3) learns about local changes from a
second fanotify group: in the daemon, **unprivileged**, notification class,
`FAN_REPORT_DFID_NAME_TARGET`, with inode marks on every directory, next to the
helper's pre-content group. None of §§1–13 covers such a group. Everything below
is **measured** by `tests/vm/watch_probe.rs`, one VM run on Btrfs and one on ext4,
kernel `7.2.7-200.fc44`. The probe starts as root in the guest and keeps root for
two things only: setting the guest up, and the helper's side, which is a
pre-content group set up as the helper sets up its own. The daemon's side runs in
a child that the probe re-executes as uid/gid 1000. The child prints its own
`/proc/self/status`: `Groups []`, `CapEff 0000000000000000`.

Btrfs and ext4 agreed on every errno, every event, every record and every order.
They differ in three things only: the handles themselves (Btrfs type `0x4d`,
20 bytes; ext4 type `0x1`, 8 bytes), the inode number in an `O_TMPFILE`'s
pseudo-name (§14.3), and the order in which `rm -rf` visited the entries. The
subvolume check (§14.6) exists only on Btrfs. The probe prints a record, not a pass/fail list. It fails only if the filesystem is
not the one named or a child does not finish.

Reproduce with:

```
cargo build --release --manifest-path tests/vm/Cargo.toml --bin watch-probe
tests/vm/run.sh tests/vm/target/release/watch-probe --fs btrfs
tests/vm/run.sh tests/vm/target/release/watch-probe --fs ext4
```

### 14.1 What an unprivileged process may set up

`fanotify_init`, with `event_f_flags` `O_RDONLY | O_CLOEXEC`:

| flags | uid 1000 | root |
| --- | --- | --- |
| `FAN_CLASS_NOTIF`, no FID reporting | **`EPERM`** | — |
| `FAN_REPORT_FID`; `FAN_REPORT_DIR_FID`; `FAN_REPORT_DFID_NAME`; `FAN_REPORT_DFID_NAME \| FAN_REPORT_FID` | OK | — |
| `FAN_REPORT_DFID_NAME_TARGET \| FAN_NONBLOCK \| FAN_CLOEXEC` (the design's) | **OK** | — |
| the design's + `FAN_UNLIMITED_QUEUE`, or + `FAN_UNLIMITED_MARKS` | **`EPERM`** | OK (both at once) |
| the design's + `FAN_REPORT_TID`, or + `FAN_REPORT_PIDFD` | **`EPERM`** | `PIDFD`: OK |
| the design's + `FAN_ENABLE_AUDIT`, or + `FAN_REPORT_FD_ERROR` | `EPERM` | — |
| `FAN_REPORT_MNT` | OK | — |
| `FAN_CLASS_CONTENT \| FAN_REPORT_FID` | `EPERM` | **`EINVAL`** |
| `FAN_CLASS_PRE_CONTENT \| FAN_REPORT_FID` | `EPERM` | **`EINVAL`** |
| `FAN_CLASS_PRE_CONTENT \| FAN_REPORT_DFID_NAME` | — | **`EINVAL`** |
| `FAN_CLASS_PRE_CONTENT` (the helper's, no FID) | `EPERM` | OK |

So on 7.2, a permission-class group cannot report file handles even for root.
The helper's group therefore cannot carry directory-entry events, and the
daemon's own group needs no privilege. That events such as `FAN_CREATE` need FID
reporting at all is what `fanotify_mark(2)` says; the probe does not test it.

`fanotify_mark` from uid 1000, in the design's group:

| mark | uid 1000 |
| --- | --- |
| inode mark on an own directory, the design's mask, then `+ FAN_MODIFY \| FAN_DELETE_SELF \| FAN_MOVE_SELF` | OK |
| inode mark on a root-owned `0755` directory (the filesystem's root) | OK. Read permission is what is checked, not ownership |
| inode mark on a root-owned `0700` directory | `EACCES` |
| `FAN_MARK_MOUNT`, `FAN_MARK_FILESYSTEM` on an own directory | `EPERM` |
| `FAN_OPEN_PERM` in a notification group | `EINVAL` |
| `FAN_RENAME` in a group without `FAN_REPORT_NAME` (`FAN_REPORT_FID` alone, or `FAN_REPORT_DIR_FID` alone) | `EINVAL` |

### 14.2 The limits: queue, groups, marks

`/proc/sys/fs/fanotify` in the 4 GiB guest: `max_queued_events = 16384`,
`max_user_groups = 128`, `max_user_marks = 36399`, `watchdog_timeout = 0`. The
host, with more memory, has `max_user_marks = 597240`: the default scales with
memory.

- **Queue.** The probe created 16 484 files in a directory marked
  `FAN_CREATE` and read nothing meanwhile (0.35 s on Btrfs). It then read
  **16 385 events: 16 384 `FAN_CREATE` and one `FAN_Q_OVERFLOW`, the last**. The
  overflow event is `event_len 24`: it has no information record, `fd -1` and
  `pid 0`. It says that something was lost, but not what or where. After the
  drain, the next create arrived normally. `FAN_UNLIMITED_QUEUE`, unprivileged:
  `EPERM`.
- **Groups.** 128 groups per uid; the 129th `fanotify_init` fails **`EMFILE`**.
- **Marks.** With `max_user_marks` lowered to 40 for the step, group 1 placed
  25 marks and group 2 placed 15. The next mark was refused **`ENOSPC`**. The
  limit counts per uid, across all of that user's groups. At the limit, adding
  bits to a directory already marked succeeds, because it is not a new mark.
  After group 1 removed one mark, group 2's refused mark succeeded at once. The
  sysctl was then restored.

### 14.3 Each operation and the events it raises

Setup: `tree`, marked with the design's mask plus `FAN_MODIFY | FAN_DELETE_SELF |
FAN_MOVE_SELF`; `tree/A` and `tree/B`, each marked with the design's mask plus
`FAN_MODIFY`; and `out`, a sibling of `tree` that nobody marks. The design's mask
is `FAN_CREATE | FAN_DELETE | FAN_RENAME | FAN_MOVED_FROM | FAN_MOVED_TO |
FAN_CLOSE_WRITE | FAN_ATTRIB | FAN_ONDIR | FAN_EVENT_ON_CHILD`. Every operation
is followed by a read of the queue.

In the output, a bare name such as `A` or `doc` is a handle that
`name_to_handle_at` took **before** any event, so each such name is also a
check that the two encodings agree (§14.6). `#n` is an object first seen in an
event, and `#n=name` when it was found under that name. Every record's `fsid`
equalled `statfs().f_fsid`, and every event had `fd -1`. The Btrfs run,
verbatim except the middle of `rm -rf`:

```
op: create a file: open(A/new, O_CREAT|O_EXCL|O_WRONLY), close
    CREATE|CLOSE_WRITE pid=self DFID_NAME(A,"new") FID(#1=new)
op: mkdir A/d1
    CREATE|ONDIR pid=self DFID_NAME(A,"d1") FID(#2=d1)
op: open A/new O_WRONLY and write 3 bytes, descriptor still open
    MODIFY pid=self DFID_NAME(A,"new") FID(#1=new)
op: ... then close it
    CLOSE_WRITE pid=self DFID_NAME(A,"new") FID(#1=new)
op: write+close in one go: open A/new O_WRONLY|O_APPEND, write, close
    MODIFY|CLOSE_WRITE pid=self DFID_NAME(A,"new") FID(#1=new)
op: truncate(A/new, 0) by path
    MODIFY pid=self DFID_NAME(A,"new") FID(#1=new)
op: rename within a directory: A/new -> A/renamed
    RENAME pid=self OLD_DFID_NAME(A,"new") NEW_DFID_NAME(A,"renamed") FID(#1=new)
    MOVED_FROM pid=self DFID_NAME(A,"new") FID(#1=new)
    MOVED_TO pid=self DFID_NAME(A,"renamed") FID(#1=new)
op: rename a directory within a directory: A/d1 -> A/d1r
    RENAME|ONDIR pid=self OLD_DFID_NAME(A,"d1") NEW_DFID_NAME(A,"d1r") FID(#2=d1)
    MOVED_FROM|ONDIR pid=self DFID_NAME(A,"d1") FID(#2=d1)
    MOVED_TO|ONDIR pid=self DFID_NAME(A,"d1r") FID(#2=d1)
op: rename across marked directories: A/renamed -> B/renamed
    RENAME pid=self OLD_DFID_NAME(A,"renamed") NEW_DFID_NAME(B,"renamed") FID(#1=new)
    MOVED_FROM pid=self DFID_NAME(A,"renamed") FID(#1=new)
    MOVED_TO pid=self DFID_NAME(B,"renamed") FID(#1=new)
op: move a file into the tree: out/in-file -> A/in-file
    RENAME pid=self NEW_DFID_NAME(A,"in-file") FID(in-file)
    MOVED_TO pid=self DFID_NAME(A,"in-file") FID(in-file)
op: move a directory into the tree: out/in-dir -> A/in-dir
    RENAME|ONDIR pid=self NEW_DFID_NAME(A,"in-dir") FID(in-dir)
    MOVED_TO|ONDIR pid=self DFID_NAME(A,"in-dir") FID(in-dir)
op: create a file inside the moved-in directory A/in-dir (which nobody marked)
    (no events)
op: move a file out of the tree: A/outfile -> out/outfile
    RENAME pid=self OLD_DFID_NAME(A,"outfile") FID(outfile)
    MOVED_FROM pid=self DFID_NAME(A,"outfile") FID(outfile)
op: move a directory out of the tree: A/outdir -> out/outdir
    RENAME|ONDIR pid=self OLD_DFID_NAME(A,"outdir") FID(outdir)
    MOVED_FROM|ONDIR pid=self DFID_NAME(A,"outdir") FID(outdir)
op: unlink A/rmme
    DELETE pid=self DFID_NAME(A,"rmme") FID(rmme)
op: rmdir A/d1r
    DELETE|ONDIR pid=self DFID_NAME(A,"d1r") FID(#2=d1)
op: chmod A/chm 0600
    ATTRIB pid=self DFID_NAME(A,"chm") FID(chm)
op: chmod a marked directory itself: A 0700
    ATTRIB|ONDIR pid=self DFID_NAME(A,".")
op: chmod an unmarked child directory: A/in-dir 0700
    ATTRIB|ONDIR pid=self DFID_NAME(in-dir,".")
op: touch A/touchme (utimensat, both times to now)
    ATTRIB pid=self DFID_NAME(A,"touchme") FID(touchme)
op: setxattr A/xa user.test
    ATTRIB pid=self DFID_NAME(A,"xa") FID(xa)
op: safe save: write A/.doc.tmp, close, rename it over A/doc
    CREATE|MOVED_FROM|MODIFY|CLOSE_WRITE pid=self DFID_NAME(A,".doc.tmp") FID(#3)
    RENAME pid=self OLD_DFID_NAME(A,".doc.tmp") NEW_DFID_NAME(A,"doc") FID(#3)
    MOVED_TO pid=self DFID_NAME(A,"doc") FID(#3)
op: backup-rename save: A/doc -> A/doc~, write a new A/doc, close, unlink A/doc~
    RENAME pid=self OLD_DFID_NAME(A,"doc") NEW_DFID_NAME(A,"doc~") FID(#3)
    MOVED_FROM pid=self DFID_NAME(A,"doc") FID(#3)
    DELETE|MOVED_TO pid=self DFID_NAME(A,"doc~") FID(#3)
    CREATE|MODIFY|CLOSE_WRITE pid=self DFID_NAME(A,"doc") FID(#4=doc)
op: hard link within the tree: A/h -> A/h2
    CREATE pid=self DFID_NAME(A,"h2") FID(h)
op: hard link into the tree: out/lnk-src -> A/lnk
    CREATE pid=self DFID_NAME(A,"lnk") FID(lnk-src)
op: hard link out of the tree: A/h -> out/h-out
    (no events)
op: write+close A/h's content through the outside link out/h-out
    (no events)
op: O_TMPFILE in A, write 3 bytes (no name yet)
    MODIFY pid=self DFID_NAME(A,"#285") FID(#5)
op: ... linkat it in as A/linked
    CREATE pid=self DFID_NAME(A,"linked") FID(#5)
op: ... then close it
    CLOSE_WRITE pid=self DFID_NAME(A,"#285") FID(#5)
op: renameat2(RENAME_EXCHANGE) A/h2 <-> A/chm
    RENAME pid=self OLD_DFID_NAME(A,"h2") NEW_DFID_NAME(A,"chm") FID(h)
    MOVED_FROM pid=self DFID_NAME(A,"h2") FID(h)
    MOVED_TO pid=self DFID_NAME(A,"chm") FID(h)
    RENAME pid=self OLD_DFID_NAME(A,"chm") NEW_DFID_NAME(A,"h2") FID(chm)
    MOVED_FROM pid=self DFID_NAME(A,"chm") FID(chm)
    MOVED_TO pid=self DFID_NAME(A,"h2") FID(chm)
op: create+write A/thr from a second thread of this process
    CREATE|MODIFY|CLOSE_WRITE pid=self DFID_NAME(A,"thr") FID(#6=thr)
op: create+write A/child from a child process (/bin/sh -c 'echo x > A/child')
    CREATE|MODIFY|CLOSE_WRITE pid=0 DFID_NAME(A,"child") FID(#7=child)
op: A/mixed: this process creates+writes it, then a child process appends, nothing read between
    CREATE|MODIFY|CLOSE_WRITE pid=self DFID_NAME(A,"mixed") FID(#8=mixed)
    MODIFY|CLOSE_WRITE pid=0 DFID_NAME(A,"mixed") FID(#8=mixed)
op: A/mixed: a child process appends, then this process appends, nothing read between
    MODIFY|CLOSE_WRITE pid=0 DFID_NAME(A,"mixed") FID(#8=mixed)
    MODIFY|CLOSE_WRITE pid=self DFID_NAME(A,"mixed") FID(#8=mixed)
op: rename the marked root: tree -> tree-moved (its parent is not marked)
    MOVE_SELF|ONDIR pid=self DFID_NAME(tree,".")
op: rm -rf tree-moved
    DELETE pid=self DFID_NAME(A,"h") FID(h)
    ... one DELETE per entry of A, each with the object's FID ...
    DELETE|ONDIR pid=self DFID_NAME(tree,"A") FID(A)
    DELETE pid=self DFID_NAME(B,"renamed") FID(#1=new)
    DELETE|ONDIR pid=self DFID_NAME(tree,"B") FID(B)
    DELETE_SELF|ONDIR pid=self DFID_NAME(tree,".")
late events, 300 ms later: 0
```

What that shows:

- **Every event names the object.** With `FAN_REPORT_DFID_NAME_TARGET`, every
  event on an entry carries the directory and the name (`DFID_NAME`) plus the
  object's own handle (`FID`). This includes `FAN_DELETE`, whose FID is that of
  the object just removed, and `FAN_MODIFY`, `FAN_CLOSE_WRITE` and
  `FAN_ATTRIB`. The exception is an event on a directory itself, which carries
  only `DFID_NAME(<that directory>, ".")`.
- **A rename is `FAN_RENAME`, then `FAN_MOVED_FROM`, then `FAN_MOVED_TO`.** When
  both directories are marked, `FAN_RENAME` carries `OLD_DFID_NAME`,
  `NEW_DFID_NAME` and the moved object's `FID`. **When only one side is marked,
  it carries only that side's record.** A move into the tree has `NEW_DFID_NAME`
  and no `OLD_DFID_NAME`. A move out has `OLD_DFID_NAME` and no `NEW_DFID_NAME`.
  The `FID` is present either way. Each `FAN_MOVED_FROM` and `FAN_MOVED_TO`
  repeated what a `FAN_RENAME` had already said; the one `FAN_MOVED_FROM` that
  was not a separate event was merged into an earlier event. `FAN_RENAME` was
  never merged. `RENAME_EXCHANGE` is two complete renames, one in each
  direction. To the kernel, `out` is simply a directory this group has not
  marked. So a move into an unmarked directory *inside* the tree should look
  exactly like a move out. That is an inference; only `out` was measured.
- **Unread events merge, and their order is lost.** Events on the same object,
  under the same name, from the same process, merge into the first such event
  still unread, and the masks are OR-ed. Examples: `CREATE|CLOSE_WRITE` for a
  plain create; `CREATE|MOVED_FROM|MODIFY|CLOSE_WRITE` for a safe save's
  temporary file; `DELETE|MOVED_TO` for `doc~`, which was moved to, then deleted.
  A mask is therefore a set, not a sequence. **Events from different processes
  never merged**, in either order (the two `A/mixed` rows).
- **A safe save reports nothing about the inode it replaces.** There is no
  `FAN_DELETE` for the old `doc`. The name simply passes to the new object: a
  `FAN_RENAME` and a `FAN_MOVED_TO` carrying the new object's `FID`.
- **Hard links.** A link made inside the tree, or from outside into it, is a
  `FAN_CREATE` whose `FID` is the existing object's. A link out of the tree
  raises nothing. **A write through the outside name raises nothing either**:
  the parent's mark sees only access through the parent. This matches what §1
  measured for permission events.
- **`O_TMPFILE`.** `FAN_MODIFY` and `FAN_CLOSE_WRITE` carry the pseudo-name
  **`#<inode number>`** (`"#285"` on Btrfs, `"#24"` on ext4) in the directory
  the file was opened in. That name never exists. Only `linkat`'s `FAN_CREATE`
  carries the real name. Every event on this file carries the same `FID`.
  Events on the descriptor after the `linkat` still carry the pseudo-name, as
  the close shows.
- **Directories.** A `chmod` of a marked directory raises one event,
  `ATTRIB|ONDIR DFID_NAME(A,".")`, not a second one through the mark on its
  parent `tree`. A `chmod` of an unmarked child directory, reported through its
  parent's mark, arrives as `DFID_NAME(in-dir,".")`. Its `DFID` is the child
  directory itself, not the marked parent. Nothing inside an unmarked
  subdirectory is seen, not even a create in a directory that was just moved in.
- **The marked root.** Renaming it, from a parent nobody marked, raises
  `MOVE_SELF|ONDIR` with `DFID_NAME(tree,".")`. Removing it raises
  `DELETE_SELF|ONDIR` with `DFID_NAME(tree,".")`, last, after the `FAN_DELETE`
  of each entry. Neither event carries a `FID` record.
- **The pid.** Every event this process caused carries its own pid, from any
  thread. Events caused by a child process it started (`/bin/sh`), or by root
  (§14.5), carry `0`. Nothing else was ever seen.

### 14.4 The pid rule, and a pre-content group on the same directories

The unprivileged group sees a real pid only for its own process, and cannot ask
for more: `FAN_REPORT_TID` and `FAN_REPORT_PIDFD` are `EPERM` (§14.1). No
`fanotify_init` or `fanotify_mark` flag excludes a process's own events, so
"drop what we caused" can only be done by the listener checking the pid.
Merging does not defeat that check: an own event and a foreign event on the same
file stayed two events (§14.3).

The probe ran the same operations twice, in fresh directories. The second time,
a root `FAN_CLASS_PRE_CONTENT` group held `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`
marks on `tree`, `A` and `B` and answered all 19 permission events those
operations raised, allowing each. The unprivileged group's record was
**identical: 116 lines, every event, record and pid the same**. The only
difference was the inode number in the `O_TMPFILE` pseudo-name, which the
comparison normalises. The two groups do not interact.

### 14.5 What writes through a pre-content event descriptor raise

For a file named `fill-*`, the root group made one kind of change through the
event descriptor and then allowed the open. The descriptor was opened
`O_RDWR | O_LARGEFILE | O_CLOEXEC | O_NONBLOCK`, as the helper opens its own
(§12.4). The opener, a uid-1000 process, opened the file `O_RDONLY`, read it and
closed it. The same changes, made by the unprivileged process on a descriptor of
its own, were the comparison:

| change | through the event descriptor (by root) | through an ordinary `O_RDWR` descriptor (own) |
| --- | --- | --- |
| nothing (control) | nothing | — |
| `pwrite` | **nothing** | `MODIFY` |
| `ftruncate` | **`MODIFY`**, pid 0 | `MODIFY` |
| `futimens` | **`ATTRIB`**, pid 0 | `ATTRIB` |
| `fsetxattr` `user.konedrive.state` | **`ATTRIB`**, pid 0 | `ATTRIB` |
| `fallocate(PUNCH_HOLE \| KEEP_SIZE)` | **nothing** | `MODIFY` |
| the descriptor's close | nothing (no `CLOSE_WRITE`) | `CLOSE_WRITE` |

`FMODE_NONOTIFY` (§6) silences what goes through the *file*: writes,
`fallocate`, the close. It does not silence changes to the inode's attributes:
size, times and xattrs. The kernel reports those from the dentry, not from the
file. That explanation is an inference from `fsnotify_change()` and
`fsnotify_xattr()`, which take no file; the table is the measurement.

A fill's commit (`konedrived`'s `sync/source.rs`, `commit`) does exactly these
three things: `set_len`, `futimens`, and the cTag, stamp and state xattrs. **So a
fill raises `FAN_MODIFY` and `FAN_ATTRIB`**, carrying the pid of whoever does the
commit. That is the daemon, so its own group sees its own pid. The data itself
is silent.

A denied create: `open(O_CREAT | O_WRONLY)` of a new name, which the root group
answered `FAN_DENY`, failed `EPERM` for the opener. The file was left behind,
empty, and the unprivileged group received its `FAN_CREATE`, pid `self`.

### 14.6 From an event to a path, without privilege

| step | uid 1000 |
| --- | --- |
| `name_to_handle_at` on an own directory and on a file | OK. The same handle with and without `AT_HANDLE_FID` |
| the event's `DFID_NAME` handle vs `name_to_handle_at(directory)` | **byte-equal** (type and bytes); Btrfs `0x4d`/20 bytes, ext4 `0x1`/8 bytes, tmpfs `0x1`/12 bytes |
| the event's `FID` vs `name_to_handle_at(file)` | **byte-equal** |
| the event's `fsid` vs `statfs(directory).f_fsid` | **equal** |
| a directory's handle before and after a rename | equal |
| `open_by_handle_at`, a file's or a directory's handle | **`EPERM`** (root, the control: OK) |

A directory map built with `name_to_handle_at` at mark time, keyed by
`(fsid, handle type, handle bytes)`, therefore matches the events exactly, and
it is the only way an unprivileged process can turn a handle into a path.

**tmpfs:** the design's group marks a tmpfs directory unprivileged, and the
event's `DFID` equals `name_to_handle_at`, with the tmpfs `f_fsid`.

**Btrfs subvolumes:** each subvolume has its own `f_fsid`. Here the
top-level subvolume was `1e5f4525:3af8e10e` and a subvolume inside it was
`1e5f4525:3af8e00b`. A group that already marks a directory on the top level
refuses a mark on the subvolume with **`EXDEV`**. A group of its own marks the
subvolume, and its events carry the subvolume's `fsid`, with a `DFID` equal to
`name_to_handle_at`. So **one group cannot watch two subvolumes of the same
filesystem**. The probe did not try a second, different filesystem mounted
inside the tree.

### 14.7 What §14 does not cover

- XFS was not run; nor was the host (the unprivileged half would run there as
  well).
- The helper's side was a stand-in group, not the shipped binary. It had the
  same class, flags and mark mask, but none of the helper's ignore marks and no
  daemon-pid exemption. The fill writes were made by root, so their pid was `0`;
  that the daemon's own commit shows its own pid follows from §14.3's own-pid
  rows and the table in §14.5, but was not run as one piece.
- Writes through a shared writable mapping, which continue after the
  descriptor is closed; bind mounts; a second filesystem mounted inside a root;
  overlayfs; network filesystems; user namespaces.
- How long the kernel takes to queue an event under load, and whether a
  `FAN_RENAME` can be split from its `FAN_MOVED_*` pair by a read that falls
  between them.

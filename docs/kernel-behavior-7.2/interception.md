# Marks, ignore marks and answers to opens (§§1–7)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

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

**Measured now, by `tests/vm/scenarios/` (Btrfs only so far):**

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

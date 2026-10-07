# `open_by_handle_at` from the helper's sandbox (§15)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

## 15. `open_by_handle_at` from the helper's sandbox, and the helper's own opens

The write phase needs a descriptor for an object that left the folder
(`docs/design/writes.md` §4.6), and the daemon cannot open a file handle (§14.6:
`EPERM`). The helper can, with its `CAP_DAC_READ_SEARCH`, but it runs under the
shipped unit: root with two capabilities and no `CAP_DAC_OVERRIDE`, a read-only
view of everything but two directories, and a seccomp filter. Two questions came
first (task W7a): does `open_by_handle_at` relative to a directory descriptor the
daemon passed give a writable descriptor under that sandbox; and do the helper's
own opens raise `FAN_OPEN_PERM` on its own marks.

**Measured** by `tests/vm/scenarios/open_by_handle.rs` under `tests/vm/run.sh unit`: kernel
`7.2.7-200.fc44`, systemd 259, Btrfs, two runs with identical records.
`helper_unit_test.sh` installs a copy of the shipped unit in which only
`ExecStart=`, `Restart=` and the runtime directory's name differ, so the probe
runs with every line of the helper's sandbox. The service's own
`/proc/self/status` says so. A root process outside the sandbox lays out
`/mnt/btrfs/obh/d` with objects of uid 1000, and hands the service `d`'s
descriptor and their handles over a socket, as a daemon hands the helper a
directory. Every open carries `O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK`. The
record, verbatim:

```
the service: uid 0, CapEff 0000000000200004, NoNewPrivs 1, Seccomp 2
the daemon's directory descriptor: directory 755 uid 1000 nlink 1; its mount is read-write
the same directory opened by the service itself: its mount is read-only

open_by_handle_at(the daemon's descriptor, handle, flags | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK):
  f644     O_PATH                 ok: file 644 uid 1000 nlink 1, st_dev = the directory's; item-id xattr EBADF
  f644     O_RDONLY               ok: file 644 uid 1000 nlink 1, st_dev = the directory's; item-id xattr read
  f644     O_RDWR                 EACCES
  f644     O_RDONLY|O_DIRECTORY   ENOTDIR
  f666     O_PATH                 ok: file 666 uid 1000 nlink 1, st_dev = the directory's; item-id xattr EBADF
  f666     O_RDONLY               ok: file 666 uid 1000 nlink 1, st_dev = the directory's; item-id xattr read
  f666     O_RDWR                 ok: file 666 uid 1000 nlink 1, st_dev = the directory's; item-id xattr read; pwrite ok
  f666     O_RDONLY|O_DIRECTORY   ENOTDIR
  f600     O_PATH                 ok: file 600 uid 1000 nlink 1, st_dev = the directory's; item-id xattr EBADF
  f600     O_RDONLY               ok: file 600 uid 1000 nlink 1, st_dev = the directory's; item-id xattr read
  f600     O_RDWR                 EACCES
  f600     O_RDONLY|O_DIRECTORY   ENOTDIR
  sub      O_PATH                 ok: directory 755 uid 1000 nlink 1, st_dev = the directory's; item-id xattr EBADF
  sub      O_RDONLY               ok: directory 755 uid 1000 nlink 1, st_dev = the directory's; item-id xattr read
  sub      O_RDWR                 EISDIR
  sub      O_RDONLY|O_DIRECTORY   ok: directory 755 uid 1000 nlink 1, st_dev = the directory's; item-id xattr read
  vol/f    O_PATH                 ok: file 644 uid 1000 nlink 1, st_dev ≠ the directory's; item-id xattr EBADF
  vol/f    O_RDONLY               ok: file 644 uid 1000 nlink 1, st_dev ≠ the directory's; item-id xattr read
  vol/f    O_RDWR                 EACCES
  vol/f    O_RDONLY|O_DIRECTORY   ENOTDIR
  tmpfs/f  O_PATH                 ESTALE
  tmpfs/f  O_RDONLY               ESTALE
  tmpfs/f  O_RDWR                 ESTALE
  tmpfs/f  O_RDONLY|O_DIRECTORY   ESTALE

relative to the directory the service opened itself (its read-only view):
  f644     O_RDONLY               ok
  f644     O_RDWR                 EACCES
  f666     O_RDONLY               ok
  f666     O_RDWR                 EROFS

the service's own opens by handle, in a FAN_CLASS_PRE_CONTENT group of its own (pid 1397):
  directory marked  f644  O_PATH                 0 FAN_OPEN_PERM event(s); the open returned ok after 0 ms
  directory marked  f644  O_RDONLY               1 FAN_OPEN_PERM event(s) (own pid); the open returned ok after 5 ms
  directory marked  f666  O_RDWR                 1 FAN_OPEN_PERM event(s) (own pid); the open returned ok after 5 ms
  directory marked  sub   O_RDONLY|O_DIRECTORY   0 FAN_OPEN_PERM event(s); the open returned ok after 0 ms
  file marked       f600  O_RDONLY               1 FAN_OPEN_PERM event(s) (own pid); the open returned ok after 5 ms
```

`vol` is a Btrfs subvolume inside `d`. `tmpfs/f` is a file on the guest's `/run`.
"Directory marked" is `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD` on `d`, as the helper
marks. "File marked" is `FAN_OPEN_PERM` on `f600` alone, as `MarkFile` marks, with
the directory's mark removed.

What that shows:

- **The object is opened on the mount of the descriptor passed, not the
  helper's.** Relative to the daemon's descriptor, a `0666` file opens `O_RDWR`
  and takes a write. Relative to the same directory opened in the service's own
  namespace, it is `EROFS`. `ProtectSystem=strict` and `ProtectHome=read-only` do
  not make the object read-only, as long as the helper opens relative to what the
  daemon sent. That is the same reason a fill on open works (§11.6).
- **The sandbox makes the helper "other" to a user's file.** The open checks
  the object's own permissions, as any open does, as root without
  `CAP_DAC_OVERRIDE`. `O_RDWR` is `EACCES` for a `0644` or `0600` file of the
  user's. Only a world-writable file opens for writing. `O_RDONLY` opens every
  file, `0600` included, through `CAP_DAC_READ_SEARCH`, and its attributes can be
  read. **So the helper cannot hand the daemon a writable descriptor for a
  user's file.** The design's `O_RDWR` became `O_RDONLY | O_NONBLOCK`. The daemon,
  the file's owner, reopens it for writing through `/proc/self/fd/<fd>`. The
  unit check measured that too (the second part of `tests/vm/scenarios/open_by_handle.rs`).
  It moved a placeholder of uid 1000's into a root-owned `0700` directory, which
  the user cannot open by path (`EACCES`). The helper, through the shipped unit,
  handed it over `O_RDONLY | O_NONBLOCK`. The user's reopen was `O_RDWR`, and the
  write landed. No path lookup happens on either side.
- **`O_PATH` raises nothing.** No event, measured. It also checks no permission
  on the object: that is the kernel's rule for `O_PATH`, not separately measured
  here, since `O_RDONLY` opened every file too. `fgetxattr` through it is
  `EBADF`. So the helper looks at
  the object through `O_PATH` first (owner, type, device, link count), and opens
  it for real only when it passes.
- **A handle is not tied to the subvolume of the descriptor.** A file in
  another Btrfs subvolume opens relative to a directory in the top-level one,
  with another `st_dev`, so the device check has to be the helper's own. A
  handle from another filesystem is `ESTALE`.
- **The helper's own opens are events aimed at itself** (§7, now for
  `open_by_handle_at`). An `O_RDONLY` or `O_RDWR` open of a file in a marked
  directory, or of a file with its own mark, raises one `FAN_OPEN_PERM`
  carrying the listener's pid. The open waits for the answer. `O_PATH` opens and
  directory opens raise none.

**The consequence for the helper**, reasoned from the code and then checked in
the VM. `OpenByHandle` runs on the thread that serves that daemon's connection,
and that thread is also the only reader of what the daemon sends. Suppose the
event from the helper's own open were decided like any other. A placeholder
would become a `HydrateRequest` to that same daemon. The daemon's `HydrateDone`
would then wait unread behind the open, so the event is never answered and the
open never returns. The daemon's call timeout (30 s) ends the socket, but not the
open. So the helper allows events that carry its own pid in its event loop, on
that thread, before the worker pool. The pid is the process's whichever thread
opened, since the group has no `FAN_REPORT_TID`. The helper opens only one kind
of file: `OpenByHandle` objects, which go straight to their owner's daemon (its
feature probe at registration is gone, limitations log F234). Checked in the VM suite,
Btrfs (`tests/vm/run.sh quick --only OpenByHandle`). An `OpenByHandle` of an
`online-only` placeholder in a marked directory returned in 0.01 s. It fetched
nothing and left no ignore mark. A reader in another process was intercepted and
filled afterwards. A moved-out placeholder asked for again after `MarkFile` came
back at once too.

Reproduce with:

```
tests/vm/run.sh unit                          # the record above, then the helper's own OpenByHandle under the unit
tests/vm/run.sh quick --only OpenByHandle     # the helper's OpenByHandle in the suite, Btrfs
```

### 15.1 What §15 does not cover

- Only Btrfs. ext4 and XFS were not run: `quick` is Btrfs only, and so is the
  unit check. On them only the handles themselves differ (§14), and they have no
  subvolumes.
- `seccomp`: `open_by_handle_at` belongs to none of the unit's denied groups,
  and the unit check's `SystemCallErrorNumber=kill` pass saw no denial. That is
  the whole evidence.
- A filesystem mounted inside the folder, a bind mount passed as the directory,
  and a directory handed over from inside a user namespace were not tried.
- A lease: `O_NONBLOCK` is there so that a file somebody holds a write lease on
  answers `EAGAIN` rather than stopping the connection's thread for up to 45 s
  (§12.4). That comes from §12.4's measurement of the same flag on event
  descriptors. It was not measured for `open_by_handle_at`.

# Notification events for the write phase (§14)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

## 14. Notification events for the write phase

The write phase (`docs/design/writes.md` §3.3) learns about local changes from a
second fanotify group: in the daemon, **unprivileged**, notification class,
`FAN_REPORT_DFID_NAME_TARGET`, with inode marks on every directory, next to the
helper's pre-content group. None of §§1–13 covers such a group. Everything below
is **measured** by `tests/vm/watch_probe/`, one VM run on Btrfs and one on ext4,
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

A fill's commit (`konedrived`'s `hydration/source/fill.rs`, `commit`) does exactly these
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

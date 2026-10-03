# Leases (§12)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

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

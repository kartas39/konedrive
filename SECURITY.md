# Security

## What runs as root

One part of KOneDrive runs as root: `konedrive-helper`, a system service
(`packaging/systemd/konedrive-helper.service`). Everything else — `konedrived`, `konedrivectl`,
the `konedrive` window, both Dolphin plugins — runs as your own user with no elevated privileges.

The helper runs as uid 0 with two capabilities, `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH`; the
unit's `CapabilityBoundingSet=` drops every other one.

- `CAP_SYS_ADMIN` is what the kernel requires to create a fanotify group with pre-content
  permission events (`FAN_CLASS_PRE_CONTENT`), which is how an open is stopped *before* the
  program reads anything. It is also one of the broadest capabilities Linux has.
- `CAP_DAC_READ_SEARCH` lets it read files and search directories whatever their permission
  bits, so it can reach a sync folder inside a `0700` home directory and read the
  `user.konedrive.*` attributes of the files there. It is also what `open_by_handle_at(2)`
  requires, which "Open by file handle" below uses.

The helper never talks to Microsoft Graph, holds no OneDrive credentials, and has no network.

## What it does, and for whom

The helper listens on `/run/konedrive/helper.sock`. The socket is mode `0666`, so any local user
can connect to it. Every request is authorised by the uid of the process that connected, which
the kernel supplies (`SO_PEERCRED`) and a client cannot forge, and by who owns the file or
directory the request is about. A request carries a descriptor for that object, not a path.

- **Register a folder.** The directory must be owned by the asking uid. It must not be on a
  filesystem the helper refuses (NFS, SMB, FUSE, FAT, exFAT, AFS, Ceph), and it must not contain,
  or sit inside, another registered folder. A folder id that another uid registered is refused.
  Beyond that, the helper does not care where the directory is: it marks a tree on any mount it
  is asked to, under `/home` or on any other local disk. It marks every directory in the tree, so
  that the kernel suspends opens of the files in them, and records the folder in
  `/var/lib/konedrive/roots.json` (mode `0600`) so that it can mark the tree again after a
  restart.
- **Mark or unmark.** Any directory or file owned by the asking uid, on a filesystem where that
  uid has a registered folder. The object does not have to be inside that folder.
- **Clear an ignore mark.** Any regular file the asking uid owns, anywhere. The worst this can do
  is make that user's own file be intercepted again.
- **Open by file handle** (`OpenByHandle`, for the write phase). The request carries a file
  handle (the kernel's name for an inode, at most 128 bytes) and a directory. The directory must
  be owned by the asking uid, on a filesystem where that uid has a registered folder. The helper
  opens the object the handle names, relative to that directory. It first looks without
  opening, through `O_PATH`, which checks no permission, raises no fanotify event and breaks no
  lease. It hands the object back only if all of these hold:
  - it is a regular file or a directory;
  - it is owned by the asking uid;
  - it is on that directory's device (a Btrfs subvolume has a device of its own);
  - it still has a link;
  - it carries `user.konedrive.item-id`.

  Anything else is refused (`EPERM`, or `ESTALE` for the asker's own deleted object), and nothing
  else is ever opened for real. A file comes back read-only (`O_RDONLY | O_NONBLOCK`): under the
  unit the helper cannot open a user's file for writing (`EACCES`, measured), and the daemon
  reopens it itself, as its owner. A directory comes back open for listing. The daemon needs this
  when an item has left its folder: to learn where it went, to keep intercepting a placeholder
  that was moved out, and to download it before the item is deleted in OneDrive. File handles are
  guessable, so these checks on the object itself are the whole authorisation.

  **What it newly lets a user do.** A user can get a descriptor for an object of their own that
  carries konedrive's item id, on a filesystem where they have a folder, even where they cannot
  reach it by path. A handle bypasses path lookup, so a directory they cannot enter does not
  stop it. The case that matters is another user moving one of your files into a private
  directory of theirs: you can still read that file, and write it through your own reopen. It is
  yours, and was marked as konedrive's. For a directory the reach is wider than one object: the
  descriptor it returns anchors `openat()` and `fstatat()`, so it opens up the whole subtree below
  that directory to whatever the user's own permissions allow there, including entries they could
  not name before because a directory above was closed to them. The daemon never lists or opens
  anything beneath such a descriptor: it reads only the directory's own attributes and where it
  is, and hands it back to the helper (`MarkDir`, `UnmarkDir`). To walk a moved-out directory it
  opens it again by its path, through the user's own lookups, and checks that it is the same
  inode, so a directory the user cannot reach by path is not walked. Two smaller things are disclosed. The answer tells a
  handle that names a live object from one that names nothing (`EPERM` against `ESTALE`), for any
  object on that filesystem, which says that an inode exists but nothing about its name or
  content. And a refused request is not logged. `docs/limitations-and-workarounds.md`, F90.
- **The helper's own opens.** The object `OpenByHandle` opens may be one the helper itself
  intercepts, and an open it had to decide on would wait for the very connection that asked. So
  events caused by the helper's own process are let through at once. The helper opens no file
  but those objects and, at registration, its probe's nameless file, and it reads neither
  (`docs/limitations-and-workarounds.md`, F92).
- **Unregister.** Only a folder the asking uid registered.
- **Answer opens.** An intercepted open is handed, as a descriptor, to the daemon of the uid that
  owns the file, and waits for that daemon's answer. So an open of a user's placeholder waits on
  that user's daemon, whoever makes it — another user or root included. A daemon is only ever
  handed descriptors for its own user's files.

The helper knows users, not OneDrive accounts. A user with several accounts has one daemon, with
one connection to the helper for all of them, and that daemon decides which account's folder a
handed-over file is in; nothing about accounts crosses the socket.

Per-uid bounds keep one local user from starving another. Each uid may hold at most 16
connections. At most 8 workers wait for one uid's daemon to connect, and at most 32 across all
uids. Each connection has at most 64 fills in flight. These numbers were chosen, not all of them
measured (`docs/limitations-and-workarounds.md`, "Provisional numbers").

**The registration's write probe.** The first time a folder is registered, the helper checks that
its filesystem can hold placeholders: as root, it creates a nameless temporary file (`O_TMPFILE`)
in the folder, writes to it and punches a hole in it, sets a `user.*` attribute and takes a
lease, then closes it. The file never has a name and is gone when it is closed. Under the unit,
the helper's view of the filesystem is read-only almost everywhere a sync folder can be
(`ProtectSystem=strict`, `ProtectHome=read-only`), so this write is normally refused. The helper
then relies on its filesystem type check, and on the same probe, which the daemon runs as you.

## What the unit's hardening prevents

`packaging/systemd/konedrive-helper.service` narrows what the helper can do. `systemd-analyze
security` rates it 2.4 ("OK"). `tests/vm/run.sh unit` boots a VM with systemd, installs this unit
and checks, through it, that a folder is registered, marked and intercepted and that no system call
is denied (`docs/limitations-and-workarounds.md`, W16).

- **No new privileges.** `NoNewPrivileges=yes`, and the two capabilities above as its whole
  bounding set. (`RestrictSUIDSGID=yes` is left out: it makes every `openat2()` fail with `ENOSYS`,
  and the helper opens registered folders only through `openat2()`.)
- **No network.** `PrivateNetwork=yes` and `RestrictAddressFamilies=AF_UNIX`.
- **Writes only to its own two directories.** `ProtectSystem=strict`, `ProtectHome=read-only`,
  `ReadWritePaths=/var/lib/konedrive /run/konedrive`, plus a private `/tmp` (`PrivateTmp=yes`).
  A fill on open still works: the daemon writes through the descriptor the helper handed it, and
  the kernel opened that descriptor against the opener's mount, not the helper's. The VM suite
  measured this (`docs/kernel-behavior-7.2.md`, §11.6). The same goes for `OpenByHandle`, which
  opens relative to the daemon's directory, on the daemon's mount (§15). Without
  `CAP_DAC_OVERRIDE`, the helper still cannot open a user's file for writing there.
- **No way to undo that with `CAP_SYS_ADMIN`.** Without a filter, the helper could simply remount
  its read-only view read-write. `SystemCallFilter=~@mount …` makes `mount`, `umount2`,
  `fsopen`, `fsmount`, `move_mount`, `mount_setattr`, `pivot_root` and `chroot` fail with
  `EPERM`. `RestrictNamespaces=yes` blocks `unshare`, `setns` and new namespaces, so it cannot
  step into another mount namespace. `SystemCallArchitectures=native` keeps a 32-bit ABI from
  getting around the filter.
- **No kernel settings.** `ProtectKernelTunables=yes` and `ProtectControlGroups=yes` make
  `/proc/sys`, `/sys` and the cgroup tree read-only, so it cannot write
  `/proc/sys/kernel/core_pattern`. `ProtectKernelModules=yes` stops module loading,
  `ProtectKernelLogs=yes` hides the kernel log, `ProtectClock=yes` stops clock changes, and
  `ProtectHostname=yes` stops hostname changes.
- **No raw disk, per the manual.** `ProtectClock=yes` also implies a device allow-list: the RTC,
  read-only, and the standard pseudo devices such as `/dev/null`. That keeps it from opening a
  block device.
- **Also denied:** swap, reboot and kexec, raw port I/O, `ptrace`, `perf_event_open`,
  `pidfd_getfd`, obsolete system calls (`@swap @reboot @raw-io @debug @obsolete
  @cpu-emulation`), writable-and-executable memory (`MemoryDenyWriteExecute=yes`), realtime
  scheduling (`RestrictRealtime=yes`) and personality changes (`LockPersonality=yes`).

## What it does not prevent

Treat a compromised helper as a compromised root.

- **It can read everything.** With uid 0 and `CAP_DAC_READ_SEARCH` it can read every file on the
  machine: other users' files, `/etc/shadow`, SSH keys. `ProtectHome=read-only` stops writes, not
  reads.
- **It can watch and stop any open.** With `CAP_SYS_ADMIN` it can mark any mount, or a whole
  filesystem, with fanotify, and so see, delay or deny every open on the machine.
- **The rest of `CAP_SYS_ADMIN`.** The system-call filter is a deny-list. It closes the best-known
  ways out of the sandbox, not all of them; `bpf(2)`, for one, is not filtered. An allow-list,
  `PrivateDevices=` and a non-root user would narrow this further. None of them is in the unit,
  because none can be tested without running it under systemd.
- **`ReadWritePaths=` is not a jail.** The daemon hands the helper directory descriptors that
  are open on the host's read-write mounts, and `openat(fd, "../..")` against one of them can
  climb out of `/var/lib/konedrive` and `/run/konedrive`. With uid 0, a compromised helper that
  does this can write root-owned files anywhere, and it can `chmod` its own files outside
  `ReadWritePaths=` even without escaping — "writes only to its own two directories" holds for a
  well-behaved helper, not a compromised one.

## Credentials

Each Microsoft account's refresh token is stored in KWallet (through the Secret Service D-Bus API),
as an item of its own. The daemon reads it from KWallet to get a short-lived access token for that
account, uses that access token to talk to Microsoft Graph, and never writes the refresh token to
disk, to a log, or anywhere else. Each account's `Dev1` D-Bus interface hands out a short-lived
(about one hour), read-only access token of that account for test runs, whatever the account's
mode, never the refresh token; see `docs/limitations-and-workarounds.md`, W11. A token that can
change files (`Dev1.ReadWriteAccessToken`) is handed out only for a test account listed in
`write_test_drive_ids`, the write phase's development gate (F60). Removing an account deletes its
refresh token. An upload session's URL, which lets anyone holding it write that one file until it
expires, is kept only in the account's tree store (mode `0600`), never logged and never published
over D-Bus, and the account's token is never sent to it.

## Reporting a vulnerability

Please report security issues privately, not in a public issue:

- GitHub's private vulnerability reporting: open the repository's **Security** tab and use
  **Report a vulnerability**;
- or email the maintainer at kartas39@gmail.com.

Please include what you found, how to reproduce it, and, if you can, what you think the impact
is — in particular whether it touches the root helper, since that is the part of KOneDrive most
worth getting right.

There is no formal disclosure timeline or bug bounty; this is a small, alpha-stage project. You
will get a reply, and credit in the fix if you would like it.

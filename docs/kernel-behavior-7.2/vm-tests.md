# Running privileged tests (§9)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

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

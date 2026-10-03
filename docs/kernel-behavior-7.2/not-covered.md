# What this does not cover (§10)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

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

# What a mark costs, and the memory verdict (§8, §13)

Part of the kernel measurements: the introduction and the index are in [`README.md`](README.md).

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

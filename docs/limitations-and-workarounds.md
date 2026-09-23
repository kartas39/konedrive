# Limitations, workarounds and weak spots

One place for everything in konedrive that is limited, worked around, fragile, or knowingly
below the quality we want. It is kept current: an entry is added whenever a decision accepts
a limitation, builds a workaround, picks a number without measuring it, or parks a finding.
Detail lives elsewhere (the spec, `docs/kernel-behavior-7.2.md`, the code); this log is the
index of what is weak and why.

**Kinds.** LIMIT — imposed by the kernel or the platform; we cannot change it, only live with
it. WORKAROUND — something built to route around a limit. FRAGILE — works, but rests on
something delicate. PROVISIONAL — a number chosen, not measured. DEBT — knowingly below the
quality we want.

**Evidence.** *measured* — observed in a test or on this machine. *reasoned* — argued from
the code or the documentation and not yet observed. This project has shipped plausible
mechanisms that turned out false; treat *reasoned* entries accordingly.

**Status.** *open*, *mitigated* (made rare or loud, not gone), *planned* (with where), *fix in
progress* (with the ruling).

---

## 1. Anything that can hand an application zeros or lose data

These come first because the property that outranks everything in this project is that an
application must never read zeros where real content should be.

### Z1. The helper's death releases every suspended open as zeros
- **Kind** LIMIT · **Evidence** measured · **Status** mitigated
- **What:** when the fanotify group's last descriptor closes, the kernel answers every
  suspended open with "allow". An online-only placeholder then reads as zeros. Measured: 200
  suspended opens, `SIGKILL`, all released in 525 ms, every one read zeros.
- **Why:** kernel behaviour on group close; nothing can change it.
- **Cost:** any application waiting for a download at the moment the helper dies gets zeros.
  The credit-gated queue (Ruling H124) raised the exposure: every waiting opener is suspended
  now, not at most 64 per connection.
- **Mitigation:** "the helper must not die" is a design requirement (H128): panics are
  contained on worker and connection threads, `EMFILE` is survivable, disconnects are handled —
  all proven in the VM suite.
- **Way out, unverified:** hand the group's descriptor to systemd's file-descriptor store so it
  outlives a helper restart. Unread events would survive; what happens to events already read
  but unanswered must be measured first. Expected best case: zeros become a hang.
- **Where:** spec §6.5, §12; `docs/kernel-behavior-7.2.md` §11.6.

### Z2. A placeholder moved into a directory the user just created is not covered
- **Kind** LIMIT · **Evidence** reasoned · **Status** planned (write phase, with the watcher)
- **What:** `mkdir ~/OneDrive/new && mv placeholder new/ && cat new/placeholder` can open the
  file before the helper marks `new/`, and read zeros. Today nothing marks user-made
  directories at all; the watcher will, but a race window remains.
- **Why:** fanotify only offers permission events for open and access. Directory creation and
  rename are reported after the fact, so they cannot be held until the mark is placed.
- **Cost:** zeros, if a program opens the file inside that window. A person will not hit it; a
  script can.
- **Way out:** the watcher narrows it to the helper's event latency. Closing it fully needs a
  permission event for rename or create; whether kernel 7.x has one is unchecked.
- **Where:** spec §5.4, §5.5, §12.

### Z3. A hardlink, or a file moved out of the folder, escapes interception
- **Kind** LIMIT · **Evidence** measured · **Status** planned (watcher)
- **What:** coverage follows names through marked directories. A hardlink to a placeholder in
  an unmarked directory, or a placeholder renamed out of the folder, is opened without
  interception and reads zeros. A second (bind) mount of the same filesystem *is* covered.
- **Why:** marks are placed per directory; the kernel has no subtree marks.
- **Way out:** the watcher should see the link count change (`FAN_ATTRIB` through the source
  directory's mark — reasoned, not verified) and put an individual mark on that one file.
  Files with more than one link are rare, so the memory cost is negligible.
- **Where:** spec §12; `docs/kernel-behavior-7.2.md` §1.

### Z4. A tool that re-sparsifies a downloaded file behind our back
- **Kind** LIMIT · **Evidence** reasoned · **Status** planned (write phase)
- **What:** a downloaded file carries an ignore mark with `FAN_MARK_IGNORED_SURV_MODIFY`, so the
  helper does not see its opens. If an external tool punches holes in it
  (`fallocate --dig-holes`, some deduplication tools), it reads zeros.
- **Why:** without `SURV_MODIFY` the ignore mark cannot be placed at all while the daemon holds
  the file open for writing (measured, kernel document §2.1).
- **Way out:** watch `FAN_MODIFY` on the directory marks and re-check a downloaded file that
  suddenly lost its blocks. The write phase needs modification tracking anyway.

### Z5. Punching a file that still carries an ignore mark
- **Kind** FRAGILE · **Evidence** measured · **Status** mitigated (Ruling H146, commit `7457294`)
- **What:** freeing a file's space while it still carries an ignore mark leaves it empty and
  un-intercepted — zeros forever. Before `SURV_MODIFY`, any write cleared the mark and the
  mistake healed itself; now nothing does.
- **History:** the argument that "a folder registered without interception cannot carry a
  stale mark" was falsified three times, each time by a new race (H132, C2, N2).
- **Mitigation, measured:** the argument is gone. Every punch site now checks on the spot: with a
  helper link it asks the helper to clear the mark and aborts on any failure; if no helper socket is
  bound at all, no mark can exist; if a helper is there but unlinked, it refuses. Proven in the VM on
  three filesystems. The danger itself remains — this is the one operation where a slip is silent
  and permanent — so every new punch site must use the same check.
- **Where:** `crates/konedrived/src/sync/root.rs` (both punch sites); spec §4.4, §8, §12.

### Z6. Lost or hand-edited `config.toml` while the helper holds a root
- **Kind** FRAGILE · **Evidence** reasoned · **Status** open
- **What:** if the daemon's record of an intercepted folder is lost, the daemon no longer knows
  the helper holds it, and the folder could be registered without interception while the
  helper's marks remain.
- **Way out:** on connecting, the daemon asks the helper which roots it holds for this user
  and reconciles. One new protocol message.

### Z7. `UnregisterRoot` is best effort
- **Kind** FRAGILE · **Evidence** reasoned · **Status** open
- **What:** a renamed or moved root, or a per-file failure during the unregistration walk, is
  logged and still answered as success, so marks can be left behind.
- **Mitigation:** the registration walk clears ignore marks, and Z5's local check does not
  depend on unregistration having succeeded.

### Z8. Names too long for Linux
- **Kind** LIMIT · **Evidence** measured on this machine · **Status** planned (read phase)
- **What:** Linux allows 255 **bytes** per name (ext4, Btrfs, XFS alike); OneDrive allows 255
  **characters**. A Cyrillic letter is two bytes in UTF-8, so a name of 128–255 Russian
  characters is valid in OneDrive and cannot be created locally. Measured on `/home`: 127
  Cyrillic characters work, 128 fail with `ENAMETOOLONG`.
- **Plan:** skip such a file and show it in the status with the reason; never drop it silently.
  Showing it under a shortened name is a later decision.

---

## 2. Platform limits that cost function, not data

### P1. Opening through a read-only mount is refused when the helper would be asked
- **Kind** LIMIT · **Evidence** measured · **Status** open — worth removing (see way out)
- **What:** the kernel opens each event's descriptor for writing, on the opener's mount, because the
  daemon writes the content through it. Through a read-only mount — a Flatpak app with `home:ro`,
  `ProtectHome=read-only`, a read-only bind — that open fails, and the kernel answers the event with
  `FAN_DENY` itself: the opener gets `EPERM` in 7–8 ms. Measured on three filesystems.
- **History:** the helper used to treat that failure as fatal and exit, releasing every waiting
  opener as zeros (Z1). Fixed by Ruling H145, commit `28ff3e6`.
- **Cost — wider than it looks:** the refusal hits every file in the folder whose open reaches the
  helper, not only placeholders: the user's own unmanaged files there (never marked, so always
  refused), and **downloaded files without an ignore mark in place**. That is not only a mark the
  kernel reclaimed under memory pressure. Ignore marks live only as long as the helper process, so
  **after every reboot or helper restart there are none**, and the registration walk clears them again
  at every daemon start and helper reconnect (W4). After each of those, a `home:ro` Flatpak app cannot
  open *any* file in the folder until something opens it once through a writable mount. Apps that open
  files through the desktop's file-chooser portal are probably unaffected — the portal opens the file
  from outside the sandbox — but that is not verified. The underlying refusal is measured; the
  consequence after a restart is reasoned from the code and the final review.
- **Way out, reasoned and untested:** open event descriptors read-only, which works on any mount, and
  give the daemon a writable descriptor only when a download needs one — opened by the helper from the
  event's file handle (`open_by_handle_at`) through the root descriptor the daemon registered, which
  lives on a writable mount. The helper's own open would raise an event aimed at itself, so it needs a
  self-exemption. Needs a VM experiment before anyone relies on it.

### P8. An executable kept in the folder cannot run twice at once
- **Kind** LIMIT · **Evidence** measured · **Status** open
- **What:** a second open of a running executable fails the kernel's write-open of the event
  descriptor with `ETXTBSY`; the kernel denies it. An AppImage stored in OneDrive cannot be started a
  second time while it runs. P1's way out would remove this too.
- **The first run can fail too — measured:** the *first* `exec` of an executable without an ignore
  mark fails `ETXTBSY` if it runs before the helper closes its event descriptor. Out of 1000 execs:
  2 failures on ext4, 2 on XFS, 0 on Btrfs. It predates the fix for Z1's exit and cannot produce
  zeros; the run simply fails and succeeds when retried.

### P2. Opening a file during a lease gets `EPERM` instead of waiting
- **Kind** LIMIT (consequence of a WORKAROUND) · **Evidence** measured · **Status** open
- **What:** with non-blocking event descriptors (see W3), the kernel answers an open of a
  leased file with `FAN_DENY` itself, in 7–8 ms.
- **Cost:** a program opening a file in the milliseconds while we free its space, or a file
  some other program holds a lease on, gets "operation not permitted".

### P3. Every directory needs its own mark, so the tree is walked
- **Kind** LIMIT · **Evidence** measured · **Status** open
- **What:** fanotify marks are not recursive, and they die with the helper process. Every
  directory is walked and marked at registration and at every helper start.
- **Cost:** measured 1.03 s on a cold cache for 10 000 directories and 100 000 files; the
  file-level part exists only to clear ignore marks (W4).
- **Way out:** one fanotify group per user. Unregistering becomes closing the group, the
  file-level walk disappears, and one user's leased file stops stalling others. A large
  change to the helper; natural to do with the watcher.

### P4. Kernel memory per directory
- **Kind** LIMIT · **Evidence** measured · **Status** open
- **What:** each directory mark pins an inode in kernel memory: 1658 B per directory measured
  on a 10 000-directory tree, against an earlier isolated measurement of 1148 B. The gap is
  unexplained because the measurement records totals only.
- **Cost:** ~16.6 MB for 10 000 folders. Marking files instead would cost ~330 MB for 200 000.

### P5. Only a fixed set of errnos can reach an application
- **Kind** LIMIT · **Evidence** measured · **Status** open
- **What:** the kernel accepts only `0, EPERM, EIO, EAGAIN, EBUSY, ETXTBSY, ENOSPC, EDQUOT` in
  a denial. A missing file in the cloud, a dropped connection or a timeout all reach the
  application as `EIO`.

### P6. Eager hydration
- **Kind** LIMIT · **Evidence** measured · **Status** open
- **What:** anything that opens a placeholder downloads it — thumbnailers, indexers, even
  `open(O_TRUNC)`. There is no way to tell a thumbnailer from a user.

### P7. Items in OneDrive that are not synced
- **Kind** LIMIT · **Evidence** reasoned · **Status** planned (read phase)
- **What:** the Personal Vault (closed behind a separate sign-in), shared folders added with
  "Add to my OneDrive" (they live on someone else's drive), and OneNote notebooks (not files)
  are skipped and shown in the status.

---

## 3. Workarounds we built

### W1. The no-interception mode
- **What:** a folder can be registered with no helper at all. Nothing is filled on open; files
  read as zeros until explicitly downloaded.
- **Why:** the helper is not installed on the development machine, and the command-line path
  has to work there.
- **Fragility:** its safety rests on Z5's check.

### W2. The read-only folder in the read phase (planned)
- **What:** files `r--r--r--`, directories `r-xr-xr-x`, so nothing can diverge from the cloud
  and there are no conflicts. The daemon lifts write permission only for the moment of its
  own operation — setting `user.*` attributes needs write permission even for the owner.
- **Weak spot:** a program that opens a file for writing inside that microsecond window keeps
  a writable descriptor. The lock is the rule the user sees; data safety rests on a check
  before every cloud change is applied (the file must still be what we downloaded).

### W3. Non-blocking event descriptors
- **What:** the event loop would otherwise freeze for up to 45 s whenever anyone held a lease
  on any file in a marked directory — measured, and triggerable by any local user.
- **Cost:** P2.

### W4. The registration walk clears ignore marks on every file
- **What:** defence in depth after the stale-mark argument failed. Makes registration cost one
  lookup per file (P3).

### W5. A credit limit between helper and daemon
- **What:** at most 64 hydrations in flight per connection, equal to the daemon's queue depth.
  Without it, the two bounded queues deadlocked under a burst (measured). Beyond the credit,
  requests queue in the helper and the application waits.

### W6. Silence, not a blocked send, ends a connection
- **What:** a daemon connection is torn down only when the daemon has been silent for a whole
  liveness window, not when one send blocks — a slow daemon under a burst is not a dead one.

### W7. Lease tests run in threads, not subprocesses
- **What:** subprocess-based lease tests failed intermittently. The explanation first given
  (that forking breaks leases) was disproved by measurement; the real cause was never found.

### W8. Fault injection lives behind a cargo feature
- **What:** the helper has two panic hooks and the daemon one stall the VM suite needs. They are
  compiled only with the `fault-injection` feature; the release binaries contain none, checked with
  `strings`.

### W9. The helper clears an ignore mark for whoever owns the file
- **What:** `ClearIgnore` is authorised by file ownership, not by owning a registered root, so a
  folder registered without interception can still ask before punching (Z5).
- **Cost:** a user can clear marks on their own files. The worst case is extra interceptions of those
  files; it cannot produce zeros or touch anyone else's.

### W10. Recovery does not wait for a busy file
- **What:** recovery takes the per-inode lock without waiting; a file whose lock is held — typically
  one an older connection is still downloading — is counted `busy` and handled on the next pass.
  Waiting would make every reconnect wait for every download.

---

## 4. Fragile spots

- **F3. Long walks behind short timeouts** — the walk grows with the number of files and
  `RegisterRoot` has a 120 s bound; `HydrateDone` calls queued behind a walk have only 30 s.
  Open.
- **F4. Startup holds the lifecycle lock through the helper's walk** — register, forget,
  dehydrate and populate wait behind it, up to 120 s. Status queries do not. Open.
- **F5. During a transient connection only the top of the stack is exempt** — the live
  daemon's own opens are intercepted meanwhile. Only startup recovery relies on the exemption.
  Open.
- **F6. A dying transient connection's queued jobs are denied `EIO`** — they were never sent
  and could be moved to the live connection underneath. Open.
- **F7. Under descriptor exhaustion, a filled file can be denied `EIO`** — the helper re-reads
  its state through a duplicate descriptor and cannot when none is free. Never zeros. Open.
- **F8. The offline content source is held in memory** — after a daemon restart, downloads
  fail until the source directory is populated again. Goes away with the real OneDrive
  source.
- **F9. A download whose name is removed mid-flight** completes into the unlinked file and
  reports success about a name that no longer exists.
- **F10. A degraded root** (a directory the helper could not mark) is logged, not shown over
  D-Bus.
- **F11. A helper that is running but unreachable blocks the no-interception folder** — if the
  daemon cannot connect (the per-user connection cap is held, the protocol versions differ), that
  folder cannot free up space, recovery defers its files, and anything not `online-only` cannot be
  re-filled. Every case is refused, none is unsafe.
- **F12. "Is a helper running" assumes the daemon sees the helper's `/run`** — the check is whether
  a socket is bound at the helper's path. A daemon in a different mount namespace would conclude
  there is no helper.

---

## 5. Provisional numbers

| Constant | Value | State |
|---|---|---|
| Worker pool / event queue | 64 / 1024 | measured comfortable at 3000 concurrent opens |
| Outbox depth | 256 | measured as the binding constraint, correctly |
| Credit per connection | 64 | equal to the daemon's queue depth; pinned by a test |
| Waiters per user | 8 | measured binding |
| Waiters in total | 32 | **guess** — a single-user machine never reaches it |
| Liveness window | 60 s | **guess** — nothing in the suite reaches it |

---

## 6. Quality debt

- **D1.** `cargo fmt --check` has never been clean and there is no `rustfmt.toml`.
- **D2.** The fake helper used by tests exists twice, in `konedrivectl` and `konedrived`.
- **D3.** Three small test gaps from the D-Bus review (named I1, R1, W5 there) were never
  closed: none guards data, each pins an existing behaviour.
- **D4.** In the no-interception mode, `sync status` says the same thing twice (the CLI's
  `Opens:` line and the daemon's `LastError`).
- **D5.** Freeing up an already freed file exits non-zero instead of succeeding idempotently.
- **D6.** Two daemons writing one file after a `chown` in the middle of a download have no
  shared lock. Needs root to trigger.
- **D7.** The crash consistency of placeholder creation has not been reviewed.
- **D9.** The notification watcher of spec §5.4/§5.5 was never built; planned for the write
  phase.
- **D10.** One unsuitable file in a populate source fails the whole populate, with a message naming
  it, rather than skipping that file.

---

## 7. Dolphin integration

The two plugins in `dolphin/`: emblems for each file's state, and "Download" / "Free up space" in the
context menu. They read a file's state from its extended attribute and never open it.

- **K1. Dolphin opens some files itself, and that downloads them.** LIMIT · measured in KIO,
  reasoned for Dolphin · planned (read phase, with thumbnails). To draw a preview, and to detect the
  type of a file whose name has no extension or an unclear one, Dolphin opens it — measured for
  `data.bin` and `noextension`, not for `.pdf` or `.txt`. Such files download just by being shown.
- **K2. No emblems in search results or Recent Files.** LIMIT · reasoned. Those views do not use
  `file://` URLs, and the emblem plugin is given only the URL. The menu actions still work there.
- **K3. Live updates cover the 256 most recently shown folders.** FRAGILE · bound measured. A folder
  shown earlier stops updating until Dolphin asks about it again. With inotify unavailable there is no
  cache at all and every file walks up its ancestors; with inotify watches exhausted, emblems still show
  but stop updating, and a warning is logged once.
- **K4. Unverified: an emblem after a download triggered by opening a file.** Reasoned. The daemon then
  writes the state through the helper's descriptor, and it is not yet confirmed that inotify reports that
  to the plugin. Needs root, so it belongs in the VM suite. If not, only those emblems stay stale until
  Dolphin refreshes.
- **K5. Reading state on Dolphin's UI thread.** Measured: 6.7 µs per file inside a sync folder, 0.55 µs
  outside. On a hung network mount it blocks exactly as a `stat` would.
- **K6. At most 1000 calls wait at once, per window.** DEBT · measured. A file already waiting is never
  sent twice, and a selection of more than 1000 files takes several clicks. A never-answering daemon costs
  up to ~3 MB of Dolphin memory per window, and on `dbus-daemon` buses (not Fedora's `dbus-broker`) those
  calls use Dolphin's own reply budget. The waiting set is per window, so two windows can each send the
  same file once. A batch method on the daemon would remove all of this.
- **K7. A root mark set or removed by hand.** Reasoned. The stale "not in a root" answer lasts until the
  folder is evicted from the cache or Dolphin restarts, including when the mark is on an unwatched
  ancestor; after a mark is removed and restored, emblems come back only when Dolphin asks again.
  Registering through the daemon is unaffected.
- **K8. Messages can be lost.** Reasoned, and confirmed in source for the desktop. A failure message is
  dropped if Dolphin rebuilds the plugin before the daemon answers (the work still happens), and the
  Plasma desktop, which also hosts the menu plugin, never shows its messages at all.
- **K9. A renamed folder that Dolphin immediately asks about stops updating live.** Reasoned.
- **K10. An unrecognised state value** shows no emblem and no actions. Reasoned.
- **K11. After a failed on-demand start** the message tells the user to start the daemon by hand.
- **K12. Cosmetic:** the "already waiting" and "too many" notes appear in Dolphin's red error bar.
- **K13. Build assumptions:** the README's `QT_PLUGIN_PATH` line assumes `lib64`; the minimum KF/Qt 6.8
  matches `app/`, but only KF 6.30 with Qt 6.11 was built and tested; the plugin tests' private buses use
  the stock 50 000 pending-reply limit rather than `dbus-daemon`'s bare default of 128.
- **K14. Memory:** about 100–150 bytes per remembered file, ~10–15 MB for a 100 000-file folder.
  Reasoned.

---

## Closed

Kept briefly so the history of a weak spot is findable; details are in the commits.

- **Recovery racing a download after a reconnect** could punch a file just filled and marked —
  fixed by Ruling H147, commit `86dbf93`.
- **A populate source leading into the folder** could fill a placeholder with zeros stamped as
  downloaded — fixed by Ruling H148, commit `3d5c183`.
- **The helper exiting on an event the kernel could not hand over** — fixed by Ruling H145, commit
  `28ff3e6`; what remains is P1 and P8.
- **The kernel document's header named only kernel 7.2.5** — corrected with this log's first commit.

# Limitations, workarounds and weak spots

One place for everything in konedrive that is limited, worked around, fragile, or knowingly
below the quality we want. It is kept current: an entry is added whenever a decision accepts
a limitation, builds a workaround, picks a number without measuring it, or parks a finding.
Detail lives elsewhere (`docs/design/`, `docs/kernel-behavior-7.2.md`, the code); this log is the
index of what is weak and why.

**Kinds.** LIMIT — imposed by the kernel or the platform; we cannot change it, only live with
it. WORKAROUND — something built to route around a limit. FRAGILE — works, but rests on
something delicate. PROVISIONAL — a number chosen, not measured. DEBT — knowingly below the
quality we want.

**Evidence.** *measured* — observed in a test or in a run on real hardware. *reasoned* — argued from
the code or the documentation and not yet observed. This project has shipped plausible
mechanisms that turned out false; treat *reasoned* entries accordingly.

**Status.** *open*, *mitigated* (made rare or loud, not gone), *planned* (with where), *fix in
progress* (with the commit).

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
- **Cost:** any application waiting for a download at the moment the helper dies gets zeros. The
  credit-gated queue (`docs/design/hydration.md` §10.3) raised the exposure: every waiting opener is
  suspended now, not at most 64 per connection.
- **Mitigation:** "the helper must not die" is a design requirement (`docs/design/hydration.md`
  §13): panics are contained on worker and connection threads, `EMFILE` is survivable, disconnects
  are handled — all proven in the VM suite.
- **Also:** updating the helper restarts it and uninstalling it stops it —
  `scripts/install-helper.sh` says so before it asks. Upgrading or removing the `konedrive`
  package does the same, without a warning (R1, R3). `--uninstall` refuses while the helper's
  `/var/lib/konedrive/roots.json` lists a folder (run `konedrivectl sync forget` first; `--force`
  overrides): without the helper that folder's placeholders read as zeros, and its Forget is
  refused `NoHelper`.
- **Way out, unverified:** hand the group's descriptor to systemd's file-descriptor store so it
  outlives a helper restart. Unread events would survive; what happens to events already read
  but unanswered must be measured first. Expected best case: zeros become a hang.
- **Where:** `docs/design/hydration.md` §13, §16; `docs/kernel-behavior-7.2.md` §11.6.

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
- **Where:** `docs/design/hydration.md` §3 (M1, M4), §16.

### Z3. A hardlink, or a file moved out of the folder, escapes interception
- **Kind** LIMIT · **Evidence** measured · **Status** planned (watcher)
- **What:** coverage follows names through marked directories. A hardlink to a placeholder in
  an unmarked directory, or a placeholder renamed out of the folder, is opened without
  interception and reads zeros. A second (bind) mount of the same filesystem *is* covered.
- **Why:** marks are placed per directory; the kernel has no subtree marks.
- **Way out:** the watcher should see the link count change (`FAN_ATTRIB` through the source
  directory's mark — reasoned, not verified) and put an individual mark on that one file.
  Files with more than one link are rare, so the memory cost is negligible.
- **Where:** `docs/design/hydration.md` §16; `docs/kernel-behavior-7.2.md` §1.

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
- **Kind** FRAGILE · **Evidence** measured · **Status** mitigated (commit `7457294`)
- **What:** freeing a file's space while it still carries an ignore mark leaves it empty and
  un-intercepted — zeros forever. Before `SURV_MODIFY`, any write cleared the mark and the
  mistake healed itself; now nothing does.
- **History:** the argument that "a folder registered without interception cannot carry a
  stale mark" was falsified three times, each time by a new race (an idle file carried
  across a re-registration, a fill finishing after a Forget, a file renamed past the registration
  walk).
- **Mitigation, measured:** the argument is gone. Every punch site now checks on the spot: with a
  helper link it asks the helper to clear the mark and aborts on any failure; if no helper socket is
  bound at all, no mark can exist; if a helper is there but unlinked, it refuses. Proven in the VM on
  three filesystems. The danger itself remains — this is the one operation where a slip is silent
  and permanent — so every new punch site must use the same check.
- **Where:** `crates/konedrived/src/sync/root.rs` (both punch sites); `docs/design/hydration.md` §3
  (M3), §8, §9.

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
- **Kind** LIMIT · **Evidence** measured on Btrfs · **Status** mitigated
- **What:** Linux allows 255 **bytes** per name (ext4, Btrfs, XFS alike); OneDrive allows 255
  **characters**. A Cyrillic letter is two bytes in UTF-8, so a name of 128–255 Cyrillic characters
  is valid in OneDrive and cannot be created locally. Measured on Btrfs: 127 Cyrillic characters
  work, 128 fail with `ENAMETOOLONG`.
- **Mitigation:** built as `SkipReason::NameTooLong` (`crates/konedrived/src/tree.rs`) — such a
  file is skipped, never created with a zero-length or truncated name, and listed visibly by
  `Skipped()` / `konedrivectl sync skipped` with its reason. Showing it under a shortened name
  is a later decision.

---

## 2. Platform limits that cost function, not data

### P1. Opening through a read-only mount is refused when the helper would be asked
- **Kind** LIMIT · **Evidence** measured · **Status** open — worth removing (see way out)
- **What:** the kernel opens each event's descriptor for writing, on the opener's mount, because the
  daemon writes the content through it. Through a read-only mount — a Flatpak app with `home:ro`,
  `ProtectHome=read-only`, a read-only bind — that open fails, and the kernel answers the event with
  `FAN_DENY` itself: the opener gets `EPERM` in 7–8 ms. Measured on three filesystems.
- **History:** the helper used to treat that failure as fatal and exit, releasing every waiting
  opener as zeros (Z1). Fixed in commit `28ff3e6`.
- **Cost — wider than it looks:** the refusal hits every file in the folder whose open reaches the
  helper, not only placeholders: the user's own unmanaged files there (never marked, so always
  refused), and **downloaded files without an ignore mark in place**. That is not only a mark the
  kernel reclaimed under memory pressure. Ignore marks live only as long as the helper process, so
  **after every reboot or helper restart there are none**, and the registration walk clears them
  again at every daemon start and helper reconnect (W4). After each of those, a `home:ro` Flatpak
  app cannot open *any* file in the folder until something opens it once through a writable mount.
  Apps that open files through the desktop's file-chooser portal are probably unaffected — the
  portal opens the file from outside the sandbox — but that is not verified. The underlying refusal
  is measured; the consequence after a restart is reasoned from the code.
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
- **Cost, seen in practice:** an editor or IDE that opens many files as soon as a folder is
  opened (VS Code indexing a project, say) triggers a download for each placeholder it touches.
  Each first open is its own Graph round trip — there is no bulk fill — and `serve_hydrations`
  (`crates/konedrived/src/sync/mod.rs`) runs at most 4 of them at once, so opening a large,
  mostly-online-only folder in such a program can take a while before everything it wanted is on
  disk; the rest simply queue. Also seen: browsing a git repository stored in the folder
  downloads its `.git/objects/*` blobs the same way — Dolphin's Git plugin (and some editors'
  own VCS integration) runs `git status` and similar commands on open, each of which reads
  object files. Dolphin's Git plugin can be switched off (Configure Dolphin → General →
  Behavior), at the cost of its version-control emblems.

### P7. Items in OneDrive that are not synced
- **Kind** LIMIT · **Evidence** measured (a real-account run) · **Status** mitigated
- **What:** six kinds of item never make it into the folder, each with its own `SkipReason`
  (`crates/konedrived/src/tree.rs`, `skip_reason`):
  - the Personal Vault (closed behind a separate sign-in, `specialFolder.name == "vault"`);
  - a shared folder added with "Add to my OneDrive" (`remoteItem`: it lives on someone else's
    drive);
  - a OneNote notebook — any item with a `package` facet counts as one, whatever its
    `package.type`;
  - an item named with the `.konedrive-` prefix, which would collide with the daemon's own
    working names;
  - a name too long for Linux (Z8);
  - `unsupported`: an item that is neither a file nor a folder, or whose id or name could not be
    a name in a directory (empty, `.`, `..`, or holding `/` or NUL).

  All six are skipped, never silently dropped, and listed by `Skipped()` /
  `konedrivectl sync skipped` with why.
- **Personal Vault detection, verified:** a real-account run (G1) asserted that no folder literally
  named "Personal Vault" was ever placed as an ordinary folder — confirming `specialFolder.name ==
  "vault"` is what actually distinguishes it, not the name, which a user could give to any folder.

### P9. `Skipped()` returns the whole list in one D-Bus message
- **Kind** LIMIT · **Evidence** reasoned · **Status** open
- **What:** `Sync1.Skipped()` answers with every skipped item at once, as one method reply — bounded
  by zbus's 128 MiB message limit, not paged. What is in it is also coarser than "every OneDrive
  item that did not make it into the folder": an item inside a skipped folder is not listed of its
  own accord (its own folder's entry already covers it, per the method's own doc comment in
  `dbus/org.konedrive.Sync1.xml`), so entries are only the top of each skipped subtree, not
  everything under it.

### P10. Pinning a big folder downloads everything in it, with no prompt
- **Kind** LIMIT (chosen) · **Evidence** reasoned · **Status** open
- **What:** "Always keep on this device" on a folder queues every online-only file under it, and
  everything OneDrive adds there later, and downloads them four at a time (`PIN_SLOTS`,
  `crates/konedrived/src/sync/pin.rs`), in slots of their own beside the four fills served on
  open. Nothing asks first, and nothing checks the free space beforehand.
- **Why:** the product decision was no size prompt, as on Windows.
- **Cost:** a pinned folder bigger than the free space fills the disk. The first download that
  fails for want of space is a `failed` event reading "not enough disk space" (the window
  notifies), and the rest of the queue is dropped until the next sweep (F38) rather than failing
  file by file. A download that fails for another reason — no network, say — is one `failed` event
  per file, each of which the window may notify, and the queue goes on to the next file.
- **Where:** `docs/design/pinning.md` §4.

---

## 3. Workarounds we built

### W1. The no-interception mode
- **What:** a folder can be registered with no helper at all (`konedrivectl sync
  register-without-interception`). Nothing is filled on open; files read as zeros until explicitly
  downloaded. It is a developer's mode, reached only from the command line: the window no longer
  offers it (`docs/design/decisions.md`, "The window registers a folder only with the helper"). And
  it is always a local folder, filled with `populate-from`, whoever is signed in
  (`docs/design/sync.md` §3): a OneDrive folder is kept in step only with the helper. A OneDrive
  folder an older daemon registered this way is not kept in step at all — no listing, no change
  applied, `RootState` `error` with how to start the helper — until the helper connects; then it
  switches as below, whatever it was recorded as.
- **Why:** development and the test suites need a sync folder on a machine where no helper runs.
  A user installs the helper instead (`scripts/install-helper.sh`).
- **Fragility:** its safety rests on Z5's check.
- **Installing the helper later** (measured with a fake helper and in the VM suite, "a
  folder registered without the helper switches to interception when the helper starts": zeros
  before the helper runs, the directory mark and the real content after): a folder
  registered this way *because no helper was connected* switches to interception when the helper connects, with no Forget. The daemon stops the folder's
  sync, writes the switch to `config.toml`, has the helper register the root (its walk marks every
  directory: P3's cost, once), runs recovery, and starts the sync again, intercepted. Content placed
  before the switch is marked after it, as at every restart. A folder registered this way *while a
  helper was connected* was a choice and stays. A failed switch leaves the folder as it was, says why
  in `LastError`, and is tried again at the next connect (but see F30).
- **Cost:** a `config.toml` written by an older daemon cannot say which of the two a folder was, and
  is read as one to switch (`sync_root_upgrade_when_helper` missing): a folder registered without
  interception on purpose, before this, switches once too.

### W2. The read-only folder in the read phase
- **What:** files `r--r--r--`, directories `r-xr-xr-x`, so nothing can diverge from the cloud and
  every local edit becomes a rescued conflict rather than silent drift. How: the kernel opens the
  event descriptor with no permission check, and `pwrite`/`ftruncate`/ `fallocate`/`futimens` by the
  owner already work on a `0444` file through an already-open descriptor — but a `user.*` attribute
  write needs inode write permission even for the owner (measured on Btrfs: `setfattr` fails
  `EACCES` on `0444`, succeeds after `chmod 0644`), and creating an entry in a `0555` directory
  fails `EACCES` too. `konedrive-fs` lifts the owner's write bit around each attribute write and
  restores it; a file that must be opened writable (dehydration, recovery, a content replacement) is
  opened read-only and reopened through `/proc/self/fd/<n>` with the bit lifted for that one open;
  directories get the same per-operation window (`docs/design/sync.md` §11). Applies to a folder
  that shows OneDrive; a local folder (F20) is not locked. Forget takes the lock off (F19).
- **Weak spot:** a program that opens a file for writing inside such a window keeps a writable
  descriptor until the next Full reconcile after a crash. The lock is the rule the user sees; data
  safety actually rests on the stamp check (`docs/design/sync.md` §10) before every cloud change is
  applied (the file must still be what we downloaded) — a changed file fails that check and is
  rescued to the rescue directory rather than overwritten or lost.

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
- **What:** the helper has two panic hooks and one stall (`KONEDRIVE_FAULT_PANIC_ON_SIZE`,
  `KONEDRIVE_FAULT_PANIC_ON_MARKFILE`, `KONEDRIVE_FAULT_DELAY_IGNORE_MS`), and the daemon one
  stall, that the VM suite needs. They are compiled only with the `fault-injection` feature; the
  release binaries contain none, checked with `strings`, and `scripts/install-helper.sh` refuses a
  helper that contains `KONEDRIVE_FAULT_`.

### W9. The helper clears an ignore mark for whoever owns the file
- **What:** `ClearIgnore` is authorised by file ownership, not by owning a registered root, so a
  folder registered without interception can still ask before punching (Z5).
- **Cost:** a user can clear marks on their own files. The worst case is extra interceptions of those
  files; it cannot produce zeros or touch anyone else's.

### W10. Recovery does not wait for a busy file
- **What:** recovery takes the per-inode lock without waiting; a file whose lock is held — typically
  one an older connection is still downloading — is counted `busy` and handled on the next pass.
  Waiting would make every reconnect wait for every download.

### W11. `Dev1.AccessToken` and `konedrivectl dev export-access-token`
- **What:** for a VM test run that needs to speak Graph itself without a full sign-in of its own.
  `Dev1` hands out the daemon's current *access* token — about an hour of `Files.Read` — never the
  refresh token, which never leaves the daemon/KWallet. **`Dev1` is served in every build**, not
  only a development one: a per-user install (`scripts/dev-install.sh`) needs it for real-account
  test runs. The CLI writes the token through a temporary file in the same directory as `--out`,
  created with `O_CREAT | O_EXCL | O_NOFOLLOW` at mode 0600 from the instant it exists, then
  `fsync`ed and renamed over `--out` — so the bytes are never observable at a looser mode, a symlink
  at `--out` is replaced rather than written through, and a reader who already had the old `--out`
  open keeps reading its old content untouched (an earlier open-and-truncate version followed a
  symlink and disturbed an existing reader).
- **Cost:** any process running as the same Linux user that can reach the session bus can call
  `Dev1.AccessToken()` and obtain about an hour of read access to the signed-in account's OneDrive.
  This is **the same access that process already has** by opening any file inside the sync folder
  directly — `Dev1` does not open a door that was not already open, only makes going through it
  faster and without a helper in the way. It carries no authorisation of its own beyond being the
  same Linux user. A Flatpak app is not "any same-user process" here: its own bus proxy (the
  portal's D-Bus filtering) decides whether it can reach `org.konedrive.Daemon` at all, same as for
  every other interface this daemon serves.

### W12. One skip-reason wording, kept identical in Rust and C++ by a test
- **What:** `konedrivectl::skip_reason_text` (`crates/konedrivectl/src/lib.rs`) and the window's
  `whyText` (`app/synccontroller.cpp`) must say exactly the same sentence for each `Skipped()`
  reason, so a person reading `konedrivectl sync skipped` and a person reading the window are told
  the same thing. There is no single source of truth the two are generated from; instead,
  `crates/konedrivectl/tests/sync_cli.rs::skip_reason_text_matches_every_branch_of_the_windows_whytext`
  parses each reason → sentence branch out of `app/synccontroller.cpp`'s source and compares it
  against `skip_reason_text` for that same reason directly, both directions — not only "does the
  Rust sentence appear somewhere in the C++ file", which two swapped branches would still pass.
- **Cost:** the two copies can still drift for one build if someone edits one file and does not run
  `cargo test -p konedrivectl` before committing; nothing stops the C++ side compiling on its own
  with a changed sentence. This project has no CI; the test only catches the drift for whoever runs
  it, and only then.

### W13. Day-to-day VM runs cover btrfs only
- **What:** `tests/vm/run.sh quick` runs the end-to-end suite on btrfs (Fedora's default for
  `/home`) in one VM; `tests/vm/run.sh full` runs btrfs, ext4 and xfs in three VMs at once. The
  routine run is `quick`; `full` is a separate, deliberate run, not a step of every change. The
  three filesystems in sequence took about 5.5 of the suite's 6.5 minutes.
- **Cost:** a regression that shows only on ext4 or xfs surfaces only when `full` is run, not in the
  change that caused it. The three parallel VMs of `full` were started only with a one-scenario
  smoke run so far; whether any timing-sensitive check turns flaky with three VMs sharing the host's
  CPUs is untested.

### W14. A OneDrive folder is excluded from KDE's Baloo indexer
- **What:** `crates/konedrived/src/sync/baloo.rs` (`Baloo::exclude`/`include_again`/`is_excluded`,
  `balooctl6 config add/rm excludeFolders`) keeps Baloo from reading every placeholder's content to
  index it — which would download the whole drive, in the background, the first time Baloo walks the
  folder. A OneDrive folder gets the exclusion only after checking, read-only, that the folder (or a
  directory above it) is not excluded already; a user's own exclusion, or a parent directory's, is
  never added on top of and never removed by a Forget. That check reads Baloo's settings file itself
  — `${XDG_CONFIG_HOME:-~/.config}/baloofilerc`, `[General]`, `exclude folders` with or without
  `[$e]`, comma-separated, `$VAR`s expanded, compared by path components — because `balooctl6 config
  list excludeFolders` was observed printing an empty list while the file held an exclusion: every
  user exclusion read as absent, and a Forget could have taken one off. It is asked at every
  bring-up of a folder `config.toml` does not record as excluded by this daemon, not only at a fresh
  registration: an exclusion that failed, timed out, was cut off by a kill before it was recorded,
  or belonged to a registration kept after it failed — or a Baloo installed later — is caught up
  then. Whether *this* daemon is the one that added it is persisted (`sync_root_baloo_excluded` in
  `config.toml`), so a restart between a registration and its Forget still gets the Forget right.
  `SyncService` starts with a `Baloo` that runs no program and reads no file (`Baloo::disabled`);
  only `main` installs the real `balooctl6` and `baloofilerc` (`Baloo::default`). Every call runs
  under a 10 s timeout (`Baloo::timeout`, settable in tests) with `kill_on_drop(true)` on the child,
  so a hung `balooctl6` — blocked on an unresponsive Baloo D-Bus service, say — is killed and
  treated like a missing program rather than blocking registration or Forget forever (both run under
  `SyncService`'s lifecycle write lock).
- **Cost:** no Baloo file-name search inside the folder — KRunner and Dolphin's own search (both built
  on Baloo) do not find files there. Content search inside the folder was already lost to K1.
  Reading `baloofilerc` by hand is FRAGILE: it follows KConfig's format as Baloo writes it today
  (a `$(command)` in the value is never run, so it matches nothing), and a system-wide exclusion in
  `/etc/xdg/baloofilerc` is not read. Where Baloo is missing, every bring-up spends one failed
  program start finding out.
- **Where:** `crates/konedrived/src/sync/baloo.rs`; `SyncService::commit`, `SyncService::unregister_root`
  (`crates/konedrived/src/sync/mod.rs`).

### W15. A real-account VM run lists the whole drive, but downloads only in one named folder, capped in size
- **What:** `tests/vm/graph.rs` runs the end-to-end suite against a real OneDrive account
  over real Graph, given a short-lived read-only token (`konedrivectl dev export-access-token`,
  W11). What such a run touches:
  - **The listing is the whole drive.** G1 lists the drive and places every item as a placeholder
    in the guest (names and sizes, no content), because `DriveClient` has no folder-scoped delta:
    it lists a drive only from its root.
  - **Downloads and checks are scoped.** `--graph-folder <path in OneDrive>` is required with
    `--graph-token` ("a real run must be scoped to one folder, not the whole drive"), and must name
    a real folder: an empty path or one with `..` is refused, a leading `/` is dropped, at least
    one component is needed, and a run in which no placed item lies inside it FAILS rather than
    skipping. Every file a scenario opens or downloads is inside that folder and no larger than
    `--graph-max-bytes` (optional, default 32 MiB).
  - **No thumbnails.** The thumbnail filler does not run in graph mode. Before, it fetched Graph's
    thumbnail of every image and video in the drive.
  - **G3 and G4 only on request.** A dropped connection (G3) and a resume after a restart (G4) run
    against the real account only with `--graph-resume-checks`. By default only G1 (listing) and
    G2 (open, download, verify) run; G1 waits for the listing cycle to finish (`LastChecked`, F32)
    rather than polling a snapshot.
  - The exported token file is deleted after every run, by hand.
- **History:** the first real-account runs, before any scope or cap existed, downloaded far more
  real content than verification needed, and one hung on the F32 gap. G1 and G2 have since run to
  completion against a real account and pass; G3 and G4 have run only in the wiremock suites.
- **Why:** the daemon speaks only GET to Graph and the token is `Files.Read`, so nothing can be
  written to the cloud this way — but an unscoped run can still read, and so download, far more of
  a real person's drive than a test needs.
- **Cost:** every real run still lists the whole drive: minutes for a drive of tens of thousands of
  items, and a placeholder for each inside the guest. A folder-scoped delta in `DriveClient` would
  remove that; it is not built. The whole-drive listing logic is covered by the mock-server suites
  (`tests/vm/scenarios.rs`), and against the real account only by G1.

### W16. The helper's unit is hardened, and runs under systemd only in a VM check you start by hand
- **What:** `packaging/systemd/konedrive-helper.service` adds to its first sandbox
  (`ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateNetwork=yes`, two capabilities):
  `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`,
  `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `LockPersonality`,
  `MemoryDenyWriteExecute`, `RestrictRealtime`, `PrivateTmp`, `SystemCallArchitectures=native`,
  and a deny-list,
  `SystemCallFilter=~@mount @swap @reboot @raw-io @module @clock @debug @obsolete @cpu-emulation`,
  answering `EPERM`. Without `@mount` in it, the helper's `CAP_SYS_ADMIN` could remount its
  read-only view read-write. `systemd-analyze security` rates the unit 2.4 (OK), down from 5.8
  (MEDIUM); `systemd-analyze verify` passes. SECURITY.md says what this stops and what it does
  not.
- **Tested under systemd:** `tests/vm/run.sh unit` (`tests/vm/helper_unit_test.sh`) boots the guest
  with systemd as PID 1. It installs this unit file byte for byte and a release helper built
  without `fault-injection`, in the guest only, and starts the unit. It checks that the unit is
  active, has its seccomp filter, `no_new_privs` and exactly its two capabilities, and is still
  the same process at the end. A uid-1000 daemon then connects to the unit's socket and
  registers a folder on btrfs. It `MarkDir`s a `0700` subdirectory. Two `0600` placeholders,
  opened from another process, are suspended, hydrated through the daemon, ignore-marked, and
  not asked for again. The check then restarts the helper. The startup walk marks the earlier
  folder again, and a new placeholder in it is intercepted. The second pass adds one drop-in,
  `SystemCallErrorNumber=kill`. Any call the deny-list refuses would then kill the helper with
  `SIGSYS`, and systemd would log it (a control unit shows that it does). None did. Measured
  2026-09-25, kernel 7.2.7, systemd 259.
- **Found by it, fixed:** `RestrictSUIDSGID=yes` stopped all interception. Seccomp cannot read
  `openat2()`'s argument struct, so the filter systemd installs for that setting fails every
  `openat2()` with `ENOSYS`. The helper opens registered folders, at registration and at every
  start, and walks them only through `openat2()` with `RESOLVE_BENEATH`. So every
  `RegisterRoot` was refused (`EINVAL`; the journal says `Function not implemented`). Every
  startup walk would have covered nothing. All the while, `systemctl status` showed the helper
  active and well. The acceptance check's `Helper: failed` would not have caught it. Bisected in
  the VM: turning off any other directive alone changed nothing, and turning off this one alone
  fixed it. The unit no longer sets it. Cost: the helper may again create setuid and setgid
  files where it can write, as before the hardening. That means `/var/lib/konedrive` (0755) and
  `/run/konedrive` (on a `nosuid` `/run`). `StateDirectoryMode=0700` would close the first to
  other users; not tried.
- **Fragile:** the check is its own mode, not part of `quick`, so nothing runs it unless someone
  asks. Run it whenever the unit changes or the helper starts using a new syscall. The guest is
  not a real machine:
  - SELinux is off (`selinux=0`). With the host's enforcing policy over the unlabelled virtiofs
    root, systemd cannot mount `/run` and freezes.
  - There is no audit daemon, so a seccomp denial leaves no audit record. An `EPERM` denial
    under the shipped unit leaves no trace at all, which is why the `kill` pass exists.
  - The folder is on a loop-mounted btrfs under `/mnt`, not under `/home`. `ProtectHome=read-only`
    is applied, but no registered folder lives beneath it.
  - The VM uses QEMU's standard machine (`--disable-microvm`), because microvm hangs at boot
    under the 7.2.7 host kernel.

  Step 2 of `docs/acceptance-check.md` stays the run on a real machine.
- **Left out, not yet tried under systemd:** a `SystemCallFilter` allow-list, `PrivateDevices=`,
  and a non-root `User=`. Each would narrow the helper further, and `tests/vm/run.sh unit` is now
  the place to try them.
- **Cost of `PrivateTmp=yes`:** the helper has its own `/tmp` and `/var/tmp`, and it finds a
  folder by its path, at registration and at every restart. A folder under `/tmp` or `/var/tmp`
  therefore cannot be registered with the helper (`EINVAL`: the path does not lead back to the
  directory). Reasoned.
- **Status:** mitigated. The unit is measured under systemd, in a VM, when someone runs the
  check.

---

## 4. Fragile spots

- **F3. Long walks behind short timeouts** — the walk grows with the number of files and
  `RegisterRoot` has a 120 s bound; `HydrateDone` calls queued behind a walk have only 30 s.
  Open.
- **F4. Startup holds the lifecycle lock through the helper's walk** — register, forget,
  dehydrate and populate wait behind it, up to 120 s. Status queries do not. A switch to
  interception (W1) holds it the same way, through the same walk. Open.
- **F5. During a transient connection only the top of the stack is exempt** — the live
  daemon's own opens are intercepted meanwhile. Only startup recovery relies on the exemption.
  Open.
- **F6. A dying transient connection's queued jobs are denied `EIO`** — they were never sent
  and could be moved to the live connection underneath. Open.
- **F7. Under descriptor exhaustion, a filled file can be denied `EIO`** — the helper re-reads
  its state through a duplicate descriptor and cannot when none is free. Never zeros. Open.
- **F8. The offline content source is held in memory** — after a daemon restart, downloads
  fail until the source directory is populated again. Still true of a local folder (F20); a
  folder that shows OneDrive downloads from the drive, set again whenever its sync starts.
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
- **F13. A file stuck mid-download or mid-free-up makes every sync cycle a Full reconcile** — a
  reconcile leaves such a file for later and asks for a Full reconcile, so the whole folder is
  scanned every 60 s until the file settles. Local work only; nothing is lost. A replacement of a
  changed file that keeps failing (a disk too full to hold both versions, an item Graph will not
  serve) is retried after every cycle as it is, with no backoff of its own — no Full reconcile —
  and the status says why. Reasoned. Open.
- **F14. A replacement's swap and a reconcile can race on a locked directory's write window** — both
  open the window (`0755`) around their own write and close it again (`0555`); whichever closes
  first can make the other's write fail with `EACCES`. The replacement then counts as failed and is
  retried; a reconcile that fails makes the next cycle Full. Reasoned, never observed. Open.
  A second window: a replacement checks, under the old file's per-inode
  lock, that the old file is still the one at its name, and then renames the new version over the
  name. A reconcile does not take that lock to move a file, so in the microseconds between the
  check and the rename it can move the old file away and put a different item at the name, and
  the rename replaces that item instead. The item is in the cloud and is placed again by the next
  Full reconcile; only a download of it is lost. Reasoned, never observed. Not fixed now.
  (A Full reconcile that locks a file again no longer does so under a fill: it skips a file whose
  per-inode lock is held.)
- **F15. Stopping the sync waits for a reconcile stuck on the helper** — a stop (and so a Forget)
  never waits for Graph or for the lifecycle lock, but it does wait for a reconcile already
  changing the folder, and one waiting for the helper's answer to a `MarkDir` can take up to that
  call's 30 s bound. Reasoned. Open.
- **F16. A rescue needs a directory on the folder's own filesystem** — a rescue is one rename, never
  a copy, so it goes to the data directory's `rescued/` only when that is on the folder's
  filesystem, and otherwise beside the folder (`.konedrive-rescued-<name>`). When the folder's
  parent is on yet another filesystem (the folder is a mount point) or cannot be written, a rescue
  fails and so does every cycle that needs one, until the folder is moved; nothing is lost and
  the status says so. Reasoned. Open.
- **F17.** The same limit as F28, and merged into it.
- **F18. A tree store that cannot be opened stops the sync until something asks again** — the status
  says so (`RootState` `error`) and downloads on open still work, but nothing retries the open on its
  own. `Refresh()` (`konedrivectl sync refresh`) does, and refuses with the reason when it still
  cannot; so does a daemon restart, and a helper reconnect. (A OneDrive folder without interception
  has no sync to start until the helper connects: `docs/design/sync.md` §3.) Measured
  (`refresh_starts_a_sync_that_could_not_start_or_says_why`). Open.
- **F19. A Forget that cannot take the lock off the folder only logs it** — the folder is forgotten
  and its files stay, but whatever the unlock walk did not reach (the folder moved or lost its root
  id, a directory could not be read) stays `r--r--r--` / `r-xr-xr-x` until `chmod -R u+w`. The walk
  runs under the lifecycle lock, so a very large folder delays other registrations and Forgets
  meanwhile. And one file can be left read-only even when the walk reaches it: a download that is
  still running when the folder is forgotten lifts the file's write bit around each attribute write
  and puts back the mode it found, `r--r--r--`, possibly after the walk made it writable. Reasoned.
  Open.
- **F20. What a folder shows is decided when it is registered** — signed in (with the drive the
  daemon always has) and with the helper (`RegisterRoot`), it shows OneDrive; signed out, or
  registered without interception (`docs/design/sync.md` §3), it is local and filled with
  `PopulateFromDirectory`. A local folder stays local after a sign-in, and a OneDrive folder stays
  OneDrive after a sign-out (it then says "signed out"); changing it takes a Forget and a new
  registration. `config.toml`'s `sync_root_source` records it. By design. Open.
- **F21. "The network is back" means NetworkManager's `CONNECTED_GLOBAL`** — the daemon follows
  NetworkManager's `StateChanged` on the system bus. Without NetworkManager, without a reachable
  system bus, or on a network NetworkManager rates as only site- or locally connected, a return
  is noticed by the retry schedule alone: 5, 15, 30 s after the first, second and third failed
  cycle in a row, and every 60 s (the ordinary interval) after that — never longer than one
  interval once a cycle has been failing a while (`Schedule::default`,
  `crates/konedrived/src/sync/listing.rs`). The system-bus half (`network::watch`) never runs in
  a test; `watch_on` is tested against a fake on a private bus. Reasoned. Open.
- **F22. A sign-in is noticed as a change of state, not as an event** — a folder's sync is nudged
  when the account's state becomes `signed-in` from anything else. A sign-out and a sign-in that
  both land before the watcher next runs would be seen as no change; a sign-in takes a browser
  round trip, so this is not expected to happen, and the next poll covers it if it does. Reasoned.
  Open.
- **F23. A OneDrive folder registered again while signed out keeps its read-only lock** — a folder
  that showed OneDrive but is not registered right now can be registered again while signed out.
  That happens when its restore failed: a folder without interception that did not come up at
  startup, or one registered in the startup window before `resume`. Registered signed out, it
  becomes a local folder, but its files stay `r--r--r--` and its directories
  `r-xr-xr-x`, so filling it from a directory fails. A Forget takes the lock off only a folder
  that shows OneDrive, not a local one. Workaround for the user: `chmod -R u+w` on the folder.
  Deliberately not fixed: the daemon does **not** unlock on a new local registration, because in
  a folder that never showed OneDrive that would make the user's own read-only files writable.
  Reasoned. Open.
- **F24. The activity log keeps 200 events, and they go with the tree store** — LIMIT, by design
  (`docs/design/desktop.md` §2.4). The oldest go past 200. A Forget drops the log and the conflicts
  with the store, and a store rebuilt (an unknown schema version, corruption) starts empty; version
  2 added the two tables, so every existing store is rebuilt once, at the cost of one full listing.
  `LastChecked` is kept in the store too and reads "never" after a rebuild until the first cycle. A
  folder filled with `PopulateFromDirectory` has no store: its activity is kept in memory, lost at a
  restart, and it has no conflicts. An event is kept only while its path is inside the folder
  registered now: a download that ends after its folder was forgotten (`Hydrate` holds no lifecycle
  lock, so a Forget does not wait for it) is recorded nowhere, rather than in the next folder's log.
  Measured (`tree.rs`, `sync::tests`, `sync::activity::tests`). Open.
- **F25. The activity log is a summary, not a record of every file** — an incremental cycle logs at
  most 50 events of each kind (`added`, `updated`, `removed`, `moved`) plus one "and N more" at the
  folder; a first listing or any Full reconcile is one `listed` event ("N items"); a folder removed
  with everything in it is one `removed`; `FreeUpSpace` is one `freed` event for the folder ("N
  files, X"), where `Dehydrate` of one file is one event for it. `conflict` events are capped the
  same way, 50 per cycle and "and N more", while `Conflicts()` lists every one. A replacement that
  keeps failing the same way on every retry (every cycle) is one `update-failed` event, said again
  only for another reason or a newer version; the status's replacement note keeps saying it. Which
  files a big change touched is not in the log. Measured (`sync::listing::tests`). Open.
- **F26. `LocalBytes` is a walk, so it lags up to 5 s behind a download** — it is measured by
  `lstat` over the folder's regular files, on a blocking thread, after every cycle and after a
  download or free-up, but never twice within 5 s: a download finishing right after a walk shows
  up to 5 s later. The walk costs one `lstat` per file each time — every 60 s at least, for the
  cycle — which a very large folder will feel. Not counted: directories' own blocks, `.konedrive-*`
  entries, anything past a mount point inside the folder. Measured on the paused clock
  (`sync::activity::tests`); the cost is reasoned. Open.
- **F27. `FreeUpSpace` skips busy files** — a file open anywhere (its write lease is refused), or
  one a download or another free-up is busy with (its per-inode lock is taken), is left as it is
  and counted as busy, never waited for: a download can take any time. A file changed here, or not
  a clean download, is left too and counted in neither. If the helper goes away part-way the call
  stops with `NoHelper`; what was freed by then stays freed and is in the log. If the folder is
  forgotten part-way, the call returns what it freed by then. Measured
  (`free_up_space_frees_what_is_not_in_use_and_counts_what_is`). Open.
- **F28. A conflict is recorded only for a reconcile that goes through** (F17 merged here) — a
  rescued file becomes a conflict (`Conflicts()`, a `conflict` event) when its reconcile commits.
  What a Changed pass rescued before it handed over to a Full one is carried into the Full and
  recorded with its result. A reconcile that fails with an error records none of the rescues it
  already made: those files are in the rescue directory and the daemon's log, not in the conflict
  list. `LastError` carries no rescue note at all: a conflict is not a problem, and `Conflicts()`,
  `ConflictCount` and the `conflict` events say where each file went. And `ActivityAdded` is a live signal with a
  1024-event queue: more than that waiting at once and the oldest of them are not signalled, only
  logged (`RecentActivity` still has the newest 200). Measured (`sync::listing::tests`) but for the
  queue, which is reasoned. Open.
- **F29. A fill on open is shown under the name the file had when it was opened** — its
  `Transfers` entry and its `downloaded`/`failed` event take the name from the open descriptor
  (`/proc/self/fd`); a file renamed or deleted while it downloads is shown under its old name, or
  with " (deleted)". `Hydrate` and replacements use the path they were given. Reasoned. Open.
- **F30. A failed switch the helper may still hold is kept intercepted, and waits for the next
  connect** — when a switch to interception (W1) fails after the helper was asked to register the
  folder, and the helper cannot confirm it let go of it (the call timed out, the link dropped), the
  folder is kept intercepted rather than left without interception (a folder the helper may hold
  must never be one the daemon holds without interception). It reads `error`, its sync stays
  stopped, and it is brought up at the next helper connect — which, with the link still up after a
  timeout, means the next reconnect or daemon restart. Measured with a fake helper
  (`a_failed_switch_the_helper_may_still_hold_…`). Open.
- **F31. A folder that already carries its root id is not probed again** — bringing a folder up
  (every restart, a switch to interception) skips the write probe that a first registration runs: a
  folder that shows OneDrive is locked read-only, the folder itself too, and the probe's write was
  refused there, so no such folder came back after a restart, in either mode (measured,
  `a_locked_onedrive_folder_comes_back_after_a_restart_in_either_mode`; now fixed). The helper's
  re-registration skips its own probe for the same reason. The cost: a filesystem that lost a
  feature since the first registration (leases turned off, say) is found by the first free-up or
  recovery that needs it, not when the folder is brought up. Reasoned. Open.
- **F32. `ItemsPlaced + SkippedCount` can be less than `ItemsListed`, by design, not a race** —
  `TreeStore::counts` walks the tree recursively from the root through folders whose own `placement`
  is `placed`; `placed` and `skipped` only count rows the walk actually reaches. An item inside a
  *skipped* folder (a OneNote folder, say) is never reached by that walk, so it counts toward
  `listed` (every row but the root, unconditionally) but toward neither `placed` nor `skipped` —
  this is exactly what `Skipped()`'s own doc means by "whose own folder is in the tree", not a bug.
  Found in a real-account run of the VM suite, where `placed + skipped` stayed short of `listed` by
  exactly the items inside skipped folders. A test (or any future monitoring code) that waits for
  `RootState` to leave `listing` and then compares `placed + skipped` against `listed` to decide a
  cycle is done can wait forever on a real, deep tree with a skipped folder that has enough inside
  it. `LastChecked` (`status().0` moving off `0`) is the reliable "this cycle's reconcile actually
  finished" signal; `tests/vm/graph.rs`'s `g1` uses it for exactly this reason. Measured
  (`crates/konedrived/src/tree.rs`'s `counts`, and the VM run above). Open — the gap itself is fine;
  naming it is the fix.
- **F33. Partial downloads are kept, not erased** — a fill that gives up, or a `hydrating` file
  found `online-only` at startup, no longer resets to empty: the durable prefix and
  `user.konedrive.progress` are kept and only what lies past them is punched
  (`roll_back`/`source.rs`; `reset_interrupted`/`keep_checkpoint`/`root.rs`). A checkpoint is
  accepted at recovery time by its byte count alone; the cTag is compared only when a fill actually
  resumes, against fresh Graph metadata, and a mismatched or hash-less checkpoint is discarded then.
  Safe because the whole file, the resumed prefix included, is checked against its `quickXorHash`
  before it is ever marked `hydrated`. DEBT · reasoned · mitigated (the file is never handed to an
  application as anything but zeros or verified content). **Cost:** a file shown as `online-only`
  may already hold part of its blocks on disk; `konedrivectl sync free-up-space` and `sync
  dehydrate` refuse it (`NotHydrated`), so the space it occupies comes back only once the download
  finishes or the cloud item changes underneath it. **The time**: a fill's writes move the file's
  time to now. A reconcile used to take that for a new version and punched the checkpoint away — at
  the Full reconcile every restart begins with, so a 1.5 GB partial download was lost seconds after
  login. A reconcile now tells a placeholder's content by cTag and size alone, puts only the cloud's
  time back (under the per-inode lock), and drops a checkpoint only when its cTag is not the tree's.
  A fill that gives up puts back the time the file had before it; recovery still restores the time
  from before its punch, and the first Full reconcile corrects it. Measured
  (`a_partial_download_survives_recovery_and_the_full_reconcile_after_it`).
- **F34. A file Graph hands out with no hash is only logged** — both personal and
  business OneDrive are expected to give a `quickXorHash` for every file; when one does not, the
  daemon logs a warning and moves on, rather than showing it as a standing status message —
  treated as an anomaly worth noticing in the log, not a normal state the user needs to act on.
  A file downloaded this way is not verified against a hash the way every other one is (see
  F33). DEBT · reasoned. Open.
- **F35. The first listing is placed page by page — only the first, and only into a folder that
  holds nothing of ours** — before this, a first listing was staged whole and the folder stayed
  empty until one Full reconcile at its end: minutes for a large drive.
  Now, when there is no delta link, `items` is empty and nothing in the folder carries an item id,
  each delta page is placed as it comes (same materializer: M1, the read-only lock, skips,
  rescues) and committed into `items` together with the link to the next page (`listing_next` in
  `meta`), so the folder fills as the drive is listed and `ItemsListed`/`ItemsPlaced` rise page by
  page. The lifecycle lock is taken per page, never across a Graph request. A stopped listing
  resumes at the page it was on, and asks for no committed page again. An entry whose folder comes
  on a later page waits in `items` and is placed with it; one whose folder never comes stays
  listed and not placed, as after a Full reconcile (F32). The limits:
  (1) A folder that already shows the drive — tree store lost or rebuilt, or Forget and
  registered again — still fills only at the end: part-way through a listing, an item of ours not
  listed yet cannot be told from one that is gone, so it is listed whole and reconciled once,
  finding what is there by its id.
  (2) The resume link a stopped listing left, if Graph refuses it when a cycle asks for it first —
  `410`, `404`, any other non-transient answer (`400` for a token it no longer takes, a page with
  no link) — lists the drive again from the start the old way: what is placed stays and is found
  by its id, the rest appears at the end. A `401` twice in a row right after the account check
  would also count as a refusal; no data is at risk, only the progressive fill of that listing.
  A next-page link handed out in the same cycle that Graph turns down is no refusal: the cycle
  fails like any trouble with Graph, and the next one resumes from that link. After a refusal
  `ItemsListed` drops to 0 and counts the new listing up from there: the count goes backwards
  once.
  (3) The first page each cycle places is reconciled Full, as every cycle after a stop or failure
  is: a scan of everything placed so far, under the lifecycle lock. A listing that keeps failing (a
  flaky network) pays that scan at every retry. Reasoned at a few seconds for 30,000 items; not
  measured.
  (4) The counts are read after every page: a recursive query over everything placed so far, so
  the listing as a whole costs O(N²/page size) in counting. Reasoned cheap at ~155 pages; not
  measured.
  (5) `conflict` events are capped (50, then "and N more") per page, not per listing, and written
  with the page, so a listing that never ends still says where each file went. A first listing
  into a folder full of the user's own colliding names can log more than 50 of them. Every
  conflict is still a row.
  (6) A page that stops part-way is not committed; the Full first page of the next cycle finds by
  id what it had placed. What the page, fetched again, no longer lists (the cloud changed in
  between) is removed then and placed again when a later page lists it, so a download made of it
  in that window is lost (it downloads again).
  (7) A folder deleted on a later page takes everything still inside it, on disk and in `items`. A
  child the feed moves out of that folder on a page after the deletion is then made again, fresh,
  where it now belongs, so a download made of it in between is lost (it downloads again). The old
  path kept it: the whole listing was in before anything was removed, and the child was moved by its
  id.
  (8) The same holds for an item a later page moves under a folder that has not come yet: it is
  removed, since it belongs nowhere yet, and made again, fresh, when its folder comes. A download
  made of it is lost. The old path moved it by its id.
  (9) Until a listing has begun page by page, a cycle with no delta link and an empty `items`
  scans the whole folder for an item id before it decides. Once one begins, `listing_next` says
  so and the scan is not repeated. A folder that shows the drive already, whose whole listing
  keeps failing, is scanned again at every retry.
  FRAGILE · measured with wiremock (`sync::listing::tests`: `a_first_listing_shows_each_page_…`,
  `an_item_whose_folder_comes_on_a_later_page_…`, `an_entry_waiting_for_its_folder_survives_a_stop`,
  `a_listing_stopped_part_way_resumes_…`, `a_refused_resume_link_…`,
  `a_next_page_turned_down_fails_the_cycle_…`, `a_page_stopped_while_it_was_being_placed_…`,
  `a_first_page_stopped_…`, `every_folder_placed_page_by_page_is_marked_…`,
  `a_folder_that_already_shows_the_drive_…`); (7) and (8) are reasoned; not yet run against the
  real account. Open.
- **F36. `HelperState` is what systemd says, asked every 30 s** (`docs/design/desktop.md` §2.5) —
  with no link, the daemon asks systemd, read-only on the system bus, how `konedrive-helper.service`
  stands. A daemon that cannot reach the system bus or systemd (a container, another init) reads
  `unknown` and says only "not connected"; so does a helper systemd says is running while the daemon
  has no link yet (it connects within the reconnect backoff, 30 s at most, or it is F11's
  unreachable helper). A helper installed under another unit name reads `not-installed`. A change
  with no link shows within 30 s; a link that comes or goes shows at once. The systemd half never
  runs in a test: a fake answers there (`HelperUnit`). FRAGILE · reasoned. Open.
- **F37. `config.toml` has two independent writers.** The account service's `set_client_id`
  (`konedrived/src/account.rs`) and the sync side's `record_drive` (`konedrived/src/sync/listing.rs`)
  each do their own read-modify-write of `config.toml`, with no lock shared between them: one can
  read the file, the other can read, modify and save it, and the first then saves over that change
  with what it read before. Both are rare and small (a client id set once; a drive id recorded once
  per fresh listing), so the window is narrow, but nothing closes it. Predates this phase. Reasoned. Open.
- **F38. Pins: the sweep, and what waits for it** (`docs/design/pinning.md` §6) — (1) the sweep
  walks the whole folder, reading every item's pin attribute (one `lgetxattr` each, beside the
  `lstat`), after each Full reconcile and at start, because nothing but the attributes records
  where the pins are; that is a second walk of the folder on top of the Full reconcile's own scan.
  (2) A pinned file whose download failed, or was dropped for a full disk, is queued again by the
  sweep after the next cycle that succeeds: a download that keeps failing — no network for one
  file, say — makes every cycle walk the whole folder once more until it goes through. A folder
  of the developer's mode has no cycles, and waits for its next start.
  (3) Every cycle that moves or removes anything while pins exist walks the whole folder too, to
  count the pins right again (a pinned item, or a folder with one inside, may have moved or gone). (4) A file with a pin of its own that a reconcile makes
  again — a local change rescued, a file in an unrecognised state — comes back without its pin
  (a replacement of a changed file keeps it, under the lock it holds for the swap). (5) Writing a
  pin lifts the owner's write bit for the moment of the write, and so did two things running at
  once: a pin on a file against a fill of that file, which lifts the same bit around each of its
  attribute writes — one could put the lock back while the other wrote, failing `EACCES` (the
  fill, or the pin); and a pin on a directory against the materializer creating, renaming or
  removing in it — the pin could put `0555` back mid-create, failing the reconcile, or the
  reconcile could lock the directory mid-pin, failing the pin. A file's pin is now written under
  its per-inode lock, and every lift of a directory's write bit in the daemon — the pin's and the
  materializer's windows and locking alike — under one lock (`disk::dir_modes`). A `chmod` from
  outside the daemon still races with both. (6) `FreeUp` counts a file changed here, which it
  leaves, in `busy`: the D-Bus answer is fixed at (files, bytes, busy, skipped_pinned), so "in
  use" and "changed here" are one number. FRAGILE · reasoned; that the sweep after a restart queues a pinned online-only file is
  measured (`sync::listing::tests::the_sweep_after_a_restart_…`). Open.

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
| Delta size reconciled in full (`FULL_THRESHOLD`) | 5000 changes | **guess** — above it one scan is assumed cheaper than item by item |
| Sync interval / waits after failures in a row | 60 s / 5, 15, 30 s | 60 s is the design's; the retry steps are a **guess** |
| A fill's checkpoint, every N bytes (`CHECKPOINT_EVERY`) | 16 MiB | **guess** |
| `Retry-After` wait when Graph throttles (`429`/`503`) | default 10 s, capped at 300 s, 5 attempts before giving up | **guess** (`RetryPolicy::default`) |
| Replacements of changed files downloading at once | 2 | **guess** |
| Fills served on open at once (`serve_hydrations`) | 4 | **guess** |
| Pinned downloads at once (`PIN_SLOTS`), beside the fills on open | 4 | **guess**, equal to the fills on open |
| Thumbnails filled per run / how far apart / how often regardless | 200 / 500 ms / every 10 min | **guess** (`crates/konedrived/src/sync/thumbs.rs`) |
| Activity events kept / logged per kind in an incremental cycle | 200 / 50 | **guess** |
| Shortest time between two `LocalBytes` walks | 5 s | **guess** |
| Shortest time between two coalesced `PropertiesChanged` (counters, status, `Transfers`) | 250 ms, at most 4 signals a second | the design's four a second |
| Notifications per event kind (A3) | one per 10 s, the rest as one summary | **guess** |
| Window's "checked N s ago" refresh | every 10 s, from the clock | **guess** |
| Window's "Recent" list | 50 rows | **guess**; the daemon keeps 200 |

---

## 6. Quality debt

- **D1.** `cargo fmt --check` has never been clean and there is no `rustfmt.toml`.
- **D2.** The fake helper used by tests exists in several copies, each shaped for its tests: in
  `konedrivectl` (`tests/sync_cli.rs`) and in `konedrived` (`tests/sync_dbus.rs`, and the test
  modules of `sync/mod.rs`, `sync/helper.rs`, `sync/root.rs`, `sync/materialize.rs` and
  `sync/listing.rs`, whose one holds back a chosen `MarkDir`).
- **D3.** Three small gaps in the D-Bus tests were never closed: none guards data, each would pin
  an existing behaviour.
- **D4.** In the no-interception mode, `sync status` says the same thing twice (the CLI's
  `Opens:` line and the daemon's `LastError`).
- **D5.** Freeing up an already freed file exits non-zero instead of succeeding idempotently.
- **D6.** Two daemons writing one file after a `chown` in the middle of a download have no
  shared lock. Needs root to trigger.
- **D7.** The crash consistency of placeholder creation has not been reviewed.
- **D9.** The notification watcher of `docs/design/hydration.md` §16 was never built; planned for
  the write phase.
- **D10.** One unsuitable file in a populate source fails the whole populate, with a message naming
  it, rather than skipping that file.
- **D11.** A delta is held in memory whole before it is staged (a full listing is staged page by
  page). A very large delta after a long time offline costs memory in proportion.
- **D12.** After a listing fails part-way, `ItemsListed` keeps the part-way count until the next
  cycle that succeeds.
- **D13.** The Baloo tests write a fake `balooctl6` script and run it at once, so a test in another
  thread that forks at that moment can make the exec fail `ETXTBSY` (the classic write-then-exec
  race): `sync::baloo::tests::is_excluded_true_under_an_excluded_parent_not_a_mere_prefix` failed
  once in a full `cargo test --workspace`, and passes alone. Only the tests
  are affected; the daemon runs the real `balooctl6`, which nothing writes.
- **D14.** For a legacy folder whose switch to interception failed while the helper is genuinely
  connected (F30), the CLI's wording still blames the helper itself: `konedrivectl sync refresh`
  says "the konedrive helper is not connected, so the folder is not kept in step with OneDrive
  until it is, and nothing was asked for", and `konedrivectl sync hydrate` can say "the folder is
  not connected yet; try again in a moment" (`refusal_text_in`, `konedrivectl/src/lib.rs`). Neither
  line says that the helper is connected and only this folder is stuck waiting for the next
  reconnect.

---

## 7. Dolphin integration

The two plugins in `dolphin/`: emblems for each file's state and pin, and "Always keep on this
device" / "Free up space" in the context menu. They read a file's state and pin from its extended
attributes and never open it.

- **K1. Dolphin opens some files itself, and that downloads them.** LIMIT · measured in KIO
  (`tests/kio/kio_probe.cpp`, `docs/kio-behavior.md` §C) · partly closed (thumbnails from OneDrive).
  To draw a preview, and to detect the type of a file whose name does not settle it, Dolphin opens
  it. Measured (§C): `data.bin` and `noextension` opened for content sniffing; `.pdf` and `.jpg`
  were resolved from the name alone, with no open; `.txt` was not measured. Such files download
  just by being shown. A `user.mime_type` xattr (the shared-mime-info/GVFS convention) on an
  extensionless placeholder did **not** stop the open: `KFileItem::determineMimeType()` on KIO
  6.30 does not consult it at all and falls straight through to content sniffing, which opens the
  file. The thumbnail filler closes the preview half of this for a placed image or video: it
  writes Graph's own thumbnail into `~/.cache/thumbnails/{normal,large,x-large}`, so a preview at
  those sizes is drawn from the cache and never opens the file (measured mechanism: §B). What
  remains open is the type-sniffing half (`data.bin`, `noextension` — unrelated to thumbnails)
  and previews at `xx-large`, which are deliberately not filled (K15).
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
  outside. On a hung network mount it blocks exactly as a `stat` would. The 6.7 µs figure predates
  pinning and does not include `isEffectivelyPinned`'s own ancestor walk (a second `lgetxattr` per
  level up to the root, not cached -- K25), which is paid on top of it for every file and folder
  Dolphin asks about; not remeasured.
- **K6. At most 1000 paths wait at once, per window.** DEBT · measured. `Pin`/`FreeUp` now take the
  whole selection in one call each (`dbus/org.konedrive.Sync1.xml`), so this cap and the dedupe against
  a path already waiting bound one batch, not one call per file as before; a selection of more than 1000
  files still takes several clicks. A never-answering daemon still costs up to ~3 MB of Dolphin memory
  per window (the paths, not the calls, are what is kept), and on `dbus-daemon` buses (not Fedora's
  `dbus-broker`) the one call in flight still shares Dolphin's own reply budget. The waiting set is per
  window, so two windows can each send the same path once. The dedupe is by path alone, not by
  path and operation: a path still waiting on a `Pin` is not sent again on a `FreeUp` either (and
  the reverse), so choosing one action right after the other on an overlapping selection, before
  the first answers, sends only the first — the second reports "not yet answered" for those
  paths. Measured in `dolphin/tests/noopentest.cpp`.
- **K7. A root mark set or removed by hand.** Reasoned. The stale "not in a root" answer lasts until the
  folder is evicted from the cache or Dolphin restarts, including when the mark is on an unwatched
  ancestor; after a mark is removed and restored, emblems come back only when Dolphin asks again.
  Registering through the daemon is unaffected.
- **K8. Messages can be lost.** Reasoned, and confirmed in source for the desktop. A failure message is
  dropped if Dolphin rebuilds the plugin before the daemon answers (the work still happens), and the
  Plasma desktop, which also hosts the menu plugin, never shows its messages at all.
- **K9. A renamed folder that Dolphin immediately asks about stops updating live.** Reasoned.
- **K10. An unrecognised state value** shows no emblem and no actions. An unmanaged file (the
  user's own, in the sync folder) and a reserved `.konedrive-*` name are also left out of what
  `menuState` sends to `Pin`/`Unpin`/`FreeUp`, so one of them in a selection cannot make the
  daemon refuse the whole batch over it (review #7). Reasoned.
- **K11. After a failed on-demand start** the message tells the user to start the daemon by hand.
- **K12. Cosmetic:** the "already waiting" and "too many" notes appear in Dolphin's red error bar.
- **K13. Build assumptions:** the README's `QT_PLUGIN_PATH` line assumes `lib64`; the minimum KF/Qt 6.8
  matches `app/`, but only KF 6.30 with Qt 6.11 was built and tested; the plugin tests' private buses use
  the stock 50 000 pending-reply limit rather than `dbus-daemon`'s bare default of 128.
- **K14. Memory:** about 100–150 bytes per remembered file, ~10–15 MB for a 100 000-file folder.
  Reasoned.
- **K15. `xx-large` (1024 px) thumbnails are not filled.** LIMIT · deliberate.
  `docs/kio-behavior.md`'s Decision line says `FILL FullyEncoded normal,large,x-large,xx-large`,
  and §B's mechanism would work at `xx-large` too (the naming scheme is the same MD5-of-URI at
  every size). The filler fills only `normal`, `large` and `x-large` from one Graph request per
  image (`c512x512`, ≤512 px on the long edge), scaled down locally for the smaller two. Asking
  Graph for a 1024 px thumbnail as well would be a second request per image — roughly 4x the bytes
  of the 512 px one for a size KIO only asks for at maximum zoom on a HiDPI display — and upscaling
  the 512 px image to 1024 would look blurred rather than sharp. At that zoom, KIO falls back to
  its own `xx-large` generation (`KIO::PreviewJob`), which opens and downloads the file — the one
  case K1's fix does not cover.
- **K16. The thumbnail filler runs one request at a time, half a second apart.** DEBT · by design.
  A folder with many freshly-synced images fills its thumbnails gradually in the background
  (kicked after every listing cycle, and every ten minutes regardless, 200 at a time per run) rather
  than all at once; Dolphin shows no preview for an unfilled file until KIO's own on-demand
  generation opens it, or until the filler gets to it. No back-pressure signal exists between the
  filler and Dolphin.
- **K17. Thumbnail body and decode caps are fixed, not configurable.** LIMIT · deliberate.
  `DriveClient::thumbnail` refuses a body over 8 MiB (checked against `Content-Length`, and again
  while streaming, in case that header is missing or dishonest); `sync::thumbs` decodes under
  `image::Limits` of 4096×4096 px and 64 MiB of decoder allocation. Both are advisory defences
  against a misbehaving or malicious answer, not values Graph is expected to approach for a real
  `c512x512` JPEG. Either cap being hit is recorded exactly like a 404 (K1): the item's `thumb_key`
  is set, so it is never asked for again, and never surfaced to the user beyond a `tracing::warn!`.
- **K18. A renamed or deleted file's old thumbnail cache entries are never cleaned up.** LIMIT ·
  advisory. `thumb_key` stops the *tree*'s bookkeeping from re-fetching a thumbnail it already
  made, but the PNG files themselves live in `~/.cache/thumbnails/{normal,large,x-large}`, named
  by the MD5 of the *old* path's `file://` URI (`docs/kio-behavior.md` §A). A rename or delete
  leaves that PNG behind: orphaned, never looked up again (KIO hashes the *current* URI), and never
  deleted by konedrive. This matches upstream file managers' own thumbnail caches, which rely on
  the freedesktop spec's periodic cleanup (by inode/mtime reuse, or a user clearing the cache) and
  not on every writer tracking every rename — konedrive does not track it either. Cost: a few KB of
  disk per renamed or deleted image/video, unbounded over the life of a sync root.
- **K19. `thumbnail_candidates` scans every image/video row on each call.** DEBT · reasoned. The
  query (`tree.rs`) filters by `kind`, `placement` and `mime` with no supporting index, so it is
  a full scan of `items` on every poll — bounded by `limit` only in how many *candidates* it
  collects, not in how many rows it reads to find them. It runs on a blocking thread
  (`Store::run`), so it does not stall the async runtime, but a folder with very many images and
  few of them still needing a thumbnail pays for the whole scan every ten minutes (or every cycle)
  regardless. An index on `(kind, placement, mime)` would fix this if it is ever measured to
  matter; not done, since the folders tested are far short of where a full-table scan is felt.
- **K20. A renamed or moved file's thumbnail is fetched again from OneDrive.** DEBT · reasoned.
  `thumb_key` (`crates/konedrived/src/sync/thumbs.rs`) is made from the item's cTag, its
  *relative path* and its mtime — because KIO's own cache checks a thumbnail against the file's
  current mtime, and the local PNG lives under a name derived from the file's `file://` URI
  (K18), a rename needs a fresh cache entry regardless of whether the cTag changed. The filler
  cannot tell "moved, image unchanged" from "replaced with different content": both invalidate
  `thumb_key` and cost one Graph thumbnail request the next time the filler runs. Cost: a folder
  whose files or folders get renamed or moved often re-fetches thumbnails it already had,
  proportional to how often that happens — never proportional to how many images there are.
  Interacts with K18: the old PNG left behind by the rename is never cleaned up either.
- **K21. The outline-check icon name is one letter from picking the filled one instead.**
  FRAGILE · measured against the installed Breeze theme. `emblems/*/checkmark.svg` is a symlink to
  `emblem-checked.svg` (the FILLED icon), so asking the icon theme for `checkmark` is ambiguous
  between that and the different, bare glyph at `actions/*/checkmark.svg` — the OUTLINE case would
  risk silently drawing the same icon as the filled one. `overlayNames()` (`dolphin/src/filestate.cpp`)
  uses `dialog-ok` instead: byte-identical artwork to `actions/*/checkmark.svg`, but a name that
  exists nowhere else in the theme, so it cannot resolve to the wrong directory. A future Breeze
  release that adds a `dialog-ok` icon elsewhere in the theme could reopen this; `iconNamesExistInBreeze`
  (`dolphin/tests/overlayenginetest.cpp`) only checks the name resolves to *some* icon, not to the
  right one.
- **K22. A pin set or removed on a folder Dolphin has only passed through, not browsed on its
  own, does not update emblems live.** Reasoned, by the same mechanism as K7: only a directory
  Dolphin has directly asked about (or that turns out to be the sync root) gets an inotify watch;
  reading a pin, unlike the cached root answer, is otherwise always fresh (`OverlayEngine::overlays`
  reads it on every call), so the emblem is correct as soon as Dolphin asks again — it just is not
  announced on its own in between.
- **K23. "Always keep" and "Free up space" act on the selection as it was when the menu was
  built, not as it is when the button is clicked.** Reasoned (TOCTOU). Both send every selected
  path inside a root in one `Pin`/`FreeUp` call (`dolphin/src/actionplugin.cpp`); if a path's pin
  changes in between — another window pins its ancestor, say — `FreeUp` can still refuse the whole
  call `NotAllowed` even though the menu showed it enabled. The refusal is reported like any other
  (K8, K12); nothing is corrupted, the click is just stale.
- **K24. `inTheContextMenuKioBuilds` does not prove KIO's real MimeTypes-based plugin filtering.**
  FRAGILE · reasoned. `dolphin/src/konedriveactions.json`'s `MimeTypes` had only
  `application/octet-stream` until this round, which review #1 found meant real Dolphin never
  shows the menu on a folder at all — `inode/directory` is now in the list too. But the test that
  exercises the real `KFileItemActions::addActionsTo` path already passed, before the fix, for
  selections of `text/plain`, `image/jpeg` and `application/pdf`, none of which was ever declared
  either: whatever matching `MenuActionSource::Plugins` does in this test harness does not filter
  by `MimeTypes` the way Dolphin's own context-menu building evidently does. The fix is right (the
  JSON is now complete for every type the plugin cares about), but no automated test actually
  proves the *filtering* itself works for a mixed selection in real Dolphin; only manual use does
  (`docs/acceptance-check.md` §10).
- **K25. A directory's own pin bit is not cached (review #15).** DEBT · by decision, not measured.
  `OverlayEngine` caches whether a directory *is a root* per watched directory, but not whether it
  *carries a pin* — `overlays()` and `recheck()` call `isEffectivelyPinned`, an ancestor walk to
  the root, fresh every time (K5), for a directory item exactly as for a file. Caching each
  watched directory's own pin bit, and rechecking a directory's descendants only when that bit or
  the root's changed, was judged not cheap enough to add in this round; the extra UI-thread cost
  is the same per-call ancestor walk K5 already measures for files, now paid for folders too.

---

## 8. Window and tray

The app in `app/` (`docs/design/desktop.md` §4–§6): the tray icon, KDE notifications, and the
window's status, activity and conflicts, all read from `org.konedrive.Sync1` and `Account1`.

- **A1. Notifications need the app running.** LIMIT · by design (`docs/design/decisions.md`,
  "Notifications come from the app"). They come from the app, not the daemon: the app starts hidden
  in the tray at login (the "Start at login" switch, on by default) and KNotification gives the user
  KDE's per-event settings. With the app quit, or autostart turned off, nothing notifies — a failed
  download, a full disk, a conflict or a sign-out goes unannounced. Nothing is lost: opening the
  window shows the conflicts, and "Recent" shows the last 50 of the 200 events the daemon keeps.
  Events that happen while the app is not running are never notified later.
- **A2. Exact strings from the daemon still steer the app.** FRAGILE · reasoned. Notifications go
  by the event's kind (`classifyFailure`, `app/activitymodel.cpp`): `failed` (a download) →
  `downloadFailed`, `update-failed` (a replacement of a file changed in OneDrive) → `updateFailed`,
  no wording read. What remains read by its words:
  - a detail of exactly "not enough disk space" — the daemon's words for ENOSPC in either kind —
    makes it `diskFull`; other words for a full disk are notified as a plain failure;
  - the tray's "a failed update" is the `LastError` part "… could not be updated here yet: …"
    (documented in `dbus/org.konedrive.Sync1.xml`), a state that survives an app restart; the app
    takes it to be the last part of `LastError`, as the daemon writes it;
  - while the folder is `ready`, whatever else `LastError` says is shown as trouble that does not
    stop the folder ("Cannot reach OneDrive (…); trying again · checked 2 h ago"), with the offline
    icon only when it starts with "cannot reach OneDrive" — any other such trouble shows the warning
    icon instead, since the folder itself may be fine and only some other thing is not
    (`app/appstatus.cpp`). A `no-interception` root reads the same way once its own fixed warning
    ("this folder is registered WITHOUT interception…", `NO_INTERCEPTION_WARNING`) and the ". "
    after it are stripped off — before that fix, whatever followed it (the same "cannot reach
    OneDrive…", a switch-failure note) never reached the status line or the tray at all. The rescue
    note this used to except no longer exists: it left `LastError` altogether (F17, F28) — a rescue
    is now carried structurally, through `Conflicts()`/`ConflictCount`/the `conflict` event, never
    as words inside `LastError`. `app/appstatus.cpp`'s regex that used to filter that note out is
    dead code left over from before the change (harmless — there is nothing left for it to match).
  - the helper's own state (`HelperState`, read as `helperState`/`helperTrouble`/
    `helperInstruction`) is read by value, not folded into `LastError`'s words: "connected",
    "not-installed", "stopped", "failed", "unknown" are matched exactly, each with its own fixed
    instruction (`sudo scripts/install-helper.sh`, `sudo systemctl start konedrive-helper`, …).
    Tested against the window's own fake daemon only.

  A `conflict` event's detail is read as the path the local version was moved to, except past the
  50-per-cycle cap: the daemon's own summary event for that has the root as its path and "and N
  more" as its detail (`crates/konedrived/src/sync/listing.rs`'s `activity::capped`), read by that
  shape rather than as a rescued path.
- **A3. At most one notification per kind in 10 s.** PROVISIONAL · guess. The first event of a kind
  notifies at once; what follows inside 10 s is counted and sent as one summary ("2 more files could
  not be downloaded") when the window ends, which opens the next window. A burst of 500 failures is
  thus a few notifications, not 500 — and one that lasts a minute is still six. A summary names only
  the count; "Show in Folder" on a conflicts summary opens the last one.
- **A4. The autostart entry runs the installed program.** WORKAROUND · reasoned. Its
  `Exec=` is `${KDE_INSTALL_FULL_BINDIR}/konedrive --background` (e.g.
  `~/.local/bin/konedrive` after `scripts/dev-install.sh`), as the launcher's own entry does, not a
  bare `konedrive`: `~/.local/bin` is not reliably on the session's `PATH`. So a build started from
  its build tree writes an entry pointing at where it *would* be installed, and moving the
  install prefix leaves a stale entry until the switch is turned off and on. Switching from the
  developer install to the packages is the one move that is handled: `scripts/dev-uninstall.sh`
  rewrites an entry that runs `~/.local/bin/konedrive` to run `/usr/bin/konedrive` (R2). The
  entry is the truth — removing it in System Settings turns the switch off — and the first run
  turns it on only once (`StartAtLogin` in `konedriverc` records that a choice exists).
- **A5. A sign-out the user asked for elsewhere still notifies.** LIMIT · reasoned. "Sign Out" in
  the window is not announced back; `konedrivectl` sign-out is, since the app cannot tell it from
  the daemon losing the account. If the window's own sign-out call fails, the next sign-out the
  user did not ask for is silent once.
- **A6. The "Recent" list merges a load with what arrives during it.** WORKAROUND · measured
  (`liveEventsDuringALoadAreKeptOnce`, `aLiveEventAlreadyInTheReplyIsNotListedTwice`). It is
  `RecentActivity(50)`, loaded when the daemon appears or the folder changes, with each live
  `ActivityAdded` put on top. Live events that arrive while a load is on its way are kept and, when
  the answer lands, added to it if it lacks them — compared by (time, kind, path, detail) — so an
  event signalled before the daemon stored it is not lost. The other order is guarded separately:
  the daemon stores an event before it signals it, so its `RecentActivity()` reply, if it happens to
  be answered after the storing but before the client sees the matching signal, can already hold the
  event — `onActivityAdded` (`app/synccontroller.cpp`) checks the model before prepending a live
  event and drops it if the row is already there, so either order lists it once, not twice. Two
  different events with all four fields equal (the same file failing twice in one second for the
  same reason) are still listed once — that collision is not distinguished from the ordering case
  above, since both look identical to the model. A daemon without these methods (an older version)
  leaves the lists empty, with no error shown.
- **A7. The desktop side of the tray and notifications is untested.** DEBT · reasoned. Tests run
  offscreen on a private bus with no notification server and, where a tray is needed, a fake
  StatusNotifierWatcher: the tray's states, tooltip, menu and click are tested on the
  `KStatusNotifierItem` object and, in `singleinstancetest`, by clicking the running program's item
  over D-Bus; notifications through a recording sink in place of KNotification. How Plasma draws
  the icon, the real popups, their actions, `konedrive.notifyrc` in System Settings, and raising a
  window under Wayland's focus-stealing prevention are unverified. The code hands over the
  activation token of each way in — a tray click (`providedToken`), the tray menu's "Open KOneDrive"
  and "Open OneDrive Folder" (the same `providedToken`, passed to
  `KWindowSystem::setCurrentXdgActivationToken` and, for the folder, as `KIO::OpenUrlJob`'s startup
  id, since a `QAction::triggered` from the menu is one step further from the click than the item's
  own `activateRequested`), a second launch (`KWindowSystem::updateStartupId`), a click on a
  notification (`xdgActivationToken`, also passed to the file manager for "Show in Folder") — but
  none of that is exercised. With the `org.kde.desktop` style, progress bars do not render offscreen;
  screenshots use Fusion for them.
- **A8. A signed-in account without a folder shows the "offline" icon.** Decision · reasoned.
  Nothing syncs, so the tray does not say "synced"; the status line says "No OneDrive folder yet".
- **A9. Single instance under the name `org.konedrive.konedrive`; closing quits only without a
  tray.** LIMIT · measured (`singleinstancetest`, `closingTheWindowWithoutATrayQuits`,
  `…WithATrayKeepsRunning`). `KDBusService(Unique)` derives the name from the organisation domain
  (`konedrive.org`) and the component name, so it is not the desktop file's id
  (`org.konedrive.KOneDrive`) and the app is not D-Bus-activatable through it. A second launch shows
  the running window; a second `--background` launch (a login while it runs) changes nothing. A
  system tray is an `org.kde.StatusNotifierWatcher` whose `IsStatusNotifierHostRegistered` is true,
  watched as it comes and goes. With one, closing the window only hides it (the tray brings it
  back). Without one, closing it quits, so no process lingers unseen; the autostart entry is
  unchanged. An app started at login before the tray is up, whose window is never opened, runs
  hidden until the tray appears or KOneDrive is launched again, which shows the window. Settings has
  "Quit KOneDrive" in every case.
- **A10. Free Up Space waits for as long as it takes.** Decision · measured
  (`freeUpSpaceWaitsAsLongAsItTakes`). Dehydrating a large folder can outlast D-Bus's 25 s default,
  so the call has no timeout; the window shows "Freeing up space…" and keeps the button disabled
  until the answer. A daemon that hangs inside it keeps the window busy until the daemon exits
  (the bus then answers the call with an error, which the window shows).
- **A11. A held removal correlates a failure by path and a 1.5 s window, not by waiting out the
  daemon's actual order.** WORKAROUND · measured
  (`removalThenFailureWithinTheGraceWindowGivesAnError`,
  `removalThenNothingGivesSuccessAfterTheGraceWindow`, and the trace in
  `crates/konedrived/src/sync/mod.rs` ~202). The real daemon usually drops a transfer from
  `Transfers` *before* its failure is knowable: the tracked transfer is removed synchronously, so
  `Transfers`' `PropertiesChanged` (the coalescer emits on the first change) goes out at once, while
  `ActivityAdded(failed)` only follows the helper's `hydrate_done` round trip and `activity.record`.
  But the coalescer also sleeps 250 ms after each emission, so a failure fast enough can still reach
  the client before the removal does; this is not a rare corner, since 250 ms is on the same order
  as a quick local failure. So `DownloadProgressController` cannot finish a job the moment its path
  leaves `Transfers`: it holds the job for a 1.5 s grace window (`RemovalGraceMs`) on the injected
  clock; a `failed`/`update-failed` `ActivityAdded` naming that path inside the window fails the job
  with the reason, and the window elapsing without one finishes it as a plain success. An
  `ActivityAdded` for a path whose job is still active (has not left `Transfers`) still fails it at
  once — this is not merely kept for robustness, it is the real daemon's other order, made possible
  by the coalescer's 250 ms sleep. 1.5 s is a guess, not measured against the real daemon's actual
  gap between the two signals; a failure slower than that still shows as finished. A failure that
  names a path inside the overflow bucket (past the 5-job cap) is simply absorbed into the summed
  job's shrinking count — no distinct error surfaces for it, since the bucket has no per-file job to
  fail. Registering a job (`KUiServerDownloadJobTracker`) is the one path in this area that real
  Plasma ever sees; tests go through `DownloadJobTracker`, a recording fake, so
  `KUiServerV2JobTracker`'s own D-Bus behaviour is unverified here, same as A7. A daemon restart
  (`serviceAvailable` false) finishes every visible and overflow job, and any held in the grace
  window, with an error ("the KOneDrive service stopped") at once, rather than leaving them frozen
  (measured, `aDaemonRestartFinishesVisibleAndOverflowJobsWithAnError`).
- **A12. Places: a folder registered only through `konedrivectl` while the app is not
  running gets its entry when the app next starts.** LIMIT · by design (`app/placescontroller.cpp`,
  `app/tests/placescontrollertest.cpp`). `PlacesController` reconciles Dolphin's Places panel entry
  on construction and on every `Sync.syncChanged`/`PlacesSettings::enabledChanged`, which only ever
  fires inside the app process: `konedrivectl` registering or forgetting the root while the app is
  not running does not touch the Places panel until the app is started again, at which point its
  constructor-time `reconcile()` catches up. The entry is found again by a bookmark metadata tag
  (`konedrive` = `1`, set with `KFilePlacesModel::bookmarkForIndex`/`KBookmark::setMetaDataItem`,
  then `editPlace`/`refresh` to make sure the tag reaches disk and not just this process' copy of
  the bookmark file), not by url, so a folder change updates the same entry in place instead of
  leaving a stale one behind. WORKAROUND: adding a fresh entry takes the *last* row matching the new
  url rather than the first, since `addPlace` does not hand back the row it created and a user could
  already have an unrelated place at that exact url; this narrows, but does not close, the window
  where such a pre-existing entry could be mistaken for konedrive's own on that one add. The icon is
  `folder-cloud` only when the current icon theme reports having it (`QIcon::hasThemeIcon`), else
  `cloudstatus`, which every Breeze release carries. "Show in Places" (`ShowInPlaces` in
  konedriverc's `[General]` group, on by default, the same way `StartAtLogin` is stored) removes the
  entry without touching anything else in the file. Tests (`addsTheEntryForARegisteredFolder`,
  `updatesTheUrlWhenTheFolderChanges`, `removesTheEntryWhenTheFolderIsForgotten`,
  `turningTheSwitchOffRemovesTheEntry`, `aUsersOwnEntryIsLeftAlone`) run against a real
  `KFilePlacesModel` with `XDG_DATA_HOME`/`XDG_CONFIG_HOME` pointed at a wiped temporary directory,
  never the user's own `user-places.xbel` or `konedriverc`.

---

## 9. Packaging

The RPM packages, `konedrive` and `konedrive-kde`, from `packaging/rpm/konedrive.spec`
(`docs/design/packaging.md`).

- **R1. Upgrading the package restarts the helper.** LIMIT (Z1) · reasoned · open. `%postun`
  marks `konedrive-helper.service`, and every logged-in user's `konedrived.service`, for a
  restart, which systemd carries out when the `dnf` transaction ends. The restart is what makes
  the new helper run; without it the old one would run until the next boot, against a daemon of
  the new version. Cost, as Z1: a program waiting for a download at that moment reads the
  placeholder's zeros, and that download is cut off. `scripts/install-helper.sh` warns before it
  restarts the helper; the package cannot, since `dnf` asks once for the whole transaction. The
  README says to upgrade when nothing is opening files in the folder. Way out: Z1's.
- **R2. The developer install and the packages must not be installed together.** FRAGILE ·
  reasoned · mitigated. Each developer-install file takes precedence over the package's:
  `~/.config/systemd/user/konedrived.service`, `~/.local/share/dbus-1/services/…`,
  `~/.local/bin` ahead of `/usr/bin` on the usual `PATH`, per-user Dolphin plugins on
  `QT_PLUGIN_PATH`, and `/etc/systemd/system/konedrive-helper.service` over the package's unit in
  `/usr/lib`, which would keep the old helper in `/usr/local/libexec` running instead of the
  packaged one, through every upgrade. Mitigation: `scripts/dev-uninstall.sh` removes the
  per-user files (and points out per-user Dolphin plugins, which are not its to remove);
  `sudo scripts/install-helper.sh --uninstall --force` removes the old helper; the package's
  `%post` warns when the `/etc` unit exists. The package cannot see per-user files. Between
  removing the old helper and installing the package, the folder is not intercepted.
- **R3. Removing the package does not refuse while a folder is registered.** LIMIT · reasoned ·
  open. `%preun` stops the helper, so a registered folder's placeholders read as zeros, and
  `konedrivectl`, which could forget the folder, goes with the package.
  `scripts/install-helper.sh --uninstall` refuses in the same case; a package scriptlet that
  failed would leave the removal half done, so the package does not. The README says to run
  `konedrivectl sync forget` first. The helper's `/var/lib/konedrive/roots.json` stays after
  removal.
- **R4. The spec is for local builds, not yet for a public repository.** DEBT. Its `License:`
  names only GPL-3.0-or-later, although the binaries link the vendored crates, each under its own
  license; it declares no `bundled(crate(…))`; it builds no debuginfo packages; the vendor tarball
  holds every crate in `Cargo.lock`, for every platform (about 49 MB); `%check` runs only
  `desktop-file-validate`, not the test suites. A rebuild of the same version and release
  installs only with `dnf reinstall`. A COPR repository needs the first two fixed.
- **R5. Nothing tests the scriptlets.** FRAGILE · reasoned · open. No test installs the RPMs: the
  preset, the first-install start, the restart on upgrade and the stop on removal first run when
  the user installs. The first-install start runs in `%post`, before systemd's own reload at the
  end of the transaction, so on first install `%post` runs `systemctl daemon-reload` itself and
  then `systemctl start konedrive-helper.service`; the start does not depend on when that reload
  comes. Neither call can fail the transaction (`|| :` on both). A failed start prints one line
  to `dnf`'s output naming `systemctl status konedrive-helper`, and `konedrivectl sync status`
  says `Helper: stopped` until `sudo systemctl start konedrive-helper` or the next boot. Checked
  once, with a stub `systemctl` whose start fails: the scriptlet exits 0 and prints that line.

---

## Closed

Kept briefly so the history of a weak spot is findable; details are in the commits.

- **Recovery racing a download after a reconnect** could punch a file just filled and marked —
  fixed in commit `86dbf93`.
- **A populate source leading into the folder** could fill a placeholder with zeros stamped as
  downloaded — fixed in commit `3d5c183`.
- **The helper exiting on an event the kernel could not hand over** — fixed in commit
  `28ff3e6`; what remains is P1 and P8.
- **The kernel document's header named only kernel 7.2.5** — corrected with this log's first commit.

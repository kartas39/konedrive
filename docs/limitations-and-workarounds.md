# Limitations, workarounds and weak spots

One place for everything in konedrive that is limited, worked around, fragile, or knowingly
below the quality we want. It is kept current: an entry is added whenever a decision accepts
a limitation, builds a workaround, picks a number without measuring it, or parks a finding.
Detail lives elsewhere (`docs/design/`, `docs/kernel-behavior-7.2.md`, the code); this log is the
index of what is weak and why. "The write design" is `docs/design/writes.md`.

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
- **Kind** LIMIT · **Evidence** measured (VM, Btrfs) · **Status** mitigated in read-write folders
  (the watcher); open in read-only ones, where the user cannot make a directory
- **What:** `mkdir ~/OneDrive/new && mv placeholder new/ && cat new/placeholder` can open the
  file before the helper marks `new/`, and read zeros. In a read-write folder the watcher
  (`konedrived/src/sync/watcher/`) asks the helper to mark (`MarkDir`) every directory its
  bring-up walk visits, since the helper's own walk may have passed a parent before a directory
  was made in it, and then every directory someone else makes or moves in, as soon as its
  `FAN_CREATE`/`FAN_RENAME` is read and before it looks inside. The window is the event's latency
  plus one helper round trip: 20–26 ms from the start of a `sh -c 'mkdir …'` to the mark,
  measured. A `MarkDir` the helper does not answer is asked again every minute and when the
  helper is back, and `LastError` says how many directories wait for it. Wider windows remain:
  (0) while the daemon is not running (stopped, or crashed), a read-write folder keeps the lock off
  from its last run, since nothing locks it at exit: a directory made then is marked only by the
  next start's registration walk and the watcher's walk, and a placeholder moved into it before
  that reads zeros; (1) while the helper is away, a new directory is marked only by the helper's
  walk once it is back; (2) inside a directory the watcher cannot watch (F71), a new directory
  raises no event and is marked by the watcher's periodic walk, every 10 minutes. On another device
  than the folder's (a nested Btrfs subvolume, a mount), where the helper marks nothing, there is no
  window: a move onto it is a copy (`rename(2)` and `link(2)` fail `EXDEV`), whose read of the
  source is intercepted and filled, and nothing from OneDrive is placed there (F72). A folder
  turning read-write is unlocked only once its watcher has walked it, and one whose watcher cannot
  start, or whose sync cannot start, is locked (F62 (6)).
- **Why:** fanotify only offers permission events for open and access. Directory creation and
  rename are reported after the fact, so they cannot be held until the mark is placed.
- **Cost:** zeros, if a program opens the file inside that window. A person will not hit it; a
  script can.
- **Way out:** closing it fully needs a permission event for rename or create; whether kernel
  7.x has one is unchecked.
- **Where:** `docs/design/hydration.md` §3 (M1, M4), §16; write design §3.5; the VM scenarios
  `watcher: a directory made after the helper's walk is marked by the watcher's`, `watcher: a
  directory made and at once given a placeholder is marked within milliseconds, and the open is
  filled` and `watcher: a tree moved into the folder is marked all the way down before its files
  are opened`.

### Z3. A hardlink, or a file moved out of the folder, escapes interception
- **Kind** LIMIT · **Evidence** measured · **Status** mitigated in read-write folders (moves out
  re-marked, F120); open for hardlinks
- **What:** coverage follows names through marked directories. A hardlink to a placeholder in
  an unmarked directory, or a placeholder renamed out of the folder, is opened without
  interception and reads zeros. A second (bind) mount of the same filesystem *is* covered.
- **Why:** marks are placed per directory; the kernel has no subtree marks.
- **Where it stands:**
  - **A read-only folder.** Its directories are `0555`, so its user cannot move anything out
    of it (W2); only root can.
  - **A read-write folder.** The watcher sees a move out (`FAN_RENAME` with no new side;
    write design §3.1), and the examination asks the helper where the object went by its file
    handle (`OpenByHandle`, F90): alive outside the folder is a `move-out` row. The outbox
    worker re-marks what every such row names before it runs anything else, whatever the rows'
    states (held, paused, offline): `MarkFile` for a placeholder, `MarkDir` for a directory and
    every directory below it. So an open is intercepted again (M4) and filled. The row then
    downloads the object where it went, takes konedrive's attributes off, and only then deletes the
    item in OneDrive (F121). A move out still reads zeros between the move and the re-mark:
    the watcher's quiet spell (2 s) and one examination, and after a reboot or a helper restart
    until the worker's first look (F120). A directory moved out keeps its own marks meanwhile
    (marks are on inodes), so only files moved out on their own have that window, and only until
    the helper restarts for directories. A row dropped unfinished tidies what it left (F123).
    Measured in the VM: `move-out: …`
    (`tests/vm/move_out.rs`).
  - **A hardlink** is not a move: no event names it (F73), and it still reads zeros.
- **Way out, for hardlinks:** the watcher should see the link count change (`FAN_ATTRIB`
  through the source directory's mark — reasoned, not verified) and put an individual mark on
  that one file. Files with more than one link are rare, so the memory cost is negligible.
- **Where:** `docs/design/hydration.md` §16; write design §8; `docs/kernel-behavior-7.2.md` §1,
  §15.

### Z4. A tool that re-sparsifies a downloaded file behind our back
- **Kind** LIMIT · **Evidence** reasoned · **Status** open
- **What:** a downloaded file carries an ignore mark with `FAN_MARK_IGNORED_SURV_MODIFY`, so the
  helper does not see its opens. If an external tool punches holes in it
  (`fallocate --dig-holes`, some deduplication tools), it reads zeros.
- **Why:** without `SURV_MODIFY` the ignore mark cannot be placed at all while the daemon holds
  the file open for writing (measured, kernel document §2.1).
- **Way out:** watch `FAN_MODIFY` on the directory marks and re-check a downloaded file that
  suddenly lost its blocks. The write phase's watcher does not subscribe to `FAN_MODIFY` (F75), so
  this stays open.

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
- **With multiple accounts** it matters more: an account taken out of `config.toml` by hand
  leaves its folder with the helper in the same way. `Accounts1.Remove` itself goes through
  the helper, as a Forget does, and is refused `NoHelper` without it. The way out is still not
  built.

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
  that shows OneDrive; a local folder (F20) is not locked. Forget takes the lock off (F19), and so
  does a switch of the account to read-write, which puts it back on when it turns read-only again
  (write design §2.2; F62, F65).
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
  `Dev1` hands out an *access* token — about an hour of `Files.Read` — never the refresh token,
  which never leaves the daemon/KWallet. It is read-only whatever the account's mode: for a
  read-write account it comes from a refresh that asks for `Files.Read` only (write design §2.1),
  and one Microsoft answered with more is not handed out. `Dev1.ReadWriteAccessToken`
  (`export-access-token --read-write`), a token that can change files, is for the test-account
  harness only, and refused for any account the write gate does not let through (F60); the commit
  that removes the gate before the release must remove it too, or keep it behind the list.
  **`Dev1` is served in every build**, not
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
  2026-09-25, kernel 7.2.7, systemd 259. The same run also measures `open_by_handle_at` inside a
  copy of the unit (only `ExecStart=`, `Restart=` and the runtime directory differ;
  `docs/kernel-behavior-7.2.md` §15). The unit's own helper then hands a placeholder, moved into
  a directory its owner cannot enter, back to the daemon (`OpenByHandle`, read-only), which
  re-marks it and reopens it for writing, and the helper refuses another uid's object (F90).
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

### W17. The VM's two-account scenario uses two local folders, signed in by hand
- **What:** the scenario `two_accounts_one_link` of `tests/vm/scenarios.rs` (multiple-accounts
  design test 15) starts the account manager as `main.rs` does, on a private `dbus-daemon` in the
  guest. It adds two accounts and marks both signed in by hand, with no drive. Their folders are
  therefore local folders. The real helper intercepts them through the hub's one link, and each is
  filled from a directory of its own. The files have the same names, and so the same item ids, in
  both folders.
- **Cost:** what a OneDrive folder adds is not run with two accounts against the real helper: the
  router's lookup of an item id in a tree store (a local folder has none, F44), the folder's drive
  attribute (F45), and a reconnect that brings both folders up one after the other (F43). The VM
  measures routing by filesystem and by path proved by inode, and `Remove`'s Forget through the
  shared link. The scenario also needs the host's `dbus-daemon` in the guest.
- **Status:** open. Measured 2026-09-25 on btrfs (`tests/vm/run.sh quick`).

### W18. The VM's write scenarios run the daemon against a fake OneDrive in the guest
- **What:** `tests/vm/writes.rs` brings a read-write folder up with the daemon's own `SyncService`:
  the real helper, the watcher, the examination and the outbox worker. The OneDrive it talks to is
  the fake the worker's host tests use (`sync::upload::fake`, on wiremock, on the guest's
  loopback). Building it takes konedrived's `fault-injection` feature, which the suite's build
  enables, so wiremock ships in no build but the tests'. Three shortcuts:
  - the folder shares the suite's helper connection, so an intercepted open is filled from the
    suite's own content source (`source/<item id>`), not from the fake's content;
  - "the daemon killed mid-session" stops the folder's sync and starts it again in the same
    process. The worker's state goes and the row's persisted session stays, as after a kill; a
    real process exit is not run. The session is held half sent by throttling its second fragment
    (`Cloud::throttle`);
  - the helper-restart check makes the one change no event reports: a write through a hard link
    outside the folder (F73). What only a Full local scan can find is thereby found by one.
- **Cost:** what the fake cannot tell is left to the test-account run (F131): `If-Match`
  on a 0-byte PUT, `conflictBehavior` in a PUT's URL, and the service's own echo.
- **Status:** open. Measured 2026-09-25 on btrfs (`tests/vm/run.sh quick --only 'writes:'`, 6/6).

### W19. The upload stress tool confirms every held delete, and can miss a fast upload's "running" moment
- **What:** `tests/stress/stress_uploads.py` drives the real daemon end to end, through
  `konedrivectl` and the filesystem, against a read-write test account. Its drain step calls
  `sync deletes confirm` whenever anything is held, so the outbox can finish without a human —
  which means it releases *any* held delete on the account, not only ones this run made, and so it
  must run only against a dedicated test account, never a real one (said in its README). Scenario
  5 (move or edit a large file while it uploads) polls `sync outbox` every 0.2 s for that row to
  reach `running` before touching the file again; on a fast enough link even a 60 MiB upload could
  finish before the first poll lands, and the tool reports that as a scenario failure ("never saw
  … reach 'running'") rather than silently skipping the case — raising `--big-file-mb` is the
  workaround if that happens often on a given connection.
- **Cost:** a run against the wrong account could confirm someone else's held deletes; a very fast
  connection may need a larger `--big-file-mb` to reliably exercise scenario 5.
- **Status:** open. Reasoned: the tool refuses to start unless `account mode` already says
  read-write and requires an explicit `--account` (checked; see its README), but it has not yet
  been run against a real test account by this change — the coordinator runs it separately.

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
  per fresh listing), so the window is narrow, but nothing closes it. Predates the multiple-accounts phase. Reasoned.
  Closed with multiple accounts: `ConfigStore` (`konedrived/src/config.rs`) is the only writer —
  the client id, labels, drives, the migration's flags and every account's folder go through it —
  and it re-reads, changes and writes the file under one lock
  (`config::tests::writers_never_save_over_each_other`).
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
- **F39. The Graph write client rests on answers only wiremock has given** (`konedrived/src/drive/write.rs`,
  `upload.rs`) — beyond the write design's own assumptions: (1) a new version by item id is sent
  with `conflictBehavior: replace`, because Microsoft names `fail` the default and says nothing of
  what it means for an update by id; `If-Match` is the guard. (2) No session request carries `fileSize`: a personal drive answers it with `400
  invalidRequest` (measured on the test account, 2026-09-25), although Microsoft documents it. A
  full drive therefore shows itself only when a fragment is refused. The test-account harness
  still reads `fileSize` for its per-file cap, and needs the size from `Content-Range` instead. (3) An empty file's time is a second request (a `PATCH` after the `PUT`); when that one
  fails, the file is up with OneDrive's time, and a warning is logged. (4) A `401` or `403` from an
  upload URL is read as the session having ended, like a `404`. The outbox worker
  (`konedrived/src/sync/upload/`) is its caller.
  FRAGILE · reasoned. Open until the test-account run (F130, F131).
- **F40. Moving version 1's tree store into its account can give up** (`konedrived/src/migrate.rs`,
  `finish_file_moves`) — with multiple accounts, `tree.sqlite` moves into `accounts/<id>/`. Before
  the move it is opened and closed once, so that its write-ahead log is folded into it and removed.
  If the log is still there after that close, another process has the store open (an older daemon
  still running). If the new place already holds a store, that store is newer. In both cases the old
  store is left where it is, and the account lists its drive again into a new one. The activity log
  and the conflict list of before are then lost (F24); the rescued files stay in `rescued/<time>/`.
  `account.json` is moved the same way, and when it is left behind the name and quota are fetched
  again. An unexpected error, such as a directory that cannot be created or a refused rename, keeps
  the step for the next start and shows in `Accounts1.LastError`. FRAGILE · measured
  (`migrate::tests::a_store_that_cannot_be_moved_is_left_where_it_is`,
  `…the_store_move_keeps_rows_committed_to_the_write_ahead_log`). Open.
- **F41. Version 2 of `config.toml` has no way back** (`konedrived/src/migrate.rs`) — the first
  start with multiple accounts copies version 1 to `config.toml.v1` (private) and rewrites
  `config.toml` as version 2. An older konedrived reads version 2 as a configuration with no
  folder. Its next write of the file (a client id set, a drive recorded, a folder registered or
  forgotten) then drops every account. So downgrading is not supported; copying `config.toml.v1`
  back by hand is the way back. The same loss would follow if an older daemon were still running
  while the new one migrates. The migration runs before the bus name is claimed, so the new daemon
  first asks the bus whether `org.konedrive.Daemon` has an owner, and refuses to start if it has
  (`accounts::start`). An older daemon that claims the name between that question and the claim is
  not caught. Two daemons of this version never migrate at once: each takes `config.toml.lock` for
  its life, and the second refuses to start. LIMIT · reasoned. Open.
- **F42. A sign-in is refused when the daemon cannot check which drive it reached**
  (`konedrived/src/account.rs`, design §8.2) — after the code exchange, and before the refresh
  token is stored, the daemon asks `GET /me/drive` with the new token, and asks every other
  signed-in account that has no drive recorded yet for its own. The check and the record run in
  one `ConfigStore` update. It is fail-closed: a Graph that does not answer, or another account
  that cannot be asked (its token cannot be refreshed), refuses the sign-in. The tokens are then
  dropped, and `LastError` says to try again. A migrated account whose folder never recorded a
  drive records one at its first `RefreshAccountInfo` or cycle. If the account was signed in as
  another drive than its folder's, the folder's sync still says so, and the account keeps the drive
  it recorded first. The wallet item is named `KOneDrive: <email>` once the email is known, and
  `KOneDrive refresh token` before that. LIMIT · measured
  (`account_flow::an_account_with_no_drive_recorded_is_asked_first`). Open.
- **F43. Every account shares one helper link** (`konedrived/src/sync/hub.rs`, design §2.3) — the
  helper sends a user's opens to that user's newest connection only, so one daemon keeps one link
  for all its accounts. (1) The 4 fill slots and the helper's credit of 64 requests are shared:
  pinning a large folder in one account slows opens in another. (2) On connect every account's
  folder is registered again and recovered one after another, in account order, before any fill is
  served: a reconnect waits for P3's walk once per folder. LIMIT · reasoned. Open.
- **F44. An open that cannot be matched to an account is refused `EIO`**
  (`konedrived/src/sync/hub.rs`, `HelperHub::route`) — a hydration request carries a descriptor and
  nothing about accounts. The daemon looks for the account by the file's filesystem (each folder's,
  read once when it is registered), then by the name the kernel has for it, proved by opening that
  name beneath the folder, then by its item id in each tree store. The open is answered `EIO` when
  none of these finds it, and the next open tries again. That happens when two folders share a
  filesystem and the file was renamed or unlinked while its open waited, and no tree store knows its
  id: a local folder has no tree store. The one folder on the file's filesystem is taken without
  that proof only while every account's folder is placed. While some account has a folder whose
  device is not known — held back (F48), or written down by a registration still under way — even
  one candidate is proved, so a file moved out of its folder is answered `EIO` then. A tree store
  in use by a registration or a Forget at that moment is not waited for. LIMIT · measured
  (`sync::hub::tests`). Open.
- **F45. A folder forgotten before multiple accounts can be adopted by another account**
  (`konedrived/src/sync/root.rs`, design §8.3) — a OneDrive folder now carries its account's drive
  (`user.konedrive.drive`). The drive is written at registration, or at the first bring-up of an
  older folder, once the account's drive is known. A registration of a folder that carries another
  drive is refused `NotEmpty`, unless the folder is empty: its stale drive is then taken off. A
  folder forgotten before the multiple-accounts phase carries no drive, so any account can register it again, as
  before, and its sync then makes the folder match that account's drive. LIMIT · measured
  (`sync::tests::onedrive::a_onedrive_folder_remembers_its_drive_and_is_refused_to_another_account`,
  `sync::tests::an_empty_folder_that_carries_another_drive_is_taken_and_a_full_one_is_not`). Open.
- **F46. Nothing answers at `/org/konedrive/Daemon` any more** (`konedrived/src/accounts.rs`, design
  §4.1) — the daemon serves `/org/konedrive/Accounts` and one object per account, and keeps no
  alias for the single-account object. A window or a Dolphin that was running across the upgrade
  calls a path that is gone until it is restarted. `konedrivectl` moved to the new contract in the
  same phase, and the deprecated single-account proxies are gone from `konedrive-dbus`.
  LIMIT · reasoned. Open.
- **F47. What removing an account keeps** (`konedrived/src/accounts.rs`, `AccountManager::remove`)
  — `Accounts1.Remove` forgets the folder as a Forget does, then deletes the refresh token and
  everything in `accounts/<id>/` (the cached name and quota, the tree store, the activity log and
  the conflicts). The folder's files stay, and a file that was never downloaded stays as an empty
  placeholder, which reads as zeros. Rescued files stay in `rescued/<id>/`, grouped by the account's
  id rather than its label; the rescues of version 1 stay in `rescued/<time>/`. LIMIT · measured
  (`accounts::removing_an_account_forgets_its_folder_and_keeps_its_rescued_files`). Open.
- **F48. An account that collides with an earlier one in a hand-edited `config.toml` is held
  back** (`konedrived/src/accounts.rs`, design §3.1) — an account whose label, drive, folder or
  root id repeats an earlier account's is loaded and shown, but its folder is not brought up:
  `RootState` reads `error`, `LastError` names the collision, and a registration is refused. Its
  folder can still be forgotten, and the account removed: a folder it registered with interception
  in an earlier session leaves through the helper, by the root id `config.toml` records, and is
  refused `NoHelper` without one. An account whose id repeats an earlier one, or cannot name an
  object, is not loaded at all, and `Accounts1.LastError` says so. Correcting the file and starting
  the daemon again is the way out. LIMIT · measured
  (`accounts::removing_a_held_account_forgets_its_folder_through_the_helper`). Open.
- **F49. Personal Microsoft accounts only** (`konedrived/src/oauth.rs`, design §12.3) — every
  account signs in through the `consumers` authority. Work or school accounts (Microsoft 365,
  OneDrive for Business) need the `organizations` authority, an app registration that allows them,
  often an administrator's consent, and testing against SharePoint-backed drives: a later phase.
  LIMIT · reasoned. Open.
- **F50. `konedrivectl` explains some refusals from its own view of the folders**
  (`konedrivectl/src/main.rs`, `explained_paths`, `carries_a_drive`) — `Files1` refuses a path in
  no account's folder `OutsideRoot` without saying which folders there are. After such a refusal
  the CLI reads every account's `RootPath` and finds the folder that holds the path by the rule
  `Files1` routes by (a component prefix, the directory part resolved by the CLI first). Where
  the two disagree — a folder reached through a link that the daemon resolves and the CLI does
  not — the explanation names the wrong folder, or none. And the daemon refuses a folder that is
  another account's under `NotEmpty`, the same name as a folder that is not empty; the CLI tells
  the two apart by reading `user.konedrive.drive` on the folder itself (a non-empty value). It
  cannot see the account's own drive, so a folder with files in it that carries this account's
  drive but no root id would be called another account's; the text then also says to sign in
  first and register again, the way back for an account's own earlier folder. A `ForeignFolder`
  error name of its own would end the guess. Only the words are at stake: the refusal is the
  daemon's. FRAGILE · reasoned. Open.
- **F51. Choosing the account on the command line** (`konedrivectl/src/lib.rs`, `choose`, design
  §5.1) — (1) an email names an account only once the account has signed in: `Account1.Email`
  is empty before. (2) `KONEDRIVE_ACCOUNT` is a default for a whole shell, so the commands that
  act on no chosen account — the path commands, `account …` and `set-client-id` — ignore it;
  `--account` given to them is refused with exit status 2 rather than ignored. (3) `login` with no
  account adds `Personal` before the browser round trip, and keeps it when the sign-in is then
  cancelled or refused. (4) `account remove` names where the files the conflicts list holds were
  rescued to, read from the list before the removal. Where the rescues of conflicts dismissed
  earlier went — the data directory, beside a folder on another filesystem, or a migrated
  account's `rescued/<time>/` — nothing on the bus says, so it says only that they stay. It also
  says the account's cached data was deleted when the daemon only logged that it could not delete
  it. (5) A name that fits two accounts is refused (exit status 2), never taken as the first. The
  daemon refuses a label shaped like an id, so only a hand-edited `config.toml` makes one; the
  window's copy of the label rules (A14) does not know that rule yet and leaves it to the daemon.
  LIMIT · measured (`konedrivectl/tests/accounts_cli.rs`, `tests/sync_cli.rs`). Open.
- **F52. An edit that kept both size and time, made while nothing watched, is not found**
  (`konedrived/src/sync/local/examine.rs`) — the examination finds a local edit by its stamp (size or
  time differ from the version the file was downloaded or last uploaded as) or by a `FAN_CLOSE_WRITE`.
  One made while the daemon or its watcher was not running that kept both the size and the
  modification time (a tool that puts the time back, `touch -r`) is not found by the Full local scan:
  OneDrive keeps the old version until the file changes again. Also, a same-size save that moves the
  time is read once in full to be hashed (quickXorHash) before the upload reads it again, so a large
  file saved in place costs two reads. LIMIT · measured
  (`sync::local::tests::an_edit_is_an_update_and_a_touch_uploads_nothing`). Open.
- **F53. A copy and a move are told apart by the file handle the store recorded**
  (`konedrived/src/sync/local/examine.rs`) — (1) an item's inode (`items.local_handle`) is recorded
  when the reconcile places it, when a replacement swaps a new version in (`record_replaced`), and
  whenever an examination finds the item. (2) When several inodes carry an item's id, the recorded
  one is the item and the others are copies; when the recorded one is not among them, the one where
  the item should be is taken (an editor's new inode that copied the attributes); when none is there,
  the helper is asked where the recorded one is (F54), and while it cannot answer the item is left
  undecided. (3) Rename-to-a-backup saves (vim's `file~`) are recognised only when the backup's name
  is on the ignore list; under any other name the original is a move and the new file a create.
  (4) A copy, or a file from elsewhere (an item id the base does not know), is uploaded only if it is
  downloaded and has no other link; a placeholder cannot be read here, and a file with other links
  (perhaps in another account's folder) would lose konedrive's attributes on every name, so both are
  listed instead. FRAGILE · measured (`sync::local::tests::save_by_rename_in_editors_patterns_…`,
  `…copies_that_kept_their_attributes_…`, `…a_copy_does_not_take_the_item_when_its_original_left_…`,
  `…a_replaced_file_moved_out_is_a_move_out`, `…a_file_from_elsewhere_…`). Open.
- **F54. A missing item is deleted in OneDrive only on the helper's word**
  (`konedrived/src/sync/local/liveness.rs`, `examine.rs`) — a base item missing from where it was is
  asked after by its recorded file handle: gone is a delete, alive outside the folder a move out,
  alive inside it is looked for again. Only the helper can open a handle, so the daemon asks its
  `OpenByHandle` (F90; `HelperLiveness`), from the examination's own thread, which waits
  for the answer: `ESTALE` is gone, but only for handles taken on the filesystem the folder is on
  now (F121 (9)), and only when nothing, or another object, stands at the item's place (an inode
  that cannot be read answers `ESTALE` too); a descriptor says where the object is, once that path, opened again, is the same
  inode (a disconnected file reads as `/`, which decides nothing). Anything else decides
  nothing, and the item is reported undecided and stays in OneDrive: `EPERM` above all, which an
  object in a nested subvolume always gets and which is never read as gone, and no helper, which
  is what `NoLiveness` (tests) answers every time. An answer that cannot be placed for sure decides nothing either: no readable root path, a
  path that is not absolute or ends in ` (deleted)`, or a place in the folder where the object's own
  handle is not. An item with no recorded handle (a rebuilt store, a filesystem that gives no handles,
  items whose held deletes were restored until they are placed again) is never deleted (WR4), nor is
  a folder with one inside it; the reconcile places it again. When a folder leaves, every item the base has inside it that is still
  with it is asked after too, one helper round trip each (estimated at about 0.1 ms each, so about
  1 s per 10 000 items): what left
  it first leaves on its own, and while any of them is elsewhere in the folder or cannot be placed,
  or left the folder but is held back itself, the folder waits. Restoring held removals (`RestoreDeletes`) drops their rows, move-outs included,
  and the item is placed again in the folder. What a dropped `move-out` named outside is tidied as
  the Trash case without the delete (`OutboxWorker::tidy_restored`, through
  `OpenByHandle`): a placeholder is removed (it holds nothing, and its item is back in the folder),
  a downloaded file is stripped (the user's own copy), a directory of the item is stripped,
  unmarked and removed if left empty. Left as it was: an object back beneath a folder, one whose
  place cannot be proved, a placeholder with another link, one being filled, and anything while no
  worker runs (a paused worker runs this). Such a placeholder, with no row left to re-mark it,
  reads zeros once the helper restarts (Z3). The examination's tests answer from a table
  (`FakeLiveness`, test builds only); the helper's answer is read by
  `sync::local::liveness::answered`. LIMIT · measured with the fake
  (`sync::local::tests::a_missing_item_is_decided_by_its_object`, `…a_placeholder_dragged_out_…`,
  `…a_placeholder_moved_out_unseen_…`, `…a_folder_whose_item_is_elsewhere_…`,
  `…an_answer_that_cannot_be_placed_…`, `…a_rebuilt_base_never_deletes`,
  `sync::upload::move_out_tests::the_helpers_answer_is_read_as_the_examination_needs`,
  `…restoring_a_held_move_out_tidies_what_left`) and in the VM with the real helper
  (`move-out: …`). Open.
- **F55. The examination's shortcuts** (`konedrived/src/sync/local/`, `konedrived/src/tree/outbox.rs`) —
  (1) there is no examination until a listing has completed (a new folder, a store rebuilt): a batch
  then fails `NoBase`, and changes made meanwhile wait for the Full local scan the watcher runs after
  the first completed cycle. (2) The name pre-check blocks only what Microsoft's page names, as exact
  names (`CON`, not `CON.txt`); the service's `400` decides the rest. (3) The mass-delete guard counts
  the items a batch removes (deletes and moves out, the Trash included) with those of the removals still
  waiting in the outbox, each item once, so a trickle adds up while the worker is offline or paused;
  removals already sent, or confirmed by the user, do not count, so a trickle the worker keeps up with
  never holds. (4) A downloaded file whose cTag is not the
  base's (a new version not yet downloaded over it) and that was edited here is queued with its own
  cTag and no eTag: its upload's guard fails and the conflict rules decide. The worker guards such
  a row with the cTag. (5) From schema 3 on, a rebuilt store (an unknown version, corruption) forgets
  pending deletes, whose items come back from the cloud, and upload progress; later schema changes
  should migrate rather than rebuild. (6) A directory the daemon may not read (`chmod 000`) is passed
  over and reported: nothing in it is examined or taken for missing. (7) A row that takes a name in
  OneDrive waits for the row that frees it, whatever their order. Where that closes a circle with the
  other waits (a swap; a folder replaced by its own subfolder, `mv F/sub F.tmp && rm -rf F && mv F.tmp
  F`; a folder wrapped in a new one of its name, `mkdir t && mv d t/ && mv t d`; a folder replaced
  offline by a new one holding one of its files, `mkdir X.new; mv X/keep X.new/; …; rm -rf X; mv X.new
  X`), its waits inside the circle are dropped, so the taking row meets the name still taken (`409`)
  and only the outbox worker keeps the content safe. The outbox worker handles it: (a) on a `409` for any taking row (a
  `mkdir`, a `create`, a move to a new place), it GETs the item that holds that (parent, name); if its id
  is the `item_id` of a live row whose `outbox::frees` is that place (names without case, in any state:
  ready, waiting, retry, blocked, held or running), the name is only taken for now, and it neither
  adopts it (write design §6.2, and a replay, §10), nor makes a create/create copy (§7),
  nor retries at that name; comparing ids, not names, lets a replay still adopt our own
  folder; (b) it takes the row to `.konedrive-swap-<id>` in the target parent instead (POST for a `mkdir`
  or `create`, PATCH for a move), saving that name in the row before sending (WR7) so that a replay
  looks for it there; (c) it commits the temporary place to `items`, with a live `move` row for the final
  name, in the same step-2 transaction: a taking row left live until its final name deadlocks the
  subfolder case at run time, and without the new row nothing renames the item, which stays
  `.konedrive-swap-*` in OneDrive until a Full scan. Rules 2 and 3 are never
  dropped, so no folder is removed before what left it. FRAGILE · reasoned; (1), (3), (6) and (7)
  measured (`sync::local::tests::an_unfinished_listing_…`, `…removals_that_trickle_in_add_up_…`,
  `…confirmed_removals_…`, `…each_removed_item_counts_once_…`, `…an_unreadable_directory_…`,
  `…a_folder_replaced_by_its_own_subfolder_…`, `…a_folder_wrapped_in_a_new_one_…`,
  `…w5_fixture_folder_replaced_offline_keeping_one_file`, which drives the worker too, with
  the freer in every state and the name's case varied; the subfolder, the wrap and a swap in
  `sync::upload::tests::swaps_and_folders_replaced_in_place_…`). Open.
- **F60. The write gate: only test accounts can be read-write, until the release**
  (`konedrived/src/config.rs`, `Config::writes_allowed`; write design §2) — while uploads are being
  developed, `Account1.SetMode("read-write")` is refused `WritesNotAllowed`, and
  `Dev1.ReadWriteAccessToken` too, for any account whose drive id is not in `write_test_drive_ids`
  in `config.toml`; the list is empty by default, and nothing in the daemon writes it (the developer
  install sets it to the test account's drive by hand). An account `config.toml` sets to read-write
  by hand whose drive is not listed loads, runs read-only, asks for `Files.Read` at every refresh,
  and its `LastError` says why. No script or stray click can make the user's real account
  writable. The outbox worker asks the gate again before each row it takes and between an
  upload's fragments (`SyncService::write_gate`): the folder and the account
  read-write, `config.toml` read again saying read-write and listing the drive, the drive the
  account's token was last seen to reach being that one, the token able to write, and the folder's
  sync not stopped by another account's drive or a sign-out. Closed, nothing more is sent, the
  rows wait, the folder's `LastError` says why, and the account's mode is worked out again, which
  turns it read-only (F140) — so an edit of `config.toml` counts at the next row, not at the next
  token refresh. Writes are still addressed to `/me/drive/items/…`, not to the recorded drive:
  what holds them to that drive is the gate's comparison with the drive last seen, and the
  sync's own same-drive check at every cycle. The release removes the gate in a commit of its own,
  a user decision; that commit removes `Dev1.ReadWriteAccessToken` too, or keeps it behind the
  list (W11). LIMIT, on purpose · measured (`config::tests::the_write_gate_refuses_every_drive_by_default`,
  `konedrived/tests/mode.rs::the_gate_refuses_read_write_by_default`, `konedrivectl/tests/mode_cli.rs`,
  `sync::tests::onedrive::a_drive_taken_off_the_list_while_the_worker_runs_sends_nothing_more`).
  Open until the release.
- **F61. A read-write account is read-write only while its last token carried `Files.ReadWrite`**
  (`konedrived/src/account.rs`, `recompute_mode`; write design §2) — the mode the account runs in
  (`Account1.Mode`) is read-write only when `config.toml` says so and the gate lets its drive
  through (F60) — both from one reading of the file, taken again each time the mode is worked out,
  and a file that cannot be read then counts as read-only, with `LastError` saying so; a token used
  meanwhile is refreshed down, so the way back is a new switch — the drive the account's token was last seen to reach
  (`GET /me/drive` at a sign-in, at `RefreshAccountInfo`, by `Dev1.ReadWriteAccessToken` with
  the very token it hands out, and by a sync cycle that finds it is not the drive the folder was
  listed from, which the account then records) is the one `config.toml` records, and the scopes its last token
  response granted include `Files.ReadWrite`. The scopes and the drive seen are kept in
  `account.json`, so a restart keeps the mode; a missing or older `account.json` reads as nothing
  granted and no drive seen, and so does a sign-out. A cached token asked for with another scope
  than the one installed now is not used: after a switch or a downgrade to read-only, the next call
  refreshes down to `Files.Read`. Every refresh asks for the scope of the mode the account
  runs in, never for more than was granted: asking a refresh for more fails `invalid_grant`, which
  would sign the account out. So a read-write account that lost its grant — `account.json` deleted,
  signed out and in again read-only, a token Microsoft answered with less — runs read-only, its
  folder goes back under the lock (the switch to read-only runs, but its waiting uploads are not
  dropped: F140), and `LastError` says to switch it to read-write again, which signs in for the permission.
  A signed-out account is refused `NotSignedIn`: the first switch to read-write takes two browser
  trips, the sign-in and then the permission; a read-write account that signs in again asks for
  `Files.ReadWrite` at once. LIMIT · measured
  (`konedrived/tests/mode.rs::a_read_write_account_runs_read_write_only_with_the_grant_and_the_gate`,
  `…a_token_reaching_another_drive_than_the_recorded_one_is_never_read_write`,
  `…a_drive_taken_off_the_list_while_running_is_read_only_at_once`). Open.
- **F62. The lock walks of a mode switch** (`konedrived/src/sync/write_mode.rs`, `disk.rs`
  `unlock_tree`, `lock_tree`; write design §2.2) — a switch stops the folder's sync and walks the
  whole folder under the lifecycle lock, but fills on open and free-ups go on. (1) The walk that
  takes the lock off (Forget's) takes no inode lock: a fill that lifted a file's write bit for an
  attribute write at that very moment puts `0444` back after it, and nothing puts `0644` on in
  read-write mode, so that file stays read-only until the user changes its mode; the window is one
  `fchmod`–`setxattr`–`fchmod`. (2) The walk that puts the lock back skips a file a fill or a free-up
  holds (locking it in its attribute window would fail the fill `EACCES`); the first Full
  reconcile after the switch locks it. (3) A crash part way, or a switch made while the folder was
  not up: each walk changes the root last, so a folder whose root does not match the mode when its
  sync starts is walked again then — a read-write one unlocked (`ensure_unlocked`), a read-only one
  locked (`ensure_locked`), without Graph. (4) A walk of a large folder delays the sync's restart,
  not the D-Bus call, since the folder follows `Mode` in a task of its own. (5) An entry the unlock
  walk cannot change — a file root owns, a directory set to `000` — is logged and passed over, and
  the root then stays locked, so every start walks again and fails again; only the log says so, not
  the folder's `LastError`, and the read-write sync fails `EACCES` where that entry is. (6) In
  read-write mode the lock comes off as the sync starts, and only once the watcher's own walk has
  marked every directory (Z2), so no directory is made before it is watched; both walks run under
  the lifecycle lock. A folder whose watcher cannot start, or whose walk is cut short, stays locked
  and runs that sync as a read-only one, and `LastError` says why; one whose sync cannot start at
  all (no drive configured, a tree store that cannot be opened) is locked, in either mode; a switch
  to read-write while the folder's sync does not run leaves it locked until the sync starts. A root that is unlocked already
  (a daemon start) does not wait for the walk, so fills are not held up behind it. FRAGILE ·
  reasoned; the walks, (3) and (6) measured
  (`sync::tests::onedrive::the_lock_comes_off_and_goes_back_on_with_the_mode`,
  `…a_read_write_folder_whose_watcher_cannot_start_stays_locked`,
  `…a_read_write_folder_whose_sync_cannot_start_is_locked_again`,
  `konedrivectl/tests/sync_cli.rs::the_folder_follows_the_accounts_mode`). Open.
- **F64. The switch to read-write ends in `Mode` or `LastError`, and nothing says it is under way**
  (`konedrived/src/account.rs`, `set_mode`; `konedrivectl/src/lib.rs`, `wait_for_read_write`) —
  `SetMode("read-write")` answers the sign-in URL at once, and the account stays `signed-in`
  throughout, so a client learns the outcome by watching `Mode` turn `read-write`, or `LastError`
  say why not (`SetMode` clears it before it answers; a cancel says nothing). `account mode
  read-write` polls both: a `LastError` set meanwhile for another reason — account info that could
  not be loaded — ends its wait with that message, though the switch may still go through. A second
  `SetMode("read-write")` gives up the first one's sign-in. FRAGILE · reasoned. Open; the window
  keeps the wait itself (A16).
- **F65. A mode the user gave a file does not survive a round trip through read-only**
  (`konedrived/src/sync/disk.rs`, `lock_tree`, `unlock_tree`; write design §2.2) — the switch to
  read-only puts `0444`/`0555` on every file and directory that is konedrive's, and the switch back
  puts `0644`/`0755` on everything, so an executable bit or a private `0600` given in read-write mode
  is gone after it. LIMIT · reasoned. Open.
- **F66. Consent to write stays with Microsoft, and a read-only request may be answered with it**
  (`konedrived/src/oauth.rs`, `pinned_authorize_url`; `account.rs`, `record_granted`; write design
  §2.2) — the consent a sign-in gives for `Files.ReadWrite` is kept by Microsoft for the Microsoft
  account that gave it, whatever konedrive does with the token; only
  https://account.live.com/consent/Manage takes it back. Every sign-in that asks for
  `Files.ReadWrite` is pinned to the account being switched — the password asked for again
  (`prompt=login`), its email filled in (`login_hint`) — so a browser signed in to the user's real
  account cannot consent for it with one stray click, and a sign-in that reaches another drive is
  refused before anything is stored. Consent given anyway stays. That a read-only refresh then
  answers with a `Files.Read` token is assumed, not measured: should Microsoft answer with more, the
  token is used to read only, what is recorded as granted is never more than was asked for (so it
  can never make the account read-write), `Dev1.AccessToken` refuses to hand it out, and
  `LastError` says so and names the page. The test-account run (F130) checks it: the token exported
  after the switch back to read-only must be refused a write. LIMIT · reasoned; the handling
  measured with a fake endpoint
  (`konedrived/tests/mode.rs::a_read_only_request_answered_with_write_access_stays_read_only`). Open until
  that run (F131).

- **F70. A full notification queue costs a Full local scan** (`konedrived/src/sync/watcher/`) —
  an unprivileged group keeps 16 384 events (`fs.fanotify.max_queued_events`), then one
  `FAN_Q_OVERFLOW` that says nothing of what was lost (kernel §14.2). Unpacking a big archive into
  the folder can do it. The watcher then examines the whole folder and walks it again, which marks
  (and asks the helper to mark) any directory whose event was lost. It costs a scan, never a missed
  change, except F52's. LIMIT · measured
  (`sync::watcher::tests::an_overflow_is_a_full_scan_and_a_walk_that_marks_what_was_missed`). Open.
- **F71. The watcher's marks come from a budget every account shares** (`konedrived/src/sync/watcher/`) —
  an unprivileged group may not ask for unlimited marks. `fs.fanotify.max_user_marks` counts per
  uid, across every group of that user (every account's folder, every subvolume's group, any other
  program of the user that uses fanotify), and scales with memory: 597 240 on the host, 36 399 in a
  4 GiB VM. A mark refused `ENOSPC` puts the folder in a degraded mode: the directories marked so far
  keep their events, the rest are found by a Full local scan and a walk of the folder every 10
  minutes (`DEGRADED_SCAN`), and `LastError` says so. The 128 groups a uid may hold
  (`fs.fanotify.max_user_groups`, `EMFILE`) are the same kind of limit: a folder or a subvolume
  that gets no group is scan-only. LIMIT · measured (the kernel's numbers, kernel §14.2; the degraded
  mode with a lowered test budget,
  `sync::watcher::tests::past_the_mark_budget_the_folder_is_scanned_on_a_timer`). Open.
- **F72. Nothing on another device than the folder is uploaded** (`konedrived/src/sync/local/examine.rs`,
  rule 2b; `sync/watcher/reader.rs`, `elsewhere`; `konedrive-helper/src/roots.rs`, `may_act_on`) — a
  nested Btrfs subvolume has its own filesystem id and device number, and so has a filesystem mounted
  inside the folder. The helper marks (`MarkDir`) only directories on its root's device (`EPERM`
  otherwise, measured), so a directory made there has no permission mark until the helper's next
  registration walk, and a placeholder placed or moved into it could read empty. So nothing on another
  device is uploaded: the examination lists it once in `local_skipped` as `other-device` and makes no
  row for it or anything below it, so it never gets an item id and nothing from OneDrive is ever placed
  in it; the watcher neither watches it (no notification group) nor asks the helper to mark it, counts
  it (`WatchStatus::other_device`), and `LastError` says so. No placeholder reaches it either: a move
  onto another device is a copy (`rename(2)` and `link(2)` fail `EXDEV`), whose read of the source is
  intercepted and filled. Another filesystem mounted over a directory that is already synced hides
  the directory's entry, and its base item then looks missing: the helper finds the hidden
  directory, but its path opened again is the mount's root, another inode, so nothing is decided
  and the item stays in OneDrive (F54). Such a base item should count as present and not be
  examined; it does not yet. LIMIT · measured (VM
  scenario `watcher: a nested btrfs subvolume is neither watched nor uploaded`, which also records the
  helper's answer). Way out: the helper accepts a directory beneath a root of the uid on another device
  (for instance by walking `..` from the passed descriptor up to the root's device and inode); then
  such a folder can be watched and uploaded like the rest. Open.
- **F73. A write through a hard link outside the folder raises no event** — marks are on
  directories, and a write through a name in an unwatched directory is not reported through the
  folder's (kernel §14.3), nor is making such a link. The change is found by the next Full local
  scan (a daemon start, a helper reconnect, an overflow), and not at all if it kept size and time
  (F52). LIMIT · measured (the kernel probe). Open.
- **F74. The watcher's shortcuts** (`konedrived/src/sync/watcher/`) — (1) a directory that leaves the
  folder keeps the watcher's mark, and its share of F71's budget, until it is deleted or the watcher
  stops: only the kernel can take a mark off a directory the daemon can no longer open. Its events
  are passed over, for up to 65 536 such directories; past that an event from one costs a walk of the
  folder, at most once a minute (`UNKNOWN_WALK`), as does an event from any directory the map does
  not know. (2) A directory the daemon may not open (`chmod 000`) when the watcher meets it is kept
  in the map unwatched, with what the map had below it; the next event on the directory itself (a
  `chmod` back) adopts it with everything below it, and a directory that could not be looked into is
  walked again at most once a minute. (3) The daemon's own changes are told apart by pid alone, so
  every change the daemon makes in a read-write folder, a fill's commit included, must be made by
  the daemon process itself (any thread), never by a child process or by the helper (write design
  §3.2). The daemon's own changes are never examined through their events either: a conflict copy
  the reconcile makes, and what it keeps or makes local, is handed to the examiner by the cycle
  itself (F114). A folder the reconcile makes
  is examined once, whole, in case someone put something in it before it was marked. (4) The
  examination runs on the watcher's own thread, and only the first cycle waits for it (F117):
  otherwise it relies on the examination's rules for an item only in `staging` or placed mid-cycle. (5) An
  ignore-list change applies from the watcher's next examination, followed by a Full local scan
  (F102). (6) What the watcher gathered and had not handed over when it stops is dropped
  unless `flush()` ran first (a switch to read-only flushes before it asks `PendingUploads`); the
  next start's Full local scan finds it (except F52's). A change made between that flush and the
  switch's stop is dropped with the rows when the switch is forced, as the switch to read-only does
  (write design §2.2) (F140).
  A flush that meets a watcher
  stopping ends at once, unanswered. (7) The folder moved or deleted stops its
  sync and the watcher and reads `error`; nothing starts them again until the daemon brings the
  folder up again (a restart), and nothing is deleted in OneDrive because it went. A slow `rm -rf`
  of the folder itself can hand over batches of deletes before the root's own event arrives; only
  the mass-delete guard stands before them; the deletes queued in the last
  `CEILING` before the root's event are not held back. (8) A filesystem mounted inside the folder is another device, as
  a subvolume is (F72): neither watched nor uploaded. (9) A stop waits for the examination under way (not interruptible) and for a
  walk up to its next directory; a helper that stops answering costs one `MarkDir` timeout (30 s)
  per walk, after which the walk stops asking and leaves the rest to the retry. (10) The unlock walk
  of a switch to read-write runs right after the watcher's walk, and raises one `FAN_ATTRIB` per entry:
  the daemon's own, dropped, but on a large folder they can overflow the queue, which costs a second
  Full local scan and walk. (11) A watcher that ends after its walk with nobody asking it to (a bug)
  says so in `LastError`, but leaves the lock off: the folder is not locked again, and nothing more
  is looked for until its sync starts again. FRAGILE · measured
  for (1), (2), (3), (6) and (7) (`sync::watcher::tests::a_move_across_the_border_…`,
  `…a_directory_closed_at_the_walk_is_watched_all_the_way_down_once_opened`,
  `…the_daemons_own_changes_and_a_fill_raise_nothing_to_examine`, `…a_flush_examines_what_is_pending_at_once`,
  VM `watcher: the daemon's own placement and fill are not handed over`,
  `…the_folder_moved_away_stops_the_watcher_and_says_so`,
  `…a_read_write_folder_gets_a_watcher_and_a_folder_moved_away_says_so`); reasoned for the rest.
  Open.
- **F75. A size change by path, and a write through a mapping, raise nothing the watcher reads**
  (`konedrived/src/sync/watcher/fan.rs`, `DIR_MASK`) — `truncate(2)` by path opens nothing, so it
  raises `FAN_MODIFY` only (kernel §14.3), and the watcher does not subscribe to `FAN_MODIFY`: it
  would wake on every `write(2)` of every file being written, and a file written for a long time would
  hold off every batch until `CEILING`. A write through a shared mapping after the descriptor is
  closed raises no event at all. Either change is found by the next Full local scan (a daemon start,
  a helper reconnect, an overflow). Tools that open the file to truncate it (coreutils `truncate`, an
  editor) close it for writing, and are seen (`FAN_CLOSE_WRITE`). LIMIT · reasoned from the probe's
  record (the truncate by path: measured, kernel §14.3). Open.
- **F80. The last fragment of a large upload can supersede an edit made in OneDrive meanwhile**
  (`konedrived/src/sync/upload/content.rs`, write design §6.3) — `If-Match` is checked when an upload
  session is created, not when it completes. Before the last fragment the worker probes the file for a
  writer, compares it with its snapshot and reads the item's eTag again; an edit made in OneDrive
  between that read and the last fragment is overwritten by the upload. OneDrive keeps it in the
  item's version history, so nothing is lost, but it is not "keep both" either. LIMIT · reasoned (the
  test-account run observes it, F131). Open.
- **F81. A file kept open for writing is not uploaded until it is closed**
  (`konedrived/src/sync/upload/content.rs`, `sync/local/examine.rs`, write design §4.3) — a log, a
  database or a running VM image held open for writing keeps its row `waiting` (`open-for-writing`),
  probed again every 30 s, and it goes up once the writer closes it. Windows behaves the same. LIMIT ·
  measured (`sync::upload::tests::pause_offline_sign_in_and_blocked_rows`). Open.
- **F82. The outbox worker's shortcuts** (`konedrived/src/sync/upload/`,
  `konedrived/src/tree/outbox/worker.rs`) — (1) `move-out` rows run only in a worker
  given the helper and the fills they need (`MoveOuts`, F121; the daemon's always is); in any
  other they wait, and a folder's delete that waits for one waits with it. (2) `user.konedrive.sync`
  is kept for the rows this run of the daemon has seen: a row that went while the daemon was not
  running (dropped by an examination) leaves its mark on the file until that file's next commit; the
  rows a forced switch to read-only drops have their marks taken off. A row that goes while the daemon runs has its mark cleared where the
  row last saw the file and where the base has the item. Directories get no mark. (3) Commit step 1 finds the local object
  where its row saw it, or where a row behind it saw it since. A create whose file was moved while the
  daemon was down, after its upload landed and before its replay, is uploaded again as new: a duplicate
  in OneDrive, nothing lost. (4) A create, `mkdir` or move whose folder is gone from OneDrive backs off
  (`parent-not-in-onedrive`) and asks for a cycle; the reconcile keeps the folder with the local work
  in it, made local, and the examination's `mkdir` makes it again (F116). (5) An item taken
  through a temporary name is committed to the base under `.konedrive-swap-*`, as placed, although a listing skips that name (reserved); other
  devices see that name until the final `move` runs. A store rebuilt in between forgets the final
  `move`: the item stays under the temporary name in OneDrive, and the local folder's id then names an
  item the listing skips — nothing is deleted: the read-write reconcile leaves the local object where
  it is (it is not the tree's), and the examination takes it for a move back to its name.
  A row on its way through a temporary name keeps it across an examination's merge while the object
  stays where it was; if the object moved on, the row goes to its new place, and an item already
  under the temporary name is moved from there (a `mkdir`'s or a create's leaves an empty folder or a
  duplicate under that name). (6) Rename × rename, and an edit of an item renamed in OneDrive: the
  worker renames the local object to OneDrive's place itself — the reconcile's job, done at once
  because the base takes OneDrive's place at once. Where that is impossible (OneDrive's folder is not
  placed here, the name is taken here, or it is a name no listing places, `.konedrive-*` included)
  the local place stands and is sent again against the fresh eTag, so the second rename wins after
  all. (7) A move adopted on a `412` (its earlier PATCH landed, or OneDrive's place won) commits
  OneDrive's answer as it is, a newer cTag included; the file keeps its own `user.konedrive.ctag`, and
  the next cycle looks again at every item the outbox committed since the last one and replaces such
  a file (F117 for the time in between). (8) Where OneDrive's version wins or both are kept
  (delete × edit, edit × edit), the item's recorded handle is forgotten, so
  that its empty name is placed again rather than taken for a delete; the next cycle places it
  (items with no local object on record are looked at again). (9) A conflict copy is a rename, then the
  attributes taken off, then one store transaction. A crash between the rename and the transaction
  leaves the renamed file carrying the item's id: the next examination takes it for a move (or an
  update) of the item, which a replay turns into a second copy, or which PATCHes OneDrive's version to
  the copy's name, or which stays `not-found` and keeps the item from being placed again at its name
  until a Full scan. No byte is lost. (10) A folder's delete is one `DELETE` of the folder itself,
  sent with no guard at all, whatever OneDrive gained or changed below it since the delete was
  decided: as Windows deletes a folder, the folder goes whole, and OneDrive's recycle bin is the
  safety net (`docs/design/decisions.md`, "A folder delete is the whole folder, as on Windows"). No
  per-item request is ever sent for what is inside; a `404` on the folder's own `DELETE` means it is
  already gone, which is success too. (11) The worker chooses the next rows by recomputing the outbox's dependencies after
  every row it finishes: a large first upload costs CPU growing with the square of the outbox. (12) A
  row rewritten and sent again at once (a temporary name, a copy, a fresh guard) more than 20 times
  backs off like a failure. (13) An answer whose content hash is not the one sent is never committed:
  a changed file is sent again from zero against the version the bad upload made; a new file's bad
  item is deleted first, and if that fails its id is kept in the row's reason
  (`hash-mismatch:<id>`), so the next run deletes it before sending again (or adopts it, should it hold
  this content after all). (14) The worker's own
  reads (the item after a `412`, the name's holder after a `409`, a folder's children) wait out a
  throttle in place, as the cycle's reads do, and then back off that row: they do not pause the whole
  worker (write design §6.2 asks it only of writes). (15) Commit step 1 is skipped when another inode stands at
  the file's name by then (an editor's backup-and-rewrite save during the upload): the item is
  committed without a local object, and the row behind it uploads the new inode. (16) Edit × edit on
  an item whose swap PATCH had landed: the local version becomes a copy, and OneDrive's version stays
  under `.konedrive-swap-*`, which no listing places and no row renames, until the user renames it in
  OneDrive. FRAGILE · measured
  for (3)'s replay, (6), (8) and (10) with a fake OneDrive (`sync::upload::tests::every_crash_point_…`,
  `…conflicts_keep_both_…`, `…a_folder_changed_in_onedrive_…`, `…a_folder_delete_never_takes_…`,
  `…a_folder_holding_what_was_never_placed_…`, `…a_row_through_a_temporary_name_…`); reasoned for the
  rest. Open.
- **F90. `OpenByHandle` gives a user their own object wherever it went** (`konedrive-helper/src/by_handle.rs`,
  write design §8.2; SECURITY.md) — the helper opens a file handle for any local user. It opens it
  relative to a directory that user owns on a filesystem where they have a registered folder,
  and hands the object back if it is:
  - their own regular file or directory;
  - on that directory's device;
  - still linked;
  - carrying `user.konedrive.item-id`.

  Anything else is `EPERM`; the asker's own deleted object is `ESTALE`. A handle bypasses path
  lookup. So a user can reach such an object of theirs in a directory they cannot enter, for
  instance one another user moved it into. It is theirs and konedrive's, which is the accepted
  residual (SECURITY.md). For a directory the descriptor anchors `*at()` calls, so it reaches the
  whole subtree below it as far as the user's own permissions go, not only the one inode; the
  daemon never lists or opens anything beneath it, and walks a moved-out directory only after
  opening it again by its path (F121 (5)). The answer also tells a handle that names an existing inode from one
  that names nothing (`EPERM` against `ESTALE`), for any inode on that filesystem. Handles are
  guessable, so this says that an inode exists, never its name or content. Refusals are not
  logged: any user can ask, and the asker gets the errno. The protocol went to version 2 with
  this message. A helper and a daemon from either side of the change refuse each other (F11),
  so the two are upgraded together. An object in a nested Btrfs subvolume (F72) is on a device
  where the helper holds no root of the user's, so it is refused `EPERM` whatever directory is
  passed: `EPERM` must never be read as "gone" (F54, F121). LIMIT · measured (VM `OpenByHandle refuses another uid's
  object, …`; the unit check, a placeholder in a root-owned `0700` directory handed to its
  owner). Open, accepted.
- **F91. A moved-out file is handed over read-only, and the daemon reopens it for writing itself**
  (`konedrived/src/sync/helper.rs`, `reopen_for_writing`) — the design asked the helper for
  `O_RDWR`. Under the unit the helper is root without `CAP_DAC_OVERRIDE`, so a user's `0644` or
  `0600` file is `EACCES` for writing (measured, kernel §15). Adding that capability would let the
  one root process every local user talks to write any file. So the helper opens a regular file
  `O_RDONLY | O_NONBLOCK`, and the daemon, its owner, reopens `/proc/self/fd/<fd>` read-write,
  which checks the file's own permissions and no directory's. What that costs:
  - a file its user made read-only (`0444`) needs the daemon's write window (`with_owner_write`,
    a `fchmod` as owner through the read-only descriptor) before the reopen;
  - the reopen is an open like any other. On a file with a mark it is intercepted, and let
    through at once only as this daemon's own open (the uid's newest connection, holding a
    root); otherwise it waits for a fill like any opener;
  - `O_NONBLOCK` makes a file under someone's write lease answer `EAGAIN` rather than hold the
    connection's thread for up to 45 s (kernel §12.4). A `move-out` row tries it again after
    `RECHECK` (30 s).

  WORKAROUND · measured (the unit check: the reopen of a placeholder in a directory its owner
  cannot enter, and the write; `sync::helper::tests::a_read_only_descriptor_is_reopened_…`).
  Open.
- **F92. The helper lets its own opens through without deciding them** (`konedrive-helper/src/main.rs`,
  `event_loop`) — an `OpenByHandle` object can sit in a marked directory or carry a mark of its
  own, and the helper's open of it raises `FAN_OPEN_PERM` in its own group, with its own pid
  (measured, kernel §15). Decided like any other, a placeholder would become a `HydrateRequest`
  to the very daemon whose connection thread is waiting in that open. Its `HydrateDone` would
  never be read, so the open would never return: the daemon's call timeout ends the socket, not
  the open. So every event with the helper's pid is allowed in the event loop, before the worker
  pool. It then reads whatever the file holds, zeros for a placeholder, which is safe only
  because the helper reads no file content and hands the object straight to its owner's daemon.
  The only other file it opens is its feature probe's nameless file. A change that makes the
  helper read a file it opened would read zeros in silence. WORKAROUND · measured (VM
  `OpenByHandle of a placeholder under the helper's own marks returns at once and fills nothing`:
  0.01 s, no fetch, no ignore mark). Open.
- **F100. The pause is the tree store's** (`konedrived/src/sync/outbox_api.rs`, `upload::paused`; write
  design §11) — `Pause`/`Resume` write `meta.paused_until` in the account's tree store, not
  `config.toml`: a store that is rebuilt (an unknown version, corruption) forgets the pause, and a
  folder with no store — a local one, or a OneDrive folder whose sync has not started — cannot be
  paused (`Unsupported`, `NoRoot`). While paused, the poll asks OneDrive for nothing, and the first
  cycle after a restart waits too; `Refresh()` asks for nothing either. A cycle already running when
  the pause comes starts no replacement (the next cycle after the pause is Full and finds them
  again), and an upload in fragments stops at its next fragment and resumes its session after the
  pause; a one-request upload or a metadata request already sent finishes. A timed pause is looked
  at by the wall clock at least every minute, so a suspend does not stretch it; its timer ends it on
  the bus only if no `Pause` or `Resume` came after it read the store; a forgotten folder is no
  longer paused. The tray's "pause every account" is one `Pause` per account (A23).
  LIMIT · measured (`sync::tests::onedrive::a_pause_holds_the_poll_outlasts_a_restart_…`,
  `…a_forgotten_folder_is_not_paused`, `…a_pause_that_lands_as_the_last_one_ends_stands`). Open.
- **F101. A coalesced property that changes and changes back is not signalled**
  (`konedrived/src/sync/dbus.rs`, `coalesce`) — the counters, `Transfers`, and
  `PendingCount`, `PendingBytes`, `BlockedCount`, `HeldCount` and `Uploads` are sent at most four
  times a second, compared with what was sent last. A value that changes and changes back within
  one 250 ms window (a small file queued and uploaded at once) sends nothing, and a client that read
  the property in between (a `GetAll` at that moment) keeps the passing value until the next
  change. `Paused` and `PausedUntil`, which a quick pause and resume would otherwise leave stale,
  are signalled at once instead. FRAGILE · measured for the pause (the flip was lost before the
  change: `konedrivectl` `binary_pauses_resumes_and_keeps_the_ignore_list`). Open.
- **F102. The outbox on the bus: what it simplifies** (`konedrived/src/sync/dbus.rs`, `outbox_api.rs`;
  `konedrivectl sync …`) — (1) `SetIgnorePatterns` refuses only an empty pattern, one holding `/`
  or a NUL, and one longer than a name; a pattern that matches nothing is kept. Once set, the list
  is written in full to `config.toml`, so a later change of the built-in defaults does not reach
  that account. It applies to the watcher's next examination, and a Full local scan follows: a
  name no longer ignored is uploaded; a file newly ignored loses its create row (a row for an item
  already in OneDrive stays: it syncs whatever its name); a directory of the user's own newly
  ignored stays local with everything in it, and the new things waiting inside it lose their rows —
  unless the worker is making that directory in OneDrive right now: it is an item then, and what is
  in it goes up.
  A OneDrive file moved into such a directory is not seen there: its move waits until the helper can
  say where it went (F54). `config.toml` and the list the watcher reads change together, under one
  lock; `sync ignore add`/`remove` read, change and set the whole list, so two of them at once can
  lose one change. (2) `MachineName` is read-only on the bus: `machine_name` in `config.toml` sets
  it. (3) `ConfirmDeletes` and `RestoreDeletes` answer how many rows they released or dropped, and
  `HeldCount` (`u`, coalesced) counts what waits for
  them. `ConfirmDeletes` releases every removal held at the moment of the call, one held after the
  caller last looked included. `RestoreDeletes` forgets the items' local objects in both the base
  and a cycle's staging, under the tree lock, and asks for a cycle with a Full reconcile at once,
  which places them again (items with no local object on record, F115); it deletes nothing in OneDrive.
  (4) `BlockedCount` does not count held removals: `HeldCount` does. (5) `Outbox()` reads a waiting
  file's size with `lstat` for each call, off the runtime; `sync outbox` shows the first 50 rows
  unless `--all`. (6) A free-up of a downloaded file named on its own whose change waits to be
  uploaded is refused `NotUploaded`, before any account frees anything; inside a folder, such a file
  is left and counted as busy. The check goes by the file's item id and its object, under the file's
  inode lock, and fails closed: a OneDrive folder whose outbox cannot be read (its sync not started,
  a store error), or a file whose state cannot be read, refuses the free-up. A file not downloaded is refused `NotHydrated` as before, row
  or not. (7) When the outbox worker stops, or its rows are dropped, its counts on the bus read 0;
  the next worker counts again. (8) `NotUploaded` and the `Transfers` direction column are all the
  CLI shows of the blocked and running rows; the mass-delete guard writes no activity event of its
  own: the window learns of it from `HeldCount`. LIMIT · measured
  (`sync::tests::onedrive::the_outbox_is_listed_decided_on_and_its_files_are_not_freed_up`,
  `…restoring_held_deletes_brings_the_files_back_at_once`, `…an_ignored_directory_keeps_everything_in_it_local`,
  `…a_free_up_that_cannot_tell_whether_a_change_waits_refuses`, `…the_ignore_list_is_kept_in_config_toml`,
  `tree::outbox::tests::dropping_held_rows_survives_a_cycles_swap`,
  `sync::local::tests::ignoring_a_directory_being_made_keeps_what_is_inside_it`). Open.
- **F110. A replacement waits while the file is open anywhere** (`konedrived/src/sync/materialize.rs`,
  `replace_leased`; write design §9) — in a read-write folder a downloaded file is replaced by
  OneDrive's new version only under a write lease, taken before the download (so nothing is fetched
  for nothing) and again at the swap, with the tree lock: a program writing into it across the rename
  would write into the unlinked old inode. At the swap the lease comes first, and the file is looked
  at again under it (through its descriptor, and by name without opening it), so a write that landed
  just before is a stamp mismatch, never swapped away; a download emptied here counts as changed too.
  While anything has it open the replacement is `Busy`, not a
  failure: nothing is said, the base keeps the version on disk, and every cycle tries again. A file held
  open for long (a mailbox, a database) keeps its old version as long; a lease refused between the
  probe and the swap costs one download. Without `CONFIG_IMA` the kernel does not count readers, so
  the lease sees writers only. Read-only folders are unchanged. LIMIT · measured
  (`sync::listing::rw::tests::a_replacement_waits_for_a_file_open_for_writing`). Open.
- **F111. The `410` upload variant removes placeholders the service lost** (`konedrived/src/sync/listing.rs`,
  `fetch_changes`; `drive/mod.rs`, `resync`; write design §9) — Graph's `410` names
  `resyncChangesApplyDifferences` or `resyncChangesUploadDifferences`, told apart by the body. After
  the first the drive is listed again and the folder made to match, keeping local work as every
  read-write reconcile does. After the second, what the new listing left out is not removed where it
  was downloaded: the file stays, stripped, and the examination uploads it as new (a new id); a
  downloaded file whose version differs from the listing's is kept beside it as a conflict copy; a
  placeholder, which holds nothing here, is removed, and that is only logged. A read-only folder takes
  both as the first, as before. LIMIT · measured
  (`sync::listing::rw::tests::the_two_resyncs_differ_in_what_the_listing_left_out`). Open.
- **F112. What waits for a local change is staged again at every cycle** (`konedrived/src/tree/reconcile.rs`,
  `sync/listing/rw.rs`; write design §9) — an item the read-write reconcile leaves as it is on disk
  (a live outbox row in any state; below a folder a `move` row takes; a local move or copy not
  examined yet, with what is below it; a replacement not landed; a file being filled) keeps its base
  row, and the delta's entry waits in the store's `deferred` table, dated by the outbox commit count its
  fetch started at, or by the item's last commit when that is later (what the stale-delta guard read
  again is newer than that commit, F113). Where the disk took OneDrive's move and only the content
  waits (a replacement, a fill), only the content does: the base takes the new place. A local move or
  delete not examined yet waits too when OneDrive removes the item, so that the outbox meets OneDrive's
  side (§7) — a delete then costs one `DELETE` answered `404`. Every cycle stages what waits before its
  own delta — a cycle with anything waiting
  that is not held by a live row therefore copies `items` to `staging` even when the delta is empty —
  until the disk takes it; an outbox commit after its fetch supersedes it. Below a folder a `delete` or
  `move-out` row removes, the delta goes to the base at once and nothing is placed.
  A read-only start applies what waits to the base at once, for its first Full reconcile to place: a
  read-only cycle knows no deferred change. Items with no local object on record, and those the outbox
  committed since the last cycle, are looked at at every cycle too; on a filesystem that gives no file
  handles every placed item is one, which costs a lookup each per cycle (such a folder cannot be watched
  anyway, F72). DEBT · measured
  (`sync::materialize::rw::tests::rows_keep_the_reconcile_off_their_items_…`, `…a_local_move_not_examined_yet_…`,
  `sync::listing::rw::tests::a_change_that_waited_for_a_row_is_applied_once_the_row_is_gone`,
  `…a_change_read_again_survives_a_replacement_that_waits`, `tree::reconcile::tests`; the fake
  OneDrive's delta is a cursor, as Graph's is). Open.
- **F113. The stale-delta guard reads again, under the tree lock, what the outbox committed during a
  fetch** (`konedrived/src/sync/listing/rw.rs`, `guard_delta`; write design §9) — the design drops a
  delta entry for an item committed after the fetch began; one that is not the commit itself (another
  eTag), a delete of it, or an entry for an item the outbox deleted (by its tombstone, `outbox_gone`) is
  read again with `GET /items/{id}` instead, and that answer staged: it is newer than both, so a change
  OneDrive made just after the commit is not lost. A full listing reads again every item committed while
  it was listed. The reads hold the tree lock, so an outbox commit waits for them; a read that fails
  fails the cycle (the next one is Full). WORKAROUND · measured
  (`sync::listing::rw::tests::a_delta_fetched_before_a_commit_does_not_undo_it`). Open.
- **F114. The reconcile's conflict copies go up through the examination** (`konedrived/src/sync/materialize/rw.rs`,
  `copy_aside`; write design §7) — where the read phase rescued, a read-write reconcile renames the
  local object in its directory to `name-<machine>.ext` (never over anything), takes konedrive's
  attributes off and records a conflict of kind `copy`; its upload is the examination's, which the cycle
  asks for by handing the watcher the copy's place (the daemon's own renames raise no event it keeps).
  Without a running watcher the copy waits for the next Full local scan; a copy whose name the ignore
  list matches is not uploaded, as its original was not. A new file where a new remote item arrives is
  copied even when its content is the same: only the outbox worker, meeting `409` on a `create` row,
  adopts by hash — so a local file already examined (a live `create`) is left to it, and the remote item
  waits. A file that replaced an item OneDrive did not change (a save by rename not examined yet) is
  the user's, never copied; a folder made here where OneDrive has a new one of that name is never
  copied either: the two merge by the `mkdir`'s `409`, and OneDrive's side waits for it. FRAGILE ·
  measured
  (`sync::materialize::rw::tests::a_local_file_in_the_way_…`, `…an_edit_here_and_in_onedrive_keeps_both`). Open.
- **F115. A missing item is placed again only with something to place** (`konedrived/src/sync/materialize/rw.rs`,
  `place_again`; write design §9, §7) — a tree item missing from its place in a read-write folder is a
  delete or a move the examination has still to see, and is left, unless it is new or has no local
  object on record; a folder is placed again when something below it is. One whose local object is on
  record is never placed again by the reconcile, even when OneDrive changed it: the object may be
  alive elsewhere in the folder, or out of it (a placeholder moved out, which only a `move-out`
  row marks and downloads — placed again, it would read zeros for good). Its base place goes to the
  examination, which decides by the object; the outbox then meets OneDrive's change (a delete or a
  move out answered `412` is dropped and its object forgotten — delete × edit: OneDrive wins), and
  the next cycle places the item again. Until then the item stays away, and without an examination
  (no watcher) until the next Full local scan. An object carrying an id the base does not
  have is never removed by the reconcile — it may be another account's, whose outbox has still to
  fetch it (§9) — so after the tree store was lost, what OneDrive removed meanwhile stays here too:
  downloaded, the examination uploads it again as new; a placeholder is listed as not downloaded. And
  local moves made while the store was lost are undone by the Full reconcile after the new listing:
  with no base, nothing tells them from OneDrive's. LIMIT · measured
  (`sync::materialize::rw::tests::a_missing_item_is_placed_again_…`, `…what_onedrive_removed_goes_…`,
  `sync::listing::rw::tests::a_placeholder_moved_out_and_changed_in_onedrive_is_downloaded_where_it_went`).
  Open.
- **F116. A folder removed in OneDrive that holds local work is made again as a new one**
  (`konedrived/src/sync/materialize/rw.rs`, `remove_in_place`; write design §9, §7 folders) — a
  read-write reconcile removes what OneDrive removed in place: a clean placeholder goes, a downloaded
  file only under a write lease, a changed file stays (stripped: uploaded again as new), and a folder
  that keeps local work — something the examination will upload, or a local change a row or an
  unexamined move holds — stays, its attributes off. A folder that keeps only what is not local work
  — a file open somewhere or being filled, an ignored name, a symlink, an object from elsewhere —
  keeps its id and base instead, and its removal waits (a folder of only such things waits for as long
  as they stay); nothing is made again in OneDrive for them. Where a folder keeps both, a clean file
  that was open goes up again with the local work, as a new item. Rows that were to go into it wait for its `mkdir`,
  which the examination records when the cycle hands it the folder, and the outbox makes the folder
  again — a new item: the old one's history and sharing links stay with it in the recycle bin. Without a
  running watcher that waits for the next Full local scan. LIMIT · measured
  (`sync::listing::rw::tests::a_folder_removed_in_onedrive_with_local_work_in_it_is_made_again`,
  `sync::materialize::rw::tests::what_onedrive_removed_goes_unless_it_holds_local_work`,
  `…a_folder_removed_in_onedrive_waits_for_what_is_in_use_…`). Open.
- **F117. The order of a read-write folder's cycle, and what it cannot close** (`konedrived/src/sync/listing/rw.rs`,
  `sync/upload/engine.rs`; write design §2.2, §3, §9) — the first cycle of a read-write folder waits
  for the watcher's first examination (its Full local scan, or its `NoBase` answer on a new folder), so
  that changes made while the daemon was down are rows before the Full reconcile; the outbox worker
  sends nothing until a cycle has gone through, at start and again after the network came back. A
  cycle that keeps failing (the helper away, OneDrive unreachable) holds uploads as long. A Full
  reconcile comes at bring-up, after a helper reconnect, after a switch, after `RestoreDeletes`,
  after a delete that OneDrive's change undid (the item must be placed again), on the first cycle
  after a pause that held back a replacement, and when a Changed cycle finds something new to place
  in a folder that is not where the tree has it; a pause lets changes wait for hours, so the first
  cycle after it meets more of them. The examination holds the tree lock too, so a reconcile and an
  examination never see each other's changes half made; a stop ends the cycle, then the outbox
  worker, then the watcher, so that none waits for another. What remains are stalls: a Full local
  scan (hashing, helper round trips) holds up cycles and commits, a cycle's re-reads from Graph hold
  up the examination, and an outbox commit waiting for a file a fill holds keeps both waiting as
  long as that download. What a reconcile moved to the holding directory never leaves the
  folder: a stop or a crash in between leaves it for the next Full reconcile, which places it or puts
  it back where the base has it — under a copy name beside its place, or in the root, when that is
  taken — so it is never taken for a move out. A put-back under a copy name reads to the examination
  as a move: where OneDrive moved the item, the move's guard fails and OneDrive's place wins; where a
  local file took the name, the item is renamed in OneDrive to the copy name — only a name changes. A
  new folder a stop left under its temporary name whose item OneDrive removed meanwhile goes, and
  whatever someone put in it is put back where it stood. A first listing placed page by page makes
  `staging` again from `items` under the lock at a page when an outbox commit wrote `items` since the
  last one; what a page's reconcile leaves unsettled still goes
  into `items` with the page (a first listing has no local changes to keep, but a file being filled).
  For a moment the base can still run ahead of a file: an outbox commit that adopts OneDrive's newer
  version (F82 (7)) until the next cycle's replacement lands. FRAGILE · measured
  (`sync::listing::rw::tests::the_first_cycle_waits_for_the_scan_and_the_outbox_for_the_cycle`,
  `…the_cycle_holds_the_tree_lock_from_staging_to_the_swap`, `…a_changed_pass_handing_over_with_something_in_holding_…`,
  `…what_a_stop_left_in_the_holding_directory_…`,
  `sync::materialize::rw::tests::a_new_folder_a_stop_left_under_its_temporary_name_…`); reasoned for
  the rest. Open.
- **F120. A placeholder moved out of the folder reads zeros until the daemon marks it again**
  (`konedrived/src/sync/upload/move_out.rs`, `Engine::protect`; write design §8.3) — a file
  moved out on its own leaves every marked directory, and nothing intercepts an open of it until
  the worker sends `MarkFile` for it: after the watcher's quiet spell (2 s) and one examination
  while the daemon runs, and after a reboot, a daemon restart or a helper restart, until the
  worker's next look. It looks before any row runs and again at every wake, rows in flight or
  not: every pending `move-out` row's object is marked, paused, held, offline or
  waiting rows included, except one already local (`moved-out:local`). A directory moved out keeps
  its own marks, and so its placeholders' interception, until the helper restarts, and is then
  re-marked (`MarkDir` for it and every directory below it) the same way. A refusal that may
  change (`EAGAIN`, a lease) is asked again at the next look. Not covered: a move out while the
  daemon is not running, until the daemon runs and its Full local scan finds it; a directory made
  inside a moved-out tree after its re-mark, until the next helper connection; a moved-out object
  whose row was dropped and that could not be tidied then (F123); one whose row waits in a folder
  turned read-only by a switch nobody forced, which runs no worker, until the
  folder is read-write again or the switch is forced; and, after a reboot, a moved-out file whose place the kernel cannot give
  (F121 (4)) is marked all the same (the mark needs no path). LIMIT · measured in the VM
  (`move-out: a placeholder moved out …` — re-marked by a paused worker, then read whole; `move-out:
  a download that stops part-way …` — the mark gone with a restarted helper, back before anything
  runs) and on the host (`…what_left_is_marked_again_while_other_rows_run`). Open.
- **F121. Moves out of the folder: downloaded first, and only then deleted in OneDrive**
  (`konedrived/src/sync/upload/move_out.rs`; write design §8, §10) — a `move-out` row reaches its
  object by handle (`OpenByHandle`) and never by a remembered path. Anywhere but the Trash, a
  placeholder is downloaded where it went, through its own descriptor (the ordinary fill, under the
  per-inode lock, resuming a checkpoint), and a folder's every placeholder of the item with it; a
  cloud-only folder moved out is downloaded in full, with no prompt (as Windows does;
  `docs/design/decisions.md`, "A move out of the folder downloads first"). A writer from before the re-mark is probed for first (a read lease): while one has
  it open, nothing is filled. Only once each reads `hydrated` (the fill's commit point, after the
  whole content and its hash), the row is still the item's newest (a row the examination recorded
  behind it — the object came back — supersedes it) and the object is still proved to be outside
  the folder (its `/proc/self/fd` path opened again on the same inode), is the row
  marked (`moved-out:local` in its `snapshot`), konedrive's attributes taken off (the item id
  first), the directories unmarked (never one beneath any account's folder, M1), and the item
  deleted as a delete is (a folder whole, in one unguarded `DELETE`, F82 (10)). An object
  moved into another account's folder is stripped and deleted all the same (write design §9), and
  its directories stay marked; F124 has what differs there. In the Trash nothing is downloaded: a placeholder is removed with
  its `.trashinfo` (`moved-out:trash` first), and the item goes to OneDrive's recycle bin only
  once the placeholder is proved gone (no link left); downloaded content stays as the
  user's own. The Trash is the user's own (`$XDG_DATA_HOME/Trash`, else `~/.local/share/Trash`) or
  a `.Trash-<uid>` or sticky `.Trash/<uid>` directly at the top of a mount (`/proc/self/mountinfo`),
  and the entry's `.trashinfo` must be there; anything else that looks like one, and a
  placeholder with another link, is downloaded first. Doubt keeps the row, and the item in
  OneDrive: `EPERM` (another owner, a nested subvolume, the attribute gone), no helper, a download
  that stopped, a place that cannot be proved, an object back in the folder (the examination's),
  anything a folder held that is alive but elsewhere in the folder or unreachable. `ESTALE` is the
  user's delete only with its evidence: the handles the store recorded belong to the filesystem
  the folder is on now (`meta.handles_root`: the root directory's own handle and, where the kernel
  gives one, the filesystem's UUID, which survive a reboot, a remount and a renumbered device, as
  `f_fsid` does not on XFS and F2FS), and where the object was last proved
  to be there is nothing, or another object: an inode that cannot be read answers
  `ESTALE` every time and still stands there, and is never gone. That place is where the
  examination found it went, then wherever the row last reached it (kept in the row's
  `target_name`); for what a folder held, its place in the folder where the folder is now; for the
  examination's own deletes, its place in the folder. The row's own object must also say `ESTALE`
  twice, 5 s apart.
  The shortcuts: (1) after a marker, `EPERM` is read as this row's own strip, for the object and
  for what it held (a strip that stopped part-way converges); an object that went
  somewhere unreachable between the marker and the strip keeps konedrive's attributes, its content
  having been local; (2) a crash in the middle of the strip leaves attributes other than the item
  id on the user's file (an ordinary file all the same); (3) a file keeps the mark `MarkFile` put
  on it (there is no `UnmarkFile`): each open of it costs one helper round trip, let through at
  once, until the helper restarts; a directory holding another item's placeholder keeps its mark;
  (4) a file whose place the kernel cannot give (its dentry disconnected after a reboot reads as
  `/`) is downloaded, but not stripped or deleted until its place is proved: the row waits
  (`moved-out-place-unknown`) until something looks the file up by its name; (5) a directory is
  walked by its path, reopened through the user's own lookups and checked to be the same inode,
  never beneath the descriptor `OpenByHandle` gave (SECURITY.md, F90): one the user cannot reach by
  path waits; (6) what left a moved-out folder since is asked after by its handle, one round trip
  per placed file the base still has below it, and made local where it went (the Trash included);
  (7) move-outs run one at a time, beside the other rows, so a big folder's download holds the
  other move-outs back; (8) a download's progress shows in `Transfers` under the moved-out path,
  and the rows' reasons (`waiting-for-the-helper`, `moved-out-unreachable`,
  `moved-out-place-unknown`, `back-in-the-folder`, `download-failed`, `gone-once`,
  `handle-from-another-filesystem`, `gone-unproved`, `lease-probe-failed`) in `sync outbox`;
  (9) after the folder's filesystem changes (a home moved to a new disk with its store, a Btrfs
  snapshot rolled back), the next examination takes every handle again: a Full local scan records
  each item found where it is, an item missing then is placed again from OneDrive rather than
  deleted (WR4: the user's deletes made meanwhile are undone), and a `move-out` takes the handle of
  what stands where it went if that carries its item id, or is dropped (the item stays in OneDrive
  and is placed again). `LastError` says so until a Full local scan finds the handles current.
  Until that examination, nothing is deleted on `ESTALE`. The filesystem is recorded at the first
  examination, not with each handle: a store from before this record trusts handles it may have carried
  from elsewhere; (10) account A strips a placeholder that now sits in account
  B's folder as the daemon's own change, which B's watcher drops: B uploads it at its next Full
  local scan; (11) when the OneDrive folder is itself a mount, the desktop's Trash for
  it (`<folder>/.Trash-<uid>`) is inside the folder, and a Delete there is a move within the
  folder, which the examination sends to OneDrive as a move into `.Trash-<uid>/files`;
  (12) the place is checked before the marker, not again before the `DELETE`: a folder moved
  back into the folder while its files are being stripped is deleted in OneDrive all the same, its
  content local and stripped, uploaded again as new (its items' ids and history are lost, not a
  byte); M1 holds, since nothing beneath a folder is unmarked; (13) with leases off
  (`fs.leases-enable=0`) or not supported, no writer can be ruled out, and no moved-out placeholder
  is downloaded: those rows wait as `lease-probe-failed`; (14) a folder in the Trash
  loses its placeholders before its downloaded files are stripped, so a removal that fails part-way
  leaves nothing stripped, and a marker is never taken off an object back in the
  folder. FRAGILE · measured with a fake helper and a fake OneDrive (`sync::upload::move_out_tests`:
  a placeholder, a folder, one file of a folder that cannot be downloaded, the Trash for a file
  and a folder, a lookalike Trash and a linked placeholder, a download that stops part-way,
  `EPERM`/`ESTALE`/no helper, a handle of another filesystem, a crash between the strip and the
  delete and between two strips of a folder, what left a moved-out folder since, an object back in
  the folder before and during its download, re-marking while another row runs, a restored held
  move-out, a changed filesystem's handles taken again, `ESTALE` with the object still in its
  place, a Trash removal that fails part-way) and in the VM with the real helper (`move-out: …`, four scenarios, the item deleted
  only when the content was there at the moment of the `DELETE`). Open.
- **F122. Fills of moved-out objects are routed by item id** (`konedrived/src/sync/hub.rs`,
  `HelperHub::set_moved_out`; write design §8.3) — an open of a moved-out placeholder is filled
  by the account whose `move-out` row names it (or names the folder the base has it inside), found
  by the item id the file carries, before the device and the path are looked at: a placeholder
  moved from one account's folder into another's on the same filesystem is the first account's,
  though its path says the second. Each account's worker hands its ids over when they change; while
  any account has some, every fill request reads the file's item id first (one `fgetxattr`).
  WORKAROUND · measured (`sync::hub::tests::a_moved_out_object_is_routed_by_its_item_id`). The
  routes go when the rows do: a forced switch to read-only and a Forget clear the account's;
  `RestoreDeletes` leaves the rest to the worker's next look. Open.
- **F123. Dropped moves out are tidied, never finished** (`konedrived/src/sync/upload/move_out.rs`,
  `Tidy::dropped`, `drop_rows`; write design §8.3) — `move-out` rows go without
  their download or their delete when `RestoreDeletes` restores them, or a forced switch to
  read-only drops the outbox: the way out a Forget and a Remove point to, which are refused while
  rows wait; one that goes ahead drops what is left with the tree store, tidied
  the same way. Nothing would then download what they left
  outside the folder, and after a helper restart nothing would mark it: an application would read
  zeros (Z3). So each dropped row's object, reached by its handle while the helper still holds the
  folder (a Forget tidies before it lets go of it), is tidied as the Trash case without the delete,
  if it is proved to be outside every folder: a placeholder that holds nothing whole (never filled,
  or a fill or a free-up cut short) is removed, a downloaded file stays as the user's own, stripped,
  and the item's directories are unmarked, stripped and removed if left empty. Nothing is sent: the
  item stays in OneDrive, and forgets its local object in the same step that drops the row, so
  that a read-write folder's reconcile places it again (a read-only one does anyway). The shortcuts:
  (1) the rows are dropped before the objects are tidied, never after: a row kept over a
  placeholder already removed would read as the user's delete; so a crash in between leaves the
  object as it was, with no row to mark it again (Z3); (2) an object the helper cannot reach at that
  moment (no helper, `EAGAIN`), one being filled, a placeholder with another link, and one whose
  place cannot be proved are left as they are; (3) a Forget whose store cannot be read counts no
  rows, and drops them with the store, untidied; so does a held-back account's, whose store is not
  open; (4) `RestoreDeletes` tidies before it answers, one helper round trip per row and a walk per
  folder; (5) a row whose item the base has under a temporary name (`.konedrive-swap-`) is not
  dropped, as no other row of such an item is. LIMIT · measured (`sync::upload::move_out_tests::dropped_move_outs_leave_no_placeholder_outside`,
  `…restoring_a_held_move_out_tidies_what_left`). Open.
- **F124. A move between two accounts keeps the file** (`konedrived/src/sync/materialize.rs`,
  `set_aside`; `hub.rs`, `claimed_elsewhere`; `upload/move_out.rs`, `kept`; write design §8.3)
  — an object moved from account A's folder into account B's is, for A, a move out
  (downloaded where it is, then deleted in A's OneDrive); for B, read-write, a new file, uploaded once
  A has stripped it (F121 (10)). Three things keep it from ending up only in A's recycle bin: (1)
  B's read-only reconcile never removes an object whose id B's tree does not know and another account
  claims — A's outbox waits to fetch it, A's tree store knows it, or the id names A's drive
  (`<drive>!<n>`, a personal account's); a store that cannot be read claims it. The object is set
  aside instead: renamed into B's rescue directory alive, attributes and all, its modes made
  ordinary, and shown as a rescue; A's move out finds it there by its handle and downloads it
  there. (2) A never takes `ESTALE` for the user's delete when the object was last proved to be
  inside another account's folder: the row goes without a delete, the item forgets its local
  object, and A places it again. A delete the user made in B's folder is so undone in A: the file
  comes back there. (3) `EPERM` is never "gone", but when the object still stands where it was last
  proved to be inside another account's folder, with the same handle and no item id, B's
  examination took it for its own (it strips a downloaded file it does not know, and uploads it):
  A's row goes the same way, and the file is in both accounts. Before, that row waited for ever
  as `moved-out-unreachable`. The shortcuts: an account whose store is not open (not
  brought up yet at a daemon start, held back) claims nothing, so B may remove such an object then,
  and only (2) keeps it; if its last proved place was outside every folder before it went into
  B's (moved twice between A's looks), A deletes it on `ESTALE` as the user's delete; a set-aside
  placeholder stays one, with A's attributes, in B's rescue directory until A's move out runs, or
  until A drops the row and tidies it (F123). LIMIT · measured
  (`sync::upload::move_out_tests::a_placeholder_moved_into_a_read_only_account_ends_up_on_disk`,
  `…a_move_out_gone_inside_another_accounts_folder_deletes_nothing`,
  `…a_file_another_account_took_for_its_own_is_kept_here_too`,
  `sync::hub::tests::another_accounts_item_ids_are_claimed`). Open.
- **F130. The test-account harness guards every request, and leaves a few things to be done by
  hand** (`tests/write-account/`; `docs/design/writes.md` §12.1) — `konedrive-write-test` sends
  every request, konedrive's own `DriveClient`'s included, through a proxy on `127.0.0.1`. Its
  guard admits a write only once the drive both tokens reach is the one named, is on the gate's
  list, has less than 1 GiB in use and fewer than 1000 items; then only a write naming an item
  inside `/konedrive-write-test/<run id>/` (learnt from OneDrive's own answers) or making that
  folder, within 64 MiB per file, 200 MiB and 500 requests per run. After its first refusal it
  admits nothing but the cleanup. What it leaves: (1) reads are not confined: the preflight lists
  the drive to count its items, and stops at 1000; (2) the top folder `/konedrive-write-test` stays,
  empty, after a run; (3) a run cut short from outside (Ctrl-C, a lost network, the machine off)
  leaves its run folder in OneDrive, to be deleted by hand; the cleanup keeps 10 of the 500 requests
  for itself; (4) the harness cannot tell where a token came from: that the write token is the test
  account's rests on the drive-id checks, the look of the drive, and the daemon handing out
  `--read-write` tokens only for drives on the list; the token files are deleted by hand; (5) Graph
  restores an item from the recycle bin only with `Files.ReadWrite.All`, which konedrive does not
  ask for, so unless the restore is allowed the recycle-bin check ends `LOOK`, to be confirmed on
  onedrive.live.com; (6) the delta check waits up to a minute for OneDrive's feed to catch up, and a
  slower feed fails it. WORKAROUND · measured (`konedrive-write-test`'s tests, on wiremock and on
  the guard itself: a drive not on the list, another drive for either token, the same token twice,
  1 GiB in use, a use it cannot read, 1000 items; every kind of write outside the run folder, each
  refused for its own reason; a file over 64 MiB, the byte and request caps, the cleanup's reserve;
  a missing flag, a token file that is not `0600`; a write outside the run folder that never reaches
  the mock server, and an upload that reaches it without the token). Open.
- **F131. What the uploads assume of OneDrive, until the test-account run**
  (`docs/design/writes.md` §13; `tests/write-account/src/checks.rs`) — the harness is built and its
  guards are tested, but it has not run: there is no test account yet. Until it runs, these stay
  assumed, each handled safely either way: `If-Match` honoured on a 0-byte `PUT`;
  `conflictBehavior=fail` honoured in a `PUT`'s URL; `409` on a rename to a taken name; names
  colliding without regard to case; the delta feed returning the daemon's own changes with the eTags
  their writes were
  answered with; 10 MiB fragments and a resume from the session's status; deletes going to the
  recycle bin; an edit made during a session superseded by its last fragment (F80); and a
  read-only refresh after a switch back answering a token that cannot write (F66). LIMIT ·
  reasoned. Open until the run.
- **F140. A read-only folder that holds changes waiting to upload is not kept in step with OneDrive**
  (`konedrived/src/sync/write_mode.rs`, `follow_mode`, `drop_pending_uploads`;
  `sync/listing.rs`, `held_back`) — only a forced switch drops the outbox's rows:
  `SetMode("read-only", force)`, whether the account is read-write or read-only already. Any other
  way to read-only — a sign-out, an expired sign-in (`invalid_grant`), the gate or `mode` edited in
  `config.toml`, a file that cannot be read, a narrower grant, another drive seen, a watcher that
  could not start — keeps them: the folder is locked, and its sync runs no cycle while they wait,
  since the read phase's reconcile would put back the moves and renames they describe and place
  deleted items again. What a read-write cycle deferred stays deferred. Files are still downloaded
  on open. `LastError` says why; the changes go once the account is read-write again. The cost:
  OneDrive's changes do not reach such a folder meanwhile. A forced drop keeps a rename half-done in
  OneDrive under a `.konedrive-swap-*` name — dropped, the item would stay under that name,
  which no listing places, and its local object would go — so such a folder stays held until the
  account is read-write again. The window offers the forced switch only as it turns uploading off:
  for an account that turned read-only by itself, `konedrivectl account mode read-only --force` is
  the way. The replay of a stale `running` delete after a restart read-only cannot undo a
  re-placed item any more: no read-only reconcile places it while its row waits. LIMIT ·
  measured (`sync::tests::onedrive::a_sign_out_keeps_the_changes_waiting_to_upload`,
  `…an_expired_sign_in_keeps_the_changes_waiting_to_upload`,
  `tree::outbox::tests::a_forced_drop_keeps_a_rename_half_done`,
  `konedrived/tests/mode.rs::a_forced_switch_drops_what_a_read_only_account_kept`). Open.
- **F141. A Forget and `Accounts1.Remove` are refused while changes wait to be uploaded**
  (`konedrived/src/sync/mod.rs`, `forget`; `write_mode.rs`, `changes_in_store`) —
  the folder's tree store, which holds the outbox, goes with a Forget and with the account, so both
  are refused `PendingUploads` while it holds any row — waiting, blocked or held by the mass-delete
  guard — asked before anything changes (the watcher hands over first) and again once the sync
  has stopped. The way out is to wait, or a forced switch to read-only (F140), which drops them.
  With no sync running, the store on disk is read; one that cannot be opened counts as holding
  nothing, since the next sync would rebuild it empty anyway. LIMIT, on purpose · measured
  (`sync::tests::onedrive::a_folder_whose_changes_wait_is_not_forgotten`,
  `konedrivectl` `a_remove_or_forget_refused_while_changes_wait_says_what_to_do`). Open.
- **F142. A held or pending removal is dropped only at the swap of a cycle that reaches one**
  (`konedrived/src/tree/outbox.rs`, `outbox_drop_removed`; `sync/listing/rw.rs`,
  `Writes::dropped_removed`) — when a delta or a Full reconcile finds that a held or pending
  `delete`/`move-out` row's item is already gone from OneDrive, the row is dropped there and then,
  without a request; the outbox is woken so `HeldCount`/`PendingCount` count it at once, and a
  dropped `move-out`'s placeholder outside the folder is tidied as `Tidy::dropped` always tidies
  one (F54). A `running` row is left alone: it is mid-request, so its own commit meets the `404`
  and drops it there instead (`Committed::Gone`), one request behind rather than swept. The sweep
  itself runs only at the swap of a read-write cycle that completes one, so a folder locked or
  waiting on changes to upload (F140, F141) keeps such a row — pointing at nothing — until it runs
  a cycle again, same as every other reconcile-driven change. FRAGILE · measured
  (`sync::listing::rw::tests::a_held_delete_of_an_item_already_deleted_in_onedrive_is_dropped`). Open.
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
| Upload fragment, and the most sent in one request (`CHUNK_SIZE`, `SMALL_UPLOAD_MAX`) | 10 MiB (32 × 320 KiB) | Microsoft's advice (5–10 MiB fragments, resumable above 10 MiB); not measured |
| One upload request's bound (`UPLOAD_REQUEST_TIMEOUT`) | 10 min: a 10 MiB fragment needs about 140 kbit/s | **guess** |
| Longest `Retry-After` a write takes (`MAX_RETRY_AFTER`) | 1 h | the write design's sanity bound (write design §6.2) |
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
| Quiet spell before a batch of local changes is examined, and its ceiling during continuous activity (`QUIET`, `CEILING`) | 2 s / 30 s | the write design's; **guess** |
| A busy file (open for writing, being filled or freed) examined again after (`RECHECK`) | 30 s | **guess** |
| A folder the watcher cannot watch in full is scanned and walked every (`DEGRADED_SCAN`) | 10 min | the write design's; **guess** |
| A batch the examination could not take yet is offered again after (`watcher::RETRY`) | 5 s with no completed listing; after an error 5 s doubled at each error in a row, up to 10 min | **guess** |
| A `MarkDir` the helper did not answer is asked again after (`MARK_RETRY`) | 60 s, and when the helper is back | **guess** |
| Shortest time between two walks for a directory the map lost (`UNKNOWN_WALK`) | 60 s | **guess** |
| Mass-delete guard (`MASS_DELETE_ITEMS`, `MASS_DELETE_PERCENT`, `MASS_DELETE_FLOOR`) | more than 500 items, or more than 20 % of the folder's items once at least 10, counting removals still waiting | 500 and 20 % the write design's, the floor of 10 ours; all **guesses** |
| Outbox rows sent at once (`upload::Limits`) — uploads at once: 4 (guess) | 1 metadata row (`mkdir`, `move`, `delete`); 4 files up to 10 MiB and 2 larger beside it | the write design's; **guess** |
| `move-out` rows run at once (`Class::Out`) / how long the examination waits for the helper's `OpenByHandle` (`ASK_WITHIN`) / a first `ESTALE` for a row's object is asked again after (`GONE_AGAIN`) | 1, beside the others / 45 s, past the link's own 30 s / 5 s | **guess** |
| A failed row's backoff / a throttle without `Retry-After` (`BACKOFF_FIRST`/`BACKOFF_MAX`, `THROTTLE_FIRST`) | 1 s doubling to 1 h / 10 s doubling to 1 h; `Retry-After` taken up to 1 h | the write design's; **guess** |
| OneDrive full, tried again (`QUOTA_RETRY`) | every 30 min, or when the quota changes | the write design's |
| A row rewritten and sent again at once before it backs off (`AGAIN_LIMIT`) / the worker's idle look at the outbox | 20 / every 300 s | **guess** |

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
- **D9.** The notification watcher `docs/design/hydration.md` §16 called for exists for read-write
  folders only (`konedrived/src/sync/watcher/`, Z2); a read-only folder needs none.
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
- **D15.** `konedrived/tests/accounts.rs::a_version_1_onedrive_folder_is_held_then_brought_up_at_the_first_connect`
  can fail when `TMPDIR` is on xfs (`/var/tmp`) and passes on the default tmpfs `/tmp`: its last
  assertion reads `LastError` once, right after the folder carries its drive, which the bring-up writes
  before the sync's first cycle has failed on the unmocked listing. It is a race in the test (the
  assertion wants an `eventually`), not in the daemon. Seen once; not chased.
- **D16.** `konedrivectl/tests/sync_cli.rs::binary_skipped_of_a_onedrive_folder_still_listing_says_the_list_may_be_partial`
  failed once in a full `cargo test --workspace` while other builds loaded the machine (load 6), and
  passes alone: it waits at most 2.5 s for `RootState` to read `listing`, behind a delta answer held
  for 2 s, so a slow bring-up misses the window. A read-only folder's path; seen once; not chased.

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
  daemon refuse the whole batch over it. Reasoned.
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
  `application/octet-stream` until this round, which meant real Dolphin never
  shows the menu on a folder at all — `inode/directory` is now in the list too. But the test that
  exercises the real `KFileItemActions::addActionsTo` path already passed, before the fix, for
  selections of `text/plain`, `image/jpeg` and `application/pdf`, none of which was ever declared
  either: whatever matching `MenuActionSource::Plugins` does in this test harness does not filter
  by `MimeTypes` the way Dolphin's own context-menu building evidently does. The fix is right (the
  JSON is now complete for every type the plugin cares about), but no automated test actually
  proves the *filtering* itself works for a mixed selection in real Dolphin; only manual use does
  (`docs/acceptance-check.md` §10).
- **K25. A directory's own pin bit is not cached.** DEBT · by decision, not measured.
  `OverlayEngine` caches whether a directory *is a root* per watched directory, but not whether it
  *carries a pin* — `overlays()` and `recheck()` call `isEffectivelyPinned`, an ancestor walk to
  the root, fresh every time (K5), for a directory item exactly as for a file. Caching each
  watched directory's own pin bit, and rechecking a directory's descendants only when that bit or
  the root's changed, was judged not cheap enough to add in this round; the extra UI-thread cost
  is the same per-call ancestor walk K5 already measures for files, now paid for folders too.
- **K26. Upload emblems read an attribute kept in step with the daemon by hand.** Reasoned; measured against
  attributes set by hand (`overlayplugintest::emblemWhileWaitingToUpload`,
  `overlayenginetest::uploadStateIsFollowedLive`, `noopentest`,
  `actionplugintest::refusalIsExplained`). The overlay reads `user.konedrive.sync` by path with
  `lgetxattr`, first, on every file and folder Dolphin asks about (one more read on top of K5's,
  not remeasured): `pending` and `uploading` show `state-sync`, `blocked` shows `state-error`, and
  either wins over the state and the pin, so a file new here with no state yet gets it too. A value
  the plugin does not know falls back to the item's other emblems (unlike K10's unknown state),
  since the file itself is still what its state says. Folders are read as well, although the write
  design names only files, so a folder the daemon marks needs no change here. The daemon's outbox
  worker writes the attribute (`konedrived/src/sync/upload/local.rs`), and "Free up space"'s
  refusal `org.konedrive.Error.NotUploaded` ("not uploaded yet, so freeing it up would lose the
  changes made here") is matched by name. No test runs the plugin against the daemon, so a value or
  name spelled differently on one side shows no upload emblem, or the generic "Freeing up … failed"
  with the daemon's message. The context menu itself is unchanged: a new file with no state still gets
  no actions. FRAGILE · open.

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
  `app/tests/placescontrollertest.cpp`). `PlacesController` reconciles Dolphin's Places panel
  entries (one per account folder, A19) whenever the accounts or `PlacesSettings::enabledChanged`
  change, which only ever happens inside the app process: `konedrivectl` registering or forgetting a
  folder while the app is not running does not touch the Places panel until the app is started
  again, at which point it catches up as soon as the daemon and every account have answered. Each
  entry is found again by a bookmark metadata tag (`konedrive-account` = the account's id, set with
  `KFilePlacesModel::bookmarkForIndex`/`KBookmark::setMetaDataItem`, then `refresh()` to make sure
  the tag reaches disk and not just this process' copy of the bookmark file — `editPlace` saves
  only when the text, url or icon change), not by url, so a folder change updates the same entry
  in place instead of leaving a stale one behind. WORKAROUND: adding a fresh entry takes the *last*
  row matching the new url rather than the first, since `addPlace` does not hand back the row it
  created and a user could already have an unrelated place at that exact url; this narrows, but does
  not close, the window where such a pre-existing entry could be mistaken for konedrive's own on
  that one add. The icon is `folder-cloud` only when the current icon theme reports having it
  (`QIcon::hasThemeIcon`), else `cloudstatus`, which every Breeze release carries. "Show in
  Places" (`ShowInPlaces` in konedriverc's `[General]` group, on by default, the same way
  `StartAtLogin` is stored), one switch for every account, removes the entries without touching
  anything else in the file. Tests (`addsAnEntryPerAccountFolder`,
  `updatesTheUrlWhenTheFolderChanges`, `forgettingOrRemovingRemovesTheEntry`,
  `turningTheSwitchOffRemovesTheEntries`, `aUsersOwnEntryIsLeftAlone`, and A19's) run against a real
  `KFilePlacesModel` with `XDG_DATA_HOME`/`XDG_CONFIG_HOME` pointed at a wiped temporary directory,
  never the user's own `user-places.xbel` or `konedriverc`.
- **A13. The window shows one account at a time.** Decision · measured
  (`app/tests/accountsmodeltest.cpp`: `followsTheManager`, `theChoiceIsRemembered`,
  `anotherAccountsTroubleShows`, `theRowsOutliveTheDaemon`). An account switcher heads the sidebar
  (`app/qml/AccountSwitcher.qml`); Status, Activity, Conflicts, Not in the Folder and Account show
  the account chosen there, Settings is the whole app's. With more than one account each page's
  title names the account ("Status · Personal"), since a narrow window hides the switcher. The
  choice is remembered as `CurrentAccount=<id>` in konedriverc's `[General]` group: the remembered
  account whenever it is there, else the one shown if it is still there, else the first. Trouble in
  an account not shown puts a warning sign on the switcher, but only trouble (the tray's
  "needs attention"): another account signed out, or without a folder, does not. The folder moved
  from Settings to the Account page and is asked for only once the account is signed in (a folder
  already there shows whatever the sign-in); the window still never registers without
  interception. While the daemon is away the window keeps its last list of accounts, each
  saying the service is not running, and follows the new list when it is back. Every account has
  its own controllers, each watching the daemon's name and reading its own object, so a daemon
  start costs two `GetAll` calls per account plus one for the manager.
- **A14. Account names are checked in the window too, by a copy of the daemon's rules.** FRAGILE ·
  reasoned (`labelProblems`). Rename checks a name before asking the daemon — trimmed, 1 to 40
  characters counted as the daemon counts them (not UTF-16 units), no "/" or control character, not
  12 hexadecimal digits in any case (that is the shape of an account id, and the command line takes
  a name or an id in one place), unique regardless of case (`AccountsModel::labelProblem`) — so the
  dialog says why at once. "@" is allowed: an account's name is its email (Sign In sets it, A15).
  The daemon checks again and its refusal is shown as it words it. Qt's and Rust's
  case-insensitive comparisons can differ on rare letters; the daemon's answer is then the one that
  counts. The name suggested for a first account is "Personal", translated like any other string,
  and only while no account is called that (`suggestedLabel`, unused by Sign In itself now).
- **A15. Sign In is several calls in a row, not one.** WORKAROUND · measured
  (`addingSetsTheClientIdAddsChoosesAndSignsIn`, `aRefusedAddSaysWhy`). "Sign in…" is
  `Accounts1.SetClientId` (only when no client ID is set yet), `Accounts1.Add` with a temporary
  label ("Signing in…"), `Account1.BeginSignIn` on the new account, whose URL opens in the browser,
  and, once its sign-in succeeds, `Account1.SetLabel` with the account's email. Nothing makes these
  one transaction: a failure part way keeps what already succeeded — a saved client ID with no
  account, for one that never reached `Add`. From `Add` on, the account is a draft
  (`AccountsModel::m_draftPath`/`m_hiddenDrafts`): kept out of the model, so the switcher, the tray,
  Places and notifications never see it, until it is signed in and renamed. If the sign-in is
  cancelled, fails, or the dialog is closed, or its email is already another account's label
  (`AccountsModel::emailAlreadyUsed`, shown as "This account is already added"), the draft is
  removed (`Accounts1.Remove`) and nothing is left. A draft still there at the next start — an
  earlier run crashed mid sign-in — is found by its temporary label and removed the same way
  (`AccountsModel::probe`).
- **A16. The upload switch keeps its own "waiting for sign-in"; the client ID is one for all.**
  FRAGILE · measured (`accountcontrollertest`: `aSwitchToReadWriteWaitsForItsSignIn`,
  `aRefusedSwitchSaysWhyInPlainWords`, `aSwitchToReadOnlyAsksBeforeDroppingUploads`;
  `dialogstest::theUploadSwitch`). The Account page's "Upload changes made on this computer" shows
  `Account1.Mode`, the mode the account runs in: on is read-write, so a read-write account whose
  token lost `Files.ReadWrite` shows off, with `LastError` saying why (F61). Turned on, it first
  explains that a sign-in follows and what uploading means, then calls `SetMode("read-write",
  false)` and opens the URL it answers, as Sign In does. `Account1` says nothing while that sign-in
  waits (F64), so the window keeps the wait itself (`AccountController::modeSignInPending`, with
  "Copy Sign-In Link" and "Cancel", which calls `CancelSignIn`) and ends it as `konedrivectl account
  mode` does: `Mode` turning read-write, a `LastError` arriving (`SetMode` cleared it before it
  answered, and the window reads the properties again after the answer instead of trusting the
  order of the signals), the account leaving `signed-in`, or the daemon going away. So (1) a window
  restarted while the browser is open forgets the wait: the switch shows off, and finishing the
  sign-in still turns it on; (2) a `LastError` set meanwhile for another reason ends the wait,
  though the sign-in may still go through and turn the switch on; (3) a sign-in left open in the
  browser waits until "Cancel", or until the daemon gives it up. Turned off, it calls
  `SetMode("read-only", false)`; a `PendingUploads` refusal asks whether to turn off without
  uploading, then calls it again with `force`. The question gives `PendingCount` as its count,
  which the daemon counts a moment apart from its refusal, and a refusal that arrives after another
  account was chosen asks nothing (the switch just shows on again). Refusals are told by their names, never the daemon's words:
  `WritesNotAllowed` says uploading is not available for this account in this version (the gate,
  F60, never the user's doing), `NotSignedIn` says to sign in first, `ModeNotGranted` to sign in
  again, and any other error keeps the daemon's message. The switch is disabled while the account
  is not signed in and while a switch is under way. WORKAROUND: Kirigami Addons'
  `FormSwitchDelegate` writes `checked` back from its inner switch, which ends a binding on it, so
  the page writes the mode again on every change (`onUploadingChanged`). The client ID stays in
  Settings, shared by every account (`Accounts1.ClientId`), and can be changed only while no
  account is signed in or signing in, the daemon's own rule; the field says so. Open.
- **A17. The tray sums up every account.** Decision · measured (`app/tests/appstatustest.cpp`:
  `theTrayShowsTheWorstStateAndALinePerAccount`, `theTrayMenuWithSeveralAccounts`,
  `aClickShowsTheOneAccountNeedingAttention`, `noAccountIsOffline`). The icon is the worst state
  across the accounts (`AppStatus`): needs attention, then signed out (which, as A8, includes no
  folder yet and OneDrive out of reach), then syncing, then synced; with no account, signed out.
  The tooltip keeps today's status line with one account; with several it has a line per account in
  account order, "Family — Signed out of OneDrive", and an account needing attention shows why
  ("2 changed files were moved out of the way") in place of its status line, so its "checked N ago"
  is not there. The menu keeps "Open OneDrive Folder" with one account; with several it has an
  "Open Folder" submenu of the accounts that have a folder (disabled when none has). "Refresh Now"
  refreshes every account whose folder shows OneDrive. A click shows the window on the one account
  needing attention when exactly one does, and that becomes the switcher's remembered choice; with
  none or several it shows the account the window last showed, as "Open KOneDrive" always does.
- **A18. Notifications and download progress name the account, only once there are several.**
  Decision · measured (`notifiertest` and `downloadprogresscontrollertest`:
  `theTitleNamesTheAccountWhenThereAreSeveral`). With more than one account a notification's title
  becomes "<what happened> — <label>" ("Download failed — Family") and its text is unchanged: the
  title keeps what happened because the text alone does not always say it (a sign-out's text is
  "Sign in again to keep your OneDrive folder up to date."). A click on one opens the window on its
  account. Each account has its own `Notifier`, so the 10 s windows and their summaries are per
  account and kind (A3): two accounts failing together notify once each. Download progress has one
  Plasma job tracker but a `DownloadProgressController` per account, so the cap of 5 jobs shown
  (A11) is per account too, each with its own "and N more files". A job's title,
  "Downloading from OneDrive — <label>", is set when the job appears; renaming the account while it
  runs leaves it as it was. A sign-out notice waits 2 s before it is shown: removing a signed-in account
  signs it out before its object goes away, and the wait lets the removal cancel the notice. A real
  sign-out is therefore announced 2 s late.
- **A19. Places: one entry per account folder; the single-account entry taken over in place.**
  Decision · measured (`renamingTheAccountRenamesItsEntry`, `theOldEntryIsTakenOverInPlace`,
  `nothingIsTouchedUntilEveryAccountHasAnswered`). Every entry is named "OneDrive — <label>", with
  one account too, so a second account renames nothing; renaming an account renames its entry, and
  forgetting its folder or removing it removes the entry. An entry of the single-account versions
  (`konedrive` = `1`) whose url is an account's folder is re-tagged to that account and renamed,
  keeping its place in the panel; any other such entry is removed. The entries are reconciled only
  once the daemon and every account have answered: until then — and while the daemon is not
  running — nothing is touched. That also ends what the single-account app did at every start,
  where its entry was removed before the daemon had answered and added back at the bottom of the
  panel. The name and icon are konedrive's: renaming an entry in Dolphin, or giving it another
  icon, is undone at the next change; a second entry carrying one account's tag is removed.
- **A20. Held removals: the notification's baseline and its default.** Decision · measured
  against the fake daemon (`appstatustest::uploadsBlockedHeldAndPaused`,
  `notifiertest::heldDeletesNotifyWithRestoreAsTheDefault`, `synccontrollertest::theOutboxAndItsControls`).
  The window follows `Sync1.HeldCount` for the tray's "needs attention", the Status page's "Restore
  Them" and "Delete in OneDrive Too", and the `massDelete` notification; `Outbox()` is read only for
  the Activity page's list (when a count changes, or the page is shown), which shows the first 100
  rows and "and N more". The notification fires when `HeldCount` rises from 0: the daemon's first
  answer after the app or the daemon starts only sets the baseline, so removals already held then
  show in the tray and on the Status page, not as a new popup, and more held while some already are
  add no second one. Its default action — a click on the notification itself — is
  `RestoreDeletes`, the choice that loses nothing; `ConfirmDeletes` is only ever its own button.
  Closed as a workaround: until `HeldCount` (1e7ac4f) the window found held rows by reading the
  whole outbox every 15 s; the rest stays as decided.
- **A21. Upload progress reuses the download jobs' rules, and a retry looks finished.** LIMIT ·
  measured (`downloadprogresscontrollertest::uploadsShowAsUploadingToOneDrive`). A second
  `DownloadProgressController` per account watches `Uploads` (`Direction::Upload`): the same 2 s
  before a job shows, the cap of 5 and "and N more", the 1.5 s grace window (A11), the same
  "Show download and upload progress" switch, titled "Uploading to OneDrive". Only an
  `upload-failed` event fails a job, and the daemon sends it only for a change that needs the user
  (`blocked`); an upload that stops for a reason expected to pass — offline, throttled, locked,
  changed while sending — leaves `Uploads`, goes to `retry`, and its job finishes as a success;
  when it is tried again a new job appears. The job's error is the reason in the window's words
  (A22). Plasma's side is unverified, as A7 says. Open.
- **A22. The reasons, the kinds and copies are read by their codes, in words kept apart from
  `konedrivectl`'s.** FRAGILE · reasoned (`modelstest::uploadKindsAndCopies`). `uploadReasonText`
  (`app/outboxmodel.cpp`) turns an outbox row's reason, an `upload-failed` detail and a
  `NotUploaded()` reason into the window's words, with the same meanings as `konedrivectl`'s
  `upload_reason_text` but pointing at the window instead of commands; no test keeps the two in
  step (unlike W12's skip reasons), and a code neither knows is shown as the daemon wrote it. The
  worker's own retry reasons (`changed-while-sending`, `parent-not-in-onedrive`, …) are shown as
  codes, as the command line shows them. `Conflicts()` carries each entry's kind (`rescued` or
  `copy`), and the window and `sync conflicts` word them by it; the `conflict` event carries none,
  so there a copy of a file changed on both sides is told from a rescue by where its other file is:
  beside the original (a copy) or anywhere else (a rescue). A rescue into the same folder, which the
  daemon never makes, would read as a copy in the Activity list. Open.
- **A23. Pausing from the tray pauses every account that can be paused.** Decision · measured
  (`appstatustest::theTrayPausesAndResumesEveryAccount`). "Pause Syncing" in the tray calls
  `Pause` on every account whose folder shows OneDrive and is not paused yet, with 2, 8 or 24 hours
  (Windows' three) or until resumed; "Resume Syncing", shown while any account is paused, resumes
  each paused one. A local folder is never asked (the daemon refuses it `Unsupported`), and
  accounts paused at different times keep their own ends. "Paused" is a state of its own, after
  "signed out" and before "syncing" (`AppStatus::rank`), with Breeze's `media-playback-pause`; an
  account that also needs attention shows the warning instead. The Status page pauses and resumes
  the account it shows, and says "Paused until 14:00" from `PausedUntil`, the time of day when it is
  today, or a date. Open.

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
- **F63. A read-write folder reconciled by the read phase's rules** — a Full reconcile
  rescued new local files out of the folder, put back local moves and rescued local edits before a
  remote change. Closed by the read-write reconcile (F110–F117, commit `60be43d`).

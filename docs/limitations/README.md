# Limitations, workarounds and weak spots

One place for everything in konedrive that is limited, worked around, fragile, or knowingly
below the quality we want. It is kept current: an entry is added whenever a decision accepts
a limitation, builds a workaround, picks a number without measuring it, or parks a finding.
Detail lives elsewhere (`docs/design/`, `docs/kernel-behavior-7.2/`, the code); this log is the
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

**Layout.** Every entry is a file in this directory, named by its id (`F71.md`). This file is
the index: each section has its own text and a line for every entry in it. A new entry is a new
file and a line for it in its section here. Ids do not change.

---

## 1. Anything that can hand an application zeros or lose data

These come first because the property that outranks everything in this project is that an
application must never read zeros where real content should be.

- [Z1](Z1.md) — The helper's death releases every suspended open as zeros
- [Z2](Z2.md) — A placeholder moved into a directory the user just created is not covered
- [Z3](Z3.md) — A hardlink, or a file moved out of the folder, escapes interception
- [Z4](Z4.md) — A tool that re-sparsifies a downloaded file behind our back
- [Z5](Z5.md) — Punching a file that still carries an ignore mark
- [Z6](Z6.md) — Lost or hand-edited `config.toml` while the helper holds a root
- [Z7](Z7.md) — `UnregisterRoot` is best effort
- [Z8](Z8.md) — Names too long for Linux

---

## 2. Platform limits that cost function, not data

- [P1](P1.md) — Opening through a read-only mount is refused when the helper would be asked
- [P8](P8.md) — An executable kept in the folder cannot run twice at once
- [P2](P2.md) — Opening a file during a lease gets `EPERM` instead of waiting
- [P3](P3.md) — Every directory needs its own mark, so the tree is walked
- [P4](P4.md) — Kernel memory per directory
- [P5](P5.md) — Only a fixed set of errnos can reach an application
- [P6](P6.md) — Eager hydration
- [P7](P7.md) — Items in OneDrive that are not synced
- [P9](P9.md) — `Skipped()` returns the whole list in one D-Bus message
- [P10](P10.md) — Pinning a big folder downloads everything in it, with no prompt

---

## 3. Workarounds we built

- [W1](W1.md) — The no-interception mode
- [W2](W2.md) — The read-only folder in the read phase
- [W3](W3.md) — Non-blocking event descriptors
- [W4](W4.md) — The registration walk clears ignore marks on every file
- [W5](W5.md) — A credit limit between helper and daemon
- [W6](W6.md) — Silence, not a blocked send, ends a connection
- [W7](W7.md) — Lease tests run in threads, not subprocesses
- [W8](W8.md) — Fault injection lives behind a cargo feature
- [W9](W9.md) — The helper clears an ignore mark for whoever owns the file
- [W10](W10.md) — Recovery does not wait for a busy file
- [W11](W11.md) — `TokenExport.ReadOnly` and `konedrivectl dev export-access-token`
- [W12](W12.md) — One skip-reason wording, kept identical in Rust and C++ by a test
- [W13](W13.md) — Day-to-day VM runs cover btrfs only
- [W14](W14.md) — A OneDrive folder is excluded from KDE's Baloo indexer
- [W15](W15.md) — A real-account VM run lists the whole drive, but downloads only in one named folder, capped in size
- [W16](W16.md) — The helper's unit is hardened, and runs under systemd only in a VM check you start by hand
- [W17](W17.md) — The VM's two-account scenario uses two local folders, signed in by hand
- [W18](W18.md) — The VM's write scenarios run the daemon against a fake OneDrive in the guest
- [W19](W19.md) — The upload stress tool confirms every held delete, and can miss a fast upload's "running" moment

---

## 4. Fragile spots

- [F3](F3.md) — Long walks behind short timeouts
- [F4](F4.md) — Startup holds the lifecycle lock through the helper's walk
- [F5](F5.md) — During a transient connection only the top of the stack is exempt
- [F6](F6.md) — A dying transient connection's queued jobs are denied `EIO`
- [F7](F7.md) — Under descriptor exhaustion, a filled file can be denied `EIO`
- [F8](F8.md) — The offline content source is held in memory
- [F9](F9.md) — A download whose name is removed mid-flight
- [F10](F10.md) — A degraded root is only logged
- [F11](F11.md) — A helper that is running but unreachable blocks the no-interception folder
- [F12](F12.md) — "Is a helper running" assumes the daemon sees the helper's `/run`
- [F13](F13.md) — A file stuck mid-download or mid-free-up makes every sync cycle a Full reconcile
- [F14](F14.md) — A replacement's swap and a reconcile can race on a locked directory's write window
- [F15](F15.md) — Stopping the sync waits for a reconcile stuck on the helper
- [F16](F16.md) — A rescue needs a directory on the folder's own filesystem
- [F17](F17.md)
- [F18](F18.md) — A tree store that cannot be opened stops the sync until something asks again
- [F19](F19.md) — A Forget that cannot take the lock off the folder only logs it
- [F20](F20.md) — What a folder shows is decided when it is registered
- [F21](F21.md) — "The network is back" means NetworkManager's `CONNECTED_GLOBAL`
- [F22](F22.md) — A sign-in is noticed as a change of state, not as an event
- [F23](F23.md) — A OneDrive folder registered again while signed out keeps its read-only lock
- [F24](F24.md) — The activity log keeps 200 events, and they go with the tree store
- [F25](F25.md) — The activity log is a summary, not a record of every file
- [F26](F26.md) — `LocalBytes` is a walk, so it lags up to 5 s behind a download
- [F27](F27.md) — `FreeUpSpace` skips busy files
- [F28](F28.md) — A conflict is recorded only for a reconcile that goes through
- [F29](F29.md) — A fill on open is shown under the name the file had when it was opened
- [F30](F30.md) — A failed switch the helper may still hold is kept intercepted, and waits for the next connect
- [F31](F31.md) — A folder that already carries its root id is not probed again
- [F32](F32.md) — `ItemsPlaced + SkippedCount` can be less than `ItemsListed`, by design, not a race
- [F33](F33.md) — Partial downloads are kept, not erased
- [F34](F34.md) — A file Graph hands out with no hash is only logged
- [F35](F35.md) — The first listing is placed page by page — only the first, and only into a folder that holds nothing of ours
- [F36](F36.md) — `HelperState` is what systemd says, asked every 30 s
- [F37](F37.md) — `config.toml` has two independent writers.
- [F38](F38.md) — Pins: the sweep, and what waits for it
- [F39](F39.md) — The Graph write client rests on answers only wiremock has given
- [F40](F40.md) — Moving version 1's tree store into its account can give up
- [F41](F41.md) — Version 2 of `config.toml` has no way back
- [F42](F42.md) — A sign-in is refused when the daemon cannot check which drive it reached
- [F43](F43.md) — Every account shares one helper link
- [F44](F44.md) — An open that cannot be matched to an account is refused `EIO`
- [F45](F45.md) — A folder forgotten before multiple accounts can be adopted by another account
- [F46](F46.md) — Nothing answers at `/org/konedrive/Daemon` any more
- [F47](F47.md) — What removing an account keeps
- [F48](F48.md) — An account that collides with an earlier one in a hand-edited `config.toml` is held back
- [F49](F49.md) — Personal Microsoft accounts only
- [F50](F50.md) — `konedrivectl` explains some refusals from its own view of the folders
- [F51](F51.md) — Choosing the account on the command line
- [F52](F52.md) — An edit that kept both size and time, made while nothing watched, is not found
- [F53](F53.md) — A copy and a move are told apart by the file handle the store recorded
- [F54](F54.md) — A missing item is deleted in OneDrive only on the helper's word
- [F55](F55.md) — The examination's shortcuts
- [F60](F60.md) — The write gate: only test accounts can be read-write, until the release
- [F61](F61.md) — A read-write account is read-write only while its last token carried `Files.ReadWrite`
- [F62](F62.md) — The lock walks of a mode switch
- [F64](F64.md) — The switch to read-write ends in `Mode` or `LastError`, and nothing says it is under way
- [F65](F65.md) — A mode the user gave a file does not survive a round trip through read-only
- [F66](F66.md) — Consent to write stays with Microsoft, and a read-only request may be answered with it
- [F70](F70.md) — A full notification queue costs a Full local scan
- [F71](F71.md) — The watcher's marks come from a budget every account shares
- [F72](F72.md) — Nothing on another device than the folder is uploaded
- [F73](F73.md) — A write through a hard link outside the folder raises no event
- [F74](F74.md) — The watcher's shortcuts
- [F75](F75.md) — A size change by path, and a write through a mapping, raise nothing the watcher reads
- [F80](F80.md) — The last fragment of a large upload can supersede an edit made in OneDrive meanwhile
- [F81](F81.md) — A file kept open for writing is not uploaded until it is closed
- [F82](F82.md) — The outbox worker's shortcuts
- [F90](F90.md) — `OpenByHandle` gives a user their own object wherever it went
- [F91](F91.md) — A moved-out file is handed over read-only, and the daemon reopens it for writing itself
- [F92](F92.md) — The helper lets its own opens through without deciding them
- [F100](F100.md) — The pause is the tree store's
- [F101](F101.md) — A coalesced property that changes and changes back is not signalled
- [F102](F102.md) — The outbox on the bus: what it simplifies
- [F110](F110.md) — A replacement waits while the file is open anywhere
- [F111](F111.md) — The `410` upload variant removes placeholders the service lost
- [F112](F112.md) — What waits for a local change is staged again at every cycle
- [F113](F113.md) — The stale-delta guard reads again, under the tree lock, what the outbox committed during a fetch
- [F114](F114.md) — The reconcile's conflict copies go up through the examination
- [F115](F115.md) — A missing item is placed again only with something to place
- [F116](F116.md) — What OneDrive removed goes from the disk in the cycle, but for what it never had
- [F117](F117.md) — The order of a read-write folder's cycle, and what it cannot close
- [F120](F120.md) — A placeholder moved out of the folder reads zeros until the daemon marks it again
- [F121](F121.md) — Moves out of the folder: downloaded first, and only then deleted in OneDrive
- [F122](F122.md) — Fills of moved-out objects are routed by item id
- [F123](F123.md) — Dropped moves out are tidied, never finished
- [F124](F124.md) — A move between two accounts keeps the file
- [F130](F130.md) — The test-account harness guards every request, and leaves a few things to be done by hand
- [F131](F131.md) — What the uploads assume of OneDrive, until the test-account run
- [F140](F140.md) — A read-only folder that holds changes waiting to upload is not kept in step with OneDrive
- [F141](F141.md) — A Forget and `Accounts.Remove` are refused while changes wait to be uploaded
- [F142](F142.md) — A held or pending removal is dropped only at the swap of a cycle that reaches one
- [F143](F143.md) — The transfer pool's numbers are guesses, and only a throttle stops its growth
- [F147](F147.md) — A transfer's size class is read once, before its first request
- [F148](F148.md) — Pinned downloads go in alphabetical order batch by batch, small and large apart
- [F144](F144.md) — An open holds background work back for as long as it runs
- [F145](F145.md) — Only the sync's Graph client reports to the pool
- [F146](F146.md) — A hydration request waits for its account's slot in a task of its own
- [F149](F149.md) — A file or folder removed here before its upload finished leaves the outbox at once
- [F150](F150.md) — A full OneDrive is decided by one quota read, and some edges are taken on trust
- [F151](F151.md) — The local scan's "about N" is the base's count, not the disk's
- [F152](F152.md) — Queue totals: what "left", "done" and "time left" count, and where they are approximate
- [F155](F155.md) — A large pinned download in parts keeps only its gap-free start
- [F156](F156.md) — Only pinned large files go in parts, and extra streams give way one piece at a time
- [F157](F157.md) — An upload stops when its file is under none of its names — a move not recorded yet included
- [F158](F158.md) — The outbox's budgets at scale are guesses, measured once on one machine
- [F159](F159.md) — The worker picks rows a hundred at a time, and stops looking at 32
- [F160](F160.md) — The counts and the Not Uploaded summary lag the outbox by up to a second
- [F161](F161.md) — The bus's lists read the last committed state
- [F162](F162.md) — One thread owns each tree store; everyone sends it jobs
- [F163](F163.md) — Whole-table reads kept, and when they run
- [F164](F164.md) — The cloud side's budgets at scale are guesses, measured once on one machine
- [F165](F165.md) — A delta is staged over `items`; a full listing is staged whole
- [F166](F166.md) — Thumbnails are picked a page at a time, and each is looked at once per drain
- [F167](F167.md) — The counts are walked once per cycle that may have changed the tree
- [F168](F168.md) — `Skipped()` climbs from the skipped rows; the window shows 200
- [F169](F169.md) — Replacements go through one queue worked by 8 tasks
- [F170](F170.md) — The conflicts are looked over 200 at a time; the window shows 200
- [F171](F171.md) — A reconcile records what it placed 500 at a time
- [F172](F172.md) — An open upload session holds its name with an empty file
- [F173](F173.md) — One quota per account, cached only at a read
- [F174](F174.md) — The files moving and the pool line count from two sources
- [F175](F175.md) — Without NetworkManager, UPower or power-profiles-daemon, no hold for that source
- [F176](F176.md) — NetworkManager's guess of a metered connection is trusted as it is
- [F177](F177.md) — A stop waits 10 s at most for the requests in flight
- [F178](F178.md) — An uploaded file's page cache is dropped only by advice
- [F179](F179.md) — The move of the hold settings to the whole app takes the strictest value
- [F180](F180.md) — The notification endpoint's lifetime is Graph's undocumented `expirationDateTime`, or a guess of one hour
- [F181](F181.md) — The notification socket ignores proxies
- [F182](F182.md) — The Socket.IO client is written from the protocol and one other client, not observed against the service
- [F183](F183.md) — While the notification socket is up the poll runs every 5 minutes
- [F184](F184.md) — While the account is paused or holds back, changes made in OneDrive are not seen
- [F185](F185.md) — Every change in the drive asks for a cycle, the account's own uploads too
- [F186](F186.md) — The live task's waits are chosen, not measured
- [F187](F187.md) — What is lost when OneDrive removes an item
- [F188](F188.md) — A folder that stopped being placed stays on disk while its uploads run
- [F189](F189.md) — A row placed again carries no local object
- [F190](F190.md) — The window between a cycle's reconcile and its swap is tested through a hook
- [F191](F191.md) — A stopped download is dropped where it is
- [F192](F192.md) — An item moved here into a folder that OneDrive then removes is removed here, and stays in OneDrive where it was
- [F193](F193.md) — A stale handle can still record a `delete` for a file moved into a leaving folder
- [F194](F194.md) — Some leaving or removed folders fail every cycle until their cause is gone
- [F196](F196.md) — What the user does inside a folder that is leaving does not reach OneDrive
- [F197](F197.md) — A `403` blocks only its row, and the row is tried again whenever a worker begins
- [F198](F198.md) — The order of the tree lock and the lifecycle lock is kept by hand
- [F199](F199.md) — A folder without interception has its path, and nothing else, while the daemon starts
- [F200](F200.md) — A bad upload's item is remembered beside its row, and blocked rows are listed per file whatever their reason
- [F203](F203.md) — A free-up whose blocking task cannot be joined leaves the file `dehydrating`
- [F204](F204.md) — A sign-in whose account is reported signed out meanwhile ends in silence
- [F205](F205.md) — A removal that fails half-way leaves the account without its folder, and a failed `Add` can leave its entry
- [F208](F208.md) — What the helper's bounds per uid on waiting opens and on roots leave open
- [F210](F210.md) — An entry the examination is refused to open, strip or read is passed over, and the user is not told which
- [F211](F211.md) — A folder whose `source` in `config.toml` is neither word is held, not repaired
- [F212](F212.md) — Some failures of the tree store inside a reconcile still do not stop the folder
- [F220](F220.md) — A OneDrive item dated before 1970 shows 1970-01-01 locally
- [F221](F221.md) — One refresh at a time, and a cached token handed out beside it
- [F230](F230.md) — A fill's file calls run in blocking sections, and a section that has begun ends by itself
- [F231](F231.md) — The write gate and the hub's file calls run on blocking threads; the rest of `config.toml`'s readers do not
- [F232](F232.md) — A replacement's file calls run in three blocking sections, and a stop is heard only between them
- [F233](F233.md) — An upload step's file calls run in blocking sections, and a section that has begun ends by itself
- [F234](F234.md) — What the helper's missing write probe, its version check and its panic containment leave open
- [F235](F235.md) — A delete that follows a read of the item goes out with an empty `If-Match` when the answer carried neither tag
- [F236](F236.md) — What the upload worker's waits leave: the throttle's rules are reasoned, its note is not said again after a closed gate, and two looks wait for a wake
- [F237](F237.md) — What the one shape of the move out leaves: a file in the Trash whose free-up was cut short goes without a second look, and the Trash rule was not run against the real helper again
- [F238](F238.md) — A file with other names that is taken off the disk: what a stop between its unlink and the removal of its id leaves
- [F239](F239.md) — One path for every file's upload session: what a file of one fragment now does that was not measured against OneDrive
- [F240](F240.md) — What the outbox worker is told without waiting is done only on the daemon's runtime, and its fault points are in the tests' build alone

---

## 5. Provisional numbers

| Constant | Value | State |
|---|---|---|
| Worker pool / event queue | 64 / 1024 | measured comfortable at 3000 concurrent opens |
| Outbox depth | 256 | measured as the binding constraint, correctly |
| Credit per connection | 64 | equal to the daemon's queue depth; pinned by a test |
| Waiters per user | 8 | measured binding |
| Waiters in total | 32 | **guess** — a single-user machine never reaches it |
| Opens waiting for one uid's daemons to answer (`MAX_SUSPENDED_OPENS_PER_UID`) | 8192 | **guess** — above the 3000-open burst twice over, an eighth of the unit's `LimitNOFILE` (F208) |
| Roots per uid (`MAX_ROOTS_PER_UID`) | 32 | **guess** — one root per intercepted account (F208) |
| Liveness window | 60 s | **guess** — nothing in the suite reaches it |
| Delta size reconciled in full (`FULL_THRESHOLD`) | 5000 changes | **guess** — above it one scan is assumed cheaper than item by item |
| Sync interval / waits after failures in a row | 60 s / 5, 15, 30 s | 60 s is the design's; the retry steps are a **guess** |
| A fill's checkpoint, every N bytes (`CHECKPOINT_EVERY`) | 16 MiB | **guess** |
| `Retry-After` wait when Graph throttles (`429`/`503`) | default 10 s, capped at 300 s, 5 attempts before giving up | **guess** (`RetryPolicy::default`) |
| Upload sessions given up, cancelled per run of the worker (`CANCELS_PER_LOOK`) / after a failed cancel, not again before (`CANCEL_AGAIN`) / cancelled at once by a forced switch to read-only (`DROPPED_CANCELS`) | 32 / 60 s / 256 | **guess** (issue #47, F172) |
| The daemon's stop: the longest wait for the requests in flight (`STOP_BOUND`) | 10 s | **guess** (issue #84, F177) |
| A placeholder taken for a recorded opening's: created between the first recording and the latest attempt with an unknown outcome, each widened by (`CLOCK_SLACK`) | 5 min | **guess** (issues #84, #89, F172) |
| A record of an opening whose row left, kept without a row (`OPENING_LEFT_KEEP`) | 7 days | **guess** (issue #89, F172) |
| Upload fragment, and the most sent in one request (`CHUNK_SIZE`) | 10 MiB (32 × 320 KiB) | Microsoft's advice (5–10 MiB fragments, resumable above 10 MiB); not measured |
| One upload request's bound (`UPLOAD_REQUEST_TIMEOUT`) | 10 min: a 10 MiB fragment needs about 140 kbit/s | **guess** |
| Longest `Retry-After` a write takes (`MAX_RETRY_AFTER`) | 1 h | the write design's sanity bound (write design §6.2) |
| Transfers at once — fills on open, `Hydrate`, pinned downloads, replacements, thumbnails, uploads, metadata rows | **adaptive**, one pool per account (`crates/konedrive-graph/src/pool.rs`, issue #3): the numbers below | see below |
| Transfer pool: start (`START`) / ceiling (`[transfers] max`, `DEFAULT_CEILING`, clamped to 1–256) | 16 / 32, each account's pool separately | **guess** |
| Transfer pool growth | +1 slot per successful transfer while work waits and every slot is busy; +1 per round (as many successes as slots) at and above the size the last `429`/`503` came at | **guess** |
| Transfer pool: throttle level forgotten after (`THROTTLE_MEMORY`) / a throttle within the wait (+1 s, `BURST_GRACE`) is the same burst / no slot for, without `Retry-After` (`DEFAULT_THROTTLE_WAIT`) | 5 min / halves once / 10 s | **guess** |
| A large file, from (`LARGE_FROM`) / streams of large sync transfers at once per account (`[transfers] large`, `DEFAULT_LARGE`, clamped to 1…`max`), files being opened outside it | 100 MiB / 4 | **guess** |
| Slots above the pool only a file being opened may take (`RESERVE`) | 2 | **guess** |
| A large pinned download's piece (`hydration::source::parts::PIECE`) / how often a download in parts looks for a free slot to add a stream in (`LOOK_AGAIN`) | 256 MiB / 100 ms | **guess** (issue #28): large enough that a request's round trip is nothing beside it, small enough that the streams share a file's end |
| Speed shown (`DownloadSpeed`, `UploadSpeed`): the average of (`SPEED_SPAN`) / published every | 3 s / 1 s while anything moves or a `Retry-After` runs, and until nothing has moved for 10 s | **guess** |
| A queue's time left (`DownloadTimeLeft`, `UploadTimeLeft`): the speed it is worked out from is the average of (`AVERAGE_SPAN`) / none once nothing has moved that way for (`STILL_AFTER`) | 30 s, or the run so far when shorter (never under 1 s) / 10 s | **guess** (F152) |
| Queue totals counted at most every (`PUBLISH_EVERY`) | 1 s | the speeds' own rate |
| Hydration requests taken off the helper's queue at once (`FILL_ADMISSION`) | 64, the helper's credit; each then waits for its account's pool | pinned by a test |
| Window's transfer charts | the last 2 min, one sample a second | **guess** |
| Thumbnails filled per run / how often regardless | 200 / every 10 min, each request in a pool slot (no pause between them any more) | **guess** (`crates/konedrived/src/desktop/thumbs.rs`) |
| Thumbnail candidates looked at per query / per store call (`THUMB_PAGE`, `THUMB_SCAN`) | 500 / 5 000 | **guess** (`crates/konedrive-tree/src/thumbs.rs`, issue #39) |
| Replacements downloading at once (`REPLACE_WORKERS`) | 8, each also in a pool slot | **guess** (`crates/konedrived/src/remote/listing/replacements.rs`, issue #39) |
| Placed items recorded in one transaction (`PLACED_BATCH`) | 500 | **guess** (`crates/konedrived/src/remote/materialize.rs`, issue #39) |
| Conflicts looked over per cycle (`PRUNE_BATCH`) / rows the window's Skipped and Conflicts pages list / how often the Skipped page asks again | 200 / 200 / at most once a second | **guess** (`crates/konedrived/src/status/activity.rs`, `app/qml/SkippedPage.qml`, `app/conflictmodel.h`; issue #39) |
| Activity events kept / logged per kind in an incremental cycle | 200 / 50 | **guess** |
| Shortest time between two `LocalBytes` walks | 5 s | **guess** |
| Shortest time between two coalesced `PropertiesChanged` (counters, status, `Transfers.Downloads`) | 250 ms, at most 4 signals a second | the design's four a second |
| Notifications per event kind (A3) | one per 10 s, the rest as one summary | **guess** |
| Window's "checked N s ago" refresh | every 10 s, from the clock | **guess** |
| Window's "Recent" list | 50 rows | **guess**; the daemon keeps 200 |
| Files of one reason the window lists, and `sync not-uploaded` without `--all` (`PerFileCap`, `PER_FILE_SHOWN`) | 20 | **guess** (A24) |
| Shortest time between two reads of what is kept back (`NotUploadedSummary`) while a page shows it | 1 s | **guess** |
| Quiet spell before a batch of local changes is examined, and its ceiling during continuous activity (`QUIET`, `CEILING`) | 2 s / 30 s | the write design's; **guess** |
| A busy file (open for writing, being filled or freed) examined again after (`RECHECK`) | 30 s | **guess** |
| A folder the watcher cannot watch in full is scanned and walked every (`DEGRADED_SCAN`) | 10 min | the write design's; **guess** |
| A batch the examination could not take yet is offered again after (`watcher::RETRY`) | 5 s with no completed listing; after an error 5 s doubled at each error in a row, up to 10 min | **guess** |
| A `MarkDir` the helper did not answer is asked again after (`MARK_RETRY`) | 60 s, and when the helper is back | **guess** |
| Shortest time between two walks for a directory the map lost (`UNKNOWN_WALK`) | 60 s | **guess** |
| Mass-delete guard (`MASS_DELETE_ITEMS`, `MASS_DELETE_PERCENT`, `MASS_DELETE_FLOOR`) | more than 500 items, or more than 20 % of the folder's items once at least 10, counting removals still waiting | 500 and 20 % the write design's, the floor of 10 ours; all **guesses** |
| Outbox rows sent at once | 1 metadata row (`mkdir`, `move`, `delete`); files, small or large alike, as many as the account's transfer pool gives | the write design's one metadata row; the rest adaptive |
| `move-out` rows run at once (`Class::Out`) / how long the examination waits for the helper's `OpenByHandle` (`ASK_WITHIN`) / a first `ESTALE` for a row's object is asked again after (`GONE_AGAIN`) | 1, beside the others / 45 s, past the link's own 30 s / 5 s | **guess** |
| A failed row's backoff / a throttle without `Retry-After` (`BACKOFF_FIRST`/`BACKOFF_MAX`, `THROTTLE_FIRST`) | 1 s doubling to 1 h / 10 s doubling to 1 h; `Retry-After` taken up to 1 h | the write design's; **guess** |
| A full OneDrive, or a file too big for the space left: the quota read again by itself (`space::QUOTA_RECHECK`) | every 30 min, one request, never the uploads themselves | **guess** (issue #2) |
| Less free space than this is none: the account is full (`space::NO_SPACE`) | 1 MiB | **guess** (issue #2) |
| A quota read shared by refusals of rows running together (`space::REUSE`) | 10 s | **guess** |
| A row rewritten and sent again at once before it backs off (`AGAIN_LIMIT`) / the worker's idle look at the outbox | 20 / every 300 s | **guess** |
| Due rows a pick reads at a time (`PORTION`) / rows it looks for before it stops reading (`PICK_WANT`) / portions before rule 1 is asked only of rows that share a key (`PORTIONS_ASKED`) | 100 / 32 / 8 | **guess** (F159) |
| The counts and the Not Uploaded summary summed again at most every (`TALLY_EVERY`) | 1 s | **guess** (F160) |
| Changed outbox rows remembered one by one for the marks (`DIRTY_MAX`) | 100 000; past it, every row once | **guess** (F163) |
| The outbox's budgets at scale (`tests/bench.rs`) | see F158 | **guess** |
| Jobs a tree store's channel holds before a sender waits (`tree::QUEUE`) | 1 024 | **guess** (F162) |
| The notification endpoint's lifetime without `expirationDateTime` (`socket::DEFAULT_LIFETIME`) / replaced before its expiry by (`RENEW_EARLY`) / opening the socket, bound (`CONNECT_TIMEOUT`) / largest message taken (`MAX_MESSAGE`) | 1 h / 2 min / 30 s / 1 MiB | **guess** (issue #54, F180) |
| The poll while the notification socket is up (`Schedule::live_interval`) | 5 min | the user's choice; how often the service drops an event is not known (F183) |
| The live task's debounce / retries / shortest endpoint life / look at a stopped account / time before a connection counts as up (`live::Timing`) | 2 s / 1, 2, 4 … 60 s / 60 s / 60 s / first ping or 30 s | **guess** (F186) |

---

## 6. Quality debt

- [D1](D1.md)
- [D2](D2.md)
- [D3](D3.md)
- [D4](D4.md)
- [D5](D5.md)
- [D6](D6.md)
- [D7](D7.md)
- [D9](D9.md)
- [D10](D10.md)
- [D11](D11.md)
- [D12](D12.md)
- [D13](D13.md)
- [D14](D14.md)
- [D15](D15.md)
- [D16](D16.md)
- [D17](D17.md)
- [D18](D18.md)
- [D19](D19.md)
- [D20](D20.md) — A failed upload's reason is one of four coarse keys
- [D21](D21.md) — `konedrivectl` opens no browser when stdout is not a terminal
- [D22](D22.md)
- [D23](D23.md)
- [D24](D24.md) — The tree store's test helpers are behind a feature.
- [D25](D25.md) — The token manager's tests use a stand-in for the account's state.
- [D26](D26.md) — The journal lines of the Graph client and the tree store carry the new crates' targets.
- [D27](D27.md) — The daemon's journal lines carry its new module paths.
- [D28](D28.md)
- [D29](D29.md) — Some items moved down a layer sit lower than their name suggests, and one lower than its users need.
- [D30](D30.md) — The structure guard reads lines, not Rust.
- [D31](D31.md) — Some tests fail now and then when the machine is busy.
- [D32](D32.md) — The tests' private bus has a configuration of its own, and only a test's own connection gives up on a call.
- [D33](D33.md) — What the one `send` of `konedrive-graph` left as it was, and what it rests on
- [D34](D34.md) — Outbox reasons and local skips are types over the strings they were.
- [D35](D35.md) — Refusals and the notes of `LastError` are types over the strings they were.
- [D36](D36.md) — The store's schema has a number for every change, and what that leaves.
- [D37](D37.md) — The test support of `sync/` is built into the daemon's crate, and finds a service's parts by a list.

---

## 7. Dolphin integration

The two plugins in `dolphin/`: emblems for each file's state and pin, and "Always keep on this
device" / "Free up space" in the context menu. They read a file's state and pin from its extended
attributes and never open it.

- [K1](K1.md) — Dolphin opens some files itself, and that downloads them.
- [K2](K2.md) — No emblems in search results or Recent Files.
- [K3](K3.md) — Live updates cover the 256 most recently shown folders.
- [K4](K4.md) — Unverified: an emblem after a download triggered by opening a file.
- [K5](K5.md) — Reading state on Dolphin's UI thread.
- [K6](K6.md) — At most 1000 paths wait at once, per window.
- [K7](K7.md) — A root mark set or removed by hand.
- [K8](K8.md) — Messages can be lost.
- [K9](K9.md) — A renamed folder that Dolphin immediately asks about stops updating live.
- [K10](K10.md) — An unrecognised state value
- [K11](K11.md) — After a failed on-demand start
- [K12](K12.md) — Cosmetic:
- [K13](K13.md) — Build assumptions:
- [K14](K14.md) — Memory:
- [K15](K15.md) — `xx-large` (1024 px) thumbnails are not filled.
- [K16](K16.md) — The thumbnail filler runs one request at a time, half a second apart.
- [K17](K17.md) — Thumbnail body and decode caps are fixed, not configurable.
- [K18](K18.md) — A renamed or deleted file's old thumbnail cache entries are never cleaned up.
- [K19](K19.md) — `thumbnail_candidates` scans every image/video row on each call.
- [K20](K20.md) — A renamed or moved file's thumbnail is fetched again from OneDrive.
- [K21](K21.md) — The outline-check icon name is one letter from picking the filled one instead.
- [K22](K22.md) — A pin set or removed on a folder Dolphin has only passed through, not browsed on its own, does not update emblems live.
- [K23](K23.md) — "Always keep" and "Free up space" act on the selection as it was when the menu was built, not as it is when the button is clicked.
- [K24](K24.md) — `inTheContextMenuKioBuilds` does not prove KIO's real MimeTypes-based plugin filtering.
- [K25](K25.md) — A directory's own pin bit is not cached.
- [K26](K26.md) — Upload emblems read an attribute kept in step with the daemon by hand.
- [K27](K27.md) — A file OneDrive refuses a thumbnail for has none, so Dolphin with previews on downloads it.
- [K28](K28.md) — "Open in OneDrive" asks OneDrive for the address on every click.
- [K29](K29.md) — An item that is in OneDrive but not in the folder cannot be opened in OneDrive from here.
- [K30](K30.md) — Whether the menu section's heading "OneDrive" is drawn depends on the widget style.
- [K31](K31.md) — "Open in OneDrive" opens only an `https` address.

---

## 8. Window and tray

The app in `app/` (`docs/design/desktop.md` §4–§6): the tray icon, KDE notifications, and the
window's status, activity and conflicts, all read from the folder's interfaces (`org.konedrive.Folder`, `Transfers`, `UploadQueue`, `Conflicts`, `LocalScan`, `ActivityLog`) and `Account`.

- [A1](A1.md) — Notifications need the app running.
- [A2](A2.md) — Exact strings from the daemon still steer the app.
- [A3](A3.md) — At most one notification per kind in 10 s.
- [A4](A4.md) — The autostart entry runs the installed program.
- [A5](A5.md) — A sign-out the user asked for elsewhere still notifies.
- [A6](A6.md) — The "Recent" list merges a load with what arrives during it.
- [A7](A7.md) — The desktop side of the tray and notifications is untested.
- [A8](A8.md) — A signed-in account without a folder shows the "offline" icon.
- [A9](A9.md) — Single instance under the name `org.konedrive.konedrive`; closing quits only without a tray.
- [A10](A10.md) — Free Up Space waits for as long as it takes.
- [A11](A11.md) — A held removal correlates a failure by path and a 1.5 s window, not by waiting out the daemon's actual order.
- [A12](A12.md) — Places: a folder registered only through `konedrivectl` while the app is not running gets its entry when the app next starts.
- [A13](A13.md) — The window shows one account at a time.
- [A14](A14.md) — Account names are checked in the window too, by a copy of the daemon's rules.
- [A15](A15.md) — Sign In is several calls in a row, not one.
- [A16](A16.md) — The upload switch keeps its own "waiting for sign-in"; the client ID is one for all.
- [A17](A17.md) — The tray sums up every account.
- [A18](A18.md) — Notifications and download progress name the account, only once there are several.
- [A19](A19.md) — Places: one entry per account folder; the single-account entry taken over in place.
- [A20](A20.md) — Held removals: the notification's baseline and its default.
- [A21](A21.md) — Upload progress reuses the download jobs' rules, and a retry looks finished.
- [A22](A22.md) — The reasons, the kinds and copies are read by their codes, in words kept apart from `konedrivectl`'s.
- [A23](A23.md) — Pausing from the tray pauses every account that can be paused.
- [A24](A24.md) — What is kept back is shown by reason; files only where each needs something done, 20 at most.
- [A25](A25.md) — The account's own hold shows as paused, and the tray lifts it for every account.
- [A26](A26.md) — "· live" hides when the last check ran.

---

## 9. Packaging

The RPM packages, `konedrive` and `konedrive-kde`, from `packaging/rpm/konedrive.spec`
(`docs/design/packaging.md`).

- [R1](R1.md) — Upgrading the package restarts the helper.
- [R2](R2.md) — The developer install and the packages must not be installed together.
- [R3](R3.md) — Removing the package does not refuse while a folder is registered.
- [R4](R4.md) — The spec is for local builds, not yet for a public repository.
- [R5](R5.md) — Nothing tests the scriptlets.
- [R6](R6.md) — The packages are not signed.
- [R7](R7.md) — Only Fedora 44 on x86_64 is built.
- [R8](R8.md) — The version is one line in `Cargo.toml`, bumped by hand after each release.
- [R9](R9.md) — A merge into `main` can go without a release of its own.
- [R10](R10.md) — The tests run on Ubuntu, the RPMs are built on Fedora.
- [R11](R11.md) — A window or a Dolphin left open across the upgrade that renamed the D-Bus interfaces talks to names that are gone.
- [R12](R12.md) — The window's QML is compiled into the binary, because Qt's disk cache kept a same-day build's.

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

# Desktop integration

What sits on top of the daemon: its D-Bus API, the command line, the KOneDrive window and tray
icon, notifications, download progress in Plasma, thumbnails, the Baloo exclusion, and the Dolphin
plugins. The daemon's own work is in [hydration.md](hydration.md) and [sync.md](sync.md).

## 1. Principles

- **Every client talks to the daemon.** The window, the tray, the CLI and the Dolphin menu call
  D-Bus methods and show what the daemon publishes. The window never touches the sync folder
  itself. The Dolphin emblems are the one exception, and they only read an extended attribute.
- **Every feature has a command-line path.** Anything the window can do, `konedrivectl` can do, so
  nothing depends on clicking through a UI, and scripts and tests can drive everything.
- **Refusals have names.** Every refusal is a D-Bus error name; clients match the name, never the
  message, and turn it into a sentence about the user's file and what to do next.
- **Nothing a client does opens a placeholder** (see the invariant in [README.md](README.md)).

## 2. The D-Bus API

### 2.1 Names

Session bus, service `org.konedrive.Daemon`, object `/org/konedrive/Daemon`, three interfaces:
`org.konedrive.Account1`, `org.konedrive.Sync1` and `org.konedrive.Dev1`. The definitions are in
`dbus/*.xml`, and a test keeps each in step with the live interface. The daemon is D-Bus activated
(`SystemdService=konedrived.service`), so the first call from any client starts it. `Sync1` is on
the object before the name is claimed, so no client sees a half-registered daemon.

Properties change through `PropertiesChanged`. Counters, status and `Transfers` are coalesced: at
most one signal per 250 ms, so a drive of hundreds of thousands of items cannot flood the bus.

### 2.2 `Account1`

| Member | Meaning |
|---|---|
| `State` (`s`) | `signed-out`, `signing-in` or `signed-in` |
| `LastError` (`s`) | the reason for the most recent failure; empty when none |
| `ClientId` (`s`) | the configured application id |
| `DisplayName`, `Email` (`s`) | from `GET /me` |
| `QuotaUsed`, `QuotaTotal` (`t`) | bytes, from `GET /me/drive` |
| `SetClientId(s)` | validates and stores the client id; refused while signing in or signed in |
| `BeginSignIn() → s url` | starts the loopback listener and returns the authorization URL; the caller opens it ([sync.md](sync.md) §12.1) |
| `CancelSignIn()`, `SignOut()`, `RefreshAccountInfo()` | as named; `SignOut` deletes the refresh token |

Neither the refresh token nor the access token is ever exposed through `Account1`.

### 2.3 `Sync1` methods

| Method | Does |
|---|---|
| `RegisterRoot(s path)` | binds an empty folder to the signed-in drive, with the helper intercepting ([hydration.md](hydration.md) §14.1) |
| `RegisterRootWithoutInterception(s path)` | the developer's local folder, with nothing intercepting ([hydration.md](hydration.md) §14.3) |
| `UnregisterRoot()` | Forget: leaves every file as it is ([hydration.md](hydration.md) §14.5) |
| `PopulateFromDirectory(s source_dir) → t created` | fills a local folder with placeholders mirroring a directory; refused on a OneDrive folder |
| `Hydrate(s path)` | downloads one file now ([hydration.md](hydration.md) §6.5) |
| `Dehydrate(s path)` | frees one file up ([hydration.md](hydration.md) §8) |
| `ItemState(s path) → s` | `online-only`, `hydrating`, `hydrated`, `dehydrating` or `not-managed`, read from the attribute by name (`lgetxattr`), never by opening the file |
| `Refresh()` | runs a sync cycle now; refused `NoHelper` while the folder waits for the helper |
| `Skipped() → a(ss)` | (path, reason) for everything in OneDrive that is not in the folder ([sync.md](sync.md) §7.5) |
| `RecentActivity(u limit) → a(xsss)` | (time, kind, path, detail), newest first |
| `Conflicts() → a(xss)` | (time, original path, rescued path) ([sync.md](sync.md) §10.3) |
| `DismissConflict(s rescued_path)` | takes one conflict off the list; the file stays where it is |
| `FreeUpSpace() → (u files, t bytes, u busy)` | frees up every downloaded file that is not in use; files open somewhere or busy with a download are skipped and counted, never waited for |

### 2.4 `Sync1` properties and signals

| Property | Meaning |
|---|---|
| `RootPath` (`s`) | the registered folder, empty when none |
| `RootState` (`s`) | `none`, `listing`, `ready`, `no-interception` or `error` (§2.5) |
| `RootSource` (`s`) | `onedrive`, `local`, or empty ([sync.md](sync.md) §3) |
| `LastError` (`s`) | what needs attention, in words: the registration's trouble and the sync's, joined |
| `HelperState` (`s`) | `connected`, `not-installed`, `stopped`, `failed` or `unknown` (§2.5) |
| `ItemsListed`, `ItemsPlaced`, `SkippedCount` (`t`) | the listing's progress ([sync.md](sync.md) §7.5) |
| `LastChecked` (`x`) | Unix time of the last successful cycle; 0 for never |
| `LocalBytes` (`t`) | the space the folder's files take on disk (`st_blocks × 512`), measured by a walk after each cycle and at most every 5 s after a download or free-up |
| `ConflictCount` (`u`) | how many conflicts are listed |
| `Transfers` (`a(stt)`) | each download under way as (path, bytes done, bytes total): fills on open, `Hydrate`, and replacements; not thumbnails |

The signal `ActivityAdded(x time, s kind, s path, s detail)` announces each event as it is recorded.
The kinds are `downloaded`, `freed`, `added`, `updated`, `removed`, `moved`, `listed`, `conflict`,
`failed` (a download) and `update-failed` (a changed file could not be replaced here). For a full
disk, the detail is exactly "not enough disk space" in either failure kind.

The daemon keeps the newest **200** events in the tree store; a Forget or a rebuild drops them. The
log is a summary, not a record of every file: a first listing or a Full reconcile is one `listed`
event ("12 345 items"), an incremental cycle logs at most 50 events of each kind plus one "and N
more", and `FreeUpSpace` is one `freed` event (limitations log F25).

### 2.5 `RootState` and `HelperState`

`RootState` is computed, never stored — one state, one source of truth:

- `none` — no folder is registered;
- `error` — the registration or the sync is in trouble: signed out, another account than the store
  was built from, an unusable store, a failed recovery, or a folder waiting for the helper;
  `LastError` says which;
- `listing` — a first or post-`410` listing is under way (only ever in place of `ready`);
- `ready` or `no-interception` — the registration's own mode.

`HelperState` says what the daemon knows of the helper. While it holds a link, `connected`.
Otherwise it asks systemd — read-only, over the system bus, with no privilege — for
`konedrive-helper.service`: not found is `not-installed`; inactive is `stopped`; failed, or a unit
that cannot be loaded, is `failed`; no system bus, no systemd, or a unit systemd says is running
while the daemon has no link yet, is `unknown`. It is asked again when the link drops or returns and
every 30 s while there is none. The sentence for each state — how to install, start or diagnose the
helper — is written once, in `konedrive_dbus::helper_advice`, for the daemon's `LastError`, the CLI
and the window alike.

### 2.6 Errors

Every refusal is an error name under `org.konedrive.Error`: `NotSignedIn`, `AlreadyRegistered`,
`NoHelper`, `NotEmpty`, `Unsupported`, `NoRoot`, `NoSource`, `OutsideRoot`, `NotManaged`,
`NotHydrated`, `ModifiedLocally`, `InUse`, `NoConflict`, and `Failed` for everything without a name
of its own (an I/O failure). Registration refusals come in the order `NotSignedIn`,
`AlreadyRegistered`, `NoHelper`, then the folder checks.

### 2.7 `Dev1`

`AccessToken() → s` returns the daemon's current access token for a test run in the VM — about an
hour of `Files.Read`, never the refresh token ([sync.md](sync.md) §12.2).

## 3. The command line

`konedrivectl` talks to the same interfaces.

| Command | Does |
|---|---|
| `set-client-id <id>`, `login`, `logout`, `status` | the account |
| `sync register <path>` | registers a OneDrive folder (needs the helper) |
| `sync register-without-interception <path>` | the developer's local folder, named after its cost on purpose |
| `sync forget` | Forget |
| `sync populate-from <dir>` | fills a local folder from a directory |
| `sync hydrate <path>`, `sync dehydrate <path>`, `sync state <path>` | one file |
| `sync status` | the folder, its state, source and counts, "Last checked", "On this computer", whether opens are intercepted, and a `Helper:` line with what to do |
| `sync skipped` | what is not in the folder, and why |
| `sync refresh` | a cycle now |
| `sync activity [--limit N]`, `sync transfers` | recent events; downloads under way |
| `sync conflicts`, `sync dismiss <rescued path>` | the conflicts |
| `sync free-up-space` | frees up every downloaded file not in use |
| `dev export-access-token --out <file>` | writes the access token to a `0600` file, atomically, never through a symlink |

When the daemon refuses, the CLI says what that means for the user's file and what to do, chosen by
the error name. `sync status` never prints `error` so that it looks like success, and
`sync register` exits non-zero if the folder it just bound was not fully recovered. The CLI resolves
only the directory a path is in, never its last component, so a symlink given as a folder reaches
the daemon as a symlink and is refused.

## 4. The window

KOneDrive is a Kirigami application with a sidebar of six pages:

| Page | Shows |
|---|---|
| **Status** | the status line, the folder and its item count, "On this computer: …", "Free Up Space…", "Refresh Now", "Open in File Manager", and a card with the helper's instruction while it is not `connected` |
| **Activity** | "Downloading now" (each file with a progress bar and its size) and "Recent" (the newest 50 events; clicking one shows the file in Dolphin) |
| **Conflicts** | each rescued file: the file, where it was, where it is now, when; "Show in Folder" and "Dismiss". Always present, with a count badge while there are conflicts, and "No conflicts" otherwise |
| **Not in the Folder** | the skipped items and why, in the same words as `sync skipped` (a test keeps the two in step) |
| **Account** | sign in or out, the account's name, email and quota |
| **Settings** | the folder (chosen here, and registered only with the helper), "Start at login", "Show download progress", the client id, "Quit KOneDrive" |

The **status line** reads, for example, "Up to date · checked 20 s ago", "Listing your OneDrive:
N items so far", "Downloading 3 files", "1 changed file was moved out of the way", "Signed out of
OneDrive", "No OneDrive folder yet", or the error, refreshed every 10 s. The window does not offer
the no-interception mode: a folder is registered only through `RegisterRoot`.

**Free Up Space** asks for confirmation, then reports how much it freed and how many files it
skipped as busy. It has no D-Bus timeout: freeing up a large folder can take longer than the
default 25 s, and the window shows "Freeing up space…" until the answer.

**One instance.** The app is single-instance through `KDBusService(Unique)`; a second launch shows
the running window. It starts at login, hidden in the tray, through an XDG autostart entry that the
"Start at login" switch writes or removes; the switch is on by default after the first run, and the
entry itself is the truth, so removing it in System Settings turns the switch off. With a system
tray, closing the window hides it; without one, closing quits, so no process lingers unseen.

## 5. The tray icon

| State | Icon | When |
|---|---|---|
| synced | `state-ok` | the folder is up to date |
| syncing | `state-sync` | a listing or a download is under way |
| needs attention | `state-warning` | a sync error, a conflict, a failed update, trouble that does not stop the folder |
| signed out | `state-offline` | signed out, OneDrive unreachable, or no folder yet |

The tooltip repeats the window's status line; a click opens the window. The menu has "Open OneDrive
Folder", "Open KOneDrive", "Refresh Now" and "Quit". The tray reads its state from `RootState`,
`HelperState`, `ConflictCount` and `LastError`; a few of those readings still depend on exact
wording from the daemon (limitations log A2).

## 6. Notifications

Notifications are sent by the **app**, not the daemon, through KNotification, with events defined
in `konedrive.notifyrc` so that each can be configured in System Settings:

| Event | When |
|---|---|
| `signedOut` | the account is signed out or needs signing in again |
| `diskFull` | a download or an update failed for want of disk space |
| `downloadFailed` | a download failed |
| `updateFailed` | a file changed in OneDrive could not be updated here |
| `conflict` | "your changed version was moved to …", with "Show in Folder" |

Ordinary downloads notify nothing. The first event of a kind notifies at once; more of the same kind
within 10 s are sent as one summary ("2 more files could not be downloaded"). A notification is
chosen by the event's kind, never by its wording, except that a full disk is recognised by the
exact detail "not enough disk space".

The cost of sending them from the app: with the app quit, nothing notifies, and events that happen
meanwhile are never announced later; the window's lists still show everything (limitations log
A1). The app runs at login in the tray, so that is the exception.

## 7. Download progress in Plasma

The app watches `Transfers`. A transfer still running **2 s** after it first appears is reported to
Plasma as a `KJob` through `KUiServerV2JobTracker` — the mechanism Dolphin's own copy progress
uses — titled "Downloading from OneDrive", with the file name, bytes done of total, and speed.
Shorter transfers never show. At most **5** jobs are visible at once; the rest are summed into one,
"and N more files".

The daemon removes a transfer from `Transfers` before it signals the transfer's failure, and the
coalesced property can also arrive after the failure. So a job whose transfer leaves `Transfers` is
held for a **1.5 s** grace window: a `failed` or `update-failed` event naming the same path inside
it fails the job with the reason, and otherwise the job finishes as a success (limitations log A11).
A daemon restart ends every job with an error. The "Show download progress" switch in Settings is
on by default.

## 8. Thumbnails

Dolphin draws a preview by opening the file, and opening a placeholder downloads it: scrolling past
a folder of photos would download them all. The Windows client avoids this with thumbnails from the
cloud, and so does konedrive.

**What KIO does, measured** (`docs/kio-behavior.md`): a thumbnail is found by the MD5 of the file's
fully percent-encoded `file://` URI, in `~/.cache/thumbnails/{normal,large,x-large,xx-large}`
(128, 256, 512, 1024 px). A cached PNG tagged with `Thumb::URI` and a `Thumb::MTime` equal to the
file's time is drawn **without opening the file**; a stale time makes KIO discard it and open the
file. Listing a folder opens nothing.

**The filler** (`sync/thumbs.rs`) runs in the daemon, in the background:

- for each placed image or video (by the item's MIME type) with no current thumbnail, it asks Graph
  for one thumbnail, `c512x512`, and writes it to `x-large` as it is and scaled down to `large`
  and `normal`, each tagged with the file's URI and the placeholder's time;
- it runs after each listing cycle and every 10 minutes regardless, up to 200 items per run, one
  request at a time with 500 ms between them;
- `thumb_key` in the tree store records the cTag, path and time a thumbnail was made for, so a file
  is fetched again only when its content, name or time changes (a rename needs a new cache entry,
  because the cache is keyed by URI and checked against the time);
- a missing thumbnail, a body over 8 MiB, or an image over 4096 × 4096 px or 64 MiB of decoder
  memory is recorded like a 404 and never asked for again.

`xx-large` (1024 px) is **not** filled: it would be a second request per image at roughly four
times the bytes, for a size Dolphin asks for only at maximum zoom on a HiDPI screen, and upscaling
the 512 px answer would look blurred. At that zoom KIO makes its own thumbnail, which downloads the
file (limitations log K15).

**What still opens a file.** Type detection is a separate problem that a thumbnail does not solve:
for a name with no extension, or an ambiguous one like `.bin`, KIO reads the first bytes to learn
the type, and it does not consult `user.mime_type`, so no attribute can prevent it (limitations log
K1). An image shown before its thumbnail is filled, a preview of any other kind of file, and the
`xx-large` zoom also open the file. Turning previews off for the folder (View → Show Previews)
avoids those downloads.

## 9. Baloo

KDE's file indexer reads every file's content to index it, and a read of a placeholder downloads
it: left alone, Baloo would download the whole drive in the background the first time it walked the
folder. So a OneDrive folder is **excluded from Baloo**:

- the exclusion is added with `balooctl6 config add excludeFolders` and removed on Forget with
  `config rm`;
- before adding, the daemon checks whether the folder, or a directory above it, is excluded
  already, by reading Baloo's own settings file (`${XDG_CONFIG_HOME:-~/.config}/baloofilerc`,
  `[General]`, `exclude folders`, with or without `[$e]`, comma-separated, variables expanded,
  compared by path components) — `balooctl6 config list excludeFolders` was observed printing an
  empty list while the file held exclusions;
- whether *this daemon* added the exclusion is recorded in `config.toml`
  (`sync_root_baloo_excluded`), and Forget removes only an exclusion the daemon added, never one the
  user had set;
- every bring-up of a folder not recorded as excluded runs the same check-then-add, which only ever
  adds, so an exclusion that failed, timed out or was interrupted, or a Baloo installed later, is
  caught up;
- every call runs under a 10 s timeout and is killed when it expires; a hung or missing `balooctl6`
  behaves like a missing one and never blocks registration or Forget.

The cost: no Baloo search inside the folder — KRunner and Dolphin's search do not find files there
(limitations log W14).

## 10. Dolphin plugins

Two plugins in `dolphin/`, both running inside Dolphin, where a crash takes the file manager down
and any `open()` downloads what is shown. Both are tested to never open a file in the sync folder:
a test drives every code path under an inotify watch and fails on any open, and the tests run under
a poisoned allocator so that a use of freed memory cannot hide.

### 10.1 Emblems

The overlay plugin gives each file an emblem from its `user.konedrive.state`, read with `lstat` and
`lgetxattr`:

| State | Emblem |
|---|---|
| `online-only` | `cloudstatus` (a cloud) |
| `hydrating`, `dehydrating` | `state-sync` |
| `hydrated` | `emblem-checked` |
| outside a root, or an unrecognised state | none |

A file is inside a root when an ancestor directory carries `user.konedrive.root`; ancestors are
resolved with `lstat` and `readlink` per component, never by opening, so a symlink out of the root
does not count as inside. The answer is cached per directory while an inotify watch on that
directory stays in place, for the 256 most recently shown directories. `IN_ATTRIB` on the directory
reports a child's state change, so emblems update live. Emblems need no daemon: they work with it
stopped. Reading the attributes on Dolphin's UI thread costs about 7 µs per file inside the folder.

### 10.2 The context menu

The action plugin adds **Download** for `online-only` files and **Free up space** for `hydrated`
ones, for any selection; an action that does not apply is hidden. Each file is one asynchronous
D-Bus call (`Hydrate` or `Dehydrate`) with no reply timeout, since a download can take minutes. A
click on "Download" starts a stopped daemon through D-Bus activation, as any KDE service would,
rather than reporting that it is not running. A file already waiting is never sent twice, and at
most 1000 calls wait at once per window (limitations log K6). Refusals are explained by their error
name. The actions can be switched off in Dolphin's context-menu settings.

## 11. Known limits

The limitations log's sections 7 and 8 list them. The main ones: Dolphin still opens some files
itself (K1); notifications need the app running (A1); no emblems in search results or Recent Files,
which do not use `file://` URLs (K2); and the Plasma side — how the tray, the popups and the job
tracker actually render — is not covered by the tests, which run offscreen on private buses (A7).

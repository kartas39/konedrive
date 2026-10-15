# Pinning: "Always keep on this device"

What it means to keep a file or a folder on this device, as Windows' Files On-Demand does: the
pin in the store, what the plan downloads for it, freeing up space around pins, and how the bus,
the command line, Dolphin and the window show it. How a download runs and lands, and how a copy is
freed up, is [mount.md](mount.md) §5 and §6; the plan itself is [sync.md](sync.md) §6.

## 1. What the user sees

- Dolphin's menu has, for files and folders in a OneDrive folder, **Always Keep on This Device**,
  a checkbox, and **Free Up Space**. There is no separate "Download": pinning is how something is
  downloaded on purpose, and it stays downloaded.
- Pinning asks for no confirmation, however big the folder is. What a pinned folder gains later,
  in OneDrive or here, is downloaded too.
- A pinned item has an emblem of its own; so has a folder a pin holds for.
- Unchecking "Always keep on this device" only removes the pin; what it kept stays downloaded, as
  on Windows. "Free up space" is the action that frees anything.
- Unchecking it for something inside a pinned folder lets that one thing go and keeps the folder
  pinned. "Free up space" on it does the same and frees it.
- A pinned file is not downloaded while its download would leave less free space on the disk than
  the account keeps free (10 GiB by default); the window says how many wait.

## 2. The pin

### 2.1 In the store

A pin is a mark in the store's table `pins` (`crates/konedrive-tree/src/item_store/item_pins.rs`),
by the OneDrive item id of the item it is on. A mark is one of two (`PinMark`):

- **a pin** (`is_kept` 1): "always keep on this device", for the item and what the folder shows
  below it;
- **an exception** (`is_kept` 0): "not this one", below a pinned directory.

**Whether a pin holds** for an item is the nearest mark at it or above it in the folder
(`ItemStore::is_pinned`): an exception below a pin wins over the pin, and a pin below an exception
over the exception. A pin holds for no item the folder does not show.

**One writer.** `ItemStore::set_item_pin(item, is_pinned)` makes a pin hold for an item or not, in
one transaction: for a directory every mark below it comes off — what is asked of a directory is
asked of all of it — and the item gets the mark of its own that says so against the marks above
(`decide_own_pin_mark`): none where those say so already, else the one that says otherwise. It
answers whether a mark changed, that the item has no binding, or that the row is gone.

**A mark follows the binding.** Kept by the OneDrive item id, a pin stays with the item when
OneDrive renames or moves it, and with the binding when it moves to another item here
([sync.md](sync.md) §4). It goes, in the transaction of the statement that does it, when OneDrive
ceases to have the item — a removal OneDrive confirmed, a listing's removal and what it takes
below, what a full listing did not see — and when the row of its binding is deleted. So no mark
outlives what it was of, and nothing calls to clean marks.

**What can be pinned**: an item OneDrive has, a file, a directory or the folder itself. An item
made here and not yet sent has no binding, and is refused (`NotManaged`).

**Lost with the store.** The pins are rows of the store, and a store that is rebuilt loses them
(D36).

### 2.2 What the mount answers

The mount answers the extended attribute `user.konedrive.pin` with `1` on every item a pin holds
for, its own or a directory's above, and has no such attribute on anything else
(`mount/item_lookup.rs`, `decide_xattr_value`). Nothing is written on disk. A change of a pin is
announced to whoever watches the folder (`ItemAnnouncement::MarksChanged`), so that a file
manager reads the emblem again ([mount.md](mount.md) §9).

## 3. What a pin downloads

A pinned file whose content is not on disk is a case of the plan like any other ([sync.md](sync.md) §6): when the two sides agree on everything else, the
plan answers `FolderStep::DownloadContent` for it (`sync_plan/item_decision.rs`).

**How the plan is given them.** A pinned file that agrees on everything is neither unsent nor
changed in OneDrive, so the plan's bounded readers do not find it. One more reader does,
`ItemStore::list_pinned_files_to_download`: from the pins down, every row the folder shows below
a pinned directory, a bounded number at a time. It is the one reader of a plan that no index
bounds, so `MissingPinnedFiles` (`sync_plan_task/missing_pinned_files.rs`) asks it only while a
pinned file may be missing its content: from the task's start, and after whatever can leave one
so — a page of a listing or its end, a pin set, a refresh the user asked for, a download that did
not land, any step that was written. A reading that finds none missing stops the scans until the
next such cause. The journal says once when a download of pinned files begins, with how many
files and bytes, and once when it ends.

**The step.** `FolderSteps::run_download_content` (`folder_steps/mod.rs`) hands the file to the
mount's downloader (`ItemDownloader::download_and_land`), which joins a download of the same
content that runs already; the download takes a download slot of the account's transfer pool, so
that a program's read never waits behind a big pinned folder, and a large file may run in several
streams ([mount.md](mount.md) §5). Only its landing is a job of the write-operation thread. A
download that fails is tried again later, as any failed step ([sync.md](sync.md) §7); OneDrive
answering another content than the row's asks for a listing, and the next download asks anew.

**What holds it back:**

| What | Owner | |
|---|---|---|
| the folder's sync stopping, the sign-out, the user's pause or the automatic hold | `SendingHolds` ([writes.md](writes.md) §6) | a download is held by these alone, not by a read-only folder or a throttle of uploads |
| the free space the account keeps | `sync_plan_task/disk_space.rs` | a download that would leave less than the reserve is not started; a smaller one after it still is |

**The free space kept** is `free_space_reserve_gb` in the account's section of `config.toml`, 10 by
default (`Folder.SetFreeSpaceReserve`, `konedrivectl sync free-space`, the Account page). It is
measured anew for every choice, on the filesystem the backing directory is on; a download that
runs counts with its whole size; space that cannot be measured holds nothing. After a plan that
held a download for space, the next is made 60 seconds later. `Folder.LastError` and `Trouble` say
how many wait. A read of a file and `Files.Hydrate` are never held: the disk answers them
`ENOSPC` itself.

**New and changed items.** What OneDrive adds to a pinned directory, and what is moved into one,
is a file the pin holds for whose content is not on disk: the next plan downloads it. A pinned
file that changes in OneDrive takes OneDrive's content as any downloaded file does — its copy goes
(`FolderStep::TakeOneDriveContent`) — and is then downloaded again.

## 4. The calls

The bus's `org.konedrive.Files` at `/org/konedrive/Accounts` routes each path to the account
whose folder holds it; one call may span several accounts' folders. Every account checks its
paths before any account changes anything, and the counts are summed
([accounts.md](accounts.md)). The owner is `account_folder/pins.rs` and `free_up.rs`; the mount's
side is `ItemFilesInterface` (`mount/item_files_interface.rs`), the one place that turns a path
into an item (`ItemReader::find_item_by_path`).

| Member | Signature | What it does |
|---|---|---|
| `Pin(paths)` | `as → u` | A pin holds for each path from now on. A directory is pinned whole: a pin or an exception below it comes off. A path a directory above pins gets no mark of its own and loses an exception it had. The plan is woken, and downloads what the pin holds. Answers how many paths a mark changed for |
| `Unpin(paths)` | `as → u` | No pin holds for each path from now on, and nothing else changes: what is downloaded stays. A path a directory above pins gets an exception. A directory is let go whole. Answers how many paths a mark changed for |
| `FreeUp(paths)` | `as → (u files, t bytes, u skipped_pinned)` | As `Unpin`, and then everything at and below each path is freed up |
| `Menu(paths)` | `as → a{sv}` | What a context menu may offer for a selection (§4.2) |
| `Folder.PinnedCount` | `u` | How many pins there are, the exceptions not among them, of bindings rows still have (`ItemReader::count_pins`) |

Every path is checked before any mark changes: one outside every folder, or one OneDrive has no
item for, refuses the whole call. A mark that cannot be written stops the call with that failure;
the marks written before it stay, and `PinnedCount` says so. The mark is written first and the
downloads follow, so a crash in between loses nothing: the plan of the next start finds the
pinned files that are not on disk.

### 4.1 Freeing up around pins

A free-up of a file is the mount's (`mount/item_free_up.rs`, [mount.md](mount.md) §6): the row
says not on disk first, then the copy goes. Its decision (`decide_item_free_up`), the first that
holds: a file a pin holds for is left; a file changed here, or open for writing, or that a
download or a step is busy with, is left; a clean copy is freed.

- **`FreeUp(paths)`** takes the pins off first (as `Unpin`), then frees each path, a directory
  file by file, each in its own job of the write-operation thread, so that a program's write
  waits for one file of a long free-up and not for all of them. A file named itself that holds a
  change not uploaded yet, or is open for writing, refuses the whole call `NotUploaded` before
  anything changes; one inside a directory is left and counted. A pin below a freed directory
  comes off with it: the directory is let go whole. `skipped_pinned` counts every downloaded file
  left — one a download or a step is busy with, one changed here or open for writing. One
  `freed` event per path that freed anything.
- **`Files.Dehydrate(path)`**, the single file, is refused `NotAllowed` for a file a pin holds
  for, with the message `<path> is pinned: unpin it first`, which `konedrivectl` reads back.
- **`Folder.FreeUpSpace()`** frees every downloaded file of the folder and leaves every file a
  pin holds for, counted with what else it left.

### 4.2 What the menu is told

`Files.Menu(paths)` (`account_folder/menu.rs`) answers, without changing anything or opening a
file, what `Pin`, `Unpin`, `FreeUp` and `WebUrl` would do with a selection, so that the menu and
the calls cannot drift apart. It asks the mount the same questions the calls ask, on the store's
reading connection, which never waits for a sync that is writing. The keys:

- `paths`: the selected paths `Pin` takes, in the order given — what the menu then calls with.
  A path in no account's folder and an account's folder itself are left out: the folder itself
  is pinned from the command line only;
- `always-keep`: `hidden` when `paths` is empty, `on` when a pin holds for every path, else
  `off`;
- `free-up`: shown for any directory, any downloaded file and any file a pin holds for, as on
  Windows; `disabled`, with `free-up-why` `not-uploaded`, when a downloaded file among them holds
  a change not uploaded yet;
- `open-online`, `open-online-path`: "Open in OneDrive", for one selected path
  ([desktop.md](desktop.md)).

## 5. The command line

The commands take paths in any account's folder and go through `Files`; the paths decide the
accounts.

- `konedrivectl sync pin <paths…>`, `sync unpin <paths…>`, `sync free <paths…>`;
- `sync menu <paths…>`: the answer of `Files.Menu`, one line per key — the command-line path of a
  right click;
- `sync dehydrate <path>`, `sync free-up-space`;
- `sync free-space [<GiB>]`: the free space the downloads of pinned files leave;
- `sync status` has a line `Always on this device: N` for each folder that is up.

A refused `dehydrate` of a pinned file names the path and says to unpin it first.

## 6. The Dolphin plugin

**Emblems** (`dolphin/src/filestate.cpp`): the plugin reads the item's state and pin attributes,
and the pin attribute of the directories above it up to the folder's root, with `lgetxattr`, and
never opens a file.

| State | Emblem |
|---|---|
| not on disk, not pinned | the cloud |
| not on disk, pinned | the syncing emblem: it is to be downloaded |
| downloaded, not pinned | an outline check |
| downloaded and pinned | a filled check |
| a directory a pin holds for | a filled check |
| a directory, not pinned | none |

A file being downloaded shows the syncing emblem. A change waiting to be uploaded, or one that
cannot be, comes before all of these, for a file and a directory ([writes.md](writes.md) §7).

**The menu** (`dolphin/src/actionplugin.cpp`) reads nothing of a pin itself: it asks the daemon
once per menu (`Files.Menu`), shows the entries disabled until the answer comes, and drops them
when none comes within 2 seconds; the call never starts a daemon that is not running.
`inode/directory` is in the plugin's MIME types, or Dolphin never offers it for a folder.
Checking "Always keep on this device" calls `Pin` with the answer's `paths`, unchecking it calls
`Unpin`, never `FreeUp`; "Free up space" calls `FreeUp`. Each is one asynchronous call for the
whole selection, and a refusal is explained in the daemon's own words.

## 7. The window

The Status page shows "Always on this device: N items" when N is more than zero. The Account page
has the free space to keep on this computer's disk. Why a pinned file waits for space is in the
folder's `LastError` and `Trouble`, which the window shows as it shows any other
([desktop.md](desktop.md)).

## 8. Known limits

- The pins are lost with the store (D36).
- Pinning a big folder downloads everything in it, with no prompt; the free space kept is the one
  bound.
- Dolphin opens some files just to show them, which downloads them whether pinned or not (K1).

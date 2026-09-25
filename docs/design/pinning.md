# Pinning: "Always keep on this device"

What it means to keep a file or a folder on this device, as Windows' Files On-Demand does: the
pin itself, the downloads it asks for, freeing up space around pins, and how the D-Bus API, the
command line, the Dolphin plugin and the window show it. How a single file is filled or freed up
is in [hydration.md](hydration.md); how the folder follows OneDrive is in [sync.md](sync.md).

## 1. What the user sees

- Dolphin's menu has two actions for files and folders in the OneDrive folder: **Always keep on
  this device**, a checkbox, and **Free up space**. There is no separate "Download": pinning is
  how something is downloaded on purpose, and it stays downloaded.
- Pinning asks for no confirmation, however big the folder is.
- A pinned item has an emblem of its own, as on Windows -- a folder included, once it is
  effectively pinned.
- **D-A.** Unchecking "Always keep on this device" only removes the pin; the files it kept stay
  downloaded, exactly as on Windows. "Free up space" is the only action that frees anything.
- **D-B.** "Free up space" is offered for *any* folder in the root, as on Windows -- not only a
  pinned or already-downloaded one -- since it already works recursively on a folder regardless.
- Free up space on something a pinned folder keeps is not allowed: the folder has to be unpinned
  first.

## 2. The pin

A pin is the extended attribute `user.konedrive.pin`, with the value `"1"`, on the pinned file
or folder (`konedrive_fs::placeholder::XATTR_PIN`). The attribute is the pin's only record:

- it survives a rebuild of the tree store, which is SQLite and can be thrown away at any time;
- the Dolphin plugin reads it directly, with `lgetxattr`, and never asks the daemon;
- it moves with the item when OneDrive renames or moves it, since a rename keeps the inode.

The daemon writes it through a descriptor opened beneath the registered root
(`SyncRoot::open_item`: `openat2` with `RESOLVE_BENEATH` and `RESOLVE_NO_SYMLINKS`), lifting the
owner's write bit for the moment of the write, as it does for every other attribute on the
read-only folder. A file's pin is written under its per-inode lock, since a fill lifts the same
bit around its own attribute writes; a folder's under the lock every lift of a directory's write
bit in the daemon takes (`disk::dir_modes`), the materializer's own included. Only a directory
inside the root, the root itself, or a regular file that carries a konedrive state can be pinned;
nothing named `.konedrive-*`, or inside such a directory, can.

**Effective pin.** An item is *pinned* when it, or any folder above it up to the root, carries
the attribute. It is *explicitly pinned* when it carries it itself, and *pinned by folder X*
when X, a folder above it, does. Every such question is asked by name — `lgetxattr` on the item
and its ancestors — never by opening a file, which in a folder with interception would download
it.

When a downloaded file changes in OneDrive, its replacement is a new inode
([sync.md](sync.md) §9); a pin the old file carried is written onto the new one before it is
swapped in.

## 3. Pin

`Pin(paths)` puts a pin on each path and queues the download of every online-only file under it.
It answers how many files this call queued: a file already waiting or downloading is not counted
again. A path that a folder above it already pins is left as it is: no attribute is written for
it, and nothing is queued. Every path is checked before any is pinned; one outside the folder, a
`.konedrive-*` name, or a file that is not a konedrive file refuses the whole call.

The pin is written first and the downloads follow. A crash in between loses nothing: the next
sweep (§6) finds every pinned file that is still online-only.

## 4. The download queue

`sync::pin::Pins` holds the files waiting to be downloaded for a pin. A file is in it once,
however often a pin, a placement or a sweep asks for it, and leaves it when its download ends.

Each download goes through the ordinary fill path, `SyncService::fill_now` — the same as
`Hydrate`: opened beneath the root, taken under the per-inode lock, verified against OneDrive's
hash, checkpointed, shown in `Transfers` while it runs, and recorded as `downloaded` or `failed`
in the activity log. At most four run at once (`PIN_SLOTS`, equal to `FILL_SLOTS`), in slots of
their own. An open of a file whose pinned download is already running waits on that same
download, rather than starting a second one — as opening a file twice always does.

Just before a queued file is downloaded, the queue asks again whether it is still pinned. A file
whose pin was taken off since it was queued (§5) is passed over. A Forget drops the queue and
cancels the downloads under way; a cancelled download is left as any fill cut short is, its
checkpoint kept.

**A full disk.** A download that fails for want of space goes through the ordinary `failed` path,
whose detail is exactly `not enough disk space`; the window notifies. The rest of the queue is
dropped then, rather than failing one file after another: the files stay online-only and pinned,
and the next sweep queues them again. There is no size prompt and no check of the free space
beforehand.

A download that fails for any other reason leaves its file online-only; the sweep after the next
cycle that succeeds (§6) queues it again.

## 5. Unpin and free up space

`Unpin(paths)` is unchecking "Always keep on this device": each path's own pin comes off, and
nothing else changes — what is downloaded stays downloaded, as on Windows. It answers how many
pins came off.

`FreeUp(paths)` is the menu's "Free up space", the only action that frees anything:

- a path with a pin of its own loses the pin, and then everything under it is freed up;
- any other path is freed up.

Both refuse `NotAllowed` a path that a folder above it pins, with the message
`<path> is pinned by <folder>: unpin it first` — also when the path carries a pin of its own,
since it would stay pinned by the folder. The one exception is a folder that is itself among the
paths of the same call, with its own pin, which that call takes off: `FreeUp` of `docs` and
`docs/a.txt` together, with `docs` pinned, is allowed.

Every path is looked at before anything changes, so a refusal changes nothing; a path outside the
folder, a `.konedrive-*` name, or a file that is not a konedrive file refuses the whole call too.
The pins are taken off first (as §2 writes them) and the space freed after, so a crash in between
leaves downloaded files that are no longer pinned, which is what was asked for. A pin that cannot
be taken off stops the call with that failure, before anything is freed; the pins already taken
off stay off, and `PinnedCount` counts them off.

Each file is freed up through the same per-file path as `Dehydrate` and `FreeUpSpace` (the
helper's `ClearIgnore`, the write lease, the per-inode lock): a file in use, one a download is
busy with, or one that is not a clean download — changed here — is left as it is and counted in
`busy`, which has no separate count for changed files. A pin below the freed path stays explicit:
what it keeps is not freed, and each downloaded file it keeps is counted in `skipped_pinned`; so
is everything under a path that a folder above it was pinned over between the check and the walk.
One `freed` event is recorded for each path that freed anything.

**Around pins, elsewhere:**

- `FreeUpSpace`, for the whole folder, leaves every file that is pinned — by itself or by a folder
  above it — and counts them. Its D-Bus answer has no place for that count; its `freed` event
  says it ("…; 2 kept on this device"), and `konedrivectl sync free-up-space` adds a line when
  anything is pinned.
- `Dehydrate(path)`, the single-file call, is refused `NotAllowed` for a pinned file, with the
  same message: its own pin or a folder's.

## 6. New and changed items, and the sweep

**New items.** When a reconcile places a file inside a pinned folder — made new, or moved there by
OneDrive — or moves a folder there, the materializer notes it (`Applied::pinned`), reading each
directory's pin once per reconcile. After the reconcile, the sync queues every online-only file at
or under what was noted.

**Changed items.** A pinned file that is downloaded and changes in OneDrive is replaced eagerly,
as every downloaded file is ([sync.md](sync.md) §9). A pinned file that is still online-only —
its download failed, or has not come yet — is queued again by the next sweep.

**The sweep.** After every Full reconcile and at start — and after the next cycle that succeeds
once a pinned download failed, and after a cycle that moved or removed anything while pins
exist — the folder is walked: every item's pin
attribute is read, which finds the pinned roots and counts `PinnedCount` again, and every
online-only file under a pin is queued. This is what makes a pin crash-safe: the pin is written
before the downloads, and any download a crash, a failure or a full disk lost is found again.

- For a folder that shows OneDrive, "at start" is the first cycle of its sync, whose reconcile is
  always Full.
- Any other folder (the developer's mode) is swept in the background when it is registered or
  brought back up.

The walk reads names and attributes only (`lstat`, `lgetxattr`), skips `.konedrive-*`, never
follows a symbolic link or leaves the root's filesystem, and stops at `konedrive_fs::MAX_DEPTH`,
as the walk that measures `LocalBytes` does. A sweep that ends after its folder was forgotten, or
another registered, changes nothing.

`PinnedCount` is kept in memory between sweeps: `Pin` adds to it, `Unpin` and `FreeUp` take off.
A pin put on or taken off while a sweep walks is applied on top of what the walk found. A pinned
item that OneDrive moves or removes — or a folder with one inside — is counted right by the sweep
after that cycle.

## 7. D-Bus

| Member | Interface | Signature | Meaning |
|---|---|---|---|
| `Pin(paths)` | `Files1` | `as → u queued` | Pins each path (§3); how many files this call queued for download |
| `Unpin(paths)` | `Files1` | `as → u unpinned` | Takes each path's own pin off, and nothing else (§5); how many came off |
| `FreeUp(paths)` | `Files1` | `as → (u files, t bytes, u busy, u skipped_pinned)` | Frees up each path (§5) |
| `PinnedCount` | `Sync1` | `u`, read | How many files and folders in the account's folder carry a pin of their own |

`Pin`, `Unpin` and `FreeUp` are on `org.konedrive.Files1` at `/org/konedrive/Accounts`: each path
is routed to the account whose folder holds it, and one call may span several accounts' folders.
Every path is routed, and for `Unpin` and `FreeUp` every path checked for a pin by a folder above
it, before any account changes anything; the counts are summed over the accounts
([accounts.md](accounts.md) §3.5). `PinnedCount` is each account's, on its `org.konedrive.Sync1`.

`PinnedCount` travels in the coalesced `PropertiesChanged` with the other status properties
([desktop.md](desktop.md) §2.4). `NotAllowed` is a named error (`org.konedrive.Error.NotAllowed`)
like every other refusal ([desktop.md](desktop.md) §2.6); its message names the path refused and
the folder that pins it, in a fixed shape: `<path> is pinned by <folder>: unpin it first`.

## 8. The command line

The three commands take paths in any account's folder, and one call may name paths in several:
they go through `Files1`, the paths decide the accounts, and `--account` is refused.

- `konedrivectl sync pin <paths…>` — says how many files are downloading now.
- `konedrivectl sync unpin <paths…>` — takes the pins off; what is downloaded stays.
- `konedrivectl sync free <paths…>` — says what was freed, what was in use or changed here, and
  what a pin below kept.
- A refusal of either names the one path refused and the folder that pins it, and says to unpin or
  free up that folder first.
- `konedrivectl sync status` has a line `Always on this device: N` for each registered folder.

## 9. The Dolphin plugin

The plugin reads the pin attribute on the item and its ancestors with `lgetxattr`, and never
opens a file. `inode/directory` is in the action plugin's `MimeTypes`, or Dolphin never calls it
for a folder at all.

**Emblems**, following Windows:

| State | Emblem |
|---|---|
| Online-only, not pinned | the cloud, as before |
| Online-only, pinned (the sweep has not filled it yet) | the syncing emblem |
| Downloaded, not pinned | an outline check |
| Downloaded and pinned | a filled check |
| A folder, effectively pinned | a filled check |
| A folder, not pinned | none |

**Menu.** "Always keep on this device" and "Free up space" replace the earlier "Download" and
"Free up space". Both work on several items at once and on folders, and call `Pin`, `Unpin` or
`FreeUp` asynchronously, one call per action chosen for the whole selection.

- "Always keep on this device" is checked when the selection is effectively pinned. Checking it
  calls `Pin`; unchecking it calls `Unpin`, never `FreeUp` (D-A). While checked, it is disabled
  when *anything* in the selection is pinned by a folder above it -- even an item that is also
  explicitly pinned itself, since `Unpin` refuses it either way (§5) -- with a tooltip naming that
  folder.
- "Free up space" is shown for any folder in the selection, or anything downloaded or explicitly
  pinned (D-B). It is disabled when anything in the selection is pinned by a folder above it,
  again regardless of its own pin.
- One bad path in a selection (unmanaged, unrecognised, or one of konedrive's own
  `.konedrive-*` names) is left out of what is sent, rather than making the daemon refuse the
  whole call over it.
- A batch refused `NotAllowed` is explained with the daemon's own words, which name the actual
  refused path and the folder that pins it -- not the first path of the selection, which may be a
  different one.
- `FreeUp`'s `busy` also counts files changed here and not uploaded, not only files in use; the
  plugin says "N files are in use or were changed here and were kept."

## 10. The window

The Status page shows "Always on this device: N items" when N is more than zero. Nothing else
about pins is shown there.

## 11. Costs and limits

Recorded in [`../limitations-and-workarounds.md`](../limitations-and-workarounds.md):

- pinning a big folder downloads everything in it, four files at a time, with no prompt (P10);
- the sweep is a walk of the whole folder after each Full reconcile and at start, and a pinned file
  whose download failed waits for it (F38).

# Pinning: "Always keep on this device"

What it means to keep a file or a folder on this device, as Windows' Files On-Demand does: the pin
itself, the downloads it asks for, freeing up space around pins, and how the D-Bus API, the command
line, the Dolphin plugin and the window show it. How a single file is filled or freed up is in
[hydration.md](hydration.md); how the folder follows OneDrive is in [sync.md](sync.md).

## 1. What the user sees

- Dolphin's menu has, for files and folders in a OneDrive folder, **Always Keep on This Device**, a
  checkbox, and **Free Up Space**. There is no separate "Download": pinning is how something is
  downloaded on purpose, and it stays downloaded.
- Pinning asks for no confirmation, however big the folder is.
- A pinned item has an emblem of its own, as on Windows; so has a folder, when it or a folder above
  it is pinned.
- **D-A.** Unchecking "Always keep on this device" only removes the pin; the files it kept stay
  downloaded, as on Windows. "Free up space" is the only action that frees anything.
- **D-B.** "Free up space" is offered for *any* folder, as on Windows, not only a pinned or
  downloaded one. On something a pinned folder keeps it is refused: the folder has to be unpinned
  first.

## 2. The pin

A pin is the extended attribute `user.konedrive.pin` on the pinned file or folder. It is written
with the value `"1"`; its presence is the pin. The attribute is the pin's only record:

- it survives a rebuild of the tree store, which can be thrown away at any time;
- the Dolphin plugin reads it directly, with `lgetxattr`, and never asks the daemon;
- it moves with the item when OneDrive renames or moves it, since a rename keeps the inode.

The daemon writes it through a descriptor opened beneath the registered root, with no symbolic link
followed on the way. It lifts the owner's write bit for the moment of the write, as it does for
every other attribute in a read-only folder. A file's pin is written under its per-inode lock, since
a fill lifts the same bit around its own attribute writes. A folder's is written under the one lock
that every lift of a directory's write bit takes in that folder. What can be pinned: a directory
inside the root, the root itself, or a regular file that carries a konedrive state. Nothing named
`.konedrive-*`, and nothing inside such a directory, can.

**Effective pin.** An item is *pinned* when it, or any folder above it up to the root, carries the
attribute. It is *explicitly pinned* when it carries it itself, and *pinned by folder X* when X, a
folder above it, does. The folders above are always asked by name (`lgetxattr`). So is the item
itself by the sweep, the free-up walks, the check before a queued download and `Files.Menu`. `Pin`,
`Unpin` and `FreeUp` open the item they are given and read its own pin through that descriptor; the
open is the daemon's own, which the helper lets through ([hydration.md](hydration.md) §5.1).

When a downloaded file changes in OneDrive, its replacement is a new inode ([sync.md](sync.md) §9);
a pin the old file carried is written onto the new one before it is swapped in.

## 3. Pin

`Pin(paths)` puts a pin on each path and queues the download of every online-only file under it. It
answers how many files this call queued: a file already waiting or downloading is not counted again.
A path that a folder above it already pins is left as it is: no attribute is written for it, and
nothing is queued. Every path is checked before any is pinned; one outside the folder, a
`.konedrive-*` name, or a file that is not a konedrive file refuses the whole call. The pin is
written first and the downloads follow. A crash in between loses nothing: the next sweep (§6) finds
every pinned file that is still online-only. When a pin cannot be written, the call ends with that
failure; the pins written before it stay, and their files are queued.

## 4. The download queue

The daemon keeps, for each folder, the files waiting to be downloaded for a pin
(`hydration/pin.rs`). A file is in the queue once, however often a pin, a placement or a sweep asks
for it, and leaves it when its download ends.

**Order.** What a pin, a placement or a sweep queues goes folder by folder: a folder's files by name
first, then its subfolders by name, each the same way (depth first). Names compare lower-cased, with
a run of digits as a number (`file2` before `file10`): close to Dolphin's order, without its locale
rules. A batch queued later goes after what already waits; the queue is never sorted again.

Each download is an ordinary fill, the same as `Hydrate` ([hydration.md](hydration.md) §6): opened
beneath the root, taken under the per-inode lock, verified against OneDrive's hash, checkpointed,
shown in `Transfers.Downloads` while it runs, and recorded as `downloaded` or `failed` in the
activity log. It takes a background slot of the account's transfer pool
([hydration.md](hydration.md) §6.4), so an open never waits behind a big pinned folder. A large file
(100 MiB and up, by its placeholder's size) also waits for the pool's limit on the streams of large
sync transfers (`[transfers] large`). Small and large files wait in two queues, each in the order
above, so a large file held by that limit lets the small ones behind it go. An open of a file whose
pinned download is running takes the same per-inode lock, so it waits for that download and starts
no second one.

**A large file in parts.** Only a large pinned file downloads over several streams at once; the
rules, and how the file shows in `Transfers`, are in [hydration.md](hydration.md) §7.5. With the
default of 4 large streams and nothing else waiting, one large file runs in up to 4 streams.

Just before a queued file is downloaded, the queue asks again whether it is still pinned. A file
whose pin was taken off since it was queued (§5) is passed over. A Forget drops the queue and
cancels the downloads under way; a cancelled download is left as any fill cut short is, its
checkpoint kept.

**A full disk.** A download that fails for want of space (`ENOSPC` or `EDQUOT`) is recorded as
`failed` with the detail `not enough disk space`, which the window notifies about. The rest of the
queue is dropped then, rather than failing one file after another: the files stay online-only and
pinned, and the next sweep queues them again. There is no size prompt and no check of the free space
beforehand. A download that fails for any other reason leaves its file online-only; the sweep after
the next cycle that succeeds (§6) queues it again.

## 5. Unpin and free up space

`Unpin(paths)` is unchecking "Always keep on this device": each path's own pin comes off, and
nothing else changes. What is downloaded stays downloaded. It answers how many pins came off.

`FreeUp(paths)` is the menu's "Free up space", the only action that frees anything:

- a path with a pin of its own loses the pin, and then everything under it is freed up;
- any other path is freed up.

Both refuse `NotAllowed` a path that a folder above it pins, with the message
`<path> is pinned by <folder>: unpin it first`. That holds also when the path carries a pin of its
own, since it would stay pinned by the folder. The one exception is a folder that is itself among
the paths of the same call, with its own pin, which that call takes off: `FreeUp` of `docs` and
`docs/a.txt` together, with `docs` pinned, is allowed.

Every path is looked at before anything changes, so a refusal changes nothing. These refuse the
whole call too:

- a path outside the folder, a `.konedrive-*` name, or a file that is not a konedrive file;
- for `FreeUp`, a folder with interception whose helper is not connected (`NoHelper`);
- for `FreeUp`, a file among the paths that has a change waiting to be uploaded (`NotUploaded`).

The pins are taken off first (as §2 writes them) and the space freed after, so a crash in between
leaves downloaded files that are no longer pinned, which is what was asked for. A pin that cannot be
taken off stops the call with that failure, before anything is freed; the pins already taken off
stay off, and `PinnedCount` counts them off.

Each file is freed up as `Dehydrate` frees one ([hydration.md](hydration.md) §8). Below a freed
folder, a file in use, one a download is busy with, one changed here, or one with a change waiting
to be uploaded is left as it is and counted in `busy`, which has no separate count for changed
files. A pin below the freed path stays: what it keeps is not freed, and each downloaded file it
keeps is counted in `skipped_pinned`. So is everything under a path that a folder above it was
pinned over between the check and the walk. One `freed` event is recorded for each path that freed
anything.

**What the menu is told.** `Files.Menu(paths)` answers, without changing anything, what these calls
would do with a selection ([desktop.md](desktop.md) §2.9 has its keys): which of the paths `Pin`
takes; whether every one of them is pinned, and whether `Unpin` would refuse them; whether `FreeUp`
would refuse them, for which one reason, and the name of the folder above that pins. An account's
folder itself is left out, although `Pin` takes it: the root is pinned from the command line only.

It asks with the checks the calls themselves make first, so the menu and the calls cannot drift
apart. One check is asked another way. Whether a downloaded file has a change waiting to be uploaded
goes to the tree store's read-only connection, once for the whole selection, so that the answer does
not wait for a sync that is writing; `FreeUp` keeps its exact check on the writer. When that cannot
be told now, the menu says so, as a reason of its own.

**Around pins, elsewhere:**

- `FreeUpSpace`, for the whole folder, leaves every file that is pinned, by itself or by a folder
  above it, and counts them. Its D-Bus answer has no place for that count. Its `freed` event,
  recorded when anything was freed, says it ("…; 2 kept on this device"), and
  `konedrivectl sync free-up-space` adds a line when anything is pinned.
- `Dehydrate(path)`, the single-file call, is refused `NotAllowed` for a pinned file, with the same
  message: its own pin or a folder's.

## 6. New and changed items, and the sweep

**New items.** When a reconcile places a file inside a pinned folder (made new, or moved there by
OneDrive) or moves a folder there, it notes the item. After the reconcile, the sync queues every
online-only file at or under what was noted.

**Changed items.** A pinned file that is downloaded and changes in OneDrive is replaced eagerly, as
every downloaded file is ([sync.md](sync.md) §9). A pinned file that is still online-only, because
its download failed or has not come yet, is queued again by the next sweep.

**The sweep** walks the folder: every item's pin attribute is read, which finds the pinned roots and
counts `PinnedCount` again, and every online-only file under a pin is queued. It runs:

- after every Full reconcile, and so at start: the first cycle of a folder's sync is always Full;
- after the next cycle that succeeds, once a pinned download failed;
- after a cycle that moved or removed anything while pins exist;
- for a folder that does not show OneDrive (the developer's mode), in the background when it is
  registered or brought back up.

This is what makes a pin crash-safe: the pin is written before the downloads, and any download a
crash, a failure or a full disk lost is found again. The cost is a walk of the whole folder
(limitations log F38).

The walk reads names and attributes only (`lstat`, `lgetxattr`), skips `.konedrive-*`, never follows
a symbolic link or leaves the root's filesystem, and stops at 128 levels, as the walk that measures
`LocalBytes` does. A sweep that ends after its folder was forgotten, or another registered, changes
nothing; the folder is told by its path. `PinnedCount` is kept in memory between sweeps: `Pin` adds
to it, `Unpin` and `FreeUp` take off. A pin put on or taken off while a sweep walks is applied on
top of what the walk found. A pinned item that OneDrive moves or removes, or a folder with one
inside, is counted right by the sweep after that cycle.

## 7. D-Bus

| Member | Interface | Signature | Meaning |
|---|---|---|---|
| `Pin(paths)` | `Files` | `as → u queued` | Pins each path (§3); how many files this call queued for download |
| `Unpin(paths)` | `Files` | `as → u unpinned` | Takes each path's own pin off, and nothing else (§5); how many came off |
| `FreeUp(paths)` | `Files` | `as → (u files, t bytes, u busy, u skipped_pinned)` | Frees up each path (§5) |
| `Menu(paths)` | `Files` | `as → a{sv}` | What a context menu may offer for a selection (§5, [desktop.md](desktop.md) §2.9) |
| `PinnedCount` | `Folder` | `u`, read | How many files and folders in the account's folder carry a pin of their own |

`Pin`, `Unpin` and `FreeUp` are on `org.konedrive.Files` at `/org/konedrive/Accounts`: each path is
routed to the account whose folder holds it, and one call may span several accounts' folders. Every
path is routed, and every account makes the checks of §3 and §5 on its paths, before any account
changes anything; the counts are summed over the accounts ([accounts.md](accounts.md) §3.5).
`PinnedCount` is each account's, on its `org.konedrive.Folder`.

`PinnedCount` travels in the coalesced `PropertiesChanged` with the other status properties
([desktop.md](desktop.md) §2.1, §2.4). `NotAllowed` is a named error
(`org.konedrive.Error.NotAllowed`) like every other refusal ([desktop.md](desktop.md) §2.6); its
message names the path refused and the folder that pins it, in the shape §5 gives.

## 8. The command line

The commands take paths in any account's folder, and one call may name paths in several: they go
through `Files`, the paths decide the accounts, and `--account` is refused.

- `konedrivectl sync pin <paths…>` — says how many files are downloading now.
- `konedrivectl sync unpin <paths…>` — takes the pins off; what is downloaded stays.
- `konedrivectl sync free <paths…>` — says what was freed, what was in use or changed here, and what
  a pin below kept.
- A refusal of `unpin` or `free` names the one path refused and the folder that pins it, and says to
  unpin or free up that folder first.
- `konedrivectl sync menu <paths…>` — prints what the menu would offer for the paths together, the
  answer of `Files.Menu`: the command-line path of a right click.
- `konedrivectl sync status` has a line `Always on this device: N` for each folder that is up.

## 9. The Dolphin plugin

For the emblems, the plugin reads the pin attribute on the item and its ancestors with `lgetxattr`,
and never opens a file. For the menu it reads nothing of a pin: it asks the daemon (`Files.Menu`,
§5). `inode/directory` is in the action plugin's `MimeTypes`, or Dolphin never calls it for a folder
at all.

**Emblems**, following Windows:

| State | Emblem |
|---|---|
| Online-only, not pinned | the cloud |
| Online-only, pinned (not downloaded yet) | the syncing emblem |
| Downloaded, not pinned | an outline check |
| Downloaded and pinned | a filled check |
| A folder, effectively pinned | a filled check |
| A folder, not pinned | none |

A file being downloaded or freed up shows the syncing emblem. A change waiting to be uploaded comes
before all of these, for a file and for a folder ([desktop.md](desktop.md) §10.1).

**Menu.** "Always keep on this device" and "Free up space" work on several items at once and on
folders, and call `Pin`, `Unpin` or `FreeUp` asynchronously, one call per action chosen for the
whole selection. What each entry shows is the daemon's answer to one `Files.Menu` call per menu. The
menu does not wait for it: the entries are shown disabled until it comes. The rule is the daemon's
(§5), and the plugin shows it ([desktop.md](desktop.md) §10.2):

- "Always keep on this device" is checked when every path the daemon takes is pinned, by itself or
  by a folder above it. Checking it calls `Pin`; unchecking it calls `Unpin`, never `FreeUp` (D-A).
  While checked, it is disabled when `Unpin` would refuse the selection: something in it is kept by
  a folder above it, even an item that is also explicitly pinned itself (§5), unless that folder is
  selected too, with its own pin. The tooltip names the folder.
- "Free up space" is shown for any folder in the selection, or anything downloaded or explicitly
  pinned (D-B). It is disabled when `FreeUp` would refuse the selection: for a folder above, as
  "Always keep" is; when the folder's helper is not connected; when a selected file has a change
  waiting to be uploaded; and when the daemon cannot tell that now.
- A path the daemon does not take (a file of the user's own, an unrecognised state, one of
  konedrive's own `.konedrive-*` names, a link) is not in the answer's `paths`, which is what is
  sent, rather than making the daemon refuse the whole call over it.
- With no answer from the daemon within 2 s, the entries go (issue #232). The call never starts a
  daemon that is not running.
- A batch refused `NotAllowed` is explained with the daemon's own words, which name the path
  actually refused and the folder that pins it.
- `FreeUp`'s `busy` also counts files changed here and not uploaded, not only files in use; the
  plugin says "N files are in use or were changed here and were kept."

## 10. The window

The Status page shows "Always on this device: N items" when N is more than zero. Nothing else about
pins is shown there.

## 11. Costs and limits

- Pinning a big folder downloads everything in it, with no prompt and no look at the free space
  (issue #217).
- A large pinned file in parts keeps only its gap-free start across a failure or a restart
  (issue #230), and a transfer that starts waiting for a slot can wait for one piece of 256 MiB to
  end.
- The sweep is a walk of the whole folder, and a pinned file whose download failed waits for it
  (limitations log [F38](../limitations/F38.md)).

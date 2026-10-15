# Design decisions

The decisions that shape konedrive, each with the reason for it and what it costs. An entry
describes the code as it is, and points to where the mechanism is described.

Entries are grouped: [architecture](#1-architecture), [the mount](#2-the-mount), [the store and
the sync](#3-the-store-and-the-sync), [uploads](#4-uploads), [account and
security](#5-account-and-security), [the desktop](#6-the-desktop), [testing](#7-testing).

## 1. Architecture

### 1.1 The sync folder is a FUSE mount over the folder itself, served by the daemon as the user

- **Decision.** Each account's folder is a FUSE mount (`fuse.konedrive`, `nosuid`, `nodev`,
  without `allow_other`) that `konedrived` serves as the user, put on and taken off with the
  system's `fusermount3`. The mount lies over the folder itself: under it, the backing directory
  holds the downloaded copies and what the user made here, and the daemon reaches it through a
  descriptor opened before the mount. There is no privileged helper, no system service and no
  kernel module ([mount.md](mount.md) §2, §3).
- **Why.** A file that is not on disk has nothing on disk at all, so no program can read it as
  zeros, whatever dies. Every name, read and change of the folder reaches the daemon as a request
  of the kernel: a removal is known as a removal and a save by rename as a rename, with nothing
  inferred after the fact. Nothing of konedrive runs with privilege. The mount lies over the folder
  and not over an empty one beside a hidden copy, so that while no daemon runs the folder still
  shows the downloaded files.
- **Trade-off.** Every read and write of a downloaded file passes through the daemon, which is
  slower than the filesystem beneath, most for work on many small files; the kernel's passthrough
  would avoid it but needs `CAP_SYS_ADMIN`. While the daemon is not running the folder is the
  backing directory alone (F296). Nobody but the user reaches the folder, root included (F301). A
  move into or out of the folder, or between two accounts' folders, is a copy and a removal
  (F124). The folder holds no links, special files or extended attributes of the user's own
  (F300).

### 1.2 One daemon serves every account, with a mount for each

- **Decision.** One `konedrived` per user serves all of the user's accounts. Each account has its
  own sign-in, folder and mount, store, listing, sync plan's task and D-Bus objects
  ([accounts.md](accounts.md) §3).
- **Why.** The window, `konedrivectl` and the Dolphin plugins talk to one name on the bus, and a
  path is enough to find its account (below). One user service is started, stopped and upgraded;
  the settings every account shares (the client id, the automatic hold) have one owner.
- **Trade-off.** A daemon that dies takes every account's folder down with it, until the unit's
  tidy step or the next start takes the dead mounts off (F296).

### 1.3 Per-file calls go to one interface, routed by path

- **Decision.** `Hydrate`, `Dehydrate`, `ItemState`, `Pin`, `Unpin`, `FreeUp`, `WebUrl` and `Menu`
  are on the daemon-wide `org.konedrive.Files`, which finds each path's account by its folder. Each
  account's own objects keep what concerns its folder as a whole.
- **Why.** The Dolphin plugin and `konedrivectl` know a path, not an account, and a selection in
  Dolphin may span two accounts' folders.
- **Trade-off.** One path in no account's folder refuses a whole `Pin`, `Unpin` or `FreeUp` call.

### 1.4 No alias for the single-account D-Bus object

- **Decision.** `/org/konedrive/Daemon` is not served. The window, the CLI and the Dolphin plugins
  use `/org/konedrive/Accounts` and the per-account objects.
- **Why.** konedrive has no outside clients, and its clients ship in the same packages as the
  daemon. An alias would need a meaning for "the first account" that changes when that account is
  removed, and tests of its own, to serve no one.
- **Trade-off.** None.

### 1.5 Rust for the daemon and the CLI; C++/QML for the window and the plugins

- **Decision.** The daemon and `konedrivectl` are Rust; the window is C++/QML on Kirigami; the
  Dolphin plugins are C++ against KIO.
- **Why.** Memory safety matters most in the daemon, which parses network input and answers the
  kernel for every file of the folder. The window and the plugins use KDE's own stack, which is
  C++.
- **Trade-off.** Two toolchains, and sentences that must say the same thing in both: they are
  generated from one Rust crate (`konedrive-text`), and a test keeps the reasons in step.

### 1.6 Read before write

- **Decision.** Reading is the default: a new account is read-only, and changes are taken and
  uploaded only in a folder whose account was switched to read-write ([writes.md](writes.md) §2).
- **Why.** A read-only client is safe to run against a live account: a bug cannot damage the copy
  in OneDrive, and Microsoft refuses any write made with its token (§5.4).
- **Trade-off.** A read-only folder refuses every change a program makes (`EROFS`).

### 1.7 A folder an older konedrive kept is not taken over

- **Decision.** A mount is never put over a folder that holds placeholders of an older konedrive
  (files with the name of a OneDrive file and none of its content) or its holding directory. Such a
  folder does not start, and the user is told to remove the account and add it again
  (`mount/backing_directory/legacy_placeholders.rs`).
- **Why.** What such a folder holds would be taken for downloaded copies or for the user's own
  files, and a placeholder's emptiness could be uploaded as the file's content. A conversion would
  be code that runs once per folder and loses data when it is wrong.
- **Trade-off.** The account's folder is set up anew: listed again in full, and its pins and
  activity log gone.

## 2. The mount

### 2.1 The kernel's node id is the local item id

- **Decision.** The node id the kernel holds for an item is the item's local item id; the root is
  item 1, the number FUSE gives the root; the generation is always 0, since ids are never given
  twice. No table of node ids is kept ([sync.md](sync.md) §2).
- **Why.** A node then means the same item through every rename, through a listing that moves it
  and across a restart of the daemon, and it can never be resolved at a place the item left:
  nothing is kept per node that could disagree with the row. The kernel's `forget` needs nothing.
- **Trade-off.** A request by a node whose row is gone is answered `ESTALE`; one that carries an
  open handle is answered from the handle's file. An item has one place, so hard links are refused
  (F300).

### 2.2 What the folder shows is the local version alone, with three named exceptions

- **Decision.** Every name, place and attribute the mount answers is read from the item's local
  version in the store, never from the disk, the OneDrive version or the plan. Three exceptions:
  the sync state is a function of the row; the blocks a file takes are its backing file's; and the
  size and times of a file a program holds open for writing are its backing file's, read through
  the writer's handle and written into the row at the last close ([sync.md](sync.md) §3).
- **Why.** One source cannot disagree with itself. The blocks are what the disk really holds, so
  that `du` and the window's space are true and a file not on disk takes none. The size of a file
  open for writing must be the backing file's because the kernel asks for the size before each
  append and puts the write where the answer says: answered the row's size of before the open, a
  program appending 60 lines was measured to leave one, and the short file was uploaded.
- **Trade-off.** A `getattr` of a file open for writing reads the backing file. The kernel's
  writeback cache stays off: with it the kernel would own size and times, and a daemon's death
  would lose writes that had returned.

### 2.3 Every change of the folder runs on one thread, under the folder's tree lock

- **Decision.** The changes a program makes (make, rename, remove, the opens of writers,
  truncate, the time, a writer's last close), the steps towards the folder, a landing and a free-up
  run one at a time on the mount's write-operation thread, under the folder's tree lock. Reads,
  lookups and writes of bytes stay on the mount's own threads. A read-only folder refuses a change
  on the mount's thread, before anything is queued ([mount.md](mount.md) §2).
- **Why.** No two changes of names interleave, a step towards the folder never runs beside a
  program's change, and the program hears that its change was made only once the row says so. The
  listing writes OneDrive versions only and takes no tree lock, so a `mkdir` never waits behind a
  listing ([sync.md](sync.md) §5).
- **Trade-off.** One slow job (a landing that syncs a large file) holds every other change of that
  folder behind it.

### 2.4 A change of a name is on record before the disk is touched; content goes the other way

- **Decision.** Before a make, a rename or a removal changes the backing directory, what it is
  about to do is written to the store (`place_change_records`); then the disk; then the row, which
  takes the record in its own transaction. A row that cannot be written is undone on disk; an undo
  that fails takes the mount down. A file's content goes the other way: the first writer's open
  marks the content changed in the row, then bytes go to the backing file with no row touched per
  write ([mount.md](mount.md) §3).
- **Why.** A daemon that dies between disk and row leaves the record, and the start-up check
  finishes or undoes exactly that change, with no guessing and no mark on disk asked. A row never
  says a copy is clean while the disk may hold the user's bytes.
- **Trade-off.** One more transaction of the store per change of a name.

### 2.5 A file that is not on disk is read as ranges, then downloaded whole past a threshold

- **Decision.** An open of a file that is not on disk is answered at once. A read fetches the range
  asked for from OneDrive; once the bytes read so reach `[transfers] download_after` (1 MiB unless
  `config.toml` says otherwise), or the whole of a smaller file, the file is downloaded whole, and
  the reads go on from the download as it arrives ([mount.md](mount.md) §5).
- **Why.** A program that reads the start of a file (a file manager finding its type, a
  thumbnailer, `file`) costs a range, not the whole file. A program that reads the file through
  gets it at the speed of one download, checked against its hash, and kept on disk. A read that
  waits for OneDrive can be interrupted: the workspace uses `fuser` with one change, which hands the
  kernel's `INTERRUPT` to the daemon (`vendor/fuser/README.konedrive.md`).
- **Trade-off.** A range is not checked against the content's version (F297). Every read before
  the threshold is a request to OneDrive. A patched copy of `fuser` is kept in the repository.

### 2.6 A read never mixes two contents

- **Decision.** A handle remembers the content it was opened as. Its bytes come from its own
  backing file, from the landed copy of that content, or from the running download of that very
  content, below what the download has told to be of it. When OneDrive is found to have another
  content, the read fails `ESTALE`, and the next open reads the new one ([mount.md](mount.md) §5).
- **Why.** A program handed half of one version and half of another has a corrupt file and no
  error to tell it so.
- **Trade-off.** A program reading a file while it changes in OneDrive gets an error and must open
  the file again. Through a range alone another content is told only once a download or a listing
  told it (F297).

### 2.7 A download lands only checked and synced, and the row says on disk last

- **Decision.** A whole download is written into a file with no name in the backing directory,
  checked against the content's size and `quickXorHash`, synced, linked under the name the row has
  by then, its directory synced, and only then the row says on disk, if the row is still what the
  download was started for. A content that changed in OneDrive, or a failed hash, lands nothing
  ([mount.md](mount.md) §5).
- **Why.** A row never says on disk what the disk does not hold for good. A death before the link
  leaves no name; a death between the link and the row leaves a whole copy the start-up check
  recognises by its marks. The hash is the one Graph gives for every drive, and it covers the whole
  file, a part continued from an earlier try included.
- **Trade-off.** Two syncs per download. A file Graph gives without a hash lands unchecked (F34). A
  download continues from where an earlier try of the same run stopped, but not across a stop of
  the daemon: a file with no name does not survive it.

### 2.8 A downloaded copy that OneDrive changed is dropped, not replaced

- **Decision.** When OneDrive has new content for a file whose copy here is clean, the file takes
  OneDrive's content and the copy on disk goes; a pinned file is then downloaded again
  ([sync.md](sync.md) §6).
- **Why.** The folder never shows the old bytes as the new content, and no second copy needs room
  beside the first. A program that holds the old copy open goes on reading it through its handle.
- **Trade-off.** A downloaded file that is not pinned is not on disk after OneDrive changed it, and
  is read from OneDrive again.

### 2.9 Freeing up space removes only a clean copy, and the row says so first

- **Decision.** A copy is freed only while the local content is the synced one, no program holds
  the file open for writing, no step or download runs for it, and no pin holds it. The row says not
  on disk first, then the copy goes ([mount.md](mount.md) §6; [pinning.md](pinning.md) §4.1).
- **Why.** A copy changed here is the only one there is. With the row first, a death between the
  two leaves a whole copy that the start-up check reads as landed.
- **Trade-off.** "Free up space" leaves out every file that is changed, open for writing or busy,
  and says so.

### 2.10 The mount waits for the start-up check, which does not guess what a person did

- **Decision.** Before the mount goes over the folder, the backing directory is compared with the
  rows once, one place at a time: what the rows say there against what the disk has there. A file
  of OneDrive missing from its place is shown again as not on disk, and nothing is removed or moved
  in OneDrive; a file found elsewhere is a new file ([mount.md](mount.md) §7).
- **Why.** A copy, a backup put back and a power loss leave the same picture as a move or a
  removal by hand, and a wrong guess removes or moves the file in OneDrive and on every other
  device. A removal made with the daemon running is a request of the kernel and needs no guess.
- **Trade-off.** A change made in the folder on disk while the daemon is not running is not a move
  or a removal in OneDrive, and an edit that keeps a file's size and time is not seen (F298). Every
  start walks the whole backing directory before the folder is served.

### 2.11 While no daemon runs, the folder is a plain directory without its write bits

- **Decision.** The unit's tidy step (`konedrived --tidy`, after the daemon's end however it
  ended) takes a dead mount off with `fusermount3 -u -z` and the write bits off everything in the
  folder; the next mount gives them back, and a Forget gives them back for good
  ([mount.md](mount.md) §8).
- **Why.** A dead mount fails every path with "not connected". What a program writes into the
  backing directory while nothing serves it could be found only by the start-up check, which does
  not see moves or removals (§2.10); without write bits nothing is put there by mistake.
- **Trade-off.** While the daemon is stopped the folder shows the downloaded files, read-only, and
  no file that is not on disk (F296). Removing the package leaves a registered folder so (R3).

### 2.12 What a step changed is told through the mount

- **Decision.** A change a step towards the folder or a landing made is announced by the inotify
  bridge: its own thread creates, touches or removes the name by path through the mount, and the
  mount answers its requests from the announcement and the rows ([mount.md](mount.md) §9).
- **Why.** The kernel tells inotify watchers of what happens through its own calls on the mount; a
  row the daemon writes raises nothing, and Dolphin and other file managers watch with inotify.
- **Trade-off.** Every announcement costs requests through the mount, on one thread, in order.

### 2.13 A file's mode is its kind and one executable bit

- **Decision.** A file shows `0644`, or `0755` with its executable bit; a directory `0755`; a
  read-only folder shows no write bit. A `chmod` keeps the owner's executable bit of a file in the
  store and nothing else (`mount/item_attributes.rs`).
- **Why.** OneDrive keeps no Unix permissions, and the mount is its owner's alone, so no other bit
  would protect anything.
- **Trade-off.** The bit stays on this computer (F303).

### 2.14 No trash inside the folder

- **Decision.** The mount refuses to make a file manager's trash directory: a name beginning
  `.Trash` at the top of the folder (`UploadRules::is_trash`).
- **Why.** A trash inside the folder would be uploaded, and a file "moved to the trash" would stay
  in OneDrive under another name. With none, KIO offers to remove the file instead.
- **Trade-off.** With KDE Frameworks before 6.21, "Move to Trash" downloads the file into the home
  trash (K34).

## 3. The store and the sync

### 3.1 One row per item with three versions; the differences are derived

- **Decision.** The store (`fuse.sqlite`) has a row per item (`items`, `ItemStore`): its local item
  id, its binding to a OneDrive item, its local and OneDrive versions, the place and content the two
  sides last agreed on, whether its content is on disk. What is unsent, the sync state Dolphin
  shows and an item's path are derived from the row and never stored ([sync.md](sync.md) §2).
- **Why.** What the folder shows is written in one place, and what is to be sent is found again
  from two versions of one row, with no queue of changes that could lose a row or disagree with
  the folder. An item has one identity from the moment it is made, also before OneDrive has it.
  The schema itself holds the rules of the rows (one name per directory, a parent that is a
  directory, a binding of one item).
- **Trade-off.** The store is the only record of what the folder shows of a file not on disk, and
  of an unsent change that left nothing on disk (§3.9).

### 3.2 The local item id follows the file; the binding follows what OneDrive must see as one item

- **Decision.** A binding moves between items only by three operations of the store
  (`take_over_binding`, `give_up_binding`, `merge_items`): a file saved by a rename over another
  takes its binding; a file changed here that OneDrive changed or removed gives its binding up and
  becomes a conflict copy or a new item; a new item here and a new one in OneDrive that are the
  same are merged ([sync.md](sync.md) §4).
- **Why.** An editor's save by rename is one item in OneDrive whose content changed: its id,
  version history and sharing links stay. A program that opens the file again finds the same item.
- **Trade-off.** A file made at the place of one removed here, before the removal is sent, is sent
  as a change of the removed one.

### 3.3 The sync plan is one pure function of the rows

- **Decision.** `make_sync_plan` is given a bounded set of rows and a few facts as values (which
  files are open for writing, which steps run, the step memory, the pins, the folder's state of
  sending) and answers the steps that can run now and why every other item waits. No disk,
  network, clock or store is in it ([sync.md](sync.md) §6).
- **Why.** Every cell of its three tables and every case between items is a test with nothing
  around it, and a test of convergence plans and applies random rows until the two sides are equal.
  The plan made after a step sees what the step did; nothing is kept between two plans that could
  go stale.
- **Trade-off.** A plan is made again after every round of steps. An item the plan needs and was
  not given makes it answer "waits", and the caller widens the set and asks again.

### 3.4 Every step is conditional

- **Decision.** A step carries the versions it was planned from and runs outside the lock; its
  commit, in one transaction under the lock, writes only if those are still the row's, and
  otherwise changes nothing and the next plan decides. This holds for every confirmation from
  OneDrive, every step towards the folder, a landing and a free-up ([sync.md](sync.md) §7).
- **Why.** A step runs for seconds or minutes while a program and the listing go on changing the
  rows. A commit that wrote over what changed meanwhile would undo a user's change or OneDrive's.
- **Trade-off.** Work is thrown away when the row changed. What OneDrive confirmed is written
  all the same, in the part it confirmed; what a program did since differs from it and is sent
  again.

### 3.5 What changes OneDrive is on record before it leaves

- **Decision.** Every request that changes OneDrive is written to the store (`request_records`;
  for an upload, its session opening) before it leaves, and goes when its outcome is known:
  confirmed, refused for certain, or read from OneDrive. A record that stands keeps its item and
  the directory it was sent into busy, also across a restart, until it is settled by reading the
  item ([sync.md](sync.md) §7).
- **Why.** An answer lost to a dropped connection or a killed daemon is never a state the plan
  cannot see: the step finds its own request's effect and adopts it, instead of sending it twice or
  taking its result for a change made elsewhere.
- **Trade-off.** One transaction before every request, and one read of OneDrive per record left
  standing; such an item waits until it is settled, after the first listing of a start.

### 3.6 A refusal decides nothing; OneDrive is the judge of names

- **Decision.** A step that OneDrive refuses writes nothing of the item; a name taken (`409`) or an
  item changed (`412`) asks for a listing, and the plan made after it has the case as rows. The
  plan foresees a clash of names by a cautious rule of its own; where OneDrive answers that a name
  is taken all the same, the step memory keeps it and the next plan treats the two as one name
  ([sync.md](sync.md) §6, §7).
- **Why.** OneDrive compares names by a rule it does not publish. Kept as a fact of the step, a
  wrong guess costs one refused request, never a loop and never a loss.
- **Trade-off.** One listing per such refusal. Two exceptions decide from what the step remembers:
  the empty file of an upload session that holds the name (F172), and the step's own earlier
  request whose answer was lost.

### 3.7 The listing writes OneDrive versions only, and an outdated one is read again

- **Decision.** OneDrive's delta listing, page by page, writes only the OneDrive versions of the
  rows, with no tree lock. A confirmed step stamps its row with a count; a page fetched before that
  stamp is taken for the row only if it says what was confirmed, and otherwise the item is read
  again. A full listing sweeps what it did not see at its end; the delta link is written once
  nothing of a listing is left to read again ([sync.md](sync.md) §5).
- **Why.** A listing fetched before an upload committed would otherwise undo the upload, and
  dropped, the change OneDrive made just after it would be lost: the listing never sends it again.
  Page by page, a large first listing fills the folder as it goes and a stopped one resumes.
- **Trade-off.** One `GET` per outdated entry.

### 3.8 Check the account every listing

- **Decision.** Each listing asks which drive the token reaches and compares it with the folder's
  drive, from the store or, while the store has none, from `config.toml`. A mismatch fails the
  listing ([accounts.md](accounts.md) §6).
- **Why.** A sign-out followed by a different sign-in between two listings would defeat a check made
  only at sign-in, and another account's drive would be listed over this folder. Keeping the drive
  in `config.toml` too covers a store made anew.
- **Trade-off.** One `GET /me/drive` per listing.

### 3.9 A lost store is rebuilt from OneDrive and the disk

- **Decision.** With no store, a full listing makes the OneDrive versions again, and the start-up
  check makes the local versions from the backing directory: an entry marked with a OneDrive item id
  that no row has is bound to that id, and taken as clean where its marks, size and time are
  OneDrive's; an unmarked entry is a new item ([mount.md](mount.md) §7).
- **Why.** The downloaded copies carry the id they belong to, so losing the store costs one listing
  and no download.
- **Trade-off.** Lost with the store: the pins, the executable bits, the activity log, the
  conflicts on record, open upload sessions, and every unsent change that left nothing on disk (a
  removal, a move) (D36).

### 3.10 A store of another version is rebuilt, not migrated

- **Decision.** A store of the version just before this build's is brought up in place where the
  change between the two only adds; a store of any other version, a newer daemon's, or a file that
  is no store, is rebuilt as a lost one is, after what the rebuild loses is counted and told
  (`konedrive-tree/src/schema/old_store.rs`).
- **Why.** The store can be made again from OneDrive and the disk. A migration step for every
  layout ever shipped would be code that runs once and can lose data when it is wrong; a change
  that only adds is no reason to lose the pins.
- **Trade-off.** An upgrade over several layouts, or a downgrade, costs a full listing and what a
  lost store costs (D36).

### 3.11 Whole-drive listing, even when only one folder matters

- **Decision.** The daemon lists the drive from its root.
- **Why.** Graph's delta for a personal drive is available only from the drive root.
- **Trade-off.** A drive with much the user never opens is listed whole all the same.

### 3.12 Show nothing that cannot be a local file, and list it

- **Decision.** Names over 255 bytes, the Personal Vault, shared items, OneNote notebooks, names
  with the `.konedrive-` prefix and items the folder cannot show are not shown, and are listed with
  their reason ([sync.md](sync.md) §5).
- **Why.** Each either cannot exist as a local file or does not belong to this drive; a truncated
  or renamed copy would be a different file with a misleading name. A skip nobody can see looks
  like data loss.
- **Trade-off.** Those items are not available here (P7), and a directory holding one cannot be
  removed here (F299).

### 3.13 Take a download's address from the item's metadata

- **Decision.** A download asks Graph for the item's metadata afresh and streams from its
  `@microsoft.graph.downloadUrl`. A program's range reads use an address remembered for 30 minutes,
  asked anew when it stops working (`content_source/graph_source.rs`).
- **Why.** One request gives size, cTag, hash and address together, and fresh metadata on every
  download replaces an expired address. A read of 32 KiB should not cost a request for an address.
- **Trade-off.** None.

### 3.14 A stream is written where it starts, or not at all

- **Decision.** Every content source reports the offset its stream actually starts at, and a
  download or a range refuses a stream that starts anywhere but where it asked.
- **Why.** A server may answer a `Range` request with `200` and the whole body. Written at the
  resume offset, the file's beginning would land in its middle while every byte is accounted for.
- **Trade-off.** None.

## 4. Uploads

These describe a read-write folder ([writes.md](writes.md)). Where Windows' OneDrive client has an
answer a user already knows, it is followed.

### 4.1 Nothing is sent of a file until its last writer closes it, and two seconds after

- **Decision.** An item a program holds open for writing is not planned at all: nothing of it is
  sent, and nothing of OneDrive's is taken over for it. Content changed within the last two
  seconds is not sent yet ([sync.md](sync.md) §6).
- **Why.** The last close is the one moment a file's bytes are all in the backing file, those
  written through a mapping too; and a program that saves in several writes is not sent half-way.
- **Trade-off.** A file a program keeps open for writing for hours is not uploaded for hours.

### 4.2 Small files go up in an upload session

- **Decision.** Every non-empty file is uploaded through an upload session, in fragments of up to
  10 MiB. Only an empty file, which a session cannot carry, uses the plain `PUT`.
- **Why.** Microsoft documents `If-Match` and `conflictBehavior` for the session, and it carries the
  file's time (`fileSystemInfo`). 10 MiB is Microsoft's own boundary for resumable transfers. A
  session survives a stop: the step memory keeps it, and the upload goes on from where OneDrive
  says it got.
- **Trade-off.** Two requests for every small file. The session is not told the file's size, which
  a personal drive refuses, so a full drive shows itself only when a fragment is refused (F39). An
  unfinished session holds its name in OneDrive with an empty file (F172).

### 4.3 Every write is guarded

- **Decision.** `If-Match` on every change of an item OneDrive has, `conflictBehavior=fail` on
  everything new. Nothing is sent without its guard, except a directory's removal (§4.4).
- **Why.** It is the only way two writers cannot overwrite each other, and with the record of every
  request (§3.5) it makes every step safe to try again. A session's guard holds to its end:
  OneDrive refuses its last fragment when the item's content changed since the session was opened,
  and keeps that change.
- **Trade-off.** A refused guard costs a listing before the plan decides (§3.6).

### 4.4 A directory's removal is one unguarded request, sent only when all in it was removed here

- **Decision.** A directory removed here with everything in it is one `DELETE` of the directory,
  with no `If-Match`. It is sent only while the rows say that everything OneDrive has below it was
  shown here and removed here, no listing is under way and no unsettled request is about any of it:
  asked when it is planned, again before the request leaves, and at its commit. `404` is success
  ([writes.md](writes.md) §4).
- **Why.** This is what OneDrive on Windows does, and one request for a tree of thousands of files.
  An unguarded removal takes all OneDrive has in the directory, so it is sent only on that positive
  condition; what the folder never showed is never removed with it.
- **Trade-off.** A directory that holds in OneDrive what the folder cannot show cannot be removed
  here (F299).

### 4.5 A removal is sent as any change is, with no local trash and no confirmation

- **Decision.** A file or a directory tree removed in the folder is removed in OneDrive, into its
  recycle bin, however many files it holds.
- **Why.** OneDrive's recycle bin is the one safety net, on every device; a trash here would be
  uploaded, and a hold that waits for a confirmation depends on a window being open.
- **Trade-off.** What was removed by mistake is got back from OneDrive's recycle bin, on the web
  (F302).

### 4.6 Changed on both sides: keep both, named after the computer

- **Decision.** The version in OneDrive keeps the name; this computer's content is renamed beside
  it to `<name>-<computer>.<ext>`, a name free on both sides, and uploaded as a new file, and the
  pair is listed as a conflict. A file removed here and changed in OneDrive comes back; a file
  removed in OneDrive and changed here is sent again as a new item. Moved on both sides to different
  places, OneDrive's place stands and the folder follows, where it can ([writes.md](writes.md),
  "Conflicts").
- **Why.** It is what Windows does, so the result is what a user expects, and neither side's work is
  lost. The computer's name says where the copy came from.
- **Trade-off.** Copies accumulate until the user merges them; a move made here can be undone by
  one made in OneDrive.

### 4.7 Names OneDrive refuses are listed, not substituted

- **Decision.** An item whose name OneDrive cannot hold (`" * : < > ? \ |`, a space at either end,
  a name that is not UTF-8, a reserved name) is not sent and not altered: it waits, listed under
  "Not Uploaded" with the reason, until the user renames it.
- **Why.** Windows does the same. A substituted name would differ between the folder and OneDrive
  for good, and every other device would see a name nobody chose.
- **Trade-off.** A `:` or `?` in a Linux file name is common, and each such file stays here until
  renamed by hand.

### 4.8 A switch to read-only sends what was unsent first

- **Decision.** A switch to read-only with nothing unsent is made at once. With changes unsent, it
  waits: the account stays read-write, the folder goes on taking and sending changes, and the switch
  is made when the plan finds nothing unsent. Forced, the switch is made at once and what was unsent
  is held here, neither sent nor dropped, until the folder is read-write again
  ([writes.md](writes.md) §2).
- **Why.** A switch must not lose a change the user made, nor leave it unsent without the user
  knowing.
- **Trade-off.** An account asked to turn read-only may stay read-write for as long as something
  cannot be sent; `Account.LastError` says it waits.

## 5. Account and security

### 5.1 Sign-in in the system browser, with PKCE and a loopback redirect

- **Decision.** OAuth authorization code with PKCE, the system browser, a single-use loopback
  listener, personal accounts only.
- **Why.** Microsoft's recommendation for desktop applications (RFC 8252): the password and second
  factor stay in the browser, which keeps the user's Microsoft session, and there is no embedded web
  view to trust.
- **Trade-off.** The consent screen calls the app unverified.

### 5.2 konedrive ships its own client ID, like any desktop client

- **Decision.** konedrive signs in with its own Microsoft Entra application registration, built
  into the daemon (`DEFAULT_CLIENT_ID`). `config.toml`'s `client_id`, set with
  `konedrivectl set-client-id` or `Accounts.SetClientId` while no account is signed in, overrides
  it.
- **Why.** A public client's id is not a secret: it is sent in every sign-in URL. Registering an app
  would be a step with no security purpose between installing konedrive and signing in.
- **Trade-off.** Every install shares one Entra application's rate limits and "unverified
  publisher" consent screen. An override is taken back only by editing `config.toml`.

### 5.3 The daemon owns the token; the refresh token lives only in the Secret Service

- **Decision.** The daemon holds each account's refresh token in KWallet (through the Secret
  Service), with no plaintext fallback, and the access token in memory only. The window never sees
  a token.
- **Why.** The daemon needs the token while the window is closed, and one owner of a secret is
  easier to reason about than two. A plaintext fallback would put the most valuable secret on disk.
- **Trade-off.** No Secret Service, no sign-in. A locked wallet prompts, and other account changes
  wait while it does (A31).

### 5.4 Scope `Files.Read` for a read-only account

- **Decision.** A read-only account asks for `Files.Read User.Read offline_access`, at the
  authorization, the code exchange and every refresh alike, and a new account always signs in so. A
  read-write account asks for `Files.ReadWrite` instead ([accounts.md](accounts.md) §10).
- **Why.** Microsoft refuses any write made with a `Files.Read` token, so "nothing is written to
  OneDrive" is enforced by the server, not by the client's discipline. Microsoft may still answer a
  read-only request with a token that can write, from consent it keeps: the daemon uses it to read
  only, hands it to nobody and says so in `LastError` (F66).
- **Trade-off.** Switching to read-write needs a sign-in of its own.

### 5.5 The mode is the user's choice, and the token decides it

- **Decision.** Every account has a mode, `read-only` or `read-write`, in `config.toml`, a new
  account read-only. `Account.Mode` publishes the mode it runs in: read-write only while
  `config.toml` says so and its last token was granted `Files.ReadWrite` for the account's own
  drive. The switch to read-write signs in again, pinned to the account. While the account may not
  write, all sending of its folder is held, and an upload asks again between fragments
  (`SendingHolds`; [writes.md](writes.md) §2).
- **Why.** Microsoft, not the client, then decides whether a write can happen. Publishing the mode
  run in keeps "read-write" from meaning anything a token cannot do. The pinned sign-in asks for the
  password again with the account's email filled in, so a stray click cannot make another account
  writable.
- **Trade-off.** Once an account is read-write, nothing but the user's own switch stands between a
  change in its folder and OneDrive. A read-write account that loses its grant turns read-only until
  it signs in for it again.

### 5.6 An access-token export for test runs, in development builds only

- **Decision.** `TokenExport.ReadOnly()` and `konedrivectl dev export-access-token` hand out an
  access token (an hour at most), never the refresh token, written atomically to a `0600` file,
  read-only whatever the account's mode. `--read-write` (`TokenExport.ReadWrite()`) hands out one
  that can write, only for an account that runs read-write and whose drive is in
  `write_test_drive_ids`.
- **Why.** A test run needs to speak to Graph without a sign-in of its own, and the refresh token
  must never leave the Secret Service. Only the tests need it, so it is built only with the cargo
  feature `dev-tools`: the released package has neither the interface nor `konedrivectl dev`
  ([packaging.md](packaging.md) §5).
- **Trade-off.** On a development install, any process of the same user on the session bus can
  obtain an hour of read access — no more than it has by opening files in the folder.

### 5.7 An account is its drive

- **Decision.** An account's identity is its Graph drive id, recorded at its first sign-in (or at
  the first answer that names its drive) and not changed after. A sign-in that reaches another drive
  than the account's own is refused, and so is one that reaches a drive another account already
  has. The check and the record are one step under the configuration's lock, and the check is
  fail-closed ([accounts.md](accounts.md) §6).
- **Why.** An account's folder and store belong to one drive: signed in as someone else, it would
  list a different drive over them. Two accounts of one drive would download everything twice.
- **Trade-off.** A sign-in is refused when the daemon cannot ask which drive it reached. Connecting
  a different Microsoft account means adding a new account.

### 5.8 A folder belongs to one account, and remembers which

- **Decision.** Two accounts' folders never nest: a registration that would is refused `Overlaps`,
  naming the other account. A folder's root carries its account's drive (`user.konedrive.drive`),
  and a folder that carries another drive and is not empty is refused (`NotEmpty`).
- **Why.** A folder inside another account's folder would be inside its mount. Without the drive on
  the folder, another account could take over a forgotten folder and list its own drive over the
  first account's files.
- **Trade-off.** A folder that carries no drive can be taken by any account.

### 5.9 Account ids are random; labels are for people

- **Decision.** An account is known by 12 random hexadecimal characters, which name its D-Bus
  object and its directories. People see and type a label, 1 to 40 characters with no `/`, unique
  among the labels, which can change at any time and names nothing on disk. The daemon names a new
  account after its email where it can.
- **Why.** What appears in a D-Bus path and in file names must be valid in both and stable across
  renames. An email address is neither stable in meaning nor valid in a D-Bus path.
- **Trade-off.** `--account` takes an id, a label or an email and refuses a name that fits two
  accounts, which a label equal to another account's email does.

### 5.10 `config.toml` has one owner

- **Decision.** Every change to `config.toml` goes through one store (`ConfigStore`) that re-reads,
  changes and writes the file atomically under one lock, and that never overwrites a file it cannot
  read. A single-account file of version 1 is copied to `config.toml.v1` and rewritten as version 2
  in place ([accounts.md](accounts.md) §4).
- **Why.** With several accounts every change touches the same file, and two independent writers
  could save over each other.
- **Trade-off.** No way back to version 1 but copying `config.toml.v1` back by hand.

### 5.11 Each account's token is a wallet item of its own kind

- **Decision.** Each account's refresh token is stored under `kind=account-refresh-token` and
  `account=<id>`, not under the single-account `kind=refresh-token` with an `account` attribute
  added.
- **Why.** A Secret Service search returns every item whose attributes include the ones asked for:
  a search for the single-account item would find every account's as well, and telling them apart
  would mean reading attributes that a locked wallet may not show.
- **Trade-off.** While an account's version-1 item has not moved yet, both kinds are looked for and
  both are removed at sign-out.

### 5.12 Removing an account keeps the user's files

- **Decision.** Removing an account forgets its folder as a Forget does — the mount taken off and
  the write bits given back —, signs it out, and removes its refresh token, its cached name and
  quota, its store and its directory. The files on disk stay. It is not refused for what is unsent:
  the caller warns first, by `UploadQueue.UnsentCount()` ([accounts.md](accounts.md) §8).
- **Why.** Nothing konedrive does removes a file of the user's on this computer.
- **Trade-off.** The folder keeps only the downloaded files and what was made here; an unsent change
  is lost with the store.

### 5.13 Personal accounts only, for now

- **Decision.** Every account signs in through the `consumers` authority.
- **Why.** Work or school accounts need another authority, an app registration that allows them,
  often an administrator's consent, and testing against SharePoint-backed drives: a phase of its
  own.
- **Trade-off.** No Microsoft 365 or OneDrive for Business accounts. "Open in OneDrive" opens under
  whichever personal account the browser is signed in to (F295).

## 6. The desktop

### 6.1 Notifications come from the app

- **Decision.** The window app, which runs at login in the tray, sends KDE notifications; the daemon
  sends none.
- **Why.** KNotification gives the user KDE's per-event settings in System Settings, and the app is
  the process that belongs to the desktop session.
- **Trade-off.** With the app quit, nothing notifies, and events that happened meanwhile are never
  announced; the window's lists still show everything.

### 6.2 Events have kinds; the app never parses wording

- **Decision.** Every event of the activity log has a kind; a conflict copy is carried as a
  conflict (`Conflicts.List()`, `Conflicts.Count`, `conflict` events), not as a note in
  `LastError`; refusals are D-Bus error names ([desktop.md](desktop.md) §2).
- **Why.** A heuristic on the daemon's wording breaks on any rewording.
- **Trade-off.** One exception: a full disk is recognised by the exact detail "not enough disk
  space".

### 6.3 The window is seven pages, and Conflicts is always one

- **Decision.** The window has seven pages in a sidebar: Status, Activity, Conflicts, Not in the
  Folder, Not Uploaded, Account and Settings. Conflicts is always present, with a count badge while
  there are conflicts, and Not Uploaded likewise ([desktop.md](desktop.md) §4).
- **Why.** A page that appears only sometimes is hard to find and moves the others around; a badge
  says the same thing without that.
- **Trade-off.** None.

### 6.4 An account switcher heads the sidebar

- **Decision.** The window shows one account at a time. A switcher at the top of the sidebar
  chooses it; the six account pages below show that account, and Settings, which holds only what is
  the whole app's, stays one page. The switcher is there with a single account too, and a warning
  sign on it says when another account needs attention.
- **Why.** The pages stay where they are and show the chosen account; KDE's multi-account
  applications, such as NeoChat and Tokodon, put the account selector in the sidebar or the drawer
  in the same way.
- **Trade-off.** Two accounts cannot be seen side by side in the window; the tray is the overview.

### 6.5 The tray shows each account, or the worst one; notifications and Places name the account

- **Decision.** The one tray icon shows the worst state across the accounts, and with several
  accounts its tooltip has a line per account. A setting, on by default, gives each account its own
  icon instead. Notifications and transfer progress name the account once there are several. Every
  Places entry is named `OneDrive — <label>`, with a single account too.
- **Why.** One icon cannot show several states, and trouble must not hide behind an account that is
  fine. A second account renames no Places entry.
- **Trade-off.** In the tooltip's line, an account that needs attention shows why in place of its
  "checked N ago".

### 6.6 The command line names the account, and a path decides it

- **Decision.** `konedrivectl` takes the account from `--account` (id, label or email), else from
  `KONEDRIVE_ACCOUNT`, else the only account there is; with several and none named, a command that
  acts on one stops with exit status 2 and lists them. The commands that take a path find the
  account from the path, and refuse `--account`, as do the commands about no one account. `login`
  with no account at all is refused and names `account add` ([desktop.md](desktop.md) §3).
- **Why.** Guessing among several accounts would act on the wrong one sooner or later, and a
  refusal that lists the labels costs one retry. A path already says whose folder it is in. An
  account is added in one way, by signing in, so `login` adds none.
- **Trade-off.** An email names an account only once the account has signed in, and
  `KONEDRIVE_ACCOUNT` is ignored by the commands that name no chosen account.

### 6.7 Thumbnails from OneDrive, up to 512 px

- **Decision.** The daemon writes OneDrive's own thumbnails into the freedesktop cache at the sizes
  `normal`, `large` and `x-large`, from one `c512x512` request per image; `xx-large` is not filled
  ([desktop.md](desktop.md) §8).
- **Why.** Dolphin draws a correctly tagged cached thumbnail without opening the file, so previews
  cost no download. One request per image serves the three sizes; a 1024 px request would be four
  times the bytes for a size used only at maximum zoom.
- **Trade-off.** At maximum zoom KIO makes its own thumbnail and reads the file; type detection of
  files without a telling extension still reads them (K1).

### 6.8 Keep Baloo out of the folder, and touch only the daemon's own exclusion

- **Decision.** A folder is excluded from Baloo. Whether it is already excluded is read from
  `baloofilerc` directly; the daemon records whether it added the exclusion, and a Forget removes
  only that. Every call to `balooctl6` has a 10 s timeout ([desktop.md](desktop.md) §9).
- **Why.** Baloo reads every file to index it, which would download the whole drive.
  `balooctl6 config list` was observed to print an empty list while the settings file held
  exclusions, so relying on it would re-add, and later remove, exclusions the user had set. A hung
  indexer must not block registration or a Forget.
- **Trade-off.** No desktop search inside the folder; reading `baloofilerc` follows its present
  format (W14).

### 6.9 Download and upload progress as a Plasma job

- **Decision.** A download or an upload still running after 2 s shows in Plasma's notifications as
  a `KJob`, through `KUiServerV2JobTracker`; at most 5 at once for an account and a direction, and
  one more job that stands for the rest ([desktop.md](desktop.md) §7).
- **Why.** It is how Dolphin shows its own copy progress, so it looks and behaves like the rest of
  the desktop; short transfers would only flicker.
- **Trade-off.** The daemon's order of "transfer gone" and "transfer failed" is not fixed, so a job
  is held 1.5 s before it is called a success.

### 6.10 Dolphin: a menu action starts the daemon; the plugins never open a file

- **Decision.** A context-menu action starts a stopped daemon through D-Bus activation; asking what
  the menu may offer does not, so with the daemon stopped the entries are not shown. Neither plugin
  opens a file in the folder: emblems come from `lgetxattr`, which the mount answers from the rows.
  At most 1000 calls wait at once per window, with one `Pin`/`Unpin`/`FreeUp` call per action
  chosen ([desktop.md](desktop.md) §11).
- **Why.** A user clicking "Always Keep on This Device" expects a download, as with any KDE service.
  An open inside Dolphin would read whatever is shown. The cap bounds Dolphin's memory and its
  D-Bus reply budget against a daemon that never answers.
- **Trade-off.** A selection of more than 1000 paths takes several clicks.

### 6.11 Pinning: unchecking only unpins; "Free up space" works on any folder

- **Decision.** Unchecking "Always Keep on This Device" only removes the pin: files already
  downloaded stay downloaded, as on Windows. It calls the daemon's `Unpin`, never `FreeUp`. "Free Up
  Space" is offered for any folder inside the root, not only a pinned or downloaded one, again as on
  Windows ([pinning.md](pinning.md)).
- **Why.** It matches what a Windows user expects of "Always Keep on This Device", and keeps the two
  actions' jobs apart: one manages the pin, the other frees space.
- **Trade-off.** Unpin is refused as one call for the whole selection if any path in it is kept
  pinned by a folder above it that is not unpinned in the same call; the checkbox is then shown
  locked. The daemon says so (`Files.Menu`), and the plugin shows it.

## 7. Testing

### 7.1 Every part is tested without a mount

- **Decision.** Every part of the mount, the steps and the plan's task is tested over one fixture
  (`mount/item_fixture.rs`): a store in a file, a backing directory under no mount, the mount's
  parts; what talks to OneDrive is tested against the fake OneDrive behind wiremock. A decision is
  a function of typed cases with a test per case ([sync.md](sync.md) §10).
- **Why.** Code that can be tested only through the whole daemon and a kernel is tested rarely and
  slowly; a part that owns its state and decides by a pure function is tested by itself, in
  milliseconds, on any machine.
- **Trade-off.** What only the kernel decides (what it caches, what it asks before an append) is
  seen only in the VM suite (§7.3).

### 7.2 The plan is tested for convergence, and the steps against races

- **Decision.** Beside a test per cell and per case, a test of convergence changes rows at random on
  both sides, plans and applies until no step is left, and asks that the two sides are equal and
  nothing was lost; the steps towards the folder run against a program's writes, a listing's pages
  and deaths, in orders made from a seed ([sync.md](sync.md) §10).
- **Why.** The rules of order between disk, row and request are of the kind only many orders find;
  a seed makes a failure repeatable.
- **Trade-off.** These tests run longer than the rest of the workspace.

### 7.3 A mounted folder is tested only in a VM

- **Decision.** Everything that needs a mounted folder runs in a virtme-ng VM booted on the host's
  own kernel (`tests/fuse-probe/run.sh`), on ext4, against the fake OneDrive, with the daemon and
  every program as an ordinary user. Nothing is mounted on the host, and no step needs `sudo`.
- **Why.** A stalled or crashing mount stays inside the VM, and the host's session, its bus and the
  user's files are never touched.
- **Trade-off.** One filesystem is covered; the suite runs once per finished piece that touches the
  mount or the sync, not per change.

### 7.4 Writes to OneDrive itself: only a test account, through a guard at the wire

- **Decision.** Uploads are tested against the fake OneDrive everywhere, and against OneDrive itself
  only in one harness (`tests/write-account/`), only for a separate test account. It refuses to
  start unless the drive both tokens reach is the one named and is in `write_test_drive_ids`, and
  looks like a test account (under 1 GiB used, under 1000 items; `--large-test-drive` lifts the two
  sizes). Every request to Graph, the daemon's own included, goes through a proxy whose guard admits
  a write only inside the run's own folder and within fixed caps, and ends the checks at its first
  refusal ([writes.md](writes.md) §8).
- **Why.** Some of what uploads rely on is what the service does, which no fake can say. A guard at
  the wire sees every request whatever code made it, so a bug in a check cannot write elsewhere, and
  the look of the drive refuses a real account even if its id were listed by mistake.
- **Trade-off.** It runs by hand, with exported tokens.

# The mount

How a folder is served: the mount and its parts, the backing directory and the order in which the
disk and the rows change, what each request of the kernel does, how a file that is not on disk is
read and downloaded, how space is freed up, the start-up check, the end of a mount, and how
whoever watches the folder is told of a change the daemon made. The words are those of
[README.md](README.md), "The words".

## 1. Scope

This document covers `crates/konedrived/src/mount/` (its `mod.rs` lists the parts), the runner of
the steps towards the folder in `crates/konedrived/src/folder_steps/` as far as the disk goes,
the content sources in `crates/konedrived/src/content_source/`, and the download in
`crates/konedrive-graph/src/drive/` and `pool.rs`.

It does not cover, and points to:

- the item, its three versions, the rules that hold of the rows, and who writes what:
  [sync.md](sync.md) §2–§4;
- what the sync plan decides, and what a step is and remembers: [sync.md](sync.md) §6–§7;
- the order of a folder's start and stop, and the pause after a take-down: [sync.md](sync.md) §9;
- uploads, the access mode from the user's side, conflicts: [writes.md](writes.md);
- what a pin downloads and keeps: [pinning.md](pinning.md);
- the bus, `konedrivectl`, the window and the Dolphin plugins: [desktop.md](desktop.md);
- choosing a folder and the probe of its filesystem: [accounts.md](accounts.md).

## 2. The mount and its parts

### 2.1 `ItemMount`

`ItemMount` (`mount/item_mount.rs`) holds the parts of one folder's mount and has no behaviour of
its own. Each part owns its state; what is decided is a pure function of typed cases inside the
part, tested without a mount. The parts are made in one place, in one order
(`item_mount/build_order.rs`): a stale mount taken off (§8.3), the backing directory opened, its
write bits given back (§8.4), the access mode switch (read-only), the take-down, the
write-operation thread, the place changes, **the start-up check** (§7), the entries still set
aside removed, and then everything that reads the rows.

`ItemMount::build` answers a mount that is not over the folder yet; `ItemMount::serve` puts it
there with `fuser`: file system name `konedrive`, `nosuid`, `nodev`, no `allow_other`, four
threads, each on a connection of its own to the kernel. At its start the mount asks the kernel for
`READDIRPLUS`, parallel directory operations, atomic `O_TRUNC` and a read-ahead of 1 MiB. Before
the mount goes over the folder, a second descriptor of the backing directory is handed to the rest
of the daemon (`folder::root::mounted_over`): from then on the folder's path is the mount, and the
daemon reaches the folder on disk only by descriptor. The tests of the daemon use a mount that was
built and not served; only the VM suite mounts.

| Part | State it owns | Changed by |
|---|---|---|
| `BackingDirectory` | the descriptor of the folder on disk | nobody: opened once |
| `PlaceChanges` | the count of place changes (§3.2) | the write-operation thread |
| `AccessModeSwitch` | read-only, read-write, or read-only for good | the folder's start, a change of the access mode, `MountTakeDown` |
| `WriteOperationThread` | the jobs that wait for its thread | anyone queues; the thread runs them |
| `ReadConnections` | one read-only connection to the store per thread of the mount | opened at a thread's first question, closed at the stop |
| `ItemHandles` | what programs hold open, by the kernel's `fh`; which items have a writer | open, create, release, opendir, releasedir |
| `ItemDownloadStates` | per item and content version: the running download and its waiters, a failed one's unfinished file, the fact that OneDrive stated another content; the bytes read as ranges | the downloader and the reads |
| `RangeReaders` | a slot of the transfer pool per file being read as ranges | the range fetcher |
| `ItemInotifyBridge` | the queue of announcements and its thread (§9) | the steps, the downloader, the free-up, a pin |
| `MountTakeDown` | whether a take-down was asked, and its first cause | the write operations, the steps, the landing |
| `InterruptibleReads` | the reads that wait for OneDrive, by the kernel's request number | the kernel's interface; the stop |

The other parts hold parts and decide: `ItemLookup` (the reading requests), `ItemWriteOperations`
(a program's changes), `ItemDownloader` and `ItemDownloadLanding`, `ItemRangeFetcher`,
`ItemFreeUp`, `ItemContentFiles` (the local content as a step towards OneDrive reads it),
`ItemFilesInterface` (the bus interface `Files`), `UploadRules` (what is never uploaded), and
`ItemKernelRequests`, one function per request, under the `fuser::Filesystem` of
`kernel_requests.rs`. The folder's sync takes from the mount the parts its steps towards the folder
are made of (`FolderStepParts`), the local contents, the open handles, the `Files` interface, the
findings of the start-up check and the access mode switch.

### 2.2 The threads

- **The mount's four threads** answer every reading request, read backing files, and take the
  writes and fsyncs of programs. Each reads the rows through its own connection, an answer in one
  read transaction, so a row and its path are of one moment; none waits for a page of the
  listing, the commit of a step or the tree lock.
- **The runtime** does what waits for OneDrive: a range, a download, the download before a
  write. The mount's thread reads every row it needs first and hands on values, so no worker of
  the runtime reads the rows through a mount's connection, and no thread of the mount is held
  for a fetch.
- **The write-operation thread** makes every change of the backing directory and its row (§2.3).
- **The bridge's thread** performs the announcements (§9).
- **The store's thread** writes every row (`Store::call_blocking`).

### 2.3 The write-operation thread and the tree lock

A change a program asks for — make, remove, rename, an open for writing, a truncation, a time, the
executable bit, a writer's release — is one job of one thread (`write_operation_thread.rs`), and
so is every step towards the folder, a landing, a free-up, the switch to read-only and the go-over
of ignored names. The jobs run one after another, each under the folder's tree lock, which the
commit of a step towards OneDrive takes too. The kernel's reply is moved into the job: a program
hears its change was made only once its row is written, and no two changes of names interleave. A
stopped thread runs nothing more: the job in flight ends, and the queued ones are dropped with
their replies, which answer the kernel an error.

A cycle of the listing takes no tree lock ([sync.md](sync.md) §5). What it may write between a
step's disk change and its commit is answered by the commit's condition: a commit that finds the
row changed writes nothing and undoes the disk ([sync.md](sync.md) §7).

The locks are taken in one order, never the other way (`place_changes.rs` has it in full): the
folder's lease (the plan's task, around a step towards the folder); the tree lock; the place
changes' own lock; one job of the store or one read transaction; the leaf mutexes, each held for
a few lines.

### 2.4 The access mode switch

`AccessModeSwitch` holds one value. A mount starts read-only, and the folder's start sets the
account's mode before any task plans ([sync.md](sync.md) §9). A read-only folder shows no write
bit and refuses every change with `EROFS`. A request that changes the folder asks the mode twice:
on the mount's thread before it queues a job, so that a program is told `EROFS` without waiting
for the tree lock; and in the job, whose answer counts. The switch to read-only is itself a job
of the thread, so no operation is half done across it; the switch to read-write is made at once,
since an operation begun in a read-only folder was refused at its start. The last close of a file
is written whatever the mode is by then. A mount that asked for its take-down is read-only for
good (§8.1). What the switch means for uploads is in [writes.md](writes.md).

## 3. The backing directory

### 3.1 What it holds

The backing directory is the folder on disk (`BackingDirectory`, `mount/backing_directory.rs`),
reached through the descriptor opened before the mount. It holds the downloaded copies, the files
and directories made here, and the directories of OneDrive that hold something on disk; a
directory of OneDrive with nothing on disk need not exist there, and is made, with its mark, when
the first content comes. A download is built in a file with no name (`O_TMPFILE`) on the backing
directory's filesystem: no listing ever shows a file that is not whole, and one the daemon leaves
behind is removed by the kernel.

The daemon's own names are hidden by the mount and refused to programs (`folder/disk.rs`): an
entry set aside (`.konedrive-aside-<local item id>`, in the root), the temporary name of an
exchange of two names (`.konedrive-swap-…`, a row's place), and `.konedrive-holding` and
`.konedrive-new-…`. A folder that holds placeholders of an older konedrive — a file marked with a
state that is not whole, or a `.konedrive-holding` directory — is not mounted
(`ItemMountError::LegacyPlaceholders`).

### 3.2 A row finds its file by the path of its local version

No file handle is stored. While the mount is up nothing but the daemon reaches the backing
directory, so the daemon keeps it in step with the local versions, and a row's file is at the path
of its local version.

For names the disk changes before the row (§3.3), so a reader that reads a row, takes its path and
opens it could meet a rename: a missing file, or after two renames another item's file.
`PlaceChanges` keeps a count that is odd from the begin of a change of names to its end. Every open
of a backing file by a row's path outside the write-operation thread goes through
`PlaceChanges::open_at_row_path`: it waits while the count is odd, reads the row and its path in
one read transaction, opens, and reads the count again; when it moved, what was opened is dropped
and the whole is tried again. After three crossed tries the reader holds the place changes' lock
for its last try, which no change can begin under. A stat at a row's path
(`stat_at_row_path`, for the blocks a file takes) never sleeps: it tries twice and then answers
without facts, and the attributes of that one answer have no blocks.

A change of names begins only on the write-operation thread, and `open_at_row_path` never runs
there; both are asserted in debug builds.

### 3.3 The order of disk and row

The order serves one rule: **a row never says a copy is clean while the disk may hold the user's
bytes, and never says content is on disk that the disk does not hold for good.**

| What | Order |
|---|---|
| A program changes a file's content | the row first: the first writer's open writes `OpenedForWriting` — the content is changed here, and the content of before the write is kept in the row; then bytes go to the backing file with no row touched per write; the last close writes `ClosedChanged` with the size and times of the backing file, or `ClosedUnchanged`, which puts the former content back; only then the writer leaves the handles |
| A backing file made for a writer: an open for writing of an empty file not on disk, an open with `O_TRUNC`, a truncation to nothing | the record, the place change begun, the row, the file made, the change ended, the record forgotten: a reader that comes between the row and the file waits |
| Make, rename, remove, by a program or a step | the record, the place change begun, the disk, the row (which takes the record in its own transaction), the change ended, what was set aside removed for good |
| A landing | the file's data, time and marks synced with no lock held; then, as one job of the thread: the record, the link, its directory synced, the row says on disk (taking the record) |
| A free-up | the row says not on disk first; then the copy is set aside and removed |

A row the store refuses is undone on disk, its record is taken by itself, and the program gets the
errno of the refusal. An undo that fails leaves the record standing and asks for the mount's
take-down (§8.1); the next start's check settles the change from the record.

**Nothing is replaced.** Every move is `RENAME_NOREPLACE`; a place the rows say is free and the
disk holds is `EEXIST` to a program and an obstacle to a step. A removal, and a move over another
entry, first set that entry aside: it is renamed to its item's aside name in the root, and removed
for good only after the row is written and the change ended. A directory that holds what no row
says is not set aside: it stays where it is, without its mark. Leftovers of a death are put back by
the start-up check, and only after the check are the entries still set aside removed
(`PlaceChanges::remove_aside_entries`): before it, the entry a record puts back would be deleted.

A step towards the folder keeps the same order; `folder_steps/mod.rs` has the record, the disk and
the commit of each case. Taking OneDrive's content for a downloaded copy sets the old copy aside
before its commit, unlike a free-up: its copy is of the old content, and a death after a commit
that came first would leave it under a row with the new content, where the check would take it for
content put there by the user.

### 3.4 A name's change is on record before the disk

Before a change of names touches the disk, what it is about to do is written to the store, in a
transaction of its own: the table `place_change_records`
(`crates/konedrive-tree/src/item_store/place_change_records.rs`). A record is one case of
`PlaceChangeRecord` — an entry made, moved, moved over another, set aside, a landed file linked —
in the rows' own terms: an item by its local item id, a place by its directory and name. It goes in
the transaction that writes the row of the change, or by itself when the change was undone. A
record does not say whose change it was, a program's or a step's.

A daemon that dies between the record and the row leaves the record, and the start-up check
finishes or undoes exactly that change from it (§7.1), with no guessing: the entry is at one of
the places the record names. The marks are not used for this.

### 3.5 The marks

The backing directory carries extended attributes, the marks:

| Mark | On | Says |
|---|---|---|
| `user.konedrive.root`, `user.konedrive.drive` | the folder itself | the folder's id and its account's drive ([accounts.md](accounts.md)) |
| `user.konedrive.item-id` | a downloaded copy, a directory of OneDrive, a file sent, a file made here for an item of OneDrive | which item of OneDrive the entry belongs to |
| `user.konedrive.state` = `hydrated` | a downloaded copy, a file sent | the file holds that item's content whole |
| `user.konedrive.ctag` | a downloaded copy, a file sent | which content of it |
| `user.konedrive.progress` | the nameless file of an unfinished download | the cTag and the bytes from the start that are on disk (§5.3) |

They are written by a landing (`write_whole_copy_marks`, the item id last), by the commit of an
upload (`ItemContentFiles`), by the directories made above a place, and by the start-up check. A
file made for an overwrite or a truncation of an item of OneDrive carries the item id alone: it
holds the user's content, not OneDrive's. No program can write a mark: the mount refuses
`setxattr` (§4.2). No request of the kernel reads them. They serve the start-up check and a lost
store (§7), and without the cTag a clean copy of an older content could not be told from an edit.

The attributes a program reads through the mount (`user.konedrive.item-id`, `.state`, `.pin`,
`.sync`) are not these marks: the mount answers them from the row and the plan
(`item_lookup.rs`), lists none of them, and writes nothing on disk. Dolphin's emblems read them
([desktop.md](desktop.md)).

## 4. What a request of the kernel does

### 4.1 The requests

A node id is the item's local item id, from the first line on (`item_attributes.rs`). Every name,
place and attribute answered comes from the item's local version, with the three exceptions of
[sync.md](sync.md) §3. `item_kernel_requests.rs` has the table in full, with the function each
request calls.

| Request | Thread | What it does | Writes |
|---|---|---|---|
| lookup, getattr | mount | one row by directory and name, or by local item id; the blocks from a stat of the backing file | — |
| opendir, readdir, readdirplus | mount | the directory's rows, listed once at offset 0 and kept with the handle; later parts read the kept listing; each item of readdirplus is described by its own row | — |
| getxattr, listxattr | mount | the attributes of §3.5's last paragraph | — |
| open to read | mount | content on disk: the backing file, through `open_at_row_path`; not on disk: a handle that holds the item and the content it was opened as; nothing is downloaded | — |
| read | mount, then runtime | §5 | — |
| open to write | mount, runtime, write | a file not on disk and not empty is downloaded whole first (§5.3), then the open is a job | `OpenedForWriting` for the first writer |
| open with `O_TRUNC` | mount, write | the backing file made empty; nothing is downloaded | `OpenedForWriting` |
| create, mknod, mkdir | mount, write | the entry made; a created file made and opened in one transaction | `Made` |
| unlink, rmdir | mount, write | the entry set aside, the row, the entry removed | `Removed` |
| rename | mount, write | the entry moved, over another one set aside | `Moved` or `SavedOver` |
| setattr with a size | mount, runtime, write | a file not on disk is downloaded first, unless cut to nothing; a file open for writing is cut with no row | `Truncated` |
| setattr with a time, with a mode | mount, write | the time into the row and the backing file; of a mode the owner's executable bit alone ([F303](../limitations/F303.md)) | the time; the executable bit |
| write, fsync, flush | mount | into the backing file of the writer's handle | none per write |
| fsyncdir | mount | the directory's backing directory synced, then the store's write-ahead log, for the names only the rows hold | — |
| release | mount, write | a reader's handle closed at once; a writer's last close is a job | `ClosedChanged` or `ClosedUnchanged` |

The attributes are made by one code for getattr and for a write operation's reply
(`item_attributes::to_item_attributes`): the content's size and time for a file, the backing
file's size and times while a program holds it open for writing, the blocks of the backing file,
0644 or 0755 and no write bit in a read-only folder, and for a directory the time of its backing
directory, or the time the mount started when it has none on disk. The kernel keeps attributes
and names for one second.

**One content through a handle.** A handle opened on a file that is not on disk reads the content
it was opened as, or nothing (§5.5). A node whose row is gone or shows nothing is `ESTALE`; a
request that carries the handle of a file removed or saved over while a program held it is
answered from the handle's own file (getattr, a truncation).

### 4.2 The requests answered as a local folder answers them

| Request | Answer |
|---|---|
| statfs | the space of the backing directory's filesystem; names up to 255 bytes |
| access | `EROFS` for a write in a read-only folder; allowed otherwise |
| ioctl | `ENOTTY` |
| poll | `ENOSYS`: the kernel answers every poll of the mount as always ready |
| fallocate | `ENOSYS`: the kernel answers `EOPNOTSUPP`, and `posix_fallocate` writes the range |
| copy_file_range | `ENOSYS`: the kernel copies by reads and writes through the mount |
| lseek | `ENOSYS`: the kernel seeks by itself, as for a file with no holes |
| symlink, link, mknod of anything but a regular file | `EPERM` ([F300](../limitations/F300.md)) |
| setxattr, removexattr | `ENOTSUP`, for every name, the daemon's own among them ([F300](../limitations/F300.md)) |
| rename with `RENAME_EXCHANGE` | `EINVAL` |
| a daemon's own name (§3.1) | hidden: lookup `ENOENT`; make and rename to it `EPERM` |
| a file manager's trash directory (`.Trash…`) in the root | mkdir `EPERM`: what is removed goes to OneDrive's recycle bin |

In a read-only folder every request that would change it is `EROFS`, before anything else is
asked.

### 4.3 Errnos

| A program gets | When |
|---|---|
| `EROFS` | a change in a read-only folder, or in one whose mount asked for its take-down |
| `ESTALE` | the row is gone or shows nothing; the content a handle was opened as is not the row's any more; OneDrive stated another content (§5.5) |
| `EEXIST` | the place is taken in the rows, or held on disk by what no row says, or the name in OneDrive by an item the folder cannot show, as OneDrive compares names ([F304](../limitations/F304.md)) |
| `ENOTEMPTY`, `ENOENT` | the store's refusal of a removal or a make ([F299](../limitations/F299.md) for a directory that holds in OneDrive what it does not show) |
| `EAGAIN` | OneDrive asks the account to wait, and the content is needed now (§5.6) |
| `ENOSPC`, `EDQUOT` | a download or a write ran out of space |
| `EIO` | the store failed; a download failed after its tries; a row against the rules |
| `EBADF` | a handle that holds no file, or may not write |

A download's own failure is clamped to `EPERM`, `EIO`, `EAGAIN`, `EBUSY`, `ETXTBSY`, `ENOSPC` or
`EDQUOT`; anything else the local filesystem reports becomes `EIO` (`guards.rs`).

## 5. Reading a file that is not on disk

### 5.1 Ranges of the content opened

A read through a handle opened not on disk is decided on the mount's thread
(`ItemKernelRequests::decide_read`, `item_read_origin.rs`), against the row as it is now:

- the content landed since the open, or is kept in the row from before a writer's open: the
  handle goes on from the backing file, opened through `open_at_row_path`;
- a download of that very item and content runs and has written its file far enough (1 MiB beyond
  the bytes asked for): the bytes come from its file;
- otherwise the range is fetched from OneDrive on the runtime, by the item's binding, and the
  kernel's interrupt aborts it.

A range asks the content source for exactly those bytes (`ItemRangeFetcher::fetch_range`). Over
Graph it is a `Range` request to the item's download address, which is kept for 30 minutes and
asked again once when it does not serve (`graph_source.rs`). An answer that starts elsewhere than
asked, or ends early, fails the read with `EIO`. While a file is read as ranges it holds one slot
of the transfer pool as a file being opened does, until two seconds after its last read
(`range_readers.rs`): what a program reads goes before the background downloads.

### 5.2 The threshold

The bytes read as ranges are counted per item. Once they reach `[transfers] download_after`
(1 MiB by default; 0 downloads at the first read), or the whole of a smaller file, the file is
downloaded whole, with no waiter: the reads go on as ranges and from the download's file as it is
written, and from the backing file once it lands. A program that looks only at the start of a
file, a file manager finding its type, downloads nothing more.

### 5.3 The whole download

`ItemDownloader` downloads a file by `ItemToDownload`: the item, its binding and the content to
download. Who asks — a read at the threshold, an open for writing or a truncation, `Files.Hydrate`,
the plan's step for a pinned file — is a `DownloadAsker`. A download of the same item and content
that runs is joined, whoever asks; a download of another content of the same item is another
download, and the older one's landing finds the row changed and drops its file.

One download, in its slot of the transfer pool (§5.6):

1. **Fetched** into a nameless file, or into the unfinished file of an earlier try of that item
   and content, which the download states keep for the latest 16 that failed. Each fetch asks Graph
   for the item afresh — size, cTag, quickXorHash and a download address — and streams from that
   address with a `Range` from where the bytes stop (`content_source/graph_source.rs`,
   `konedrive-graph/src/drive/mod.rs`). A `200` to a `Range` request has its first bytes skipped,
   so the stream starts where it says; a `416` is an empty stream; a `401`, `403`, `404` or `410`
   from the address is an expired one, replaced once.
2. **Held against the asked content.** Every answer — the first, one after a break, each piece of
   a download in parts — is held against the content asked for before a byte of it is written
   (`item_downloader/download_source.rs`): one of another content ends the download (§5.5). An
   answer that says no size is refused; a size of 0 said against a content that has bytes is
   another content.
3. **Checked** (`content_source/source/download.rs`, `guards.rs`). The quickXorHash is computed
   over the bytes as they are written, and a content OneDrive gives a hash for is never landed
   unless it matches; a mismatch starts the download over once, and a second is `EIO`. A file
   OneDrive gives without a hash is landed unverified, with a warning ([F34](../limitations/F34.md)).
   A read error or a short stream is a break: the next fetch asks from where the bytes stopped,
   after 200 ms and then 400 ms; the third break ends the download.
4. **Checkpointed**: every 16 MiB of a content with a hash, the bytes are synced and
   `user.konedrive.progress` written on the file. An unfinished file is continued only for the same
   cTag, after its bytes are read back into the hash. The file has no name, so nothing of it
   outlives the daemon.
5. **Given its time and the marks of a whole copy, and synced**, by the downloader with no lock
   held: the landing's job waits for no file to reach the disk.
6. **Landed** (§5.4), then announced (§9), and its waiters answered.

**A large file in parallel parts.** A download that nothing waits to open (a pinned file) of
100 MiB and up goes in pieces of 256 MiB (`content_source/source/parts.rs`). Its first stream runs
in the slot the download took; extra streams each take a free large slot of the pool, only while
nothing waits for a slot, and give it back after a piece when something does, or when another file
in parts has two streams fewer. Each piece is hashed at its offset and the pieces combine into the
whole file's quickXorHash. The checkpoint counts the bytes from the start with no gap. Three breaks
of one piece fail the download.

### 5.4 Landing

The landing (`item_download_landing.rs`) is one job of the write-operation thread. It reads the row
and its path, writes the record (`LinkFile`, with the content that was downloaded), begins a place
change, links the file under the name the row has by then — following a rename made meanwhile —
with the directories above made and marked, syncs each directory a name was made in, and commits
conditionally: for a program, the item's local content must still be the one downloaded
(`commit_landed_content`); for the plan's step, every version it was planned from
(`commit_download_content`). The record goes in the commit.

| The landing finds | What happens | The waiter hears |
|---|---|---|
| the row as downloaded, not on disk | linked; the row says on disk | landed |
| that content on disk already (a program's download) | nothing linked | landed |
| the row changed, the item gone or not shown any more | nothing linked; or, when the commit finds it, the named file removed again and the record forgotten | `ESTALE` |
| something at the name though the row says not on disk | nothing replaced | `EEXIST` |
| the named file cannot be removed again | the record stands; take-down (§8.1) | the errno |

### 5.5 Another content

A program never reads the bytes of one content at the size of another. How a read learns that
OneDrive has another content than the one it was opened as:

- **A listing's step** changes the row's local content: every handle opened as the old content is
  `ESTALE` from then on, and the next open reads the new one.
- **A download's answer** states another content: the download ends at that first answer, before a
  byte is written; its waiters hear `ESTALE`; nothing lands; and the download states remember the
  fact for that item and content (the latest 256). It answers nothing by itself: a read of a
  remembered content asks for the whole download anew, or joins the one that runs and has no
  answer yet, and waits for it. The fact is void at the first answer that download accepts. Stated
  another content again: `ESTALE`, remembered again. Stated the asked one: it lands, and the read
  gets its range. A file OneDrive does not have, and an answer that says no size, are failures of
  that one asking and are not remembered.
- **A range** cannot tell: Graph's answer to a range names no version, so another content is
  told only when OneDrive no longer has the file ([F297](../limitations/F297.md)).

### 5.6 The transfer pool, a throttle, a pause

Every transfer of an account takes a slot of its transfer pool (`konedrive-graph/src/pool.rs`):
downloads, ranges, uploads and metadata requests. The pool starts at 16 slots and grows by one for
each transfer that succeeds while work waits and every slot is busy, up to `[transfers] max` (32 by
default, 1 to 256). A `429` or `503` halves it, once per burst, and no slot is handed out for the
whole `Retry-After` (10 s when none is given); for five minutes after that wait it grows by one
slot per round only. A free slot goes first to a file being opened, which may take two slots
above the pool, and while any open waits or runs nothing else takes a new slot; then metadata;
then background downloads and uploads in turn. The classes are `Class::Open` (a read, an open for
writing, `Files.Hydrate`), `Metadata`, `Download` (the plan's pinned files) and `Upload`. At most
`[transfers] large` (4 by default) streams of large sync transfers, 100 MiB and up, run at once; a
file being opened is never counted large. Every number is chosen, not measured against a real
account.

**A throttle.** While OneDrive asks the account to wait (`status::throttle_wait`), what a program
waits for fails at once with `EAGAIN` and fetches nothing: an open to read a file not on disk, a
range, the download before a write or a truncation, `Files.Hydrate`. A download that runs when the
wait starts is given up where it waits for its slot or for OneDrive's answer, and keeps its
unfinished file; one whose bytes are coming is read to its end and lands. The plan's download of a
pinned file nobody else asked for waits the time out. The first file refused during a wait is told
once, and a refusal is no failure in the activity log.

**A pause** holds back everything but opens. A download the plan started that still waits for its
slot when a program asks for it gives that wait up and takes an open's slot: a program is never
kept waiting for a pause. Pausing is described in [writes.md](writes.md).

## 6. Freeing up space

Freeing up removes a file's downloaded copy; the file stays in the folder, not on disk
(`item_free_up.rs`). Each file is freed as one job of the write-operation thread, by the first case
of `FreeUpDecision` that holds:

| The file | Free-up |
|---|---|
| a pin holds for it ([pinning.md](pinning.md)) | left |
| changed here, or never in OneDrive | left: its copy is the only one |
| open for writing | left |
| being downloaded, or a step runs for it | left |
| the local content is the synced one | freed |
| not on disk, or a directory | nothing to free |

The row says not on disk first (`commit_freed_content`, conditional on the content decided on);
then the copy is set aside and removed, and the change announced (§9). A copy that cannot be
removed is said on disk in its row again. A daemon that dies between the two leaves a whole copy
where the row says not on disk, which the start-up check takes for a copy that landed before its
row (§7.2): nothing is lost. A program that reads the file through a handle keeps reading the
removed copy; the next open reads from OneDrive.

A directory is freed file by file, a part of its rows at a time, each file in its own job, so that
a program's change waits for one file of a long free-up. `Files.FreeUp` and `Folder.FreeUpSpace`
are the bus's ways in ([desktop.md](desktop.md)); `konedrivectl sync dehydrate`, `sync free` and
`sync free-up-space` are the command line's.

**A copy that is missing.** An open that finds nothing where its row says the content is on disk
(at a moment no place change crossed) has the row mended when it is a clean copy: written not on
disk, and the open reads from OneDrive. A row whose content was changed here is left for the next
start-up check.

## 7. The start-up check

### 7.1 One place at a time

The start-up check (`backing_directory_check.rs`, called by `start_check.rs`) runs in the mount's
build, before the mount goes over the folder and before any part reads the rows; nothing else reads
or writes the rows or the backing directory meanwhile. It is the one place where the disk is read
to learn what the folder contains. **It does not guess what a person did while no daemon ran**: it
looks at one place at a time, what the rows say of it and what the disk has there, and nothing is
matched by a mark across places ([F298](../limitations/F298.md)).

1. **The records first** (`standing_place_changes.rs`, decided by
   `place_change_settlement::decide_settlement`). A row that was written is not undone. A made entry
   of a new item gets its row. A move or a set-aside is undone on disk, so the rows stand as they
   were and the plan or the person does it again; no row is moved or removed by a record. The
   backing file of a writer's open that was never made puts the row back as it was before the
   open. A landed file whose row still has the downloaded content is on disk; one whose row has
   another content by now is removed. An undo that cannot be done (the directory is gone, the name
   is taken, the entry at the new place is not the one moved) is not tried again: the record goes
   and the check decides the places as it finds them. A record stands only where a place it names
   cannot be looked at.
2. **The rows**, read whole once (`item_rows.rs`).
3. **The backing directory**, read once, a level of directories at a time on up to eight threads,
   to 128 levels, with the marks of each entry (`backing_directory_contents.rs`).
4. **The walk** over every place, a directory before what is in it (`place_walk.rs`): each place
   decided by `disk_finding::decide_finding`, a function with no disk and no store in it, and
   written through `row_writer.rs`. A row that leaves the folder leaves first, with what is below
   it; then the entry at that place, if no row has it now, becomes a new item.

### 7.2 The cases

A file's content is compared by its size, its time in whole seconds, and its cTag mark where the
row has a cTag (`compare_content`).

| The row says, at that place | The disk has there | Finding |
|---|---|---|
| a file or a directory | an entry of that kind | the item; a file's content the same (`NothingChanged`) or changed here (`ContentChanged`) |
| a file a writer held open when the daemon stopped | a file | changed here, whatever its size and time say: bytes may have been written that neither shows |
| a file not on disk | a whole copy marked with the row's item id, of the row's content | the copy, landed before its row (`CopyLandedBeforeRow`) |
| a file not on disk | any other file | content put there: changed here |
| a file of OneDrive not on disk, or a directory of OneDrive | nothing | nothing changed: neither needs an entry |
| a file of OneDrive on disk | nothing | not shown any more (below) |
| an item of OneDrive | an entry of the other kind | not shown any more (`NotShownAnyMore`): its local version and its synced version are taken away, its binding and its OneDrive version stay, and the plan shows it again as OneDrive has it. **Nothing is removed in OneDrive**, also when the copy was changed here and not sent |
| an item OneDrive never had | nothing, or an entry of the other kind | the row goes (`Removed`) |
| nothing | an entry marked with an item id no row has, which no other entry carries | a new item bound to that id (`MarkedWithoutRow`) |
| nothing | any other entry | a new item (`Made`) |

The check writes marks for two things only (`mark_change.rs`): the item id on the own entry of an
item of OneDrive, and a mark taken off an entry that becomes a new item of the user's own. A file
with several names is left as its marks are: the marks are the file's, and another name of it may
be another item's.

### 7.3 What cannot be read

A part of the backing directory that cannot be read — a directory that cannot be listed, one
deeper than 128 levels, a name that is neither a file nor a directory, marks that cannot be read,
a record that stands — is left as it is: no row in it or below it is changed. Those rows are held
(`DiskFindings::list_held_items`): nothing of them, or of what is below them, is sent for as long
as the mount of this start is up, and the queue on the bus shows them as "could not be read on
disk" ([writes.md](writes.md)). Each path is said in the journal with its reason. No other row, and
not the folder, is held for them.

### 7.4 A lost store

A store that is lost, or of another layout, is made again ([D36](../limitations/D36.md);
[sync.md](sync.md) §9). The check then finds every entry with no row. An entry marked with an item
id that no other entry carries is bound to that id; a whole copy with a cTag mark gets that cTag as
its content, with no hash, and is taken as clean once the full listing, which the folder's start
asks for, brings OneDrive's version and the plan compares the two; any other file is content
changed here. An unmarked entry is a new item. What was unsent and left nothing on disk — a
removal, a move — is gone.

## 8. The mount's end

### 8.1 The take-down

A part that cannot keep the disk and the rows saying the same — an undo that failed, a backing
file that could not be made after its row, an entry set aside that could not be put back — asks
for the mount's take-down (`MountTakeDown::ask`). The ask holds the folder read-only for good, so
no further change is taken, and reports the cause to the owner of the folder's state, which stops
the sync, takes the folder down and starts it again after a growing pause
([sync.md](sync.md) §9). The new start's check settles the change from its record.

### 8.2 The stop

`ItemMount::stop`, once, in this order: the write-operation thread (the job in flight waited for,
the queued ones dropped); the bridge; every download aborted, its waiters answered `EIO`, and none
started from then on; every read that waits for OneDrive aborted; the mount's connection aborted
through `/sys/fs/fuse/connections/<n>/abort`, so that a program still holding a file does not keep
the unmount waiting; the mount taken off and its session joined; the reading connections closed;
and the write bits taken off the folder on disk. A program that held a file of the folder gets an
error ([F296](../limitations/F296.md)). A Forget gives the write bits back for good.

### 8.3 A stale mount

A daemon that dies leaves its mount on the folder, and every path in it then fails with "not
connected". It is told without trusting the folder's attributes, which the kernel answers from
memory for a second after the daemon's death (`stale_mount.rs`): the kernel's table of mounts says
whether a FUSE mount lies over the folder, and `statfs` of it, which the kernel hands to the
daemon each time, is refused at once by a mount whose daemon is gone. The question is asked on a
thread of its own and waited for two seconds: a mount whose daemon answers slowly is left alone. A
stale mount is taken off with `fusermount3 -u -z`, by the mount's build and by the tidy step.

### 8.4 The tidy step and the write bits

While a mount lies over the folder the backing directory is the daemon's alone and has its write
bits; while none does, nothing has them (0555 and 0444), so nothing is put into the folder by
mistake. The build gives them back before the start-up check, a stop and a build that fails take
them off.

The tidy step (`tidy_step.rs`) follows the daemon's end however it ended: the unit's
`ExecStopPost`, `konedrived --tidy`. For every folder the configuration records as showing
OneDrive it takes a stale mount off and, when no mount lies over the folder, removes the write
bits; then it tells the desktop to read those directories again. The folder is then a plain
directory that shows the downloaded files and the files made here that were not sent, and no file
that is not on disk ([F296](../limitations/F296.md)).

## 9. Telling others what changed

The kernel tells whoever watches the folder (inotify, a file manager) of what a program does
through the mount. A step towards the folder, a landing, a free-up and a pin change the backing
directory and the rows behind the kernel's back: it tells nobody, and keeps the pages and the
attributes it cached.

The inotify bridge (`item_inotify_bridge.rs`, `item_announcements.rs`) is a queue of announcements,
each by local item id and place: `Appeared`, `Vanished` with the former place and the kind,
`ContentChanged`, `MarksChanged`; a move is a vanishing and an appearing. The former place is taken
by the step before its commit, since the rows then have the new one only. The bridge's own thread
performs each one as a system call by path through the mount — `mknod` or `mkdir`, `unlink` or
`rmdir`, `utimensat` — so that the kernel tells its watchers. The mount knows the bridge's
requests by the id of the thread that asks, and answers them from the rows and the announcement in
progress, changing nothing:

- a lookup of a vanishing name gets the attributes of the item that had it, with no lifetime; a
  lookup of an appearing name gets `ENOENT`, and its `mknod` or `mkdir` is answered from the rows;
- its `unlink` or `rmdir` is done when the rows show no such name;
- its `utimensat` is answered as getattr. For `ContentChanged` the kernel's cached pages of that
  node are dropped first (`FUSE_NOTIFY_INVAL_INODE`); without it a read through the mount would get
  the old content from the cache.

A program's own requests are never announced. `MarksChanged` tells a file manager to read the
attributes of §3.5's last paragraph again: after a pin, and after the plan changed what the sync
mark says ([desktop.md](desktop.md)).

## 10. Known limits

- **No folder while no daemon runs.** The folder is the backing directory then, without write bits,
  and a program that held a file open gets an error ([F296](../limitations/F296.md)). Only the user
  who runs the daemon reaches the folder, root included ([F301](../limitations/F301.md)).
- **A range is not checked against the content.** A content OneDrive replaced is read as the one
  opened until a listing's step or a download tells otherwise ([F297](../limitations/F297.md)); a
  file without a hash is downloaded unverified ([F34](../limitations/F34.md)).
- **Programs that read to look download.** A file read whole, or past the threshold, is downloaded;
  Dolphin reads some files to find their type or draw a preview ([K1](../limitations/K1.md)).
- **No links, special files or attributes of the user's own** ([F300](../limitations/F300.md)); the
  executable bit is kept on this computer only ([F303](../limitations/F303.md)).
- **Hand changes while no daemon runs** are a new item and an item shown again, never a move or a
  removal in OneDrive; an edit that keeps the size and the time in whole seconds is not seen
  ([F298](../limitations/F298.md)).
- **A lost store** costs a full listing and every unsent change that left nothing on disk
  ([D36](../limitations/D36.md)).
- **An unfinished download does not outlive the daemon**: its file has no name, so a large file
  whose download broke is downloaded from its start after a restart.
- **A row the start-up check held** stays held for as long as that mount is up, also when a
  program writes the item meanwhile; the next start's check decides it.
- **A read refused during a wait** of OneDrive's reaches a program that reads through the kernel's
  page cache as `EIO`, not `EAGAIN`; an open is therefore refused at once, which reaches it as
  `EAGAIN`.
- **A directory with nothing on disk** shows the time the mount started, which changes at every
  start.

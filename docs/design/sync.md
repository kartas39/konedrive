# Sync

How a folder's two sides are kept in step: the item and its three versions, who writes what, the
listing, the sync plan, a step and what it remembers, the plan's task, and a folder's start and
stop. The kernel's requests and the backing directory are in [mount.md](mount.md); what an upload
sends and how OneDrive answers it, in [writes.md](writes.md).

## 1. Scope

Everything here runs in the daemon, once for each account's folder.

| Part | Where | Answers for |
|---|---|---|
| The store | `crates/konedrive-tree/src/` (`ItemStore` writes, `ItemReader` reads) | the rows: what the folder shows, what OneDrive has, what the two sides agreed on (§2) |
| The listing | `crates/konedrived/src/listing/` (`ListingCycle`, `ListingPoller`) | OneDrive's delta listing written into the rows' OneDrive versions (§5) |
| The sync plan | `crates/konedrive-tree/src/sync_plan.rs`, `sync_plan/` (`make_sync_plan`) | a pure function: the steps that make the two sides agree, and why the rest waits (§6) |
| The step runners | `crates/konedrived/src/folder_steps/`, `onedrive_steps/`, `sync_plan_task/row_steps.rs` | one step each, towards the folder, towards OneDrive or of the rows, and its commit (§7) |
| The step memory | `crates/konedrive-tree/src/step_memory.rs`, `step_memory/` | what a step keeps of an item between tries and across a stop (§7.5) |
| The plan's task | `crates/konedrived/src/sync_plan_task/` (`SyncPlanTask`) | reading the rows, making a plan, starting and settling its steps (§8) |
| What the tasks share | `crates/konedrived/src/sync_parts/` | `RunningSteps`, `SyncPlanWake`, `PublishedPlan`, `SendingHolds`, `OneDriveSpace`: each one piece of state several tasks read (§8) |
| Start and stop | `crates/konedrived/src/sync_tasks/` (`SyncTasks`) | the order of a start, a stop and a switch of the access mode (§9) |
| The owner of a folder | `crates/konedrived/src/account_folder/` (`SyncService`) | when a folder starts and stops (§9) |

Three writers write rows: the mount's write operations write local versions, the listing writes
OneDrive versions, and a step's commit writes what the step did. Each wakes the plan's task. The
task reads what differs in one read transaction, makes a plan and starts its steps. A step runs
outside every lock and commits under the folder's tree lock, only if the rows are still those it
was planned from.

```text
 kernel ──▶ mount's write operations ──┐
                                       ├─▶ rows ──read──▶ plan's task ──▶ make_sync_plan
 Graph delta ──▶ listing cycle ────────┘   ▲                                   │
                                           └── commit ◀── step runners ◀── steps
```

## 2. The item

### 2.1 The row

An item is one row of `items` (`Item`, `crates/konedrive-tree/src/model.rs`):

| Field | Holds |
|---|---|
| `local_item_id` | the key: never reused, never changed |
| `onedrive_item` | the binding, an `OneDriveItemReference`; unique when set |
| `kind` | a file or a directory, set when the row is made and never changed: a file replaced by a directory is another item |
| `local`, `onedrive` | the local and the OneDrive version; `None` while that side does not have the item |
| `onedrive_etag`, `onedrive_mime` | what a request about the item is guarded with; the MIME type a listing gave, for thumbnails |
| `synced_place`, `synced_content` | what the two sides last agreed on, the place and the content apart |
| `content_presence` | `OnDisk` or `NotOnDisk`: a stable state, not a download in progress |
| `content_before_write` | a file's content before its first writer opened it, kept until the last writer closes |
| `placement` | whether the folder can show the item at all, as the listing said of the item itself ([P7](../limitations/P7.md)) |
| `is_name_ignored` | the upload rules ignore the item's own name |
| `confirmed_at_count` | the stamp of the guard against an outdated listing (§5.3) |
| `is_executable` | the owner's executable bit, this computer's alone ([F303](../limitations/F303.md)) |
| `unsent_since` | a stamp of one counter while the local version is not the synced one |

The row also keeps each name folded as OneDrive compares names (`FoldedName`: lower case and
Unicode's composed form), and whether a directory above it in the folder has an ignored name and no
binding.

### 2.2 The three versions

`ItemVersion` holds a place (`ItemPlace`: the parent's local item id and a name) and, for a file, a
`ContentVersion`. One type serves the three sides; the field says whose. A parent is named by its
local item id on every side, so that places of the two sides compare. A name is kept as it was
given. The root has no place.

A content version has a size, a time, and one of two cases: `Hashed { hash, ctag }`, given by
OneDrive or computed here (either may be missing); or `ChangedHere { change_count }`, changed by a
program with no hash known, the count raised at each first open for writing.

What a side did is read off the row against the synced version (`sync_plan/side_change.rs`): it
never had the item, removed it, or has it, moved or not, its content the same, changed, or, here,
not known without a hash. A time alone is no change of a content.

### 2.3 What holds of the rows at all times

The module doc of `crates/konedrive-tree/src/item_store.rs` names who holds each, the schema or the
code:

- in the folder, an item's parent exists and is a directory, no item is above itself, the depth is
  bounded, and a directory holds one item per name;
- a binding belongs to one item, and moves only between items of one kind;
- nothing is agreed on for an item with no binding;
- the root is row 1, a directory with no place, and stays;
- an item has `unsent_since` exactly while its local version is not the synced one: three triggers
  keep it, whoever writes.

On the OneDrive side the schema holds only that a parent is a directory's row and no row is its own
parent: while a listing is written page by page, two rows may say one name there (§6.4).

### 2.4 The node id is the local item id

The kernel's node id is the local item id; the root is 1, the number FUSE gives the root. Ids are
never reused, so the generation is always 0 and no table of node ids is kept. A request by an id
whose row is gone, or that the folder does not show, is answered `ESTALE` ([mount.md](mount.md) §4).

### 2.5 What is derived and never stored

- **The sync state of a file** (`Item::file_sync_state`): the first that holds of `LocalOnly` (no
  binding), `RemoteOnly` (not on disk), `ModifiedHere` (the local content is not the synced one) and
  `Synced`.
- **The path**: the names of the local places up to the root.
- **What is unsent** and **what OneDrive changed**: found by the indexes `items_unsent` and
  `items_changed_in_onedrive`, bounded; no reader walks every row.
- **Whether an item is left here**, never to be sent: no binding, and its own name ignored or that of
  a directory above it with no binding. An item OneDrive has is never left here.
- **Whether the folder can show an item as OneDrive has it** (`FolderVisibility`): its placement and
  that of every directory above it in OneDrive; not known while one above was never listed.

### 2.6 The store

`$XDG_STATE_HOME/konedrive/accounts/<account id>/fuse.sqlite`, in WAL mode. One thread owns the
writing connection (`Store`); readers use `ItemReader`, one read transaction per closure, and never
wait for the writer. Beside `items`: the step memory (§7.5), the records of place changes
([mount.md](mount.md) §3), the pins, the thumbnail marks, the activity log, the conflicts, and in
`meta` the delta link and the listing under way. A store of another layout is rebuilt, but for the
one just before when the change only added ([D36](../limitations/D36.md)).

## 3. Who writes what

Everything in a row is written through `ItemStore`, each change one transaction.

| What | Written by |
|---|---|
| local version | the mount's write operations (`ItemStore::apply`), when a program makes, renames, removes, opens for writing, truncates or closes ([mount.md](mount.md) §4); the start-up check ([mount.md](mount.md) §7); a step towards the folder's commit |
| OneDrive version, eTag, MIME type, `placement` | the listing (`write_listed_changes`, §5) |
| OneDrive version, eTag, synced place or content, `confirmed_at_count` | a step towards OneDrive's commit, when OneDrive confirmed (§7.2) |
| synced place or content | the commits of steps towards the folder and of the rows |
| `content_presence` | a download's landing, a free-up, the write operation that makes a file, the start-up check ([mount.md](mount.md) §5, §6) |
| `is_name_ignored` | every write of a local place, and `give_up_binding`; all rows once when the ignored names change |
| `is_executable` | `set_executable_bit` alone |
| `unsent_since` | the schema's triggers alone |
| the binding | given to an item OneDrive confirmed as new (`bind_item`), to a row made for an item a listing brought (`make_onedrive_item`, also at a parent's first mention) and to the root; moved by the operations of §4 alone |

The listing writes OneDrive versions only, so it takes no tree lock: a `mkdir` never waits behind a
listing. The mount's write operations and the commits of steps take the tree lock
([mount.md](mount.md) §2).

**The mount reads the local version alone** for names, places and attributes: never the OneDrive
version, the plan or the disk. Three exceptions: the sync state is §2.5's function of the row; the
blocks a file takes are the backing file's; and the size and times of a file a program holds open
for writing are the backing file's, written into the row at the last close. The last is a rule of
correctness: the kernel puts an append where the answered size says ([mount.md](mount.md) §4).

## 4. When one item takes another's place

The local item id follows the file the kernel holds; the binding follows what OneDrive must see as
one item. Where the two part, the binding moves, by one of three operations
(`crates/konedrive-tree/src/item_store/bindings.rs`), each inside one transaction:

| Operation | When | What it does |
|---|---|---|
| `take_over_binding` | a file not in OneDrive is renamed over one that is (an editor's save, `LocalVersionChange::SavedOver`), or made where a file was removed here whose removal is not sent (`Made { replaced }`). The write operation names the item; the store checks it | the replacing file takes the binding, the OneDrive version and what was agreed on; the replaced row goes. OneDrive sees one item whose content changed: its id, history and sharing links stay |
| `give_up_binding` | the step `GiveUpBinding` (§6.3) | the item keeps its id, place and content and is an item OneDrive does not have; a new row takes the binding and what OneDrive has |
| `merge_items` | the step `MergeItems`: two new directories of one name, or two new files of one name and content, one on each side | the local row takes the binding and the OneDrive version of the other, and what they were merged for is agreed on; the other row goes |

What is OneDrive's goes with the binding: its version, eTag, placement and stamp, and what the two
sides agreed is in a directory. What the step memory learnt of an unsent change of the items is
dropped, an upload session to be cancelled (§7.5). A pin follows the binding
([pinning.md](pinning.md)).

## 5. The listing

### 5.1 A cycle

`ListingCycle::run` (`crates/konedrived/src/listing/listing_cycle.rs`):

1. the drive the token reaches is checked against the folder's; another stops the cycle
   ([accounts.md](accounts.md) §6);
2. how to begin is decided (`decide_listing_start`): a full listing under way goes on from the link
   the store kept; a full listing asked for, or no delta link, begins a full listing; otherwise the
   changes since the delta link. A request from the start is always a listing begun anew;
3. per page: the count of confirmations read, the page fetched, each item classified
   (`listing/classify.rs`) and the page written in one transaction. Items the store answers are to be
   read again (§5.3) are read one by one and the page given again with the same link, five rounds at
   most. The plan is woken;
4. the page with the delta link ends the listing: the plan is woken with `ListingEnded`, the
   thumbnail filler nudged, `last_checked` written.

A page is written root first, then what is there (a parent's row made at its first mention, with
the binding alone), then what is gone with what the rows say is below it. Cycles never overlap,
and one is cancelled between requests, never in a page.

### 5.2 The poller and the notification socket

`ListingPoller` runs a cycle at once, then 60 s after the last ended (300 s while the notification
socket is up), after a failure after 5, 15 and 30 s and then every 60 s, and at once on a refresh;
refreshes given during a cycle are one. While the account's work is stopped no cycle runs, and
whoever ends the stop asks for one ([writes.md](writes.md) §6.3).

`listing/live.rs` keeps Graph's notification socket open: events within 2 s give one cycle, a
failed connection is tried again after 1, 2, 4 … 60 s, and the first connection after a drop, and
every renewal, ask for one cycle, since events in the gap are lost.

### 5.3 The guard against an outdated listing

A page fetched before a step's confirmation may say an older truth than the confirmation wrote. A
confirmation stamps its row with the next count of confirmations. A page is given with the count
read before its fetch, and what it says of a row stamped later is taken only when it is what was
confirmed; otherwise the row is left and the item answered, to be read again.

- A row whose removal was confirmed keeps its binding alone until a listing ends that began at its
  stamp or after: no older page finds the binding free and makes the item again, nor puts an item
  into a directory whose removal was confirmed since.
- Nothing is taken through a row left to be read again: a removal is written whole or not at all,
  and a full listing's end spares such a row, and what is below it, until it was read.

### 5.4 A full listing and its sweep

A full listing is begun for the first listing, after OneDrive answers 410, when OneDrive refuses the
kept link of a stopped full listing, when the start-up check bound entries by their marks
([mount.md](mount.md) §7), and when the plan finds rows no listing will mend
(`PlanFault::FullListingNeeded`). A request for one is kept in the store across a stop. Its pages
mark each row they write with the listing's id; at its end every row OneDrive had without the mark
is taken as gone, bounded by the count the listing began at. A refused link begins a full listing
once in a cycle; a second fails the cycle.

### 5.5 The delta link

The store keeps the listing under way: its id, its kind, the count it began at and the link to its
next page. A page of another listing is refused whole and dropped. The link is written last: a call
that leaves items to read again writes no link and ends nothing, so a stop in between loses
nothing, and the delta link is written only once nothing of its listing is left to read again. A
delta link refused otherwise than with 410 fails the cycle and is kept.

## 6. The sync plan

`make_sync_plan` is one pure function: no disk, no network, no clock, no store. Its module doc lists
the pieces and the order they decide in; one item by itself is decided in
`sync_plan/item_decision.rs`, each kind of case between items in its own module.

### 6.1 What it is given

`SyncPlanInput`, all values, gathered by `sync_plan_task/plan_input.rs`:

- a bounded set of rows — the unsent items by their stamps, what OneDrive changed, the pinned files
  to download, the shown items whose placement says no — closed with each one's directories up to
  the root, the items that have or could take its name, and for a directory that leaves or was
  removed, what is in it with their count;
- what only the daemon knows: the items left here, names OneDrive cannot hold, the files open for
  writing, the quiet time (2 s after a file's last change), the items a step runs for and the
  directories they send into, the pins, whether a listing is under way, whether OneDrive is full,
  this computer's name, the items the start-up check could not decide;
- what the steps remember (§7.5), and the time now.

What it can tell it was not given, it answers (`ItemNotGiven`, `ItemsInDirectoryNotGiven`), and the
task widens the set (§8.2).

### 6.2 What it answers

`SyncPlan`: the steps that can run now, one per item, the unsent first by their stamps; for every
other item a wait (for another item, a time, the user, a running step, the start-up check, a
listing, or what was not given), an exclusion (left here, cannot be shown, left by an upload), or a
fault. All steps of one plan can run at once in any order: where one must go first, the other waits
for its item. A wait never ends at nothing (`PlanFault::WaitWithoutEnd`).

Nothing is planned of an item held back: a step runs for it or its request is on record (§7.4), a
program holds it open for writing, or the start-up check could not decide it or a directory above
it. A file is planned as it stands once its last writer closed.

### 6.3 The tables

One item by itself, a test per cell (`sync_plan/tests/existence.rs`, `place.rs`, `content.rs`).
"Here" is the local version against the synced one, "in OneDrive" the OneDrive version against it.

**Whether the item exists:**

| here \ in OneDrive | is there | removed | never was |
|---|---|---|---|
| **is there** | the two tables below | a file not moved or changed here goes, its copy with it. Changed here, or moved here and on disk: `GiveUpBinding`, then sent as a new file. Moved here, not on disk: it goes. A directory goes once it shows nothing; one holding the user's own: §6.4 | new here: `MakeDirectory` or `SendNewFile`. Bound, but no listing ever told of it: waits for a listing, then `GiveUpBinding` |
| **removed** | a file: `SendFileRemoval`, guarded by the eTag, also when OneDrive moved it; when OneDrive changed its content it comes back (`ShowItem`), or where the folder cannot show it what was agreed on is forgotten. A directory: §6.4 | `AgreeOnRemoval`; the row keeps its binding until §5.3 lets it go | — |
| **never was** | `ShowItem`, not on disk; excluded where the folder cannot show it | — | — |

**Its place**, for an item both sides have:

| here \ in OneDrive | same | moved |
|---|---|---|
| **same** | — | `MoveItem`, the copy with it. Where the folder cannot show it: it goes here as if removed, unless its content was changed here, which is sent into it. Into a directory removed here, or to its own temporary name: the place here is sent |
| **moved** | `SendMove`, guarded by the eTag, before new content | one place: `AgreeOnPlace`. Two: OneDrive's stands and the folder follows; where it cannot (the name is taken or unusable here, OneDrive's directory was removed here, the item cannot be shown there) the local place is sent |

A directory the folder cannot show is never moved in OneDrive: it goes once it shows nothing.

**Its content**, for a file both sides have, once nothing of its place is to do:

| here \ in OneDrive | same | changed |
|---|---|---|
| **same** | — (versions one but for a cTag or hash: `AgreeOnContent`) | `TakeOneDriveContent`: the copy goes; a pinned file is then downloaded again |
| **changed** | `SendContent`, guarded by OneDrive's cTag | the same hash: `AgreeOnContent`; another: `GiveUpBinding`, and the local file becomes a conflict copy (§6.5); no hash here: `HashLocalContent` first |

A content OneDrive has that an upload of this folder left in place of what it sent is no change of
OneDrive's: the local content is sent again ([writes.md](writes.md) §4).
An item both sides have with nothing agreed on, at two places — a binding the start-up check guessed
from a mark on a copy — gives its binding up. A pinned file that agrees and is not on disk gets
`DownloadContent`. What the rows say OneDrive removed, or cannot be shown, waits while a listing is
under way; with none, rows that cannot be judged are a fault a full listing mends.

### 6.4 Between items

Each decision between items reads the answers so far and overrules what it must; a held item's answer
stands. Each case has a test (`sync_plan/tests/item_relations.rs`); an enumeration of every kind of row
against every relation checks that the plan always has a way on (`sync_plan/tests/enumeration.rs`).

- **One step per item**; a later change waits behind the step that runs.
- **What frees a name goes before what takes it**, on both sides (`name_contest`).
- **Names exchanged, or moved in a circle**: one item goes through a temporary name, on the side the
  names changed on (`name_exchange`). Here the move is on record before the disk
  ([mount.md](mount.md) §3); towards OneDrive the name is in the step memory before the request.
- **A directory moved here into what OneDrive has in it**: OneDrive's move stands and the local one is
  taken back (`TakeMoveBack`, `move_cycle`).
- **A directory before what is in it; a removal the other way.**
- **A directory removed here** (`removed_directory`): its removal is sent only when everything the rows
  say OneDrive has below it was shown here and removed here, and OneDrive changed none of its content
  since. It is then one unguarded `SendDirectoryRemoval`, whose confirmation takes the rows below.
  Otherwise the removal is of the folder alone: what was agreed on of the directory is forgotten, and
  what the user removed below it is decided item by item ([F299](../limitations/F299.md)). The plan,
  the step before its request (`ItemStore::can_directory_removal_be_sent`) and the commit ask one
  function. While it waits, nothing below it has a step, and it waits for every running request that
  sends into it.
- **A directory OneDrive removed that holds the user's own**: the clean items go, the directory gives
  up its binding and is sent again, then what is in it ([F255](../limitations/F255.md)).
- **A directory whose binding moves**: what is agreed to be in it waits (`directory_binding`).
- **Two rows that say OneDrive has one name** (`onedrive_name_holders`): the second waits for a
  listing under way; with none, one name twice is a fault, and two names that only fold to one are
  two items.
- **What the folder cannot show**: a new item at the name of such an item steps aside; nothing is sent
  into a directory the folder cannot show or that is left here.
- **A wait is followed to its end** (`wait_chain`).

### 6.5 Names

OneDrive takes two names that differ by case for one, and says neither by which rule nor what it does
with Unicode's forms of one letter. The plan does not depend on knowing:

- **OneDrive is the judge.** The plan foresees a clash by its own cautious rule (`FoldedName`). Where
  OneDrive answers that a name is taken, that is kept (§7.5) and the next plan treats the two names as
  one. A wrong rule costs one refused request, never a loop or a loss.
- **A new item of one name on each side**: two directories, or two files of one content, are one
  (`MergeItems`); where only a hash says, the local content is hashed first; otherwise the user's item
  steps aside.
- **Stepping aside** is `RenameToConflictCopy`: renamed here to `<stem>-<computer>.<ext>` (from the
  second try with a number, `make_conflict_copy_name`), free on both sides as OneDrive compares
  names, and sent under it by the next plan. Of two local items that want one name, the later unsent
  steps aside. A file changed on both sides comes here too: its binding given up, OneDrive's item is
  shown at the name and the local file steps aside ([writes.md](writes.md) §5).
- **A name OneDrive stores otherwise than sent** is OneDrive's change of the place; the folder follows.
- **A name OneDrive cannot hold** (`local/names.rs`: certain characters, a space at either end, not
  UTF-8, reserved names) is not sent and not altered: the item waits for the user.

### 6.6 What is not sent

Asked before any step towards OneDrive (`sync_plan/sending_obstacle.rs`), the first that holds: an
item left here; a name OneDrive cannot hold, where a name is sent; what OneDrive refused of the item,
until the user acts; content while OneDrive is full, and within the quiet time; a next try still to
come. Moves and removals go on while OneDrive is full. A file too large for OneDrive is not foreseen:
the refusal is kept and the file waits for the user (§7.3).

### 6.7 The access mode's part

The plan knows no access mode. A read-only folder refuses every change and sends nothing: a hold of
all sending (`SendingHold::FolderReadOnly`), set before any plan of a folder started read-only and
before the mount's switch. What is unsent stays and is sent when the folder is read-write again.
Steps towards the folder, of the rows and the downloads of pinned files run in either mode
([writes.md](writes.md) §2).

### 6.8 What a removal takes

There is none. A removal is planned and sent as any other change, however many files it takes
([F302](../limitations/F302.md)). What the plan bounds is what a removal takes with it: a directory's
removal only what the user saw and removed (§6.4); nothing because a file is missing from the disk
at start ([F298](../limitations/F298.md)), and nothing because a name is ignored.

## 7. A step

A step is one case of `SyncStep`: `OneDriveStep` (one request to Graph; an upload is one request in
fragments), `FolderStep` (the backing directory and the local version), or `RowStep` (the rows
alone). Each runner's module doc gives the order of its steps.

### 7.1 Every step is conditional

A step carries the versions it was planned from (`ItemVersions`). It runs outside the lock, for
seconds or minutes; its commit (`item_store/step_commit.rs`), under the tree lock in one
transaction, writes what the step did only if those are still the row's, and otherwise nothing
(`RowsChanged`, no error): the next plan decides. So do a download's landing and a free-up.

A step towards the folder or of the rows asks all the versions. A step that made, moved or filled an
item in OneDrive asks only the OneDrive version and what was agreed on: a program may rename or
change the item meanwhile, or save a file over it, and what OneDrive confirmed is written to the row
that has the binding then. A step towards the folder runs on the mount's write-operation thread, its
name change on record first, and is undone on disk when its commit answers `RowsChanged`
([mount.md](mount.md) §3).

### 7.2 What OneDrive confirms

Written in parts, through one confirmation that writes the version, the eTag and the stamp of §5.3 in
one statement:

- a move: OneDrive's place as answered, and the place sent as agreed on;
- an upload: OneDrive's content, the content sent as agreed on, and as the local content only while it
  is still the one sent; a file changed meanwhile stays unsent;
- a new item: the binding (`bind_item`), merging a row a listing made of it meanwhile;
- a removal: the row keeps its binding alone.

Where the commit finds a new item's row gone (removed here while it was sent), the step removes what
it made (`onedrive_steps/made_items.rs`). After a restart it does not: such a file comes back.

### 7.3 Refusals

A step OneDrive refuses decides nothing. A name taken (409) or an item changed (412) asks for a cycle,
and the next plan has the case as rows. Two causes no listing shows are the step's own
([writes.md](writes.md) §4): an upload session's empty file that holds the
name ([F172](../limitations/F172.md)), and its own earlier request whose answer was lost, which it
adopts. What came of a step is settled in one place, the table of `sync_plan_task/step_settlement.rs`:
a failed try and its wait; a taken name; a refusal for good (403, 413, a final 400) kept until the item
changes; a refusal for lack of space kept until the quota says it fits; a throttle or a sign-out,
which hold all sending and count no try.

### 7.4 Requests on record

A request that changes OneDrive with one answer — a directory made, a move, a removal, an empty file
— is on record before it leaves (`request_records`): the item, the directory, the name, the content.
An upload through a session has its opening and session in the step memory instead. The record goes
with the commit, with a certain refusal, or once what became of it was read
(`onedrive_steps/lost_answers.rs`). While it stands the plan holds the item as running and sends no
removal of its directory, also across a stop; one a stop left is settled by reading the item once
sending is not held, after the first listing of the run. A lost answer is never a state the plan
cannot see.

### 7.5 The step memory

What a step keeps beyond the versions is kept by local item id and is no source of truth: a fact is
answered only while the row says what it was learnt for (`ItemMemory::split_by_standing`), and every
fact names what voids it. A fact of an unsent change is learnt under the row's stamp and answered only
while the row has it, so a commit that makes the sides agree forgets it by that alone; a fact of what
OneDrive has is void once OneDrive answers again, by the step's next answer or by a listing, in the
page's transaction.

| Kept | For | Void when |
|---|---|---|
| the upload session: address, expiry, progress, content sent | going on after a stop | the stamp or the bytes are others (the address is then to cancel); the binding moves; the commit |
| a session about to open, by place and time | telling this folder's empty file from another's | its address is known; the place resolved; a day after its last try |
| a session to cancel | no empty file left holding a name | cancelled |
| the failed tries | waiting, and saying why | the stamp; a confirmed request; the binding moves; the time, or a refresh |
| a refusal for lack of space | not sending until there is room | a quota read says it fits; the stamp or bytes; the binding moves |
| a refusal for good, of a version or a removal | not sending until the item changes | the stamp, place or bytes; the binding moves; a sign-in, for "not allowed" |
| a name answered taken, at the place sent | treating two names as one (§6.5) | OneDrive answers again about the name; the binding moves |
| a temporary name, and where it leads | finishing an exchange after a crash | the item confirmed or listed elsewhere, or gone; the step done with it |
| an item an upload left with other content | removing it before sending again | the folder has the item; a listing names it otherwise or gone |
| a content that is this folder's own upload (`own_upload_contents`) | not reading it as OneDrive's change | a listing writes another content; it was sent again, or OneDrive's taken |
| a request on record | settling a lost answer (§7.4) | the commit; a certain refusal; its outcome read |

Every fact goes with its item's row. The failed tries of a step towards the folder, of the rows, or
towards OneDrive from a row that is not unsent are kept by the plan's task for its run, with the
versions the step was planned from (`sync_plan_task/local_step_tries.rs`): nothing of a row tells one
piece of such work from the next.

## 8. The plan's task

### 8.1 A round

`SyncPlanTask`, one loop per folder in either access mode. A round (`decide_round`) makes a plan —
in one read closure (`plan_input::plan_next_steps`), its answers published in `PublishedPlan` for the
bus and the mount's sync mark ([writes.md](writes.md) §7) —; or goes
on with the last plan, when only steps finished and none changed anything; or sleeps. The steps that
can start are chosen (`step_choice`) and started; outcomes ready together are settled in one job.

### 8.2 What bounds the plans

A plan reads 1000 items of each kind that differs, and twice as many while no step can run and more
is there, or while the unsent items that wait for the user crowd out those that could be sent. A
plan is made for any cause but a finished step, for a time reached, or for steps that changed
something — once for all settled together. The new plan's steps replace those the last one left:
every step is conditional, so a step a few plans old is safe. A plan's `ShowItem` steps run as one
job, so a first listing of N items is a few plans per page, not N. The pinned files to download, the
one reader that scans, are read only while one may be missing its content.

### 8.3 How many steps run at once

One per item, counting every row a step carries. Of the rows, all at once. Towards the folder, as
they come: each is one job of the write-operation thread. Uploads of content and downloads of pinned
files: the transfer pool's slots and 4 more waiting at the pool, so that it can grow; every other
request to OneDrive, one at a time. A download starts only where it leaves the free space the
account keeps (`sync_plan_task/disk_space.rs`).

What holds sending (`SendingHolds`): the stop, the sign-out, an account that may not write, a
read-only folder, the pause, OneDrive's throttle, and no listing of this run ended yet. Each holds the
steps towards OneDrive; the stop, the sign-out and the pause hold downloads too; none holds a step
towards the folder or of the rows. While OneDrive is full no content starts (`OneDriveSpace`).

### 8.4 What wakes it

`WakeCause`: a local version written; a listing's page written or a listing ended; a step finished;
a pin set or taken off; a refresh; a quota read; a hold changed; a request record settled; the next
try of what is left to clean in OneDrive. And the soonest time: a plan's wait, the end of a throttle
or a timed pause, the next quota read.

### 8.5 The stop

At the stop's token the loop ends its own steps and returns: sending is held; requests and hashes in
flight are aborted, none having written anything; a step whose commit has begun, and every step
towards the folder, is let finish and settled. Nobody else ends a step.

## 9. A folder's start and stop

### 9.1 Who owns them

`SyncService` (`crates/konedrived/src/account_folder/`), one per account, starts and stops a folder
only inside a change of the folder, which holds the folder's write lock. `sync_tasks/` holds the
order of each; `SyncTasks` holds the poller, the plan's task, the thumbnail filler and the stop's
token. No task of the sync starts or takes down a folder, and none watches the account: the owner's
watcher sets and ends the holds of a sign-out and of an account that may not write. The sync is
stopped only to leave the account, forget the folder, take it down, or end the daemon; a switch of
the access mode and a change of the ignored names run beside the tasks.

### 9.2 The start

`start_order::start_folder_sync`, run in a task of its own and never cut between stages:

0. the upload sessions a store of an older version holds are cancelled before that store goes;
1. the store opened, made or rebuilt; the activity log attached;
2. the drive for the check decided; a folder whose drive is not known is not mounted;
3. the leaf state made, then the mount built — the start-up check runs in it — and served
   ([mount.md](mount.md) §7);
4. a full listing asked for where the check bound entries by their marks;
5. the holds that stand set, and the access mode applied with its hold, before any plan;
6. the quota read once;
7. the runners, the listing's poller (a first cycle at once), the thumbnail filler and the plan's
   task started. Nothing is sent until the first listing of this start ended.

A folder that keeps its parts from an earlier sync skips stages 0 to 4.

### 9.3 The stop

The owner cancels the stop's token, then takes the folder's write lock, then awaits the stop
(`stop_order::stop_folder_sync`). In that order because a step towards the folder waits for the
folder's lease, a read of that lock: cancelled first, it ends as not run. The poller ends, then the
plan's task (§8.5), then the thumbnail filler, and the parts the folder keeps — the store and the
mount, still served — are started again, taken down (`take_folder_down`) or forgotten
(`forget_folder`, which gives the backing directory its write bits back).

### 9.4 The take-down and its growing pause

When the mount asks to be taken down for a fault ([mount.md](mount.md) §8) it tells the owner, which
takes the folder down inside one change, says why on the folder, and starts it again by itself with
nothing kept, so that the start-up check runs: after 1 minute, then 5, 30, then every hour. A start
that stays up 10 minutes starts the count over; a refresh tries at once.

## 10. How it is tested

- **A test per case** of every decision: each cell of §6.3, each case between items, each line of the
  settlement's table, each voider of the step memory.
- **Enumeration**: every kind of row against every relation to another item, with every fact only the
  caller knows; the plan must always answer something that proceeds (`sync_plan/tests/enumeration.rs`,
  and `step_memory/tests/enumeration.rs` for the facts).
- **Convergence**: rows changed at random on both sides, each by a real operation of the store,
  planned and run in random orders until no step is left; within a bound of rounds, the rules holding
  after every step, the three versions equal, nothing lost (`sync_plan/tests/convergence.rs`). The
  plan's task does the same through its loop.
- **Races**: the steps towards the folder against a program's changes, a listing's pages and deaths at
  every point between a disk change and its commit, in orders from a seed
  (`folder_steps/tests/races.rs`).
- **The fixture** (`crates/konedrived/src/mount/item_fixture.rs`): a store in a file, a backing
  directory under no mount, the fake OneDrive, a clock the test holds, a mount built and not served.
  Each test makes it and the one part it tests.
- **The VM suite**, `tests/fuse-probe/run.sh`: a folder mounted for real over the fake OneDrive; the
  groups `mounted_folder`, `file_writes`, `file_reads` and `daemon_restarts` run the sync end to end.

## 11. Known limits

From [`../limitations/`](../limitations/README.md): some OneDrive items are never shown
([P7](../limitations/P7.md)); a removal is sent with no confirmation ([F302](../limitations/F302.md));
a directory that holds what the folder cannot show cannot be removed here
([F299](../limitations/F299.md)); a file never uploaded keeps a directory OneDrive removed
([F255](../limitations/F255.md)); a change on disk while no daemon runs is no move or removal in
OneDrive ([F298](../limitations/F298.md)); an unfinished upload holds its name
([F172](../limitations/F172.md)); a store of another version is rebuilt ([D36](../limitations/D36.md)).

Beside them: Graph lists a drive only from its root, so the whole drive is listed; a guessed binding
and a real one renamed while no store was there read the same and are both given up (§6.3); and the
numbers here are chosen, not measured.

//! The rows of `items` for who only reads them: the threads of the mount, each with a
//! connection of its own, and the bus.
//!
//! **State owned:** none. An [`ItemReader`] is handed out for one closure
//! ([`TreeStore::read_items`]) over a store that was opened for reading alone
//! ([`TreeStore::open_read_only`]) or the store's own, and writes nothing. The closure
//! runs in ONE read transaction: every answer it gets is of one moment, however many rows
//! it reads (a path is one read per directory above; a plan reads thousands). It sees
//! what was last committed and never waits for the writer (WAL).
//!
//! **Answers for:** every question about the rows and about what the steps remember that
//! needs no write: it is the ONE reading surface. [`ItemStore`](super::ItemStore) writes;
//! what it reads, it reads through [`ItemStore::reader`](super::ItemStore::reader). The
//! readers `ItemStore` has today call the same functions of `item_row` and go when the
//! switch has re-pointed their callers. Here are also the readers nothing answered before
//! the mount and the bus read the rows (the map's MISSING-1.q, 2.a, 2.b, 2.d, 2.e, 2.g,
//! 6.d, an item by its path, and the items left here), the plan's readers
//! (`plan_readers` below), and the step memory's (`memory_reader`).
//!
//! **Tested without the rest**, against a store in memory filled through `ItemStore`:
// test: a_reader_of_a_store_opened_for_reading_alone_sees_what_the_writer_committed
// test: a_closure_reads_one_moment_though_the_writer_commits_meanwhile
// test: an_item_is_found_by_its_path_and_a_path_through_a_file_finds_nothing
// test: every_reader_answers_what_the_item_stores_reader_of_the_same_name_answers
// test: a_removed_file_is_found_by_the_place_the_two_sides_agreed_on
// test: a_removed_file_whose_removal_was_sent_is_not_found_by_its_synced_place
// test: the_shown_files_below_a_directory_are_read_at_any_depth_and_bounded
// test: the_counts_tell_what_onedrive_has_what_is_shown_and_what_cannot_be_shown
// test: of_what_cannot_be_shown_the_top_item_of_each_part_is_listed_with_its_reason
// test: the_unsent_total_leaves_out_what_is_left_here_and_sums_the_local_content
// test: the_items_left_here_are_read_bounded
// test: the_path_of_an_item_the_folder_does_not_show_is_that_of_its_synced_place
// test: the_unsent_removals_are_read_in_the_order_they_came_to_be_unsent
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::model::{FolderVisibility, Item, LocalItemId, OneDriveItemId, OneDriveItemReference, SkipReason};
use crate::sync_plan::SendingState;
use crate::{FoldedName, ListingUnderWay, Store, TreeError, TreeStore};

/// The rows of `items`, read and never written: borrowed from a store, for one question or
/// one job. Who may make one: anyone with a `&TreeStore`; the mount's threads make theirs
/// over a connection opened for reading alone, the bus over the store's reading thread
/// ([`Store::read_items`]).
pub struct ItemReader<'a> {
    store: &'a TreeStore,
}

/// How many items there are of each kind the folder's counts on the bus show
/// (`ItemsListed`, `ItemsPlaced`, `SkippedCount`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemCounts {
    /// The items OneDrive has, by their rows: those with an OneDrive version, the root
    /// left out.
    pub in_onedrive: u64,
    /// The items the folder shows, the user's own unsent ones among them, the root left
    /// out ([`ItemStore::count_shown_items`](super::ItemStore::count_shown_items)).
    pub shown: u64,
    /// The items OneDrive has that the folder cannot show: each with all OneDrive has
    /// below it.
    pub cannot_be_shown: u64,
}

/// What is unsent, as one count and one sum: `PendingCount` and `PendingBytes` before what
/// is kept back is taken off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnsentTotal {
    /// The items with a stamp `unsent_since` that are not left here.
    pub count: u64,
    /// The size of the local content of those of them that are files whose content is not
    /// the synced one.
    pub bytes: u64,
}

/// An item OneDrive has that the folder cannot show: a line of `Folder.Skipped()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemNotShown {
    pub item: Item,
    /// Where OneDrive has it, relative to the root: the names of its OneDrive places.
    pub onedrive_path: PathBuf,
    /// Why, as the listing said of the item itself.
    pub reason: SkipReason,
}

impl TreeStore {
    /// Runs `read` over the rows in one read transaction of this connection, which is
    /// ended when `read` returns. The one way to an [`ItemReader`] over a `&TreeStore`.
    /// Called by the mount's threads over their own connection (`fuse::read_connections`),
    /// by [`Store::read_items`], and by the write-operation thread's jobs over the writer,
    /// where a read must see the job's own last write.
    pub fn read_items<T>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError>) -> Result<T, TreeError> {
        todo!()
    }
}

impl super::ItemStore<'_> {
    /// The reader over this store's connection, inside whatever transaction the caller of
    /// the store's job has open: what an operation reads before it writes. Called by the
    /// jobs of the store's thread that read and then write.
    pub fn reader(&self) -> ItemReader<'_> {
        todo!()
    }
}

impl Store {
    /// Runs `read` over the rows on the store's read-only connection, in one read
    /// transaction, never waiting for the writer: the plan's reads
    /// (`sync::sync_plan_task::plan_input::plan_next_steps`) and the bus's questions about
    /// the queue, the counts and what is not shown. As [`Store::read`], which hands out a
    /// [`ReadStore`](crate::ReadStore); a store in memory is read through its own thread.
    pub async fn read_items<T: Send + 'static>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        todo!()
    }

    /// [`read_items`](Self::read_items) for plain threads.
    pub fn read_items_blocking<T: Send + 'static>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        todo!()
    }
}

/// The readers [`ItemStore`](super::ItemStore) has too: each calls the function of
/// `item_row` (or of the module named) that the store's reader of the same name calls, over
/// this reader's connection, and answers the same.
impl ItemReader<'_> {
    /// The item with this local item id, which is the kernel's node id; `None` when its
    /// row is gone. Called for getattr, open, the xattrs and every request by a node id.
    /// One read by the key (design §9, §15).
    pub fn find_item(&self, item: LocalItemId) -> Result<Option<Item>, TreeError> {
        todo!()
    }

    /// The item the folder shows under `name` in `directory`: a lookup. One read of the
    /// index `items_local_place`; the name is compared as it was given, bytes that are not
    /// UTF-8 included (design §3, §9).
    pub fn find_item_at(&self, directory: LocalItemId, name: &OsStr) -> Result<Option<Item>, TreeError> {
        todo!()
    }

    /// The item that has this binding. Called by a step towards OneDrive that asks whether
    /// an item Graph answered with is known here, and by the bus for a path's web address.
    pub fn find_bound_item(&self, onedrive_item: &OneDriveItemReference) -> Result<Option<Item>, TreeError> {
        todo!()
    }

    /// What the folder shows in `directory`, by name: a directory reading. One read of the
    /// index by parent; nothing of the backing directory is read (design §4, §9).
    pub fn list_children(&self, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// The path of what the folder shows as `item`, relative to the root: the names from
    /// its local place up. `None` for an item the folder does not show. One read per
    /// directory above it; called only where the backing directory is touched (design §8,
    /// §15).
    pub fn find_path(&self, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        todo!()
    }

    /// Whether `item` is left here: it has no binding, and its own name is ignored or that
    /// of a directory above it that has none. Called for the plan's `ignored_items` and
    /// the queue's answers.
    pub fn is_ignored(&self, item: LocalItemId) -> Result<bool, TreeError> {
        todo!()
    }

    /// Whether the folder can show `item` as OneDrive has it: its own placement and that
    /// of every directory above it in OneDrive. Called for the plan's `folder_visibility`
    /// and for `Folder.Skipped()`.
    pub fn find_folder_visibility(&self, item: LocalItemId) -> Result<FolderVisibility, TreeError> {
        todo!()
    }

    /// Whether a pin holds for `item`: its own, or that of a directory above it in the
    /// folder (`pinned_items`). Called for `user.konedrive.pin`, free-up and the plan.
    pub fn is_pinned(&self, item: LocalItemId) -> Result<bool, TreeError> {
        todo!()
    }

    /// The unsent items that are not left here, in the order they came to be unsent, after
    /// the stamp `after`, at most `limit`: `UploadQueue.Changes(limit)`. One read of the
    /// partial index `items_unsent`.
    pub fn list_unsent(&self, after: Option<u64>, limit: usize) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// The items that have `name`, as OneDrive compares names, in `directory` in the
    /// folder. Called by the mount's write operations, which refuse nothing by it, and by
    /// a step towards OneDrive that looks for the holder of a name.
    pub fn list_children_with_folded_name(&self, directory: LocalItemId, name: &FoldedName) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// The listing the store was given the beginning of and not the end. Called for the
    /// folder's "listing" state on the bus.
    pub fn find_listing_under_way(&self) -> Result<Option<ListingUnderWay>, TreeError> {
        todo!()
    }
}

/// The readers nothing had before the mount and the bus read the rows.
impl ItemReader<'_> {
    /// The file that was removed here from `name` in `directory` and whose removal is not
    /// sent: a file OneDrive has, which the folder does not show, whose synced place is
    /// that one. `None` when there is none, or the row there is a directory's.
    ///
    /// Called by the write operation that makes a file (mknod, create), under the tree
    /// lock, before it writes `LocalVersionChange::Made { replaced }`: the store checks
    /// that statement and does not find the item by itself (design §5). One read of the
    /// index `items_synced_place`. The caller leaves out an item a step is running for.
    /// MISSING-2.a.
    pub fn find_removed_file_at_synced_place(&self, directory: LocalItemId, name: &OsStr) -> Result<Option<Item>, TreeError> {
        todo!()
    }

    /// The files the folder shows below `directory`, at any depth, by local item id, after
    /// the item `after`, at most `limit`. Called by free-up of a directory, by a pin's
    /// download of what it holds and by the count of what is downloaded below a path. It
    /// follows the local places, never OneDrive's. MISSING-2.b.
    pub fn list_shown_files_below(&self, directory: LocalItemId, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// How many items OneDrive has, the folder shows, and the folder cannot show. Called
    /// after a listing's page is written and after a step's commit, for the folder's
    /// counts on the bus. Three counts, each over an index; `cannot_be_shown` walks the
    /// rows below each item whose own placement says so. MISSING-2.d, 2.e.
    pub fn count_items(&self) -> Result<ItemCounts, TreeError> {
        todo!()
    }

    /// The items OneDrive has that the folder cannot show by their own placement — the
    /// top item of each such part, not what is below it — by local item id, after `after`,
    /// at most `limit`, each with its path in OneDrive and the reason: `Folder.Skipped()`.
    /// Whether one of them is still on this computer, and what keeps it, is the plan's
    /// answer for the item, which the caller adds. MISSING-2.e.
    pub fn list_items_not_shown(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemNotShown>, TreeError> {
        todo!()
    }

    /// How many items are unsent and not left here, and the bytes their changed contents
    /// have: one sum over the partial index `items_unsent`, no row read whole. Called for
    /// `PendingCount`/`PendingBytes`, for the text of a switch to read-only, and by Forget
    /// from a store that is closed (through a reader opened for the question).
    /// MISSING-2.g.
    pub fn count_unsent(&self) -> Result<UnsentTotal, TreeError> {
        todo!()
    }

    /// The items that are left here — no binding, and an ignored name of their own or of a
    /// directory above — by local item id, after `after`, at most `limit`.
    /// No member of the bus calls it: `NotUploaded()` lists what is kept back, as today,
    /// and what is left here is listed nowhere (the map's UNSURE-6.f). It is for
    /// `konedrivectl`'s diagnosis.
    pub fn list_items_left_here(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// The path the two sides last agreed on for `item`, relative to the root: the name of
    /// its synced place, below the path of that directory as the folder shows it, or as it
    /// was agreed on where the directory is not shown either. `None` when nothing was
    /// agreed on. Called for the path of a removal in `UploadQueue.Changes()` and in the
    /// activity log: [`find_path`](Self::find_path) walks local places, and a removed item
    /// has none. MISSING-6.d.
    pub fn find_synced_path(&self, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        todo!()
    }

    /// The items that were removed here and that OneDrive has, in the order they came to
    /// be unsent, after the stamp `after`, at most `limit`: a removed directory, and each
    /// item OneDrive has below it that was removed with it, as rows of their own. Called
    /// by `ConfirmDeletes` (what is confirmed) and `RestoreDeletes` (what is taken back),
    /// which read it whole in pages. MISSING-1.k, 2.l.
    pub fn list_unsent_removals(&self, after: Option<u64>, limit: usize) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// The folder's state of sending, with the stamp of its switch to read-only, as
    /// [`ItemStore::switch_sending_to_read_only`](super::ItemStore::switch_sending_to_read_only)
    /// left it; read-write in a store that was never switched. Called by the plan's task
    /// at each plan and at start. MISSING-1.c.
    pub fn find_sending_state(&self) -> Result<SendingState, TreeError> {
        todo!()
    }

    /// Whether a full listing was asked for and has not begun
    /// ([`ItemStore::request_full_listing`](super::ItemStore::request_full_listing)).
    /// Called by the listing cycle when it decides how to begin. MISSING-1.d.
    pub fn is_full_listing_requested(&self) -> Result<bool, TreeError> {
        todo!()
    }
}

/// An item by its path, for the bus, whose every member of `org.konedrive.Files` takes one.
impl ItemReader<'_> {
    /// The item the folder shows at `path`, relative to the root: one read of the index by
    /// parent and name per name of the path, in this reader's one transaction. `None` for
    /// a path the folder does not show, and for one that goes through a file. Called by
    /// `fuse::ItemFilesInterface` and by the queue's answers; nothing else walks a path.
    pub fn find_item_by_path(&self, path: &Path) -> Result<Option<Item>, TreeError> {
        todo!()
    }

    /// The path of what the folder shows as `item`, relative to the root, and for each
    /// directory of that path, the outermost first, the item id of its binding when it
    /// has one: what a directory made in the backing directory above a place is marked
    /// with. `None` for an item the folder does not show. One read per directory above,
    /// in this reader's one transaction. Called by the write operations' read of their
    /// case, by the steps towards the folder and by a landing, each before it changes a
    /// place (`fuse::place_changes`).
    pub fn find_path_with_marks(&self, item: LocalItemId) -> Result<Option<(PathBuf, Vec<Option<OneDriveItemId>>)>, TreeError> {
        todo!()
    }
}

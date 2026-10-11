//! The rows of `items` for who only reads them: the threads of the mount, each with a
//! connection of its own, and the bus.
//!
//! **State owned:** none. An [`ItemReader`] is handed out for one closure
//! ([`TreeStore::read_items`]) over a store that was opened for reading alone
//! ([`TreeStore::open_read_only`]) or the store's own, and writes nothing. The closure
//! runs in ONE read transaction: every answer it gets is of one moment, however many rows
//! it reads (a path is one read per directory above; a plan reads thousands). It sees
//! what was last committed and never waits for the writer.
//!
//! **What that relies on.** A store in a file is in SQLite's WAL journal mode, which
//! `TreeStore::open_file` sets and the file keeps: a connection opened for reading alone
//! has it too. In that mode a read transaction reads one snapshot, the last commit at its
//! first read, whatever other connections commit meanwhile, and no reader waits for the
//! writer or holds it up. The moment is that of the closure's first read, not of its call.
//! A store in memory has one connection: it is read on its writer's thread, between two
//! jobs.
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
// test: a_reader_sees_nothing_of_an_open_write_transaction_and_all_of_it_once_committed
// test: a_closure_reads_one_moment_though_the_writer_commits_meanwhile
// test: a_reader_cannot_write
// test: a_read_over_the_writers_connection_sees_its_own_last_write
// test: the_shared_store_reads_what_its_writer_committed
// test: the_marks_of_a_path_are_one_for_each_name_the_outermost_first
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
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use super::item_row::{self, item_from, name_value, CONTENT_COLUMNS, ITEM_COLUMNS, LEFT_HERE};
use super::{find_folder_visibility, find_path, is_ignored, list_rows_up_in_onedrive, refuse};
use crate::model::{FolderVisibility, Item, ItemKind, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement, SkipReason};
use crate::step_memory::find_mismatched_item;
use crate::{FoldedName, ListingUnderWay, Store, TreeError, TreeStore, MAX_CHAIN};

/// SQL for "the row's own placement says the folder cannot show the item". The index
/// `items_not_placed` holds these rows, and a query finds them by it when it asks in these
/// words.
pub(crate) const NOT_PLACED: &str = "placement != 'placed'";

/// The rows of `items`, read and never written: borrowed from a store, for one question or
/// one job. Who may make one: anyone with a `&TreeStore`; the mount's threads make theirs
/// over a connection opened for reading alone, the bus over the store's reading thread
/// ([`Store::read_items`]).
pub struct ItemReader<'a> {
    store: &'a TreeStore,
}

/// What OneDrive has in a directory, at any depth, that the folder never showed
/// ([`ItemReader::find_onedrive_items_never_shown_below`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemsNeverShown {
    /// Nothing: all OneDrive has in it the folder shows, or showed and removed.
    Nothing,
    /// Items a listing brought that no step has shown yet, and none the folder cannot
    /// show: they appear by themselves.
    NotShownYet,
    /// At least one item the folder cannot show at all (a name too long, a OneNote
    /// notebook…): it stays in OneDrive whatever is done here.
    CannotBeShown,
}

impl ItemReader<'_> {
    /// The connection every reader of this module and of the modules beside it reads on.
    pub(crate) fn conn(&self) -> &Connection {
        &self.store.conn
    }

    /// The rows a query that selects [`ITEM_COLUMNS`] answers.
    fn list_items(&self, sql: &str, values: impl rusqlite::Params) -> Result<Vec<Item>, TreeError> {
        let mut statement = self.conn().prepare_cached(sql)?;
        let items = statement.query_map(values, item_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// `row`, an item OneDrive has whose own placement says the folder cannot show it, as
    /// a line of what is not shown; `None` when a directory above it in OneDrive cannot be
    /// shown by itself, so that the item is no top of its part.
    fn describe_top_item_not_shown(&self, row: Item) -> Result<Option<ItemNotShown>, TreeError> {
        let Placement::Skipped(reason) = row.placement else { return Ok(None) };
        let rows_up = list_rows_up_in_onedrive(self.conn(), row.local_item_id)?;
        if rows_up.iter().skip(1).any(|above| above.placement != Placement::Placed) {
            return Ok(None);
        }
        let onedrive_path = rows_up.iter().rev().filter_map(|row| row.onedrive.as_ref()?.place.as_ref()).map(|place| &place.name).collect();
        Ok(Some(ItemNotShown { item: row, onedrive_path, reason }))
    }
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
    /// The pins there are (`count_pins`): `Folder.PinnedCount`, published with the three
    /// above so that a listing that took a pinned item is said.
    pub pinned: u64,
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

/// The pinned files whose content is still to be downloaded, as one count and one sum: what
/// is left to download of what the user keeps on this device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PinnedDownloadTotal {
    /// The files a pin holds for that agree on everything and whose content is not on disk
    /// ([`ItemReader::list_pinned_files_to_download`]).
    pub count: u64,
    /// The size of their contents.
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
    /// Called by the mount's threads over their own connection (`mount::read_connections`),
    /// by [`Store::read_items`], and by the write-operation thread's jobs over the writer,
    /// where a read must see the job's own last write. On a connection that has a
    /// transaction open already, `read` runs inside that one. `read` must not call the
    /// store, wait for another thread, or block on anything but SQLite: a read
    /// transaction that stays open keeps the journal from being folded back into the file.
    pub fn read_items<T>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError>) -> Result<T, TreeError> {
        let reader = ItemReader { store: self };
        // Inside a transaction the caller of this connection has open, the read is of that
        // transaction, its own writes among what it sees: nothing is begun or ended here.
        if !self.conn.is_autocommit() {
            return read(&reader);
        }
        let transaction = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Deferred)?;
        let answer = read(&reader);
        // A transaction that only read has nothing to commit: it is ended.
        transaction.finish()?;
        answer
    }
}

impl super::ItemStore<'_> {
    /// The reader over this store's connection: what a job of the store's thread reads
    /// before it writes. It begins no transaction; every operation of `ItemStore` that
    /// writes begins its own and asks again, in it, what it was given. On the writer's
    /// connection nothing else writes between the job's reads, so they are of one moment
    /// all the same. Called by the jobs of the store's thread that read and then write.
    pub fn reader(&self) -> ItemReader<'_> {
        ItemReader { store: self.store }
    }
}

impl Store {
    /// Runs `read` over the rows on the store's read-only connection, in one read
    /// transaction, never waiting for the writer: the plan's reads
    /// (`sync_plan_task::plan_input::plan_next_steps`) and the bus's questions about
    /// the queue, the counts and what is not shown. As [`Store::read`], which hands out a
    /// [`ReadStore`](crate::ReadStore); a store in memory is read through its own thread.
    pub async fn read_items<T: Send + 'static>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.call_reading_connection(move |store: &mut TreeStore| store.read_items(read)).await
    }

    /// [`read_items`](Self::read_items) for plain threads.
    pub fn read_items_blocking<T: Send + 'static>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.call_reading_connection_blocking(move |store: &mut TreeStore| store.read_items(read))
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
        item_row::find_item(self.conn(), item)
    }

    /// The item the folder shows under `name` in `directory`: a lookup. One read of the
    /// index `items_local_place`; the name is compared as it was given, bytes that are not
    /// UTF-8 included (design §3, §9).
    pub fn find_item_at(&self, directory: LocalItemId, name: &OsStr) -> Result<Option<Item>, TreeError> {
        item_row::find_item_at(self.conn(), directory, name)
    }

    /// The item that has this binding. Called by a step towards OneDrive that asks whether
    /// an item Graph answered with is known here, and by the bus for a path's web address.
    pub fn find_bound_item(&self, onedrive_item: &OneDriveItemReference) -> Result<Option<Item>, TreeError> {
        item_row::find_bound_item(self.conn(), onedrive_item)
    }

    /// What the folder shows in `directory`, by name: a directory reading. One read of the
    /// index by parent; nothing of the backing directory is read (design §4, §9).
    pub fn list_children(&self, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
        item_row::list_children(self.conn(), directory)
    }

    /// The path of what the folder shows as `item`, relative to the root: the names from
    /// its local place up. `None` for an item the folder does not show. One read per
    /// directory above it; called only where the backing directory is touched (design §8,
    /// §15).
    pub fn find_path(&self, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        find_path(self.conn(), item)
    }

    /// Whether `item` is left here: it has no binding, and its own name is ignored or that
    /// of a directory above it that has none. Called for the plan's `ignored_items` and
    /// the queue's answers.
    pub fn is_ignored(&self, item: LocalItemId) -> Result<bool, TreeError> {
        is_ignored(self.conn(), item)
    }

    /// Whether the folder can show `item` as OneDrive has it: its own placement and that
    /// of every directory above it in OneDrive. Called for the plan's `folder_visibility`
    /// and for `Folder.Skipped()`.
    pub fn find_folder_visibility(&self, item: LocalItemId) -> Result<FolderVisibility, TreeError> {
        find_folder_visibility(self.conn(), item)
    }

    /// Whether a pin holds for `item`: its own, or that of a directory above it in the
    /// folder (`pinned_items`). Called for `user.konedrive.pin`, free-up and the plan.
    pub fn is_pinned(&self, item: LocalItemId) -> Result<bool, TreeError> {
        item_row::find_is_pinned(self.conn(), item)
    }

    /// The unsent items that are not left here, in the order they came to be unsent, after
    /// the stamp `after`, at most `limit`: `UploadQueue.Changes(limit)`. One read of the
    /// partial index `items_unsent`.
    pub fn list_unsent(&self, after: Option<u64>, limit: usize) -> Result<Vec<Item>, TreeError> {
        item_row::list_unsent(self.conn(), after, limit)
    }

    /// The items that have `name`, as OneDrive compares names, in `directory` in the
    /// folder. Called by the mount's write operations, which refuse nothing by it, and by
    /// a step towards OneDrive that looks for the holder of a name.
    pub fn list_children_with_folded_name(&self, directory: LocalItemId, name: &FoldedName) -> Result<Vec<Item>, TreeError> {
        item_row::list_children_with_folded_name(self.conn(), directory, name)
    }

    /// The listing the store was given the beginning of and not the end. Called for the
    /// folder's "listing" state on the bus.
    pub fn find_listing_under_way(&self) -> Result<Option<ListingUnderWay>, TreeError> {
        ListingUnderWay::find(self.conn())
    }
}

/// The readers nothing had before the mount and the bus read the rows.
impl ItemReader<'_> {
    /// The file that was removed here from `name` in `directory` and whose removal is not
    /// sent: a file OneDrive has, which the folder does not show, whose synced place is
    /// that one. `None` when there is none, or the row there is a directory's, or a
    /// listing said since that OneDrive does not have the file any more: a file made there
    /// is then a new item, and the two removals agree.
    ///
    /// Called by the write operation that makes a file (mknod, create), under the tree
    /// lock, before it writes `LocalVersionChange::Made { replaced }`: the store checks
    /// that statement and does not find the item by itself (design §5). One read of the
    /// index `items_synced_place`. Of several such files, the one removed last. The
    /// caller leaves out an item a step is running for. MISSING-2.a.
    pub fn find_removed_file_at_synced_place(&self, directory: LocalItemId, name: &OsStr) -> Result<Option<Item>, TreeError> {
        let sql = format!(
            "SELECT {ITEM_COLUMNS} FROM items INDEXED BY items_synced_place
             WHERE synced_parent_id = ?1 AND synced_name = ?2 AND local_parent_id IS NULL AND onedrive_parent_id IS NOT NULL AND kind = '{}'
             ORDER BY unsent_since DESC, local_item_id DESC LIMIT 1",
            ItemKind::File.as_str()
        );
        Ok(self.conn().prepare_cached(&sql)?.query_row(params![directory.as_i64(), name_value(name)], item_from).optional()?)
    }

    /// The item whose own pin holds for `item`: the nearest directory above it in the
    /// folder that has a pin of its own, else `item` itself when it has one, else none.
    /// A directory above comes first, also when the item has a pin of its own: that is
    /// the pin an unpin or a free-up of the item cannot take off. By the local places, as
    /// [`is_pinned`](Self::is_pinned), which answers whether this is some. Called by the
    /// mount's `Files` interface for the status of a path (`PinHolder`).
    pub fn find_pin_holder(&self, item: LocalItemId) -> Result<Option<LocalItemId>, TreeError> {
        // The way up is the one of `item_row::find_is_pinned`; of the rows on it that have a pin, a directory above comes before the item, the nearest first.
        let sql = format!(
            "WITH RECURSIVE above(id, depth) AS (
                 SELECT local_item_id, 0 FROM items WHERE local_item_id = ?1 AND local_parent_id IS NOT NULL
                 UNION ALL
                 SELECT items.local_parent_id, above.depth + 1 FROM above JOIN items ON items.local_item_id = above.id WHERE items.local_parent_id IS NOT NULL AND above.depth <= {MAX_CHAIN})
             SELECT above.id FROM above JOIN items ON items.local_item_id = above.id JOIN pins ON pins.id = items.onedrive_item_id
             ORDER BY above.depth = 0, above.depth LIMIT 1"
        );
        let holder = self.conn().prepare_cached(&sql)?.query_row([item.as_i64()], |row| row.get::<_, i64>(0)).optional()?;
        Ok(holder.map(LocalItemId::new))
    }

    /// What OneDrive has in `directory`, at any depth, that the folder NEVER showed: a row
    /// with an OneDrive place below it, no local place, and nothing agreed on (no synced
    /// place) — as `ItemBelowRemovedDirectory::NeverShownHere`. A row the two sides
    /// agreed on that has no local place is an item removed here whose removal is
    /// unsent, and is none of these: the last `rmdir` of an `rm -r` finds none.
    /// `CannotBeShown` when one of them is an item the folder cannot show
    /// (`find_folder_visibility`), else `NotShownYet` when there is any, else `Nothing`.
    /// By the OneDrive places, as `list_below_in_onedrive`. Called by the mount's `rmdir`,
    /// in the job that reads its case: a directory for which this is not `Nothing` is not
    /// removed here (`ENOTEMPTY`).
    ///
    /// An item an upload of this folder left there with other content than was sent, as
    /// the step memory says it now (`step_memory::find_mismatched_item` of the row's
    /// binding), is none of these either: it is the folder's own leftover and goes with
    /// the directory, as the removal's own check has it
    /// (`step_commit::is_directory_removal_as_planned`), so it does not make `rmdir`
    /// answer `ENOTEMPTY`.
    pub fn find_onedrive_items_never_shown_below(&self, directory: LocalItemId) -> Result<ItemsNeverShown, TreeError> {
        let mut never_shown = ItemsNeverShown::Nothing;
        for row in item_row::list_below_in_onedrive(self.conn(), directory)? {
            // What the folder shows, and what the two sides agreed on (removed here, the removal unsent), was shown.
            if row.local.is_some() || row.synced_place.is_some() || row.synced_content.is_some() {
                continue;
            }
            let is_left_by_upload = match row.onedrive_item.as_ref() {
                Some(onedrive_item) => find_mismatched_item(self.conn(), onedrive_item)?.is_some(),
                None => false,
            };
            if is_left_by_upload {
                continue;
            }
            match find_folder_visibility(self.conn(), row.local_item_id)? {
                FolderVisibility::CannotBeShown => return Ok(ItemsNeverShown::CannotBeShown),
                FolderVisibility::CanBeShown | FolderVisibility::NotKnownYet | FolderVisibility::NotInOneDrive => never_shown = ItemsNeverShown::NotShownYet,
            }
        }
        Ok(never_shown)
    }

    /// The directories the folder shows, the root first, each with its path relative to
    /// the root, by local item id, after the directory `after`, at most `limit`. By the
    /// local places. Called to tell the desktop's programs to list them again
    /// (`desktop::dirnotify::read_folder_again`), in pages.
    pub fn list_shown_directories(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<(LocalItemId, std::path::PathBuf)>, TreeError> {
        // The root has no local place and the smallest id: it is the first of the first page.
        let sql = format!(
            "SELECT local_item_id FROM items WHERE kind = '{}' AND (local_parent_id IS NOT NULL OR local_item_id = {}) AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2",
            ItemKind::Directory.as_str(),
            LocalItemId::ROOT.as_i64()
        );
        let mut directories = Vec::new();
        let mut after = after;
        // A row with a local place whose directories above do not show it has no path, and is passed over: the ids are read on, a bounded number at a time, until `limit` directories have a path or no id is left.
        while directories.len() < limit {
            let ids = {
                let mut statement = self.conn().prepare_cached(&sql)?;
                let ids = statement.query_map(params![after.map_or(0, LocalItemId::as_i64), limit as i64], |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
                ids
            };
            let Some(last) = ids.last() else { break };
            after = Some(LocalItemId::new(*last));
            let is_last_read = ids.len() < limit;
            for id in ids.into_iter().map(LocalItemId::new) {
                // Never more than `limit`: the caller goes on after the last one answered.
                if directories.len() == limit {
                    break;
                }
                if let Some(path) = find_path(self.conn(), id)? {
                    directories.push((id, path));
                }
            }
            if is_last_read {
                break;
            }
        }
        Ok(directories)
    }

    /// The files the folder shows below `directory`, at any depth, by local item id, after
    /// the item `after`, at most `limit`. Called by free-up of a directory, by a pin's
    /// download of what it holds and by the count of what is downloaded below a path. It
    /// follows the local places, never OneDrive's. Each call walks all that is below the
    /// directory: one that reads it in pages walks it once a page. MISSING-2.b.
    pub fn list_shown_files_below(&self, directory: LocalItemId, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        let sql = format!(
            "WITH RECURSIVE below(id) AS (
                 SELECT local_item_id FROM items INDEXED BY items_local_place WHERE local_parent_id = ?1
                 UNION
                 SELECT child.local_item_id FROM below JOIN items child INDEXED BY items_local_place ON child.local_parent_id = below.id)
             SELECT {ITEM_COLUMNS} FROM items WHERE local_item_id IN (SELECT id FROM below) AND kind = '{}' AND local_item_id > ?2
             ORDER BY local_item_id LIMIT ?3",
            ItemKind::File.as_str()
        );
        self.list_items(&sql, params![directory.as_i64(), after.map_or(0, LocalItemId::as_i64), limit as i64])
    }

    /// How many items OneDrive has, the folder shows, and the folder cannot show. Called
    /// after a listing's page is written and after a step's commit, for the folder's
    /// counts on the bus. Three counts, each over an index; `cannot_be_shown` walks the
    /// rows below each item whose own placement says so, each item once. MISSING-2.d, 2.e.
    pub fn count_items(&self) -> Result<ItemCounts, TreeError> {
        let count = |sql: &str| -> Result<u64, TreeError> { Ok(self.conn().prepare_cached(sql)?.query_row([], |row| row.get::<_, i64>(0))?.max(0) as u64) };
        let cannot_be_shown = format!(
            "WITH RECURSIVE hidden(id) AS (
                 SELECT local_item_id FROM items INDEXED BY items_not_placed WHERE {NOT_PLACED} AND onedrive_parent_id IS NOT NULL
                 UNION
                 SELECT child.local_item_id FROM hidden JOIN items child INDEXED BY items_onedrive_place ON child.onedrive_parent_id = hidden.id)
             SELECT COUNT(*) FROM hidden"
        );
        Ok(ItemCounts {
            in_onedrive: count("SELECT COUNT(*) FROM items INDEXED BY items_onedrive_place WHERE onedrive_parent_id IS NOT NULL")?,
            shown: item_row::count_shown_items(self.conn())?,
            cannot_be_shown: count(&cannot_be_shown)?,
            pinned: self.count_pins()?,
        })
    }

    /// The items OneDrive has that the folder cannot show by their own placement — the
    /// top item of each such part, not what is below it — by local item id, after `after`,
    /// at most `limit`, each with its path in OneDrive and the reason: `Folder.Skipped()`.
    /// Whether one of them is still on this computer, and what keeps it, is the plan's
    /// answer for the item, which the caller adds. An item is a top one when no directory
    /// above it in OneDrive, as far as the rows go, cannot be shown by itself; its path is
    /// as far as the rows go too, while a listing has not said the rest. MISSING-2.e.
    pub fn list_items_not_shown(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemNotShown>, TreeError> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM items INDEXED BY items_not_placed WHERE {NOT_PLACED} AND onedrive_parent_id IS NOT NULL AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2");
        let mut not_shown = Vec::new();
        let mut after = after;
        // The rows that cannot be shown by themselves, a bounded number at a time, until
        // enough of them are the top of their part.
        while not_shown.len() < limit {
            let rows = self.list_items(&sql, params![after.map_or(0, LocalItemId::as_i64), limit as i64])?;
            let Some(last) = rows.last() else { break };
            after = Some(last.local_item_id);
            let is_last_read = rows.len() < limit;
            for row in rows {
                if let Some(top) = self.describe_top_item_not_shown(row)? {
                    not_shown.push(top);
                }
            }
            if is_last_read {
                break;
            }
        }
        not_shown.truncate(limit);
        Ok(not_shown)
    }

    /// How many items are unsent and not left here, and the bytes their changed contents
    /// have: one sum over the partial index `items_unsent`, no row read whole. Called for
    /// `PendingCount`/`PendingBytes`, for the text of a switch to read-only, and by Forget
    /// from a store that is closed (through a reader opened for the question).
    /// MISSING-2.g.
    pub fn count_unsent(&self) -> Result<UnsentTotal, TreeError> {
        let is_content_changed = CONTENT_COLUMNS.map(|column| format!("local_{column} IS NOT synced_{column}")).join(" OR ");
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN local_size IS NOT NULL AND ({is_content_changed}) THEN local_size ELSE 0 END), 0)
             FROM items INDEXED BY items_unsent WHERE unsent_since IS NOT NULL AND NOT {LEFT_HERE}"
        );
        let (count, bytes): (i64, i64) = self.conn().prepare_cached(&sql)?.query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(UnsentTotal { count: count.max(0) as u64, bytes: bytes.max(0) as u64 })
    }

    /// The items that are left here — no binding, and an ignored name of their own or of a
    /// directory above — by local item id, after `after`, at most `limit`.
    /// No member of the bus calls it: `NotUploaded()` lists what is kept back, as today,
    /// and what is left here is listed nowhere (the map's UNSURE-6.f). It is for
    /// `konedrivectl`'s diagnosis.
    pub fn list_items_left_here(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE {LEFT_HERE} AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2");
        self.list_items(&sql, params![after.map_or(0, LocalItemId::as_i64), limit as i64])
    }

    /// The path the two sides last agreed on for `item`, relative to the root: the name of
    /// its synced place, below the path of that directory as the folder shows it, or as it
    /// was agreed on where the directory is not shown either. `None` when nothing was
    /// agreed on, for the root, and when a directory it was agreed to be in is neither
    /// shown nor agreed to be anywhere. Called for the path of a removal in `UploadQueue.Changes()` and in the
    /// activity log: [`find_path`](Self::find_path) walks local places, and a removed item
    /// has none. MISSING-6.d.
    pub fn find_synced_path(&self, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        let mut names = Vec::new();
        let mut at = item;
        // One row for each directory above the item that the folder does not show.
        for _ in 0..=MAX_CHAIN {
            let Some(place) = item_row::find_item(self.conn(), at)?.and_then(|row| row.synced_place) else { return Ok(None) };
            names.push(place.name);
            at = place.parent_local_item_id;
            if let Some(directory) = find_path(self.conn(), at)? {
                return Ok(Some(directory.join(names.iter().rev().collect::<PathBuf>())));
            }
        }
        Err(refuse(format!("the directories item {item} was agreed to be below do not end at one the folder shows")))
    }

    /// Whether a full listing was asked for and has not begun
    /// ([`ItemStore::request_full_listing`](super::ItemStore::request_full_listing)).
    /// Called by the listing cycle when it decides how to begin. MISSING-1.d.
    pub fn is_full_listing_requested(&self) -> Result<bool, TreeError> {
        super::full_listing_request::is_full_listing_requested(self.conn())
    }
}

/// An item by its path, for the bus, whose every member of `org.konedrive.Files` takes one.
impl ItemReader<'_> {
    /// The item the folder shows at `path`, relative to the root: one read of the index by
    /// parent and name per name of the path, in this reader's one transaction. `None` for
    /// a path the folder does not show, for one that goes through a file, and for one
    /// that is not relative to the root or names `..`; the empty path is the root. Called by
    /// `fuse::ItemFilesInterface` and by the queue's answers; nothing else walks a path.
    pub fn find_item_by_path(&self, path: &Path) -> Result<Option<Item>, TreeError> {
        let Some(mut found) = item_row::find_item(self.conn(), LocalItemId::ROOT)? else { return Ok(None) };
        for component in path.components() {
            let name = match component {
                Component::CurDir => continue,
                Component::Normal(name) => name,
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => return Ok(None),
            };
            if found.kind != ItemKind::Directory {
                return Ok(None);
            }
            let Some(child) = item_row::find_item_at(self.conn(), found.local_item_id, name)? else { return Ok(None) };
            found = child;
        }
        Ok(Some(found))
    }

    /// The path of what the folder shows as `item`, relative to the root, and a mark for
    /// each name of that path, the outermost first: the item id of the binding of the
    /// directory of that name when it has one — what a directory made in the backing
    /// directory above a place is marked with. As many marks as the path has names: the
    /// last is the item's own when it is a directory, and none when it is a file, whose
    /// name no directory is made under (`mount::place_changes`, `DirectoryMarks`). The
    /// empty path and no mark for the root. `None` for an item the folder does not show
    /// and for an id with no row. One read per directory above,
    /// in this reader's one transaction. Called by the write operations' read of their
    /// case, by the steps towards the folder and by a landing, each before it changes a
    /// place (`mount::place_changes`).
    pub fn find_path_with_marks(&self, item: LocalItemId) -> Result<Option<(PathBuf, Vec<Option<OneDriveItemId>>)>, TreeError> {
        let mut names_and_marks = Vec::new();
        let mut at = item;
        // One row for each directory above the item.
        for _ in 0..=MAX_CHAIN {
            let Some(row) = item_row::find_item(self.conn(), at)? else { return Ok(None) };
            let Some(local) = row.local else { return Ok(None) };
            let Some(place) = local.place else {
                let (names, marks): (Vec<_>, Vec<_>) = names_and_marks.into_iter().rev().unzip();
                return Ok(Some((names.iter().collect(), marks)));
            };
            let mark = row.onedrive_item.filter(|_| row.kind == ItemKind::Directory).map(|binding| binding.item_id);
            names_and_marks.push((place.name, mark));
            at = place.parent_local_item_id;
        }
        Err(refuse(format!("the directories above item {item} do not end at the root")))
    }
}

#[cfg(test)]
mod tests;

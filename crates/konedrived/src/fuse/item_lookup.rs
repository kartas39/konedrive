//! What the kernel's reading requests are answered from: the rows, read on the asking
//! thread's own connection.
//!
//! **State owned:** none; it holds the parts it reads through. It replaces, for reading,
//! the node table, the node resolver, the unsent changes and the directory contents
//! (design §11): a node is a row, and the row's place is the only place.
//!
//! **Answers for:** lookup, getattr, the directory readings and the extended attributes
//! (design §9, first row): one row by parent and name, or by local item id; the local
//! version alone says what the folder contains, with §4's three named exceptions. A
//! request that carries an open handle whose row is gone is answered from the handle's
//! file (design §3): getattr takes the `fh`.
//!
//! The rows of an answer are read in ONE read closure (`ReadConnections::read`), so one
//! read transaction; what it then stats in the backing directory it stats through
//! `PlaceChanges::stat_at_row_path`, a second closure, which never sleeps: a lookup and a
//! getattr wait for no place change, no listing's page and no step's commit. A stat that a
//! change crossed twice answers `RowPathStat::Crossed`: no blocks for that one answer.
//!
//! **Tested without the rest**, over the fixture (`item_fixture`):
// test: a_name_is_looked_up_by_one_read_of_the_local_place
// test: a_name_that_is_not_utf8_is_looked_up_as_it_was_given
// test: a_node_whose_row_is_gone_is_stale
// test: a_node_whose_row_has_no_local_version_is_stale
// test: a_node_whose_row_is_gone_is_answered_from_the_file_of_the_handle_the_request_carries
// test: a_row_at_a_name_of_the_daemons_own_is_not_shown
// test: a_directory_lists_what_the_rows_show_in_it_and_reads_no_disk
// test: the_attributes_of_a_file_open_for_writing_are_read_from_its_writers_file
// test: the_state_attribute_is_the_rows_sync_state_and_whether_it_is_downloading
// test: the_item_id_attribute_is_the_bindings_and_absent_without_one
// test: the_sync_mark_is_read_from_the_row_the_running_steps_and_the_plans_answer
// test: each_case_of_a_node_row_has_its_errno
// test: a_lookup_in_a_node_that_is_no_shown_directory_has_the_directorys_errno
// test: a_lookup_while_a_place_change_is_under_way_does_not_wait_and_answers_no_blocks
// test: a_lookup_racing_renames_never_answers_the_blocks_of_another_items_file
// test: a_directory_has_the_time_of_the_backing_directory_when_it_is_on_disk_else_the_mounts_start
// test: a_file_on_disk_has_the_blocks_of_its_backing_file_and_one_not_on_disk_none
// test: a_directory_lists_neither_hidden_names_nor_what_the_folder_does_not_show_and_lists_ignored_names
// test: the_folders_own_attributes_are_the_roots_alone
// test: the_pin_attribute_is_the_rows_pin
// test: the_attributes_a_node_has_are_named_by_one_read
// test: the_sync_mark_per_case
// test: the_state_attribute_per_case
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use fuser::{Errno, FileAttr};
use konedrive_fs::placeholder::{State, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};
use konedrive_tree::{ContentPresence, FileSyncState, Item, ItemKind, LocalItemId, TreeError, WaitReason};

use super::access_mode_switch::AccessModeSwitch;
use super::backing_directory::{BackingDirectory, EntryKind};
use super::item_attributes::{to_item_attributes, to_open_file_attributes, to_vanishing_attributes, BackingFileFacts, OpenFileFacts, SizeAndTimes};
use super::item_handles::OpenItemFile;
use super::place_changes::RowPathStat;
use crate::folder::disk::is_hidden_name;
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::item_handles::Fh;
use super::place_changes::PlaceChanges;
use super::read_connections::ReadConnections;
use super::upload_rules::UploadRules;
use crate::folder::published_plan::{ItemAnswer, PublishedPlan};
use crate::folder::running_steps::RunningSteps;

/// How the path of an item's local version is read: on the connection of the calling
/// thread of the mount, or through the writer, for the write-operation thread.
pub(super) type FindPath<'a> = &'a dyn Fn(LocalItemId) -> Result<Option<PathBuf>, TreeError>;

/// What the rows say of a node id the kernel asks by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NodeRow {
    /// The folder shows the item.
    Shown(Item),
    /// The row is there and has no local version: removed here and not sent yet, or an
    /// item of OneDrive the folder does not show.
    NotShown,
    /// No row has the id.
    Gone,
}

/// The errno of a request by a node id whose row is `row`; `None` for a shown item. Pure.
/// `ESTALE` for a row that is gone (design §3) and for one with no local version: to the
/// kernel the node is gone either way. (Today such a node is `ENOENT`: the map's E9.)
pub(super) fn decide_node_errno(row: &NodeRow) -> Option<Errno> {
    match row {
        NodeRow::Shown(_) => None,
        NodeRow::NotShown | NodeRow::Gone => Some(Errno::ESTALE),
    }
}

/// The errno of a lookup in a node whose row is `row`, before any name is looked at;
/// `None` for a directory the folder shows. The node's own errno ([`decide_node_errno`]),
/// and `ENOTDIR` for a file. Pure.
pub(super) fn decide_directory_errno(row: &NodeRow) -> Option<Errno> {
    match row {
        NodeRow::Shown(item) if item.kind == ItemKind::File => Some(Errno::ENOTDIR),
        row => decide_node_errno(row),
    }
}

/// What a row says of the folder's view of it.
pub(super) fn to_node_row(item: Option<Item>) -> NodeRow {
    match item {
        Some(item) if item.local.is_some() => NodeRow::Shown(item),
        Some(_) => NodeRow::NotShown,
        None => NodeRow::Gone,
    }
}

/// An error of the store under a reading request: logged, and `EIO` to the program.
fn to_store_errno(error: TreeError) -> Errno {
    tracing::warn!("the rows could not be read for a request of the kernel: {error}");
    Errno::EIO
}

/// What `fstat` says of the file a handle holds.
fn read_open_file_facts(file: &File) -> std::io::Result<OpenFileFacts> {
    let metadata = file.metadata()?;
    Ok(OpenFileFacts { size: metadata.size(), mtime: metadata.mtime(), mtime_nsec: metadata.mtime_nsec() as u32, blocks: metadata.blocks() })
}

/// What the rows and the daemon say of an item beside its row, for its extended
/// attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct XattrFacts {
    /// A pin holds for it: its own, or that of a directory above it.
    pub(super) is_pinned: bool,
    /// It is left here: its name, or that of a directory above it, is ignored.
    pub(super) is_ignored: bool,
    /// Its whole content is being downloaded.
    pub(super) is_downloading: bool,
    /// A step runs for it.
    pub(super) has_running_step: bool,
    /// The plan's last answer for it.
    pub(super) answer: Option<ItemAnswer>,
}

/// The names of the attributes an item may have, in the order `listxattr` names them.
const ITEM_XATTR_NAMES: [&str; 4] = [XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN, XATTR_SYNC];
/// The names of the folder's own attributes: on the root alone, read from the backing
/// directory, where the folder's registration wrote them.
const FOLDER_XATTR_NAMES: [&str; 2] = [XATTR_ROOT, XATTR_DRIVE];

/// The value of the attribute `name` of `item`, which the folder shows; `None` for an
/// attribute the item has not. Pure.
///
/// - its OneDrive item id: an item with a binding has it;
/// - its state: a file with a binding has it ([`decide_state_attribute`]);
/// - its pin: `1` on what a pin holds for, absent on everything else;
/// - its sync mark: [`decide_sync_mark`].
pub(super) fn decide_xattr_value(name: &str, item: &Item, facts: &XattrFacts) -> Option<Vec<u8>> {
    let value: &str = match name {
        XATTR_ITEM_ID => item.onedrive_item.as_ref()?.item_id.as_str(),
        XATTR_STATE => decide_state_attribute(item, facts.is_downloading)?.as_str(),
        XATTR_PIN if facts.is_pinned => "1",
        XATTR_SYNC => decide_sync_mark(item, facts.is_ignored, facts.has_running_step, facts.answer.as_ref())?.as_str(),
        _ => return None,
    };
    Some(value.as_bytes().to_vec())
}

/// What `user.konedrive.state` says of `item`: `None` for what is no file of OneDrive (a
/// directory, a file with no binding, an item the folder does not show); `hydrating`
/// while its whole content is being downloaded; else `online-only` for content that is
/// not on disk and `hydrated` for content that is. Pure: of `Item::file_sync_state`.
pub(super) fn decide_state_attribute(item: &Item, is_downloading: bool) -> Option<State> {
    match item.file_sync_state()? {
        FileSyncState::LocalOnly => None,
        _ if is_downloading => Some(State::Hydrating),
        FileSyncState::Synced | FileSyncState::ModifiedHere => Some(State::Hydrated),
        FileSyncState::RemoteOnly => Some(State::OnlineOnly),
    }
}

/// An item the folder shows, with the attributes it is answered with.
#[derive(Debug, Clone)]
pub(super) struct ShownEntry {
    pub(super) item: Item,
    pub(super) attributes: FileAttr,
}

/// The answers to the kernel's reading requests.
pub(super) struct ItemLookup {
    read_connections: Arc<ReadConnections>,
    place_changes: Arc<PlaceChanges>,
    item_handles: Arc<ItemHandles>,
    download_states: Arc<ItemDownloadStates>,
    access_mode_switch: Arc<AccessModeSwitch>,
    upload_rules: Arc<UploadRules>,
    /// Which steps run: the sync mark's `uploading`.
    running_steps: Arc<RunningSteps>,
    /// The plan's last answers: the sync mark's `blocked`.
    published_plan: Arc<PublishedPlan>,
    /// The folder's own attributes (`user.konedrive.root`, `user.konedrive.drive`) are
    /// read from it, and nothing else: every entry is reached through `place_changes`.
    backing_directory: Arc<BackingDirectory>,
    /// Unix seconds: the time of a directory that has none of its own.
    mount_started_at: i64,
}

impl ItemLookup {
    /// Made by the mount's build.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        read_connections: Arc<ReadConnections>,
        place_changes: Arc<PlaceChanges>,
        item_handles: Arc<ItemHandles>,
        download_states: Arc<ItemDownloadStates>,
        access_mode_switch: Arc<AccessModeSwitch>,
        upload_rules: Arc<UploadRules>,
        running_steps: Arc<RunningSteps>,
        published_plan: Arc<PublishedPlan>,
        backing_directory: Arc<BackingDirectory>,
        mount_started_at: i64,
    ) -> Self {
        Self { read_connections, place_changes, item_handles, download_states, access_mode_switch, upload_rules, running_steps, published_plan, backing_directory, mount_started_at }
    }

    /// The row of `node`, as the folder shows it or not. Calls `ItemReader::find_item` on
    /// the thread's connection. An error of the store is `EIO`.
    pub(super) fn find_node_row(&self, node: LocalItemId) -> Result<NodeRow, Errno> {
        self.read_connections.read(|reader| reader.find_item(node)).map(to_node_row).map_err(to_store_errno)
    }

    /// The item the folder shows as `node`, or the errno [`decide_node_errno`] gives.
    pub(super) fn find_shown_item(&self, node: LocalItemId) -> Result<Item, Errno> {
        match self.find_node_row(node)? {
            NodeRow::Shown(item) => Ok(item),
            row => Err(decide_node_errno(&row).unwrap_or(Errno::ESTALE)),
        }
    }

    /// Lookup: the entry under `name` in `directory`. Calls `ItemReader::find_item_at`
    /// (one read of the index by parent and name), then [`describe`](Self::describe).
    /// `ENOENT` for no such name and for a name the mount hides
    /// (`folder::disk::is_hidden_name`: an entry no row says, a row the check at start set
    /// aside, a temporary name of an exchange); the directory's own errno when its row
    /// is not a shown directory. Design §9.
    ///
    /// The directory's row and the name's are read in one read closure: of one moment.
    pub(super) fn find_entry(&self, directory: LocalItemId, name: &OsStr) -> Result<ShownEntry, Errno> {
        let (directory_row, item) = self
            .read_connections
            .read(|reader| {
                let directory_row = to_node_row(reader.find_item(directory)?);
                let item = match decide_directory_errno(&directory_row) {
                    None if !is_hidden_name(name) => reader.find_item_at(directory, name)?,
                    None | Some(_) => None,
                };
                Ok((directory_row, item))
            })
            .map_err(to_store_errno)?;
        if let Some(errno) = decide_directory_errno(&directory_row) {
            return Err(errno);
        }
        self.describe(item.ok_or(Errno::ENOENT)?)
    }

    /// The path of the local version of `node`, on the calling thread's connection: how a
    /// thread of the mount finds a backing file.
    fn find_path(&self, node: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        self.read_connections.read(|reader| reader.find_path(node))
    }

    /// Getattr: the attributes of `node`. A shown item: [`describe`](Self::describe). A
    /// row that is gone or not shown: when the request carries a handle with a file
    /// (`fh`: a writer's, a reader's backing file), the attributes of that file by
    /// `fstat`, under the node id asked; otherwise `ESTALE`.
    pub(super) fn find_attributes(&self, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        self.describe_node(self.find_node_row(node)?, node, fh, &|node| self.find_path(node))
    }

    /// The attributes getattr answers for `node`, whose row was read as `row`, with the
    /// path of a backing file found by `find_path`
    /// ([`find_attributes`](Self::find_attributes) reads both on the connection of a
    /// thread of the mount). For the write-operation thread, which answers a truncation
    /// and a time set "as getattr answers" and reads its rows through the writer: it has
    /// no connection here, and must see the row it wrote a moment ago.
    pub(super) fn describe_node(&self, row: NodeRow, node: LocalItemId, fh: Option<Fh>, find_path: FindPath<'_>) -> Result<FileAttr, Errno> {
        let row = match row {
            NodeRow::Shown(item) => return Ok(self.describe_through(item, find_path)?.attributes),
            row => row,
        };
        let errno = decide_node_errno(&row).unwrap_or(Errno::ESTALE);
        // The handle's file, when the handle is one of this node's and has a file.
        let backing_file = match fh.and_then(|fh| self.item_handles.find_file(fh)) {
            Some(OpenItemFile::Writing { item, backing_file } | OpenItemFile::ReadingBackingFile { item, backing_file }) if item == node => backing_file,
            Some(_) | None => return Err(errno),
        };
        let facts = read_open_file_facts(&backing_file).map_err(|error| super::errno::to_errno(&error))?;
        Ok(to_open_file_attributes(node, self.access_mode_switch.access_mode(), &facts))
    }

    /// Readdir and readdirplus: what the folder shows in `directory`, by name, the
    /// names the mount hides left out. Calls `ItemReader::list_children`; the backing
    /// directory is not read (design §4). A node that is no directory lists nothing, as
    /// today.
    /// The directory's row and its entries are read in one read closure. A node whose
    /// row is gone or shows nothing has the node's errno.
    pub(super) fn list_entries(&self, directory: LocalItemId) -> Result<Vec<Item>, Errno> {
        let (directory_row, mut listed) = self
            .read_connections
            .read(|reader| {
                let directory_row = to_node_row(reader.find_item(directory)?);
                let listed = match &directory_row {
                    NodeRow::Shown(item) if item.kind == ItemKind::Directory => reader.list_children(directory)?,
                    NodeRow::Shown(_) | NodeRow::NotShown | NodeRow::Gone => Vec::new(),
                };
                Ok((directory_row, listed))
            })
            .map_err(to_store_errno)?;
        if let Some(errno) = decide_node_errno(&directory_row) {
            return Err(errno);
        }
        listed.retain(|item| item.local.as_ref().and_then(|local| local.place.as_ref()).is_some_and(|place| !is_hidden_name(&place.name)));
        Ok(listed)
    }

    /// What the backing directory says of `item`, where the design lets it speak: the
    /// writer's file for size and times when `ItemHandles::find_writer_file` has one (an
    /// `fstat` of the handle, no path); the blocks of a file whose content is on disk and
    /// the time of a directory, by one `stat` through `PlaceChanges::stat_at_row_path`
    /// with `ItemReader::find_path` (design §4, §15). Nothing is read for a file that is
    /// not on disk. Each answer of the stat is told apart (`place_changes::RowPathStat`):
    /// `Crossed` and `NoRow` answer no blocks; so does `NothingOnDisk` — a file the row
    /// says is on disk and the disk has not: the open says so, not the attributes.
    ///
    /// The blocks of a file open for writing are its writer's file's too, by the same
    /// `fstat`: no path is looked at for it. An error of the store or of the `fstat` is
    /// logged and answers no facts: the attributes are then the row's.
    pub(super) fn read_backing_file_facts(&self, item: &Item) -> BackingFileFacts {
        self.read_backing_file_facts_through(item, &|node| self.find_path(node))
    }

    /// [`read_backing_file_facts`](Self::read_backing_file_facts), with the path of the
    /// item's local version found by `find_path`: the one reading of these facts, for a
    /// thread of the mount and for the write-operation thread alike.
    pub(super) fn read_backing_file_facts_through(&self, item: &Item, find_path: FindPath<'_>) -> BackingFileFacts {
        let no_facts = BackingFileFacts { size_and_times: SizeAndTimes::OfLocalContent, blocks: 0, directory_mtime: None };
        if item.local.is_none() {
            return no_facts;
        }
        let node = item.local_item_id;
        if item.kind == ItemKind::File {
            if let Some(writer_file) = self.item_handles.find_writer_file(node) {
                match read_open_file_facts(&writer_file) {
                    Ok(facts) => return BackingFileFacts { size_and_times: SizeAndTimes::OfWritersFile { size: facts.size, mtime: facts.mtime, mtime_nsec: facts.mtime_nsec }, blocks: facts.blocks, directory_mtime: None },
                    Err(error) => tracing::warn!("the file a program writes item {node} into could not be asked for its size: {error}"),
                }
            }
            if item.content_presence == ContentPresence::NotOnDisk {
                return no_facts;
            }
        }
        let stat = self.place_changes.stat_at_row_path(|| Ok(find_path(node)?.map(|path| (path, ()))));
        match (stat, item.kind) {
            (Ok(RowPathStat::Facts { facts, row: () }), ItemKind::File) if facts.kind == EntryKind::File => BackingFileFacts { blocks: facts.blocks, ..no_facts },
            (Ok(RowPathStat::Facts { facts, row: () }), ItemKind::Directory) if facts.kind == EntryKind::Directory => {
                let modified = facts.modified.duration_since(UNIX_EPOCH).unwrap_or_default();
                BackingFileFacts { directory_mtime: Some((modified.as_secs() as i64, modified.subsec_nanos())), ..no_facts }
            }
            // An entry of the other kind is not the item's: the check at start says so.
            (Ok(RowPathStat::Facts { .. } | RowPathStat::NoRow | RowPathStat::Crossed { .. } | RowPathStat::NothingOnDisk { .. }), _) => no_facts,
            (Err(error), _) => {
                tracing::warn!("the path of item {node} could not be read for its attributes: {error}");
                no_facts
            }
        }
    }

    /// `item` with its attributes: [`read_backing_file_facts`](Self::read_backing_file_facts),
    /// the access mode, and `item_attributes::to_item_attributes`. Called for lookup,
    /// getattr and each name of a readdirplus.
    /// `ESTALE` for an item the folder does not show.
    pub(super) fn describe(&self, item: Item) -> Result<ShownEntry, Errno> {
        self.describe_through(item, &|node| self.find_path(node))
    }

    /// [`describe`](Self::describe), with the path of the item's local version found by
    /// `find_path`: for the reply of a write operation, made on the write-operation
    /// thread.
    pub(super) fn describe_through(&self, item: Item, find_path: FindPath<'_>) -> Result<ShownEntry, Errno> {
        let facts = self.read_backing_file_facts_through(&item, find_path);
        let attributes = to_item_attributes(&item, self.access_mode_switch.access_mode(), &facts, self.mount_started_at).ok_or(Errno::ESTALE)?;
        Ok(ShownEntry { item, attributes })
    }

    /// The attributes the bridge's own lookup is answered for a name that is vanishing
    /// (`item_announcements::AnnouncedName::Vanishing`), in the folder's access mode. No
    /// row is read: the rows do not show the item at that name any more.
    pub(super) fn describe_vanishing(&self, item: LocalItemId, kind: ItemKind) -> FileAttr {
        to_vanishing_attributes(item, kind, self.access_mode_switch.access_mode(), self.mount_started_at)
    }

    /// The shown item `node` and what its attributes named `wanted` are decided from (all
    /// of them for `None`), the row's part in one read closure. What a name does not need
    /// is not read: the pin for the pin alone, and whether the item is left here for the
    /// sync mark of an item that is unsent and has no binding (an item OneDrive has is
    /// never left here).
    fn read_xattr_facts(&self, node: LocalItemId, wanted: Option<&str>) -> Result<(Item, XattrFacts), Errno> {
        let wants = |name: &str| wanted.is_none_or(|wanted| wanted == name);
        let (row, is_pinned, is_ignored) = self
            .read_connections
            .read(|reader| {
                let row = to_node_row(reader.find_item(node)?);
                let NodeRow::Shown(item) = &row else { return Ok((row, false, false)) };
                let is_pinned = wants(XATTR_PIN) && reader.is_pinned(node)?;
                let is_ignored = wants(XATTR_SYNC) && item.unsent_since.is_some() && item.onedrive_item.is_none() && reader.is_ignored(node)?;
                Ok((row, is_pinned, is_ignored))
            })
            .map_err(to_store_errno)?;
        let item = match row {
            NodeRow::Shown(item) => item,
            row => return Err(decide_node_errno(&row).unwrap_or(Errno::ESTALE)),
        };
        let is_unsent = wants(XATTR_SYNC) && item.unsent_since.is_some();
        let facts = XattrFacts {
            is_pinned,
            is_ignored,
            is_downloading: wants(XATTR_STATE) && self.download_states.is_downloading(node),
            has_running_step: is_unsent && self.running_steps.has_running_step(node),
            answer: if is_unsent { self.published_plan.find_answer(node) } else { None },
        };
        Ok((item, facts))
    }

    /// Getxattr: the value of the attribute `name` of `node`: the folder's own on the
    /// root; the binding's item id; the state, from `Item::file_sync_state` and
    /// `ItemDownloadStates::is_downloading`; the pin, from `ItemReader::is_pinned`; the
    /// sync mark, from [`decide_sync_mark`] with `RunningSteps::has_running_step` and
    /// `PublishedPlan::find_answer`. `ENODATA` for an attribute the item has not.
    /// `ESTALE` for a node whose row is gone or shows nothing.
    pub(super) fn find_xattr(&self, node: LocalItemId, name: &OsStr) -> Result<Vec<u8>, Errno> {
        let name = name.to_str().ok_or(Errno::ENODATA)?;
        if node == LocalItemId::ROOT && FOLDER_XATTR_NAMES.contains(&name) {
            return self.backing_directory.read_folder_attribute(name).map(String::into_bytes).ok_or(Errno::ENODATA);
        }
        if !ITEM_XATTR_NAMES.contains(&name) {
            return Err(Errno::ENODATA);
        }
        let (item, facts) = self.read_xattr_facts(node, Some(name))?;
        decide_xattr_value(name, &item, &facts).ok_or(Errno::ENODATA)
    }

    /// Listxattr: the names of the attributes `node` has. One read of the row.
    pub(super) fn list_xattr_names(&self, node: LocalItemId) -> Result<Vec<&'static str>, Errno> {
        let (item, facts) = self.read_xattr_facts(node, None)?;
        let of_folder = FOLDER_XATTR_NAMES.into_iter().filter(|name| node == LocalItemId::ROOT && self.backing_directory.read_folder_attribute(name).is_some());
        Ok(of_folder.chain(ITEM_XATTR_NAMES.into_iter().filter(|name| decide_xattr_value(name, &item, &facts).is_some())).collect())
    }
}

/// What `user.konedrive.sync` says of a file: what Dolphin's emblem reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncMark {
    /// Unsent, and nothing runs for it.
    Pending,
    /// A step towards OneDrive runs for it.
    Uploading,
    /// It waits for the user.
    Blocked,
}

impl SyncMark {
    /// The attribute's value, as Dolphin's emblem plugin and `konedrivectl` read it.
    // The three words are also `upload::local::SYNC_*`, which today's worker writes on
    // the backing file and which `fuse` may not name (the layer order): they go with it.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            SyncMark::Pending => "pending",
            SyncMark::Uploading => "uploading",
            SyncMark::Blocked => "blocked",
        }
    }
}

/// The sync mark of `item`, a file or a directory the folder shows: `None` for an item
/// that agrees (`unsent_since` is none) or is left here (`is_ignored`, or the plan's
/// answer leaves it out by a rule); `Uploading` while a step runs for it; `Blocked` when
/// the plan's answer is a wait for the user or a fault; else `Pending` — a wait for a
/// time, another item or a listing among it, and an item the last plan was not given.
/// Pure. The mount answers the attribute; nothing is written on disk, and design §8's
/// marks stay two (the map's MISSING-2.j).
///
/// `is_ignored` is `ItemReader::is_ignored`: the row alone says only whether the item's
/// OWN name is ignored, and the plan is not given an item that is left here, so it has
/// no answer to tell one below an ignored directory by.
pub(super) fn decide_sync_mark(item: &Item, is_ignored: bool, has_running_step: bool, answer: Option<&ItemAnswer>) -> Option<SyncMark> {
    if item.local.is_none() || item.unsent_since.is_none() || is_ignored {
        return None;
    }
    match answer {
        Some(ItemAnswer::Excluded(_)) => None,
        _ if has_running_step => Some(SyncMark::Uploading),
        Some(ItemAnswer::Waiting(WaitReason::User(_)) | ItemAnswer::Fault(_)) => Some(SyncMark::Blocked),
        Some(ItemAnswer::Planned(_) | ItemAnswer::Waiting(_)) | None => Some(SyncMark::Pending),
    }
}

#[cfg(test)]
mod tests;

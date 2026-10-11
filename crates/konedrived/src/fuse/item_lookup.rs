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
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::sync::Arc;

use fuser::{Errno, FileAttr};
use konedrive_tree::{Item, LocalItemId};

use super::access_mode_switch::AccessModeSwitch;
use super::item_attributes::BackingFileFacts;
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::open_handles::Fh;
use super::place_changes::PlaceChanges;
use super::read_connections::ReadConnections;
use super::upload_rules::UploadRules;
use crate::folder::published_plan::{ItemAnswer, PublishedPlan};
use crate::folder::running_steps::RunningSteps;

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
    todo!()
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
        mount_started_at: i64,
    ) -> Self {
        Self { read_connections, place_changes, item_handles, download_states, access_mode_switch, upload_rules, running_steps, published_plan, mount_started_at }
    }

    /// The row of `node`, as the folder shows it or not. Calls `ItemReader::find_item` on
    /// the thread's connection. An error of the store is `EIO`.
    pub(super) fn find_node_row(&self, node: LocalItemId) -> Result<NodeRow, Errno> {
        todo!()
    }

    /// The item the folder shows as `node`, or the errno [`decide_node_errno`] gives.
    pub(super) fn find_shown_item(&self, node: LocalItemId) -> Result<Item, Errno> {
        todo!()
    }

    /// Lookup: the entry under `name` in `directory`. Calls `ItemReader::find_item_at`
    /// (one read of the index by parent and name), then [`describe`](Self::describe).
    /// `ENOENT` for no such name and for a name the mount hides
    /// (`folder::disk::is_hidden_name`: an entry no row says, a row the check at start set
    /// aside, a temporary name of an exchange); the directory's own errno when its row
    /// is not a shown directory. Design §9.
    pub(super) fn find_entry(&self, directory: LocalItemId, name: &OsStr) -> Result<ShownEntry, Errno> {
        todo!()
    }

    /// Getattr: the attributes of `node`. A shown item: [`describe`](Self::describe). A
    /// row that is gone or not shown: when the request carries a handle with a file
    /// (`fh`: a writer's, a reader's backing file), the attributes of that file by
    /// `fstat`, under the node id asked; otherwise `ESTALE`.
    pub(super) fn find_attributes(&self, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        todo!()
    }

    /// Readdir and readdirplus: what the folder shows in `directory`, by name, the
    /// names the mount hides left out. Calls `ItemReader::list_children`; the backing
    /// directory is not read (design §4). A node that is no directory lists nothing, as
    /// today.
    pub(super) fn list_entries(&self, directory: LocalItemId) -> Result<Vec<Item>, Errno> {
        todo!()
    }

    /// What the backing directory says of `item`, where the design lets it speak: the
    /// writer's file for size and times when `ItemHandles::find_writer_file` has one (an
    /// `fstat` of the handle, no path); the blocks of a file whose content is on disk and
    /// the time of a directory, by one `stat` through `PlaceChanges::stat_at_row_path`
    /// with `ItemReader::find_path` (design §4, §15). Nothing is read for a file that is
    /// not on disk. Each answer of the stat is told apart (`place_changes::RowPathStat`):
    /// `Crossed` and `NoRow` answer no blocks; so does `NothingOnDisk` — a file the row
    /// says is on disk and the disk has not: the open says so, not the attributes.
    pub(super) fn read_backing_file_facts(&self, item: &Item) -> BackingFileFacts {
        todo!()
    }

    /// `item` with its attributes: [`read_backing_file_facts`](Self::read_backing_file_facts),
    /// the access mode, and `item_attributes::to_item_attributes`. Called for lookup,
    /// getattr and each name of a readdirplus.
    pub(super) fn describe(&self, item: Item) -> Result<ShownEntry, Errno> {
        todo!()
    }

    /// Getxattr: the value of the attribute `name` of `node`: the folder's own on the
    /// root; the binding's item id; the state, from `Item::file_sync_state` and
    /// `ItemDownloadStates::is_downloading`; the pin, from `ItemReader::is_pinned`; the
    /// sync mark, from [`decide_sync_mark`] with `RunningSteps::has_running_step` and
    /// `PublishedPlan::find_answer`. `ENODATA` for an attribute the item has not.
    pub(super) fn find_xattr(&self, node: LocalItemId, name: &OsStr) -> Result<Vec<u8>, Errno> {
        todo!()
    }

    /// Listxattr: the names of the attributes `node` has. One read of the row.
    pub(super) fn list_xattr_names(&self, node: LocalItemId) -> Result<Vec<&'static str>, Errno> {
        todo!()
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

/// The sync mark of `item`: `None` for an item that agrees or is left here; `Uploading`
/// while a step runs for it; `Blocked` when the plan's answer is a wait for the user or a
/// fault; else `Pending`. Pure. The mount answers the attribute; nothing is written on
/// disk, and design §8's marks stay two (the map's MISSING-2.j).
pub(super) fn decide_sync_mark(item: &Item, has_running_step: bool, answer: Option<&ItemAnswer>) -> Option<SyncMark> {
    todo!()
}

//! The attributes the kernel is given for an item: a function of its row, the access mode
//! and what the backing file says where the design lets it speak.
//!
//! **State owned:** none.
//!
//! **Answers for:** design §4: names, places and attributes are the local version's, with
//! two exceptions that are the backing file's — the blocks a file takes on disk, always,
//! and the size and the times of a file a program holds open for writing. And design §3:
//! the kernel's node id is the local item id.
//!
//! **Tested without the rest**, a test per case of [`SizeAndTimes`] and per kind:
// test: a_files_size_and_times_are_its_local_contents
// test: a_file_open_for_writing_has_the_size_and_times_of_its_backing_file
// test: a_files_blocks_are_its_backing_files_and_none_when_it_is_not_on_disk
// test: a_directory_has_the_times_the_decision_gives_it
// test: a_read_only_folder_shows_no_write_bit
// test: an_item_the_folder_does_not_show_has_no_attributes
// test: the_node_id_of_an_item_is_its_local_item_id_and_back
#![allow(dead_code, unused_variables)]

use fuser::{FileAttr, INodeNo};
use konedrive_tree::{Item, LocalItemId};

use super::access_mode_switch::AccessMode;

/// Where the size and the times of a file come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SizeAndTimes {
    /// The row's local content: no program holds the file open for writing.
    OfLocalContent,
    /// The backing file's, read through a writer's handle: a program holds the file open
    /// for writing, and the row keeps the size of before the open until the last close.
    OfWritersFile { size: u64, mtime: i64, mtime_nsec: u32 },
}

/// What the backing directory says of an item, where it is asked at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BackingFileFacts {
    pub(super) size_and_times: SizeAndTimes,
    /// The 512-byte blocks the backing file takes; 0 for content that is not on disk and
    /// for a directory.
    pub(super) blocks: u64,
    /// The modification time of the directory in the backing directory, when it has one.
    pub(super) directory_mtime: Option<(i64, u32)>,
}

/// The kernel's node id of `item`. The root is 1 on both sides.
pub(super) fn to_node_id(item: LocalItemId) -> INodeNo {
    todo!()
}

/// The item a node id of the kernel names. Called first in every request by a node id.
pub(super) fn to_local_item_id(node_id: INodeNo) -> LocalItemId {
    todo!()
}

/// The modification time shown for a directory.
/// The row has none and gets none (no column): the backing directory's time when the
/// directory is on disk, else the time the mount started — which changes at every start
/// for a directory of OneDrive with none on disk. Pure.
pub(super) fn decide_directory_mtime(facts: &BackingFileFacts, mount_started_at: i64) -> (i64, u32) {
    todo!()
}

/// The attributes of what the folder shows as `item`; `None` for a row with no local
/// version. Pure. Called by lookup, getattr, readdirplus, mkdir, mknod, create and
/// setattr's answer, each with the facts `ItemLookup::read_backing_file_facts` read for
/// the item. The generation is always 0, the link count a constant (design §3).
pub(super) fn to_item_attributes(item: &Item, access_mode: AccessMode, facts: &BackingFileFacts, mount_started_at: i64) -> Option<FileAttr> {
    todo!()
}

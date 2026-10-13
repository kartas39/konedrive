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
// test: a_file_with_its_executable_bit_is_shown_executable_and_a_directory_as_ever
// test: the_owners_executable_bit_is_all_of_a_mode_that_is_kept
// test: an_item_the_folder_does_not_show_has_no_attributes
// test: the_node_id_of_an_item_is_its_local_item_id_and_back
// test: a_file_whose_handle_outlived_its_row_has_the_attributes_of_the_handles_file
// test: a_name_that_is_vanishing_has_attributes_of_its_kind_and_nothing_else
// test: a_time_before_1970_is_shown_as_it_is
#![allow(dead_code, unused_variables)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use konedrive_tree::{Item, ItemKind, LocalItemId};

use super::access_mode_switch::AccessMode;
use super::backing_directory::{to_mode_bits, EntryKind, WriteBits};

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
    INodeNo(item.as_i64() as u64)
}

/// The item a node id of the kernel names. Called first in every request by a node id.
pub(super) fn to_local_item_id(node_id: INodeNo) -> LocalItemId {
    // A number no row can have (beyond `i64`) names no item: the request is stale.
    LocalItemId::new(node_id.0 as i64)
}

/// What a file's handle says of its file by `fstat`: a writer's, or the backing file of a
/// reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OpenFileFacts {
    pub(super) size: u64,
    pub(super) mtime: i64,
    pub(super) mtime_nsec: u32,
    /// In units of 512 bytes.
    pub(super) blocks: u64,
}

/// The time `seconds` and `nanoseconds` after 1970, or before it for negative seconds.
fn to_time(seconds: i64, nanoseconds: u32) -> SystemTime {
    match seconds >= 0 {
        true => UNIX_EPOCH + Duration::new(seconds as u64, nanoseconds),
        false => UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()) + Duration::from_nanos(u64::from(nanoseconds)),
    }
}

/// Whether `mode`, as a chmod or the making of a file gave it, has the owner's executable
/// bit: all of a mode that is kept (`Item::is_executable`). Pure.
pub(super) fn has_executable_bit(mode: u32) -> bool {
    mode & 0o100 != 0
}

/// The mode the folder shows for a directory or a file: the layer's own
/// (`backing_directory::to_mode_bits`), and for a file that has its owner's executable
/// bit an executable bit wherever the mode has a read bit. A directory's is as ever. Pure.
fn to_shown_mode(kind: EntryKind, write_bits: WriteBits, is_executable: bool) -> u16 {
    let mode = to_mode_bits(kind, write_bits);
    match (kind, is_executable) {
        (EntryKind::File, true) => (mode | (mode & 0o444) >> 2) as u16,
        (EntryKind::File, false) | (EntryKind::Directory, _) => mode as u16,
    }
}

/// The attributes of the node `item` of `kind`, with this size, these blocks and this
/// time. A read-only folder shows no write bit anywhere: a file manager reads that from
/// the modes before anything is tried. `is_executable` is the row's executable bit, of a
/// file alone. The link count is a constant: an item has one place (design §3).
fn make_attributes(item: LocalItemId, kind: ItemKind, access_mode: AccessMode, is_executable: bool, size: u64, blocks: u64, modified: SystemTime) -> FileAttr {
    let write_bits = match access_mode {
        AccessMode::ReadWrite => WriteBits::Present,
        AccessMode::ReadOnly => WriteBits::Removed,
    };
    let (entry_kind, file_type, nlink) = match kind {
        ItemKind::Directory => (EntryKind::Directory, FileType::Directory, 2),
        ItemKind::File => (EntryKind::File, FileType::RegularFile, 1),
    };
    // SAFETY: two system calls that take nothing and cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        ino: to_node_id(item),
        size,
        blocks,
        atime: modified,
        mtime: modified,
        ctime: modified,
        crtime: UNIX_EPOCH,
        kind: file_type,
        perm: to_shown_mode(entry_kind, write_bits, is_executable),
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// The attributes of the file a handle holds, under the node id `node` the request asked
/// by: what a getattr with an `fh` is answered once the row of `node` is gone or shows
/// nothing (design §3) — the file was removed or saved over while a program held it. Its
/// executable bit went with its row: it is shown with none. Pure.
pub(super) fn to_open_file_attributes(node: LocalItemId, access_mode: AccessMode, facts: &OpenFileFacts) -> FileAttr {
    make_attributes(node, ItemKind::File, access_mode, false, facts.size, facts.blocks, to_time(facts.mtime, facts.mtime_nsec))
}

/// The attributes the bridge's own lookup is answered for a name that is vanishing
/// (`item_announcements::AnnouncedName::Vanishing`): the item that had the name, of its
/// kind, with no size: the rows do not show it there any more, and the kernel needs the
/// node and the kind alone to tell its watchers. Pure.
pub(super) fn to_vanishing_attributes(item: LocalItemId, kind: ItemKind, access_mode: AccessMode, mount_started_at: i64) -> FileAttr {
    make_attributes(item, kind, access_mode, false, 0, 0, to_time(mount_started_at, 0))
}

/// The modification time shown for a directory.
/// The row has none and gets none (no column): the backing directory's time when the
/// directory is on disk, else the time the mount started — which changes at every start
/// for a directory of OneDrive with none on disk. Pure.
pub(super) fn decide_directory_mtime(facts: &BackingFileFacts, mount_started_at: i64) -> (i64, u32) {
    facts.directory_mtime.unwrap_or((mount_started_at, 0))
}

/// The attributes of what the folder shows as `item`; `None` for a row with no local
/// version. Pure. Called by lookup, getattr, readdirplus, mkdir, mknod, create and
/// setattr's answer, each with the facts `ItemLookup::read_backing_file_facts` read for
/// the item. The generation is always 0, the link count a constant (design §3).
pub(super) fn to_item_attributes(item: &Item, access_mode: AccessMode, facts: &BackingFileFacts, mount_started_at: i64) -> Option<FileAttr> {
    let local = item.local.as_ref()?;
    let (size, blocks, (mtime, mtime_nsec)) = match (item.kind, facts.size_and_times, &local.content) {
        (ItemKind::Directory, _, _) => (0, 0, decide_directory_mtime(facts, mount_started_at)),
        (ItemKind::File, SizeAndTimes::OfWritersFile { size, mtime, mtime_nsec }, _) => (size, facts.blocks, (mtime, mtime_nsec)),
        (ItemKind::File, SizeAndTimes::OfLocalContent, Some(content)) => (content.size(), facts.blocks, content.mtime()),
        // A file with no content is a row against the rules: shown empty, never refused.
        (ItemKind::File, SizeAndTimes::OfLocalContent, None) => (0, facts.blocks, (mount_started_at, 0)),
    };
    Some(make_attributes(item.local_item_id, item.kind, access_mode, item.is_executable, size, blocks, to_time(mtime, mtime_nsec)))
}

#[cfg(test)]
pub(super) mod tests;

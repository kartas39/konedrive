//! Where the bytes of a file are read from: its backing file, or OneDrive.
//!
//! **State owned:** none: the decisions of open-to-read and of read, as functions of the
//! row.
//!
//! **Answers for:** a read (`docs/design/mount.md` §5.1): content on disk comes from the
//! backing file, found by the path of the row's local version (`docs/design/mount.md`
//! §3.2) and opened through `PlaceChanges::open_at_row_path`, so that no rename comes
//! between the row and the open; content not on disk comes as ranges from OneDrive, or
//! the file is downloaded. A reader of a file that is not on disk reads ONE content: the
//! one it was opened as.
//!
//! **Tested without the rest**, a test per case:
// test: a_file_on_disk_is_opened_from_its_backing_file
// test: a_file_not_on_disk_is_opened_by_its_item_and_its_content_and_nothing_on_disk
// test: a_reader_whose_content_landed_since_reads_the_backing_file
// test: a_reader_whose_content_landed_and_which_a_writer_opened_since_reads_the_backing_file
// test: a_reader_whose_row_is_gone_is_stale
// test: a_reader_whose_item_shows_another_content_by_now_is_stale
// test: a_file_with_no_binding_and_nothing_on_disk_cannot_be_read
// test: a_range_is_asked_of_the_drive_and_the_item_the_binding_names
// test: a_directory_and_an_item_the_folder_does_not_show_are_not_opened_to_be_read

use fuser::Errno;
use konedrive_tree::{ContentPresence, ContentVersion, Item, ItemKind, LocalItemId, OneDriveItemReference};

use super::item_lookup::NodeRow;

/// How a file is opened to be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OpenToRead {
    /// Its content is on disk: the backing file at the path of its local version, opened
    /// through `PlaceChanges::open_at_row_path`.
    BackingFile,
    /// Its content is not on disk: the handle holds the item and the content it is opened
    /// as.
    NotOnDisk { item: LocalItemId, content: ContentVersion },
}

/// How `item`, a file the folder shows, is opened to be read. Pure. Called by the open
/// request's read branch.
///
/// `EISDIR` for a directory and `ESTALE` for an item the folder does not show, should one
/// be asked. `EIO` for a file that is not on disk and has no binding: nothing of it is
/// anywhere, which no operation leaves — the rows are at fault, not the program.
pub(super) fn decide_open_to_read(item: &Item) -> Result<OpenToRead, Errno> {
    let local = item.local.as_ref().ok_or(Errno::ESTALE)?;
    if item.kind == ItemKind::Directory {
        return Err(Errno::EISDIR);
    }
    match (item.content_presence, &local.content, &item.onedrive_item) {
        (ContentPresence::OnDisk, _, _) => Ok(OpenToRead::BackingFile),
        (ContentPresence::NotOnDisk, Some(content), Some(_)) => Ok(OpenToRead::NotOnDisk { item: item.local_item_id, content: content.clone() }),
        (ContentPresence::NotOnDisk, _, _) => Err(Errno::EIO),
    }
}

/// What a range of a file that is not on disk is fetched by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RangeSource {
    /// The drive and the item; the content source is asked by the item id.
    pub(crate) onedrive_item: OneDriveItemReference,
    /// The content the range is a part of: its size is the end of the file, and its cTag
    /// tells a download or a range of another version.
    pub(crate) content: ContentVersion,
}

/// Where a read through a handle that was opened not on disk gets its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NotOnDiskReadOrigin {
    /// The content landed since the open: the handle goes on from the backing file
    /// (opened through `PlaceChanges::open_at_row_path`).
    LandedSince,
    /// A range from OneDrive, or from the file of the download of this content that runs.
    OneDriveRange(RangeSource),
    /// Nothing to read from: the errno.
    Refused(Errno),
}

/// The origin of a read through a handle opened as `opened_as`, from the item's row as it
/// is now. Pure. `ESTALE` for a row that is gone or not shown (this handle has no file to
/// be answered from), and for a row whose local content is another than `opened_as`: the
/// mount shows the new size from then on, and no program gets the bytes of one content at
/// the size of another. This holds between a listing and its step (the row's local
/// content is still the old one: the old content is read by its version) and after the
/// step.
///
/// A content that landed and that a writer opened since is on disk all the same: the row's
/// content is the writer's by then, and `opened_as` is the one the row keeps from before
/// the write. The handle goes on from the backing file, as a reader that opened the file
/// on disk reads beside a writer. After a close that changed the file the row keeps
/// nothing of `opened_as`, and a handle that asks again is `ESTALE`: a handle that was
/// answered `LandedSince` once should keep its backing file and not ask again.
///
/// A row with the content opened and no binding (`EIO`) is one no operation leaves.
pub(super) fn decide_not_on_disk_read_origin(row: &NodeRow, opened_as: &ContentVersion) -> NotOnDiskReadOrigin {
    let NodeRow::Shown(item) = row else { return NotOnDiskReadOrigin::Refused(Errno::ESTALE) };
    let is_content_of_row = item.local.as_ref().and_then(|local| local.content.as_ref()) == Some(opened_as);
    let is_content_before_write = item.content_before_write.as_ref() == Some(opened_as);
    if item.content_presence == ContentPresence::OnDisk && (is_content_of_row || is_content_before_write) {
        return NotOnDiskReadOrigin::LandedSince;
    }
    if !is_content_of_row {
        return NotOnDiskReadOrigin::Refused(Errno::ESTALE);
    }
    match &item.onedrive_item {
        Some(onedrive_item) => NotOnDiskReadOrigin::OneDriveRange(RangeSource { onedrive_item: onedrive_item.clone(), content: opened_as.clone() }),
        None => NotOnDiskReadOrigin::Refused(Errno::EIO),
    }
}

#[cfg(test)]
mod tests;

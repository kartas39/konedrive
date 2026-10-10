//! Where the bytes of a file are read from: its backing file, or OneDrive.
//!
//! **State owned:** none: the decisions of open-to-read and of read, as functions of the
//! row.
//!
//! **Answers for:** design §9, "a read": content on disk comes from the backing file,
//! found by the path of the row's local version (design §8) and opened through
//! `PlaceChanges::open_at_row_path`, so that no rename comes between the row and the open;
//! content not on disk comes as ranges from OneDrive, or the file is downloaded, as
//! today. A reader of a file that is not on disk reads ONE content: the one it was
//! opened as.
//!
//! **Tested without the rest**, a test per case:
// test: a_file_on_disk_is_opened_from_its_backing_file
// test: a_file_not_on_disk_is_opened_by_its_item_and_its_content_and_nothing_on_disk
// test: a_reader_whose_content_landed_since_reads_the_backing_file
// test: a_reader_whose_row_is_gone_is_stale
// test: a_reader_whose_item_shows_another_content_by_now_is_stale
// test: a_file_with_no_binding_and_nothing_on_disk_cannot_be_read
// test: a_range_is_asked_of_the_drive_and_the_item_the_binding_names
#![allow(dead_code, unused_variables)]

use fuser::Errno;
use konedrive_tree::{ContentVersion, Item, LocalItemId, OneDriveItemReference};

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
pub(super) fn decide_open_to_read(item: &Item) -> Result<OpenToRead, Errno> {
    todo!()
}

/// What a range of a file that is not on disk is fetched by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RangeSource {
    /// The drive and the item: the content source is asked by both (the map's MISSING:
    /// today `ContentSource::read_range` takes an item id alone).
    pub(super) onedrive_item: OneDriveItemReference,
    /// The content the range is a part of: its size is the end of the file, and its cTag
    /// tells a download or a range of another version.
    pub(super) content: ContentVersion,
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
pub(super) fn decide_not_on_disk_read_origin(row: &NodeRow, opened_as: &ContentVersion) -> NotOnDiskReadOrigin {
    todo!()
}

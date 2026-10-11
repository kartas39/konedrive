//! What the check writes rows through ([`RowWriter`]): the handle of the store and the
//! upload rules.
//!
//! What a finding writes is here, each write one operation of [`ItemStore`] in a
//! transaction of its own: a new item, a binding, a content, a row that leaves the folder.
//! There is no write that moves a row, and none that removes an item OneDrive has: an item
//! of OneDrive leaves the folder by [`ItemStore::clear_local_and_synced_version`] alone,
//! which keeps its binding and sends nothing. What is written is what the decision found;
//! a finding that would remove an item OneDrive has is refused here. Nothing here asks which write comes first:
//! the order is [`PlaceWalk`](super::place_walk::PlaceWalk)'s.

use konedrive_tree::{ContentPresence, ContentVersion, DriveId, ItemKind, ItemPlace, ItemStore, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference, TreeError};

use super::backing_directory_contents::BackingDirectoryEntry;
use super::disk_finding::{CopyWholeness, DiskContent, DiskFinding, ItemBinding, UnresolvedReason};
use crate::fuse::upload_rules::UploadRules;

/// The reason a path is left with when the store refused its write.
fn to_refused(error: TreeError) -> UnresolvedReason {
    UnresolvedReason::Refused(error.to_string())
}

/// The content a file is made with: of a file bound by its mark that is marked whole and
/// with a cTag, the one that cTag tells, with no hash, for the plan to compare with
/// OneDrive's; of any other, one changed here. A directory has none.
pub(super) fn tell_made_content(entry: &BackingDirectoryEntry, finding: DiskFinding) -> Option<ContentVersion> {
    let (size, mtime, mtime_nsec) = (entry.file.size, entry.file.mtime, entry.file.mtime_nsec);
    (entry.kind == ItemKind::File).then(|| match &entry.file.ctag {
        Some(ctag) if finding == DiskFinding::MarkedWithoutRow && entry.wholeness == CopyWholeness::Whole => ContentVersion::Hashed { size, mtime, mtime_nsec, hash: None, ctag: Some(ctag.clone()) },
        _ => ContentVersion::ChangedHere { size, mtime, mtime_nsec, change_count: 1 },
    })
}

pub(super) struct RowWriter<'a, 'store> {
    item_store: &'a mut ItemStore<'store>,
    /// What says of a made item's name whether it is ignored.
    upload_rules: &'a UploadRules,
}

impl<'a, 'store> RowWriter<'a, 'store> {
    pub(super) fn new(item_store: &'a mut ItemStore<'store>, upload_rules: &'a UploadRules) -> Self {
        RowWriter { item_store, upload_rules }
    }

    /// A new item for `entry`, at its name in `directory`, a file with `content`, its
    /// content on disk.
    pub(super) fn make(&mut self, directory: LocalItemId, entry: &BackingDirectoryEntry, content: Option<ContentVersion>) -> Result<LocalItemId, UnresolvedReason> {
        let place = ItemPlace { parent_local_item_id: directory, name: entry.name.clone() };
        let is_name_ignored = self.upload_rules.is_name_ignored(&entry.name);
        self.item_store.apply(&LocalVersionChange::Made { kind: entry.kind, place, content, is_name_ignored, replaced: None }).map_err(to_refused)
    }

    /// `item`, which was made for an entry marked with the item id `onedrive_id` that no
    /// row has, is that item of OneDrive, of the drive `drive`.
    pub(super) fn bind(&mut self, item: LocalItemId, drive: &DriveId, onedrive_id: &str) -> Result<(), UnresolvedReason> {
        let reference = OneDriveItemReference { drive_id: drive.clone(), item_id: OneDriveItemId::new(onedrive_id) };
        self.item_store.bind_item(item, &reference).map_err(to_refused)
    }

    /// What a finding of a file's content writes to the row of `item`, whose content on
    /// disk is `file`. `was_open_for_writing` and `was_on_disk` are what the row said when
    /// it was read.
    pub(super) fn write_content(&mut self, item: LocalItemId, finding: DiskFinding, file: &DiskContent, was_open_for_writing: bool, was_on_disk: bool) -> Result<(), UnresolvedReason> {
        match finding {
            // A daemon died with a writer open that wrote nothing: as its close leaves it.
            DiskFinding::NothingChanged if was_open_for_writing => self.item_store.apply(&LocalVersionChange::ClosedUnchanged { item }).map(|_| ()).map_err(to_refused),
            DiskFinding::ContentChanged => {
                // Another content, its count raised: by the open that the dead daemon wrote,
                // or by one written here with the close.
                let closed = LocalVersionChange::ClosedChanged { item, size: file.size, mtime: file.mtime, mtime_nsec: file.mtime_nsec };
                let changes = if was_open_for_writing { vec![closed] } else { vec![LocalVersionChange::OpenedForWriting { item }, closed] };
                self.item_store.apply_all(&changes).map_err(to_refused)?;
                if !was_on_disk {
                    self.item_store.set_content_presence(item, ContentPresence::OnDisk).map_err(to_refused)?;
                }
                Ok(())
            }
            DiskFinding::CopyLandedBeforeRow => self.item_store.set_content_presence(item, ContentPresence::OnDisk).map_err(to_refused),
            _ => Ok(()),
        }
    }

    /// `item`, which the folder shows nothing in any more, leaves the folder as `finding`
    /// says: an item of OneDrive is not shown any more and keeps its binding, and the row
    /// of an item OneDrive never had goes. `binding` is the item's as its row says it: a
    /// finding that would remove an item OneDrive has is refused, and writes nothing.
    pub(super) fn take_from_folder(&mut self, item: LocalItemId, finding: DiskFinding, binding: ItemBinding) -> Result<(), UnresolvedReason> {
        match (finding, binding) {
            (DiskFinding::NotShownAnyMore, ItemBinding::Bound) => self.item_store.clear_local_and_synced_version(item),
            (DiskFinding::Removed, ItemBinding::Unbound) => self.item_store.apply(&LocalVersionChange::Removed { item }).map(|_| ()),
            (finding, binding) => Err(TreeError::Refused(format!("{finding:?} takes no item that is {binding:?} from the folder"))),
        }
        .map_err(to_refused)
    }
}

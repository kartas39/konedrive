//! A local-only entry: a file or directory that was made here and is not in OneDrive yet
//! ([`LocalOnlyEntry`]), whether one at a place is uploaded from there
//! ([`find_upload_status`], [`is_in_ignored_directory`]), and the local-only entries at and
//! below a place, walked on disk ([`collect_local_only_entries`]).
//!
//! These are the one step between what [`BackingDirectory`] finds on disk and what
//! [`UploadRules`] say: the rules touch no disk, and are told here the one fact of it they
//! need — which directory is marked with a OneDrive id. Nobody else puts the two together.
//! The walk reads the disk and asks what is ignored; it decides nothing else.

use std::path::Path;

use super::backing_directory::{BackingDirectory, DiskEntry, EntryKind};
use super::directory_reference::DirectoryReference;
use super::entry_place::EntryPlace;
use super::upload_rules::{UploadRules, UploadStatus};

/// A file or directory that was made here and is not in OneDrive yet: what the backing
/// directory holds of it, and the place it is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalOnlyEntry {
    pub(super) on_disk: DiskEntry,
    pub(super) place: EntryPlace,
}

/// Whether what is `name` in the directory at the path `directory` and is not in OneDrive
/// is uploaded from there, or ignored: by its name, or by a directory it is in that is not
/// OneDrive's.
pub(super) fn find_upload_status(backing_directory: &BackingDirectory, upload_rules: &UploadRules, directory: &Path, name: &str) -> UploadStatus {
    upload_rules.find_upload_status(directory, name, |at| backing_directory.is_marked_with_onedrive_id(at))
}

/// Whether what is in the directory at the path `directory` and is not in OneDrive is
/// ignored whatever its name: a directory it is in, which is not OneDrive's, is on the
/// ignore list.
pub(super) fn is_in_ignored_directory(backing_directory: &BackingDirectory, upload_rules: &UploadRules, directory: &Path) -> bool {
    upload_rules.is_in_ignored_directory(directory, |at| backing_directory.is_marked_with_onedrive_id(at))
}

/// How many local-only entries one walk collects at most: a tree larger than this is
/// recorded as far as that, and the rest as it is next changed.
const MOST_ENTRIES: usize = 50_000;

/// The local-only entries at `name` of `directory` and below it, each at its place, the top
/// first: what is uploaded of it from now on. Nothing of what is ignored, of a
/// directory that is marked with a OneDrive id, or of what is neither a file nor a directory.
pub(super) fn collect_local_only_entries(backing_directory: &BackingDirectory, upload_rules: &UploadRules, directory: &DirectoryReference, name: &str) -> Vec<LocalOnlyEntry> {
    let mut entries = Vec::new();
    collect_into(backing_directory, upload_rules, directory, name, &mut entries);
    entries
}

fn collect_into(backing_directory: &BackingDirectory, upload_rules: &UploadRules, directory: &DirectoryReference, name: &str, into: &mut Vec<LocalOnlyEntry>) {
    let rel = directory.path.join(name);
    let Some(on_disk) = backing_directory.find_unmarked_disk_entry(&rel) else { return };
    if find_upload_status(backing_directory, upload_rules, &directory.path, name) == UploadStatus::Ignored {
        return;
    }
    let is_directory = on_disk.metadata.kind == EntryKind::Directory;
    into.push(LocalOnlyEntry { on_disk, place: EntryPlace::new(directory, name) });
    if is_directory && into.len() < MOST_ENTRIES {
        let inside = DirectoryReference { onedrive_id: None, path: rel.clone() };
        for entry in backing_directory.list_directory(&rel) {
            if let Some(name) = entry.file_name().to_str() {
                collect_into(backing_directory, upload_rules, &inside, name, into);
            }
        }
    }
}

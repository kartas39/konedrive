//! A local-only entry: a file or directory that was made here and is not in OneDrive yet
//! ([`LocalOnlyEntry`]), and the local-only entries at and below a place, walked on disk
//! ([`collect_local_only_entries`]).
//!
//! The walk is the one step between what [`BackingDirectory`] finds on disk and what
//! [`super::detections`] decides from: it reads the disk, and asks [`UploadRules`] what is
//! ignored; it decides nothing else.

use super::backing_directory::{BackingDirectory, DiskEntry, EntryKind};
use super::directory_reference::DirectoryReference;
use super::entry_place::EntryPlace;
use super::upload_rules::UploadRules;

/// A file or directory that was made here and is not in OneDrive yet: what the backing
/// directory holds of it, and the place it is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalOnlyEntry {
    pub(super) on_disk: DiskEntry,
    pub(super) place: EntryPlace,
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
    if upload_rules.is_ignored(&directory.path, name, |at| backing_directory.read_onedrive_id(at).is_some()) {
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

//! The local-only entries of the backing directory at their places: the place of a name in a
//! directory ([`folder_place`]), and the local-only entries at and below a place, walked on
//! disk ([`collect_local_only_entries`]).
//!
//! This is the one step between what [`BackingDirectory`] finds on disk and what
//! [`super::detections`] decides from: the walk reads the disk, and asks [`UploadRules`] what
//! stays on this computer; it decides nothing else.

use super::backing_directory::{BackingDirectory, EntryKind};
use super::detections::{FolderPlace, LocalOnlyEntry};
use super::directory::Directory;
use super::upload_rules::UploadRules;

/// How many local-only entries one walk collects at most: a tree larger than this is
/// recorded as far as that, and the rest as it is next changed.
const MOST_ENTRIES: usize = 50_000;

/// `name` in `directory`.
pub(super) fn folder_place(directory: &Directory, name: &str) -> FolderPlace {
    FolderPlace::new(directory.id.as_deref(), &directory.rel, name)
}

/// The local-only entries at `name` of `directory` and below it, each at its place, the top
/// first: what is uploaded of it from now on. Nothing of what stays on this computer, of a
/// directory that is marked with a OneDrive id, or of what is neither a file nor a directory.
pub(super) fn collect_local_only_entries(backing_directory: &BackingDirectory, upload_rules: &UploadRules, directory: &Directory, name: &str) -> Vec<LocalOnlyEntry> {
    let mut entries = Vec::new();
    collect_into(backing_directory, upload_rules, directory, name, &mut entries);
    entries
}

fn collect_into(backing_directory: &BackingDirectory, upload_rules: &UploadRules, directory: &Directory, name: &str, into: &mut Vec<LocalOnlyEntry>) {
    let rel = directory.rel.join(name);
    let Some(on_disk) = backing_directory.find_disk_entry(&rel) else { return };
    let stays = upload_rules.stays_local(&directory.rel, name, |at| backing_directory.read_onedrive_id(at).is_some());
    if stays || backing_directory.read_onedrive_id(&rel).is_some() {
        return;
    }
    let is_directory = on_disk.metadata.kind == EntryKind::Directory;
    into.push(LocalOnlyEntry { on_disk, place: folder_place(directory, name) });
    if is_directory && into.len() < MOST_ENTRIES {
        let inside = Directory { id: None, rel: rel.clone() };
        for entry in backing_directory.list(&rel) {
            if let Some(name) = entry.file_name().to_str() {
                collect_into(backing_directory, upload_rules, &inside, name, into);
            }
        }
    }
}

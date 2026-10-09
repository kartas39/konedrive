//! The local objects of the backing directory as a row sees them: one object
//! ([`local_object`]), its place ([`folder_place`]), and the local entries at and below a
//! place, walked on disk ([`collect_local_only_entries`]).
//!
//! This is the one step between what [`BackingDirectory`] finds on disk and what
//! [`super::detections`] decides from: the walk reads the disk, and asks
//! [`UploadRules`] what stays on this computer; it decides nothing else.

use super::detections::{FolderPlace, LocalEntry, LocalObject, ObjectKind};
use super::backing_directory::{Object, BackingDirectory};
use super::upload_rules::UploadRules;
use super::directory_contents::Directory;

/// How many local objects one walk collects at most: a tree larger than this is recorded as
/// far as that, and the rest as it is next changed.
const MOST_ENTRIES: usize = 50_000;

/// What a row needs of `object`: its file handle, and whether it is a directory or a file of
/// its size.
pub(super) fn local_object(object: &Object) -> LocalObject {
    let kind = if object.meta.is_dir() { ObjectKind::Directory } else { ObjectKind::File { size: object.meta.len() } };
    LocalObject { handle: object.handle.clone(), kind }
}

/// `name` in the directory `dir`.
pub(super) fn folder_place(dir: &Directory, name: &str) -> FolderPlace {
    FolderPlace::new(dir.id.as_deref(), &dir.rel, name)
}

/// The local objects at `name` of `dir` and below it, each at its place, the top first:
/// what is uploaded of it from now on. Nothing of what stays on this computer, of a
/// directory that carries an item's id, or of what is neither a file nor a directory.
pub(super) fn collect_local_only_entries(backing_directory: &BackingDirectory, rules: &UploadRules, dir: &Directory, name: &str) -> Vec<LocalEntry> {
    let mut entries = Vec::new();
    collect_into(backing_directory, rules, dir, name, &mut entries);
    entries
}

fn collect_into(backing_directory: &BackingDirectory, rules: &UploadRules, dir: &Directory, name: &str, into: &mut Vec<LocalEntry>) {
    let rel = dir.rel.join(name);
    let Some(object) = backing_directory.find_disk_entry(&rel) else { return };
    let stays = rules.stays_local(&dir.rel, name, |at| backing_directory.read_onedrive_id(at).is_some());
    if stays || backing_directory.read_onedrive_id(&rel).is_some() || !(object.meta.is_dir() || object.meta.is_file()) {
        return;
    }
    into.push(LocalEntry { place: folder_place(dir, name), object: local_object(&object) });
    if object.meta.is_dir() && into.len() < MOST_ENTRIES {
        let inside = Directory { id: None, rel: rel.clone() };
        for entry in backing_directory.list(&rel) {
            if let Some(name) = entry.file_name().to_str() {
                collect_into(backing_directory, rules, &inside, name, into);
            }
        }
    }
}

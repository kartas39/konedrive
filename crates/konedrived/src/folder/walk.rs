//! A walk over the folder's files that opens none of them.

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Whether a name is one konedrive keeps for itself (`.konedrive-holding`, a
/// replacement's `.konedrive-new-<id>`, ...), never a user's file.
pub(crate) fn reserved(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(konedrive_fs::RESERVED_PREFIX.as_bytes())
}

/// Every regular file under `root` with its `lstat` metadata: `.konedrive-*`
/// entries skipped, symbolic links below the root never followed, no mount point
/// below the root's own filesystem crossed, and nothing deeper than
/// `konedrive_fs::MAX_DEPTH`. No file is opened, only directories are.
///
/// `root` itself is taken as the directory it names, also when it is a link to it — the
/// link the kernel keeps for a descriptor, by which the backing directory is walked
/// (`BackingDirectory::resolve_root_path`): the filesystem that is not left is the
/// directory's, not the link's.
pub(crate) fn walk_files(root: &Path, visit: &mut dyn FnMut(&Path, &Metadata)) {
    let Ok(root_meta) = std::fs::metadata(root) else { return };
    let mut dirs = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if reserved(&entry.file_name()) {
                continue;
            }
            // `DirEntry::metadata` does not follow a symbolic link.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if depth < konedrive_fs::MAX_DEPTH && meta.dev() == root_meta.dev() {
                    dirs.push((entry.path(), depth + 1));
                }
            } else if meta.is_file() {
                visit(&entry.path(), &meta);
            }
        }
    }
}

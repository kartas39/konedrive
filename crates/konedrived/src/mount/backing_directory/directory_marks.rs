//! The directories of the backing directory down to a path: what a downloaded copy, or
//! anything a program makes, lies in. Each is marked with the OneDrive id of the directory it
//! backs, so that it is known as that directory whatever its name becomes.
//!
//! Which OneDrive directory a name is, the disk does not know: whoever asks for the
//! directories says, name by name.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::BackingDirectory;

impl BackingDirectory {
    /// Makes the directories down to `rel` where they are missing, and marks each with the
    /// OneDrive id of the directory it backs. `root_id` is the id of the folder itself;
    /// `id_of` tells the id of a name from the directory it is in — that directory's id,
    /// when it has one, and its path. A directory `id_of` knows nothing of is made and left
    /// unmarked. A file that stands where a directory is to be made is an error
    /// (`ENOTDIR`): it is the user's own, and is neither marked nor made way for.
    pub(in crate::mount) fn make_marked_directories(&self, rel: &Path, root_id: Option<String>, id_of: impl Fn(Option<&str>, &Path, &str) -> Option<String>) -> io::Result<()> {
        let (mut parent_id, mut parent) = (root_id, PathBuf::new());
        for name in rel.components() {
            let at = parent.join(name);
            let id = name.as_os_str().to_str().and_then(|n| id_of(parent_id.as_deref(), &parent, n));
            match self.make_directory(&at) {
                Ok(()) => {}
                // What is there already must be a directory: a file in the way is not marked
                // with the directory's OneDrive id, which would hide it as that directory.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if !fs::metadata(self.resolve_path(&at)).is_ok_and(|metadata| metadata.is_dir()) {
                        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                    }
                }
                Err(e) => return Err(e),
            }
            if let Some(id) = &id {
                if self.read_onedrive_id(&at).as_deref() != Some(id.as_str()) {
                    self.mark_with_onedrive_id(&at, id)?;
                }
            }
            (parent_id, parent) = (id, at);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

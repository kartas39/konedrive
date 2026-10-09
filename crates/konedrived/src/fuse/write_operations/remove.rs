//! A file or a directory a program removes from the folder: recorded first, then removed
//! from the backing directory.

use fuser::Errno;

use super::WriteOperations;
use crate::fuse::detections::EntryRemoved;
use crate::fuse::shared::errno;
use crate::fuse::directory_contents::{Directory, Entry};

impl WriteOperations {
    /// Removes `name` from the directory node `parent`: a directory (`dir`), which must hold
    /// nothing, or a file. An item is removed from OneDrive by the outbox; a local object is
    /// forgotten there.
    pub(in crate::fuse) fn remove(&self, parent: u64, name: &str, dir: bool) -> Result<(), Errno> {
        self.check_writable()?;
        let above = self.node_lookup.find_directory(parent).ok_or(Errno::ENOENT)?;
        let entry = self.directory_contents.find_entry(&above, name).ok_or(Errno::ENOENT)?;
        match (entry.is_dir(), dir) {
            (true, false) => return Err(Errno::EISDIR),
            (false, true) => return Err(Errno::ENOTDIR),
            _ => {}
        }
        let rel = above.rel.join(name);
        if dir && !self.directory_contents.list_entries(&Directory { id: entry.id().map(str::to_owned), rel: rel.clone() }).is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        let event = match &entry {
            Entry::Item(row) => EntryRemoved::InOneDrive { id: row.id.clone(), rel: rel.clone() },
            Entry::Local(local) => EntryRemoved::LocalOnly { handle: local.object.handle.clone(), rel: rel.clone() },
        };
        self.record_or_undo(event, format_args!("the removal of {}", rel.display()), || {})?;
        match &entry {
            // The directory under the mount goes when nothing is left in it.
            Entry::Item(_) if dir => drop(self.backing_directory.remove_dir(&rel)),
            Entry::Item(row) => drop(self.backing_directory.remove_downloaded_copy(&rel, &row.id)),
            Entry::Local(_) if dir => self.backing_directory.remove_dir(&rel).map_err(|e| errno(&e))?,
            Entry::Local(_) => self.backing_directory.remove_file(&rel).map_err(|e| errno(&e))?,
        }
        if let Entry::Local(local) = &entry {
            self.node_lookup.forget(&local.object.handle);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

//! A file or a directory a program removes from the folder.

use std::fs;

use fuser::Errno;

use super::changes::{based, forgotten, leaves, object};
use super::copies::remove_copy;
use super::local::{Dir, Entry};
use super::nodes::Key;
use super::shared::{errno, Shared};

impl Shared {
    /// Removes `name` from the directory `parent`: a directory (`dir`), which must hold
    /// nothing, or a file. An item is removed from OneDrive by the outbox; what had no item
    /// is forgotten there.
    pub(super) fn remove(&self, parent: u64, name: &str, dir: bool) -> Result<(), Errno> {
        self.may_change()?;
        let above = self.dir(parent).ok_or(Errno::ENOENT)?;
        let entry = self.entry(&above, name).ok_or(Errno::ENOENT)?;
        match (entry.is_dir(), dir) {
            (true, false) => return Err(Errno::EISDIR),
            (false, true) => return Err(Errno::ENOTDIR),
            _ => {}
        }
        let rel = above.rel.join(name);
        if dir && !self.entries(&Dir { id: entry.id().map(str::to_owned), rel: rel.clone() }).is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        let (path, at) = (self.under(&rel), rel.clone());
        let recorded = match &entry {
            Entry::Item(row) => {
                let id = row.id.clone();
                self.record(move |s| Ok((based(s, &id)?.iter().map(|base| leaves(Some(base), None, &at)).collect(), Vec::new())))
            }
            Entry::Local(local) => {
                let inode = object(&local.meta);
                self.record(move |s| Ok((forgotten(s, &inode, &at)?, Vec::new())))
            }
        };
        if let Err(e) = recorded {
            tracing::warn!("the removal of {} could not be recorded: {e}", rel.display());
            return Err(Errno::EIO);
        }
        match &entry {
            // The directory under the mount goes when nothing is left in it.
            Entry::Item(_) if dir => drop(fs::remove_dir(&path)),
            Entry::Item(row) => drop(remove_copy(&path, &row.id)),
            Entry::Local(_) if dir => fs::remove_dir(&path).map_err(|e| errno(&e))?,
            Entry::Local(_) => fs::remove_file(&path).map_err(|e| errno(&e))?,
        }
        if let Entry::Local(local) = &entry {
            // Its inode number is the filesystem's to give again: the next object that gets
            // it is another node.
            crate::panic::lock(&self.nodes).inos.remove(&Key::Local(std::os::unix::fs::MetadataExt::ino(&local.meta)));
        }
        Ok(())
    }
}

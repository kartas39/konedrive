//! A file or a directory a program removes from the folder.

use fuser::Errno;

use super::changes::{based, forgotten, leaves};
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
        let at = rel.clone();
        let recorded = match &entry {
            Entry::Item(row) => {
                let id = row.id.clone();
                self.record(move |s| Ok((based(s, &id)?.iter().map(|base| leaves(Some(base), None, &at)).collect(), Vec::new())))
            }
            Entry::Local(local) => {
                let object = local.object.handle.clone();
                self.record(move |s| Ok((forgotten(s, &object, &at)?, Vec::new())))
            }
        };
        if let Err(e) = recorded {
            tracing::warn!("the removal of {} could not be recorded: {e}", rel.display());
            return Err(Errno::EIO);
        }
        match &entry {
            // The directory under the mount goes when nothing is left in it.
            Entry::Item(_) if dir => drop(self.under.remove_dir(&rel)),
            Entry::Item(row) => drop(self.under.remove_copy(&rel, &row.id)),
            Entry::Local(_) if dir => self.under.remove_dir(&rel).map_err(|e| errno(&e))?,
            Entry::Local(_) => self.under.remove_file(&rel).map_err(|e| errno(&e))?,
        }
        if let Entry::Local(local) = &entry {
            // The object is gone, and no other is ever this one: nothing is found by it again.
            crate::panic::lock(&self.nodes).inos.remove(&Key::Local(local.object.handle.clone()));
        }
        Ok(())
    }
}

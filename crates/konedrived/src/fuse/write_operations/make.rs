//! A file or a directory a program makes in the folder: made in the backing directory,
//! where it has no item yet, and recorded for the outbox to send.

use std::fs::File;

use fuser::{Errno, FileAttr};
use konedrive_tree::Row;

use super::{OpenedFile, WriteOperations};
use crate::fuse::attr;
use crate::fuse::detections::Made;
use crate::fuse::local_entries::{folder_place, local_object};
use crate::fuse::shared::errno;
use crate::fuse::view::{Dir, Entry, Local};

/// What was made: what the mount shows of it, and the file a program made it to write.
pub(in crate::fuse) struct MadeEntry {
    pub(in crate::fuse) attr: FileAttr,
    pub(in crate::fuse) opened: Option<OpenedFile>,
}

impl WriteOperations {
    /// Makes `name` in the directory node `parent`: a directory, or a file, which `open`
    /// leaves open for writing.
    pub(in crate::fuse) fn make(&self, parent: u64, name: &str, dir: bool, open: bool) -> Result<MadeEntry, Errno> {
        let above = self.check_make(parent, name, dir)?;
        let rel = above.rel.join(name);
        self.view.backing(&above.rel).map_err(|e| errno(&e))?;
        let file: Option<File> = if dir {
            self.under.make_dir(&rel).map_err(|e| errno(&e))?;
            None
        } else {
            Some(self.under.make_file(&rel).map_err(|e| errno(&e))?)
        };
        let unmake = || drop(if dir { self.under.remove_dir(&rel) } else { self.under.remove_file(&rel) });
        // What cannot be looked at is not recorded, and is not left in the folder either.
        let Some(object) = self.under.object(&rel) else {
            unmake();
            return Err(Errno::EIO);
        };
        let removed = if dir { None } else { self.view.removed_item_at(&rel) };
        let (place, local) = (folder_place(&above, name), local_object(&object));
        let event = match &removed {
            Some(base) => {
                self.under.mark_copy(&rel, &base.id).map_err(|e| errno(&e))?;
                Some(Made::OverRemovedItem { place, file: local, open, base: base.clone() })
            }
            None if self.stays_local(&above, name) => None,
            None => Some(Made::New { place, object: local, open }),
        };
        if let Some(event) = event {
            self.record_or_undo(event, format_args!("a new {}", rel.display()), unmake)?;
        }
        let entry = match &removed {
            Some(base) => Entry::Item(Row { name: name.to_owned(), ..base.clone() }),
            None => Entry::Local(Local { name: name.to_owned(), object }),
        };
        let ino = self.resolve.ino_of(&entry, parent);
        let attr = attr::of_entry(&entry, ino, &rel, self.recorder.is_writable(), &self.view, &self.writers);
        let opened = file.filter(|_| open).map(|file| {
            // What is written into a file just made is its content: the outbox waits for
            // its last writer already.
            self.writers.opened(ino, true);
            OpenedFile { ino, id: removed.map(|base| base.id), rel, file }
        });
        Ok(MadeEntry { attr, opened })
    }

    /// The directory node `parent` is, if `name` may be made in it as a directory (`dir`) or
    /// a file. Neither the daemon's own names nor a file manager's trash in the folder
    /// itself are made.
    fn check_make(&self, parent: u64, name: &str, dir: bool) -> Result<Dir, Errno> {
        self.check_writable()?;
        let above = self.resolve.dir(parent).ok_or(Errno::ENOENT)?;
        if self.rules.daemon_owned(name) || (dir && parent == 1 && self.rules.is_trash(name)) {
            return Err(Errno::EPERM);
        }
        if self.view.entry(&above, name).is_some() {
            return Err(Errno::EEXIST);
        }
        Ok(above)
    }
}

#[cfg(test)]
mod tests;

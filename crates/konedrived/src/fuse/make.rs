//! A file or a directory a program makes in the folder: made in the folder under the mount,
//! where it has no item yet, and recorded for the outbox to send.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::{Errno, FileAttr};
use konedrive_tree::outbox::{OutboxKind, OutboxState};
use konedrive_tree::{Kind, Row};

use super::changes::{waits, Place};
use super::local::{Entry, Local};
use super::read::Handle;
use super::shared::{errno, Shared};
use crate::folder::disk::daemon_owned;

/// What was made: what the mount shows of it, and the handle it was opened by.
pub(super) struct Made {
    pub(super) attr: FileAttr,
    pub(super) handle: Option<u64>,
}

impl Shared {
    /// Makes `name` in the directory `parent`: a directory, or a file, which `open` leaves
    /// open for writing.
    pub(super) fn make(&self, parent: u64, name: &str, dir: bool, open: bool) -> Result<Made, Errno> {
        self.may_change()?;
        let above = self.dir(parent).ok_or(Errno::ENOENT)?;
        // No trash directory of a file manager either: what is removed here goes to
        // OneDrive's recycle bin, and a trash inside the folder would be uploaded.
        if daemon_owned(name.as_ref()) || (dir && parent == 1 && name.starts_with(".Trash")) {
            return Err(Errno::EPERM);
        }
        if self.entry(&above, name).is_some() {
            return Err(Errno::EEXIST);
        }
        let rel = above.rel.join(name);
        self.backing(&above.rel).map_err(|e| errno(&e))?;
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
        // A file made where the user just removed an item's file is that item's new content:
        // programs replace a file that way, and the item keeps its history and its links.
        let again = if dir { None } else { self.removed_here(&rel) };
        let recorded = match &again {
            Some(base) => {
                let mut update = Place { dir: &above, name }.of_item(OutboxKind::Update, base, Some(&object));
                if open {
                    waits(&mut update);
                }
                self.under.mark_copy(&rel, &base.id).map_err(|e| errno(&e))?;
                self.record(move |_| Ok((vec![update], Vec::new()))).map(drop)
            }
            None if self.stays_local(&above, name) => Ok(()),
            None => {
                let made = Place { dir: &above, name }.new(&object, open);
                self.record(move |_| Ok((vec![made], Vec::new()))).map(drop)
            }
        };
        if let Err(e) = recorded {
            tracing::warn!("a new {} could not be recorded: {e}", rel.display());
            unmake();
            return Err(Errno::EIO);
        }
        let entry = match &again {
            Some(base) => Entry::Item(Row { name: name.to_owned(), ..base.clone() }),
            None => Entry::Local(Local { name: name.to_owned(), object }),
        };
        let ino = self.ino_of(&entry, parent);
        let attr = self.attr_of(&entry, ino, &rel);
        let id = again.map(|base| base.id);
        let handle = file.filter(|_| open).map(|file| {
            // What is written into a file just made is its content: the outbox waits for
            // its last writer already.
            self.writers.opened(ino, true);
            let fh = self.next_handle.fetch_add(1, Ordering::SeqCst);
            crate::panic::lock(&self.handles).insert(fh, Handle { ino, id: id.clone(), rel, copy: Some(Arc::new(file)), writes: true });
            fh
        });
        Ok(Made { attr, handle })
    }

    /// The item whose file the user removed from `rel` and whose removal is not sent yet, as
    /// the base has it.
    fn removed_here(&self, rel: &Path) -> Option<Row> {
        let rows = self.read(|s| s.outbox_at_or_under(rel))?;
        let removed = rows.into_iter().find(|row| row.rel == rel && row.kind == OutboxKind::Delete && row.state != OutboxState::Running)?;
        self.row(&removed.item_id?).filter(|base| base.kind == Kind::File)
    }
}

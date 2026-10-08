//! A file or a directory a program makes in the folder: made in the folder under the mount,
//! where it has no item yet, and recorded for the outbox to send.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::{Errno, FileAttr};

use super::changes::Place;
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
        if daemon_owned(name.as_ref()) {
            return Err(Errno::EPERM);
        }
        if self.entry(&above, name).is_some() {
            return Err(Errno::EEXIST);
        }
        let rel = above.rel.join(name);
        self.backing(&above.rel).map_err(|e| errno(&e))?;
        let path = self.under(&rel);
        let file: Option<File> = if dir {
            fs::create_dir(&path).map_err(|e| errno(&e))?;
            None
        } else {
            Some(OpenOptions::new().read(true).write(true).create_new(true).mode(0o644).open(&path).map_err(|e| errno(&e))?)
        };
        let meta = fs::symlink_metadata(&path).map_err(|e| errno(&e))?;
        if !self.stays_local(&above, name) {
            let made = Place { dir: &above, name }.new(&meta, open);
            if let Err(e) = self.record(move |_| Ok((vec![made], Vec::new()))) {
                tracing::warn!("a new {} could not be recorded: {e}", rel.display());
                let _ = if dir { fs::remove_dir(&path) } else { fs::remove_file(&path) };
                return Err(Errno::EIO);
            }
        }
        let entry = Entry::Local(Local { name: name.to_owned(), meta });
        let ino = self.ino_of(&entry, parent);
        let attr = self.attr_of(&entry, ino, &rel);
        let handle = file.filter(|_| open).map(|file| {
            // What is written into a file just made is its content: the outbox waits for
            // its last writer already.
            self.writers.opened(ino, true);
            let fh = self.next_handle.fetch_add(1, Ordering::SeqCst);
            crate::panic::lock(&self.handles).insert(fh, Handle { ino, id: None, rel, copy: Some(Arc::new(file)), writes: true });
            fh
        });
        Ok(Made { attr, handle })
    }
}

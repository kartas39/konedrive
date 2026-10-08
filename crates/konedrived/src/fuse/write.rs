//! A file a program writes: opened for writing in the folder under the mount, where its
//! content lies, and sent once its last writer closed it.
//!
//! An item's file is written in its downloaded copy, which stays the item's: from the first
//! change on the outbox holds the item, so nothing from OneDrive replaces the copy, and what
//! the mount shows of the file is the copy's own.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::Row;

use super::changes::{based, waits, Place};
use super::copies::mark_copy;
use super::local::Entry;
use super::read::Handle;
use super::shared::{errno, Shared};

impl Shared {
    /// The file of node `ino` in the folder under the mount, ready to be written: an item's
    /// downloaded copy made writable, or made empty when it is to be cut to nothing. A copy
    /// that is needed and missing is the caller's to download first.
    fn writable_file(&self, entry: &Entry, rel: &Path, empty: bool) -> Result<(), Errno> {
        let Entry::Item(row) = entry else { return Ok(()) };
        let path = self.under(rel);
        if !self.has_copy(rel, &row.id) {
            if !empty && row.size > 0 {
                return Err(Errno::EIO);
            }
            self.backing(rel.parent().unwrap_or(Path::new(""))).map_err(|e| errno(&e))?;
            OpenOptions::new().write(true).create_new(true).mode(0o644).open(&path).map_err(|e| errno(&e))?;
            mark_copy(&path, &row.id).map_err(|e| errno(&e))?;
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).map_err(|e| errno(&e))
    }

    /// Whether writing node `ino` needs its content downloaded first: an item's file that
    /// is not downloaded and is not cut to nothing.
    pub(super) fn needs_download(&self, ino: u64, empty: bool) -> Option<(Row, std::path::PathBuf)> {
        match self.node(ino)? {
            (Entry::Item(row), rel) if !empty && row.size > 0 && !self.has_copy(&rel, &row.id) => Some((row, rel)),
            _ => None,
        }
    }

    /// Opens node `ino` for writing, cut to nothing when `truncate`. The handle.
    pub(super) fn open_to_write(&self, ino: u64, truncate: bool) -> Result<u64, Errno> {
        self.may_change()?;
        let (entry, rel) = self.node(ino).ok_or(Errno::ENOENT)?;
        if entry.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.writable_file(&entry, &rel, truncate)?;
        let file = OpenOptions::new().read(true).write(true).truncate(truncate).open(self.under(&rel)).map_err(|e| errno(&e))?;
        self.writer_opened(ino);
        if truncate {
            self.changing(ino)?;
        }
        let fh = self.next_handle.fetch_add(1, Ordering::SeqCst);
        crate::panic::lock(&self.handles).insert(fh, Handle { ino, id: entry.id().map(str::to_owned), rel, copy: Some(Arc::new(file)), writes: true });
        Ok(fh)
    }

    /// Node `ino` is about to be changed by a program that holds it open: the first time
    /// since it was last closed, the outbox is told that it waits for its writer.
    pub(super) fn changing(&self, ino: u64) -> Result<(), Errno> {
        if self.first_change(ino) {
            self.content(ino, true)?;
        }
        Ok(())
    }

    /// Cuts or extends node `ino` to `size`.
    pub(super) fn truncate(&self, ino: u64, size: u64) -> Result<(), Errno> {
        self.may_change()?;
        let (entry, rel) = self.node(ino).ok_or(Errno::ENOENT)?;
        if entry.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.writable_file(&entry, &rel, size == 0)?;
        let file: File = OpenOptions::new().write(true).open(self.under(&rel)).map_err(|e| errno(&e))?;
        let open = self.being_written(ino);
        if open {
            self.changing(ino)?;
        }
        file.set_len(size).map_err(|e| errno(&e))?;
        if !open {
            self.content(ino, false)?;
        }
        Ok(())
    }

    /// Records that the content of node `ino` is the user's own now: still being written
    /// (`open`), or ready to be sent.
    pub(super) fn content(&self, ino: u64, open: bool) -> Result<(), Errno> {
        // Gone since: removed while it was open.
        let (Some((entry, rel)), Some((_, above))) = (self.node(ino), self.above(ino)) else { return Ok(()) };
        let Ok(meta) = fs::symlink_metadata(self.under(&rel)) else { return Ok(()) };
        let name = entry.name().to_owned();
        let recorded = match entry {
            Entry::Local(_) if self.stays_local(&above, &name) => return Ok(()),
            Entry::Local(_) => {
                let new = Place { dir: &above, name: &name }.new(&meta, open);
                self.record(move |_| Ok((vec![new], Vec::new())))
            }
            Entry::Item(row) => self.record(move |s| {
                let Some(base) = based(s, &row.id)? else { return Ok(Default::default()) };
                let mut update = Place { dir: &above, name: &name }.of_item(OutboxKind::Update, &base, Some(&meta));
                if open {
                    waits(&mut update);
                }
                Ok((vec![update], Vec::new()))
            }),
        };
        recorded.map(drop).map_err(|e| {
            tracing::warn!("the change of {} could not be recorded: {e}", rel.display());
            Errno::EIO
        })
    }
}

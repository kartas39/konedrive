//! A file a program writes: opened for writing in the folder under the mount, where its
//! content lies, and sent once its last writer closed it.
//!
//! An item's file is written in its downloaded copy, which stays the item's: from the first
//! change on the outbox holds the item, so nothing from OneDrive replaces the copy, and what
//! the mount shows of the file is the copy's own.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::Row;

use super::changes::{based, settles, waits, Place};
use super::view::Entry;
use super::read::Handle;
use super::shared::{errno, Shared};

impl Shared {
    /// The file of node `ino` in the folder under the mount, ready to be written: an item's
    /// downloaded copy made writable, or made empty when it is to be cut to nothing. A copy
    /// that is needed and missing is the caller's to download first.
    fn writable_file(&self, entry: &Entry, rel: &Path, empty: bool) -> Result<(), Errno> {
        let Entry::Item(row) = entry else { return Ok(()) };
        if !self.under.has_copy(rel, &row.id) {
            if !empty && row.size > 0 {
                return Err(Errno::EIO);
            }
            self.view.backing(rel.parent().unwrap_or(Path::new(""))).map_err(|e| errno(&e))?;
            self.under.make_file(rel).map_err(|e| errno(&e))?;
            self.under.mark_copy(rel, &row.id).map_err(|e| errno(&e))?;
        }
        self.under.writable(rel).map_err(|e| errno(&e))
    }

    /// Whether writing node `ino` needs its content downloaded first: an item's file that
    /// is not downloaded and is not cut to nothing.
    pub(super) fn needs_download(&self, ino: u64, empty: bool) -> Option<(Row, std::path::PathBuf)> {
        match self.resolve.node(ino)? {
            (Entry::Item(row), rel) if !empty && row.size > 0 && !self.under.has_copy(&rel, &row.id) => Some((row, rel)),
            _ => None,
        }
    }

    /// Opens node `ino` for writing, cut to nothing when `truncate`. The handle.
    pub(super) fn open_to_write(&self, ino: u64, truncate: bool) -> Result<u64, Errno> {
        self.may_change()?;
        let (entry, rel) = self.resolve.node(ino).ok_or(Errno::ENOENT)?;
        if entry.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.writable_file(&entry, &rel, truncate)?;
        let file = self.under.write(&rel, truncate).map_err(|e| errno(&e))?;
        // Its content is the user's own already when the outbox holds a change of it; from
        // the first writer on the outbox holds it in any case.
        let own = truncate || entry.id().is_some_and(|id| self.view.overlay().changed.contains(id));
        if self.writers.opened(ino, own) {
            if let Err(e) = self.content(ino, true) {
                self.writers.closed(ino);
                return Err(e);
            }
        }
        let fh = self.next_handle.fetch_add(1, Ordering::SeqCst);
        crate::panic::lock(&self.handles).insert(fh, Handle { ino, id: entry.id().map(str::to_owned), rel, copy: Some(Arc::new(file)), writes: true });
        Ok(fh)
    }

    /// The last writer of node `ino` closed it: its content is ready to be sent, or, when
    /// nothing was written, the outbox lets go of it.
    pub(super) fn closed(&self, ino: u64, changed: bool) {
        let untouched = match self.resolve.node(ino) {
            Some((Entry::Item(row), _)) if !changed => Some(row),
            _ => None,
        };
        let Some(row) = untouched else {
            let _ = self.content(ino, false);
            return;
        };
        // A node the item no longer is — its name was taken over by a file saved over it —
        // says nothing of the item's content.
        if self.resolve.item_node(&row.id) != Some(ino) {
            return;
        }
        let Some((_, above)) = self.resolve.above(ino) else { return };
        // The item is where it stands, with the content the base has: the row that held it
        // goes, or is the move it was before.
        let recorded = self.record(move |s| {
            let Some(base) = based(s, &row.id)? else { return Ok(Default::default()) };
            let mut here = Place { dir: &above, name: &row.name }.of_item(OutboxKind::Move, &base, None);
            here.same_content = true;
            Ok((vec![here], Vec::new()))
        });
        if let Err(e) = recorded {
            tracing::warn!("a file closed unchanged could not be let go of: {e}");
        }
    }

    /// Cuts or extends node `ino` to `size`.
    pub(super) fn truncate(&self, ino: u64, size: u64) -> Result<(), Errno> {
        self.may_change()?;
        let (entry, rel) = self.resolve.node(ino).ok_or(Errno::ENOENT)?;
        if entry.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.writable_file(&entry, &rel, size == 0)?;
        let file = self.under.write(&rel, false).map_err(|e| errno(&e))?;
        // Recorded before it is cut: a daemon that dies in between leaves a row for content
        // that did not change, never a changed file without a row.
        if self.writers.open(ino) {
            self.writers.written(ino);
        } else {
            self.content(ino, false)?;
        }
        file.set_len(size).map_err(|e| errno(&e))
    }

    /// Records that the content of node `ino` is the user's own now: still being written
    /// (`open`), or ready to be sent.
    pub(super) fn content(&self, ino: u64, open: bool) -> Result<(), Errno> {
        // Gone since: removed while it was open.
        let (Some((entry, rel)), Some((_, above))) = (self.resolve.node(ino), self.resolve.above(ino)) else { return Ok(()) };
        let Some(object) = self.under.object(&rel) else { return Ok(()) };
        let name = entry.name().to_owned();
        let recorded = match entry {
            Entry::Local(_) if self.stays_local(&above, &name) => return Ok(()),
            Entry::Local(_) => {
                let new = Place { dir: &above, name: &name }.new(&object, open);
                self.record(move |_| Ok((vec![new], Vec::new())))
            }
            Entry::Item(row) => self.record(move |s| {
                let Some(base) = based(s, &row.id)? else { return Ok(Default::default()) };
                let mut update = Place { dir: &above, name: &name }.of_item(OutboxKind::Update, &base, Some(&object));
                if open {
                    waits(&mut update);
                } else {
                    settles(&mut update);
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

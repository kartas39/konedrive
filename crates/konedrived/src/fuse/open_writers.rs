//! The files programs hold open for writing: how many handles each has, and whether anything
//! was written through them. A file is sent only once its last writer closed it; from its
//! first writer on the outbox holds it, so that nothing from OneDrive replaces a file a
//! program is about to write.

use std::collections::HashMap;
use std::sync::Mutex;

/// The files open for writing, by node.
#[derive(Default)]
pub(super) struct OpenWriters(Mutex<HashMap<u64, FileWriters>>);

/// One file open for writing.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct FileWriters {
    /// The handles that may write it.
    pub(super) open: u32,
    /// Its content was changed through one of them, or was the user's own before.
    pub(super) changed: bool,
}

/// What closing a handle that could write left of its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterClose {
    /// Other handles still write it.
    StillOpenElsewhere,
    /// The last writer closed it, and its content was changed.
    ContentChanged,
    /// The last writer closed it, and nothing was written.
    ContentUntouched,
}

impl OpenWriters {
    /// One more handle writes node `ino`, whose content is the user's own already when
    /// `changed`. Whether it is the first: the one the outbox is told of.
    pub(super) fn add_writer(&self, node_id: u64, changed: bool) -> bool {
        let mut open_writers = crate::panic::lock(&self.0);
        let written = open_writers.entry(node_id).or_default();
        written.open += 1;
        written.changed |= changed;
        written.open == 1
    }

    /// Something was written through a handle of node `ino`.
    pub(super) fn mark_written(&self, node_id: u64) {
        if let Some(written) = crate::panic::lock(&self.0).get_mut(&node_id) {
            written.changed = true;
        }
    }

    /// A handle that could write node `ino` was closed.
    pub(super) fn remove_writer(&self, node_id: u64) -> AfterClose {
        let mut open_writers = crate::panic::lock(&self.0);
        let Some(written) = open_writers.get_mut(&node_id) else { return AfterClose::StillOpenElsewhere };
        written.open = written.open.saturating_sub(1);
        if written.open > 0 {
            return AfterClose::StillOpenElsewhere;
        }
        match open_writers.remove(&node_id) {
            Some(FileWriters { changed: true, .. }) => AfterClose::ContentChanged,
            _ => AfterClose::ContentUntouched,
        }
    }

    /// Whether a program holds node `ino` open for writing.
    pub(super) fn is_open(&self, node_id: u64) -> bool {
        crate::panic::lock(&self.0).get(&node_id).is_some_and(|written| written.open > 0)
    }
}

#[cfg(test)]
mod tests;

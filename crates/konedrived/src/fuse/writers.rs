//! The files programs hold open for writing: how many handles each has, and whether anything
//! was written through them. A file is sent only once its last writer closed it; from its
//! first writer on the outbox holds it, so that nothing from OneDrive replaces a file a
//! program is about to write.

use std::collections::HashMap;
use std::sync::Mutex;

/// The files open for writing, by node.
#[derive(Default)]
pub(super) struct Writers(Mutex<HashMap<u64, Written>>);

/// One file open for writing.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Written {
    /// The handles that may write it.
    pub(super) open: u32,
    /// Its content was changed through one of them, or was the user's own before.
    pub(super) changed: bool,
}

/// What closing a handle that could write left of its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Closed {
    /// Other handles still write it.
    StillOpen,
    /// The last writer closed it, and its content was changed.
    Changed,
    /// The last writer closed it, and nothing was written.
    Untouched,
}

impl Writers {
    /// One more handle writes node `ino`, whose content is the user's own already when
    /// `changed`. Whether it is the first: the one the outbox is told of.
    pub(super) fn opened(&self, ino: u64, changed: bool) -> bool {
        let mut writers = crate::panic::lock(&self.0);
        let written = writers.entry(ino).or_default();
        written.open += 1;
        written.changed |= changed;
        written.open == 1
    }

    /// Something was written through a handle of node `ino`.
    pub(super) fn written(&self, ino: u64) {
        if let Some(written) = crate::panic::lock(&self.0).get_mut(&ino) {
            written.changed = true;
        }
    }

    /// A handle that could write node `ino` was closed.
    pub(super) fn closed(&self, ino: u64) -> Closed {
        let mut writers = crate::panic::lock(&self.0);
        let Some(written) = writers.get_mut(&ino) else { return Closed::StillOpen };
        written.open = written.open.saturating_sub(1);
        if written.open > 0 {
            return Closed::StillOpen;
        }
        match writers.remove(&ino) {
            Some(Written { changed: true, .. }) => Closed::Changed,
            _ => Closed::Untouched,
        }
    }

    /// Whether a program holds node `ino` open for writing.
    pub(super) fn open(&self, ino: u64) -> bool {
        crate::panic::lock(&self.0).get(&ino).is_some_and(|written| written.open > 0)
    }
}

#[cfg(test)]
mod tests;

//! The files programs hold open for writing: how many handles each has, and whether anything
//! was written through them. A file is sent only once its last writer closed it; from its
//! first writer on the outbox holds it, so that nothing from OneDrive replaces a file a
//! program is about to write.

use std::collections::HashMap;
use std::sync::Mutex;

/// The files open for writing, by node id.
#[derive(Default)]
pub(super) struct OpenWriters(Mutex<HashMap<u64, FileWriters>>);

/// The writers of one file.
#[derive(Debug, Default, Clone, Copy)]
struct FileWriters {
    /// The handles that may write it.
    handles: u32,
    /// Its content was changed through one of them, or was the user's own before.
    content_changed: bool,
}

/// What closing a handle that could write left of its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseOutcome {
    /// Other handles still write it.
    StillOpenElsewhere,
    /// The last writer closed it, and its content was changed.
    ContentChanged,
    /// The last writer closed it, and nothing was written.
    ContentUntouched,
}

impl OpenWriters {
    /// One more handle writes the node `node_id`, whose content is the user's own already
    /// when `content_changed`. Whether it is the first: the one the outbox is told of.
    pub(super) fn add_writer(&self, node_id: u64, content_changed: bool) -> bool {
        let mut open_writers = crate::panic::lock(&self.0);
        let writers = open_writers.entry(node_id).or_default();
        writers.handles += 1;
        writers.content_changed |= content_changed;
        writers.handles == 1
    }

    /// Something was written through a handle of the node `node_id`.
    pub(super) fn mark_written(&self, node_id: u64) {
        if let Some(writers) = crate::panic::lock(&self.0).get_mut(&node_id) {
            writers.content_changed = true;
        }
    }

    /// A handle that could write the node `node_id` was closed.
    pub(super) fn remove_writer(&self, node_id: u64) -> CloseOutcome {
        let mut open_writers = crate::panic::lock(&self.0);
        let Some(writers) = open_writers.get_mut(&node_id) else { return CloseOutcome::StillOpenElsewhere };
        writers.handles = writers.handles.saturating_sub(1);
        if writers.handles > 0 {
            return CloseOutcome::StillOpenElsewhere;
        }
        match open_writers.remove(&node_id) {
            Some(FileWriters { content_changed: true, .. }) => CloseOutcome::ContentChanged,
            _ => CloseOutcome::ContentUntouched,
        }
    }

    /// Whether a program holds the node `node_id` open for writing.
    pub(super) fn is_open(&self, node_id: u64) -> bool {
        crate::panic::lock(&self.0).get(&node_id).is_some_and(|writers| writers.handles > 0)
    }
}

#[cfg(test)]
mod tests;

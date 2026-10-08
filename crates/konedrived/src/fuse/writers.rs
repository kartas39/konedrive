//! The files programs hold open for writing: how many handles each has, and whether anything
//! was written through them. A file is sent only once its last writer closed it.

use super::shared::Shared;

/// One file open for writing.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Written {
    /// The handles that may write it.
    pub(super) open: u32,
    /// Its content was changed through one of them, and the outbox knows it waits.
    pub(super) changed: bool,
}

impl Shared {
    /// One more handle writes node `ino`.
    pub(super) fn writer_opened(&self, ino: u64) {
        crate::panic::lock(&self.writers).entry(ino).or_default().open += 1;
    }

    /// Node `ino` is being changed. Whether this is the first change since it was last
    /// closed: the one the outbox is told of before it is made. Every change of a file
    /// nobody holds open is the first.
    pub(super) fn first_change(&self, ino: u64) -> bool {
        match crate::panic::lock(&self.writers).get_mut(&ino) {
            Some(written) => !std::mem::replace(&mut written.changed, true),
            None => true,
        }
    }

    /// A handle that wrote node `ino` was closed. Whether it was the last one of a file that
    /// was changed: its content is ready to be sent.
    pub(super) fn writer_closed(&self, ino: u64) -> bool {
        let mut writers = crate::panic::lock(&self.writers);
        let Some(written) = writers.get_mut(&ino) else { return false };
        written.open = written.open.saturating_sub(1);
        if written.open > 0 {
            return false;
        }
        writers.remove(&ino).is_some_and(|written| written.changed)
    }

    /// Whether a program holds node `ino` open for writing.
    pub(super) fn being_written(&self, ino: u64) -> bool {
        crate::panic::lock(&self.writers).get(&ino).is_some_and(|written| written.open > 0)
    }
}

//! The mode a folder is kept in, said once and carried down a cycle.
//!
//! A read-only folder shows OneDrive and is kept under the read-only lock;
//! a read-write folder also holds the user's own changes
//! (`docs/design/writes.md` §2.2). What read-write mode brings differs by
//! level, so the enum is one and its load is the level's: the folder's part
//! in uploading ([`Writes`](super::listing::Writes)), what one cycle carries
//! to its swap, the rules a pass over the folder goes by
//! ([`Rw`](super::materialize::Rw)).

/// Read-only, or read-write with what that level needs for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode<W> {
    ReadOnly,
    ReadWrite(W),
}

impl<W> Mode<W> {
    /// What read-write mode carries; `None` in read-only mode.
    pub fn read_write(&self) -> Option<&W> {
        match self {
            Mode::ReadOnly => None,
            Mode::ReadWrite(with) => Some(with),
        }
    }

    pub fn is_read_only(&self) -> bool {
        matches!(self, Mode::ReadOnly)
    }

    /// The same mode, carrying what `f` makes of its load.
    pub fn map<U>(self, f: impl FnOnce(W) -> U) -> Mode<U> {
        match self {
            Mode::ReadOnly => Mode::ReadOnly,
            Mode::ReadWrite(with) => Mode::ReadWrite(f(with)),
        }
    }

    pub fn as_ref(&self) -> Mode<&W> {
        match self {
            Mode::ReadOnly => Mode::ReadOnly,
            Mode::ReadWrite(with) => Mode::ReadWrite(with),
        }
    }
}

//! How the file or directory of OneDrive at a path stands: what the daemon's bus interface
//! says of a path, and decides a free-up of it by.

use std::path::PathBuf;

use konedrive_fs::placeholder::State;

use crate::fuse::directory_contents::{EntryState, FileSyncState};
use crate::fuse::free_up_decision::{decide_free_up, FreeUpDecision};
use crate::fuse::state_attribute::to_state_attribute;

/// How the file or directory of OneDrive at a path stands: its id, its pin, and the facts
/// everything else that is said of it is worked out from — its sync state, whether it is
/// being downloaded and whether a program holds it open for writing. Nothing is kept twice:
/// what is shown of it and what a free-up does with it are asked of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStatus {
    /// Its OneDrive id.
    pub onedrive_id: String,
    /// It carries a pin of its own.
    pub own_pin: bool,
    /// The directory above that pins it.
    pub pinned_above: Option<PathBuf>,
    pub(in crate::fuse) entry_state: EntryState,
    pub(in crate::fuse) is_downloading: bool,
    pub(in crate::fuse) is_open_for_writing: bool,
}

impl PathStatus {
    pub fn is_directory(&self) -> bool {
        matches!(self.entry_state, EntryState::Directory(_))
    }

    /// What is shown of it: online-only, being downloaded, or downloaded. `None` for a
    /// directory.
    pub fn state_attribute(&self) -> Option<State> {
        match &self.entry_state {
            EntryState::File(file) => to_state_attribute(file, self.is_downloading),
            EntryState::Directory(_) => None,
        }
    }

    /// Whether it is a file whose content is the user's own and not uploaded yet: changed
    /// here, or held open for writing by a program.
    pub fn is_modified_here(&self) -> bool {
        matches!(self.entry_state, EntryState::File(FileSyncState::ModifiedHere(..))) || self.is_open_for_writing
    }

    /// What a free-up does with it as it stands ([`decide_free_up`]).
    pub fn decide_free_up(&self) -> FreeUpDecision {
        decide_free_up(&self.entry_state, self.own_pin || self.pinned_above.is_some(), self.is_downloading, self.is_open_for_writing)
    }

    /// What a free-up does with it once its own pin is taken off: only a directory above
    /// still pins it.
    pub fn decide_free_up_once_unpinned(&self) -> FreeUpDecision {
        decide_free_up(&self.entry_state, self.pinned_above.is_some(), self.is_downloading, self.is_open_for_writing)
    }
}

#[cfg(test)]
mod tests;

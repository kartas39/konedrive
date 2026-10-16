//! What the folder's sync last ran into ([`SyncTrouble`]): what a cycle with OneDrive failed
//! on ([`CycleError`], which `listing::listing_cycle` returns), or why the sync could not
//! start. Held in the snapshot; `blocking` trouble makes `RootState` read `error`, the rest
//! is said in `LastError` and retried. Their sentences are `crate::sentences`'s.

use konedrive_tree::TreeError;

use super::folder_trouble::FolderTrouble;

/// What a cycle with OneDrive failed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CycleError {
    SignedOut,
    /// The folder was listed from another drive than the one the account reaches: that
    /// drive.
    OtherAccount(String),
    /// The account is signed in to a drive another account of this daemon has
    /// (`docs/design/accounts.md` §6.1); that account's label. Two folders of one drive would
    /// download everything twice.
    DriveTaken(String),
    /// OneDrive cannot be reached: what the request ran into.
    Offline(String),
    /// A failure of the tree store, in its own words ("the tree store: …"), that ends
    /// the cycle and stops the folder.
    Store(String),
    /// What OneDrive answered could not be written into the folder: why.
    Apply(String),
    Cancelled,
}

impl std::error::Error for CycleError {}

impl CycleError {
    /// Trouble that stops the folder until someone acts.
    pub(crate) fn blocking(&self) -> bool {
        matches!(
            self,
            CycleError::SignedOut | CycleError::OtherAccount(_) | CycleError::DriveTaken(_) | CycleError::Store(_)
        )
    }

    /// The kind of trouble this is for the account's overall state: only
    /// [`Offline`](CycleError::Offline) is OneDrive out of reach.
    pub(crate) fn kind(&self) -> TroubleKind {
        match self {
            CycleError::Offline(_) => TroubleKind::Unreachable,
            _ => TroubleKind::Other,
        }
    }
}

impl From<TreeError> for CycleError {
    fn from(e: TreeError) -> Self {
        CycleError::Store(e.to_string())
    }
}

/// What the folder's sync last ran into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncTrouble {
    /// A cycle with OneDrive failed.
    Cycle(CycleError),
    /// The sync could not start, or a fault took its mount down: blocking until a start
    /// stays up.
    NotStarted(FolderTrouble),
}

impl SyncTrouble {
    /// Trouble that stops the folder until someone acts: `RootState` reads `error`.
    pub(crate) fn blocking(&self) -> bool {
        match self {
            SyncTrouble::Cycle(error) => error.blocking(),
            SyncTrouble::NotStarted(_) => true,
        }
    }

    /// What `status::overall` decides by, never by a sentence.
    pub(crate) fn kind(&self) -> TroubleKind {
        match self {
            SyncTrouble::Cycle(error) => error.kind(),
            SyncTrouble::NotStarted(_) => TroubleKind::Other,
        }
    }
}

/// The kinds of trouble that the account's overall state tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TroubleKind {
    /// OneDrive cannot be reached: the account reads offline when nothing else is wrong.
    Unreachable,
    /// Anything else.
    Other,
}

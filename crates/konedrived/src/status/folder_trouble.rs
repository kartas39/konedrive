//! Why an account's folder is not up, or its sync does not run, as cases with their data:
//! what `LastError`, `Trouble` and a `NotUp` refusal say of the folder. Written by the
//! owner of the folder's state (`account_folder`), held in the snapshot; their sentences are
//! `crate::sentences`'s.

use std::path::PathBuf;

use crate::config::AccountHold;

/// What is wrong with the folder at `path`, which its owner decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderTrouble {
    /// `config.toml` gives the account what an earlier account has: its folder is not
    /// brought up.
    HeldBack { hold: AccountHold },
    /// The folder recorded is not at its path: it was moved or deleted, or another folder
    /// stands there.
    Moved { path: PathBuf },
    /// Nothing gives the recorded folder its root id.
    NoRootId { path: PathBuf },
    /// The folder could not be registered as a root: `detail` is why.
    RootNotRegistered { path: PathBuf, detail: String },
    /// `config.toml` says what the folder shows in a word that is not `"onedrive"`.
    UnreadSource { path: PathBuf, written: String },
    /// Bringing the folder up panicked: what the panic said.
    BringUpPanicked { path: PathBuf, detail: String },
    /// No drive is known for the account yet: it has not signed in.
    DriveNotKnown { path: PathBuf },
    /// The folder's store could not be opened, read or written at the start.
    StoreFailed { path: PathBuf, detail: String },
    /// The backing directory, the mount or a thread of it could not be made at the start.
    MountFailed { path: PathBuf, detail: String },
    /// The folder holds the placeholders of a konedrive from before the mount.
    LegacyPlaceholders { path: PathBuf },
    /// A fault took the folder's mount down.
    TakenDown { path: PathBuf, cause: TakeDownCause },
}

/// Why a mount cannot go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeDownCause {
    /// A change of the backing directory whose row could not be written could not be
    /// undone either: the disk has at `path` what no row says.
    UndoFailed { path: PathBuf },
    /// A backing file could not be made or cut after the row that says so was written:
    /// the row says of the file at `path` what the disk does not hold.
    BackingFileBehindRow { path: PathBuf },
}

//! What is said of a folder's trouble (`crate::status::folder_trouble`,
//! `crate::status::sync_trouble`) and of what holds its uploads and downloads
//! (`UploadQueueNote`, `DiskSpaceShortage`).

use std::fmt;

use crate::status::activity::human_size;
use crate::status::folder_trouble::{FolderTrouble, TakeDownCause};
use crate::status::snapshot::{DiskSpaceShortage, UploadQueueNote};
use crate::status::sync_trouble::{CycleError, SyncTrouble};

impl fmt::Display for FolderTrouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FolderTrouble::HeldBack { hold } => {
                write!(f, "this account is held back: {hold}; correct config.toml and start konedrived again")
            }
            FolderTrouble::Moved { path } => write!(
                f,
                "the sync folder is not at {} any more: it was moved or deleted, or another folder stands in its place; \
                 move the sync folder back, or forget it",
                path.display()
            ),
            FolderTrouble::NoRootId { path } => write!(
                f,
                "cannot bring up the sync folder {}: config.toml does not record its root id, and the folder carries none \
                 that can be read",
                path.display()
            ),
            FolderTrouble::RootNotRegistered { path, detail } => write!(f, "cannot bring up the sync folder {}: {detail}", path.display()),
            FolderTrouble::UnreadSource { path, written } => write!(
                f,
                "cannot bring up the sync folder {}: config.toml has source = {written:?} for it, which is not \"onedrive\"; \
                 correct it and start konedrive again, or forget the folder and add it again",
                path.display()
            ),
            FolderTrouble::BringUpPanicked { path, detail } => write!(
                f,
                "bringing up the sync folder {} failed inside konedrive ({detail}); nothing is synced until it is brought up again",
                path.display()
            ),
            FolderTrouble::DriveNotKnown { path } => write!(f, "{} is not synced yet: sign in to this account first", path.display()),
            FolderTrouble::StoreFailed { path, detail } | FolderTrouble::MountFailed { path, detail } => {
                write!(f, "{} is not synced: {detail}", path.display())
            }
            FolderTrouble::LegacyPlaceholders { path } => write!(
                f,
                "{} was set up by an older version of konedrive, which is no longer supported. Remove this account and add it again.",
                path.display()
            ),
            FolderTrouble::TakenDown { path, cause } => write!(
                f,
                "the sync folder {} was stopped: {cause}; konedrive checks the folder and starts it again by itself, and a \
                 refresh tries at once",
                path.display()
            ),
        }
    }
}

impl fmt::Display for TakeDownCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TakeDownCause::UndoFailed { path } => write!(f, "a change of {} could not be recorded and could not be undone", path.display()),
            TakeDownCause::BackingFileBehindRow { path } => {
                write!(f, "{} could not be made or cut on disk after the change was recorded", path.display())
            }
        }
    }
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CycleError::SignedOut => write!(f, "signed out: sign in again to keep this folder in step with OneDrive"),
            CycleError::OtherAccount(drive) => write!(
                f,
                "this folder was listed from another OneDrive account (drive {drive}); forget it and register a folder for the \
                 account signed in now"
            ),
            CycleError::DriveTaken(label) => write!(
                f,
                "this account is signed in to the Microsoft account already connected as '{label}'; sign it out, or remove one \
                 of the two"
            ),
            CycleError::Offline(detail) => write!(f, "cannot reach OneDrive ({detail}); trying again"),
            CycleError::Store(detail) => write!(f, "{detail}"),
            CycleError::Apply(detail) => write!(f, "the folder could not be brought up to date: {detail}"),
            CycleError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl fmt::Display for SyncTrouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncTrouble::Cycle(error) => error.fmt(f),
            SyncTrouble::NotStarted(trouble) => trouble.fmt(f),
        }
    }
}

impl fmt::Display for UploadQueueNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The minute the wait is over in, never one already past when it ends.
            UploadQueueNote::Throttled(until) => write!(
                f,
                "OneDrive asked to slow down; uploads continue at {}",
                clock(until.div_euclid(60) * 60 + if until.rem_euclid(60) > 0 { 60 } else { 0 })
            ),
            UploadQueueNote::WritesNotAllowed(why) => write!(f, "nothing is uploaded: {}", super::account_sentences::say_writes_refusal(why)),
        }
    }
}

impl fmt::Display for DiskSpaceShortage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let files = if self.count == 1 { "1 file kept on this device waits".to_owned() } else { format!("{} files kept on this device wait", self.count) };
        write!(
            f,
            "{files} for disk space: {} to download, {} free, {} GiB stay free",
            human_size(self.bytes),
            human_size(self.free_space),
            self.reserve >> 30
        )
    }
}

/// `at` (Unix seconds) as the clock on the wall shows it here, `HH:MM`.
fn clock(at: i64) -> String {
    let seconds = at as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers are valid for
    // the call and nothing keeps them after it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return "a later time".to_owned();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use crate::config::AccountHold;
use crate::status::folder_trouble::{FolderTrouble, TakeDownCause};
use crate::status::snapshot::{DiskSpaceShortage, UploadQueueNote};
use crate::status::sync_trouble::{CycleError, SyncTrouble};

fn folder() -> PathBuf {
    PathBuf::from("/home/u/OneDrive")
}

/// Each reason a folder is not up, or its sync does not run, names the folder and what to
/// do about it.
#[test]
fn each_trouble_of_a_folder_names_the_folder_and_what_to_do() {
    let path = folder;
    let detail = || "disk I/O error".to_owned();
    let said = [
        (
            FolderTrouble::HeldBack { hold: AccountHold::SameLabel { other: "Home".into() } },
            "this account is held back: its label is also the label of \"Home\"; correct config.toml and start konedrived again",
        ),
        (
            FolderTrouble::Moved { path: path() },
            "the sync folder is not at /home/u/OneDrive any more: it was moved or deleted, or another folder stands in its \
             place; move the sync folder back, or forget it",
        ),
        (
            FolderTrouble::NoRootId { path: path() },
            "cannot bring up the sync folder /home/u/OneDrive: config.toml does not record its root id, and the folder \
             carries none that can be read",
        ),
        (FolderTrouble::RootNotRegistered { path: path(), detail: detail() }, "cannot bring up the sync folder /home/u/OneDrive: disk I/O error"),
        (
            FolderTrouble::UnreadSource { path: path(), written: "dropbox".into() },
            "cannot bring up the sync folder /home/u/OneDrive: config.toml has source = \"dropbox\" for it, which is not \
             \"onedrive\"; correct it and start konedrive again, or forget the folder and add it again",
        ),
        (
            FolderTrouble::BringUpPanicked { path: path(), detail: "index out of bounds".into() },
            "bringing up the sync folder /home/u/OneDrive failed inside konedrive (index out of bounds); nothing is synced \
             until it is brought up again",
        ),
        (FolderTrouble::DriveNotKnown { path: path() }, "/home/u/OneDrive is not synced yet: sign in to this account first"),
        (FolderTrouble::StoreFailed { path: path(), detail: detail() }, "/home/u/OneDrive is not synced: disk I/O error"),
        (FolderTrouble::MountFailed { path: path(), detail: detail() }, "/home/u/OneDrive is not synced: disk I/O error"),
        (
            FolderTrouble::LegacyPlaceholders { path: path() },
            "/home/u/OneDrive was set up by an older version of konedrive, which is no longer supported. Remove this \
             account and add it again.",
        ),
    ];
    for (trouble, sentence) in said {
        assert_eq!(trouble.to_string(), sentence);
    }
}

/// A folder a fault took down names the folder, the file the fault was about and what the
/// user can do; each cause has a sentence of its own.
#[test]
fn a_take_down_names_the_folder_the_cause_and_what_the_user_can_do() {
    let said = [
        (
            TakeDownCause::UndoFailed { path: PathBuf::from("/home/u/OneDrive/a.txt") },
            "the sync folder /home/u/OneDrive was stopped: a change of /home/u/OneDrive/a.txt could not be recorded and could \
             not be undone; konedrive checks the folder and starts it again by itself, and a refresh tries at once",
        ),
        (
            TakeDownCause::BackingFileBehindRow { path: PathBuf::from("/home/u/OneDrive/b.txt") },
            "the sync folder /home/u/OneDrive was stopped: /home/u/OneDrive/b.txt could not be made or cut on disk after the \
             change was recorded; konedrive checks the folder and starts it again by itself, and a refresh tries at once",
        ),
    ];
    for (cause, sentence) in said {
        assert_eq!(FolderTrouble::TakenDown { path: folder(), cause }.to_string(), sentence);
    }
}

/// Each way a cycle with OneDrive fails, and the sync's trouble says the same of it as the
/// case it holds.
#[test]
fn each_failure_of_a_cycle_says_what_happened() {
    let said = [
        (CycleError::SignedOut, "signed out: sign in again to keep this folder in step with OneDrive"),
        (
            CycleError::OtherAccount("D2".into()),
            "this folder was listed from another OneDrive account (drive D2); forget it and register a folder for the account \
             signed in now",
        ),
        (
            CycleError::DriveTaken("Home".into()),
            "this account is signed in to the Microsoft account already connected as 'Home'; sign it out, or remove one of the two",
        ),
        (CycleError::Offline("timed out".into()), "cannot reach OneDrive (timed out); trying again"),
        (CycleError::Store("the tree store: disk I/O error".into()), "the tree store: disk I/O error"),
        (CycleError::Apply("a name repeats".into()), "the folder could not be brought up to date: a name repeats"),
        (CycleError::Cancelled, "cancelled"),
    ];
    for (error, sentence) in said {
        assert_eq!(error.to_string(), sentence);
        assert_eq!(SyncTrouble::Cycle(error).to_string(), sentence);
    }
    let not_started = FolderTrouble::DriveNotKnown { path: folder() };
    assert_eq!(SyncTrouble::NotStarted(not_started.clone()).to_string(), not_started.to_string());
}

/// The throttle's note names the minute the wait is over in: a time within a minute is
/// rounded up, so the note never names a minute that is past while the wait lasts.
#[test]
fn the_throttles_time_is_rounded_up_to_the_minute() {
    let minute = 1_700_000_040; // a multiple of 60
    assert_eq!(minute % 60, 0);
    let said = |at| UploadQueueNote::Throttled(at).to_string();
    assert_eq!(said(minute + 1), said(minute + 60), "a second into the minute reads as the next minute");
    assert_eq!(said(minute + 59), said(minute + 60));
    assert_ne!(said(minute), said(minute + 1), "the full minute reads as itself");
    assert!(said(minute).starts_with("OneDrive asked to slow down; uploads continue at "), "{}", said(minute));
    assert_eq!(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()).to_string(), "nothing is uploaded: the sign-in cannot write");
}

/// The pinned files that wait for disk space: how many, what they need and what there is.
#[test]
fn a_shortage_of_disk_space_says_how_many_files_wait_and_for_what() {
    let shortage = |count| DiskSpaceShortage { count, bytes: 3 << 20, free_space: 1 << 20, reserve: 5 << 30 };
    assert_eq!(shortage(1).to_string(), "1 file kept on this device waits for disk space: 3.0 MiB to download, 1.0 MiB free, 5 GiB stay free");
    assert_eq!(shortage(4).to_string(), "4 files kept on this device wait for disk space: 3.0 MiB to download, 1.0 MiB free, 5 GiB stay free");
}

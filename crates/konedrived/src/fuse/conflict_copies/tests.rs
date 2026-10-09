use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::TreeStore;

use crate::fuse::access_mode_switch::AccessMode;
use super::{ConflictCopies, ConflictCopy, CycleConflicts, MoveAsideOutcome, RemovedDirectoryOutcome};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::directory_contents::tests::drive;
use crate::fuse::directory_contents::DirectoryContents;
use crate::fuse::upload_rules::tests::rules;

/// The conflict copies of the drive the directory contents' tests have, on the machine
/// `fedora`, over a backing directory that holds nothing.
fn conflict_copies() -> (tempfile::TempDir, ConflictCopies) {
    let mut store = TreeStore::in_memory().unwrap();
    drive(&mut store, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let backing_directory = Arc::new(BackingDirectory::open(dir.path()).unwrap());
    let directory_contents = Arc::new(DirectoryContents::new(store, Arc::clone(&backing_directory)));
    (dir, ConflictCopies::new(backing_directory, directory_contents, Arc::new(rules()), "fedora".into()))
}

#[test]
fn a_conflict_copys_name_carries_the_machines() {
    let (_dir, conflict_copies) = conflict_copies();
    assert_eq!(conflict_copies.copy_name("Report.docx", 1), "Report-fedora.docx");
    assert_eq!(conflict_copies.copy_name("Report.docx", 2), "Report-fedora-2.docx");
}

#[test]
fn a_local_only_file_in_the_way_in_a_read_write_folder_is_moved_aside_and_uploaded_from_there() {
    let (dir, conflict_copies) = conflict_copies();
    fs::write(dir.path().join("new.txt"), b"mine").unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.move_aside(Path::new("new.txt"), Some("R"), AccessMode::ReadWrite, 1_000, &mut conflicts), MoveAsideOutcome::MovedAside);
    assert!(!dir.path().join("new.txt").exists() && dir.path().join("new-fedora.txt").is_file());
    assert_eq!(conflicts.moved_aside, [ConflictCopy { original: PathBuf::from("new.txt"), copy: PathBuf::from("new-fedora.txt") }]);
    assert_eq!(conflicts.detections.iter().map(|d| (d.kind, d.rel.clone())).collect::<Vec<_>>(), [(OutboxKind::Create, PathBuf::from("new-fedora.txt"))]);
}

/// The user's own is never hidden or overwritten, so it is moved aside all the same; but a
/// read-only folder's outbox takes no row.
#[test]
fn a_local_only_file_in_the_way_in_a_read_only_folder_is_moved_aside_and_not_recorded() {
    let (dir, conflict_copies) = conflict_copies();
    fs::write(dir.path().join("new.txt"), b"mine").unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.move_aside(Path::new("new.txt"), Some("R"), AccessMode::ReadOnly, 1_000, &mut conflicts), MoveAsideOutcome::MovedAside);
    assert!(!dir.path().join("new.txt").exists());
    assert_eq!(fs::read(dir.path().join("new-fedora.txt")).unwrap(), b"mine");
    assert_eq!(conflicts.moved_aside, [ConflictCopy { original: PathBuf::from("new.txt"), copy: PathBuf::from("new-fedora.txt") }]);
    assert!(conflicts.detections.is_empty() && conflicts.ops.is_empty());
}

/// Every conflict copy's name of it is taken: it stays where it is, and that is said.
#[test]
fn a_local_only_file_that_no_name_is_free_for_is_still_in_the_way() {
    let (dir, conflict_copies) = conflict_copies();
    fs::write(dir.path().join("new.txt"), b"mine").unwrap();
    for n in 1..=100 {
        fs::write(dir.path().join(conflict_copies.copy_name("new.txt", n)), b"an earlier copy").unwrap();
    }
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.move_aside(Path::new("new.txt"), Some("R"), AccessMode::ReadWrite, 1_000, &mut conflicts), MoveAsideOutcome::StillInTheWay);
    assert_eq!(fs::read(dir.path().join("new.txt")).unwrap(), b"mine");
    assert!(conflicts.moved_aside.is_empty() && conflicts.detections.is_empty());
}

/// What is marked with a OneDrive id is OneDrive's own, and stays where it is.
#[test]
fn what_is_marked_with_a_onedrive_id_is_not_moved_aside() {
    let (dir, conflict_copies) = conflict_copies();
    fs::write(dir.path().join("a.txt"), b"copy").unwrap();
    conflict_copies.backing_directory.mark_downloaded_copy(Path::new("a.txt"), "A").unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.move_aside(Path::new("a.txt"), Some("R"), AccessMode::ReadWrite, 1_000, &mut conflicts), MoveAsideOutcome::NothingInTheWay);
    assert!(dir.path().join("a.txt").is_file() && conflicts.moved_aside.is_empty() && conflicts.detections.is_empty());
}

#[test]
fn a_removed_directory_that_holds_local_only_entries_stays_and_is_made_again() {
    let (dir, conflict_copies) = conflict_copies();
    fs::create_dir(dir.path().join("docs")).unwrap();
    conflict_copies.backing_directory.mark_with_onedrive_id(Path::new("docs"), "D").unwrap();
    fs::write(dir.path().join("docs/own.txt"), b"mine").unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.remove_directory_if_empty("D", Path::new("docs"), AccessMode::ReadWrite, 1_000, &mut conflicts), RemovedDirectoryOutcome::KeptAsLocalOnly);
    assert_eq!(conflict_copies.backing_directory.read_onedrive_id(Path::new("docs")), None);
    assert_eq!(conflicts.kept_directories, ["D"]);
    assert_eq!(conflicts.detections.iter().map(|d| (d.kind, d.rel.clone())).collect::<Vec<_>>(), [(OutboxKind::Mkdir, PathBuf::from("docs")), (OutboxKind::Create, PathBuf::from("docs/own.txt"))]);
}

#[test]
fn a_removed_directory_that_holds_nothing_is_removed_from_disk() {
    let (dir, conflict_copies) = conflict_copies();
    fs::create_dir(dir.path().join("docs")).unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.remove_directory_if_empty("D", Path::new("docs"), AccessMode::ReadWrite, 1_000, &mut conflicts), RemovedDirectoryOutcome::Removed);
    assert!(!dir.path().join("docs").exists() && conflicts.kept_directories.is_empty());
}

/// A conflict copy of the name stands there already: the next name is taken.
#[test]
fn a_conflict_copy_gets_the_first_name_that_is_free() {
    let (dir, conflict_copies) = conflict_copies();
    fs::write(dir.path().join("new.txt"), b"mine").unwrap();
    fs::write(dir.path().join("new-fedora.txt"), b"an earlier copy").unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.move_aside(Path::new("new.txt"), Some("R"), AccessMode::ReadWrite, 1_000, &mut conflicts), MoveAsideOutcome::MovedAside);
    assert_eq!(conflicts.moved_aside, [ConflictCopy { original: PathBuf::from("new.txt"), copy: PathBuf::from("new-fedora-2.txt") }]);
    assert_eq!(fs::read(dir.path().join("new-fedora.txt")).unwrap(), b"an earlier copy");
}

/// In a read-only folder the directory stays, and nothing is recorded for it.
#[test]
fn a_removed_directory_kept_in_a_read_only_folder_is_not_made_again() {
    let (dir, conflict_copies) = conflict_copies();
    fs::create_dir(dir.path().join("docs")).unwrap();
    conflict_copies.backing_directory.mark_with_onedrive_id(Path::new("docs"), "D").unwrap();
    fs::write(dir.path().join("docs/own.txt"), b"mine").unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.remove_directory_if_empty("D", Path::new("docs"), AccessMode::ReadOnly, 1_000, &mut conflicts), RemovedDirectoryOutcome::KeptAsLocalOnly);
    assert!(dir.path().join("docs/own.txt").is_file());
    assert_eq!(conflict_copies.backing_directory.read_onedrive_id(Path::new("docs")), None);
    assert!(conflicts.kept_directories.is_empty() && conflicts.detections.is_empty());
}

/// A directory on disk that is marked with another id backs that other directory of OneDrive.
#[test]
fn a_directory_marked_with_another_id_is_left_alone() {
    let (dir, conflict_copies) = conflict_copies();
    fs::create_dir(dir.path().join("docs")).unwrap();
    conflict_copies.backing_directory.mark_with_onedrive_id(Path::new("docs"), "OTHER").unwrap();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.remove_directory_if_empty("D", Path::new("docs"), AccessMode::ReadWrite, 1_000, &mut conflicts), RemovedDirectoryOutcome::LeftForOtherDirectory);
    assert!(dir.path().join("docs").is_dir());
    assert_eq!(conflict_copies.backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("OTHER"));
}

/// OneDrive removed a directory the backing directory never had: nothing is kept, and
/// nothing is recorded.
#[test]
fn a_removed_directory_the_backing_directory_never_had_is_nothing_to_keep() {
    let (dir, conflict_copies) = conflict_copies();
    let mut conflicts = CycleConflicts::default();
    assert_eq!(conflict_copies.remove_directory_if_empty("D", Path::new("docs"), AccessMode::ReadWrite, 1_000, &mut conflicts), RemovedDirectoryOutcome::Removed);
    assert!(!dir.path().join("docs").exists());
    assert!(conflicts.kept_directories.is_empty() && conflicts.detections.is_empty());
}

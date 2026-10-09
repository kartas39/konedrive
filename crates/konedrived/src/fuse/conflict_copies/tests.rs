use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::TreeStore;

use crate::fuse::access_mode_switch::AccessMode;
use super::{ConflictCopies, CycleConflicts};
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
fn a_local_only_file_in_the_way_is_moved_aside_and_uploaded_from_there() {
    let (dir, conflict_copies) = conflict_copies();
    fs::write(dir.path().join("new.txt"), b"mine").unwrap();
    let mut conflicts = CycleConflicts::default();
    conflict_copies.move_aside(Path::new("new.txt"), Some("R"), 1_000, &mut conflicts);
    assert!(!dir.path().join("new.txt").exists() && dir.path().join("new-fedora.txt").is_file());
    assert_eq!(conflicts.moved_aside, [(PathBuf::from("new.txt"), PathBuf::from("new-fedora.txt"))]);
    assert_eq!(conflicts.detections.iter().map(|d| (d.kind, d.rel.clone())).collect::<Vec<_>>(), [(OutboxKind::Create, PathBuf::from("new-fedora.txt"))]);
}

/// What is marked with a OneDrive id is OneDrive's own, and stays where it is.
#[test]
fn what_is_marked_with_a_onedrive_id_is_not_moved_aside() {
    let (dir, conflict_copies) = conflict_copies();
    fs::write(dir.path().join("a.txt"), b"copy").unwrap();
    conflict_copies.backing_directory.mark_downloaded_copy(Path::new("a.txt"), "A").unwrap();
    let mut conflicts = CycleConflicts::default();
    conflict_copies.move_aside(Path::new("a.txt"), Some("R"), 1_000, &mut conflicts);
    assert!(dir.path().join("a.txt").is_file() && conflicts.moved_aside.is_empty() && conflicts.detections.is_empty());
}

#[test]
fn a_removed_directory_that_holds_local_only_entries_stays_and_is_made_again() {
    let (dir, conflict_copies) = conflict_copies();
    fs::create_dir(dir.path().join("docs")).unwrap();
    conflict_copies.backing_directory.mark_with_onedrive_id(Path::new("docs"), "D").unwrap();
    fs::write(dir.path().join("docs/own.txt"), b"mine").unwrap();
    let mut conflicts = CycleConflicts::default();
    conflict_copies.keep_removed_directory("D", Path::new("docs"), AccessMode::ReadWrite, 1_000, &mut conflicts);
    assert_eq!(conflict_copies.backing_directory.read_onedrive_id(Path::new("docs")), None);
    assert_eq!(conflicts.kept_directories, ["D"]);
    assert_eq!(conflicts.detections.iter().map(|d| (d.kind, d.rel.clone())).collect::<Vec<_>>(), [(OutboxKind::Mkdir, PathBuf::from("docs")), (OutboxKind::Create, PathBuf::from("docs/own.txt"))]);
}

#[test]
fn a_removed_directory_that_holds_nothing_is_removed_from_disk() {
    let (dir, conflict_copies) = conflict_copies();
    fs::create_dir(dir.path().join("docs")).unwrap();
    let mut conflicts = CycleConflicts::default();
    conflict_copies.keep_removed_directory("D", Path::new("docs"), AccessMode::ReadWrite, 1_000, &mut conflicts);
    assert!(!dir.path().join("docs").exists() && conflicts.kept_directories.is_empty());
}

use std::path::PathBuf;

use fuser::Errno;
use konedrive_tree::Kind;

use super::{check_make, choose_made_entry_record, EntryToMake, MadeEntryRecord, MakePlaceContents};
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::upload_rules::UploadDecision;

fn docs() -> DirectoryReference {
    DirectoryReference { onedrive_id: Some("D".into()), path: PathBuf::from("docs") }
}

/// A free name in `docs`.
fn free_place() -> MakePlaceContents {
    MakePlaceContents { parent_directory: Some(docs()), is_in_root: false, name_is_daemon_owned: false, name_is_trash: false, name_is_taken: false }
}

#[test]
fn a_free_name_is_made_in_the_directory_the_node_stands_for() {
    for entry_to_make in [EntryToMake::Directory, EntryToMake::File, EntryToMake::FileOpenedForWriting] {
        assert_eq!(check_make(entry_to_make, free_place()), Ok(docs()));
    }
}

#[test]
fn a_node_that_stands_for_no_directory_is_refused_first() {
    let place = MakePlaceContents { parent_directory: None, name_is_daemon_owned: true, name_is_taken: true, ..free_place() };
    assert_eq!(check_make(EntryToMake::File, place), Err(Errno::ENOENT));
}

#[test]
fn one_of_the_daemons_own_names_is_refused_before_a_name_that_is_taken() {
    let place = MakePlaceContents { name_is_daemon_owned: true, name_is_taken: true, ..free_place() };
    assert_eq!(check_make(EntryToMake::File, place.clone()), Err(Errno::EPERM));
    assert_eq!(check_make(EntryToMake::Directory, place), Err(Errno::EPERM));
}

#[test]
fn a_trash_directory_is_refused_in_the_folder_itself_and_nowhere_else() {
    let in_root = MakePlaceContents { is_in_root: true, name_is_trash: true, ..free_place() };
    assert_eq!(check_make(EntryToMake::Directory, in_root.clone()), Err(Errno::EPERM));
    // A file of that name is no trash directory.
    assert!(check_make(EntryToMake::File, in_root).is_ok());
    let deeper = MakePlaceContents { is_in_root: false, name_is_trash: true, ..free_place() };
    assert!(check_make(EntryToMake::Directory, deeper).is_ok());
}

#[test]
fn a_name_that_is_taken_is_refused() {
    let place = MakePlaceContents { name_is_taken: true, ..free_place() };
    assert_eq!(check_make(EntryToMake::Directory, place), Err(Errno::EEXIST));
}

#[test]
fn what_is_made_says_its_kind() {
    assert_eq!(EntryToMake::Directory.kind(), EntryKind::Directory);
    assert_eq!(EntryToMake::File.kind(), EntryKind::File);
    assert_eq!(EntryToMake::FileOpenedForWriting.kind(), EntryKind::File);
}

#[test]
fn a_new_entry_at_a_place_that_is_uploaded_is_recorded_as_fresh() {
    assert_eq!(choose_made_entry_record(EntryToMake::File, None, UploadDecision::Upload), MadeEntryRecord::Fresh);
    assert_eq!(choose_made_entry_record(EntryToMake::Directory, None, UploadDecision::Upload), MadeEntryRecord::Fresh);
}

#[test]
fn a_new_entry_at_a_place_that_is_ignored_is_not_recorded() {
    assert_eq!(choose_made_entry_record(EntryToMake::FileOpenedForWriting, None, UploadDecision::Ignore), MadeEntryRecord::NotRecorded);
    assert_eq!(choose_made_entry_record(EntryToMake::Directory, None, UploadDecision::Ignore), MadeEntryRecord::NotRecorded);
}

/// Also at an ignored place: the file is in OneDrive already.
#[test]
fn a_file_made_where_a_onedrive_file_was_removed_is_that_files_new_content() {
    let base = row("A", Some("R"), "a.tmp", Kind::File);
    assert_eq!(choose_made_entry_record(EntryToMake::File, Some(base.clone()), UploadDecision::Upload), MadeEntryRecord::ReplacingRemovedFile(base.clone()));
    assert_eq!(choose_made_entry_record(EntryToMake::FileOpenedForWriting, Some(base.clone()), UploadDecision::Ignore), MadeEntryRecord::ReplacingRemovedFile(base));
}

#[test]
fn a_directory_made_where_a_onedrive_file_was_removed_is_a_fresh_directory() {
    let base = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(choose_made_entry_record(EntryToMake::Directory, Some(base), UploadDecision::Upload), MadeEntryRecord::Fresh);
}

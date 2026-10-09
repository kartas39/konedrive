use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::Kind;

use super::{CloseOutcome, Fh, OpenFile, OpenHandles};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::ListedName;
use crate::fuse::node_table::NodeId;

fn backing_file() -> Arc<File> {
    Arc::new(tempfile::tempfile().unwrap())
}

/// A file of OneDrive opened to be read while it is not downloaded.
fn remote_only(onedrive_id: &str) -> OpenFile {
    OpenFile::ReadingRemoteOnly { onedrive_id: onedrive_id.to_owned(), path: PathBuf::from("a.txt") }
}

/// A writer of the node `node_id`, which is the OneDrive file `onedrive_id`.
fn add_writer(open_handles: &OpenHandles, node_id: u64, onedrive_id: Option<&str>, written: bool) -> (Fh, bool) {
    let added = open_handles.add_writer(NodeId(node_id), onedrive_id.map(str::to_owned), backing_file(), written);
    (added.fh, added.is_first)
}

#[test]
fn each_open_file_and_directory_has_an_fh_of_its_own_and_none_is_the_one_of_a_file_not_opened() {
    let open_handles = OpenHandles::default();
    let first = open_handles.add_file(remote_only("A"));
    let (second, _) = add_writer(&open_handles, 2, Some("A"), false);
    let directory = open_handles.add_directory();
    assert!(first != second && second != directory && first != directory);
    assert!(first != Fh::NOT_OPENED && second != Fh::NOT_OPENED && directory != Fh::NOT_OPENED);
}

#[test]
fn the_file_held_open_by_an_fh_is_found_until_it_is_closed() {
    let open_handles = OpenHandles::default();
    let fh = open_handles.add_file(remote_only("A"));
    assert!(matches!(open_handles.find_file(fh), Some(OpenFile::ReadingRemoteOnly { onedrive_id, .. }) if onedrive_id == "A"));
    assert!(open_handles.find_file(Fh(fh.0 + 1)).is_none());
    assert!(matches!(open_handles.close_file(fh), (Some(OpenFile::ReadingRemoteOnly { .. }), CloseOutcome::NotOpen)));
    assert!(open_handles.find_file(fh).is_none());
}

/// A file that is downloaded while it is held open is read from the backing directory from
/// then on.
#[test]
fn a_file_opened_while_it_was_not_downloaded_is_switched_to_its_backing_file() {
    let open_handles = OpenHandles::default();
    let fh = open_handles.add_file(remote_only("A"));
    open_handles.switch_to_backing_file(fh, backing_file());
    assert!(matches!(open_handles.find_file(fh), Some(OpenFile::ReadingBackingFile { .. })));
}

#[test]
fn only_a_file_opened_for_writing_is_written_through_its_fh() {
    let open_handles = OpenHandles::default();
    let reader = open_handles.add_file(OpenFile::ReadingBackingFile { backing_file: backing_file() });
    let (writer, _) = add_writer(&open_handles, 3, None, false);
    assert!(open_handles.find_file_to_write(reader).is_none());
    assert_eq!(open_handles.find_file_to_write(writer).map(|(node_id, _)| node_id), Some(NodeId(3)));
    assert!(open_handles.find_file_to_write(Fh(writer.0 + 1)).is_none());
}

#[test]
fn the_first_writer_of_a_node_is_told_apart_from_another() {
    let open_handles = OpenHandles::default();
    assert!(add_writer(&open_handles, 7, None, false).1, "the first writer");
    assert!(!add_writer(&open_handles, 7, None, false).1, "another writer of the same node");
    assert!(add_writer(&open_handles, 8, None, false).1, "the first writer of another node");
}

#[test]
fn closing_one_of_two_writers_leaves_the_file_open_elsewhere() {
    let open_handles = OpenHandles::default();
    let (first, _) = add_writer(&open_handles, 7, None, false);
    let (second, _) = add_writer(&open_handles, 7, None, false);
    assert_eq!(open_handles.close_file(first).1, CloseOutcome::StillOpenElsewhere);
    assert!(open_handles.is_open_for_writing_by_node_id(NodeId(7)));
    assert_eq!(open_handles.close_file(second).1, CloseOutcome::ContentUntouched);
    assert!(!open_handles.is_open_for_writing_by_node_id(NodeId(7)));
}

#[test]
fn the_last_writer_of_a_file_that_was_written_leaves_its_content_changed() {
    let open_handles = OpenHandles::default();
    let (fh, _) = add_writer(&open_handles, 7, None, false);
    open_handles.mark_written(NodeId(7));
    assert_eq!(open_handles.close_file(fh).1, CloseOutcome::ContentChanged);
    // What was written is remembered only while the file is open for writing.
    let (again, _) = add_writer(&open_handles, 7, None, false);
    assert_eq!(open_handles.close_file(again).1, CloseOutcome::ContentUntouched);
}

/// A file whose content is the user's own already when it is opened counts as changed with
/// nothing written through the handle.
#[test]
fn a_writer_of_content_that_was_the_users_own_leaves_it_changed() {
    let open_handles = OpenHandles::default();
    let (fh, _) = add_writer(&open_handles, 7, None, true);
    assert_eq!(open_handles.close_file(fh).1, CloseOutcome::ContentChanged);
}

#[test]
fn the_last_writer_of_a_file_nothing_was_written_to_leaves_its_content_untouched() {
    let open_handles = OpenHandles::default();
    let (fh, _) = add_writer(&open_handles, 7, None, false);
    // Written to another node, and to one that no handle writes: neither counts.
    open_handles.mark_written(NodeId(8));
    assert_eq!(open_handles.close_file(fh).1, CloseOutcome::ContentUntouched);
}

#[test]
fn closing_what_was_not_open_for_writing_says_so() {
    let open_handles = OpenHandles::default();
    let reader = open_handles.add_file(remote_only("A"));
    let directory = open_handles.add_directory();
    assert_eq!(open_handles.close_file(reader).1, CloseOutcome::NotOpen);
    assert!(matches!(open_handles.close_file(Fh(99)), (None, CloseOutcome::NotOpen)));
    assert!(matches!(open_handles.close_file(Fh::NOT_OPENED), (None, CloseOutcome::NotOpen)));
    // A directory is not closed as a file: it is still open.
    assert!(matches!(open_handles.close_file(directory), (None, CloseOutcome::NotOpen)));
    open_handles.keep_listed_names(directory, Arc::new(Vec::new()));
    assert!(open_handles.find_listed_names(directory).is_some());
}

#[test]
fn a_file_is_open_for_writing_by_node_id_while_a_handle_writes_it() {
    let open_handles = OpenHandles::default();
    open_handles.add_file(remote_only("A"));
    assert!(!open_handles.is_open_for_writing_by_node_id(NodeId(7)));
    let (fh, _) = add_writer(&open_handles, 7, None, false);
    assert!(open_handles.is_open_for_writing_by_node_id(NodeId(7)) && !open_handles.is_open_for_writing_by_node_id(NodeId(8)));
    open_handles.close_file(fh);
    assert!(!open_handles.is_open_for_writing_by_node_id(NodeId(7)));
}

/// Free-up asks this: the sync state alone does not say that a program writes the file.
#[test]
fn a_onedrive_file_is_open_for_writing_by_onedrive_id_while_a_handle_writes_it() {
    let open_handles = OpenHandles::default();
    open_handles.add_file(remote_only("A"));
    assert!(!open_handles.is_open_for_writing_by_onedrive_id("A"), "a reader does not write it");
    let (writer, _) = add_writer(&open_handles, 2, Some("A"), false);
    add_writer(&open_handles, 3, None, false);
    assert!(open_handles.is_open_for_writing_by_onedrive_id("A") && !open_handles.is_open_for_writing_by_onedrive_id("B"));
    open_handles.close_file(writer);
    assert!(!open_handles.is_open_for_writing_by_onedrive_id("A"));
}

#[test]
fn an_open_directory_keeps_the_names_it_listed_until_it_is_closed() {
    let open_handles = OpenHandles::default();
    let fh = open_handles.add_directory();
    assert!(open_handles.find_listed_names(fh).is_none(), "nothing is kept before it is read");
    let listed_names = Arc::new(vec![ListedName::InOneDrive(row("A", Some("R"), "a.txt", Kind::File))]);
    open_handles.keep_listed_names(fh, Arc::clone(&listed_names));
    assert_eq!(open_handles.find_listed_names(fh), Some(listed_names));
    open_handles.close_directory(fh);
    assert!(open_handles.find_listed_names(fh).is_none());
}

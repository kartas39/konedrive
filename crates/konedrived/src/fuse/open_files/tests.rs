use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::Kind;

use crate::fuse::open_files::Fh;
use crate::fuse::node_table::NodeId;
use super::{OpenFile, OpenFiles};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::ListedName;

/// A file held open by the node `node_id`, with nothing on disk.
fn open_file(node_id: u64, onedrive_id: Option<&str>, may_write: bool) -> OpenFile {
    OpenFile { node_id: NodeId(node_id), onedrive_id: onedrive_id.map(str::to_owned), rel: PathBuf::from("a.txt"), file_on_disk: None, may_write }
}

#[test]
fn each_open_file_has_a_file_handle_of_its_own_and_none_is_zero() {
    let open_files = OpenFiles::default();
    let first = open_files.add_file(open_file(2, Some("A"), false));
    let second = open_files.add_file(open_file(2, Some("A"), false));
    let directory = open_files.add_directory();
    assert!(first != Fh::NOT_OPENED && second != Fh::NOT_OPENED && directory != Fh::NOT_OPENED);
    assert!(first != second && second != directory && first != directory);
}

#[test]
fn a_file_is_found_by_its_file_handle_until_it_is_closed() {
    let open_files = OpenFiles::default();
    let handle = open_files.add_file(open_file(7, Some("A"), true));
    let found = open_files.find_file(handle).unwrap();
    assert!(found.node_id == NodeId(7) && found.onedrive_id.as_deref() == Some("A") && found.may_write && found.file_on_disk.is_none());
    assert!(open_files.find_file(Fh(handle.0 + 1)).is_none());
    assert_eq!(open_files.remove_file(handle).map(|closed| closed.node_id), Some(NodeId(7)));
    assert!(open_files.find_file(handle).is_none() && open_files.remove_file(handle).is_none());
}

/// A file that is downloaded after it was opened is read from the backing directory from
/// then on.
#[test]
fn an_open_file_gets_its_file_on_disk_once_there_is_one() {
    let open_files = OpenFiles::default();
    let handle = open_files.add_file(open_file(7, Some("A"), false));
    open_files.set_file_on_disk(handle, Arc::new(tempfile::tempfile().unwrap()));
    assert!(open_files.find_file(handle).unwrap().file_on_disk.is_some());
}

/// A write goes only through a handle that may write, into the file it has on disk.
#[test]
fn only_a_handle_that_may_write_gives_a_file_to_write() {
    let open_files = OpenFiles::default();
    let on_disk = || Some(Arc::new(tempfile::tempfile().unwrap()));
    let reader = open_files.add_file(OpenFile { file_on_disk: on_disk(), ..open_file(2, Some("A"), false) });
    let writer = open_files.add_file(OpenFile { file_on_disk: on_disk(), ..open_file(3, Some("A"), true) });
    assert!(open_files.find_file_to_write(reader).is_none());
    assert_eq!(open_files.find_file_to_write(writer).map(|(node_id, _)| node_id), Some(NodeId(3)));
    assert!(open_files.find_file_to_write(Fh(writer.0 + 1)).is_none());
}

/// What free-up asks: only a handle that may write counts, only of that OneDrive file, and
/// only until it is closed.
#[test]
fn a_onedrive_file_is_open_for_writing_while_a_handle_that_may_write_holds_it() {
    let open_files = OpenFiles::default();
    let reader = open_files.add_file(open_file(2, Some("A"), false));
    assert!(!open_files.is_open_for_writing("A"));
    let writer = open_files.add_file(open_file(2, Some("A"), true));
    let local_only = open_files.add_file(open_file(3, None, true));
    assert!(open_files.is_open_for_writing("A") && !open_files.is_open_for_writing("B"));
    open_files.remove_file(reader);
    assert!(open_files.is_open_for_writing("A"));
    open_files.remove_file(writer);
    assert!(!open_files.is_open_for_writing("A"));
    open_files.remove_file(local_only);
}

#[test]
fn an_open_directory_keeps_what_it_held_when_it_was_first_read_until_it_is_closed() {
    let open_files = OpenFiles::default();
    let (first, second) = (open_files.add_directory(), open_files.add_directory());
    assert!(open_files.find_listed_names(first).is_none());
    let listed_names = Arc::new(vec![ListedName::InOneDrive(row("A", Some("R"), "a.txt", Kind::File))]);
    open_files.keep_listed_names(first, Arc::clone(&listed_names));
    assert_eq!(open_files.find_listed_names(first), Some(listed_names));
    // Another open directory has its own.
    assert!(open_files.find_listed_names(second).is_none());
    // Read from its start again, it is listed anew.
    open_files.keep_listed_names(first, Arc::new(Vec::new()));
    assert!(open_files.find_listed_names(first).unwrap().is_empty());
    open_files.remove_directory(first);
    assert!(open_files.find_listed_names(first).is_none());
}

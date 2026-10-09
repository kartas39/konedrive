use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::{Store, TreeStore};

use super::list_pinned_files_to_download;
use crate::fuse::directory_contents::tests::{directory_contents, download};
use crate::fuse::pins::Pins;

/// The ids and paths of what the pins `pinned_ids` have to download in the fixture's drive
/// (`docs/c.txt`, `a.txt`, `b.txt`), where `downloaded` (a path and its id) is on disk.
fn to_download(pinned_ids: &[&str], downloaded: Option<(&str, &str)>) -> Vec<(String, PathBuf)> {
    let (dir, contents) = directory_contents(Vec::new());
    if let Some((rel, id)) = downloaded {
        download(dir.path(), &contents, rel, id, b"content");
    }
    let contents = Arc::new(contents);
    let pinned_ids = pinned_ids.iter().map(|id| (*id).to_owned()).collect();
    let pins = Pins::new(pinned_ids, Store::new(TreeStore::in_memory().unwrap()), Arc::clone(&contents));
    let mut files: Vec<(String, PathBuf)> = list_pinned_files_to_download(&pins, &contents).into_iter().map(|file| (file.row.id, file.path)).collect();
    files.sort();
    files
}

#[test]
fn a_pinned_file_that_is_not_downloaded_is_to_download() {
    assert_eq!(to_download(&["A"], None), [("A".to_owned(), PathBuf::from("a.txt"))]);
}

#[test]
fn a_pinned_file_that_is_downloaded_is_not() {
    assert!(to_download(&["A"], Some(("a.txt", "A"))).is_empty());
}

#[test]
fn a_pinned_directory_downloads_what_is_below_it_and_not_what_is_beside_it() {
    assert_eq!(to_download(&["D"], None), [("C".to_owned(), PathBuf::from("docs/c.txt"))]);
}

#[test]
fn without_a_pin_nothing_is_to_download() {
    assert!(to_download(&[], None).is_empty());
}

/// A pin on an id the store no longer has keeps nothing.
#[test]
fn a_pin_on_what_is_gone_downloads_nothing() {
    assert!(to_download(&["gone"], None).is_empty());
}

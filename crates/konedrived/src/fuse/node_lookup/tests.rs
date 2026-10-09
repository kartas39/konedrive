use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::{Detection, OutboxKind};
use konedrive_tree::Row;

use super::NodeLookup;
use crate::fuse::node_table::NodeTable;
use crate::fuse::directory_contents::tests::{of, directory_contents};
use crate::fuse::directory_contents::{Entry, Local, DirectoryContents};

/// The nodes of the drive the view's tests have (`docs` with `c.txt`, and `a.txt` and
/// `b.txt` in the root), over a folder on disk that holds nothing.
fn node_lookup(detections: Vec<Detection>) -> (tempfile::TempDir, Arc<DirectoryContents>, NodeLookup) {
    let (dir, directory_contents) = directory_contents(detections);
    let directory_contents = Arc::new(directory_contents);
    let node_table = Arc::new(Mutex::new(NodeTable { next: 1, ..NodeTable::default() }));
    (dir, Arc::clone(&directory_contents), NodeLookup::new(directory_contents, node_table))
}

fn item(directory_contents: &DirectoryContents, id: &str) -> Entry {
    Entry::Item(directory_contents.row(id).unwrap())
}

fn local(directory_contents: &DirectoryContents, rel: &str) -> Entry {
    let object = directory_contents.backing_directory().find_disk_entry(Path::new(rel)).unwrap();
    Entry::Local(Local { name: Path::new(rel).file_name().unwrap().to_string_lossy().into_owned(), object })
}

fn id(found: Option<(Entry, PathBuf)>) -> Option<(Option<String>, PathBuf)> {
    found.map(|(entry, rel)| (entry.id().map(str::to_owned), rel))
}

#[test]
fn an_entry_gets_one_node_and_keeps_it_when_it_is_renamed() {
    let (_dir, directory_contents, node_lookup) = node_lookup(Vec::new());
    let docs = node_lookup.assign_node_id(&item(&directory_contents, "D"), 1);
    let file = node_lookup.assign_node_id(&item(&directory_contents, "A"), 1);
    assert!(file != docs && file != 1 && node_lookup.assign_node_id(&item(&directory_contents, "A"), 1) == file);
    assert_eq!(id(node_lookup.find_entry(file)), Some((Some("A".into()), PathBuf::from("a.txt"))));
    // Shown under another name in another directory: the same node, at its new place.
    let renamed = Entry::Item(Row { name: "b.txt".into(), ..directory_contents.row("A").unwrap() });
    assert_eq!(node_lookup.assign_node_id(&renamed, docs), file);
    assert_eq!(id(node_lookup.find_entry(file)), Some((Some("A".into()), PathBuf::from("docs/b.txt"))));
    assert!(matches!(node_lookup.find_entry(file), Some((entry, _)) if entry.name() == "b.txt"));
    assert_eq!(node_lookup.find_node_id("A"), Some(file));
}

#[test]
fn the_root_is_node_one_and_a_directory() {
    let (_dir, directory_contents, node_lookup) = node_lookup(Vec::new());
    assert_eq!(id(node_lookup.find_entry(1)), Some((Some("R".into()), PathBuf::new())));
    assert_eq!(node_lookup.find_directory(1).map(|dir| (dir.id, dir.rel)), Some((Some("R".into()), PathBuf::new())));
    let docs = node_lookup.assign_node_id(&item(&directory_contents, "D"), 1);
    let file = node_lookup.assign_node_id(&item(&directory_contents, "C"), docs);
    assert!(node_lookup.find_directory(file).is_none());
    assert_eq!(node_lookup.find_parent(file).map(|(node_id, dir)| (node_id, dir.id, dir.rel)), Some((docs, Some("D".into()), PathBuf::from("docs"))));
    assert!(node_lookup.find_entry(99).is_none() && node_lookup.find_parent(1).is_none());
}

/// A file made here is a node by its object. Once it is uploaded it carries its item's id,
/// and its node is the item's: programs that hold it open go on with the same file.
#[test]
fn an_object_that_got_its_item_keeps_its_node() {
    let (dir, directory_contents, node_lookup) = node_lookup(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let node = node_lookup.assign_node_id(&local(&directory_contents, "made.txt"), 1);
    assert!(matches!(node_lookup.find_entry(node), Some((Entry::Local(_), _))));
    xattr::set(dir.path().join("made.txt"), XATTR_ITEM_ID, b"A").unwrap();
    assert_eq!(id(node_lookup.find_entry(node)), Some((Some("A".into()), PathBuf::from("made.txt"))));
    assert_eq!(node_lookup.find_node_id("A"), Some(node));
}

/// The same, found from the name: the item is looked up where its object was known.
#[test]
fn an_item_looked_up_where_its_object_was_known_gets_the_objects_node() {
    let (dir, directory_contents, node_lookup) = node_lookup(Vec::new());
    fs::write(dir.path().join("b.txt"), b"mine").unwrap();
    let node = node_lookup.assign_node_id(&local(&directory_contents, "b.txt"), 1);
    assert_eq!(node_lookup.assign_node_id(&item(&directory_contents, "B"), 1), node);
    assert_eq!(id(node_lookup.find_entry(node)), Some((Some("B".into()), PathBuf::from("b.txt"))));
}

#[test]
fn a_node_whose_object_is_gone_is_nothing() {
    let (dir, directory_contents, node_lookup) = node_lookup(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let node = node_lookup.assign_node_id(&local(&directory_contents, "made.txt"), 1);
    // Another file under its name is another object.
    fs::rename(dir.path().join("made.txt"), dir.path().join("kept.txt")).unwrap();
    fs::write(dir.path().join("made.txt"), b"other").unwrap();
    assert!(node_lookup.find_entry(node).is_none());
    fs::remove_file(dir.path().join("made.txt")).unwrap();
    assert!(node_lookup.find_entry(node).is_none());
}

#[test]
fn a_node_whose_item_was_removed_here_is_nothing() {
    let (_dir, directory_contents, node_lookup) = node_lookup(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    let node = node_lookup.assign_node_id(&item(&directory_contents, "B"), 1);
    assert!(node_lookup.find_entry(node).is_none());
}

/// A file without an item saved over an item's file: the file's node is the item's from
/// then on, at the item's name.
#[test]
fn an_object_renamed_over_an_items_name_is_that_items_node() {
    let (dir, directory_contents, node_lookup) = node_lookup(Vec::new());
    fs::write(dir.path().join("a.tmp"), b"new").unwrap();
    let entry = local(&directory_contents, "a.tmp");
    let Entry::Local(saved) = &entry else { unreachable!() };
    let node = node_lookup.assign_node_id(&entry, 1);
    fs::rename(dir.path().join("a.tmp"), dir.path().join("a.txt")).unwrap();
    node_lookup.move_node(node, 1, "a.txt", Some((&saved.object.handle, "A")));
    assert_eq!(node_lookup.find_node_id("A"), Some(node));
    assert_eq!(id(node_lookup.find_entry(node)), Some((Some("A".into()), PathBuf::from("a.txt"))));
}

#[test]
fn an_object_that_is_forgotten_gets_a_new_node_if_it_is_ever_seen_again() {
    let (dir, directory_contents, node_lookup) = node_lookup(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let entry = local(&directory_contents, "made.txt");
    let Entry::Local(made) = &entry else { unreachable!() };
    let node = node_lookup.assign_node_id(&entry, 1);
    node_lookup.forget(&made.object.handle);
    assert!(node_lookup.find_entry(node).is_none() && node_lookup.find_parent(node).is_none());
    assert_ne!(node_lookup.assign_node_id(&entry, 1), node);
}

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::{Detection, OutboxKind};
use konedrive_tree::Row;

use super::Resolve;
use crate::fuse::nodes::Nodes;
use crate::fuse::view::tests::{of, view};
use crate::fuse::view::{Entry, Local, View};

/// The nodes of the drive the view's tests have (`docs` with `c.txt`, and `a.txt` and
/// `b.txt` in the root), over a folder on disk that holds nothing.
fn resolve(detections: Vec<Detection>) -> (tempfile::TempDir, Arc<View>, Resolve) {
    let (dir, view) = view(detections);
    let view = Arc::new(view);
    let nodes = Arc::new(Mutex::new(Nodes { next: 1, ..Nodes::default() }));
    (dir, Arc::clone(&view), Resolve::new(view, nodes))
}

fn item(view: &View, id: &str) -> Entry {
    Entry::Item(view.row(id).unwrap())
}

fn local(view: &View, rel: &str) -> Entry {
    let object = view.under().object(Path::new(rel)).unwrap();
    Entry::Local(Local { name: Path::new(rel).file_name().unwrap().to_string_lossy().into_owned(), object })
}

fn id(found: Option<(Entry, PathBuf)>) -> Option<(Option<String>, PathBuf)> {
    found.map(|(entry, rel)| (entry.id().map(str::to_owned), rel))
}

#[test]
fn an_entry_gets_one_node_and_keeps_it_when_it_is_renamed() {
    let (_dir, view, resolve) = resolve(Vec::new());
    let docs = resolve.ino_of(&item(&view, "D"), 1);
    let file = resolve.ino_of(&item(&view, "A"), 1);
    assert!(file != docs && file != 1 && resolve.ino_of(&item(&view, "A"), 1) == file);
    assert_eq!(id(resolve.node(file)), Some((Some("A".into()), PathBuf::from("a.txt"))));
    // Shown under another name in another directory: the same node, at its new place.
    let renamed = Entry::Item(Row { name: "b.txt".into(), ..view.row("A").unwrap() });
    assert_eq!(resolve.ino_of(&renamed, docs), file);
    assert_eq!(id(resolve.node(file)), Some((Some("A".into()), PathBuf::from("docs/b.txt"))));
    assert!(matches!(resolve.node(file), Some((entry, _)) if entry.name() == "b.txt"));
    assert_eq!(resolve.item_node("A"), Some(file));
}

#[test]
fn the_root_is_node_one_and_a_directory() {
    let (_dir, view, resolve) = resolve(Vec::new());
    assert_eq!(id(resolve.node(1)), Some((Some("R".into()), PathBuf::new())));
    assert_eq!(resolve.dir(1).map(|dir| (dir.id, dir.rel)), Some((Some("R".into()), PathBuf::new())));
    let docs = resolve.ino_of(&item(&view, "D"), 1);
    let file = resolve.ino_of(&item(&view, "C"), docs);
    assert!(resolve.dir(file).is_none());
    assert_eq!(resolve.above(file).map(|(ino, dir)| (ino, dir.id, dir.rel)), Some((docs, Some("D".into()), PathBuf::from("docs"))));
    assert!(resolve.node(99).is_none() && resolve.above(1).is_none());
}

/// A file made here is a node by its object. Once it is uploaded it carries its item's id,
/// and its node is the item's: programs that hold it open go on with the same file.
#[test]
fn an_object_that_got_its_item_keeps_its_node() {
    let (dir, view, resolve) = resolve(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let node = resolve.ino_of(&local(&view, "made.txt"), 1);
    assert!(matches!(resolve.node(node), Some((Entry::Local(_), _))));
    xattr::set(dir.path().join("made.txt"), XATTR_ITEM_ID, b"A").unwrap();
    assert_eq!(id(resolve.node(node)), Some((Some("A".into()), PathBuf::from("made.txt"))));
    assert_eq!(resolve.item_node("A"), Some(node));
}

/// The same, found from the name: the item is looked up where its object was known.
#[test]
fn an_item_looked_up_where_its_object_was_known_gets_the_objects_node() {
    let (dir, view, resolve) = resolve(Vec::new());
    fs::write(dir.path().join("b.txt"), b"mine").unwrap();
    let node = resolve.ino_of(&local(&view, "b.txt"), 1);
    assert_eq!(resolve.ino_of(&item(&view, "B"), 1), node);
    assert_eq!(id(resolve.node(node)), Some((Some("B".into()), PathBuf::from("b.txt"))));
}

#[test]
fn a_node_whose_object_is_gone_is_nothing() {
    let (dir, view, resolve) = resolve(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let node = resolve.ino_of(&local(&view, "made.txt"), 1);
    // Another file under its name is another object.
    fs::rename(dir.path().join("made.txt"), dir.path().join("kept.txt")).unwrap();
    fs::write(dir.path().join("made.txt"), b"other").unwrap();
    assert!(resolve.node(node).is_none());
    fs::remove_file(dir.path().join("made.txt")).unwrap();
    assert!(resolve.node(node).is_none());
}

#[test]
fn a_node_whose_item_was_removed_here_is_nothing() {
    let (_dir, view, resolve) = resolve(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    let node = resolve.ino_of(&item(&view, "B"), 1);
    assert!(resolve.node(node).is_none());
}

/// A file without an item saved over an item's file: the file's node is the item's from
/// then on, at the item's name.
#[test]
fn an_object_renamed_over_an_items_name_is_that_items_node() {
    let (dir, view, resolve) = resolve(Vec::new());
    fs::write(dir.path().join("a.tmp"), b"new").unwrap();
    let entry = local(&view, "a.tmp");
    let Entry::Local(saved) = &entry else { unreachable!() };
    let node = resolve.ino_of(&entry, 1);
    fs::rename(dir.path().join("a.tmp"), dir.path().join("a.txt")).unwrap();
    resolve.renamed(node, 1, "a.txt", Some((&saved.object.handle, "A")));
    assert_eq!(resolve.item_node("A"), Some(node));
    assert_eq!(id(resolve.node(node)), Some((Some("A".into()), PathBuf::from("a.txt"))));
}

#[test]
fn an_object_that_is_forgotten_gets_a_new_node_if_it_is_ever_seen_again() {
    let (dir, view, resolve) = resolve(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let entry = local(&view, "made.txt");
    let Entry::Local(made) = &entry else { unreachable!() };
    let node = resolve.ino_of(&entry, 1);
    resolve.forget(&made.object.handle);
    assert!(resolve.node(node).is_none() && resolve.above(node).is_none());
    assert_ne!(resolve.ino_of(&entry, 1), node);
}

use std::path::Path;

use konedrive_fs::handle::FileHandle;

use super::{NodeKey, NodeTable};

fn item(id: &str) -> NodeKey {
    NodeKey::InOneDrive(id.into())
}

/// An object by a handle as ext4 gives it: the inode's number, then its generation.
fn local(ino: u32, generation: u32) -> NodeKey {
    NodeKey::LocalOnly(FileHandle { kind: 1, bytes: [ino.to_le_bytes(), generation.to_le_bytes()].concat() })
}

fn nodes() -> NodeTable {
    NodeTable { next: 1, ..NodeTable::default() }
}

/// The kernel keeps a node id for as long as it knows the item: the id an item was given is
/// the one it has under any name and in any directory.
#[test]
fn an_item_keeps_its_node_when_it_is_renamed_or_moved() {
    let mut nodes = nodes();
    let docs = nodes.ino(item("D"), 1, "docs");
    let file = nodes.ino(item("F"), docs, "a.txt");
    assert_eq!(nodes.rel(file).as_deref(), Some(Path::new("docs/a.txt")));
    assert_eq!(nodes.ino(item("F"), 1, "b.txt"), file);
    assert_eq!(nodes.rel(file).as_deref(), Some(Path::new("b.txt")));
}

#[test]
fn two_items_never_share_a_node_and_none_is_the_root() {
    let mut nodes = nodes();
    let (a, b) = (nodes.ino(item("A"), 1, "a"), nodes.ino(item("B"), 1, "b"));
    assert!(a != b && a != 1 && b != 1);
}

/// The root is node 1 and has no name: its path is the folder itself.
#[test]
fn the_roots_path_is_empty_and_an_unknown_nodes_is_none() {
    let nodes = nodes();
    assert_eq!(nodes.rel(1).as_deref(), Some(Path::new("")));
    assert_eq!(nodes.rel(7), None);
}

/// A file made here is a node by its object; once it is uploaded it is its item's node, the
/// same one.
#[test]
fn an_object_that_gets_its_item_keeps_its_node() {
    let mut nodes = nodes();
    let file = nodes.ino(local(42, 1), 1, "new.txt");
    nodes.rekey(&local(42, 1), item("N"));
    assert_eq!(nodes.ino(item("N"), 1, "new.txt"), file);
    assert_eq!(nodes.inos.get(&local(42, 1)), None);
}

/// The filesystem gives the inode number of a removed file to the next one made. The two are
/// two objects, by their handles, and never one node: what a program still holds of the
/// first says nothing of the second.
#[test]
fn two_objects_that_had_one_inode_number_are_two_nodes() {
    let mut nodes = nodes();
    let first = nodes.ino(local(42, 1), 1, "a.txt");
    let second = nodes.ino(local(42, 2), 1, "a.txt");
    assert_ne!(first, second);
    assert_eq!(nodes.ino(local(42, 1), 1, "a.txt"), first);
}

/// A removed object's node is dropped whole: nothing is left of it, and the next object is
/// a node of its own.
#[test]
fn a_forgotten_node_leaves_nothing_behind() {
    let mut nodes = nodes();
    let gone = nodes.ino(local(42, 1), 1, "a.txt");
    nodes.forget(&local(42, 1));
    assert!(nodes.items.is_empty() && nodes.inos.is_empty());
    assert_eq!(nodes.rel(gone), None);
    assert_ne!(nodes.ino(local(42, 1), 1, "a.txt"), gone);
}

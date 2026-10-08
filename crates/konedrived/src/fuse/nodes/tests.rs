use std::path::Path;

use super::{Key, Nodes};

fn item(id: &str) -> Key {
    Key::Item(id.into())
}

fn nodes() -> Nodes {
    Nodes { next: 1, ..Nodes::default() }
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
    let file = nodes.ino(Key::Local(42), 1, "new.txt");
    nodes.rekey(&Key::Local(42), item("N"));
    assert_eq!(nodes.ino(item("N"), 1, "new.txt"), file);
    assert_eq!(nodes.inos.get(&Key::Local(42)), None);
}

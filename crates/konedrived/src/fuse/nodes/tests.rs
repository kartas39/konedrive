use std::path::Path;

use konedrive_tree::{Kind, Placement, Row};

use super::Nodes;

fn row(id: &str, name: &str) -> Row {
    Row { id: id.into(), parent_id: None, name: name.into(), kind: Kind::File, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed }
}

fn nodes() -> Nodes {
    Nodes { next: 1, ..Nodes::default() }
}

/// The kernel keeps a node id for as long as it knows the item: the id an item was given is
/// the one it has under any name and in any directory.
#[test]
fn an_item_keeps_its_node_when_it_is_renamed_or_moved() {
    let mut nodes = nodes();
    let docs = nodes.ino(&row("D", "docs"), 1);
    let file = nodes.ino(&row("F", "a.txt"), docs);
    assert_eq!(nodes.rel(file).as_deref(), Some(Path::new("docs/a.txt")));
    assert_eq!(nodes.ino(&row("F", "b.txt"), 1), file);
    assert_eq!(nodes.rel(file).as_deref(), Some(Path::new("b.txt")));
}

#[test]
fn two_items_never_share_a_node_and_none_is_the_root() {
    let mut nodes = nodes();
    let (a, b) = (nodes.ino(&row("A", "a"), 1), nodes.ino(&row("B", "b"), 1));
    assert!(a != b && a != 1 && b != 1);
}

/// The root is node 1 and has no name: its path is the folder itself.
#[test]
fn the_roots_path_is_empty_and_an_unknown_nodes_is_none() {
    let nodes = nodes();
    assert_eq!(nodes.rel(1).as_deref(), Some(Path::new("")));
    assert_eq!(nodes.rel(7), None);
}

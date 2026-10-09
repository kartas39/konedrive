use std::path::Path;

use konedrive_fs::handle::FileHandle;

use super::{NodeKey, NodeTable};

fn in_onedrive(id: &str) -> NodeKey {
    NodeKey::InOneDrive(id.into())
}

/// A local-only entry by a handle as ext4 gives it: the inode's number, then its generation.
fn local_only(inode: u32, generation: u32) -> NodeKey {
    NodeKey::LocalOnly(FileHandle { kind: 1, bytes: [inode.to_le_bytes(), generation.to_le_bytes()].concat() })
}

/// The kernel keeps a node id for as long as it knows the file: the node id a file of
/// OneDrive was given is the one it has under any name and in any directory.
#[test]
fn a_onedrive_file_keeps_its_node_id_when_it_is_renamed_or_moved() {
    let mut node_table = NodeTable::new();
    let docs = node_table.assign(in_onedrive("D"), 1, "docs");
    let file = node_table.assign(in_onedrive("F"), docs, "a.txt");
    assert_eq!(node_table.find_path(file).as_deref(), Some(Path::new("docs/a.txt")));
    assert_eq!(node_table.assign(in_onedrive("F"), 1, "b.txt"), file);
    assert_eq!(node_table.find_path(file).as_deref(), Some(Path::new("b.txt")));
}

#[test]
fn two_keys_never_share_a_node_id_and_none_is_the_root() {
    let mut node_table = NodeTable::new();
    let (a, b) = (node_table.assign(in_onedrive("A"), 1, "a"), node_table.assign(in_onedrive("B"), 1, "b"));
    assert!(a != b && a != 1 && b != 1);
}

/// The root is node 1 and has no name: its path is the folder itself.
#[test]
fn the_roots_path_is_empty_and_an_unknown_node_ids_is_none() {
    let node_table = NodeTable::new();
    assert_eq!(node_table.find_path(1).as_deref(), Some(Path::new("")));
    assert_eq!(node_table.find_path(7), None);
}

/// A file made here has its node id as a local-only entry; once it is uploaded the same node
/// id stands for the file in OneDrive.
#[test]
fn a_local_only_entry_that_gets_into_onedrive_keeps_its_node_id() {
    let mut node_table = NodeTable::new();
    let file = node_table.assign(local_only(42, 1), 1, "new.txt");
    node_table.rekey(&local_only(42, 1), in_onedrive("N"));
    assert_eq!(node_table.assign(in_onedrive("N"), 1, "new.txt"), file);
    assert_eq!(node_table.find_node_id(&local_only(42, 1)), None);
}

/// The filesystem gives the inode number of a removed file to the next one made. The two are
/// two entries, by their handles, and never one node: what a program still holds of the
/// first says nothing of the second.
#[test]
fn two_local_only_entries_that_had_one_inode_number_have_two_node_ids() {
    let mut node_table = NodeTable::new();
    let first = node_table.assign(local_only(42, 1), 1, "a.txt");
    let second = node_table.assign(local_only(42, 2), 1, "a.txt");
    assert_ne!(first, second);
    assert_eq!(node_table.assign(local_only(42, 1), 1, "a.txt"), first);
}

/// The node of a removed local-only entry is dropped whole: nothing is left of it, and the
/// next entry gets a node id of its own.
#[test]
fn a_dropped_node_leaves_nothing_behind() {
    let mut node_table = NodeTable::new();
    let gone = node_table.assign(local_only(42, 1), 1, "a.txt");
    node_table.drop_node(&local_only(42, 1));
    assert!(node_table.find_record(gone).is_none() && node_table.find_node_id(&local_only(42, 1)).is_none());
    assert_eq!(node_table.find_path(gone), None);
    assert_ne!(node_table.assign(local_only(42, 1), 1, "a.txt"), gone);
}

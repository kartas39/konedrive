//! The extended attributes of a node, as values, on the parts of a mount built without a
//! mount.

use std::ffi::OsStr;

use konedrive_fs::placeholder::{State, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};

use crate::fuse::mount::tests::{build_mount_with, BuiltMount};
use crate::fuse::node_table::NodeId;

/// The node id of `name` in the root of `built`, as a lookup of it gives it.
fn node_id(built: &BuiltMount, name: &str) -> NodeId {
    let (attributes, _) = built.kernel_requests.lookup_requests.find_lookup_answer(false, NodeId::ROOT, OsStr::new(name)).unwrap();
    NodeId::from(attributes.ino)
}

#[test]
fn a_file_of_onedrive_shows_its_id_its_state_and_its_own_pin() {
    let built = build_mount_with(&[], &["A"]);
    let xattr_requests = &built.kernel_requests.xattr_requests;
    let (a, b) = (node_id(&built, "a.txt"), node_id(&built, "b.txt"));
    assert_eq!(xattr_requests.find_xattr(a, XATTR_ITEM_ID).as_deref(), Some("A"));
    assert_eq!(xattr_requests.find_xattr(a, XATTR_STATE).as_deref(), Some(State::OnlineOnly.as_str()));
    assert_eq!(xattr_requests.find_xattr(a, XATTR_PIN).as_deref(), Some("1"));
    assert_eq!(xattr_requests.find_xattr(b, XATTR_PIN), None);
    built.download("a.txt", "A", b"content");
    assert_eq!(xattr_requests.find_xattr(a, XATTR_STATE).as_deref(), Some(State::Hydrated.as_str()));
    // While it is being downloaded, that is what is shown of it.
    xattr_requests.download_states.start_or_join("B", None);
    assert_eq!(xattr_requests.find_xattr(b, XATTR_STATE).as_deref(), Some(State::Hydrating.as_str()));
}

#[test]
fn a_directory_of_onedrive_shows_its_id_and_no_state() {
    let built = build_mount_with(&[], &[]);
    let xattr_requests = &built.kernel_requests.xattr_requests;
    let docs = node_id(&built, "docs");
    assert_eq!(xattr_requests.find_xattr(docs, XATTR_ITEM_ID).as_deref(), Some("D"));
    assert_eq!(xattr_requests.find_xattr(docs, XATTR_STATE), None);
}

/// As its registration and its first cycle wrote them on the backing directory; no other
/// node has them.
#[test]
fn the_folders_own_attributes_are_read_from_the_backing_directory() {
    let built = build_mount_with(&[], &[]);
    let xattr_requests = &built.kernel_requests.xattr_requests;
    xattr::set(&built.disk, XATTR_ROOT, b"R").unwrap();
    xattr::set(&built.disk, XATTR_DRIVE, b"drive").unwrap();
    assert_eq!(xattr_requests.find_xattr(NodeId::ROOT, XATTR_ROOT).as_deref(), Some("R"));
    assert_eq!(xattr_requests.find_xattr(NodeId::ROOT, XATTR_DRIVE).as_deref(), Some("drive"));
    assert_eq!(xattr_requests.find_xattr(node_id(&built, "a.txt"), XATTR_ROOT), None);
}

/// The mark the outbox's worker puts on a file it sends is the disk's, whatever the file is.
#[test]
fn the_upload_mark_is_read_from_the_file_in_the_backing_directory() {
    let built = build_mount_with(&[], &[]);
    let xattr_requests = &built.kernel_requests.xattr_requests;
    std::fs::write(built.disk.join("new.txt"), b"mine").unwrap();
    let new = node_id(&built, "new.txt");
    assert_eq!(xattr_requests.find_xattr(new, XATTR_SYNC), None);
    xattr::set(built.disk.join("new.txt"), XATTR_SYNC, b"uploading").unwrap();
    assert_eq!(xattr_requests.find_xattr(new, XATTR_SYNC).as_deref(), Some("uploading"));
    // A local-only entry has none of the store's.
    assert_eq!(xattr_requests.find_xattr(new, XATTR_ITEM_ID), None);
    assert_eq!(xattr_requests.find_xattr(new, XATTR_STATE), None);
}

#[test]
fn a_node_that_stands_for_nothing_has_no_attribute() {
    let built = build_mount_with(&[], &[]);
    assert_eq!(built.kernel_requests.xattr_requests.find_xattr(NodeId(999), XATTR_ITEM_ID), None);
}

/// Each name ended by a zero byte, and only the names of what the node has.
#[test]
fn the_names_listed_are_those_of_the_attributes_the_node_has() {
    let built = build_mount_with(&[], &["A"]);
    let xattr_requests = &built.kernel_requests.xattr_requests;
    let names = |node_id| String::from_utf8(xattr_requests.list_xattr_names(node_id)).unwrap();
    assert_eq!(names(node_id(&built, "a.txt")), format!("{XATTR_ITEM_ID}\0{XATTR_STATE}\0{XATTR_PIN}\0"));
    assert_eq!(names(node_id(&built, "docs")), format!("{XATTR_ITEM_ID}\0"));
    std::fs::write(built.disk.join("new.txt"), b"mine").unwrap();
    assert_eq!(names(node_id(&built, "new.txt")), "");
}

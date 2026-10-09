//! What a lookup, a remove the bridge announces and `access` are answered, as values, on the
//! parts of a mount built without a mount: the handlers only reply with them.

use std::ffi::OsStr;
use std::time::Duration;

use fuser::{AccessFlags, Errno, FileType};
use konedrive_tree::Kind;

use crate::fuse::attributes::TTL;
use crate::fuse::inotify_bridge::AnnouncementInProgress;
use crate::fuse::mount::tests::build_mount;
use crate::fuse::node_table::NodeId;

fn name(name: &str) -> &OsStr {
    OsStr::new(name)
}

#[test]
fn a_name_the_folder_has_is_answered_with_its_attributes_and_its_node_id() {
    let built = build_mount();
    let lookup_requests = &built.kernel_requests.lookup_requests;
    let (attributes, ttl) = lookup_requests.find_lookup_answer(false, NodeId::ROOT, name("a.txt")).unwrap();
    assert_eq!((attributes.kind, attributes.size, ttl), (FileType::RegularFile, 1, TTL));
    assert_eq!(lookup_requests.node_resolver.find_node_id("A"), Some(NodeId::from(attributes.ino)));
    let (docs, _) = lookup_requests.find_lookup_answer(false, NodeId::ROOT, name("docs")).unwrap();
    assert_eq!(docs.kind, FileType::Directory);
    // What is in a directory is found by the directory's node id.
    let (inside, _) = lookup_requests.find_lookup_answer(false, NodeId::from(docs.ino), name("c.txt")).unwrap();
    assert_eq!(lookup_requests.node_resolver.find_node_id("C"), Some(NodeId::from(inside.ino)));
}

#[test]
fn a_name_the_folder_does_not_have_is_no_entry() {
    let built = build_mount();
    let lookup_requests = &built.kernel_requests.lookup_requests;
    assert_eq!(lookup_requests.find_lookup_answer(false, NodeId::ROOT, name("none.txt")).err(), Some(Errno::ENOENT));
    assert_eq!(lookup_requests.find_lookup_answer(false, NodeId(999), name("a.txt")).err(), Some(Errno::ENOENT), "a node that stands for no directory");
}

/// The store has the name already; the kernel must hear it is not there, or it would not
/// pass the bridge's create on.
#[test]
fn the_bridge_is_told_that_the_name_it_announces_as_appearing_is_absent() {
    let built = build_mount();
    let lookup_requests = &built.kernel_requests.lookup_requests;
    lookup_requests.inotify_bridge.begin_announcing_on_this_thread(AnnouncementInProgress::NameAppearing { parent: NodeId::ROOT, name: "a.txt".into() });
    assert_eq!(lookup_requests.find_lookup_answer(true, NodeId::ROOT, name("a.txt")).err(), Some(Errno::ENOENT));
    // Everyone else is answered what the folder has, and so is the bridge for another name.
    assert!(lookup_requests.find_lookup_answer(false, NodeId::ROOT, name("a.txt")).is_ok());
    assert!(lookup_requests.find_lookup_answer(true, NodeId::ROOT, name("b.txt")).is_ok());
}

/// The store no longer has the name; the kernel must hear it is still there, with its node,
/// or it would not pass the bridge's remove on. The answer is not to be kept.
#[test]
fn the_bridge_is_told_that_the_name_it_announces_as_vanishing_is_still_there() {
    let built = build_mount();
    let lookup_requests = &built.kernel_requests.lookup_requests;
    lookup_requests.inotify_bridge.begin_announcing_on_this_thread(AnnouncementInProgress::NameVanishing { parent: NodeId::ROOT, name: "gone".into(), node_id: NodeId(77), kind: Kind::Folder });
    let (attributes, ttl) = lookup_requests.find_lookup_answer(true, NodeId::ROOT, name("gone")).unwrap();
    assert_eq!((NodeId::from(attributes.ino), attributes.kind, ttl), (NodeId(77), FileType::Directory, Duration::ZERO));
    assert_eq!(lookup_requests.find_lookup_answer(false, NodeId::ROOT, name("gone")).err(), Some(Errno::ENOENT));
}

#[test]
fn a_remove_the_bridge_announces_is_done_only_for_a_name_that_is_gone() {
    let built = build_mount();
    let lookup_requests = &built.kernel_requests.lookup_requests;
    assert_eq!(lookup_requests.check_announced_remove(NodeId::ROOT, name("gone.txt")), Ok(()));
    assert_eq!(lookup_requests.check_announced_remove(NodeId::ROOT, name("a.txt")), Err(Errno::EROFS));
}

#[test]
fn writing_is_not_allowed_in_a_read_only_folder_and_everything_else_is() {
    let built = build_mount();
    let lookup_requests = &built.kernel_requests.lookup_requests;
    assert_eq!(lookup_requests.check_access(AccessFlags::W_OK), Err(Errno::EROFS));
    assert_eq!(lookup_requests.check_access(AccessFlags::R_OK | AccessFlags::X_OK), Ok(()));
    lookup_requests.access_mode_switch.switch_to_read_write();
    assert_eq!(lookup_requests.check_access(AccessFlags::W_OK | AccessFlags::R_OK), Ok(()));
}

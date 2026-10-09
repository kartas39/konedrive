//! The names an open directory lists, on the parts of a mount built without a mount.

use std::sync::Arc;

use crate::fuse::mount::tests::build_mount;
use crate::fuse::node_table::NodeId;

fn sorted(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = names.collect();
    names.sort();
    names
}

/// A program that reads a directory in parts reads the same names, whatever changes
/// meanwhile; one that reads it from its start again reads it as it is then.
#[test]
fn the_names_listed_at_the_start_are_kept_for_every_later_part_of_the_reading() {
    let built = build_mount();
    let directory_requests = &built.kernel_requests.directory_requests;
    let fh = directory_requests.open_handles.add_directory();
    let (directory, at_start) = directory_requests.find_listed_names(NodeId::ROOT, fh, 0).unwrap();
    assert!(directory.path.as_os_str().is_empty());
    assert_eq!(sorted(at_start.iter().map(|listed_name| listed_name.name().to_owned())), ["a.txt", "b.txt", "docs"]);
    std::fs::write(built.disk.join("late.txt"), b"made meanwhile").unwrap();
    let (_, later) = directory_requests.find_listed_names(NodeId::ROOT, fh, 2).unwrap();
    assert!(Arc::ptr_eq(&at_start, &later), "the names kept with the fh");
    let (_, again) = directory_requests.find_listed_names(NodeId::ROOT, fh, 0).unwrap();
    assert_eq!(sorted(again.iter().map(|listed_name| listed_name.name().to_owned())), ["a.txt", "b.txt", "docs", "late.txt"]);
    // And those are the names kept from now on.
    let (_, after) = directory_requests.find_listed_names(NodeId::ROOT, fh, 1).unwrap();
    assert!(Arc::ptr_eq(&again, &after));
}

/// Each open of a directory has its own names.
#[test]
fn a_directory_that_was_not_read_from_its_start_is_listed_as_it_is() {
    let built = build_mount();
    let directory_requests = &built.kernel_requests.directory_requests;
    let (first, second) = (directory_requests.open_handles.add_directory(), directory_requests.open_handles.add_directory());
    let (_, of_first) = directory_requests.find_listed_names(NodeId::ROOT, first, 0).unwrap();
    std::fs::write(built.disk.join("late.txt"), b"made meanwhile").unwrap();
    let (_, of_second) = directory_requests.find_listed_names(NodeId::ROOT, second, 3).unwrap();
    assert_eq!((of_first.len(), of_second.len()), (3, 4));
}

#[test]
fn a_node_that_stands_for_no_directory_lists_nothing() {
    let built = build_mount();
    let directory_requests = &built.kernel_requests.directory_requests;
    let fh = directory_requests.open_handles.add_directory();
    let (_, names) = directory_requests.find_listed_names(NodeId::ROOT, fh, 0).unwrap();
    let file = names.iter().find(|listed_name| listed_name.name() == "a.txt").unwrap();
    let file_node_id = directory_requests.node_resolver.assign_node_id_of_listed_name(file, NodeId::ROOT);
    assert!(directory_requests.find_listed_names(file_node_id, fh, 0).is_none());
    assert!(directory_requests.find_listed_names(NodeId(999), fh, 0).is_none());
}

use std::fs;
use std::path::Path;

use konedrive_fs::placeholder::{State, XATTR_ITEM_ID, XATTR_STATE};

use crate::fuse::backing_directory::tests::folder;

const AT: &str = "a.txt";

/// A file at `a.txt` that carries `id` and is marked `state`, where each is given.
fn file(dir: &Path, id: Option<&str>, state: Option<State>) {
    let _ = fs::remove_file(dir.join(AT));
    fs::write(dir.join(AT), b"abc").unwrap();
    if let Some(id) = id {
        xattr::set(dir.join(AT), XATTR_ITEM_ID, id.as_bytes()).unwrap();
    }
    if let Some(state) = state {
        xattr::set(dir.join(AT), XATTR_STATE, state.as_str().as_bytes()).unwrap();
    }
}

#[test]
fn a_whole_file_that_carries_the_items_id_is_its_copy() {
    let (dir, under) = folder();
    file(dir.path(), Some("A"), Some(State::Hydrated));
    assert_eq!(under.copy(Path::new(AT), "A").map(|m| m.len()), Some(3));
    assert!(under.has_copy(Path::new(AT), "A"));
}

#[test]
fn the_copy_of_another_item_is_not_this_items() {
    let (dir, under) = folder();
    file(dir.path(), Some("B"), Some(State::Hydrated));
    assert!(!under.has_copy(Path::new(AT), "A"));
}

/// A file the user put in the folder while nothing was mounted.
#[test]
fn a_file_that_carries_no_id_is_no_items_copy() {
    let (dir, under) = folder();
    file(dir.path(), None, None);
    assert!(!under.has_copy(Path::new(AT), "A"));
}

/// A placeholder of an older konedrive has the item's name and id, and none of its content.
#[test]
fn a_file_that_is_not_whole_is_not_a_copy() {
    let (dir, under) = folder();
    file(dir.path(), Some("A"), Some(State::OnlineOnly));
    assert!(!under.has_copy(Path::new(AT), "A"));
    file(dir.path(), Some("A"), None);
    assert!(!under.has_copy(Path::new(AT), "A"));
}

#[test]
fn a_directory_and_a_missing_file_are_no_copy() {
    let (dir, under) = folder();
    assert!(!under.has_copy(Path::new(AT), "A"));
    fs::create_dir(dir.path().join("docs")).unwrap();
    xattr::set(dir.path().join("docs"), XATTR_ITEM_ID, b"A").unwrap();
    xattr::set(dir.path().join("docs"), XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
    assert!(!under.has_copy(Path::new("docs"), "A"));
}

#[test]
fn a_file_marked_as_a_copy_is_the_items_copy() {
    let (dir, under) = folder();
    file(dir.path(), None, None);
    under.mark_copy(Path::new(AT), "A").unwrap();
    assert!(under.has_copy(Path::new(AT), "A") && !under.has_copy(Path::new(AT), "B"));
}

#[test]
fn a_copy_is_removed_as_its_own_items() {
    let (dir, under) = folder();
    file(dir.path(), Some("A"), Some(State::Hydrated));
    assert!(under.remove_copy(Path::new(AT), "A").is_some());
    assert!(under.stat(Path::new(AT)).is_none());
}

#[test]
fn the_copy_of_another_item_and_a_file_of_the_users_own_are_not_removed() {
    let (dir, under) = folder();
    file(dir.path(), Some("B"), Some(State::Hydrated));
    assert_eq!(under.remove_copy(Path::new(AT), "A"), None);
    file(dir.path(), None, None);
    assert_eq!(under.remove_copy(Path::new(AT), "A"), None);
    assert!(under.stat(Path::new(AT)).is_some());
    assert_eq!(under.remove_copy(Path::new("none.txt"), "A"), None);
}

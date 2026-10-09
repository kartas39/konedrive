use std::fs;
use std::path::Path;

use konedrive_fs::placeholder::{State, XATTR_ITEM_ID, XATTR_STATE};

use super::DiskMarks;
use crate::fuse::backing_directory::tests::empty_backing_directory;

const AT: &str = "a.txt";

/// A file at `a.txt` that is marked with the OneDrive id `id` and the state `state`, where each is given.
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
fn a_whole_file_marked_with_the_files_onedrive_id_is_its_downloaded_copy() {
    let (dir, backing_directory) = empty_backing_directory();
    file(dir.path(), Some("A"), Some(State::Hydrated));
    assert_eq!(backing_directory.find_downloaded_copy(Path::new(AT), "A").map(|copy| copy.size), Some(3));
    assert!(backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some());
}

#[test]
fn the_copy_of_another_onedrive_file_is_not_this_files() {
    let (dir, backing_directory) = empty_backing_directory();
    file(dir.path(), Some("B"), Some(State::Hydrated));
    assert!(!backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some());
}

/// A file the user put in the directory while nothing was mounted.
#[test]
fn a_file_marked_with_no_onedrive_id_is_nobodys_copy() {
    let (dir, backing_directory) = empty_backing_directory();
    file(dir.path(), None, None);
    assert!(!backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some());
}

/// A placeholder of an older konedrive has the file's name and OneDrive id, and none of its content.
#[test]
fn a_file_that_is_not_whole_is_not_a_copy() {
    let (dir, backing_directory) = empty_backing_directory();
    file(dir.path(), Some("A"), Some(State::OnlineOnly));
    assert!(!backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some());
    file(dir.path(), Some("A"), None);
    assert!(!backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some());
}

#[test]
fn a_directory_and_a_missing_file_are_no_copy() {
    let (dir, backing_directory) = empty_backing_directory();
    assert!(!backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some());
    fs::create_dir(dir.path().join("docs")).unwrap();
    xattr::set(dir.path().join("docs"), XATTR_ITEM_ID, b"A").unwrap();
    xattr::set(dir.path().join("docs"), XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
    assert!(!backing_directory.find_downloaded_copy(Path::new("docs"), "A").is_some());
}

#[test]
fn a_file_marked_as_a_downloaded_copy_is_the_files_copy() {
    let (dir, backing_directory) = empty_backing_directory();
    file(dir.path(), None, None);
    backing_directory.mark_downloaded_copy(Path::new(AT), "A").unwrap();
    assert!(backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some() && !backing_directory.find_downloaded_copy(Path::new(AT), "B").is_some());
}

#[test]
fn a_downloaded_copy_is_removed_by_its_own_onedrive_id() {
    let (dir, backing_directory) = empty_backing_directory();
    file(dir.path(), Some("A"), Some(State::Hydrated));
    assert!(backing_directory.remove_downloaded_copy(Path::new(AT), "A").is_some());
    assert!(backing_directory.read_metadata(Path::new(AT)).is_none());
}

#[test]
fn the_copy_of_another_onedrive_file_and_a_file_of_the_users_own_are_not_removed() {
    let (dir, backing_directory) = empty_backing_directory();
    file(dir.path(), Some("B"), Some(State::Hydrated));
    assert_eq!(backing_directory.remove_downloaded_copy(Path::new(AT), "A"), None);
    file(dir.path(), None, None);
    assert_eq!(backing_directory.remove_downloaded_copy(Path::new(AT), "A"), None);
    assert!(backing_directory.read_metadata(Path::new(AT)).is_some());
    assert_eq!(backing_directory.remove_downloaded_copy(Path::new("none.txt"), "A"), None);
}

/// The one test of a downloaded copy, and the other things the marks say, case by case.
#[test]
fn the_marks_of_a_file_say_what_it_is() {
    let (dir, _backing_directory) = empty_backing_directory();
    let marks = |id: Option<&str>, state: Option<State>| {
        file(dir.path(), id, state);
        DiskMarks::read(&dir.path().join(AT))
    };
    let whole_copy = marks(Some("A"), Some(State::Hydrated));
    assert!(whole_copy.is_downloaded_copy_of("A") && !whole_copy.is_downloaded_copy_of("B"));
    assert!(whole_copy.has_onedrive_id() && !whole_copy.is_partial_file() && !whole_copy.is_legacy_placeholder());
    // Marked with the id alone: nothing says it is whole.
    let only_id = marks(Some("A"), None);
    assert!(!only_id.is_downloaded_copy_of("A") && only_id.is_partial_file() && only_id.has_onedrive_id() && !only_id.is_legacy_placeholder());
    let placeholder = marks(Some("A"), Some(State::OnlineOnly));
    assert!(!placeholder.is_downloaded_copy_of("A") && placeholder.is_partial_file() && placeholder.is_legacy_placeholder());
    // A state and no id: an older konedrive's, and nobody's copy.
    let state_alone = marks(None, Some(State::OnlineOnly));
    assert!(state_alone.is_legacy_placeholder() && !state_alone.has_onedrive_id() && !state_alone.is_partial_file());
    let whole_without_id = marks(None, Some(State::Hydrated));
    assert!(!whole_without_id.is_downloaded_copy_of("A") && !whole_without_id.is_legacy_placeholder());
    let unmarked = marks(None, None);
    assert!(!unmarked.is_downloaded_copy_of("A") && !unmarked.has_onedrive_id() && !unmarked.is_partial_file() && !unmarked.is_legacy_placeholder());
    assert_eq!(DiskMarks::read(&dir.path().join("nothing-there")), unmarked);
}

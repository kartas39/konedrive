use std::fs;
use std::path::Path;

use crate::fuse::backing_directory::tests::folder;

/// A drive whose root `R` holds `docs` (`D`), which holds `deep` (`E`).
fn item(above: Option<&str>, _dir: &Path, name: &str) -> Option<String> {
    match (above, name) {
        (Some("R"), "docs") => Some("D".to_owned()),
        (Some("D"), "deep") => Some("E".to_owned()),
        _ => None,
    }
}

#[test]
fn each_directory_down_to_a_place_is_made_and_carries_its_items_id() {
    let (_dir, backing_directory) = folder();
    backing_directory.make_marked_directories(Path::new("docs/deep"), Some("R".into()), item).unwrap();
    assert!(backing_directory.stat(Path::new("docs/deep")).is_some_and(|m| m.is_dir()));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("D"));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs/deep")).as_deref(), Some("E"));
}

/// A directory the user made, which OneDrive has no item for yet: it is made, as what stands
/// in it must stand somewhere, and is no item's.
#[test]
fn a_directory_no_item_is_known_for_is_made_and_left_unmarked() {
    let (_dir, backing_directory) = folder();
    backing_directory.make_marked_directories(Path::new("docs/new/deep"), Some("R".into()), item).unwrap();
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("D"));
    assert!(backing_directory.stat(Path::new("docs/new/deep")).is_some_and(|m| m.is_dir()));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs/new")), None);
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs/new/deep")), None);
}

/// The directory an item's name was given back to is that item's from here on, and what it
/// holds stays.
#[test]
fn a_directory_that_stands_already_is_kept_and_marked_as_its_item() {
    let (dir, backing_directory) = folder();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"a").unwrap();
    backing_directory.mark_with_onedrive_id(Path::new("docs"), "other").unwrap();
    backing_directory.make_marked_directories(Path::new("docs"), Some("R".into()), item).unwrap();
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("D"));
    assert!(backing_directory.stat(Path::new("docs/a.txt")).is_some());
}

#[test]
fn the_item_of_a_name_is_asked_with_the_place_of_its_directory() {
    let (_dir, backing_directory) = folder();
    let by_place = |_: Option<&str>, dir: &Path, name: &str| (dir == Path::new("a") && name == "b").then(|| "B".to_owned());
    backing_directory.make_marked_directories(Path::new("a/b"), None, by_place).unwrap();
    assert_eq!((backing_directory.read_onedrive_id(Path::new("a")), backing_directory.read_onedrive_id(Path::new("a/b")).as_deref()), (None, Some("B")));
}

#[test]
fn a_file_in_the_way_of_a_directory_is_an_error() {
    let (dir, backing_directory) = folder();
    fs::write(dir.path().join("docs"), b"a").unwrap();
    assert!(backing_directory.make_marked_directories(Path::new("docs/deep"), Some("R".into()), item).is_err());
}

#[test]
fn the_folder_itself_needs_no_directory() {
    let (_dir, backing_directory) = folder();
    backing_directory.make_marked_directories(Path::new(""), Some("R".into()), item).unwrap();
    assert_eq!(backing_directory.list(Path::new("")).count(), 0);
}

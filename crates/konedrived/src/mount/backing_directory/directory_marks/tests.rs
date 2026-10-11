use std::fs;
use std::path::Path;

use crate::mount::backing_directory::tests::empty_backing_directory;

/// A drive whose root `R` holds `docs` (`D`), which holds `deep` (`E`).
fn id_of(parent_id: Option<&str>, _parent: &Path, name: &str) -> Option<String> {
    match (parent_id, name) {
        (Some("R"), "docs") => Some("D".to_owned()),
        (Some("D"), "deep") => Some("E".to_owned()),
        _ => None,
    }
}

#[test]
fn each_directory_down_to_a_path_is_made_and_marked_with_its_onedrive_id() {
    let (_dir, backing_directory) = empty_backing_directory();
    backing_directory.make_marked_directories(Path::new("docs/deep"), Some("R".into()), id_of).unwrap();
    assert!(backing_directory.read_metadata(Path::new("docs/deep")).is_some_and(|m| m.is_dir()));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("D"));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs/deep")).as_deref(), Some("E"));
}

/// A directory the user made, which is not in OneDrive yet: it is made, as what lies
/// in it must lie somewhere, and is marked with nothing.
#[test]
fn a_directory_no_onedrive_id_is_known_for_is_made_and_left_unmarked() {
    let (_dir, backing_directory) = empty_backing_directory();
    backing_directory.make_marked_directories(Path::new("docs/new/deep"), Some("R".into()), id_of).unwrap();
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("D"));
    assert!(backing_directory.read_metadata(Path::new("docs/new/deep")).is_some_and(|m| m.is_dir()));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs/new")), None);
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs/new/deep")), None);
}

/// The directory a name of OneDrive was given back to backs that directory from here on, and what it
/// holds stays.
#[test]
fn a_directory_that_is_there_already_is_kept_and_marked_with_its_onedrive_id() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"a").unwrap();
    backing_directory.mark_with_onedrive_id(Path::new("docs"), "other").unwrap();
    backing_directory.make_marked_directories(Path::new("docs"), Some("R".into()), id_of).unwrap();
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("D"));
    assert!(backing_directory.read_metadata(Path::new("docs/a.txt")).is_some());
}

#[test]
fn the_id_of_a_name_is_asked_with_the_path_of_the_directory_it_is_in() {
    let (_dir, backing_directory) = empty_backing_directory();
    let by_place = |_: Option<&str>, dir: &Path, name: &str| (dir == Path::new("a") && name == "b").then(|| "B".to_owned());
    backing_directory.make_marked_directories(Path::new("a/b"), None, by_place).unwrap();
    assert_eq!((backing_directory.read_onedrive_id(Path::new("a")), backing_directory.read_onedrive_id(Path::new("a/b")).as_deref()), (None, Some("B")));
}

#[test]
fn a_file_in_the_way_of_a_directory_is_an_error() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("docs"), b"a").unwrap();
    assert!(backing_directory.make_marked_directories(Path::new("docs/deep"), Some("R".into()), id_of).is_err());
}

#[test]
fn the_backing_directory_itself_needs_no_directory() {
    let (_dir, backing_directory) = empty_backing_directory();
    backing_directory.make_marked_directories(Path::new(""), Some("R".into()), id_of).unwrap();
    assert_eq!(backing_directory.list_directory(Path::new("")).count(), 0);
}

/// A file of the user's own that has the name of a directory of OneDrive is not taken for
/// that directory: nothing is made, and the file is marked with nothing.
#[test]
fn a_file_that_stands_where_a_directory_is_to_be_made_is_an_error_and_is_not_marked() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("docs"), b"mine").unwrap();
    let refused = backing_directory.make_marked_directories(Path::new("docs/deep"), Some("R".into()), id_of).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(libc::ENOTDIR));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")), None);
    assert_eq!(fs::read(dir.path().join("docs")).unwrap(), b"mine");
    // Also as the last name of the path.
    assert_eq!(backing_directory.make_marked_directories(Path::new("docs"), Some("R".into()), id_of).unwrap_err().raw_os_error(), Some(libc::ENOTDIR));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")), None);
}

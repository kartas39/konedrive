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
    let (_dir, under) = folder();
    under.backing(Path::new("docs/deep"), Some("R".into()), item).unwrap();
    assert!(under.stat(Path::new("docs/deep")).is_some_and(|m| m.is_dir()));
    assert_eq!(under.carried(Path::new("docs")).as_deref(), Some("D"));
    assert_eq!(under.carried(Path::new("docs/deep")).as_deref(), Some("E"));
}

/// A directory the user made, which OneDrive has no item for yet: it is made, as what stands
/// in it must stand somewhere, and is no item's.
#[test]
fn a_directory_no_item_is_known_for_is_made_and_left_unmarked() {
    let (_dir, under) = folder();
    under.backing(Path::new("docs/new/deep"), Some("R".into()), item).unwrap();
    assert_eq!(under.carried(Path::new("docs")).as_deref(), Some("D"));
    assert!(under.stat(Path::new("docs/new/deep")).is_some_and(|m| m.is_dir()));
    assert_eq!(under.carried(Path::new("docs/new")), None);
    assert_eq!(under.carried(Path::new("docs/new/deep")), None);
}

/// The directory an item's name was given back to is that item's from here on, and what it
/// holds stays.
#[test]
fn a_directory_that_stands_already_is_kept_and_marked_as_its_item() {
    let (dir, under) = folder();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"a").unwrap();
    under.carry(Path::new("docs"), "other").unwrap();
    under.backing(Path::new("docs"), Some("R".into()), item).unwrap();
    assert_eq!(under.carried(Path::new("docs")).as_deref(), Some("D"));
    assert!(under.stat(Path::new("docs/a.txt")).is_some());
}

#[test]
fn the_item_of_a_name_is_asked_with_the_place_of_its_directory() {
    let (_dir, under) = folder();
    let by_place = |_: Option<&str>, dir: &Path, name: &str| (dir == Path::new("a") && name == "b").then(|| "B".to_owned());
    under.backing(Path::new("a/b"), None, by_place).unwrap();
    assert_eq!((under.carried(Path::new("a")), under.carried(Path::new("a/b")).as_deref()), (None, Some("B")));
}

#[test]
fn a_file_in_the_way_of_a_directory_is_an_error() {
    let (dir, under) = folder();
    fs::write(dir.path().join("docs"), b"a").unwrap();
    assert!(under.backing(Path::new("docs/deep"), Some("R".into()), item).is_err());
}

#[test]
fn the_folder_itself_needs_no_directory() {
    let (_dir, under) = folder();
    under.backing(Path::new(""), Some("R".into()), item).unwrap();
    assert_eq!(under.list(Path::new("")).count(), 0);
}

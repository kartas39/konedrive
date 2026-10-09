use std::fs;
use std::path::Path;

use crate::fuse::backing_directory::tests::folder;

#[test]
fn an_object_is_what_stands_at_a_place_with_its_handle() {
    let (dir, under) = folder();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"abc").unwrap();
    let (file, docs) = (under.object(Path::new("docs/a.txt")).unwrap(), under.object(Path::new("docs")).unwrap());
    assert!(file.meta.is_file() && file.meta.len() == 3 && docs.meta.is_dir());
    assert_eq!(under.object(Path::new("docs/a.txt")).unwrap().handle, file.handle);
    assert!(under.object(Path::new("docs/none.txt")).is_none());
}

#[test]
fn two_files_are_two_objects() {
    let (dir, under) = folder();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    fs::write(dir.path().join("b.txt"), b"a").unwrap();
    assert_ne!(under.object(Path::new("a.txt")).unwrap().handle, under.object(Path::new("b.txt")).unwrap().handle);
}

/// The handle goes with the object: the same one under its new name, and not the one of a
/// file made where it stood.
#[test]
fn a_renamed_file_is_the_object_it_was() {
    let (dir, under) = folder();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    let was = under.object(Path::new("a.txt")).unwrap();
    fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    assert_eq!(under.object(Path::new("b.txt")).unwrap().handle, was.handle);
    assert_ne!(under.object(Path::new("a.txt")).unwrap().handle, was.handle);
}

/// A link is the object at its name, never what it points to.
#[test]
fn a_link_is_not_followed() {
    let (dir, under) = folder();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
    let link = under.object(Path::new("link")).unwrap();
    assert!(link.meta.is_symlink());
    assert_ne!(link.handle, under.object(Path::new("a.txt")).unwrap().handle);
}

#[test]
fn an_object_carries_the_item_id_it_was_given_until_it_is_taken_off() {
    let (dir, under) = folder();
    fs::create_dir(dir.path().join("docs")).unwrap();
    assert_eq!(under.carried(Path::new("docs")), None);
    under.carry(Path::new("docs"), "D").unwrap();
    assert_eq!(under.carried(Path::new("docs")).as_deref(), Some("D"));
    under.carry_nothing(Path::new("docs"));
    assert_eq!(under.carried(Path::new("docs")), None);
    assert_eq!(under.carried(Path::new("none")), None);
}

use std::fs;
use std::path::Path;

use konedrive_fs::placeholder::XATTR_ROOT;

use super::{Under, OLD_STAGING};

/// A folder on disk with nothing in it, opened as a mount's would be.
pub(super) fn folder() -> (tempfile::TempDir, Under) {
    let dir = tempfile::tempdir().unwrap();
    let under = Under::open(dir.path()).unwrap();
    (dir, under)
}

/// A place is reached through the descriptor: what is written by such a path is in the folder.
#[test]
fn a_path_of_the_folder_leads_into_the_folder_itself() {
    let (dir, under) = folder();
    fs::write(under.path(Path::new("a.txt")), b"a").unwrap();
    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"a");
    assert_eq!(fs::read_dir(under.path(Path::new(""))).unwrap().count(), 1);
}

#[test]
fn the_staging_directory_of_an_earlier_build_goes_when_the_folder_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(OLD_STAGING)).unwrap();
    fs::write(dir.path().join(OLD_STAGING).join("part"), b"x").unwrap();
    let under = Under::open(dir.path()).unwrap();
    assert!(under.stat(Path::new(OLD_STAGING)).is_none());
}

/// The empty path is the folder itself, which carries attributes of its own.
#[test]
fn an_attribute_is_read_of_the_folder_and_of_what_it_holds() {
    let (dir, under) = folder();
    xattr::set(dir.path(), XATTR_ROOT, b"R").unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    xattr::set(dir.path().join("a.txt"), "user.test", b"v").unwrap();
    assert_eq!(under.attribute(Path::new(""), XATTR_ROOT).as_deref(), Some("R"));
    assert_eq!(under.attribute(Path::new("a.txt"), "user.test").as_deref(), Some("v"));
    assert_eq!(under.attribute(Path::new("a.txt"), XATTR_ROOT), None);
}

#[test]
fn the_folders_filesystem_says_how_much_space_it_has() {
    let (_dir, under) = folder();
    assert!(under.space().unwrap().f_bsize > 0);
}

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use crate::mount::backing_directory::tests::empty_backing_directory;

#[test]
fn a_file_and_a_directory_are_made_and_removed_in_the_backing_directory() {
    let (dir, backing_directory) = empty_backing_directory();
    backing_directory.make_directory(Path::new("docs")).unwrap();
    backing_directory.make_file(Path::new("docs/a.txt")).unwrap().write_all(b"abc").unwrap();
    assert_eq!(fs::read(dir.path().join("docs/a.txt")).unwrap(), b"abc");
    backing_directory.remove_file(Path::new("docs/a.txt")).unwrap();
    assert!(backing_directory.read_metadata(Path::new("docs/a.txt")).is_none());
    backing_directory.remove_directory(Path::new("docs")).unwrap();
    assert_eq!(backing_directory.read_directory(Path::new("")).unwrap().count(), 0);
}

/// A download is built where nothing lists it.
#[test]
fn a_file_without_a_name_is_in_no_directory() {
    let (dir, backing_directory) = empty_backing_directory();
    backing_directory.make_unnamed_file().unwrap().write_all(b"downloaded").unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_file_is_not_made_where_something_is() {
    let (_dir, backing_directory) = empty_backing_directory();
    backing_directory.make_file(Path::new("a.txt")).unwrap();
    assert_eq!(backing_directory.make_file(Path::new("a.txt")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(backing_directory.make_directory(Path::new("a.txt")).unwrap_err().kind(), ErrorKind::AlreadyExists);
}

#[test]
fn a_directory_that_holds_something_is_not_removed() {
    let (_dir, backing_directory) = empty_backing_directory();
    backing_directory.make_directory(Path::new("docs")).unwrap();
    backing_directory.make_file(Path::new("docs/a.txt")).unwrap();
    assert_eq!(backing_directory.remove_directory(Path::new("docs")).unwrap_err().kind(), ErrorKind::DirectoryNotEmpty);
    assert!(backing_directory.read_metadata(Path::new("docs/a.txt")).is_some());
}

/// A name is looked at itself: a link that leads nowhere is still there.
#[test]
fn a_link_that_leads_nowhere_is_at_its_name() {
    let (dir, backing_directory) = empty_backing_directory();
    std::os::unix::fs::symlink("none", dir.path().join("link")).unwrap();
    assert!(backing_directory.read_metadata(Path::new("link")).is_some_and(|m| m.is_symlink()));
    assert!(backing_directory.read_metadata(Path::new("none")).is_none());
}

#[test]
fn a_file_opened_to_write_keeps_its_content_and_one_that_is_not_there_is_not_found() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("a.txt"), b"abc").unwrap();
    assert_eq!(backing_directory.open_to_write(Path::new("a.txt")).unwrap().metadata().unwrap().len(), 3);
    assert_eq!(backing_directory.open_to_read(Path::new("none.txt")).unwrap_err().kind(), ErrorKind::NotFound);
}

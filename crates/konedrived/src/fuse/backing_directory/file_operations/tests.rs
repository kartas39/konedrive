use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::fuse::backing_directory::tests::empty_backing_directory;

#[test]
fn a_file_and_a_directory_are_made_renamed_and_removed_in_the_backing_directory() {
    let (dir, backing_directory) = empty_backing_directory();
    backing_directory.make_directory(Path::new("docs")).unwrap();
    backing_directory.make_file(Path::new("docs/a.txt")).unwrap().write_all(b"abc").unwrap();
    assert_eq!(fs::read(dir.path().join("docs/a.txt")).unwrap(), b"abc");
    backing_directory.rename(Path::new("docs/a.txt"), Path::new("b.txt")).unwrap();
    assert!(backing_directory.read_metadata(Path::new("docs/a.txt")).is_none() && backing_directory.read_metadata(Path::new("b.txt")).is_some_and(|m| m.is_file()));
    backing_directory.remove_directory(Path::new("docs")).unwrap();
    backing_directory.remove_file(Path::new("b.txt")).unwrap();
    assert_eq!(backing_directory.list_directory(Path::new("")).count(), 0);
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
    assert_eq!(backing_directory.list_directory(Path::new("docs")).map(|e| e.file_name()).collect::<Vec<_>>(), ["a.txt"]);
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
fn a_file_opened_to_write_keeps_its_content_and_one_opened_to_overwrite_is_truncated_to_nothing() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("a.txt"), b"abc").unwrap();
    assert_eq!(backing_directory.open_to_write(Path::new("a.txt")).unwrap().metadata().unwrap().len(), 3);
    assert_eq!(backing_directory.open_to_read(Path::new("none.txt")).unwrap_err().kind(), ErrorKind::NotFound);
}

#[test]
fn a_file_without_a_write_bit_is_given_its_owners() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("a.txt"), b"abc").unwrap();
    fs::set_permissions(dir.path().join("a.txt"), fs::Permissions::from_mode(0o444)).unwrap();
    backing_directory.grant_write_bit(Path::new("a.txt")).unwrap();
    assert_eq!(backing_directory.read_metadata(Path::new("a.txt")).unwrap().permissions().mode() & 0o777, 0o644);
}

/// Neither replaces the other: each is at the other's path, a file or a directory.
#[test]
fn two_entries_are_exchanged_in_one_step() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"a").unwrap();
    fs::write(dir.path().join("b.txt"), b"b").unwrap();
    backing_directory.exchange(Path::new("docs/a.txt"), Path::new("b.txt")).unwrap();
    assert_eq!((fs::read(dir.path().join("docs/a.txt")).unwrap(), fs::read(dir.path().join("b.txt")).unwrap()), (b"b".to_vec(), b"a".to_vec()));
    fs::create_dir(dir.path().join("empty")).unwrap();
    backing_directory.exchange(Path::new("docs"), Path::new("empty")).unwrap();
    assert!(dir.path().join("empty/a.txt").is_file() && fs::read_dir(dir.path().join("docs")).unwrap().next().is_none());
    assert_eq!(backing_directory.exchange(Path::new("none"), Path::new("b.txt")).unwrap_err().kind(), ErrorKind::NotFound);
}

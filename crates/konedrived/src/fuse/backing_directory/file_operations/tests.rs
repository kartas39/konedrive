use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::fuse::backing_directory::tests::folder;

#[test]
fn a_file_and_a_directory_are_made_renamed_and_removed_in_the_folder() {
    let (dir, under) = folder();
    under.make_dir(Path::new("docs")).unwrap();
    under.make_file(Path::new("docs/a.txt")).unwrap().write_all(b"abc").unwrap();
    assert_eq!(fs::read(dir.path().join("docs/a.txt")).unwrap(), b"abc");
    under.rename(Path::new("docs/a.txt"), Path::new("b.txt")).unwrap();
    assert!(under.stat(Path::new("docs/a.txt")).is_none() && under.stat(Path::new("b.txt")).is_some_and(|m| m.is_file()));
    under.remove_dir(Path::new("docs")).unwrap();
    under.remove_file(Path::new("b.txt")).unwrap();
    assert_eq!(under.list(Path::new("")).count(), 0);
}

#[test]
fn a_file_is_not_made_where_something_stands() {
    let (_dir, under) = folder();
    under.make_file(Path::new("a.txt")).unwrap();
    assert_eq!(under.make_file(Path::new("a.txt")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(under.make_dir(Path::new("a.txt")).unwrap_err().kind(), ErrorKind::AlreadyExists);
}

#[test]
fn a_directory_that_holds_something_is_not_removed() {
    let (_dir, under) = folder();
    under.make_dir(Path::new("docs")).unwrap();
    under.make_file(Path::new("docs/a.txt")).unwrap();
    assert!(under.remove_dir(Path::new("docs")).is_err());
    assert_eq!(under.list(Path::new("docs")).map(|e| e.file_name()).collect::<Vec<_>>(), ["a.txt"]);
}

/// A name is looked at itself: a link that leads nowhere still stands there.
#[test]
fn a_link_that_leads_nowhere_stands_at_its_name() {
    let (dir, under) = folder();
    std::os::unix::fs::symlink("none", dir.path().join("link")).unwrap();
    assert!(under.stat(Path::new("link")).is_some_and(|m| m.is_symlink()));
    assert!(under.stat(Path::new("none")).is_none());
}

#[test]
fn a_file_opened_to_write_is_cut_to_nothing_only_when_asked() {
    let (dir, under) = folder();
    fs::write(dir.path().join("a.txt"), b"abc").unwrap();
    assert_eq!(under.write(Path::new("a.txt"), false).unwrap().metadata().unwrap().len(), 3);
    assert_eq!(under.write(Path::new("a.txt"), true).unwrap().metadata().unwrap().len(), 0);
    assert!(under.read(Path::new("none.txt")).is_err());
}

#[test]
fn a_file_without_a_write_bit_is_given_its_owners() {
    let (dir, under) = folder();
    fs::write(dir.path().join("a.txt"), b"abc").unwrap();
    fs::set_permissions(dir.path().join("a.txt"), fs::Permissions::from_mode(0o444)).unwrap();
    under.writable(Path::new("a.txt")).unwrap();
    assert_eq!(under.stat(Path::new("a.txt")).unwrap().permissions().mode() & 0o777, 0o644);
}

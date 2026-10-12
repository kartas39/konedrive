//! A part of the backing directory that cannot be read is left as it is: no row in it or
//! below it is changed, they are held and said, and nothing else is held for them.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::{assert_no_item_of_onedrive_is_removed_or_moved, folder_of_each, found, left, DiskFinding, DiskFindings, Folder, UnresolvedReason};

/// Whether the tests run as root, whom no mode keeps from reading a directory.
pub(super) fn is_root() -> bool {
    // SAFETY: no arguments, no failure.
    unsafe { libc::geteuid() == 0 }
}

/// The check, run while `locked` cannot be read; it is readable again afterwards.
pub(super) fn check_with_unreadable(f: &mut Folder, locked: &str) -> DiskFindings {
    let mode = fs::metadata(f.disk(locked)).unwrap().permissions().mode();
    fs::set_permissions(f.disk(locked), fs::Permissions::from_mode(0o000)).unwrap();
    let findings = f.check();
    fs::set_permissions(f.disk(locked), fs::Permissions::from_mode(mode)).unwrap();
    findings
}

#[test]
fn the_rows_in_and_below_a_directory_that_cannot_be_read_are_held_by_its_row_and_not_changed() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.own_directory("docs/sub");
    f.own_file("docs/sub/deep.txt", b"deep");
    // Everything in it is gone or changed, for all the check can know.
    f.write("docs/a.txt", b"changed while nobody could look");
    fs::remove_file(f.disk("docs/sub/deep.txt")).unwrap();
    f.write("top.txt", b"changed and readable");
    f.write("new.txt", b"new and readable");
    let before = f.rows();
    let docs = f.id("docs");

    let findings = check_with_unreadable(&mut f, "docs");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2), (DiskFinding::Made, 1), (DiskFinding::ContentChanged, 1)], "what can be read is decided");
    assert_eq!(findings.list_held_items(), [docs], "the top row of what is held: what the rows say is below it is held with it");
    assert!(findings.has_unread_part());
    assert_eq!(left(&findings), [PathBuf::from("docs")]);
    assert!(matches!(findings.unresolved()[0].reason, UnresolvedReason::Unreadable(_)));
    for held in ["docs", "docs/a.txt", "docs/remote.bin", "docs/sub", "docs/sub/deep.txt"] {
        let id = f.id(held);
        assert_eq!(Some(&f.item(id)), before.iter().find(|row| row.local_item_id == id), "{held}");
    }

    // Readable: decided like any other.
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 7), (DiskFinding::ContentChanged, 1), (DiskFinding::Removed, 1)]);
    assert_eq!(findings.list_held_items(), []);
    f.assert_settled();
}

#[test]
fn a_new_directory_that_cannot_be_read_gets_no_row_until_it_can() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.make_directory("secret");
    f.write("secret/in.txt", b"x");
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "secret");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((left(&findings), findings.list_held_items()), (vec![PathBuf::from("secret")], &[][..]), "no row is in it to hold");
    assert_eq!(f.rows(), before);
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 2)]);
    f.assert_settled();
}

#[test]
fn the_row_at_a_name_that_is_no_file_and_no_directory_is_held_and_nothing_else_is() {
    let mut f = folder_of_each();
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    std::os::unix::fs::symlink("elsewhere", f.disk("docs/a.txt")).unwrap();
    std::os::unix::fs::symlink("elsewhere", f.disk("stray-link")).unwrap();
    fs::remove_file(f.disk("top.txt")).unwrap();
    let before = f.rows();
    let a = f.id("docs/a.txt");

    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 4), (DiskFinding::Removed, 1)], "top.txt is decided though a name was not looked at");
    assert_eq!(findings.list_held_items(), [a]);
    assert!(findings.has_unread_part());
    assert_eq!(left(&findings), [PathBuf::from("docs/a.txt"), PathBuf::from("stray-link")]);
    assert!(findings.unresolved().iter().all(|left| left.reason == UnresolvedReason::NotFileOrDirectory));
    assert_eq!(Some(&f.item(a)), before.iter().find(|row| row.local_item_id == a));
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
}

#[test]
fn a_root_that_cannot_be_read_holds_everything_and_changes_nothing() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    fs::remove_file(f.disk("top.txt")).unwrap();
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "");
    assert_eq!(found(&findings), []);
    assert_eq!(findings.list_held_items(), [super::ROOT]);
    assert_eq!(f.rows(), before);
}

/// A directory whose own name and marks are read and whose entries are not: here one deeper
/// than the rows go.
#[test]
fn a_directory_that_is_too_deep_gets_no_row_and_what_is_above_it_is_decided() {
    let mut f = super::folder();
    let mut path = PathBuf::new();
    for _ in 0..konedrive_fs::MAX_DEPTH {
        path.push("d");
        f.make_directory(&path);
    }
    f.write(path.join("in.txt"), b"too deep");
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::Made, konedrive_fs::MAX_DEPTH - 1)]);
    assert_eq!((left(&findings), findings.unresolved()[0].reason.clone()), (vec![path.clone()], UnresolvedReason::TooDeep));
    assert!(findings.has_unread_part() && findings.list_held_items().is_empty());
    assert!(f.row(path.parent().unwrap()).is_some() && f.row(&path).is_none());
    f.assert_settled_but(&[path]);
}

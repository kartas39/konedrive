//! While a part of the backing directory was not read, what is gone from where its row says
//! may be in that part: nothing is found removed, and no row loses its local version. The
//! sequences are the review's M1 a), b) and c).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use konedrive_tree::{ContentPresence, LocalVersionChange};

use super::{folder_of_each, found, DiskFinding, DiskFindings, Folder, UnresolvedReason, MTIME};

/// Whether the tests run as root, whom no mode keeps from reading a directory.
fn is_root() -> bool {
    // SAFETY: no arguments, no failure.
    unsafe { libc::geteuid() == 0 }
}

/// The check, run while `locked` cannot be read; the directory is readable again afterwards.
fn check_with_unreadable(f: &mut Folder, locked: &str) -> DiskFindings {
    fs::set_permissions(f.disk(locked), fs::Permissions::from_mode(0o000)).unwrap();
    let findings = f.check();
    fs::set_permissions(f.disk(locked), fs::Permissions::from_mode(0o755)).unwrap();
    findings
}

fn left(findings: &DiskFindings) -> Vec<(PathBuf, bool)> {
    findings.unresolved().iter().map(|left| (left.path.clone(), left.reason == UnresolvedReason::MayBeUnread)).collect()
}

fn path(of: &str) -> PathBuf {
    PathBuf::from(of)
}

/// M1 a): `mv top.txt secret/ ; chmod 000 secret`.
#[test]
fn an_item_moved_into_a_directory_that_cannot_be_read_is_not_removed() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.own_directory("secret");
    let top = f.row("top.txt").unwrap();
    f.rename("top.txt", "secret/top.txt");
    let findings = check_with_unreadable(&mut f, "secret");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5)]);
    assert!(!findings.is_read_whole());
    assert_eq!(left(&findings), [(path("secret"), false), (path("top.txt"), true)]);
    assert_eq!(f.item(top.local_item_id), top, "its row is as it was");

    // Readable again: an item OneDrive never had carries no mark, and is a new item there.
    let findings = f.check();
    assert!(findings.is_read_whole());
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 1), (DiskFinding::Removed, 1)]);
    f.assert_settled();
}

/// M1 b): docs/b.txt of OneDrive, edited through the mount and unsent; box/, the user's own,
/// with the clean copy c.txt of OneDrive in it. `mv docs/b.txt secret/ ; mv box secret/ ;
/// chmod 000 secret`.
#[test]
fn an_unsent_edit_and_a_directory_moved_into_a_directory_that_cannot_be_read_are_left_as_they_are() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.own_directory("secret");
    let b = f.copy("docs/b.txt", b"bbb", "B");
    f.store.item_store().apply(&LocalVersionChange::Truncated { item: b, size: 3, mtime: MTIME, mtime_nsec: 0 }).unwrap();
    let box_directory = f.own_directory("box");
    let c = f.copy("c.txt", b"ccc", "C");
    f.move_through_the_mount("c.txt", "box/c.txt");
    let before = f.rows();
    f.rename("docs/b.txt", "secret/b.txt");
    f.rename("box", "secret/box");
    let findings = check_with_unreadable(&mut f, "secret");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 6)]);
    assert!(!findings.is_read_whole());
    assert_eq!(left(&findings), [(path("box"), true), (path("docs/b.txt"), true), (path("secret"), false)]);
    assert_eq!(f.rows(), before, "no row lost its local version, and none went");

    // Readable again: the two items of OneDrive are found by their marks, where they are.
    let findings = f.check();
    assert!(findings.is_read_whole());
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 7), (DiskFinding::Made, 1), (DiskFinding::Moved, 2), (DiskFinding::Removed, 1)]);
    assert_eq!((f.id("secret/b.txt"), f.id("secret/box/c.txt")), (b, c));
    assert_eq!(f.store.item_store().find_item(box_directory).unwrap(), None, "the directory carried no mark: it is a new one there");
    f.assert_settled();
}

/// M1 c): `mv docs docs2 ; chmod 000 docs2`. The marks of docs2 cannot be read either.
#[test]
fn a_directory_of_onedrive_renamed_and_made_unreadable_is_not_made_anew_and_what_was_in_it_is_not_removed() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    let b = f.copy("docs/b.txt", b"bbb", "B");
    f.store.item_store().apply(&LocalVersionChange::Truncated { item: b, size: 3, mtime: MTIME, mtime_nsec: 0 }).unwrap();
    f.rename("docs", "docs2");
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "docs2");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentNoLongerOnDisk, 1)]);
    assert_eq!(left(&findings), [(path("docs/b.txt"), true), (path("docs2"), false)]);
    let now = f.rows();
    assert_eq!(now.len(), before.len(), "docs2 is not made as a new item");
    let a = f.id("docs/a.txt");
    for (was, is) in before.iter().zip(&now) {
        // The clean copy is not where its row says: its content is not on disk, and nothing
        // else of any row changed.
        let expected = if was.local_item_id == a { konedrive_tree::Item { content_presence: ContentPresence::NotOnDisk, ..was.clone() } } else { was.clone() };
        assert_eq!(*is, expected);
    }

    // Readable again: the directory is found by its mark, and what is in it where it is.
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 1), (DiskFinding::CopyLandedBeforeRow, 1)]);
    assert_eq!((f.id("docs2/b.txt"), f.row("docs2/a.txt").unwrap().content_presence), (b, ContentPresence::OnDisk));
    f.assert_settled();
}

/// What a write waits for is held back with the removal it depends on, and said: a file
/// made at the name of a file that may be in the part that was not read. What does not
/// depend on a removal is written.
#[test]
fn a_new_item_at_the_name_of_a_row_that_may_be_unread_waits_and_what_depends_on_no_removal_is_written() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.own_directory("secret");
    f.rename("docs/a.txt", "a-moved.txt");
    f.write("new.txt", b"new");
    f.make_directory("docs/remote.bin");
    f.rename("mine", "secret/mine");
    f.write("mine", b"a file now");
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "secret");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2), (DiskFinding::Made, 1), (DiskFinding::Moved, 1)]);
    assert_eq!(left(&findings), [(path("docs/remote.bin"), false), (path("docs/remote.bin"), true), (path("mine"), false), (path("mine"), true), (path("secret"), false)]);
    let blocked: Vec<&Path> = findings.unresolved().iter().filter(|left| left.reason == UnresolvedReason::Blocked).map(|left| left.path.as_path()).collect();
    assert_eq!(blocked, [Path::new("docs/remote.bin"), Path::new("mine")]);
    assert_eq!(f.rows().len(), before.len() + 1);
    assert!(f.row("a-moved.txt").is_some() && f.row("new.txt").is_some());
}

/// Found by the test of convergence. `cp -a docs docs-copy ; chmod 000 docs`: the copy is
/// the one entry that is seen with the mark of `docs`. It is not taken as `docs` moved.
#[test]
fn a_copy_of_a_directory_that_cannot_be_read_is_not_taken_as_that_directory_moved() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.make_directory("docs-copy");
    f.mark_directory("docs-copy", "D");
    f.write("docs-copy/a.txt", b"aaa");
    f.mark_copy("docs-copy/a.txt", "A");
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "docs");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 3)]);
    assert_eq!(left(&findings), [(path("docs"), false), (path("docs-copy"), true)]);
    assert_eq!(f.rows(), before);

    // Readable again: the original is where its row says, and the copy is a new directory.
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::MadeWithStaleMark, 2)]);
    assert_eq!(f.rows().len(), before.len() + 2);
    f.assert_settled();
}

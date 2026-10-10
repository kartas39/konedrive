//! An entry the check takes as the own entry of an item OneDrive has gets that item's id as
//! its mark when it carries none, so that a later move by hand is followed: a file saved by
//! rename, a file put at the name of a file that is not on disk, a directory made at the
//! name of a directory of OneDrive. The row is written first, the mark second; the mark is
//! the id alone, never the state of a whole copy and never a cTag. The sequence is the
//! second review's S1.

use konedrive_fs::placeholder::{XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_tree::ContentPresence;

use super::{changed_here, content, folder_of_each, found, reference, DiskFinding};

/// S1: docs/a.txt is saved by rename (no marks); start; `mv docs/a.txt docs/b.txt`; start.
#[test]
fn a_file_saved_by_rename_gets_its_items_id_as_its_mark_and_is_followed_when_it_is_moved() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.write("docs/a.txt", b"saved by rename");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    assert_eq!(f.mark_of("docs/a.txt"), Some("A".to_owned()));
    let marks = |name: &str| xattr::get(f.disk("docs/a.txt"), name).unwrap();
    assert_eq!((marks(XATTR_STATE), marks(XATTR_CTAG)), (None, None), "the id alone: the file is no whole copy of what OneDrive has");
    f.assert_settled();

    f.rename("docs/a.txt", "docs/b.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 1)]);
    let row = f.row("docs/b.txt").unwrap();
    assert_eq!((row.local_item_id, row.onedrive_item.clone(), content(&row)), (a, Some(reference("d1", "A")), changed_here(15, 1)));
    f.assert_settled();
}

/// A file put by hand at the name of a file that is not downloaded is that item's content.
#[test]
fn a_file_put_at_the_name_of_a_file_that_is_not_on_disk_gets_its_items_id_as_its_mark() {
    let mut f = folder_of_each();
    let remote = f.id("docs/remote.bin");
    f.write("docs/remote.bin", b"my own bytes");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    assert_eq!(f.mark_of("docs/remote.bin"), Some("R".to_owned()));
    f.assert_settled();

    f.rename("docs/remote.bin", "mine/moved.bin");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 1)]);
    assert_eq!((f.id("mine/moved.bin"), f.item(remote).content_presence), (remote, ContentPresence::OnDisk));
    f.assert_settled();
}

/// A daemon that died between the row and the mark: the row says the content changed here,
/// the file carries no mark. The next check finds nothing changed and writes the mark.
#[test]
fn a_death_between_the_row_and_the_mark_is_finished_by_the_next_check() {
    let mut f = folder_of_each();
    f.write("docs/a.txt", b"saved by rename");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    xattr::remove(f.disk("docs/a.txt"), XATTR_ITEM_ID).unwrap();
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.mark_of("docs/a.txt")), (before, Some("A".to_owned())), "no row is written, and the mark is");
    f.assert_settled();
}

/// A directory made by hand where a directory of OneDrive had none on disk is that
/// directory's own.
#[test]
fn a_directory_made_at_the_name_of_a_directory_of_onedrive_gets_its_id_as_its_mark() {
    let mut f = folder_of_each();
    let docs = f.id("docs");
    std::fs::remove_dir_all(f.disk("docs")).unwrap();
    f.check();
    f.make_directory("docs");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.id("docs"), f.mark_of("docs")), (docs, Some("D".to_owned())));
    f.rename("docs", "papers");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 1)]);
    assert_eq!(f.id("papers"), docs);
    f.assert_settled();
}

/// An unmarked file is never the copy a download landed: one that stands where a clean copy
/// was, of its size and time, is content the user put there.
#[test]
fn an_unmarked_file_where_a_clean_copy_was_is_content_the_user_put_there() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.write("docs/a.txt", b"aaa");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    assert_eq!((content(&f.item(a)), f.mark_of("docs/a.txt")), (changed_here(3, 1), Some("A".to_owned())));
    f.assert_settled();
}

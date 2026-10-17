//! What the check writes of marks: the id of its item on the own entry of an item OneDrive
//! has, and nothing through one name of a file that has several.

use std::fs;

use super::{assert_no_item_of_onedrive_is_removed_or_moved, folder_of_each, found, DiskFinding};

#[test]
fn the_own_entry_of_an_item_of_onedrive_gets_the_id_it_lacks() {
    let mut f = folder_of_each();
    // Saved by rename: the file at the name is a new one, with no marks. And a directory
    // made again by hand.
    f.write("docs/a.txt", b"saved by rename");
    xattr::remove(f.disk("docs"), konedrive_fs::placeholder::XATTR_ITEM_ID).unwrap();
    assert_eq!((f.mark_of("docs/a.txt"), f.mark_of("docs")), (None, None));
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    assert_eq!((f.mark_of("docs/a.txt").as_deref(), f.mark_of("docs").as_deref()), (Some("A"), Some("D")));
    assert_eq!(f.copy_marks_of("docs/a.txt"), (false, None), "the id alone: it is no whole copy of what OneDrive has");
    f.assert_settled();
}

/// A death between the row and the mark: the next check writes the mark and no row.
#[test]
fn a_mark_that_was_not_written_is_written_by_the_next_check_which_writes_no_row() {
    let mut f = folder_of_each();
    f.write("docs/a.txt", b"saved by rename");
    f.check();
    xattr::remove(f.disk("docs/a.txt"), konedrive_fs::placeholder::XATTR_ITEM_ID).unwrap();
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!(f.rows(), before);
    assert_eq!(f.mark_of("docs/a.txt").as_deref(), Some("A"));
}

#[test]
fn another_items_copy_laid_over_a_file_is_that_files_content_changed_and_carries_its_id_alone() {
    let mut f = folder_of_each();
    f.copy("docs/b.txt", b"bbbbb", "B");
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    f.copy_with_marks("docs/b.txt", "docs/a.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::ContentChanged, 1)]);
    assert_eq!((f.mark_of("docs/a.txt").as_deref(), f.copy_marks_of("docs/a.txt")), (Some("A"), (false, None)));
    assert_eq!((f.mark_of("docs/b.txt").as_deref(), f.copy_marks_of("docs/b.txt")), (Some("B"), (true, Some("c-B".to_owned()))));
    f.assert_settled();
}

#[test]
fn the_entry_of_an_item_onedrive_never_had_keeps_whatever_marks_it_has() {
    let mut f = folder_of_each();
    fs::remove_file(f.disk("top.txt")).unwrap();
    f.copy_with_marks("docs/a.txt", "top.txt");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)], "the same size and time");
    assert_eq!(f.mark_of("top.txt").as_deref(), Some("A"));
    assert_eq!(f.rows(), before);
}

/// The marks are the file's, not the name's.
#[test]
fn the_marks_of_a_file_with_a_second_name_are_left_and_a_second_check_writes_nothing() {
    let mut f = folder_of_each();
    let before = f.rows();
    f.link("docs/a.txt", "a-link.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 1)]);
    for name in ["docs/a.txt", "a-link.txt"] {
        assert_eq!((f.mark_of(name).as_deref(), f.copy_marks_of(name)), (Some("A"), (true, Some("c-A".to_owned()))), "{name}");
    }
    assert_eq!(f.row("a-link.txt").unwrap().onedrive_item, None);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    // Start after start: nothing is written, on disk or in the rows.
    for _ in 0..3 {
        f.assert_settled();
        assert_eq!(f.mark_of("a-link.txt").as_deref(), Some("A"));
    }
}

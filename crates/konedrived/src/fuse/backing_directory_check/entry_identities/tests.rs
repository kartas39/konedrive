use konedrive_tree::{Item, ItemKind};

use super::{EntryIdentities, EntryIdentity};
use crate::fuse::backing_directory_check::backing_directory_contents::tests::{contents_of, index_of, Listed};
use crate::fuse::backing_directory_check::disk_finding::MarkOwner;
use crate::fuse::backing_directory_check::item_rows::tests::{bound, bound_in, drive, id, not_downloaded, own, removed_here};
use crate::fuse::backing_directory_check::item_rows::ItemRows;

use ItemKind::{Directory, File};

/// Which item each of `paths` is, and whose its mark is, in a backing directory that has
/// `listed` and with these rows; the folder's own drive is `d1`.
fn settle(rows: Vec<Item>, listed: &[(&str, Listed)], paths: &[&str]) -> Vec<(EntryIdentity, MarkOwner)> {
    let (rows, contents) = (ItemRows::new(rows), contents_of(listed));
    let identities = EntryIdentities::settle(&rows, &contents, &drive("d1"));
    paths.iter().map(|path| index_of(&contents, path)).map(|index| (identities.identity(index), identities.mark_owner(index))).collect()
}

fn item(number: i64) -> EntryIdentity {
    EntryIdentity::Item(id(number))
}

#[test]
fn an_entry_where_the_row_of_its_mark_says_is_that_item() {
    let rows = vec![bound("D", 2, 1, "docs", Directory), bound("A", 3, 2, "a.txt", File)];
    let listed = [("docs", Listed::Directory(Some("D"))), ("docs/a.txt", Listed::File(Some("A")))];
    assert_eq!(settle(rows, &listed, &["docs", "docs/a.txt"]), [(item(2), MarkOwner::RowAtPath), (item(3), MarkOwner::RowAtPath)]);
}

#[test]
fn an_unmarked_entry_is_the_item_of_the_row_its_path_has_when_that_row_is_of_its_kind() {
    let rows = vec![own(2, 1, "mine", Directory), own(3, 2, "own.txt", File), own(4, 1, "file", File), own(5, 1, "dir", Directory)];
    let listed = [("mine", Listed::Directory(None)), ("mine/own.txt", Listed::File(None)), ("mine/new.txt", Listed::File(None)), ("file", Listed::Directory(None)), ("dir", Listed::File(None))];
    let settled = settle(rows, &listed, &["mine", "mine/own.txt", "mine/new.txt", "file", "dir"]);
    assert_eq!(settled[..2], [(item(2), MarkOwner::Unmarked), (item(3), MarkOwner::Unmarked)]);
    assert_eq!(settled[2..], [(EntryIdentity::NewItem, MarkOwner::Unmarked); 3], "no row, and a row of the other kind");
}

#[test]
fn the_only_entry_with_the_mark_of_a_row_at_another_path_is_that_item_moved() {
    let rows = vec![bound("A", 2, 1, "a.txt", File), own(3, 1, "mine", Directory)];
    let listed = [("mine", Listed::Directory(None)), ("mine/renamed.txt", Listed::File(Some("A")))];
    assert_eq!(settle(rows, &listed, &["mine/renamed.txt"]), [(item(2), MarkOwner::RowElsewhere)]);
}

/// The rows below a directory name it by its id: they are found where they are, whatever
/// path the directory has now.
#[test]
fn what_is_in_a_directory_moved_by_hand_is_where_its_row_says() {
    let rows = vec![bound("D", 2, 1, "docs", Directory), bound("A", 3, 2, "a.txt", File), own(4, 2, "own.txt", File)];
    let listed = [("papers", Listed::Directory(Some("D"))), ("papers/a.txt", Listed::File(Some("A"))), ("papers/own.txt", Listed::File(None))];
    assert_eq!(
        settle(rows, &listed, &["papers", "papers/a.txt", "papers/own.txt"]),
        [(item(2), MarkOwner::RowElsewhere), (item(3), MarkOwner::RowAtPath), (item(4), MarkOwner::Unmarked)]
    );
}

#[test]
fn of_two_entries_with_one_mark_the_one_where_the_row_says_is_the_item_and_the_other_a_copy() {
    let rows = vec![bound("A", 2, 1, "m.txt", File)];
    let listed = [("a-copy.txt", Listed::File(Some("A"))), ("m.txt", Listed::File(Some("A"))), ("z-copy.txt", Listed::File(Some("A")))];
    assert_eq!(
        settle(rows, &listed, &["a-copy.txt", "m.txt", "z-copy.txt"]),
        [(EntryIdentity::NewItem, MarkOwner::RowOfAnotherEntry), (item(2), MarkOwner::RowAtPath), (EntryIdentity::NewItem, MarkOwner::RowOfAnotherEntry)]
    );
}

#[test]
fn of_two_entries_with_one_mark_and_none_where_the_row_says_the_first_is_the_item_moved() {
    let rows = vec![bound("A", 2, 1, "a.txt", File)];
    let listed = [("first.txt", Listed::File(Some("A"))), ("second.txt", Listed::File(Some("A")))];
    assert_eq!(settle(rows, &listed, &["first.txt", "second.txt"]), [(item(2), MarkOwner::RowElsewhere), (EntryIdentity::NewItem, MarkOwner::RowOfAnotherEntry)]);
}

/// An item never changes its kind: the mark of a file on a directory says nothing of the
/// directory but that it was copied from somewhere.
#[test]
fn an_entry_marked_with_the_id_of_a_row_of_the_other_kind_is_a_copy() {
    let rows = vec![bound("A", 2, 1, "a.txt", File), own(3, 1, "dir", Directory)];
    let listed = [("a.txt", Listed::File(Some("A"))), ("dir", Listed::Directory(Some("A"))), ("new", Listed::Directory(Some("A")))];
    assert_eq!(settle(rows, &listed, &["dir", "new"]), [(item(3), MarkOwner::RowOfAnotherEntry), (EntryIdentity::NewItem, MarkOwner::RowOfAnotherEntry)]);
}

#[test]
fn one_entry_is_the_item_of_a_mark_no_row_the_folder_shows_has_and_a_second_is_a_copy() {
    let rows = vec![own(2, 1, "own.txt", File), removed_here(bound("G", 3, 1, "gone.txt", File))];
    let listed = [("lost.txt", Listed::File(Some("Z"))), ("lost-again.txt", Listed::File(Some("Z"))), ("own.txt", Listed::File(Some("U"))), ("back.txt", Listed::File(Some("G")))];
    assert_eq!(
        settle(rows, &listed, &["lost-again.txt", "lost.txt", "own.txt", "back.txt"]),
        [(EntryIdentity::NewItem, MarkOwner::NoRow), (EntryIdentity::NewItem, MarkOwner::RowOfAnotherEntry), (item(2), MarkOwner::NoRow), (EntryIdentity::NewItem, MarkOwner::NoRow)]
    );
}

/// The row at the path is moved elsewhere, by its own mark: what stands at its path now is
/// not it.
#[test]
fn an_unmarked_entry_at_the_path_of_a_row_that_is_found_elsewhere_is_a_new_item() {
    let rows = vec![bound("A", 2, 1, "a.txt", File)];
    let listed = [("a.txt", Listed::File(None)), ("a.txt~", Listed::File(Some("A")))];
    assert_eq!(settle(rows, &listed, &["a.txt", "a.txt~"]), [(EntryIdentity::NewItem, MarkOwner::Unmarked), (item(2), MarkOwner::RowElsewhere)]);
}

/// A mark names an item id and no drive. An entry is first the row at its own place, when
/// that row has the mark's id: with one id in two drives and nothing touched by hand, each
/// entry is its own row — whichever row was made first.
#[test]
fn an_entry_is_the_row_at_its_own_place_that_has_its_marks_id_whichever_drive_that_row_is_of() {
    let rows = vec![not_downloaded(bound_in("d1", "X", 2, 1, "l.bin", File)), bound_in("d2", "X", 3, 1, "h.txt", File)];
    assert_eq!(settle(rows, &[("h.txt", Listed::File(Some("X")))], &["h.txt"]), [(item(3), MarkOwner::RowAtPath)]);

    let rows = vec![bound_in("d1", "X", 2, 1, "l.bin", File), bound_in("d2", "X", 3, 1, "h.txt", File)];
    let listed = [("h.txt", Listed::File(Some("X"))), ("l.bin", Listed::File(Some("X")))];
    assert_eq!(settle(rows, &listed, &["h.txt", "l.bin"]), [(item(3), MarkOwner::RowAtPath), (item(2), MarkOwner::RowAtPath)]);
}

/// Elsewhere a mark is looked for in the drive of the row of the directory the entry is in:
/// of the nearest directory above that has a binding, or the folder's own.
#[test]
fn an_entry_that_is_not_where_a_row_of_its_mark_says_is_looked_for_in_the_drive_of_its_directory() {
    let rows = || vec![bound_in("d2", "S", 2, 1, "shared", Directory), own(3, 2, "new", Directory), bound_in("d1", "X", 4, 1, "one.txt", File), bound_in("d2", "X", 5, 2, "two.txt", File)];
    // The file of the other drive was renamed where it is: the one of the folder's drive,
    // which the mark alone would find first, is not meant.
    let listed = [("shared", Listed::Directory(Some("S"))), ("shared/renamed.txt", Listed::File(Some("X"))), ("one.txt", Listed::File(Some("X")))];
    assert_eq!(settle(rows(), &listed, &["shared/renamed.txt", "one.txt"]), [(item(5), MarkOwner::RowElsewhere), (item(4), MarkOwner::RowAtPath)]);
    // In a directory with no binding the drive is that of the directory above it.
    let listed = [("shared", Listed::Directory(Some("S"))), ("shared/new", Listed::Directory(None)), ("shared/new/renamed.txt", Listed::File(Some("X"))), ("one.txt", Listed::File(Some("X")))];
    assert_eq!(settle(rows(), &listed, &["shared/new/renamed.txt"]), [(item(5), MarkOwner::RowElsewhere)]);
    // The drive only tells two rows of one id apart: a shared folder, which is of another
    // drive than the root it is in, is found by its mark when it was renamed by hand.
    let rows = vec![bound_in("d2", "S", 2, 1, "shared", Directory), bound_in("d2", "Y", 3, 2, "y.txt", File)];
    let listed = [("renamed", Listed::Directory(Some("S"))), ("renamed/y.txt", Listed::File(Some("Y"))), ("y-copy.txt", Listed::File(Some("Y")))];
    assert_eq!(
        settle(rows, &listed, &["renamed", "renamed/y.txt", "y-copy.txt"]),
        [(item(2), MarkOwner::RowElsewhere), (item(3), MarkOwner::RowAtPath), (EntryIdentity::NewItem, MarkOwner::RowOfAnotherEntry)]
    );
}

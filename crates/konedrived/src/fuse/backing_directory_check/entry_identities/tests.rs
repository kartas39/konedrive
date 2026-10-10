use konedrive_tree::{Item, ItemKind};

use super::{EntryIdentities, EntryIdentity, IdentityDoubt};
use crate::fuse::backing_directory_check::backing_directory_contents::tests::{contents_of, index_of, Listed};
use crate::fuse::backing_directory_check::disk_finding::MarkOwner;
use crate::fuse::backing_directory_check::item_rows::tests::{bound, bound_in, id, not_downloaded, own, removed_here};
use crate::fuse::backing_directory_check::item_rows::ItemRows;

use ItemKind::{Directory, File};

/// Which item each of `paths` is, and whose its mark is, in a backing directory that has
/// `listed` and with these rows.
fn settle(rows: Vec<Item>, listed: &[(&str, Listed)], paths: &[&str]) -> Vec<(EntryIdentity, MarkOwner)> {
    let (rows, contents) = (ItemRows::new(rows), contents_of(listed));
    let identities = EntryIdentities::settle(&rows, &contents);
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
        [(EntryIdentity::NewItem, MarkOwner::AnotherItem), (item(2), MarkOwner::RowAtPath), (EntryIdentity::NewItem, MarkOwner::AnotherItem)]
    );
}

#[test]
fn of_two_entries_with_one_mark_and_none_where_the_row_says_the_first_is_the_item_moved() {
    let rows = vec![bound("A", 2, 1, "a.txt", File)];
    let listed = [("first.txt", Listed::File(Some("A"))), ("second.txt", Listed::File(Some("A")))];
    assert_eq!(settle(rows, &listed, &["first.txt", "second.txt"]), [(item(2), MarkOwner::RowElsewhere), (EntryIdentity::NewItem, MarkOwner::AnotherItem)]);
}

/// An item never changes its kind: the mark of a file on a directory says nothing of the
/// directory but that it was copied from somewhere.
#[test]
fn an_entry_marked_with_the_id_of_a_row_of_the_other_kind_is_a_copy() {
    let rows = vec![bound("A", 2, 1, "a.txt", File), own(3, 1, "dir", Directory)];
    let listed = [("a.txt", Listed::File(Some("A"))), ("dir", Listed::Directory(Some("A"))), ("new", Listed::Directory(Some("A")))];
    assert_eq!(settle(rows, &listed, &["dir", "new"]), [(item(3), MarkOwner::AnotherItem), (EntryIdentity::NewItem, MarkOwner::AnotherItem)]);
}

#[test]
fn one_entry_is_the_item_of_a_mark_no_row_the_folder_shows_has_and_a_second_is_a_copy() {
    let rows = vec![own(2, 1, "own.txt", File), removed_here(bound("G", 3, 1, "gone.txt", File))];
    let listed = [("lost.txt", Listed::File(Some("Z"))), ("lost-again.txt", Listed::File(Some("Z"))), ("own.txt", Listed::File(Some("U"))), ("back.txt", Listed::File(Some("G")))];
    assert_eq!(
        settle(rows, &listed, &["lost-again.txt", "lost.txt", "own.txt", "back.txt"]),
        [(EntryIdentity::NewItem, MarkOwner::NoRow), (EntryIdentity::NewItem, MarkOwner::AnotherItem), (item(2), MarkOwner::NoRow), (EntryIdentity::NewItem, MarkOwner::NoRow)]
    );
    // The id is one whichever directory the entries are in: the check makes no second row of it.
    let rows = vec![bound_in("d2", "S", 2, 1, "shared", Directory)];
    let listed = [("lost.txt", Listed::File(Some("Z"))), ("shared", Listed::Directory(Some("S"))), ("shared/lost.txt", Listed::File(Some("Z")))];
    assert_eq!(settle(rows, &listed, &["lost.txt", "shared/lost.txt"]), [(EntryIdentity::NewItem, MarkOwner::NoRow), (EntryIdentity::NewItem, MarkOwner::AnotherItem)]);
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

/// A mark names no drive, and none is guessed: where rows of two drives have its id, an
/// entry that is not where one of them says is left, whatever directory it is in, and so is
/// an entry at the place of one of them, which may be that row's or new.
#[test]
fn an_entry_whose_marks_id_is_in_rows_of_two_drives_and_that_is_not_where_one_says_is_left() {
    let rows = || vec![bound_in("d2", "S", 2, 1, "shared", Directory), bound_in("d1", "X", 4, 1, "one.txt", File), bound_in("d2", "X", 5, 2, "two.txt", File)];
    let left = EntryIdentity::Left(IdentityDoubt::ItemIdInTwoDrives);
    let listed = [("shared", Listed::Directory(Some("S"))), ("shared/renamed.txt", Listed::File(Some("X"))), ("one.txt", Listed::File(Some("X")))];
    let settled = settle(rows(), &listed, &["shared/renamed.txt", "one.txt"]);
    assert_eq!((settled[0].0, settled[1]), (left, (item(4), MarkOwner::RowAtPath)), "the one in place is its row, the other is not the other row moved");
    // An unmarked file at the place of one of the rows: the entry that was left may be it.
    let listed = [("shared", Listed::Directory(Some("S"))), ("shared/renamed.txt", Listed::File(Some("X"))), ("one.txt", Listed::File(None))];
    assert_eq!(settle(rows(), &listed, &["shared/renamed.txt", "one.txt"]).iter().map(|(identity, _)| *identity).collect::<Vec<_>>(), [left, left]);
    // With each marked entry where a row says, nothing depends on which is which.
    let listed = [("shared", Listed::Directory(Some("S"))), ("shared/two.txt", Listed::File(Some("X"))), ("one.txt", Listed::File(None))];
    assert_eq!(settle(rows(), &listed, &["shared/two.txt", "one.txt"]), [(item(5), MarkOwner::RowAtPath), (item(4), MarkOwner::Unmarked)]);
    // Also when the folder shows one of the two rows only.
    let rows = vec![removed_here(bound_in("d1", "X", 4, 1, "one.txt", File)), not_downloaded(bound_in("d2", "X", 5, 1, "two.txt", File))];
    assert_eq!(settle(rows, &[("a.txt", Listed::File(Some("X")))], &["a.txt"])[0].0, left);
}

/// A shared folder is of another drive than the directory it is in: the one row that has
/// its id is the row its mark names, whichever drive that is.
#[test]
fn an_entry_is_the_one_row_that_has_its_marks_id_whichever_drive_that_row_is_of() {
    let rows = vec![bound_in("d2", "S", 2, 1, "shared", Directory), bound_in("d2", "Y", 3, 2, "y.txt", File)];
    let listed = [("renamed", Listed::Directory(Some("S"))), ("renamed/y.txt", Listed::File(Some("Y"))), ("y-copy.txt", Listed::File(Some("Y")))];
    assert_eq!(
        settle(rows, &listed, &["renamed", "renamed/y.txt", "y-copy.txt"]),
        [(item(2), MarkOwner::RowElsewhere), (item(3), MarkOwner::RowAtPath), (EntryIdentity::NewItem, MarkOwner::AnotherItem)]
    );
}

/// What was not on disk was not moved by hand: a file marked with the id of a file whose
/// row says its content is not on disk is a copy, wherever it is. An unmarked file at the
/// place of that row is the row's, whatever else carries the id.
#[test]
fn an_entry_marked_with_the_id_of_a_file_that_was_not_on_disk_is_not_that_file_moved() {
    let rows = || vec![not_downloaded(bound("R", 2, 1, "r.bin", File)), own(3, 1, "mine", Directory)];
    assert_eq!(settle(rows(), &[("mine", Listed::Directory(None)), ("mine/r.bin", Listed::File(Some("R")))], &["mine/r.bin"]), [(EntryIdentity::NewItem, MarkOwner::AnotherItem)]);
    let listed = [("mine", Listed::Directory(None)), ("mine/r.bin", Listed::File(Some("R"))), ("r.bin", Listed::File(None))];
    assert_eq!(settle(rows(), &listed, &["mine/r.bin", "r.bin"]), [(EntryIdentity::NewItem, MarkOwner::AnotherItem), (item(2), MarkOwner::Unmarked)]);
}

/// The place a row says counts as looked at only through the entries taken as the rows of
/// the directories above it. `docs` could not be looked at; `docs-copy` carries its mark
/// and is left, so it stands in for nothing: an entry with the mark of what the rows say is
/// in `docs` is left too, and so is all that is in an entry that is left.
#[test]
fn an_entry_with_the_mark_of_a_row_whose_place_was_not_looked_at_is_left_and_stands_in_for_no_directory() {
    let rows = || vec![bound("D", 2, 1, "docs", Directory), bound("A", 3, 2, "a.txt", File), bound("B", 4, 1, "b.txt", File)];
    let left = EntryIdentity::Left(IdentityDoubt::PlaceNotLookedAt);
    let listed = [("docs", Listed::Special), ("docs-copy", Listed::Directory(Some("D"))), ("docs-copy/in.txt", Listed::File(None)), ("a-copy.txt", Listed::File(Some("A"))), ("b-moved.txt", Listed::File(Some("B")))];
    let settled = settle(rows(), &listed, &["docs-copy", "docs-copy/in.txt", "a-copy.txt", "b-moved.txt"]);
    assert_eq!(settled.iter().map(|(identity, _)| *identity).collect::<Vec<_>>(), [left, left, left, item(4)]);
    // A directory that was read to no end, likewise.
    let listed = [("docs", Listed::UnreadableDirectory(Some("D"))), ("a-copy.txt", Listed::File(Some("A")))];
    assert_eq!(settle(rows(), &listed, &["docs", "a-copy.txt"]).iter().map(|(identity, _)| *identity).collect::<Vec<_>>(), [item(2), left]);
}

/// An unmarked entry at the place of a row whose id an entry that is left carries may be
/// the item, or a new file at the name of the item that is elsewhere: it is left.
#[test]
fn an_unmarked_entry_at_the_place_of_a_row_whose_mark_an_entry_that_is_left_carries_is_left() {
    let rows = vec![bound("D", 2, 1, "docs", Directory), bound("X", 3, 2, "x", Directory), bound("A", 4, 1, "a.txt", File)];
    let listed = [("docs", Listed::Special), ("x-copy", Listed::Directory(Some("X"))), ("x-copy/a.txt", Listed::File(Some("A"))), ("a.txt", Listed::File(None))];
    let left = EntryIdentity::Left(IdentityDoubt::PlaceNotLookedAt);
    assert_eq!(settle(rows, &listed, &["x-copy", "x-copy/a.txt", "a.txt"]).iter().map(|(identity, _)| *identity).collect::<Vec<_>>(), [left, left, left]);
}

/// A directory moved by hand is settled before a copy of what is in it: the entry that is
/// where its row says in the moved directory is the item, whatever the walk comes to first.
#[test]
fn what_is_where_its_row_says_in_a_moved_directory_is_the_item_and_a_copy_nearer_the_root_is_not() {
    let rows = vec![bound("D", 2, 1, "docs", Directory), bound("S", 3, 2, "sub", Directory), bound("A", 4, 3, "a.txt", File)];
    let listed = [("A-copy", Listed::Directory(Some("S"))), ("a-copy.txt", Listed::File(Some("A"))), ("papers", Listed::Directory(Some("D"))), ("papers/sub", Listed::Directory(Some("S"))), ("papers/sub/a.txt", Listed::File(Some("A")))];
    assert_eq!(
        settle(rows, &listed, &["A-copy", "a-copy.txt", "papers", "papers/sub", "papers/sub/a.txt"]),
        [(EntryIdentity::NewItem, MarkOwner::AnotherItem), (EntryIdentity::NewItem, MarkOwner::AnotherItem), (item(2), MarkOwner::RowElsewhere), (item(3), MarkOwner::RowAtPath), (item(4), MarkOwner::RowAtPath)]
    );
}

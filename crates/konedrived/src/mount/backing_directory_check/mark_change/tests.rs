use super::EntryLinks::{One, Several};
use super::EntryOwner::{BoundRow, NewItem, NewItemBoundByMark, UnboundRow};
use super::{decide_mark_change, MarkChange};
use crate::mount::backing_directory_check::disk_finding::EntryMark::{OfAnotherRow, OfNoRow, OfRowAtPlace, OfSeveralEntries, Unmarked};

#[test]
fn the_own_entry_of_an_item_of_onedrive_carries_its_id_afterwards() {
    assert_eq!(decide_mark_change(BoundRow, OfRowAtPlace, One), MarkChange::Nothing);
    assert_eq!(decide_mark_change(BoundRow, Unmarked, One), MarkChange::WriteId);
    assert_eq!(decide_mark_change(BoundRow, OfAnotherRow, One), MarkChange::ReplaceWithId);
    assert_eq!(decide_mark_change(BoundRow, OfNoRow, One), MarkChange::ReplaceWithId);
}

#[test]
fn a_mark_on_a_new_item_of_the_users_own_is_taken_off() {
    assert_eq!(decide_mark_change(NewItem, Unmarked, One), MarkChange::Nothing);
    assert_eq!(decide_mark_change(NewItem, OfAnotherRow, One), MarkChange::TakeOff);
    assert_eq!(decide_mark_change(NewItem, OfRowAtPlace, One), MarkChange::TakeOff);
    assert_eq!(decide_mark_change(NewItem, OfNoRow, One), MarkChange::TakeOff);
}

#[test]
fn the_mark_a_new_item_is_bound_by_stays() {
    for mark in [Unmarked, OfRowAtPlace, OfNoRow, OfAnotherRow, OfSeveralEntries] {
        assert_eq!(decide_mark_change(NewItemBoundByMark, mark, One), MarkChange::Nothing);
    }
}

#[test]
fn the_marks_of_the_entry_of_a_row_onedrive_never_had_are_not_touched() {
    for mark in [Unmarked, OfRowAtPlace, OfNoRow, OfAnotherRow, OfSeveralEntries] {
        assert_eq!(decide_mark_change(UnboundRow, mark, One), MarkChange::Nothing);
    }
}

/// A mark is the file's, and another name of the file may be another item's.
#[test]
fn the_marks_of_a_file_that_has_several_names_are_never_written_or_taken_off() {
    for owner in [BoundRow, UnboundRow, NewItem, NewItemBoundByMark] {
        for mark in [Unmarked, OfRowAtPlace, OfNoRow, OfAnotherRow, OfSeveralEntries] {
            assert_eq!(decide_mark_change(owner, mark, Several), MarkChange::Nothing, "{owner:?} {mark:?}");
        }
    }
}

/// A copy made with its marks, in a store that was rebuilt, is the user's own, and
/// so is what it was copied from.
#[test]
fn the_mark_of_an_id_several_entries_carry_is_taken_off_each_that_has_one_name() {
    assert_eq!(decide_mark_change(NewItem, OfSeveralEntries, One), MarkChange::TakeOff);
    assert_eq!(decide_mark_change(NewItem, OfSeveralEntries, Several), MarkChange::Nothing);
    assert_eq!(decide_mark_change(BoundRow, OfSeveralEntries, One), MarkChange::ReplaceWithId);
}

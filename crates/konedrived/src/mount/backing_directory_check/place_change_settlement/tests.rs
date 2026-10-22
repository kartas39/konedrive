use super::AsideEntry as Aside;
use super::DiskPlace::{AnotherEntry, NoDirectory, NotLookedAt, Nothing, OwnEntry};
use super::PlaceChangeSettlement::{Finish, LeftStanding, NothingLeft, Undo};
use super::{decide_settlement, AsideRow, ChangeUndo, DiskPlace, LandedRow, LinkedEntry, MovedRow, RecordFacts, ReplacedRow, RowFinish, WriterFileRow};

const PLACES: [DiskPlace; 5] = [Nothing, NoDirectory, OwnEntry, AnotherEntry, NotLookedAt];
const ASIDES: [Aside; 3] = [Aside::Nothing, Aside::Entry, Aside::NotLookedAt];

// Make, of an item with no row yet: the record, the entry, the row.

#[test]
fn a_make_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: Nothing }), NothingLeft);
}

#[test]
fn a_make_that_died_after_the_disk_and_before_the_row_is_finished_with_its_row() {
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: OwnEntry }), Finish(RowFinish::MadeItem));
}

#[test]
fn a_make_whose_place_a_row_has_or_holds_the_other_kind_is_the_checks_to_decide() {
    for entry in [Nothing, OwnEntry, AnotherEntry] {
        assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: true, entry }), NothingLeft, "{entry:?}");
    }
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: AnotherEntry }), NothingLeft);
}

// The backing file of a file that has a row: the record, the row, the file.

#[test]
fn a_writers_backing_file_that_died_before_its_row_left_nothing() {
    for entry in [Nothing, OwnEntry, AnotherEntry] {
        assert_eq!(decide_settlement(RecordFacts::MadeForItem { row: WriterFileRow::NotOnDisk, entry }), NothingLeft, "{entry:?}");
        assert_eq!(decide_settlement(RecordFacts::MadeForItem { row: WriterFileRow::NotAtPlace, entry }), NothingLeft, "{entry:?}");
    }
}

#[test]
fn a_writers_backing_file_that_died_after_its_row_and_before_the_file_has_its_row_put_back() {
    assert_eq!(decide_settlement(RecordFacts::MadeForItem { row: WriterFileRow::OnDiskWithContentBeforeWrite, entry: Nothing }), Undo(ChangeUndo::ContentBeforeWrite));
}

#[test]
fn a_truncation_that_died_after_its_row_and_before_the_file_is_the_checks_to_decide() {
    assert_eq!(decide_settlement(RecordFacts::MadeForItem { row: WriterFileRow::OnDiskWithoutContentBeforeWrite, entry: Nothing }), NothingLeft);
}

#[test]
fn a_writers_backing_file_that_was_made_left_nothing() {
    for row in [WriterFileRow::OnDiskWithContentBeforeWrite, WriterFileRow::OnDiskWithoutContentBeforeWrite] {
        assert_eq!(decide_settlement(RecordFacts::MadeForItem { row, entry: OwnEntry }), NothingLeft);
        assert_eq!(decide_settlement(RecordFacts::MadeForItem { row, entry: AnotherEntry }), NothingLeft);
    }
}

// A move to a place nothing is at: the record, the disk, the row.

#[test]
fn a_move_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from: OwnEntry, to: Nothing }), NothingLeft);
}

#[test]
fn a_move_that_died_after_the_disk_and_before_the_row_is_moved_back() {
    assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from: Nothing, to: OwnEntry }), Undo(ChangeUndo::MoveBack));
}

#[test]
fn a_move_whose_row_was_written_or_whose_item_is_elsewhere_is_never_moved_back() {
    for row in [MovedRow::AtTo, MovedRow::Elsewhere] {
        for from in PLACES {
            for to in PLACES {
                assert_eq!(decide_settlement(RecordFacts::Moved { row, from, to }), NothingLeft, "{row:?} {from:?} {to:?}");
            }
        }
    }
}

/// What is at the new place of an item with nothing on disk is not its entry: a file a
/// person put there is not moved under the item's row.
#[test]
fn nothing_is_moved_back_for_an_item_with_nothing_on_disk() {
    for from in [Nothing, OwnEntry, AnotherEntry] {
        for to in [Nothing, OwnEntry, AnotherEntry] {
            assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithoutEntry, from, to }), NothingLeft, "{from:?} {to:?}");
        }
    }
}

#[test]
fn a_move_with_something_where_it_came_from_or_nothing_of_its_kind_where_it_went_is_the_checks_to_decide() {
    for (from, to) in [(OwnEntry, OwnEntry), (AnotherEntry, OwnEntry), (Nothing, Nothing), (Nothing, AnotherEntry), (AnotherEntry, Nothing)] {
        assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from, to }), NothingLeft, "{from:?} {to:?}");
    }
}

// A move over another item's entry: the record, the set-aside, the move, the row.

fn moved_over(from: DiskPlace, to: DiskPlace, aside: Aside) -> RecordFacts {
    RecordFacts::MovedOver { row: MovedRow::AtFromWithEntry, replaced: ReplacedRow::AtToWithEntry, from, to, aside }
}

#[test]
fn a_move_over_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(moved_over(OwnEntry, OwnEntry, Aside::Nothing)), NothingLeft);
}

/// An undo is two renames: a death between them leaves this state too, and the next start
/// does the second.
#[test]
fn a_move_over_that_died_after_the_set_aside_has_the_entry_put_back_under_its_name() {
    assert_eq!(decide_settlement(moved_over(OwnEntry, Nothing, Aside::Entry)), Undo(ChangeUndo::Restore));
}

#[test]
fn a_move_over_that_died_after_the_move_and_before_the_row_is_moved_back_and_the_entry_put_back() {
    assert_eq!(decide_settlement(moved_over(Nothing, OwnEntry, Aside::Entry)), Undo(ChangeUndo::MoveBackAndRestore));
}

/// With nothing set aside, the entry at the new place is the replaced item's own, where
/// its row says it has one: the move was not begun, and the moved entry is gone by hand.
#[test]
fn the_entry_of_the_replaced_item_is_never_moved_back_as_the_moved_one() {
    assert_eq!(decide_settlement(moved_over(Nothing, OwnEntry, Aside::Nothing)), NothingLeft);
}

/// The replaced file had nothing on disk to set aside: the entry at its place is the moved one.
#[test]
fn a_move_over_an_item_with_nothing_on_disk_is_moved_back() {
    let facts = RecordFacts::MovedOver { row: MovedRow::AtFromWithEntry, replaced: ReplacedRow::AtToWithoutEntry, from: Nothing, to: OwnEntry, aside: Aside::Nothing };
    assert_eq!(decide_settlement(facts), Undo(ChangeUndo::MoveBack));
}

/// The moved file has nothing on disk: all the disk saw is the set-aside.
#[test]
fn a_move_of_an_item_with_nothing_on_disk_over_an_entry_has_that_entry_put_back_and_nothing_moved() {
    let facts = |to, aside| RecordFacts::MovedOver { row: MovedRow::AtFromWithoutEntry, replaced: ReplacedRow::AtToWithEntry, from: Nothing, to, aside };
    assert_eq!(decide_settlement(facts(Nothing, Aside::Entry)), Undo(ChangeUndo::Restore));
    assert_eq!(decide_settlement(facts(OwnEntry, Aside::Nothing)), NothingLeft);
    assert_eq!(decide_settlement(facts(OwnEntry, Aside::Entry)), NothingLeft);
}

#[test]
fn a_move_over_whose_row_was_written_is_never_undone() {
    for row in [MovedRow::AtTo, MovedRow::Elsewhere] {
        for replaced in [ReplacedRow::AtToWithEntry, ReplacedRow::AtToWithoutEntry, ReplacedRow::NotAtTo] {
            for from in PLACES {
                for to in PLACES {
                    for aside in ASIDES {
                        assert_eq!(decide_settlement(RecordFacts::MovedOver { row, replaced, from, to, aside }), NothingLeft);
                    }
                }
            }
        }
    }
    for from in PLACES {
        for to in PLACES {
            for aside in ASIDES {
                let facts = RecordFacts::MovedOver { row: MovedRow::AtFromWithEntry, replaced: ReplacedRow::NotAtTo, from, to, aside };
                assert_eq!(decide_settlement(facts), NothingLeft, "the replaced item's row is not at the place any more");
            }
        }
    }
}

#[test]
fn a_move_over_with_another_entry_where_the_set_aside_one_belongs_is_the_checks_to_decide() {
    assert_eq!(decide_settlement(moved_over(OwnEntry, OwnEntry, Aside::Entry)), NothingLeft);
    assert_eq!(decide_settlement(moved_over(Nothing, AnotherEntry, Aside::Entry)), NothingLeft);
    assert_eq!(decide_settlement(moved_over(Nothing, Nothing, Aside::Nothing)), NothingLeft);
}

#[test]
fn the_entry_set_aside_is_put_back_also_when_the_moved_one_is_gone() {
    assert_eq!(decide_settlement(moved_over(Nothing, Nothing, Aside::Entry)), Undo(ChangeUndo::Restore));
}

// A set-aside before a removal: the record, the set-aside, the row.

#[test]
fn a_set_aside_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: OwnEntry, aside: Aside::Nothing }), NothingLeft);
}

#[test]
fn a_set_aside_that_died_before_its_row_has_the_entry_put_back_under_its_name() {
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: Nothing, aside: Aside::Entry }), Undo(ChangeUndo::Restore));
}

/// The row of a removal, or of OneDrive's content taken, was written: the entry that was
/// set aside is the old copy, and putting it back would make it content changed here.
#[test]
fn an_entry_set_aside_is_never_put_back_once_its_row_was_written() {
    for place in PLACES {
        for aside in ASIDES {
            assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::Otherwise, place, aside }), NothingLeft, "{place:?} {aside:?}");
        }
    }
}

#[test]
fn an_entry_set_aside_is_not_put_back_over_what_is_at_its_name_now() {
    for place in [OwnEntry, AnotherEntry] {
        assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place, aside: Aside::Entry }), NothingLeft);
    }
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: Nothing, aside: Aside::Nothing }), NothingLeft);
}

// A landed download: the record, the link, the row.

#[test]
fn a_landing_that_died_before_the_link_left_nothing() {
    for row in [LandedRow::NotAtPlace, LandedRow::NotOnDiskWithRecordedContent, LandedRow::NotOnDiskWithOtherContent, LandedRow::OnDisk] {
        assert_eq!(decide_settlement(RecordFacts::Linked { row, entry: LinkedEntry::Nothing }), NothingLeft, "{row:?}");
    }
}

#[test]
fn a_landing_that_died_after_the_link_and_before_its_row_is_finished_with_its_row() {
    assert_eq!(decide_settlement(RecordFacts::Linked { row: LandedRow::NotOnDiskWithRecordedContent, entry: LinkedEntry::FileOfRecordedContent }), Finish(RowFinish::ContentOnDisk));
}

/// The file is the daemon's own download of a content the row has no more: left, it would
/// be read as content the user put there, and sent over what OneDrive has.
#[test]
fn a_landed_file_whose_row_has_another_content_by_now_is_removed() {
    assert_eq!(decide_settlement(RecordFacts::Linked { row: LandedRow::NotOnDiskWithOtherContent, entry: LinkedEntry::FileOfRecordedContent }), Undo(ChangeUndo::RemoveLinkedFile));
}

#[test]
fn a_file_that_is_not_the_one_that_landed_or_a_row_that_says_on_disk_or_is_gone_is_the_checks_to_decide() {
    for row in [LandedRow::NotAtPlace, LandedRow::NotOnDiskWithRecordedContent, LandedRow::NotOnDiskWithOtherContent, LandedRow::OnDisk] {
        assert_eq!(decide_settlement(RecordFacts::Linked { row, entry: LinkedEntry::AnotherEntry }), NothingLeft, "{row:?}");
    }
    for row in [LandedRow::NotAtPlace, LandedRow::OnDisk] {
        assert_eq!(decide_settlement(RecordFacts::Linked { row, entry: LinkedEntry::FileOfRecordedContent }), NothingLeft, "{row:?}");
    }
}

// A place that could not be looked at.

#[test]
fn a_record_whose_place_could_not_be_looked_at_stands_while_its_row_was_not_written() {
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: NotLookedAt }), LeftStanding);
    assert_eq!(decide_settlement(RecordFacts::MadeForItem { row: WriterFileRow::OnDiskWithContentBeforeWrite, entry: NotLookedAt }), LeftStanding);
    for (from, to) in [(NotLookedAt, OwnEntry), (Nothing, NotLookedAt), (NotLookedAt, NotLookedAt)] {
        assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from, to }), LeftStanding, "{from:?} {to:?}");
    }
    for (from, to, aside) in [(NotLookedAt, OwnEntry, Aside::Entry), (Nothing, NotLookedAt, Aside::Entry), (Nothing, OwnEntry, Aside::NotLookedAt)] {
        assert_eq!(decide_settlement(moved_over(from, to, aside)), LeftStanding, "{from:?} {to:?} {aside:?}");
    }
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: NotLookedAt, aside: Aside::Entry }), LeftStanding);
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: Nothing, aside: Aside::NotLookedAt }), LeftStanding);
    assert_eq!(decide_settlement(RecordFacts::Linked { row: LandedRow::NotOnDiskWithRecordedContent, entry: LinkedEntry::NotLookedAt }), LeftStanding);
}

// An undo that cannot be done: the record goes, and the entries are the check's to decide.

/// `mv mine/own.txt moved.txt`, a death after the disk, and then `rmdir mine`
/// by hand. The record stood at every start, and the moved file had no row for good.
#[test]
fn a_move_is_not_moved_back_into_a_directory_that_is_gone_and_its_record_does_not_stand() {
    assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from: NoDirectory, to: OwnEntry }), NothingLeft);
    assert_eq!(decide_settlement(moved_over(NoDirectory, OwnEntry, Aside::Entry)), NothingLeft);
    assert_eq!(decide_settlement(moved_over(NoDirectory, OwnEntry, Aside::Nothing)), NothingLeft);
}

/// `rm docs/a.txt`, a death after the set-aside, and then `rm -r docs` by hand.
#[test]
fn an_entry_set_aside_is_not_put_back_into_a_directory_that_is_gone_and_its_record_does_not_stand() {
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: NoDirectory, aside: Aside::Entry }), NothingLeft);
    for from in [Nothing, NoDirectory, OwnEntry] {
        assert_eq!(decide_settlement(moved_over(from, NoDirectory, Aside::Entry)), NothingLeft, "{from:?}");
    }
}

/// An editor's save over a file, a death after the disk, and the name saved
/// again by hand. What is at the new place is the person's file, not the moved entry.
#[test]
fn a_move_is_not_moved_back_when_what_is_at_the_new_place_is_not_the_moved_entry() {
    assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from: Nothing, to: AnotherEntry }), NothingLeft);
    assert_eq!(decide_settlement(moved_over(Nothing, AnotherEntry, Aside::Entry)), NothingLeft);
    let over_nothing_on_disk = RecordFacts::MovedOver { row: MovedRow::AtFromWithEntry, replaced: ReplacedRow::AtToWithoutEntry, from: Nothing, to: AnotherEntry, aside: Aside::Nothing };
    assert_eq!(decide_settlement(over_nothing_on_disk), NothingLeft);
}

#[test]
fn a_made_entry_or_a_writers_file_whose_directory_is_gone_is_as_one_that_is_not_there() {
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: NoDirectory }), NothingLeft);
    assert_eq!(decide_settlement(RecordFacts::MadeForItem { row: WriterFileRow::OnDiskWithContentBeforeWrite, entry: NoDirectory }), Undo(ChangeUndo::ContentBeforeWrite));
}

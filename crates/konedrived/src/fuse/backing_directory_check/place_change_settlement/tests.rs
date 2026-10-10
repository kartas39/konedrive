use super::AsideEntry as Aside;
use super::DiskPlace::{NotLookedAt, Nothing, OtherKind, OwnKind};
use super::PlaceChangeSettlement::{Finish, LeftStanding, NothingLeft, Undo};
use super::{decide_settlement, AsideRow, ChangeUndo, DiskPlace, LandedRow, LinkedEntry, MovedRow, RecordFacts, ReplacedRow, RowFinish, WriterFileRow};

const PLACES: [DiskPlace; 4] = [Nothing, OwnKind, OtherKind, NotLookedAt];
const ASIDES: [Aside; 3] = [Aside::Nothing, Aside::Entry, Aside::NotLookedAt];

// Make, of an item with no row yet: the record, the entry, the row.

#[test]
fn a_make_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: Nothing }), NothingLeft);
}

#[test]
fn a_make_that_died_after_the_disk_and_before_the_row_is_finished_with_its_row() {
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: OwnKind }), Finish(RowFinish::MadeItem));
}

#[test]
fn a_make_whose_place_a_row_has_or_holds_the_other_kind_is_the_checks_to_decide() {
    for entry in [Nothing, OwnKind, OtherKind] {
        assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: true, entry }), NothingLeft, "{entry:?}");
    }
    assert_eq!(decide_settlement(RecordFacts::MadeForNewItem { is_place_shown: false, entry: OtherKind }), NothingLeft);
}

// The backing file of a file that has a row: the record, the row, the file.

#[test]
fn a_writers_backing_file_that_died_before_its_row_left_nothing() {
    for entry in [Nothing, OwnKind, OtherKind] {
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
        assert_eq!(decide_settlement(RecordFacts::MadeForItem { row, entry: OwnKind }), NothingLeft);
        assert_eq!(decide_settlement(RecordFacts::MadeForItem { row, entry: OtherKind }), NothingLeft);
    }
}

// A move to a place nothing is at: the record, the disk, the row.

#[test]
fn a_move_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from: OwnKind, to: Nothing }), NothingLeft);
}

#[test]
fn a_move_that_died_after_the_disk_and_before_the_row_is_moved_back() {
    assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from: Nothing, to: OwnKind }), Undo(ChangeUndo::MoveBack));
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
    for from in [Nothing, OwnKind, OtherKind] {
        for to in [Nothing, OwnKind, OtherKind] {
            assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithoutEntry, from, to }), NothingLeft, "{from:?} {to:?}");
        }
    }
}

#[test]
fn a_move_with_something_where_it_came_from_or_nothing_of_its_kind_where_it_went_is_the_checks_to_decide() {
    for (from, to) in [(OwnKind, OwnKind), (OtherKind, OwnKind), (Nothing, Nothing), (Nothing, OtherKind), (OtherKind, Nothing)] {
        assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from, to }), NothingLeft, "{from:?} {to:?}");
    }
}

// A move over another item's entry: the record, the set-aside, the move, the row.

fn moved_over(from: DiskPlace, to: DiskPlace, aside: Aside) -> RecordFacts {
    RecordFacts::MovedOver { row: MovedRow::AtFromWithEntry, replaced: ReplacedRow::AtToWithEntry, from, to, aside }
}

#[test]
fn a_move_over_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(moved_over(OwnKind, OwnKind, Aside::Nothing)), NothingLeft);
}

#[test]
fn a_move_over_that_died_after_the_set_aside_has_the_entry_put_back_under_its_name() {
    assert_eq!(decide_settlement(moved_over(OwnKind, Nothing, Aside::Entry)), Undo(ChangeUndo::Restore));
}

#[test]
fn a_move_over_that_died_after_the_move_and_before_the_row_is_moved_back_and_the_entry_put_back() {
    assert_eq!(decide_settlement(moved_over(Nothing, OwnKind, Aside::Entry)), Undo(ChangeUndo::MoveBackAndRestore));
}

/// The undo is two renames; a death between them leaves the first done, and the next start
/// does the second.
#[test]
fn an_undo_of_a_move_over_that_died_half_way_is_gone_on_with() {
    assert_eq!(decide_settlement(moved_over(OwnKind, Nothing, Aside::Entry)), Undo(ChangeUndo::Restore));
}

/// With nothing set aside, the entry at the new place is the replaced item's own, where
/// its row says it has one: the move was not begun, and the moved entry is gone by hand.
#[test]
fn the_entry_of_the_replaced_item_is_never_moved_back_as_the_moved_one() {
    assert_eq!(decide_settlement(moved_over(Nothing, OwnKind, Aside::Nothing)), NothingLeft);
}

/// The replaced file had nothing on disk to set aside: the entry at its place is the moved one.
#[test]
fn a_move_over_an_item_with_nothing_on_disk_is_moved_back() {
    let facts = RecordFacts::MovedOver { row: MovedRow::AtFromWithEntry, replaced: ReplacedRow::AtToWithoutEntry, from: Nothing, to: OwnKind, aside: Aside::Nothing };
    assert_eq!(decide_settlement(facts), Undo(ChangeUndo::MoveBack));
}

/// The moved file has nothing on disk: all the disk saw is the set-aside.
#[test]
fn a_move_of_an_item_with_nothing_on_disk_over_an_entry_has_that_entry_put_back_and_nothing_moved() {
    let facts = |to, aside| RecordFacts::MovedOver { row: MovedRow::AtFromWithoutEntry, replaced: ReplacedRow::AtToWithEntry, from: Nothing, to, aside };
    assert_eq!(decide_settlement(facts(Nothing, Aside::Entry)), Undo(ChangeUndo::Restore));
    assert_eq!(decide_settlement(facts(OwnKind, Aside::Nothing)), NothingLeft);
    assert_eq!(decide_settlement(facts(OwnKind, Aside::Entry)), NothingLeft);
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
    assert_eq!(decide_settlement(moved_over(OwnKind, OwnKind, Aside::Entry)), NothingLeft);
    assert_eq!(decide_settlement(moved_over(Nothing, OtherKind, Aside::Entry)), NothingLeft);
    assert_eq!(decide_settlement(moved_over(Nothing, Nothing, Aside::Nothing)), NothingLeft);
}

#[test]
fn the_entry_set_aside_is_put_back_also_when_the_moved_one_is_gone() {
    assert_eq!(decide_settlement(moved_over(Nothing, Nothing, Aside::Entry)), Undo(ChangeUndo::Restore));
}

// A set-aside before a removal: the record, the set-aside, the row.

#[test]
fn a_set_aside_that_died_before_the_disk_left_nothing() {
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: OwnKind, aside: Aside::Nothing }), NothingLeft);
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
    for place in [OwnKind, OtherKind] {
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
    for (from, to) in [(NotLookedAt, OwnKind), (Nothing, NotLookedAt), (NotLookedAt, NotLookedAt)] {
        assert_eq!(decide_settlement(RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from, to }), LeftStanding, "{from:?} {to:?}");
    }
    for (from, to, aside) in [(NotLookedAt, OwnKind, Aside::Entry), (Nothing, NotLookedAt, Aside::Entry), (Nothing, OwnKind, Aside::NotLookedAt)] {
        assert_eq!(decide_settlement(moved_over(from, to, aside)), LeftStanding, "{from:?} {to:?} {aside:?}");
    }
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: NotLookedAt, aside: Aside::Entry }), LeftStanding);
    assert_eq!(decide_settlement(RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: Nothing, aside: Aside::NotLookedAt }), LeftStanding);
    assert_eq!(decide_settlement(RecordFacts::Linked { row: LandedRow::NotOnDiskWithRecordedContent, entry: LinkedEntry::NotLookedAt }), LeftStanding);
}

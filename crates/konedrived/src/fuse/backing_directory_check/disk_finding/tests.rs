use konedrive_tree::{ContentPresence, ContentVersion};

use super::ContentOrigin::{ChangedHere, FromOneDrive};
use super::ContentComparison::{Different, Same};
use super::CopyWholeness::{Partial, Whole};
use super::ItemBinding::{Bound, Unbound};
use super::MarkOwner::{RowOfAnotherEntry, RowElsewhere, RowAtPath, NoRow, Unmarked};
use super::{compare_content, decide_finding, ContentOrigin, ContentComparison, CopyWholeness, DiskFinding, DiskContent, ItemBinding, MarkOwner, PathEntry, PathRow, UnresolvedReason};
use konedrive_tree::ContentPresence::{NotOnDisk, OnDisk};

fn file_row(binding: ItemBinding, content_presence: ContentPresence, origin: ContentOrigin) -> PathRow {
    PathRow::File { binding, content_presence, origin }
}

/// A clean downloaded copy, as its row says it.
const CLEAN_COPY: PathRow = PathRow::File { binding: Bound, content_presence: OnDisk, origin: FromOneDrive };
/// A file of OneDrive changed here and not sent.
const CHANGED_COPY: PathRow = PathRow::File { binding: Bound, content_presence: OnDisk, origin: ChangedHere };
/// A file OneDrive never had.
const OWN_FILE: PathRow = PathRow::File { binding: Unbound, content_presence: OnDisk, origin: ChangedHere };
/// A file of OneDrive that is not downloaded.
const NOT_DOWNLOADED: PathRow = PathRow::File { binding: Bound, content_presence: NotOnDisk, origin: FromOneDrive };

fn file(mark: MarkOwner, wholeness: CopyWholeness, comparison: ContentComparison) -> PathEntry {
    PathEntry::File { mark, wholeness, comparison }
}

fn directory(mark: MarkOwner) -> PathEntry {
    PathEntry::Directory { mark }
}

fn found(row: PathRow, entry: PathEntry) -> DiskFinding {
    decide_finding(row, entry).unwrap_or_else(|reason| panic!("{row:?} and {entry:?} are left unresolved: {reason:?}"))
}

// What the rows have no row for.

#[test]
fn an_unmarked_file_or_directory_no_row_knows_is_a_new_item() {
    assert_eq!(found(PathRow::Nothing, file(Unmarked, Whole, Different)), DiskFinding::Made);
    assert_eq!(found(PathRow::Nothing, directory(Unmarked)), DiskFinding::Made);
}

#[test]
fn an_entry_marked_as_an_item_whose_row_is_elsewhere_with_nothing_there_is_that_item_moved() {
    assert_eq!(found(PathRow::Nothing, file(RowElsewhere, Whole, Different)), DiskFinding::Moved);
    assert_eq!(found(PathRow::Nothing, file(RowElsewhere, Partial, Different)), DiskFinding::Moved);
    assert_eq!(found(PathRow::Nothing, directory(RowElsewhere)), DiskFinding::Moved);
}

#[test]
fn a_second_entry_marked_as_an_item_that_is_where_its_row_says_is_a_new_item_with_a_stale_mark() {
    assert_eq!(found(PathRow::Nothing, file(RowOfAnotherEntry, Whole, Different)), DiskFinding::MadeWithStaleMark);
    assert_eq!(found(PathRow::Nothing, file(RowOfAnotherEntry, Partial, Different)), DiskFinding::MadeWithStaleMark);
    assert_eq!(found(PathRow::Nothing, directory(RowOfAnotherEntry)), DiskFinding::MadeWithStaleMark);
}

#[test]
fn an_entry_marked_with_an_id_no_row_has_is_a_new_item_bound_to_it() {
    assert_eq!(found(PathRow::Nothing, file(NoRow, Whole, Different)), DiskFinding::MarkedWithoutRow);
    assert_eq!(found(PathRow::Nothing, directory(NoRow)), DiskFinding::MarkedWithoutRow);
}

/// It holds nothing of the file, and a made item's content is on disk: no row can say what
/// it is.
#[test]
fn a_file_that_is_not_whole_and_marked_with_an_id_no_row_has_is_left_unresolved() {
    assert_eq!(decide_finding(PathRow::Nothing, file(NoRow, Partial, Different)), Err(UnresolvedReason::PartialCopy));
    assert_eq!(decide_finding(PathRow::AnotherItemsRow, file(NoRow, Partial, Different)), Err(UnresolvedReason::PartialCopy));
}

// What stands where another item's row is.

#[test]
fn an_item_moved_to_a_path_another_items_row_has_is_moved_over_it() {
    assert_eq!(found(PathRow::AnotherItemsRow, file(RowElsewhere, Whole, Different)), DiskFinding::MovedOverAnotherItem);
    assert_eq!(found(PathRow::AnotherItemsRow, directory(RowElsewhere)), DiskFinding::MovedOverAnotherItem);
}

/// The matcher says "another item" of a row the entry is not; said of the row itself, a
/// moved item is over it all the same.
#[test]
fn an_item_moved_to_a_path_is_over_the_row_that_has_it_whatever_the_row_is() {
    assert_eq!(found(CLEAN_COPY, file(RowElsewhere, Whole, Same)), DiskFinding::MovedOverAnotherItem);
    assert_eq!(found(NOT_DOWNLOADED, file(RowElsewhere, Whole, Same)), DiskFinding::MovedOverAnotherItem);
    assert_eq!(found(PathRow::Directory { binding: Bound }, directory(RowElsewhere)), DiskFinding::MovedOverAnotherItem);
}

#[test]
fn what_was_made_by_hand_where_another_items_row_is_is_a_new_item() {
    assert_eq!(found(PathRow::AnotherItemsRow, file(Unmarked, Whole, Different)), DiskFinding::Made);
    assert_eq!(found(PathRow::AnotherItemsRow, directory(Unmarked)), DiskFinding::Made);
    assert_eq!(found(PathRow::AnotherItemsRow, directory(RowOfAnotherEntry)), DiskFinding::MadeWithStaleMark);
    assert_eq!(found(PathRow::AnotherItemsRow, file(NoRow, Whole, Different)), DiskFinding::MarkedWithoutRow);
}

/// A file is never the item of a directory's row, nor a directory of a file's.
#[test]
fn an_entry_of_the_other_kind_than_the_row_is_decided_as_where_another_items_row_is() {
    assert_eq!(found(CLEAN_COPY, directory(Unmarked)), DiskFinding::Made);
    assert_eq!(found(PathRow::Directory { binding: Bound }, file(Unmarked, Whole, Different)), DiskFinding::Made);
    assert_eq!(found(PathRow::Directory { binding: Unbound }, file(RowElsewhere, Whole, Different)), DiskFinding::MovedOverAnotherItem);
    assert_eq!(found(OWN_FILE, directory(NoRow)), DiskFinding::MarkedWithoutRow);
}

/// A file of OneDrive whose content was not on disk, opened by a writer of a daemon that died.
const NOT_DOWNLOADED_AND_OPENED: PathRow = PathRow::File { binding: Bound, content_presence: NotOnDisk, origin: ChangedHere };
/// A file with no binding whose content is not on disk.
const OWN_FILE_NOT_ON_DISK: PathRow = PathRow::File { binding: Unbound, content_presence: NotOnDisk, origin: ChangedHere };

// A row no entry is taken as, that cannot stay where it is.

/// The person put something where a file they had before them was, or where an item of
/// their own was: it is removed here.
#[test]
fn a_row_whose_name_another_item_has_on_disk_is_removed_here_when_its_copy_was_on_disk_or_onedrive_never_had_it() {
    for row in [CLEAN_COPY, CHANGED_COPY, OWN_FILE, OWN_FILE_NOT_ON_DISK, PathRow::Directory { binding: Unbound }] {
        assert_eq!(found(row, PathEntry::AnotherItemsEntry), DiskFinding::Removed, "{row:?}");
    }
}

/// What the person could not see is not removed: its place is lost, and it stays OneDrive's.
#[test]
fn an_item_of_onedrive_with_nothing_of_its_own_on_disk_whose_name_another_item_has_loses_its_place() {
    for row in [NOT_DOWNLOADED, NOT_DOWNLOADED_AND_OPENED, PathRow::Directory { binding: Bound }] {
        assert_eq!(found(row, PathEntry::AnotherItemsEntry), DiskFinding::PlaceLost, "{row:?}");
    }
}

/// The row of its directory goes, or its name is one the check sets items aside to. An item
/// OneDrive never had goes with it, and so does a file changed here whose copy was on disk:
/// the person had it before them.
#[test]
fn a_row_whose_path_is_no_more_is_removed_here_when_onedrive_never_had_it_or_it_was_changed_here_on_disk() {
    for row in [CHANGED_COPY, OWN_FILE, OWN_FILE_NOT_ON_DISK, PathRow::Directory { binding: Unbound }] {
        assert_eq!(found(row, PathEntry::NoSuchPath), DiskFinding::Removed, "{row:?}");
    }
}

/// Any other item of OneDrive loses its place: one with nothing on disk, and a clean copy
/// that is gone, which is no removal anywhere.
#[test]
fn an_item_of_onedrive_whose_path_is_no_more_loses_its_place_unless_it_was_changed_here_on_disk() {
    for row in [NOT_DOWNLOADED, NOT_DOWNLOADED_AND_OPENED, CLEAN_COPY, PathRow::Directory { binding: Bound }] {
        assert_eq!(found(row, PathEntry::NoSuchPath), DiskFinding::PlaceLost, "{row:?}");
    }
    assert_eq!(found(PathRow::Nothing, PathEntry::NoSuchPath), DiskFinding::NothingChanged);
}

// A file's row, and a file that is its item.

#[test]
fn a_copy_of_the_rows_size_and_time_is_unchanged() {
    for row in [CLEAN_COPY, CHANGED_COPY, OWN_FILE] {
        assert_eq!(found(row, file(RowAtPath, Whole, Same)), DiskFinding::NothingChanged, "{row:?}");
        assert_eq!(found(row, file(Unmarked, Whole, Same)), DiskFinding::NothingChanged, "{row:?}");
    }
}

#[test]
fn a_file_of_another_size_or_time_than_the_row_has_its_content_changed() {
    for row in [CLEAN_COPY, CHANGED_COPY, OWN_FILE] {
        assert_eq!(found(row, file(RowAtPath, Whole, Different)), DiskFinding::ContentChanged, "{row:?}");
        assert_eq!(found(row, file(Unmarked, Whole, Different)), DiskFinding::ContentChanged, "{row:?}");
    }
}

#[test]
fn a_copy_changed_here_that_is_gone_is_removed_by_hand() {
    assert_eq!(found(CHANGED_COPY, PathEntry::Nothing), DiskFinding::Removed);
    assert_eq!(found(OWN_FILE, PathEntry::Nothing), DiskFinding::Removed);
}

/// A power loss can leave the same picture, and a wrong removal destroys the file everywhere.
#[test]
fn a_clean_copy_that_is_gone_is_not_on_disk_any_more_and_not_removed() {
    assert_eq!(found(CLEAN_COPY, PathEntry::Nothing), DiskFinding::ContentNoLongerOnDisk);
}

#[test]
fn content_that_is_not_on_disk_and_nothing_there_is_as_expected() {
    assert_eq!(found(NOT_DOWNLOADED, PathEntry::Nothing), DiskFinding::NothingChanged);
    assert_eq!(found(file_row(Bound, NotOnDisk, ChangedHere), PathEntry::Nothing), DiskFinding::NothingChanged);
}

#[test]
fn the_whole_copy_of_a_file_whose_content_is_not_on_disk_landed_before_its_row() {
    assert_eq!(found(NOT_DOWNLOADED, file(RowAtPath, Whole, Same)), DiskFinding::CopyLandedBeforeRow);
}

#[test]
fn any_other_file_where_content_is_not_on_disk_is_content_the_user_put_there() {
    assert_eq!(found(NOT_DOWNLOADED, file(Unmarked, Whole, Same)), DiskFinding::ContentChanged);
    assert_eq!(found(NOT_DOWNLOADED, file(Unmarked, Whole, Different)), DiskFinding::ContentChanged);
    assert_eq!(found(NOT_DOWNLOADED, file(RowAtPath, Whole, Different)), DiskFinding::ContentChanged, "a whole copy of another size or time");
    assert_eq!(found(NOT_DOWNLOADED, file(RowOfAnotherEntry, Whole, Same)), DiskFinding::ContentChanged);
}

/// A file marked with the row's id and not whole holds nothing of the file, unless a writer
/// began to change it: then it holds the user's bytes.
#[test]
fn a_file_that_is_not_whole_is_no_content_unless_a_writer_began_to_change_it() {
    assert_eq!(found(NOT_DOWNLOADED, file(RowAtPath, Partial, Different)), DiskFinding::NothingChanged);
    assert_eq!(found(NOT_DOWNLOADED, file(RowAtPath, Partial, Same)), DiskFinding::NothingChanged);
    assert_eq!(found(file_row(Bound, NotOnDisk, ChangedHere), file(RowAtPath, Partial, Different)), DiskFinding::ContentChanged);
    assert_eq!(found(file_row(Bound, NotOnDisk, ChangedHere), file(RowAtPath, Partial, Same)), DiskFinding::ContentChanged);
}

#[test]
fn a_clean_copy_that_is_not_whole_is_not_on_disk_any_more() {
    assert_eq!(found(CLEAN_COPY, file(RowAtPath, Partial, Same)), DiskFinding::ContentNoLongerOnDisk);
    assert_eq!(found(CLEAN_COPY, file(RowAtPath, Partial, Different)), DiskFinding::ContentNoLongerOnDisk);
}

#[test]
fn a_copy_changed_here_that_is_not_whole_is_told_by_its_size_and_time() {
    assert_eq!(found(CHANGED_COPY, file(RowAtPath, Partial, Same)), DiskFinding::NothingChanged);
    assert_eq!(found(CHANGED_COPY, file(RowAtPath, Partial, Different)), DiskFinding::ContentChanged);
}

/// Another item's bytes were laid over the file: its size and time may be the row's by
/// chance, the mark says they are not its content.
#[test]
fn a_file_that_carries_another_items_mark_where_its_row_is_has_its_content_changed() {
    for row in [CLEAN_COPY, CHANGED_COPY, OWN_FILE] {
        assert_eq!(found(row, file(RowOfAnotherEntry, Whole, Same)), DiskFinding::ContentChanged, "{row:?}");
    }
    assert_eq!(found(CLEAN_COPY, file(NoRow, Whole, Same)), DiskFinding::ContentChanged);
    assert_eq!(found(CHANGED_COPY, file(NoRow, Whole, Different)), DiskFinding::ContentChanged);
}

/// A daemon that died between marking an uploaded file and its row.
#[test]
fn an_item_with_no_binding_whose_entry_is_marked_with_an_id_no_row_has_is_bound_to_it() {
    assert_eq!(found(OWN_FILE, file(NoRow, Whole, Same)), DiskFinding::MarkedWithoutRow);
    assert_eq!(found(OWN_FILE, file(NoRow, Whole, Different)), DiskFinding::MarkedWithoutRow);
    assert_eq!(found(PathRow::Directory { binding: Unbound }, directory(NoRow)), DiskFinding::MarkedWithoutRow);
}

// A directory's row.

#[test]
fn a_directory_where_its_row_is_is_unchanged_whatever_it_is_marked_with() {
    for binding in [Bound, Unbound] {
        for mark in [Unmarked, RowAtPath, RowOfAnotherEntry] {
            assert_eq!(found(PathRow::Directory { binding }, directory(mark)), DiskFinding::NothingChanged, "{binding:?} {mark:?}");
        }
    }
    assert_eq!(found(PathRow::Directory { binding: Bound }, directory(NoRow)), DiskFinding::NothingChanged);
}

#[test]
fn a_directory_of_onedrive_needs_none_on_disk() {
    assert_eq!(found(PathRow::Directory { binding: Bound }, PathEntry::Nothing), DiskFinding::NothingChanged);
}

#[test]
fn a_directory_onedrive_does_not_have_that_is_gone_is_removed_by_hand() {
    assert_eq!(found(PathRow::Directory { binding: Unbound }, PathEntry::Nothing), DiskFinding::Removed);
}

#[test]
fn a_path_with_no_row_and_nothing_on_disk_is_unchanged() {
    assert_eq!(found(PathRow::Nothing, PathEntry::Nothing), DiskFinding::NothingChanged);
    assert_eq!(found(PathRow::Nothing, PathEntry::AnotherItemsEntry), DiskFinding::NothingChanged);
    assert_eq!(found(PathRow::AnotherItemsRow, PathEntry::Nothing), DiskFinding::NothingChanged);
}

// The comparison of a file with its row.

fn on_disk(size: u64, mtime: i64, mtime_nsec: u32, ctag: Option<&str>) -> DiskContent {
    DiskContent { size, mtime, mtime_nsec, ctag: ctag.map(str::to_owned) }
}

fn hashed(ctag: Option<&str>) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some("h".to_owned()), ctag: ctag.map(str::to_owned) }
}

#[test]
fn a_copy_without_a_ctag_mark_is_the_rows_content_when_its_size_and_whole_seconds_are() {
    assert_eq!(compare_content(&hashed(Some("c1")), &on_disk(3, 100, 0, None)), Same);
    assert_eq!(compare_content(&hashed(Some("c1")), &on_disk(3, 100, 999_999_999, None)), Same, "the time in whole seconds");
    assert_eq!(compare_content(&hashed(Some("c1")), &on_disk(4, 100, 0, None)), Different);
    assert_eq!(compare_content(&hashed(Some("c1")), &on_disk(3, 101, 0, None)), Different);
    assert_eq!(compare_content(&hashed(Some("c1")), &on_disk(3, 99, 0, None)), Different);
}

#[test]
fn a_copy_with_a_ctag_mark_is_the_rows_content_when_the_mark_is_the_rows_ctag_too() {
    assert_eq!(compare_content(&hashed(Some("c1")), &on_disk(3, 100, 0, Some("c1"))), Same);
    assert_eq!(compare_content(&hashed(Some("c2")), &on_disk(3, 100, 0, Some("c1"))), Different, "a clean copy of an older version");
    assert_eq!(compare_content(&hashed(Some("c1")), &on_disk(4, 100, 0, Some("c1"))), Different);
}

/// The mark is of the copy a change began from; the row of a content changed here has no
/// cTag to compare it with.
#[test]
fn the_ctag_mark_is_not_asked_of_a_row_that_has_no_ctag() {
    let changed_here = ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 5, change_count: 2 };
    assert_eq!(compare_content(&changed_here, &on_disk(3, 100, 5, Some("c1"))), Same);
    assert_eq!(compare_content(&changed_here, &on_disk(3, 100, 0, None)), Same);
    assert_eq!(compare_content(&changed_here, &on_disk(3, 200, 5, Some("c1"))), Different);
    assert_eq!(compare_content(&hashed(None), &on_disk(3, 100, 0, Some("c1"))), Same);
}

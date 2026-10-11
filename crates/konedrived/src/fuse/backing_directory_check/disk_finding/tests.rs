use konedrive_tree::ContentPresence::{NotOnDisk, OnDisk};
use konedrive_tree::ContentVersion;

use super::ContentComparison::{Different, Same};
use super::CopyWholeness::{NotWhole, Whole};
use super::EntryMark::{OfAnotherRow, OfNoRow, OfRowAtPlace, OfSeveralEntries, Unmarked};
use super::ItemBinding::{Bound, Unbound};
use super::WriterAtStop::{Closed, Open};
use crate::fuse::backing_directory_check::mark_change::EntryLinks;
use super::{compare_content, decide_finding, decide_finding_without_place, ContentComparison, CopyWholeness, DiskContent, DiskFinding, EntryMark, PlaceEntry, PlaceRow};

/// A downloaded copy, clean or changed here: the row says its content is on disk.
const COPY: PlaceRow = PlaceRow::File { binding: Bound, content_presence: OnDisk, writer: Closed };
/// A file OneDrive never had.
const OWN_FILE: PlaceRow = PlaceRow::File { binding: Unbound, content_presence: OnDisk, writer: Closed };
/// A file of OneDrive that is not downloaded.
const NOT_DOWNLOADED: PlaceRow = PlaceRow::File { binding: Bound, content_presence: NotOnDisk, writer: Closed };
const ONEDRIVE_DIRECTORY: PlaceRow = PlaceRow::Directory { binding: Bound };
const OWN_DIRECTORY: PlaceRow = PlaceRow::Directory { binding: Unbound };

const MARKS: [EntryMark; 5] = [Unmarked, OfRowAtPlace, OfNoRow, OfAnotherRow, OfSeveralEntries];
const WHOLENESSES: [CopyWholeness; 2] = [Whole, NotWhole];
const COMPARISONS: [ContentComparison; 2] = [Same, Different];

fn file(mark: EntryMark, wholeness: CopyWholeness, comparison: ContentComparison) -> PlaceEntry {
    PlaceEntry::File { mark, wholeness, comparison }
}

/// Every file an entry can be.
fn every_file() -> Vec<PlaceEntry> {
    MARKS.into_iter().flat_map(|mark| WHOLENESSES.into_iter().flat_map(move |wholeness| COMPARISONS.into_iter().map(move |comparison| file(mark, wholeness, comparison)))).collect()
}

fn every_directory() -> Vec<PlaceEntry> {
    MARKS.into_iter().map(|mark| PlaceEntry::Directory { mark }).collect()
}

// Row 1 of the table: a file or directory, and an entry of that kind.

#[test]
fn a_directory_at_a_directorys_row_is_that_item_whatever_its_mark() {
    for row in [ONEDRIVE_DIRECTORY, OWN_DIRECTORY] {
        for entry in every_directory() {
            assert_eq!(decide_finding(row, entry), DiskFinding::NothingChanged, "{row:?} {entry:?}");
        }
    }
}

#[test]
fn a_file_at_the_row_of_a_file_on_disk_is_that_item_clean_or_changed_by_the_comparison_alone() {
    for row in [COPY, OWN_FILE] {
        for mark in MARKS {
            for wholeness in WHOLENESSES {
                assert_eq!(decide_finding(row, file(mark, wholeness, Same)), DiskFinding::NothingChanged, "{row:?} {mark:?} {wholeness:?}");
                assert_eq!(decide_finding(row, file(mark, wholeness, Different)), DiskFinding::ContentChanged, "{row:?} {mark:?} {wholeness:?}");
            }
        }
    }
}

/// The review's M1: bytes written through a handle that was never closed may keep the
/// file's size and its time. A row that still keeps the content of before the write is
/// never made clean by the comparison.
#[test]
fn a_file_a_writer_held_open_when_the_daemon_stopped_is_changed_here_whatever_the_comparison_says() {
    for binding in [Bound, Unbound] {
        for content_presence in [OnDisk, NotOnDisk] {
            for entry in every_file() {
                let row = PlaceRow::File { binding, content_presence, writer: Open };
                assert_eq!(decide_finding(row, entry), DiskFinding::ContentChanged, "{row:?} {entry:?}");
            }
        }
    }
}

/// Nothing of it on disk, or a directory at its name: as any other row, by its binding.
#[test]
fn a_file_a_writer_held_open_that_is_gone_from_disk_leaves_as_any_other_file_does() {
    let open = |binding| PlaceRow::File { binding, content_presence: OnDisk, writer: Open };
    assert_eq!(decide_finding(open(Bound), PlaceEntry::Nothing), DiskFinding::NotShownAnyMore);
    assert_eq!(decide_finding(open(Unbound), PlaceEntry::Nothing), DiskFinding::Removed);
    for entry in every_directory() {
        assert_eq!(decide_finding(open(Bound), entry), DiskFinding::NotShownAnyMore, "{entry:?}");
        assert_eq!(decide_finding(open(Unbound), entry), DiskFinding::Removed, "{entry:?}");
    }
}

// Row 2: a file whose content is not on disk, and a file marked as the whole copy of that item.

#[test]
fn the_whole_copy_of_a_file_that_is_not_on_disk_landed_before_its_row() {
    assert_eq!(decide_finding(NOT_DOWNLOADED, file(OfRowAtPlace, Whole, Same)), DiskFinding::CopyLandedBeforeRow);
}

// Row 3: a file whose content is not on disk, and any other file.

#[test]
fn any_other_file_at_a_file_that_is_not_on_disk_is_content_changed_here() {
    for entry in every_file() {
        if entry == file(OfRowAtPlace, Whole, Same) {
            continue;
        }
        assert_eq!(decide_finding(NOT_DOWNLOADED, entry), DiskFinding::ContentChanged, "{entry:?}");
    }
}

// Row 4: an item OneDrive has, and nothing or an entry of the other kind.

#[test]
fn a_copy_of_onedrive_that_is_gone_is_not_shown_any_more() {
    assert_eq!(decide_finding(COPY, PlaceEntry::Nothing), DiskFinding::NotShownAnyMore);
}

#[test]
fn an_item_of_onedrive_with_an_entry_of_the_other_kind_at_its_name_is_not_shown_any_more() {
    for entry in every_directory() {
        assert_eq!(decide_finding(COPY, entry), DiskFinding::NotShownAnyMore, "{entry:?}");
        assert_eq!(decide_finding(NOT_DOWNLOADED, entry), DiskFinding::NotShownAnyMore, "{entry:?}");
    }
    for entry in every_file() {
        assert_eq!(decide_finding(ONEDRIVE_DIRECTORY, entry), DiskFinding::NotShownAnyMore, "{entry:?}");
    }
}

/// The rows of these say that nothing of the item is on disk: nothing there is what they say.
#[test]
fn a_file_that_is_not_downloaded_and_a_directory_of_onedrive_have_nothing_on_disk_and_stay() {
    assert_eq!(decide_finding(NOT_DOWNLOADED, PlaceEntry::Nothing), DiskFinding::NothingChanged);
    assert_eq!(decide_finding(ONEDRIVE_DIRECTORY, PlaceEntry::Nothing), DiskFinding::NothingChanged);
}

// Row 5: an item OneDrive never had, and nothing or an entry of the other kind.

#[test]
fn an_item_onedrive_never_had_that_is_gone_or_has_the_other_kind_at_its_name_goes() {
    assert_eq!(decide_finding(OWN_FILE, PlaceEntry::Nothing), DiskFinding::Removed);
    assert_eq!(decide_finding(OWN_DIRECTORY, PlaceEntry::Nothing), DiskFinding::Removed);
    assert_eq!(decide_finding(PlaceRow::File { binding: Unbound, content_presence: NotOnDisk, writer: Closed }, PlaceEntry::Nothing), DiskFinding::Removed);
    for entry in every_directory() {
        assert_eq!(decide_finding(OWN_FILE, entry), DiskFinding::Removed, "{entry:?}");
    }
    for entry in every_file() {
        assert_eq!(decide_finding(OWN_DIRECTORY, entry), DiskFinding::Removed, "{entry:?}");
    }
}

// Row 6: nothing, and an entry marked with an item id that no row has.

#[test]
fn an_entry_no_row_has_marked_with_an_id_no_row_has_is_a_new_item_bound_to_it() {
    for wholeness in WHOLENESSES {
        for comparison in COMPARISONS {
            assert_eq!(decide_finding(PlaceRow::Nothing, file(OfNoRow, wholeness, comparison)), DiskFinding::MarkedWithoutRow);
        }
    }
    assert_eq!(decide_finding(PlaceRow::Nothing, PlaceEntry::Directory { mark: OfNoRow }), DiskFinding::MarkedWithoutRow);
}

// Row 7: nothing, and any other entry.

#[test]
fn any_other_entry_no_row_has_is_a_new_item() {
    for mark in [Unmarked, OfAnotherRow, OfRowAtPlace, OfSeveralEntries] {
        for wholeness in WHOLENESSES {
            for comparison in COMPARISONS {
                assert_eq!(decide_finding(PlaceRow::Nothing, file(mark, wholeness, comparison)), DiskFinding::Made, "{mark:?}");
            }
        }
        assert_eq!(decide_finding(PlaceRow::Nothing, PlaceEntry::Directory { mark }), DiskFinding::Made, "{mark:?}");
    }
}

#[test]
fn nothing_where_no_row_is_is_nothing() {
    assert_eq!(decide_finding(PlaceRow::Nothing, PlaceEntry::Nothing), DiskFinding::NothingChanged);
}

/// No row and no entry gives a finding that removes or moves an item of OneDrive: the only
/// findings of a bound row are these four.
#[test]
fn a_row_of_an_item_of_onedrive_is_never_found_removed() {
    let entries: Vec<PlaceEntry> = every_file().into_iter().chain(every_directory()).chain([PlaceEntry::Nothing]).collect();
    for row in [COPY, NOT_DOWNLOADED, ONEDRIVE_DIRECTORY] {
        for entry in &entries {
            let finding = decide_finding(row, *entry);
            assert!(matches!(finding, DiskFinding::NothingChanged | DiskFinding::ContentChanged | DiskFinding::CopyLandedBeforeRow | DiskFinding::NotShownAnyMore), "{row:?} {entry:?}: {finding:?}");
        }
        assert_eq!(decide_finding_without_place(row), DiskFinding::NotShownAnyMore);
    }
}

#[test]
fn a_row_whose_directory_leaves_the_folder_leaves_too_by_its_binding() {
    for (binding, finding) in [(Bound, DiskFinding::NotShownAnyMore), (Unbound, DiskFinding::Removed)] {
        for content_presence in [OnDisk, NotOnDisk] {
            for writer in [Closed, Open] {
                assert_eq!(decide_finding_without_place(PlaceRow::File { binding, content_presence, writer }), finding);
            }
        }
        assert_eq!(decide_finding_without_place(PlaceRow::Directory { binding }), finding);
    }
    assert_eq!(decide_finding_without_place(PlaceRow::Nothing), DiskFinding::NothingChanged);
}

// The comparison of a file's content.

fn hashed(size: u64, mtime: i64, ctag: Option<&str>) -> ContentVersion {
    ContentVersion::Hashed { size, mtime, mtime_nsec: 0, hash: Some("h".to_owned()), ctag: ctag.map(str::to_owned) }
}

fn compare_content_of_one_name(row: &ContentVersion, file: &DiskContent) -> ContentComparison {
    compare_content(row, file, EntryLinks::One)
}

/// Review S4: the cTag mark is the file's, and says nothing of the item of another name.
#[test]
fn the_ctag_mark_is_not_asked_of_a_file_that_has_several_names() {
    assert_eq!(compare_content(&hashed(3, 100, Some("c-1")), &on_disk(3, 100, 0, Some("c-0")), EntryLinks::Several), Same);
    assert_eq!(compare_content(&hashed(3, 100, Some("c-1")), &on_disk(4, 100, 0, Some("c-1")), EntryLinks::Several), Different);
}

/// Review S1: an id that more than one entry carries binds none of them.
#[test]
fn an_entry_whose_id_no_row_has_and_several_entries_carry_is_a_new_item_of_the_users_own() {
    assert_eq!(decide_finding(PlaceRow::Nothing, file(OfSeveralEntries, Whole, Same)), DiskFinding::Made);
    assert_eq!(decide_finding(PlaceRow::Nothing, PlaceEntry::Directory { mark: OfSeveralEntries }), DiskFinding::Made);
}

fn on_disk(size: u64, mtime: i64, mtime_nsec: u32, ctag: Option<&str>) -> DiskContent {
    DiskContent { size, mtime, mtime_nsec, ctag: ctag.map(str::to_owned) }
}

#[test]
fn a_file_of_the_rows_size_and_whole_seconds_is_the_same_content() {
    assert_eq!(compare_content_of_one_name(&hashed(3, 100, None), &on_disk(3, 100, 999, None)), Same);
    assert_eq!(compare_content_of_one_name(&hashed(3, 100, None), &on_disk(4, 100, 0, None)), Different);
    assert_eq!(compare_content_of_one_name(&hashed(3, 100, None), &on_disk(3, 101, 0, None)), Different);
}

#[test]
fn a_ctag_mark_that_is_not_the_rows_ctag_is_another_content_and_one_that_is_missing_says_nothing() {
    assert_eq!(compare_content_of_one_name(&hashed(3, 100, Some("c-1")), &on_disk(3, 100, 0, Some("c-1"))), Same);
    assert_eq!(compare_content_of_one_name(&hashed(3, 100, Some("c-1")), &on_disk(3, 100, 0, Some("c-0"))), Different);
    assert_eq!(compare_content_of_one_name(&hashed(3, 100, Some("c-1")), &on_disk(3, 100, 0, None)), Same);
    assert_eq!(compare_content_of_one_name(&hashed(3, 100, None), &on_disk(3, 100, 0, Some("c-0"))), Same);
}

#[test]
fn the_ctag_mark_is_not_asked_of_a_content_changed_here() {
    let changed = ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 0, change_count: 2 };
    assert_eq!(compare_content_of_one_name(&changed, &on_disk(3, 100, 0, Some("c-0"))), Same);
    assert_eq!(compare_content_of_one_name(&changed, &on_disk(5, 100, 0, Some("c-0"))), Different);
}

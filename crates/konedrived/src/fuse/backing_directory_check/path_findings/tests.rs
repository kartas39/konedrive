use std::path::Path;

use konedrive_tree::{ContentVersion, Item, ItemKind, LocalItemId};

use super::PathFindings;
use crate::folder::disk::SET_ASIDE_PREFIX;
use crate::fuse::backing_directory_check::backing_directory_contents::tests::{contents_of, index_of, Listed};
use crate::fuse::backing_directory_check::backing_directory_contents::BackingDirectoryContents;
use crate::fuse::backing_directory_check::disk_finding::{DiskFinding, UnresolvedReason};
use crate::fuse::backing_directory_check::entry_identities::EntryIdentities;
use crate::fuse::backing_directory_check::item_rows::tests::{bound, changed_here, drive, id, not_downloaded, own};
use crate::fuse::backing_directory_check::item_rows::ItemRows;

use DiskFinding::{ContentNoLongerOnDisk, Made, Moved, MovedOverAnotherItem, NothingChanged, PlaceLost, Removed};
use ItemKind::{Directory, File};

/// The findings of a backing directory that has `listed`, with these rows.
struct Decided {
    contents: BackingDirectoryContents,
    findings: PathFindings,
}

fn decide(rows: Vec<Item>, listed: &[(&str, Listed)]) -> Decided {
    let (rows, contents) = (ItemRows::new(rows), contents_of(listed));
    let identities = EntryIdentities::settle(&rows, &contents, &drive("d1"));
    let findings = PathFindings::decide(&rows, &contents, &identities);
    Decided { contents, findings }
}

impl Decided {
    /// The finding of the entry at `path`.
    fn entry(&self, path: &str) -> Result<DiskFinding, UnresolvedReason> {
        self.findings.entry(index_of(&self.contents, path)).finding.clone()
    }

    /// The rows no entry is taken as, each with its finding, by id.
    fn rows(&self) -> Vec<(i64, DiskFinding)> {
        let mut rows: Vec<(i64, DiskFinding)> = self.findings.list_rows().iter().map(|(item, finding)| (item.as_i64(), *finding)).collect();
        rows.sort();
        rows
    }

    fn held_back(&self) -> Vec<LocalItemId> {
        let mut held_back = self.findings.list_held_back().to_vec();
        held_back.sort();
        held_back
    }
}

/// A row whose content the rows took from the cTag mark of its copy: nothing is agreed on.
fn bound_by_its_mark(mut item: Item) -> Item {
    item.local.as_mut().unwrap().content = Some(ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: None, ctag: Some("c-Z".to_owned()) });
    Item { synced_place: None, synced_content: None, onedrive: None, ..item }
}

#[test]
fn each_row_no_entry_is_taken_as_is_decided_by_itself_a_directory_before_what_is_in_it() {
    let rows = vec![bound("D", 2, 1, "docs", Directory), bound("A", 3, 2, "a.txt", File), changed_here(bound("B", 4, 2, "b.txt", File)), not_downloaded(bound("R", 5, 2, "r.bin", File)), own(6, 1, "top.txt", File)];
    let decided = decide(rows, &[]);
    assert_eq!(decided.rows(), [(2, NothingChanged), (3, ContentNoLongerOnDisk), (4, Removed), (5, NothingChanged), (6, Removed)]);
    assert_eq!(decided.findings.list_rows()[0].0, id(2), "the directory first");
    assert!(decided.findings.is_read_whole() && decided.held_back().is_empty());
}

/// A file marked with an id and not whole holds nothing of that file.
#[test]
fn a_file_that_is_not_whole_is_no_copy_and_one_of_an_id_no_row_has_is_left() {
    let rows = vec![bound("A", 2, 1, "a.txt", File), not_downloaded(bound("R", 3, 1, "r.bin", File))];
    let decided = decide(rows, &[("a.txt", Listed::PartialFile("A")), ("r.bin", Listed::PartialFile("R")), ("p.bin", Listed::PartialFile("P"))]);
    assert_eq!((decided.entry("a.txt"), decided.entry("r.bin"), decided.entry("p.bin")), (Ok(ContentNoLongerOnDisk), Ok(NothingChanged), Err(UnresolvedReason::PartialCopy)));
    assert!(decided.rows().is_empty() && decided.findings.is_read_whole());
}

/// Review M4. The store was lost, the copy was bound by its mark, and it is gone before a
/// listing agreed on it: a copy not known to be changed here is "not on disk", never removed.
/// A file edited through the mount has a content a program changed here, whatever was
/// agreed on: gone, it was removed by hand.
#[test]
fn a_copy_whose_content_the_rows_took_from_its_mark_is_a_clean_copy_when_it_is_gone() {
    let decided = decide(vec![bound_by_its_mark(bound("Z", 2, 1, "lost.txt", File)), changed_here(bound("B", 3, 1, "b.txt", File))], &[]);
    assert_eq!(decided.rows(), [(2, ContentNoLongerOnDisk), (3, Removed)]);
}

/// Review M2 a). `new` is the user's own; r.bin of OneDrive, not downloaded, was moved into
/// it through the mount. `mv new new2`, or `rmdir new`: the row of `new` goes, and what the
/// person could not see in it is not removed with it. What they could see is.
#[test]
fn what_is_in_a_directory_whose_row_goes_is_removed_only_when_the_person_had_it_before_them() {
    let rows = vec![
        own(2, 1, "new", Directory),
        not_downloaded(bound("R", 3, 2, "r.bin", File)),
        bound("C", 4, 2, "clean.txt", File),
        changed_here(bound("E", 5, 2, "edited.txt", File)),
        own(6, 2, "own.txt", File),
        bound("S", 7, 2, "sub", Directory),
        not_downloaded(bound("R2", 8, 7, "r2.bin", File)),
        own(9, 7, "own", Directory),
    ];
    let decided = decide(rows.clone(), &[("new2", Listed::Directory(None))]);
    assert_eq!(decided.entry("new2"), Ok(Made));
    let expected = [(2, Removed), (3, PlaceLost), (4, PlaceLost), (5, Removed), (6, Removed), (7, PlaceLost), (8, PlaceLost), (9, Removed)];
    assert_eq!(decided.rows(), expected);
    assert_eq!(decide(rows, &[]).rows(), expected, "rmdir");
}

/// What the rows say is in a directory that goes and that is found at another path is moved
/// out, and what is in it is decided where it is.
#[test]
fn what_is_found_at_another_path_leaves_a_directory_whose_row_goes() {
    let rows = vec![own(2, 1, "mine", Directory), bound("A", 3, 2, "a.txt", File), bound("D", 4, 2, "docs", Directory), bound("B", 5, 4, "b.txt", File), not_downloaded(bound("R", 6, 4, "r.bin", File))];
    let decided = decide(rows, &[("a.txt", Listed::File(Some("A"))), ("docs", Listed::Directory(Some("D"))), ("docs/b.txt", Listed::File(Some("B")))]);
    assert_eq!((decided.entry("a.txt"), decided.entry("docs"), decided.entry("docs/b.txt")), (Ok(Moved), Ok(Moved), Ok(NothingChanged)));
    assert_eq!(decided.rows(), [(2, Removed), (6, NothingChanged)]);
}

/// Review M2 b). `Notes` of OneDrive has no directory on disk, nor has what is in it: the
/// folder looks empty, and a file `Notes` is saved. Nothing the person did not see is
/// removed.
#[test]
fn a_file_at_the_name_of_a_directory_of_onedrive_that_has_none_on_disk_removes_nothing() {
    let rows = vec![bound("N", 2, 1, "Notes", Directory), bound("NS", 3, 2, "sub", Directory), not_downloaded(bound("R1", 4, 2, "r1.bin", File)), not_downloaded(bound("R2", 5, 3, "r2.bin", File))];
    let decided = decide(rows, &[("Notes", Listed::File(None))]);
    assert_eq!(decided.entry("Notes"), Ok(Made));
    assert_eq!(decided.rows(), [(2, PlaceLost), (3, PlaceLost), (4, PlaceLost), (5, PlaceLost)]);
}

/// Review M2 c), and its reverse for a file whose copy the person had before them.
#[test]
fn a_directory_at_the_name_of_a_file_removes_it_only_when_its_copy_was_on_disk() {
    let rows = vec![not_downloaded(bound("R", 2, 1, "report", File)), bound("C", 3, 1, "clean", File), own(4, 1, "own", File)];
    let decided = decide(rows, &[("report", Listed::Directory(None)), ("clean", Listed::Directory(None)), ("own", Listed::Directory(None))]);
    assert_eq!((decided.entry("report"), decided.entry("clean"), decided.entry("own")), (Ok(Made), Ok(Made), Ok(Made)));
    assert_eq!(decided.rows(), [(2, PlaceLost), (3, Removed), (4, Removed)]);
}

/// The collision by a moved marked entry: the item moved there is there, and what the row
/// had there is removed only when the person had its copy before them.
#[test]
fn an_item_moved_over_the_name_of_another_removes_it_only_when_its_copy_was_on_disk() {
    let rows = vec![bound("A", 2, 1, "a.txt", File), not_downloaded(bound("R", 3, 1, "r.bin", File)), bound("B", 4, 1, "b.txt", File), bound("C", 5, 1, "c.txt", File)];
    let decided = decide(rows, &[("r.bin", Listed::File(Some("A"))), ("c.txt", Listed::File(Some("B")))]);
    assert_eq!((decided.entry("r.bin"), decided.entry("c.txt")), (Ok(MovedOverAnotherItem), Ok(MovedOverAnotherItem)));
    assert_eq!(decided.rows(), [(3, PlaceLost), (5, Removed)]);
}

/// Review M1 a), b). A directory could not be read: what is gone from where its row says
/// may be in it. Nothing is found removed and no row loses its local version; the rows are
/// left and said, with what is below them. A content that is not on disk any more is said
/// so all the same: the row keeps its local version.
#[test]
fn nothing_is_found_removed_while_a_directory_was_not_read() {
    let rows = vec![
        own(2, 1, "top.txt", File),
        bound("D", 3, 1, "docs", Directory),
        changed_here(bound("B", 4, 3, "b.txt", File)),
        own(5, 1, "box", Directory),
        bound("C", 6, 5, "c.txt", File),
        bound("A", 7, 3, "a.txt", File),
        not_downloaded(bound("R", 8, 1, "report", File)),
    ];
    let decided = decide(rows.clone(), &[("docs", Listed::Directory(Some("D"))), ("secret", Listed::UnreadableDirectory(None)), ("report", Listed::Directory(None))]);
    assert!(!decided.findings.is_read_whole());
    assert_eq!(decided.rows(), [(7, ContentNoLongerOnDisk)]);
    assert_eq!(decided.held_back(), [id(2), id(4), id(5), id(8)], "the directory is said, not each row in it");
    // The same rows once everything is read.
    let decided = decide(rows, &[("docs", Listed::Directory(Some("D"))), ("secret", Listed::Directory(None)), ("report", Listed::Directory(None))]);
    assert!(decided.findings.is_read_whole() && decided.held_back().is_empty());
    assert_eq!(decided.rows(), [(2, Removed), (4, Removed), (5, Removed), (6, PlaceLost), (7, ContentNoLongerOnDisk), (8, PlaceLost)]);
}

/// Review M1 c). `mv docs docs2 ; chmod 000 docs2`: the marks of a directory that cannot be
/// read cannot be read either. It is not taken for a new directory: it is left, and so is
/// what is gone from `docs`.
#[test]
fn a_directory_that_could_not_be_read_and_that_no_row_is_found_for_is_left_and_not_made() {
    let rows = vec![bound("D", 2, 1, "docs", Directory), changed_here(bound("B", 3, 2, "b.txt", File))];
    let decided = decide(rows, &[("docs2", Listed::UnreadableDirectory(None))]);
    assert!(matches!(decided.entry("docs2"), Err(UnresolvedReason::Unreadable(_))), "{:?}", decided.entry("docs2"));
    assert_eq!((decided.rows(), decided.held_back()), (vec![(2, NothingChanged)], vec![id(3)]));
}

/// A name that is neither a file nor a directory hides what the rows say is below it when
/// it stands where a directory's row is: the rows there are left, and the backing directory
/// is not read whole. Where a file's row is, or no row, it hides nothing.
#[test]
fn a_name_that_is_no_file_and_no_directory_hides_what_is_below_the_row_of_a_directory() {
    let rows = || vec![bound("D", 2, 1, "docs", Directory), changed_here(bound("B", 3, 2, "b.txt", File)), own(4, 1, "top.txt", File), own(5, 1, "gone.txt", File)];
    let decided = decide(rows(), &[("docs", Listed::Special)]);
    assert!(!decided.findings.is_read_whole());
    assert_eq!((decided.rows(), decided.held_back()), (vec![], vec![id(4), id(5)]), "what is below it is left with it");

    let decided = decide(rows(), &[("docs", Listed::Directory(Some("D"))), ("docs/b.txt", Listed::File(Some("B"))), ("top.txt", Listed::Special), ("link", Listed::Special)]);
    assert!(decided.findings.is_read_whole());
    assert_eq!((decided.rows(), decided.held_back()), (vec![(5, Removed)], vec![]), "the row at the name is left as it is");
}

/// A row at a name the check sets items aside to was left there by a check that died, or
/// that could not move the item on. No entry has such a name: when no entry is taken as the
/// item, its row has no place to stay at.
#[test]
fn a_row_left_at_a_name_the_check_set_it_aside_to_does_not_stay_there() {
    let aside = |number: i64| format!("{SET_ASIDE_PREFIX}{number}");
    let rows = vec![
        bound("A", 2, 1, &aside(2), File),
        not_downloaded(bound("R", 3, 1, &aside(3), File)),
        bound("D", 4, 1, &aside(4), Directory),
        not_downloaded(bound("R2", 5, 4, "r2.bin", File)),
        changed_here(bound("E", 6, 1, &aside(6), File)),
        bound("M", 7, 1, &aside(7), File),
    ];
    let decided = decide(rows, &[("found.txt", Listed::File(Some("M")))]);
    assert_eq!(decided.entry("found.txt"), Ok(Moved), "an item that is found is moved on from there");
    assert_eq!(decided.rows(), [(2, PlaceLost), (3, PlaceLost), (4, PlaceLost), (5, PlaceLost), (6, Removed)]);
    assert!(!decided.contents.resolve_path(0).starts_with(Path::new(SET_ASIDE_PREFIX)));
}

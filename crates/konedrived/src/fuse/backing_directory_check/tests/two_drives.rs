//! One OneDrive item id in rows of two drives. A mark names an item id and no drive, and the
//! check does not guess one: an entry is the row at its own place when that row has the
//! mark's id, and an entry that carries such an id anywhere else is left as it is, with
//! every row and entry whose finding would depend on which of the two it is. The sequences
//! are the second review's M1 and M2.

use std::fs;

use konedrive_tree::{ContentPresence, ItemKind};

use super::{changed_here, content, folder, found, reference, DiskFinding, DiskFindings, Folder, UnresolvedReason};

/// `shared` is another drive's; l.bin is d1!X and shared/h.txt is d2!X, both clean copies.
fn folder_of_twins() -> Folder {
    let mut f = folder();
    f.directory_of_drive("shared", "d2", "S");
    f.copy("l.bin", b"lll", "X");
    f.file_of_drive("shared/h.txt", "d2", "X", true);
    f
}

/// M1. l.bin is saved by an editor that writes a new file and renames it over: no marks. The
/// file of the other drive is where its row says, so nothing depends on which is which.
#[test]
fn a_file_whose_item_id_is_in_two_drives_saved_by_rename_has_its_content_changed_and_is_not_removed() {
    let mut f = folder_of_twins();
    let l = f.id("l.bin");
    let h = f.row("shared/h.txt").unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 3)]);
    f.write("l.bin", b"edited");
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2), (DiskFinding::ContentChanged, 1)]);
    assert_eq!(findings.unresolved(), []);
    let row = f.row("l.bin").unwrap();
    assert_eq!((row.local_item_id, row.onedrive_item.clone(), content(&row)), (l, Some(reference("d1", "X")), changed_here(6, 1)));
    assert_eq!(f.item(h.local_item_id), h, "the file of the other drive is as it was");
    f.assert_settled();
}

/// M1, the marks of l.bin taken off and no edit: the entry is the row's own all the same.
#[test]
fn a_file_whose_item_id_is_in_two_drives_and_whose_marks_were_taken_off_keeps_its_row() {
    let mut f = folder_of_twins();
    let l = f.id("l.bin");
    f.write("l.bin", b"lll");
    let findings = f.check();
    assert_eq!(findings.count(DiskFinding::Removed) + findings.count(DiskFinding::Made), 0, "{:?}", found(&findings));
    assert_eq!((f.id("l.bin"), f.item(l).onedrive_item), (l, Some(reference("d1", "X"))));
    f.assert_settled();
}

/// M2, with no row of d1!X at all: shared/x.bin is d2!X, not downloaded. The whole copy of
/// d1!X comes to be in the root, put back by hand from a backup after its removal was sent.
/// A file whose content was not on disk had nothing there to be moved by hand: the copy is
/// not that file moved, and is a new item.
#[test]
fn a_marked_file_is_not_taken_as_a_file_that_was_not_on_disk_moved() {
    let mut f = folder();
    f.directory_of_drive("shared", "d2", "S");
    let x = f.file_of_drive("shared/x.bin", "d2", "X", false);
    let before = f.item(x);
    f.write("a.txt", b"the bytes of another item");
    f.mark_copy("a.txt", "X");
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2), (DiskFinding::MadeWithStaleMark, 1)]);
    assert_eq!(f.item(x), before, "not moved out of its folder, and not given the content");
    let made = f.row("a.txt").unwrap();
    assert_eq!((made.kind, made.onedrive_item.clone(), f.mark_of("a.txt")), (ItemKind::File, None, None));
    f.assert_settled();
}

/// M2, with a row of d1!X that a listing made and the folder does not show yet: rows of two
/// drives have the id, and the mark does not say which is meant. Nothing is moved, bound or
/// given content; the entry is left as it is, its marks with it.
#[test]
fn a_marked_file_whose_item_id_is_in_rows_of_two_drives_is_left_as_it_is() {
    let mut f = folder();
    f.directory_of_drive("shared", "d2", "S");
    let x = f.file_of_drive("shared/x.bin", "d2", "X", false);
    let listed = f.store.item_store().make_onedrive_item(&reference("d1", "X"), ItemKind::File).unwrap();
    f.write("a.txt", b"the bytes of another item");
    f.mark_copy("a.txt", "X");
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2)]);
    assert_eq!(f.rows(), before, "no row is written");
    assert_eq!((f.item(x).content_presence, f.item(listed).local, f.mark_of("a.txt")), (ContentPresence::NotOnDisk, None, Some("X".to_owned())));
    assert_eq!(findings.unresolved().iter().map(|left| left.path.to_str().unwrap()).collect::<Vec<_>>(), ["a.txt"]);
}

/// A file of an id in two drives moved by hand, and the other's saved by rename: the moved
/// one may be either item, so neither it nor the unmarked file at a row's place is decided,
/// and neither row is removed, moved or given content.
#[test]
fn a_file_whose_item_id_is_in_two_drives_moved_by_hand_is_left_with_all_that_depends_on_which_it_is() {
    let mut f = folder_of_twins();
    f.rename("shared/h.txt", "shared/h2.txt");
    f.write("l.bin", b"edited");
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 1)]);
    assert_eq!(f.rows(), before, "no row is written");
    assert_eq!(f.mark_of("shared/h2.txt"), Some("X".to_owned()));
    assert_eq!(findings.unresolved().iter().map(|left| left.path.to_str().unwrap()).collect::<Vec<_>>(), ["l.bin", "shared/h.txt", "shared/h2.txt"]);
}

/// A store that was lost, and a copy made by hand into a folder of another drive: one entry
/// is bound to the id no row has, and the second that carries it is a copy. The check makes
/// no second row of one item id.
#[test]
fn of_two_entries_with_an_id_no_row_has_one_is_bound_and_the_check_makes_no_id_of_two_drives() {
    let mut f = folder();
    f.directory_of_drive("shared", "d2", "S");
    f.write("a.txt", b"zzz");
    f.mark_copy("a.txt", "Z");
    f.write("shared/a.txt", b"zzz");
    f.mark_copy("shared/a.txt", "Z");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 1), (DiskFinding::MadeWithStaleMark, 1), (DiskFinding::MarkedWithoutRow, 1)]);
    assert_eq!((f.row("a.txt").unwrap().onedrive_item, f.row("shared/a.txt").unwrap().onedrive_item), (Some(reference("d1", "Z")), None));
    f.assert_settled();
}

fn left(findings: &DiskFindings) -> Vec<(&str, UnresolvedReason)> {
    findings.unresolved().iter().map(|left| (left.path.to_str().unwrap(), left.reason.clone())).collect()
}

/// A directory does not leave the folder while a row the rows say is below it is left as
/// it is: it is left with it, whole. `rm -r box`, where the file of d1!X was, while a copy
/// marked X stands elsewhere: nothing that was in `box` is removed or loses its place
/// before it can be said which item the copy is.
#[test]
fn a_directory_that_is_gone_stays_whole_while_a_row_below_it_is_left_for_its_item_id() {
    let mut f = folder_of_twins();
    let (box_directory, l) = (f.own_directory("box"), f.id("l.bin"));
    f.move_through_the_mount("l.bin", "box/l.bin");
    f.own_file("box/own.txt", b"own");
    let clean = f.copy("c.txt", b"ccc", "C");
    f.move_through_the_mount("c.txt", "box/c.txt");
    fs::remove_dir_all(f.disk("box")).unwrap();
    f.write("stray.bin", b"which of the two");
    f.mark_copy("stray.bin", "X");
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2)]);
    assert_eq!(f.rows(), before, "no row is written");
    assert_eq!(left(&findings), [("box", UnresolvedReason::ItemIdInTwoDrives), ("stray.bin", UnresolvedReason::ItemIdInTwoDrives)]);
    assert_eq!(findings.list_held_items(), [box_directory], "the directory, and with it what the rows say is in it");

    // The copy taken away again: the directory was removed by hand, and what OneDrive has
    // of what was in it is not removed.
    fs::remove_file(f.disk("stray.bin")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 2), (DiskFinding::Removed, 2), (DiskFinding::PlaceLost, 2)]);
    assert_eq!((f.item(l).local, f.item(clean).local, f.item(l).synced_place), (None, None, None));
    f.assert_settled();
}

/// A directory made at the name of a row that is left cannot have its row, and an item
/// moved into it cannot be moved in the rows: its row stays where it was, where the disk
/// has nothing, and that path is said too. The rows that are not as the disk has them are
/// held.
#[test]
fn the_row_of_an_item_moved_into_a_directory_that_waits_is_said_where_it_is_and_is_held() {
    let mut f = folder_of_twins();
    let (l, a) = (f.id("l.bin"), f.copy("a.txt", b"aaa", "A"));
    fs::remove_file(f.disk("l.bin")).unwrap();
    f.make_directory("l.bin");
    f.rename("a.txt", "l.bin/a.txt");
    f.write("stray.bin", b"which of the two");
    f.mark_copy("stray.bin", "X");
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2)]);
    assert_eq!(f.rows(), before, "no row is written");
    assert_eq!(
        left(&findings),
        [("a.txt", UnresolvedReason::Blocked), ("l.bin", UnresolvedReason::Blocked), ("l.bin", UnresolvedReason::ItemIdInTwoDrives), ("stray.bin", UnresolvedReason::ItemIdInTwoDrives)]
    );
    assert_eq!(findings.list_held_items(), [l, a]);

    // The copy taken away again: a directory stands where the clean copy was, which is
    // removed here, and the file is found in it.
    fs::remove_file(f.disk("stray.bin")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 2), (DiskFinding::Made, 1), (DiskFinding::Moved, 1), (DiskFinding::Removed, 1)]);
    assert_eq!(f.id("l.bin/a.txt"), a);
    f.assert_settled();
}

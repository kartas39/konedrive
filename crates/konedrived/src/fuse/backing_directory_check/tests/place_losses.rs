//! What the person could not see is not removed by the check: an item of OneDrive with
//! nothing of its own on disk whose place another entry took, or whose directory's row
//! goes, loses its place and stays OneDrive's. The sequences are the review's M2 a), b), c),
//! M3 a), b) and M4, and the row a check left at a name of its own.

use std::fs;

use konedrive_tree::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange};

use super::{content, folder, folder_of_each, found, reference, DiskFinding, Folder, ROOT, MTIME};
use crate::folder::disk::{daemon_owned, SET_ASIDE_PREFIX};

/// The row of an item OneDrive has and the folder does not show, with nothing unsent: no
/// local version, nothing agreed on, the binding kept.
fn assert_is_onedrives_alone(f: &mut Folder, item: LocalItemId, onedrive_id: &str) {
    let row = f.item(item);
    assert_eq!((row.local.clone(), row.synced_place.clone(), row.synced_content.clone()), (None, None, None), "{row:?}");
    assert_eq!((row.onedrive_item.clone(), row.content_presence, row.unsent_since), (Some(reference("d1", onedrive_id)), ContentPresence::NotOnDisk, None), "{row:?}");
}

/// A file of OneDrive, not downloaded, moved through the mount into `directory`.
fn not_downloaded_in(f: &mut Folder, directory: &str, name: &str, onedrive_id: &str) -> LocalItemId {
    let item = f.not_downloaded(name, 9, onedrive_id);
    let place = ItemPlace { parent_local_item_id: f.id(directory), name: name.into() };
    f.store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored: false }).unwrap();
    item
}

/// M2 a): `new` is the user's own, not sent yet; r.bin of OneDrive, not downloaded, was
/// moved into it through the mount. `mv new new2`.
#[test]
fn a_directory_of_the_users_own_renamed_by_hand_does_not_remove_the_file_of_onedrive_that_was_not_on_disk_in_it() {
    let mut f = folder();
    f.own_directory("new");
    let remote = not_downloaded_in(&mut f, "new", "r.bin", "R");
    f.rename("new", "new2");
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 1), (DiskFinding::Removed, 1), (DiskFinding::PlaceLost, 1)]);
    assert_is_onedrives_alone(&mut f, remote, "R");
    assert!(f.row("new").is_none() && f.row("new2").is_some());
    f.assert_settled();
}

/// The same with `rmdir new`, which looks empty.
#[test]
fn a_directory_of_the_users_own_removed_by_hand_does_not_remove_the_file_of_onedrive_that_was_not_on_disk_in_it() {
    let mut f = folder();
    f.own_directory("new");
    let remote = not_downloaded_in(&mut f, "new", "r.bin", "R");
    fs::remove_dir(f.disk("new")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::Removed, 1), (DiskFinding::PlaceLost, 1)]);
    assert_is_onedrives_alone(&mut f, remote, "R");
    f.assert_settled();
}

/// M2 b): Notes/ and Notes/sub/ of OneDrive with Notes/r1.bin and Notes/sub/r2.bin not
/// downloaded, nothing of them on disk. A file `Notes` is saved.
#[test]
fn a_file_saved_at_the_name_of_a_directory_of_onedrive_that_has_none_on_disk_removes_nothing_of_it() {
    let mut f = folder();
    let notes = f.onedrive_directory("Notes", "N");
    let sub = f.onedrive_directory("Notes/sub", "NS");
    let r1 = f.not_downloaded("Notes/r1.bin", 9, "R1");
    let r2 = f.not_downloaded("Notes/sub/r2.bin", 9, "R2");
    fs::remove_dir_all(f.disk("Notes")).unwrap();
    f.assert_settled();
    f.write("Notes", b"my notes");
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 1), (DiskFinding::PlaceLost, 4)]);
    for (item, onedrive_id) in [(notes, "N"), (sub, "NS"), (r1, "R1"), (r2, "R2")] {
        assert_is_onedrives_alone(&mut f, item, onedrive_id);
    }
    let made = f.row("Notes").unwrap();
    assert_eq!((made.kind, made.onedrive_item), (ItemKind::File, None));
    f.assert_settled();
}

/// M2 c): `mkdir report` where `report` is a file that is not downloaded.
#[test]
fn a_directory_made_at_the_name_of_a_file_that_is_not_downloaded_does_not_remove_it() {
    let mut f = folder();
    let report = f.not_downloaded("report", 9, "R");
    f.make_directory("report");
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 1), (DiskFinding::PlaceLost, 1)]);
    assert_is_onedrives_alone(&mut f, report, "R");
    f.assert_settled();
}

/// What the person had before them is removed as before: a directory made where the clean
/// copy of a file was, and where a file of their own was.
#[test]
fn a_directory_made_at_the_name_of_a_file_whose_copy_was_on_disk_removes_it() {
    let mut f = folder_of_each();
    let clean = f.id("docs/a.txt");
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    f.make_directory("docs/a.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 1), (DiskFinding::Removed, 1)]);
    let removed = f.item(clean);
    assert!(removed.local.is_none() && removed.synced_place.is_some(), "its removal is to be sent");
    f.assert_settled();
}

/// M3 a): l.bin is d1!X, not downloaded; h.txt is d2!X, a clean copy marked X. Nothing was
/// touched by hand.
#[test]
fn one_item_id_in_two_drives_with_nothing_touched_by_hand_has_nothing_changed() {
    let mut f = folder();
    f.not_downloaded("l.bin", 9, "X");
    let h = f.own_file("h.txt", b"hhh");
    f.mark_copy("h.txt", "X");
    f.agree_in(h, "d2", "X");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 2)]);
    assert_eq!(f.rows(), before);
    f.assert_settled();
}

/// M3 b): both on disk where their rows say, both marked X.
#[test]
fn one_item_id_in_two_drives_with_both_copies_on_disk_has_nothing_changed() {
    let mut f = folder();
    f.copy("l.bin", b"lll", "X");
    let h = f.own_file("h.txt", b"hhh");
    f.mark_copy("h.txt", "X");
    f.agree_in(h, "d2", "X");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 2)]);
    assert_eq!((f.rows(), f.mark_of("h.txt")), (before, Some("X".to_owned())), "no mark is taken off");
    f.assert_settled();
}

/// M4: the store is lost; a start binds lost.txt by its mark; the daemon stops before a
/// listing agreed on it; lost.txt is gone; start.
#[test]
fn a_copy_bound_by_its_mark_that_is_gone_before_a_listing_agreed_on_it_is_not_on_disk_and_not_removed() {
    let mut f = folder();
    f.write("lost.txt", b"zzz");
    f.mark_copy("lost.txt", "Z");
    assert_eq!(found(&f.check()), [(DiskFinding::MarkedWithoutRow, 1)]);
    let bound = f.row("lost.txt").unwrap();
    assert_eq!(content(&bound), Some(ContentVersion::Hashed { size: 3, mtime: MTIME, mtime_nsec: 0, hash: None, ctag: Some("c-Z".to_owned()) }));
    fs::remove_file(f.disk("lost.txt")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::ContentNoLongerOnDisk, 1)]);
    assert_eq!(f.row("lost.txt"), Some(Item { content_presence: ContentPresence::NotOnDisk, ..bound }), "the folder still shows it");
    f.assert_settled();
}

/// The name an item's row is set aside to is one of the daemon's own: no entry of the
/// backing directory is walked under it, and nothing is made there through the mount.
#[test]
fn the_name_an_item_is_set_aside_to_is_one_of_the_daemons_own() {
    assert!(daemon_owned(format!("{SET_ASIDE_PREFIX}7").as_ref()));
    let mut f = folder_of_each();
    f.write(format!("{SET_ASIDE_PREFIX}7"), b"not the folder's");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!(f.rows(), before);
}

/// A check that died, or that could not move an item on, left its row at a name of the
/// check's own. A start that finds the item by its mark moves it on from there; one that
/// does not find it takes the row from the folder, since no entry has that name: an item
/// of OneDrive is OneDrive's alone again and is not removed.
#[test]
fn a_start_that_finds_a_row_set_aside_moves_the_item_on_or_takes_the_row_from_the_folder() {
    let mut f = folder_of_each();
    let (found_again, gone) = (f.id("docs/a.txt"), f.copy("docs/gone.txt", b"ggg", "G"));
    for item in [found_again, gone] {
        let place = ItemPlace { parent_local_item_id: ROOT, name: format!("{SET_ASIDE_PREFIX}{item}").into() };
        f.store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored: false }).unwrap();
    }
    f.rename("docs/a.txt", "mine/a.txt");
    fs::remove_file(f.disk("docs/gone.txt")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 1), (DiskFinding::PlaceLost, 1)]);
    assert_eq!(f.id("mine/a.txt"), found_again);
    assert_is_onedrives_alone(&mut f, gone, "G");
    assert!(f.rows().iter().all(|row| row.local.as_ref().and_then(|local| local.place.as_ref()).is_none_or(|place| !daemon_owned(&place.name))), "no row is at a name of the check's own");
    f.assert_settled();
}

/// The second review's N10. A content hashed here that the two sides have not agreed on,
/// and its file then removed by hand: the content was here alone, so the removal is the
/// user's own. The row does not stay as content to send that no file holds.
#[test]
fn a_file_whose_content_was_hashed_here_and_not_agreed_on_that_is_gone_is_removed_here() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.store.bench_sql(&format!("UPDATE items SET local_hash = 'h-new', local_ctag = NULL WHERE local_item_id = {a}")).unwrap();
    assert!(matches!(content(&f.item(a)), Some(ContentVersion::Hashed { hash: Some(hash), .. }) if hash == "h-new"));
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Removed, 1)]);
    let removed = f.item(a);
    assert_eq!((removed.local.clone(), removed.onedrive_item.clone()), (None, Some(reference("d1", "A"))));
    assert!(removed.synced_place.is_some(), "the removal is to be sent");
    f.assert_settled();
}

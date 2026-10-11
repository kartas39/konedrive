use std::fs;
use std::os::unix::fs::PermissionsExt;

use crate::fuse::item_fixture::{DiskMarksRead, ItemFixture};

#[test]
fn a_check_that_left_nothing_holds_nothing_back() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let copy = fixture.show_onedrive_file("docs/copy.txt", b"copy");
    fixture.land_copy(copy.local_item_id, b"copy");
    fixture.show_onedrive_file("not-on-disk.txt", b"elsewhere");
    fixture.make_own_file("own.txt", b"own");
    let rows = fixture.list_items();

    let findings = fixture.run_start_check();

    assert!(findings.held_items.is_empty(), "nothing is held: {findings:?}");
    assert!(!findings.has_unread_part);
    assert_eq!((findings.unresolved_path_count, findings.bound_by_mark_count), (0, 0));
    assert_eq!(fixture.list_items(), rows, "the check wrote nothing");
}

#[test]
fn the_rows_a_check_left_are_answered_as_items() {
    let fixture = ItemFixture::new();
    let locked = fixture.make_own_directory("locked");
    fixture.make_own_file("locked/inside.txt", b"inside");
    let free = fixture.make_own_file("free.txt", b"free");
    fs::set_permissions(fixture.disk_path("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let findings = fixture.run_start_check();
    fs::set_permissions(fixture.disk_path("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(findings.held_items.contains(&locked.local_item_id), "the row of the directory that could not be read is held: {findings:?}");
    assert!(!findings.held_items.contains(&free.local_item_id), "a row beside it is not");
    assert!(findings.has_unread_part);
    assert!(findings.unresolved_path_count >= 1);
    assert!(fixture.find_item_at("locked/inside.txt").is_some(), "no row below what was not read is changed");
}

#[test]
fn a_store_that_was_made_empty_has_its_marked_entries_bound_before_the_first_listing() {
    let fixture = ItemFixture::new();
    fixture.write_disk_file("kept.txt", b"kept");
    fixture.write_disk_marks("kept.txt", &DiskMarksRead { onedrive_item_id: Some("KEPT".into()), is_whole: true, ctag: Some("c-kept".into()) });
    fixture.write_disk_file("new.txt", b"new");

    let findings = fixture.run_start_check();

    assert_eq!(findings.bound_by_mark_count, 1, "{findings:?}");
    assert!(findings.held_items.is_empty());
    let kept = fixture.find_item_at("kept.txt").expect("the marked entry has a row");
    assert_eq!(kept.onedrive_item.map(|reference| reference.item_id.as_str().to_owned()).as_deref(), Some("KEPT"), "bound to the id of its mark");
    assert_eq!(fixture.find_item_at("new.txt").expect("the unmarked entry has a row").onedrive_item, None);
}

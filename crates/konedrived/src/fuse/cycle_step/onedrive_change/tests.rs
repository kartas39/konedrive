use konedrive_tree::{Kind, Row};

use super::OneDriveChange;
use crate::fuse::cycle_step::tests::placed;

fn id() -> String {
    "A".to_owned()
}

#[test]
fn an_id_placed_only_in_the_new_tree_appeared() {
    let change = OneDriveChange::of(id(), None, placed(Kind::File, "docs/a.txt"));
    assert!(matches!(change, Some(OneDriveChange::Appeared { id, now, .. }) if id == "A" && now.to_str() == Some("docs/a.txt")));
}

#[test]
fn an_id_placed_only_in_the_store_vanished() {
    let change = OneDriveChange::of(id(), placed(Kind::Folder, "docs/old"), None);
    assert!(matches!(change, Some(OneDriveChange::Vanished { was, old, .. }) if was.to_str() == Some("docs/old") && old.kind == Kind::Folder));
}

#[test]
fn an_id_placed_at_another_path_moved() {
    let change = OneDriveChange::of(id(), placed(Kind::File, "a.txt"), placed(Kind::File, "docs/a.txt"));
    assert!(matches!(change, Some(OneDriveChange::Moved { was, now, .. }) if was.to_str() == Some("a.txt") && now.to_str() == Some("docs/a.txt")));
}

#[test]
fn an_id_placed_at_the_same_path_stayed() {
    let change = OneDriveChange::of(id(), placed(Kind::File, "a.txt"), placed(Kind::File, "a.txt"));
    assert!(matches!(change, Some(OneDriveChange::Stayed { at, .. }) if at.to_str() == Some("a.txt")));
}

/// What the store never had placed and the new tree has not placed either is no change of
/// the folder.
#[test]
fn an_id_placed_in_neither_is_no_change() {
    assert_eq!(OneDriveChange::of(id(), None, None), None);
}

#[test]
fn a_file_has_new_content_when_its_tag_hash_or_size_is_another() {
    let with = |edit: fn(&mut Row)| {
        let mut new = placed(Kind::File, "a.txt");
        edit(&mut new.as_mut().unwrap().0);
        OneDriveChange::of(id(), placed(Kind::File, "a.txt"), new).unwrap()
    };
    assert!(!with(|_| {}).has_new_content());
    assert!(with(|row| row.ctag = Some("other".into())).has_new_content());
    assert!(with(|row| row.quickxor = Some("hash".into())).has_new_content());
    assert!(with(|row| row.size = 7).has_new_content());
    // A name alone is not content.
    assert!(!with(|row| row.name = "b.txt".into()).has_new_content());
}

#[test]
fn a_file_that_moved_has_new_content_when_its_content_is_another_too() {
    let mut new = placed(Kind::File, "docs/a.txt");
    new.as_mut().unwrap().0.size = 7;
    assert!(OneDriveChange::of(id(), placed(Kind::File, "a.txt"), new).unwrap().has_new_content());
}

#[test]
fn a_directory_and_what_appeared_or_vanished_has_no_new_content() {
    assert!(!OneDriveChange::of(id(), placed(Kind::Folder, "docs"), placed(Kind::Folder, "docs")).unwrap().has_new_content());
    assert!(!OneDriveChange::of(id(), None, placed(Kind::File, "a.txt")).unwrap().has_new_content());
    assert!(!OneDriveChange::of(id(), placed(Kind::File, "a.txt"), None).unwrap().has_new_content());
}

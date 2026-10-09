use std::path::PathBuf;

use konedrive_tree::{Kind, Row};

use super::{list_announcements, OneDriveChange, UncommittedCycle};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::inotify_bridge::Announcement;

/// The OneDrive id `A` in the directory `D`, at `rel`.
fn placed(kind: Kind, rel: &str) -> Option<(Row, PathBuf)> {
    let rel = PathBuf::from(rel);
    let name = rel.file_name().unwrap().to_string_lossy().into_owned();
    Some((row("A", Some("D"), &name, kind), rel))
}

fn change(old: Option<(Row, PathBuf)>, new: Option<(Row, PathBuf)>) -> OneDriveChange {
    OneDriveChange { id: "A".into(), old, new }
}

fn vanished(kind: Kind, rel: &str) -> Announcement {
    Announcement::NameVanished { rel: rel.into(), kind, id: "A".into(), parent_id: Some("D".into()) }
}

fn appeared(kind: Kind, rel: &str) -> Announcement {
    Announcement::NameAppeared { rel: rel.into(), kind, parent_id: Some("D".into()) }
}

#[test]
fn what_is_gone_from_onedrive_is_announced_as_a_name_that_vanished() {
    assert_eq!(list_announcements(&change(placed(Kind::File, "docs/a.txt"), None)), [vanished(Kind::File, "docs/a.txt")]);
    assert_eq!(list_announcements(&change(placed(Kind::Folder, "docs/old"), None)), [vanished(Kind::Folder, "docs/old")]);
}

#[test]
fn what_is_new_in_onedrive_is_announced_as_a_name_that_appeared() {
    assert_eq!(list_announcements(&change(None, placed(Kind::File, "docs/a.txt"))), [appeared(Kind::File, "docs/a.txt")]);
}

#[test]
fn what_moved_is_announced_as_a_name_that_vanished_and_one_that_appeared() {
    let moved = change(placed(Kind::Folder, "docs/old"), placed(Kind::Folder, "docs/new"));
    assert_eq!(list_announcements(&moved), [vanished(Kind::Folder, "docs/old"), appeared(Kind::Folder, "docs/new")]);
}

#[test]
fn a_file_that_stayed_where_it_was_is_announced_as_changed_and_a_directory_is_not() {
    let file = change(placed(Kind::File, "docs/a.txt"), placed(Kind::File, "docs/a.txt"));
    assert_eq!(list_announcements(&file), [Announcement::FileShownChanged { rel: "docs/a.txt".into(), id: "A".into() }]);
    let directory = change(placed(Kind::Folder, "docs/sub"), placed(Kind::Folder, "docs/sub"));
    assert!(list_announcements(&directory).is_empty());
    // What the store never had placed and the new tree has not placed either.
    assert!(list_announcements(&change(None, None)).is_empty());
}

#[test]
fn a_file_has_new_content_when_its_tag_hash_or_size_is_another() {
    let old = placed(Kind::File, "a.txt");
    let with = |edit: fn(&mut Row)| {
        let mut new = placed(Kind::File, "a.txt");
        edit(&mut new.as_mut().unwrap().0);
        change(old.clone(), new)
    };
    assert!(!with(|_| {}).has_new_content());
    assert!(with(|row| row.ctag = Some("other".into())).has_new_content());
    assert!(with(|row| row.quickxor = Some("hash".into())).has_new_content());
    assert!(with(|row| row.size = 7).has_new_content());
    // A name alone is not content, and a directory has none.
    assert!(!with(|row| row.name = "b.txt".into()).has_new_content());
    assert!(!change(placed(Kind::Folder, "docs"), placed(Kind::Folder, "docs")).has_new_content());
    assert!(!change(None, placed(Kind::File, "a.txt")).has_new_content());
}

#[test]
fn what_a_cycle_leaves_is_taken_once_when_it_is_committed() {
    let uncommitted_cycle = UncommittedCycle::default();
    uncommitted_cycle.keep(vec![vanished(Kind::File, "a.txt")], vec!["A".to_owned()]);
    assert_eq!(uncommitted_cycle.take(), (vec![vanished(Kind::File, "a.txt")], vec!["A".to_owned()]));
    assert_eq!(uncommitted_cycle.take(), (Vec::new(), Vec::new()));
}

/// A cycle that was applied and never committed is applied again: what it left is not
/// announced twice.
#[test]
fn only_the_last_cycle_applied_leaves_anything() {
    let uncommitted_cycle = UncommittedCycle::default();
    uncommitted_cycle.keep(vec![vanished(Kind::File, "a.txt")], vec!["A".to_owned()]);
    uncommitted_cycle.keep(vec![appeared(Kind::File, "b.txt")], Vec::new());
    assert_eq!(uncommitted_cycle.take(), (vec![appeared(Kind::File, "b.txt")], Vec::new()));
}

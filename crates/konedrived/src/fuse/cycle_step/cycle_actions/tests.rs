use std::path::PathBuf;

use konedrive_tree::Kind;

use super::{list_cycle_actions, order_deepest_first, BackingDirectoryFacts, CycleAction, NewPathContents, RemovedDirectory};
use crate::fuse::cycle_step::onedrive_change::OneDriveChange;
use crate::fuse::cycle_step::tests::placed;
use crate::fuse::inotify_bridge::Announcement;

fn change(old: Option<&str>, new: Option<&str>, kind: Kind) -> OneDriveChange {
    OneDriveChange::of("A".into(), old.and_then(|rel| placed(kind, rel)), new.and_then(|rel| placed(kind, rel))).unwrap()
}

fn nothing_on_disk() -> BackingDirectoryFacts {
    BackingDirectoryFacts::default()
}

fn at_new_path(new_path: NewPathContents) -> BackingDirectoryFacts {
    BackingDirectoryFacts { holds_at_old_path: true, new_path }
}

fn vanished(kind: Kind, path: &str) -> CycleAction {
    CycleAction::Announce(Announcement::NameVanished { path: path.into(), kind, onedrive_id: "A".into(), parent_id: Some("D".into()) })
}

fn appeared(kind: Kind, path: &str) -> CycleAction {
    CycleAction::Announce(Announcement::NameAppeared { path: path.into(), kind, parent_id: Some("D".into()) })
}

fn aside(path: &str) -> CycleAction {
    CycleAction::MoveLocalOnlyEntryAside { path: path.into(), parent_id: Some("D".into()) }
}

fn remove_copy(path: &str) -> CycleAction {
    CycleAction::RemoveDownloadedCopy { path: path.into(), onedrive_id: "A".into() }
}

fn move_on_disk(from: &str, to: &str) -> CycleAction {
    CycleAction::MoveInBackingDirectory { from: from.into(), to: to.into() }
}

#[test]
fn a_file_that_vanished_loses_its_downloaded_copy_and_its_name() {
    let actions = list_cycle_actions(&change(Some("docs/a.txt"), None, Kind::File), &nothing_on_disk());
    assert_eq!(actions, [remove_copy("docs/a.txt"), vanished(Kind::File, "docs/a.txt")]);
}

#[test]
fn a_directory_that_vanished_is_removed_if_empty_and_loses_its_name() {
    let actions = list_cycle_actions(&change(Some("docs/old"), None, Kind::Folder), &nothing_on_disk());
    assert_eq!(actions, [CycleAction::RemoveDirectoryIfEmpty { onedrive_id: "A".into(), path: "docs/old".into() }, vanished(Kind::Folder, "docs/old")]);
}

#[test]
fn what_appeared_where_nothing_is_only_gets_its_name() {
    let actions = list_cycle_actions(&change(None, Some("docs/a.txt"), Kind::File), &nothing_on_disk());
    assert_eq!(actions, [appeared(Kind::File, "docs/a.txt")]);
}

#[test]
fn what_appeared_where_the_users_own_is_moves_it_aside_first() {
    let facts = BackingDirectoryFacts { holds_at_old_path: false, new_path: NewPathContents::UnmarkedEntry };
    let actions = list_cycle_actions(&change(None, Some("docs/a.txt"), Kind::File), &facts);
    assert_eq!(actions, [aside("docs/a.txt"), appeared(Kind::File, "docs/a.txt")]);
}

/// What is marked with a OneDrive id at the new path is OneDrive's own, and stays.
#[test]
fn what_appeared_where_another_entry_of_onedrive_is_moves_nothing_aside() {
    let facts = BackingDirectoryFacts { holds_at_old_path: false, new_path: NewPathContents::OtherEntry };
    let actions = list_cycle_actions(&change(None, Some("docs/a.txt"), Kind::File), &facts);
    assert_eq!(actions, [appeared(Kind::File, "docs/a.txt")]);
}

#[test]
fn a_file_that_moved_takes_its_downloaded_copy_along() {
    let actions = list_cycle_actions(&change(Some("a.txt"), Some("docs/a.txt"), Kind::File), &at_new_path(NewPathContents::Nothing));
    assert_eq!(actions, [move_on_disk("a.txt", "docs/a.txt"), vanished(Kind::File, "a.txt"), appeared(Kind::File, "docs/a.txt")]);
}

#[test]
fn a_file_that_moved_and_is_not_downloaded_moves_nothing_on_disk() {
    let actions = list_cycle_actions(&change(Some("a.txt"), Some("docs/a.txt"), Kind::File), &nothing_on_disk());
    assert_eq!(actions, [vanished(Kind::File, "a.txt"), appeared(Kind::File, "docs/a.txt")]);
}

#[test]
fn a_file_that_moved_to_where_the_users_own_is_moves_it_aside_and_then_moves() {
    let actions = list_cycle_actions(&change(Some("a.txt"), Some("docs/a.txt"), Kind::File), &at_new_path(NewPathContents::UnmarkedEntry));
    assert_eq!(actions, [aside("docs/a.txt"), move_on_disk("a.txt", "docs/a.txt"), vanished(Kind::File, "a.txt"), appeared(Kind::File, "docs/a.txt")]);
}

#[test]
fn a_file_that_moved_to_where_another_entry_stays_is_not_moved_onto_it() {
    let actions = list_cycle_actions(&change(Some("a.txt"), Some("docs/a.txt"), Kind::File), &at_new_path(NewPathContents::OtherEntry));
    assert_eq!(actions, [vanished(Kind::File, "a.txt"), appeared(Kind::File, "docs/a.txt")]);
}

/// The copy at the old path is of the old content: removed, and not moved.
#[test]
fn a_file_that_moved_with_new_content_loses_its_downloaded_copy() {
    let mut new = placed(Kind::File, "docs/a.txt");
    new.as_mut().unwrap().0.size = 7;
    let moved = OneDriveChange::of("A".into(), placed(Kind::File, "a.txt"), new).unwrap();
    let actions = list_cycle_actions(&moved, &at_new_path(NewPathContents::Nothing));
    assert_eq!(actions, [remove_copy("a.txt"), vanished(Kind::File, "a.txt"), appeared(Kind::File, "docs/a.txt")]);
}

#[test]
fn a_directory_that_moved_has_its_rows_rebased_before_it_moves() {
    let actions = list_cycle_actions(&change(Some("docs/old"), Some("docs/new"), Kind::Folder), &at_new_path(NewPathContents::Nothing));
    let rebase = CycleAction::RebaseOutboxRows { from: "docs/old".into(), to: "docs/new".into() };
    assert_eq!(actions, [rebase, move_on_disk("docs/old", "docs/new"), vanished(Kind::Folder, "docs/old"), appeared(Kind::Folder, "docs/new")]);
}

#[test]
fn a_file_that_stayed_is_announced_as_shown_anew() {
    let actions = list_cycle_actions(&change(Some("docs/a.txt"), Some("docs/a.txt"), Kind::File), &nothing_on_disk());
    assert_eq!(actions, [CycleAction::Announce(Announcement::FileShownChanged { path: "docs/a.txt".into(), onedrive_id: "A".into() })]);
}

#[test]
fn a_file_that_stayed_with_new_content_loses_its_downloaded_copy() {
    let mut new = placed(Kind::File, "docs/a.txt");
    new.as_mut().unwrap().0.ctag = Some("other".into());
    let stayed = OneDriveChange::of("A".into(), placed(Kind::File, "docs/a.txt"), new).unwrap();
    let actions = list_cycle_actions(&stayed, &nothing_on_disk());
    assert_eq!(actions, [remove_copy("docs/a.txt"), CycleAction::Announce(Announcement::FileShownChanged { path: "docs/a.txt".into(), onedrive_id: "A".into() })]);
}

#[test]
fn a_directory_that_stayed_does_nothing() {
    assert!(list_cycle_actions(&change(Some("docs/sub"), Some("docs/sub"), Kind::Folder), &nothing_on_disk()).is_empty());
}

#[test]
fn the_removed_directories_go_deepest_first_and_keep_their_order_at_one_depth() {
    let removed = |onedrive_id: &str, path: &str| RemovedDirectory { onedrive_id: onedrive_id.into(), path: PathBuf::from(path) };
    let ordered = order_deepest_first(vec![removed("1", "a"), removed("2", "a/b/c"), removed("3", "a/b"), removed("4", "x/y")]);
    assert_eq!(ordered, [removed("2", "a/b/c"), removed("3", "a/b"), removed("4", "x/y"), removed("1", "a")]);
}

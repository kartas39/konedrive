use std::path::PathBuf;

use fuser::Errno;
use konedrive_tree::Kind;

use super::{describe_rename, plan_rename, RenameDecision, RenameMode, RenamePlaces, RenamePlan};
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::detections::tests::{directory, file, handle, in_root, local_only};
use crate::fuse::detections::{DisplacedEntry, EntryRenamed, MovedEntry, WriteState};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::upload_rules::UploadStatus;

fn root() -> DirectoryReference {
    DirectoryReference { onedrive_id: Some("R".into()), path: PathBuf::new() }
}

fn docs() -> DirectoryReference {
    DirectoryReference { onedrive_id: Some("D".into()), path: PathBuf::from("docs") }
}

fn onedrive_file(id: &str, name: &str) -> DirectoryEntry {
    DirectoryEntry { name: name.into(), state: EntryState::File(FileSyncState::RemoteOnly(row(id, Some("R"), name, Kind::File))) }
}

fn onedrive_directory(id: &str, name: &str) -> DirectoryEntry {
    DirectoryEntry { name: name.into(), state: EntryState::Directory(DirectorySyncState::InOneDrive(row(id, Some("R"), name, Kind::Folder))) }
}

fn local_only_file(name: &str) -> DirectoryEntry {
    DirectoryEntry::from_local_only(local_only(&file(1, 3), &in_root(name)))
}

fn local_only_directory(name: &str) -> DirectoryEntry {
    DirectoryEntry::from_local_only(local_only(&directory(2), &in_root(name)))
}

/// `moved_entry`, which is `a.txt` in the root, renamed to `new.txt` in `docs`, where
/// nothing is.
fn places(moved_entry: DirectoryEntry) -> RenamePlaces {
    RenamePlaces {
        from_directory: Some(root()),
        to_directory: Some(docs()),
        name: "a.txt".into(),
        new_name: "new.txt".into(),
        new_name_is_daemon_owned: false,
        moved_entry: Some(moved_entry),
        target: None,
        target_holds_entries: false,
        to_is_in_ignored_directory: false,
        to_upload_status: UploadStatus::Uploaded,
    }
}

fn over(moved_entry: DirectoryEntry, target: DirectoryEntry) -> RenamePlaces {
    RenamePlaces { target: Some(target), ..places(moved_entry) }
}

fn refusal(mode: RenameMode, places: RenamePlaces) -> Option<Errno> {
    plan_rename(mode, places).err()
}

#[test]
fn a_rename_to_a_free_place_moves_the_entry_from_its_path_to_the_new_place() {
    let moved_entry = onedrive_file("A", "a.txt");
    let plan = RenamePlan { moved_entry: moved_entry.clone(), target: None, from: "a.txt".into(), to: EntryPlace::new(&docs(), "new.txt"), to_upload_status: UploadStatus::Uploaded };
    assert_eq!(plan_rename(RenameMode::Replace, places(moved_entry.clone())), Ok(RenameDecision::Rename(plan.clone())));
    // Nothing is there to keep.
    assert_eq!(plan_rename(RenameMode::NoReplace, places(moved_entry)), Ok(RenameDecision::Rename(plan)));
}

#[test]
fn a_node_that_stands_for_no_directory_is_refused_first() {
    let from_gone = RenamePlaces { from_directory: None, new_name_is_daemon_owned: true, ..places(onedrive_file("A", "a.txt")) };
    assert_eq!(refusal(RenameMode::Replace, from_gone), Some(Errno::ENOENT));
    let to_gone = RenamePlaces { to_directory: None, new_name_is_daemon_owned: true, ..places(onedrive_file("A", "a.txt")) };
    assert_eq!(refusal(RenameMode::Replace, to_gone), Some(Errno::ENOENT));
}

#[test]
fn one_of_the_daemons_own_names_is_refused_before_the_old_place_is_looked_at() {
    let daemon_owned = RenamePlaces { new_name_is_daemon_owned: true, moved_entry: None, ..places(onedrive_file("A", "a.txt")) };
    assert_eq!(refusal(RenameMode::Replace, daemon_owned), Some(Errno::EPERM));
}

#[test]
fn nothing_at_the_old_place_is_no_entry() {
    let nothing = RenamePlaces { moved_entry: None, ..places(onedrive_file("A", "a.txt")) };
    assert_eq!(refusal(RenameMode::Replace, nothing), Some(Errno::ENOENT));
}

/// Also when the rename is told to keep what is at the new place: the entry is there itself.
#[test]
fn a_rename_onto_itself_renames_nothing() {
    let moved_entry = onedrive_file("A", "a.txt");
    let onto_itself = RenamePlaces { to_directory: Some(root()), new_name: "a.txt".into(), target: Some(moved_entry.clone()), ..places(moved_entry) };
    assert_eq!(plan_rename(RenameMode::Replace, onto_itself.clone()), Ok(RenameDecision::NothingToRename));
    assert_eq!(plan_rename(RenameMode::NoReplace, onto_itself), Ok(RenameDecision::NothingToRename));
}

#[test]
fn a_rename_that_keeps_what_is_there_is_refused_by_any_entry() {
    for target in [onedrive_file("B", "new.txt"), local_only_file("new.txt"), onedrive_directory("E", "new.txt")] {
        assert_eq!(refusal(RenameMode::NoReplace, over(onedrive_file("A", "a.txt"), target)), Some(Errno::EEXIST));
    }
}

#[test]
fn a_file_replaces_a_file_and_is_planned_with_what_it_replaces() {
    let (moved_entry, target) = (onedrive_file("A", "a.txt"), onedrive_file("B", "new.txt"));
    let decision = plan_rename(RenameMode::Replace, over(moved_entry, target.clone())).unwrap();
    assert!(matches!(decision, RenameDecision::Rename(plan) if plan.target == Some(target)));
}

#[test]
fn a_file_is_not_renamed_over_a_directory() {
    assert_eq!(refusal(RenameMode::Replace, over(onedrive_file("A", "a.txt"), onedrive_directory("E", "new.txt"))), Some(Errno::EISDIR));
}

#[test]
fn a_directory_is_not_renamed_over_a_file() {
    assert_eq!(refusal(RenameMode::Replace, over(onedrive_directory("E", "a.txt"), local_only_file("new.txt"))), Some(Errno::ENOTDIR));
}

#[test]
fn a_directory_replaces_a_directory_only_when_that_one_holds_nothing() {
    let empty = over(onedrive_directory("E", "a.txt"), local_only_directory("new.txt"));
    assert!(matches!(plan_rename(RenameMode::Replace, empty.clone()), Ok(RenameDecision::Rename(_))));
    let holding = RenamePlaces { target_holds_entries: true, ..empty };
    assert_eq!(refusal(RenameMode::Replace, holding), Some(Errno::ENOTEMPTY));
}

#[test]
fn what_is_in_onedrive_is_not_moved_to_where_nothing_is_uploaded() {
    let ignored = |moved_entry| RenamePlaces { to_is_in_ignored_directory: true, to_upload_status: UploadStatus::Ignored, ..places(moved_entry) };
    assert_eq!(refusal(RenameMode::Replace, ignored(onedrive_file("A", "a.txt"))), Some(Errno::EXDEV));
    assert_eq!(refusal(RenameMode::Replace, ignored(onedrive_directory("E", "a.txt"))), Some(Errno::EXDEV));
}

/// A local-only entry is only no longer uploaded there, which the plan says.
#[test]
fn a_local_only_entry_moves_to_where_nothing_is_uploaded() {
    let ignored = RenamePlaces { to_is_in_ignored_directory: true, to_upload_status: UploadStatus::Ignored, ..places(local_only_file("a.txt")) };
    let decision = plan_rename(RenameMode::Replace, ignored).unwrap();
    assert!(matches!(decision, RenameDecision::Rename(plan) if plan.to_upload_status == UploadStatus::Ignored));
}

#[test]
fn a_local_only_file_renamed_over_a_onedrive_file_is_saved_over_it() {
    let saved = plan_rename(RenameMode::Replace, over(local_only_file("a.txt"), onedrive_file("B", "new.txt"))).unwrap();
    let RenameDecision::Rename(plan) = saved else { panic!("a rename: {saved:?}") };
    assert_eq!(plan.saved_over().map(|(file, over)| (file.place.name.as_str(), over)), Some(("a.txt", "B")));
}

#[test]
fn any_other_rename_saves_over_nothing() {
    let plans = [
        over(onedrive_file("A", "a.txt"), onedrive_file("B", "new.txt")),
        over(local_only_file("a.txt"), local_only_file("new.txt")),
        places(local_only_file("a.txt")),
        over(local_only_directory("a.txt"), onedrive_directory("E", "new.txt")),
    ];
    for places in plans {
        let RenameDecision::Rename(plan) = plan_rename(RenameMode::Replace, places).unwrap() else { panic!("a rename") };
        assert_eq!(plan.saved_over().map(|(_, over)| over), None);
    }
}

/// The plan of `moved_entry`, which is `a.txt` in the root, renamed to `new.txt` in `docs`,
/// over `target`, with `to_upload_status` at the new place.
fn plan(moved_entry: DirectoryEntry, target: Option<DirectoryEntry>, to_upload_status: UploadStatus) -> RenamePlan {
    RenamePlan { moved_entry, target, from: "a.txt".into(), to: EntryPlace::new(&docs(), "new.txt"), to_upload_status }
}

/// What `plan` is recorded as moving, when nothing is displaced.
fn moved(plan: &RenamePlan, entry: MovedEntry) -> EntryRenamed {
    EntryRenamed::Moved { entry, from: plan.from.clone(), to: plan.to.clone(), displaced: None }
}

#[test]
fn only_a_local_only_directory_that_is_uploaded_from_its_new_place_is_walked() {
    assert!(plan(local_only_directory("a.txt"), None, UploadStatus::Uploaded).uploads_moved_directory());
    assert!(!plan(local_only_directory("a.txt"), None, UploadStatus::Ignored).uploads_moved_directory());
    assert!(!plan(local_only_file("a.txt"), None, UploadStatus::Uploaded).uploads_moved_directory());
    assert!(!plan(onedrive_directory("D", "a.txt"), None, UploadStatus::Uploaded).uploads_moved_directory());
}

#[test]
fn a_local_only_file_renamed_over_a_onedrive_file_is_recorded_as_saved_over_it() {
    let plan = plan(local_only_file("a.txt"), Some(onedrive_file("B", "new.txt")), UploadStatus::Uploaded);
    let saved = EntryRenamed::SavedOverOneDriveFile { file: file(1, 3), write_state: WriteState::OpenForWriting, over: "B".into(), to: plan.to.clone() };
    assert_eq!(describe_rename(&plan, WriteState::OpenForWriting, None, Vec::new()), saved);
}

/// With the handle of what the backing directory moved for it, or none when it held nothing.
#[test]
fn a_onedrive_file_or_directory_is_recorded_as_moved_in_onedrive() {
    let file_plan = plan(onedrive_file("A", "a.txt"), None, UploadStatus::Uploaded);
    let moved_file = MovedEntry::InOneDrive { id: "A".into(), kind: EntryKind::File, handle: Some(handle(7)) };
    assert_eq!(describe_rename(&file_plan, WriteState::NotOpen, Some(handle(7)), Vec::new()), moved(&file_plan, moved_file));
    // What the rules say of the new place is not asked of what is in OneDrive.
    let directory_plan = plan(onedrive_directory("D", "a.txt"), None, UploadStatus::Ignored);
    let moved_directory = MovedEntry::InOneDrive { id: "D".into(), kind: EntryKind::Directory, handle: None };
    assert_eq!(describe_rename(&directory_plan, WriteState::NotOpen, None, Vec::new()), moved(&directory_plan, moved_directory));
}

#[test]
fn a_local_only_file_is_recorded_as_uploaded_from_its_new_place_or_ignored_there() {
    let uploaded = plan(local_only_file("a.txt"), None, UploadStatus::Uploaded);
    assert_eq!(describe_rename(&uploaded, WriteState::OpenForWriting, Some(handle(1)), Vec::new()), moved(&uploaded, MovedEntry::LocalOnlyFile { file: file(1, 3), write_state: WriteState::OpenForWriting }));
    let ignored = plan(local_only_file("a.txt"), None, UploadStatus::Ignored);
    assert_eq!(describe_rename(&ignored, WriteState::NotOpen, Some(handle(1)), Vec::new()), moved(&ignored, MovedEntry::IgnoredLocalOnlyFile { file: file(1, 3) }));
}

#[test]
fn a_local_only_directory_is_recorded_with_what_the_walk_found_at_its_new_place() {
    let uploaded = plan(local_only_directory("a.txt"), None, UploadStatus::Uploaded);
    let uploaded_entries = vec![local_only(&directory(2), &uploaded.to), local_only(&file(3, 1), &EntryPlace::new(&DirectoryReference { onedrive_id: None, path: "docs/new.txt".into() }, "x.txt"))];
    assert_eq!(describe_rename(&uploaded, WriteState::NotOpen, Some(handle(2)), uploaded_entries.clone()), moved(&uploaded, MovedEntry::LocalOnlyDirectory { uploaded_entries }));
}

#[test]
fn a_local_only_directory_moved_to_an_ignored_place_is_recorded_as_ignored() {
    let ignored = plan(local_only_directory("a.txt"), None, UploadStatus::Ignored);
    assert_eq!(describe_rename(&ignored, WriteState::NotOpen, Some(handle(2)), Vec::new()), moved(&ignored, MovedEntry::IgnoredLocalOnlyDirectory { handle: handle(2) }));
}

/// The directory is marked with a OneDrive id the store does not have yet: the mount shows
/// it as local-only, and the walk, which takes only what is unmarked, finds nothing.
#[test]
fn a_local_only_directory_whose_walk_finds_nothing_is_recorded_as_ignored() {
    let uploaded = plan(local_only_directory("a.txt"), None, UploadStatus::Uploaded);
    assert_eq!(describe_rename(&uploaded, WriteState::NotOpen, Some(handle(2)), Vec::new()), moved(&uploaded, MovedEntry::IgnoredLocalOnlyDirectory { handle: handle(2) }));
}

#[test]
fn what_was_at_the_new_place_is_recorded_as_displaced() {
    let over_onedrive = plan(onedrive_file("A", "a.txt"), Some(onedrive_file("B", "new.txt")), UploadStatus::Uploaded);
    let EntryRenamed::Moved { displaced, .. } = describe_rename(&over_onedrive, WriteState::NotOpen, None, Vec::new()) else { panic!("a move") };
    assert_eq!(displaced, Some(DisplacedEntry::InOneDrive("B".into())));
    let over_local_only = plan(onedrive_file("A", "a.txt"), Some(local_only_file("new.txt")), UploadStatus::Uploaded);
    let EntryRenamed::Moved { displaced, .. } = describe_rename(&over_local_only, WriteState::NotOpen, None, Vec::new()) else { panic!("a move") };
    assert_eq!(displaced, Some(DisplacedEntry::LocalOnly(handle(1))));
}

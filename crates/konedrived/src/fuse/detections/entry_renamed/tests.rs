//! A test for each way a rename is recorded.

use std::path::Path;

use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState, Reason};

use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use super::{DisplacedEntry, EntryRenamed, MovedEntry};
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::detections::WriteState;
use crate::fuse::detections::tests::{detect, detection_of, directory, file, handle, in_docs, in_root, local_only, quiet, rel, removes, rows, store, writing};
use crate::fuse::directory_contents::tests::of;

fn moved(entry: MovedEntry, from: &str, to: EntryPlace, displaced: Option<DisplacedEntry>) -> EntryRenamed {
    EntryRenamed::Moved { entry, from: rel(from), to, displaced }
}

fn in_onedrive(id: &str, kind: EntryKind) -> MovedEntry {
    MovedEntry::InOneDrive { id: id.into(), kind, handle: None }
}

/// What each detection is, in order: its kind, and its OneDrive id or the first byte of its
/// file handle.
fn kinds(renamed: EntryRenamed, store: &konedrive_tree::TreeStore) -> Vec<(OutboxKind, String)> {
    kinds_of(&detect(renamed, store))
}

fn pair(kind: OutboxKind, what: &str) -> (OutboxKind, String) {
    (kind, what.to_owned())
}

#[test]
fn a_onedrive_file_renamed_is_moved_with_the_content_it_had() {
    let store = store(Vec::new());
    let renamed = moved(MovedEntry::InOneDrive { id: "A".into(), kind: EntryKind::File, handle: Some(handle(9)) }, "a.txt", in_docs("new.txt"), None);
    let batch = detect(renamed, &store);
    let [d] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    assert_eq!((d.kind, d.item_id.as_deref(), d.object.as_ref(), d.same_content), (OutboxKind::Move, Some("A"), Some(&handle(9)), true));
    assert_eq!((d.rel.as_path(), d.target_parent.as_deref(), d.target_name.as_deref()), (Path::new("docs/new.txt"), Some("D"), Some("new.txt")));
    assert_eq!(d.base.as_ref().map(|b| (b.parent.as_deref(), b.name.as_deref())), Some((Some("R"), Some("a.txt"))));
    assert!(batch.ops.is_empty());
}

#[test]
fn a_onedrive_file_with_new_content_waiting_is_moved_with_it() {
    let store = store(vec![of(OutboxKind::Update, "A", "a.txt", "a.txt", (Some("R"), Some("a.txt")))]);
    let batch = detect(moved(in_onedrive("A", EntryKind::File), "a.txt", in_root("new.txt"), None), &store);
    assert!(!batch.detections[0].same_content);
}

#[test]
fn a_onedrive_directory_moved_takes_the_rows_below_it_along() {
    let batch = detect(moved(in_onedrive("D", EntryKind::Directory), "docs", in_root("papers"), None), &store(Vec::new()));
    assert_eq!(batch.detections[0].kind, OutboxKind::Move);
    assert_eq!(batch.ops, [OutboxOp::Rebase { from: rel("docs"), to: rel("papers") }]);
}

/// What was at the new place is removed first: its row frees the name the move takes.
#[test]
fn a_onedrive_file_renamed_over_another_deletes_that_one_first() {
    let store = store(Vec::new());
    let renamed = moved(in_onedrive("A", EntryKind::File), "a.txt", in_root("b.txt"), Some(DisplacedEntry::InOneDrive("B".into())));
    assert_eq!(kinds(renamed, &store), [pair(OutboxKind::Delete, "B"), pair(OutboxKind::Move, "A")]);
}

#[test]
fn a_onedrive_file_renamed_over_a_local_only_entry_forgets_it_if_the_outbox_held_it() {
    let mine = file(1, 3);
    let held = store(vec![detection_of(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Ready)]);
    let over = || moved(in_onedrive("A", EntryKind::File), "a.txt", in_root("mine.txt"), Some(DisplacedEntry::LocalOnly(mine.handle.clone())));
    assert_eq!(kinds(over(), &held), [pair(OutboxKind::Delete, "#1"), pair(OutboxKind::Move, "A")]);
    assert_eq!(kinds(over(), &store(Vec::new())), [pair(OutboxKind::Move, "A")]);
}

#[test]
fn what_the_base_no_longer_has_is_not_moved() {
    assert!(detect(moved(in_onedrive("gone", EntryKind::File), "gone.txt", in_root("new.txt"), None), &store(Vec::new())).is_empty());
}

/// Save by a temporary file: the file's own create is removed, and the OneDrive file gets its
/// content. A row of the file that is being sent is left to finish.
#[test]
fn a_local_only_file_saved_over_a_onedrive_file_is_that_files_new_content() {
    let tmp = file(1, 7);
    let store = store(vec![
        detection_of(OutboxKind::Create, &tmp, &in_root("a.tmp"), OutboxState::Running),
        detection_of(OutboxKind::Create, &tmp, &in_root("a.tmp"), OutboxState::Ready),
    ]);
    let all = rows(&store);
    let saved = |write_state| EntryRenamed::SavedOverOneDriveFile { file: tmp.clone(), write_state, over: "A".into(), to: in_root("a.txt") };
    let batch = detect(saved(WriteState::NotOpen), &store);
    let [update] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    assert_eq!((update.kind, update.item_id.as_deref(), update.object.as_ref(), update.size), (OutboxKind::Update, Some("A"), Some(&tmp.handle), Some(7)));
    assert_eq!((update.state, update.rel.as_path()), (OutboxState::Ready, Path::new("a.txt")));
    let pending: Vec<_> = all.iter().filter(|row| row.state != OutboxState::Running).collect();
    assert_eq!(batch.ops, removes(&pending));
    let open = detect(saved(WriteState::OpenForWriting), &store);
    assert_eq!((open.detections[0].state, open.detections[0].reason.clone()), (OutboxState::Waiting, Some(Reason::OpenForWriting)));
}

#[test]
fn a_file_saved_over_a_file_the_base_no_longer_has_is_recorded_as_nothing() {
    let saved = EntryRenamed::SavedOverOneDriveFile { file: file(1, 7), write_state: WriteState::NotOpen, over: "gone".into(), to: in_root("gone.txt") };
    assert!(detect(saved, &store(Vec::new())).is_empty());
}

#[test]
fn a_local_only_file_moved_is_uploaded_from_its_new_place() {
    let mine = file(1, 3);
    let to = || in_docs("mine.txt");
    let uploaded = |write_state| moved(MovedEntry::LocalOnlyFile { file: mine.clone(), write_state }, "mine.txt", to(), None);
    let ready = detection_of(OutboxKind::Create, &mine, &to(), OutboxState::Ready);
    assert_eq!(detect(uploaded(WriteState::NotOpen), &store(Vec::new())).detections, [quiet(ready.clone())]);
    assert_eq!(detect(uploaded(WriteState::OpenForWriting), &store(Vec::new())).detections, [writing(ready)]);
}

/// Going up right now: it goes up as it is, and only where it is changed.
#[test]
fn a_local_only_file_moved_while_it_is_uploaded_is_moved() {
    let mine = file(1, 3);
    let store = store(vec![detection_of(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Running)]);
    let renamed = moved(MovedEntry::LocalOnlyFile { file: mine.clone(), write_state: WriteState::NotOpen }, "mine.txt", in_docs("mine.txt"), None);
    assert_eq!(kinds(renamed, &store), [pair(OutboxKind::Move, "#1")]);
}

/// Into a place that is ignored: its row is removed, and nothing is recorded in its stead —
/// a removal recorded beside it would find no row to cancel, and stay in the outbox.
#[test]
fn a_local_only_file_moved_to_an_ignored_place_leaves_no_row() {
    let mine = file(1, 3);
    let store = store(vec![detection_of(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Ready)]);
    let all = rows(&store);
    let renamed = moved(MovedEntry::IgnoredLocalOnlyFile { file: mine.clone() }, "mine.txt", in_root("mine.tmp"), None);
    let batch = detect(renamed, &store);
    assert!(batch.detections.is_empty(), "{:?}", batch.detections);
    assert_eq!(batch.ops, removes(&all.iter().collect::<Vec<_>>()));
}

/// Its row that is being sent is left to finish, and what it makes in OneDrive is removed
/// after it.
#[test]
fn a_local_only_file_moved_to_an_ignored_place_while_it_is_uploaded_keeps_the_running_row() {
    let mine = file(1, 3);
    let store = store(vec![detection_of(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Running)]);
    let renamed = moved(MovedEntry::IgnoredLocalOnlyFile { file: mine.clone() }, "mine.txt", in_root("mine.tmp"), None);
    let batch = detect(renamed, &store);
    let [delete] = &batch.detections[..] else { panic!("one detection: {:?}", batch.detections) };
    assert_eq!((delete.kind, delete.object.as_ref(), delete.rel.as_path()), (OutboxKind::Delete, Some(&mine.handle), Path::new("mine.tmp")));
    assert!(batch.ops.is_empty());
}

/// A row that is being sent and one that waits behind it: the one that waits is removed, and
/// the removal follows the one that is being sent.
#[test]
fn a_local_only_file_moved_to_an_ignored_place_loses_the_row_that_waits_behind_the_running_one() {
    let mine = file(1, 3);
    let store = store(vec![
        detection_of(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Running),
        detection_of(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Ready),
    ]);
    let all = rows(&store);
    let renamed = moved(MovedEntry::IgnoredLocalOnlyFile { file: mine.clone() }, "mine.txt", in_root("mine.tmp"), None);
    let batch = detect(renamed, &store);
    assert_eq!(kinds_of(&batch), [pair(OutboxKind::Delete, "#1")]);
    assert_eq!(batch.ops, removes(&all.iter().filter(|row| row.state != OutboxState::Running).collect::<Vec<_>>()));
}

#[test]
fn a_local_only_directory_moved_is_made_from_its_new_place_with_all_it_holds() {
    let (new_directory, inside) = (directory(1), file(2, 4));
    let child = EntryPlace { directory: DirectoryReference { onedrive_id: None, path: "docs/new".into() }, name: "x.txt".into() };
    let uploaded_entries = vec![local_only(&new_directory, &in_docs("new")), local_only(&inside, &child)];
    let batch = detect(moved(MovedEntry::LocalOnlyDirectory { uploaded_entries }, "new", in_docs("new"), None), &store(Vec::new()));
    let made = detection_of(OutboxKind::Mkdir, &new_directory, &in_docs("new"), OutboxState::Ready);
    assert_eq!(batch.detections, [made, quiet(detection_of(OutboxKind::Create, &inside, &child, OutboxState::Ready))]);
    assert_eq!(batch.ops, [OutboxOp::Rebase { from: rel("new"), to: rel("docs/new") }]);
}

/// Nothing of it is uploaded from the new place: its rows and those of the local-only
/// entries below it are removed, and nothing is recorded in their stead; a row of something
/// of OneDrive below it stays, and moves along.
#[test]
fn a_local_only_directory_moved_to_an_ignored_place_takes_its_rows_out_of_the_outbox() {
    let (new_directory, inside) = (directory(1), file(2, 4));
    let store = store(vec![
        detection_of(OutboxKind::Mkdir, &new_directory, &in_root("new"), OutboxState::Ready),
        detection_of(OutboxKind::Create, &inside, &EntryPlace { directory: DirectoryReference { onedrive_id: None, path: "new".into() }, name: "x.txt".into() }, OutboxState::Ready),
        of(OutboxKind::Move, "A", "a.txt", "new/a.txt", (None, Some("a.txt"))),
    ]);
    let all = rows(&store);
    let renamed = moved(MovedEntry::IgnoredLocalOnlyDirectory { handle: new_directory.handle.clone() }, "new", in_root("new.tmp"), None);
    let batch = detect(renamed, &store);
    assert!(batch.detections.is_empty(), "{:?}", batch.detections);
    let mut ops = removes(&all.iter().filter(|row| row.item_id.is_none()).collect::<Vec<_>>());
    ops.push(OutboxOp::Rebase { from: rel("new"), to: rel("new.tmp") });
    assert_eq!(batch.ops, ops);
}

fn kinds_of(batch: &crate::fuse::detections::OutboxBatch) -> Vec<(OutboxKind, String)> {
    batch.detections.iter().map(|d| (d.kind, d.item_id.clone().unwrap_or_else(|| format!("#{}", d.object.as_ref().map_or(0, |o| o.bytes[0]))))).collect()
}

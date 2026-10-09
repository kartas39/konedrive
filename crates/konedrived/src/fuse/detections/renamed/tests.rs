//! A test for each way a rename is recorded: every branch the old tuple-matching had.

use std::path::Path;

use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState, Reason};

use super::{DisplacedEntry, MovedEntry, Renamed};
use crate::fuse::detections::tests::{detect, dir, file, handle, in_docs, in_root, local, quiet, rel, removes, rows, store, writing};
use crate::fuse::detections::{FolderPlace, LocalEntry};
use crate::fuse::view::tests::of;

fn moved(entry: MovedEntry, from: &str, to: FolderPlace, displaced: Option<DisplacedEntry>) -> Renamed {
    Renamed::Moved { entry, from: rel(from), to, displaced }
}

fn item(id: &str, dir: bool) -> MovedEntry {
    MovedEntry::Item { id: id.into(), dir, object: None }
}

/// What each detection is, in order: its kind, and its item or the first byte of its object.
fn kinds(renamed: Renamed, store: &konedrive_tree::TreeStore) -> Vec<(OutboxKind, String)> {
    kinds_of(&detect(renamed, store))
}

fn pair(kind: OutboxKind, what: &str) -> (OutboxKind, String) {
    (kind, what.to_owned())
}

#[test]
fn an_item_renamed_is_moved_with_the_content_it_had() {
    let store = store(Vec::new());
    let renamed = moved(MovedEntry::Item { id: "A".into(), dir: false, object: Some(handle(9)) }, "a.txt", in_docs("new.txt"), None);
    let batch = detect(renamed, &store);
    let [d] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    assert_eq!((d.kind, d.item_id.as_deref(), d.object.as_ref(), d.same_content), (OutboxKind::Move, Some("A"), Some(&handle(9)), true));
    assert_eq!((d.rel.as_path(), d.target_parent.as_deref(), d.target_name.as_deref()), (Path::new("docs/new.txt"), Some("D"), Some("new.txt")));
    assert_eq!(d.base.as_ref().map(|b| (b.parent.as_deref(), b.name.as_deref())), Some((Some("R"), Some("a.txt"))));
    assert!(batch.ops.is_empty());
}

#[test]
fn an_item_with_new_content_waiting_is_moved_with_it() {
    let store = store(vec![of(OutboxKind::Update, "A", "a.txt", "a.txt", (Some("R"), Some("a.txt")))]);
    let batch = detect(moved(item("A", false), "a.txt", in_root("new.txt"), None), &store);
    assert!(!batch.detections[0].same_content);
}

#[test]
fn a_folder_moved_takes_the_rows_below_it_along() {
    let batch = detect(moved(item("D", true), "docs", in_root("papers"), None), &store(Vec::new()));
    assert_eq!(batch.detections[0].kind, OutboxKind::Move);
    assert_eq!(batch.ops, [OutboxOp::Rebase { from: rel("docs"), to: rel("papers") }]);
}

/// What stood at the new place goes first: its row frees the name the move takes.
#[test]
fn an_item_renamed_over_an_item_deletes_that_one_first() {
    let store = store(Vec::new());
    let renamed = moved(item("A", false), "a.txt", in_root("b.txt"), Some(DisplacedEntry::Item("B".into())));
    assert_eq!(kinds(renamed, &store), [pair(OutboxKind::Delete, "B"), pair(OutboxKind::Move, "A")]);
}

#[test]
fn an_item_renamed_over_a_local_object_forgets_it_if_the_outbox_held_it() {
    let mine = file(1, 3);
    let held = store(vec![local(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Ready)]);
    let over = || moved(item("A", false), "a.txt", in_root("mine.txt"), Some(DisplacedEntry::LocalObject(mine.handle.clone())));
    assert_eq!(kinds(over(), &held), [pair(OutboxKind::Delete, "#1"), pair(OutboxKind::Move, "A")]);
    assert_eq!(kinds(over(), &store(Vec::new())), [pair(OutboxKind::Move, "A")]);
}

#[test]
fn an_item_the_base_no_longer_has_is_not_moved() {
    assert!(detect(moved(item("gone", false), "gone.txt", in_root("new.txt"), None), &store(Vec::new())).is_empty());
}

/// Save by a temporary file: the file's own create goes, and the item gets its content. A
/// row of the file that is being sent is left to finish.
#[test]
fn a_local_file_saved_over_an_items_file_is_that_items_new_content() {
    let tmp = file(1, 7);
    let store = store(vec![
        local(OutboxKind::Create, &tmp, &in_root("a.tmp"), OutboxState::Running),
        local(OutboxKind::Create, &tmp, &in_root("a.tmp"), OutboxState::Ready),
    ]);
    let all = rows(&store);
    let saved = |open| Renamed::SavedOver { file: tmp.clone(), open, over: "A".into(), to: in_root("a.txt") };
    let batch = detect(saved(false), &store);
    let [update] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    assert_eq!((update.kind, update.item_id.as_deref(), update.object.as_ref(), update.size), (OutboxKind::Update, Some("A"), Some(&tmp.handle), Some(7)));
    assert_eq!((update.state, update.rel.as_path()), (OutboxState::Ready, Path::new("a.txt")));
    let pending: Vec<_> = all.iter().filter(|row| row.state != OutboxState::Running).collect();
    assert_eq!(batch.ops, removes(&pending));
    let open = detect(saved(true), &store);
    assert_eq!((open.detections[0].state, open.detections[0].reason.clone()), (OutboxState::Waiting, Some(Reason::OpenForWriting)));
}

#[test]
fn a_file_saved_over_an_item_the_base_no_longer_has_is_recorded_as_nothing() {
    let saved = Renamed::SavedOver { file: file(1, 7), open: false, over: "gone".into(), to: in_root("gone.txt") };
    assert!(detect(saved, &store(Vec::new())).is_empty());
}

#[test]
fn a_local_file_moved_is_uploaded_from_its_new_place() {
    let mine = file(1, 3);
    let to = || in_docs("mine.txt");
    let uploaded = |open| moved(MovedEntry::LocalFile { file: mine.clone(), open, uploaded: true }, "mine.txt", to(), None);
    let ready = local(OutboxKind::Create, &mine, &to(), OutboxState::Ready);
    assert_eq!(detect(uploaded(false), &store(Vec::new())).detections, [quiet(ready.clone())]);
    assert_eq!(detect(uploaded(true), &store(Vec::new())).detections, [writing(ready)]);
}

/// Going up right now: it goes up as it is, and only where it stands changed.
#[test]
fn a_local_file_moved_while_it_is_uploaded_is_moved() {
    let mine = file(1, 3);
    let store = store(vec![local(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Running)]);
    let renamed = moved(MovedEntry::LocalFile { file: mine.clone(), open: false, uploaded: true }, "mine.txt", in_docs("mine.txt"), None);
    assert_eq!(kinds(renamed, &store), [pair(OutboxKind::Move, "#1")]);
}

/// Into a place that stays on this computer: its row goes, and is no longer sent.
#[test]
fn a_local_file_moved_where_nothing_is_uploaded_is_forgotten() {
    let mine = file(1, 3);
    let store = store(vec![local(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Ready)]);
    let all = rows(&store);
    let renamed = moved(MovedEntry::LocalFile { file: mine.clone(), open: false, uploaded: false }, "mine.txt", in_root("mine.tmp"), None);
    let batch = detect(renamed, &store);
    let [delete] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    assert_eq!((delete.kind, delete.object.as_ref(), delete.rel.as_path()), (OutboxKind::Delete, Some(&mine.handle), Path::new("mine.tmp")));
    assert_eq!(batch.ops, removes(&all.iter().collect::<Vec<_>>()));
}

/// Its rows that are being sent are left to finish.
#[test]
fn a_local_file_kept_here_while_it_is_uploaded_keeps_the_running_row() {
    let mine = file(1, 3);
    let store = store(vec![local(OutboxKind::Create, &mine, &in_root("mine.txt"), OutboxState::Running)]);
    let renamed = moved(MovedEntry::LocalFile { file: mine, open: false, uploaded: false }, "mine.txt", in_root("mine.tmp"), None);
    let batch = detect(renamed, &store);
    assert_eq!(batch.detections.len(), 1);
    assert!(batch.ops.is_empty());
}

#[test]
fn a_local_directory_moved_is_made_from_its_new_place_with_all_it_holds() {
    let (new_dir, inside) = (dir(1), file(2, 4));
    let child = FolderPlace::new(None, Path::new("docs/new"), "x.txt");
    let uploaded = vec![LocalEntry { place: in_docs("new"), object: new_dir.clone() }, LocalEntry { place: child.clone(), object: inside.clone() }];
    let batch = detect(moved(MovedEntry::LocalDir { handle: new_dir.handle.clone(), uploaded }, "new", in_docs("new"), None), &store(Vec::new()));
    let made = local(OutboxKind::Mkdir, &new_dir, &in_docs("new"), OutboxState::Ready);
    assert_eq!(batch.detections, [made, quiet(local(OutboxKind::Create, &inside, &child, OutboxState::Ready))]);
    assert_eq!(batch.ops, [OutboxOp::Rebase { from: rel("new"), to: rel("docs/new") }]);
}

/// Nothing of it is uploaded from the new place: its rows and those of the local objects
/// below it go; an item's row below it stays, and moves along.
#[test]
fn a_local_directory_moved_where_nothing_is_uploaded_takes_its_rows_out_of_the_outbox() {
    let (new_dir, inside) = (dir(1), file(2, 4));
    let store = store(vec![
        local(OutboxKind::Mkdir, &new_dir, &in_root("new"), OutboxState::Ready),
        local(OutboxKind::Create, &inside, &FolderPlace::new(None, Path::new("new"), "x.txt"), OutboxState::Ready),
        of(OutboxKind::Move, "A", "a.txt", "new/a.txt", (None, Some("a.txt"))),
    ]);
    let all = rows(&store);
    let renamed = moved(MovedEntry::LocalDir { handle: new_dir.handle.clone(), uploaded: Vec::new() }, "new", in_root("new.tmp"), None);
    let batch = detect(renamed, &store);
    assert_eq!(kinds_of(&batch), [pair(OutboxKind::Delete, "#1")]);
    let mut ops = removes(&all.iter().filter(|row| row.item_id.is_none()).collect::<Vec<_>>());
    ops.push(OutboxOp::Rebase { from: rel("new"), to: rel("new.tmp") });
    assert_eq!(batch.ops, ops);
}

fn kinds_of(batch: &crate::fuse::detections::OutboxBatch) -> Vec<(OutboxKind, String)> {
    batch.detections.iter().map(|d| (d.kind, d.item_id.clone().unwrap_or_else(|| format!("#{}", d.object.as_ref().map_or(0, |o| o.bytes[0]))))).collect()
}

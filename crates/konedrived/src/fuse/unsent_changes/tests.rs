use std::path::PathBuf;

use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::{Change, Kind, Placement, Row, TreeStore};

use super::{Above, UnsentChanges};

fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
    Row { id: id.into(), parent_id: parent.map(str::to_owned), name: name.into(), kind, size: 1, mtime: 0, etag: Some("e".into()), ctag: Some("c".into()), quickxor: None, mime: None, placement: Placement::Placed }
}

/// A drive with a folder `docs` and two files in its root.
fn store() -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    let changes = [
        Change::Root(row("R", None, "root", Kind::Folder)),
        Change::Upsert(row("D", Some("R"), "docs", Kind::Folder)),
        Change::Upsert(row("A", Some("R"), "a.txt", Kind::File)),
        Change::Upsert(row("B", Some("R"), "b.txt", Kind::File)),
    ];
    store.commit_page(&changes, "next").unwrap();
    store
}

/// A detection of item `id`, which the base has as `name` in the root.
fn of(kind: OutboxKind, id: &str, name: &str, rel: &str, to: (Option<&str>, Option<&str>)) -> Detection {
    Detection {
        kind,
        item_id: Some(id.into()),
        object: None,
        rel: PathBuf::from(rel),
        base: Some(Base { etag: Some("e".into()), ctag: Some("c".into()), parent: Some("R".into()), name: Some(name.into()) }),
        target_parent: to.0.map(str::to_owned),
        target_name: to.1.map(str::to_owned),
        same_content: kind == OutboxKind::Move,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

fn recorded(detections: Vec<Detection>) -> UnsentChanges {
    let mut store = store();
    let ops: Vec<OutboxOp> = detections.into_iter().map(OutboxOp::Record).collect();
    store.outbox_apply(&ops, 0).unwrap();
    UnsentChanges::of(&store)
}

/// An item the user moved stands in the folder it was moved to, under its new name, until
/// the move is sent.
#[test]
fn a_moved_item_stands_where_its_row_takes_it() {
    let unsent_changes = recorded(vec![of(OutboxKind::Move, "A", "a.txt", "docs/new.txt", (Some("D"), Some("new.txt")))]);
    assert_eq!(unsent_changes.moved.get("A"), Some(&(Above::Item("D".into()), "new.txt".to_owned())));
    assert!(unsent_changes.gone.is_empty() && unsent_changes.changed.is_empty());
}

/// Moved into a directory OneDrive has no item for yet: found by the directory's place.
#[test]
fn an_item_moved_into_a_new_directory_is_found_by_that_directorys_place() {
    let unsent_changes = recorded(vec![of(OutboxKind::Move, "A", "a.txt", "new/a.txt", (None, Some("a.txt")))]);
    assert_eq!(unsent_changes.moved.get("A"), Some(&(Above::Local(PathBuf::from("new")), "a.txt".to_owned())));
}

/// A removed item is gone from the folder though OneDrive still has it.
#[test]
fn a_removed_item_is_gone() {
    let unsent_changes = recorded(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    assert!(unsent_changes.gone.contains("B") && unsent_changes.moved.is_empty());
}

/// New content where the item is: the item's content is the user's own, and it has not moved.
#[test]
fn an_item_with_new_content_is_changed_and_stays_in_place() {
    let unsent_changes = recorded(vec![of(OutboxKind::Update, "A", "a.txt", "a.txt", (Some("R"), Some("a.txt")))]);
    assert!(unsent_changes.changed.contains("A") && unsent_changes.moved.is_empty() && unsent_changes.gone.is_empty());
}

#[test]
fn an_empty_outbox_changes_nothing() {
    let unsent_changes = UnsentChanges::of(&store());
    assert!(unsent_changes.moved.is_empty() && unsent_changes.gone.is_empty() && unsent_changes.changed.is_empty());
}

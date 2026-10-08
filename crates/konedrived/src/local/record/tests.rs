use std::path::PathBuf;

use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxState, Reason};
use konedrive_tree::{Change, Kind, Placement, Row, TreeStore};

use super::record;

fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
    Row { id: id.into(), parent_id: parent.map(str::to_owned), name: name.into(), kind, size: 1, mtime: 0, etag: Some("e".into()), ctag: Some("c".into()), quickxor: None, mime: None, placement: Placement::Placed }
}

/// A drive with `files` files in its root, `f0` and on.
fn store(files: usize) -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    let mut changes = vec![Change::Root(row("R", None, "root", Kind::Folder))];
    changes.extend((0..files).map(|i| Change::Upsert(row(&format!("f{i}"), Some("R"), &format!("{i}.txt"), Kind::File))));
    store.commit_page(&changes, "next").unwrap();
    store
}

fn detection(kind: OutboxKind, id: Option<&str>, name: &str) -> Detection {
    Detection {
        kind,
        item_id: id.map(str::to_owned),
        inode: None,
        rel: PathBuf::from(name),
        base: id.map(|_| Base { etag: Some("e".into()), ctag: Some("c".into()), parent: Some("R".into()), name: Some(name.into()) }),
        target_parent: (!kind.removes()).then(|| "R".to_owned()),
        target_name: (!kind.removes()).then(|| name.to_owned()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

fn delete(i: usize) -> Detection {
    detection(OutboxKind::Delete, Some(&format!("f{i}")), &format!("{i}.txt"))
}

fn states(store: &TreeStore) -> Vec<(OutboxState, Option<Reason>)> {
    store.outbox_rows().unwrap().into_iter().map(|r| (r.state, r.reason)).collect()
}

/// A few removals go to OneDrive as they are.
#[test]
fn a_few_removals_are_ready() {
    let mut store = store(40);
    let done = record(&mut store, (0..3).map(delete).collect(), Vec::new(), 0).unwrap();
    assert_eq!(done.held, 0);
    assert!(states(&store).iter().all(|(state, _)| *state == OutboxState::Ready));
}

/// Removing a large share of the folder at once is held until the user confirms it.
#[test]
fn a_large_share_removed_at_once_is_held() {
    let mut store = store(40);
    let done = record(&mut store, (0..10).map(delete).collect(), Vec::new(), 0).unwrap();
    assert_eq!(done.held, 10);
    assert!(states(&store).iter().all(|(state, reason)| *state == OutboxState::Held && *reason == Some(Reason::MassDelete)));
}

/// Removals that come one at a time, as a program removes a tree, add up: the one that
/// reaches the limit holds those still waiting too.
#[test]
fn removals_one_at_a_time_add_up() {
    let mut store = store(40);
    for i in 0..9 {
        assert_eq!(record(&mut store, vec![delete(i)], Vec::new(), 0).unwrap().held, 0);
    }
    assert_eq!(record(&mut store, vec![delete(9)], Vec::new(), 0).unwrap().held, 10);
    let states = states(&store);
    assert_eq!(states.len(), 10);
    assert!(states.iter().all(|(state, _)| *state == OutboxState::Held));
}

/// The row that frees a name in OneDrive is written before the row that takes it, in
/// whatever order they were seen.
#[test]
fn a_name_is_freed_before_it_is_taken() {
    let mut store = store(40);
    let takes = detection(OutboxKind::Create, None, "0.txt");
    record(&mut store, vec![takes, delete(0)], Vec::new(), 0).unwrap();
    let kinds: Vec<OutboxKind> = store.outbox_rows().unwrap().into_iter().map(|r| r.kind).collect();
    assert_eq!(kinds, [OutboxKind::Delete, OutboxKind::Create]);
}

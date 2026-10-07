use std::path::Path;

use konedrive_fs::handle::FileHandle;

use super::*;
use crate::outbox::{Base, Inode};
use crate::{Change, Placement, Row};

fn item(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
    Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: name.into(),
        kind,
        size: 0,
        mtime: 0,
        etag: Some(format!("e-{id}")),
        ctag: None,
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    }
}

fn store(items: &[Row]) -> TreeStore {
    let mut s = TreeStore::in_memory().unwrap();
    let mut changes = vec![Change::Root(item("R", None, "", Kind::Folder))];
    changes.extend(items.iter().cloned().map(Change::Upsert));
    s.begin_staging(crate::NewTree::Whole).unwrap();
    s.stage(&changes).unwrap();
    s.commit_staging("link").unwrap();
    s
}

fn object(n: u64) -> Inode {
    Inode { dev: 1, ino: n, handle: Some(FileHandle { kind: 1, bytes: n.to_le_bytes().to_vec() }) }
}

fn row(kind: OutboxKind, rel: &str, n: u64) -> OutboxRow {
    OutboxRow {
        seq: 0,
        kind,
        item_id: None,
        inode: Some(object(n)),
        rel: rel.into(),
        base: None,
        target_parent: None,
        target_name: Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()),
        state: OutboxState::Ready,
        reason: None,
        attempts: 0,
        next_try: None,
        snapshot: None,
        session_url: None,
        session_expires: None,
        session_next: None,
        confirmed: false,
        size: None,
    }
}

/// A row of item `of` (as the base has it) going to `rel` under `parent`.
fn of_item(kind: OutboxKind, of: &Row, rel: &str, parent: Option<&str>, n: u64) -> OutboxRow {
    OutboxRow {
        item_id: Some(of.id.clone()),
        base: Some(Base { etag: of.etag.clone(), ctag: None, parent: of.parent_id.clone(), name: Some(of.name.clone()) }),
        target_parent: parent.map(str::to_owned),
        target_name: if kind.removes() { None } else { Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()) },
        ..row(kind, rel, n)
    }
}

fn pick(s: &TreeStore, flying: &HashSet<i64>, now: i64) -> Picked {
    s.outbox_pick(&Pick { now, flying, move_outs: true, want: 32, allows: &|_: &TreeStore, _: &OutboxRow| Ok(true) }).unwrap()
}

fn seqs(picked: &Picked) -> Vec<i64> {
    picked.rows.iter().map(|r| r.seq).collect()
}

/// 1 000 files in a new folder whose `mkdir` came last: every portion
/// waits on it, and it is followed there and runs first; the files then.
#[test]
fn a_mkdir_at_the_end_of_the_queue_runs_first() {
    let mut s = store(&[]);
    let mut rows: Vec<OutboxRow> = (0..1000).map(|i| row(OutboxKind::Create, &format!("new/f{i:04}"), i)).collect();
    rows.push(OutboxRow { target_parent: Some("R".into()), ..row(OutboxKind::Mkdir, "new", 5000) });
    s.bench_insert(&rows).unwrap();
    let first = pick(&s, &HashSet::new(), 0);
    assert_eq!(seqs(&first), vec![1001], "only the mkdir");
    let flying = HashSet::from([1001]);
    let behind = pick(&s, &flying, 0);
    assert!(behind.rows.is_empty() && behind.running && behind.stalled.is_empty(), "{behind:?}");
    // The mkdir landed: the files go, in order.
    s.conn.execute("DELETE FROM outbox WHERE seq = 1001", []).unwrap();
    assert_eq!(seqs(&pick(&s, &HashSet::new(), 0))[..3], [1, 2, 3]);
}

/// A folder's removal waits for a row inside it that came later; followed,
/// that row runs first.
#[test]
fn a_folder_removal_waits_for_what_is_inside_it_even_later() {
    let d = item("D", Some("R"), "d", Kind::Folder);
    let x = item("X", Some("D"), "x", Kind::File);
    let e = item("E", Some("R"), "e", Kind::Folder);
    let mut s = store(&[d.clone(), x.clone(), e.clone()]);
    s.bench_insert(&[of_item(OutboxKind::Delete, &d, "d", None, 1), of_item(OutboxKind::Move, &x, "e/x", Some("E"), 2)]).unwrap();
    assert_eq!(s.checked_blockers(1).unwrap(), vec![2]);
    let picked = s.outbox_pick(&Pick { now: 0, flying: &HashSet::new(), move_outs: true, want: 1, allows: &|_: &TreeStore, _: &OutboxRow| Ok(true) }).unwrap();
    assert_eq!(seqs(&picked), vec![2]);
}

/// A swap is a circle of names alone: found in the graph of the rows that
/// free or take a name — not the queue's other rows — its edges go, and
/// both rows run.
#[test]
fn a_swap_among_many_rows_waits_on_nothing() {
    let a = item("A", Some("R"), "a", Kind::File);
    let b = item("B", Some("R"), "b", Kind::File);
    let mut s = store(&[a.clone(), b.clone()]);
    let mut rows: Vec<OutboxRow> =
        (0..500).map(|i| OutboxRow { target_parent: Some("R".into()), ..row(OutboxKind::Create, &format!("f{i:04}"), 100 + i) }).collect();
    rows.push(of_item(OutboxKind::Move, &a, "b", Some("R"), 1));
    rows.push(of_item(OutboxKind::Move, &b, "a", Some("R"), 2));
    s.bench_insert(&rows).unwrap();
    assert_eq!(name_edges(&s.conn).unwrap(), HashMap::new(), "the circle's edges are dropped");
    assert!(s.checked_blockers(501).unwrap().is_empty() && s.checked_blockers(502).unwrap().is_empty());
    let picked = s.outbox_pick(&Pick { now: 0, flying: &HashSet::new(), move_outs: true, want: 1000, allows: &|_: &TreeStore, _: &OutboxRow| Ok(true) }).unwrap();
    assert_eq!(picked.rows.len(), 502);
    // A later freer that is no circle is still waited for.
    let c = item("C", Some("R"), "c", Kind::File);
    let mut s = store(std::slice::from_ref(&c));
    s.bench_insert(&[OutboxRow { target_parent: Some("R".into()), ..row(OutboxKind::Create, "c", 7) }, of_item(OutboxKind::Delete, &c, "c", None, 8)])
        .unwrap();
    assert_eq!(s.checked_blockers(1).unwrap(), vec![2]);
}

/// Nothing can run: the pick says a row runs, a time comes, or the user is
/// needed — never nothing.
#[test]
fn nothing_runnable_says_why() {
    let mut s = store(&[]);
    let mkdir = |state: OutboxState, next_try: Option<i64>| OutboxRow {
        state,
        next_try,
        target_parent: Some("R".into()),
        reason: Some("because".into()),
        ..row(OutboxKind::Mkdir, "d", 1)
    };
    let file = row(OutboxKind::Create, "d/f", 2);
    s.bench_insert(&[mkdir(OutboxState::Ready, None), file.clone()]).unwrap();
    let running = pick(&s, &HashSet::from([1]), 0);
    assert!(running.rows.is_empty() && running.running && running.stalled.is_empty(), "{running:?}");

    let mut s = store(&[]);
    s.bench_insert(&[mkdir(OutboxState::Retry, Some(500)), file.clone()]).unwrap();
    let later = pick(&s, &HashSet::new(), 100);
    assert_eq!((later.rows.len(), later.until, later.stalled.len()), (0, Some(500), 0));
    assert_eq!(s.outbox_next_due(100).unwrap(), Some(500));

    for state in [OutboxState::Blocked, OutboxState::Held] {
        let mut s = store(&[]);
        s.bench_insert(&[mkdir(state, None), file.clone()]).unwrap();
        let user = pick(&s, &HashSet::new(), 0);
        assert!(user.rows.is_empty() && user.user && user.stalled.is_empty(), "{user:?}");
    }
    // Waiting for space: ready, and not allowed.
    let mut s = store(&[]);
    s.bench_insert(&[mkdir(OutboxState::Ready, None), file]).unwrap();
    let full = s.outbox_pick(&Pick { now: 0, flying: &HashSet::new(), move_outs: true, want: 32, allows: &|_: &TreeStore, _: &OutboxRow| Ok(false) }).unwrap();
    assert!(full.rows.is_empty() && full.user && full.stalled.is_empty(), "{full:?}");
}

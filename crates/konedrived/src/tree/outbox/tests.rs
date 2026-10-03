use super::*;
use crate::tree::Placement;

fn base_row(id: &str, parent: &str, name: &str, kind: Kind) -> Row {
    Row {
        id: id.into(),
        parent_id: Some(parent.into()),
        name: name.into(),
        kind,
        size: 3,
        mtime: 0,
        etag: Some(format!("e-{id}")),
        ctag: Some(format!("c-{id}")),
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    }
}

fn store(rows: &[Row]) -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    let mut changes = vec![Change::Root(Row { parent_id: None, name: String::new(), ..base_row("R", "", "", Kind::Folder) })];
    changes.extend(rows.iter().cloned().map(Change::Upsert));
    store.begin_staging(false).unwrap();
    store.stage(&changes).unwrap();
    store.commit_staging("link").unwrap();
    store
}

fn inode(n: u64) -> Inode {
    Inode { dev: 1, ino: n, handle: Some(FileHandle { kind: 1, bytes: n.to_le_bytes().to_vec() }) }
}

fn base_of(row: &Row) -> Base {
    Base { etag: row.etag.clone(), ctag: row.ctag.clone(), parent: row.parent_id.clone(), name: Some(row.name.clone()) }
}

fn detect(kind: OutboxKind, item: Option<&Row>, object: Option<Inode>, rel: &str, parent: Option<&str>) -> Detection {
    Detection {
        kind,
        item_id: item.map(|r| r.id.clone()),
        inode: object,
        rel: rel.into(),
        base: item.map(base_of),
        target_parent: parent.map(str::to_owned),
        target_name: Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

/// A store of the version before issue #89 — `upload_openings` without
/// `last`, the trigger that deleted a record with its row — brought up to
/// date: a record whose row leaves is kept without it, and its missing
/// `last` reads as its first time.
#[test]
fn an_older_stores_openings_are_upgraded() {
    let mut s = store(&[]);
    s.conn
        .execute_batch(
            "DROP TRIGGER upload_openings_left_behind; DROP TABLE upload_openings_left; DROP TABLE upload_openings;
                 CREATE TABLE upload_openings (seq INTEGER PRIMARY KEY, parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL);
                 CREATE TRIGGER upload_openings_leave AFTER DELETE ON outbox BEGIN DELETE FROM upload_openings WHERE seq = OLD.seq; END;",
        )
        .unwrap();
    let Recorded::Inserted(seq) = s.outbox_record(&detect(OutboxKind::Create, None, Some(inode(1)), "a.txt", Some("R"))).unwrap() else { panic!() };
    s.conn.execute("INSERT INTO upload_openings (seq, parent, name, at) VALUES (?1, 'R', 'a.txt', 100)", [seq]).unwrap();
    upgrade(&s.conn).unwrap();
    assert_eq!(s.upload_opening_windows("R", "a.txt").unwrap(), [(100, 100)]);
    let old_trigger: bool = s.conn.prepare("SELECT 1 FROM sqlite_master WHERE name = 'upload_openings_leave'").unwrap().exists([]).unwrap();
    assert!(!old_trigger);
    s.outbox_drop(seq, None, None, None).unwrap();
    let left: i64 = s.conn.query_row("SELECT COUNT(*) FROM upload_openings_left", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 1, "kept without its row");
    assert_eq!(s.upload_opening_windows("R", "A.TXT").unwrap(), [(100, 100)]);
}

fn kinds(store: &TreeStore) -> Vec<(OutboxKind, String)> {
    store.outbox_rows().unwrap().into_iter().map(|r| (r.kind, r.rel.display().to_string())).collect()
}

/// §3.5's coalescing table, row by row.
#[test]
fn detections_coalesce_into_one_live_row_per_item() {
    use OutboxKind::*;
    let a = base_row("A", "R", "a.txt", Kind::File);
    let d = base_row("D", "R", "d", Kind::Folder);
    let mut s = store(&[a.clone(), d.clone()]);

    // create + update / move → create, newest content at the newest place.
    s.outbox_record(&detect(Create, None, Some(inode(10)), "n.txt", Some("R"))).unwrap();
    s.outbox_record(&detect(Create, None, Some(inode(10)), "d/n.txt", Some("D"))).unwrap();
    assert_eq!(kinds(&s), vec![(Create, "d/n.txt".into())]);
    // create + delete → removed.
    assert!(matches!(s.outbox_record(&detect(Delete, None, Some(inode(10)), "d/n.txt", None)).unwrap(), Recorded::Removed(_)));
    assert!(kinds(&s).is_empty());

    // update + update → update; update + move → one row, move then content.
    s.outbox_record(&detect(Update, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap();
    s.outbox_record(&detect(Update, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap();
    s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/a.txt", Some("D"))).unwrap();
    let rows = s.outbox_rows().unwrap();
    assert_eq!((rows.len(), rows[0].kind, rows[0].target_parent.as_deref()), (1, Update, Some("D")));
    // update + delete → delete, the base of the update kept.
    s.outbox_record(&Detection { base: None, ..detect(Delete, Some(&a), None, "d/a.txt", None) }).unwrap();
    let row = &s.outbox_rows().unwrap()[0];
    assert_eq!((row.kind, row.base.as_ref().and_then(|b| b.etag.as_deref()), row.target_name.as_deref()), (Delete, Some("e-A"), None));
    // delete + a new file at the same name → update (save-by-rename).
    s.outbox_record(&detect(Update, Some(&a), Some(inode(2)), "a.txt", Some("R"))).unwrap();
    let row = &s.outbox_rows().unwrap()[0];
    assert_eq!((row.kind, row.inode.clone()), (Update, Some(inode(2))));
    // Checked and the same again: a move back to the base place → removed.
    s.outbox_record(&Detection { same_content: true, ..detect(Move, Some(&a), Some(inode(2)), "a.txt", Some("R")) }).unwrap();
    assert!(kinds(&s).is_empty());

    // move + move → one move to the final place; back where the base has it → removed.
    s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/a.txt", Some("D"))).unwrap();
    s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/b.txt", Some("D"))).unwrap();
    assert_eq!(kinds(&s), vec![(Move, "d/b.txt".into())]);
    s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap();
    assert!(kinds(&s).is_empty());
    // A move at the base place with no row is nothing at all.
    assert_eq!(s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap(), Recorded::Nothing);

    // move + delete → delete.
    s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/a.txt", Some("D"))).unwrap();
    s.outbox_record(&detect(Delete, Some(&a), None, "d/a.txt", None)).unwrap();
    assert_eq!(kinds(&s), vec![(Delete, "d/a.txt".into())]);
}

/// A detection never merges into a running row: one follow-up waits
/// behind it, and a create's follow-up learns the item id at commit.
#[test]
fn a_running_row_gets_one_follow_up() {
    use OutboxKind::*;
    let mut s = store(&[]);
    let Recorded::Inserted(first) = s.outbox_record(&detect(Create, None, Some(inode(7)), "n.txt", Some("R"))).unwrap() else { panic!() };
    s.outbox_set_state(first, OutboxState::Running, None, None).unwrap();
    // Where the running create takes it already: nothing new.
    assert_eq!(s.outbox_record(&detect(Move, None, Some(inode(7)), "n.txt", Some("R"))).unwrap(), Recorded::Nothing);
    // New content (the examination compares the snapshot): an update behind it.
    assert_eq!(s.outbox_record(&detect(Create, None, Some(inode(7)), "n.txt", Some("R"))).unwrap(), Recorded::Inserted(first + 1));
    assert_eq!(s.outbox_record(&detect(Create, None, Some(inode(7)), "n.txt", Some("R"))).unwrap(), Recorded::Merged(first + 1));
    let rows = s.outbox_rows().unwrap();
    assert_eq!(rows.iter().map(|r| (r.kind, r.state)).collect::<Vec<_>>(), vec![(Create, OutboxState::Running), (Update, OutboxState::Ready)]);
    assert_eq!(s.outbox_blockers(rows[1].seq).unwrap(), vec![first], "the follow-up waits for the running row");

    let committed = base_row("N", "R", "n.txt", Kind::File);
    let local_seq = s.outbox_commit(first, Committed::Item { row: &committed, handle: inode(7).handle.as_ref() }, None).unwrap();
    assert_eq!((local_seq, s.outbox_seq().unwrap()), (1, 1));
    let rows = s.outbox_rows().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].item_id.as_deref(), Some("N"), "the follow-up is now an update of the new item");
    assert_eq!(rows[0].base.as_ref().and_then(|b| b.etag.as_deref()), Some("e-N"));
    assert_eq!(s.item_by_handle(inode(7).handle.as_ref().unwrap()).unwrap().map(|r| r.id), Some("N".into()));
    assert!(s.outbox_blockers(rows[0].seq).unwrap().is_empty());

    // Behind a running update, the follow-up's base becomes what that
    // commit made: its If-Match is the new eTag.
    let n = base_row("N", "R", "n.txt", Kind::File);
    let running = rows[0].seq;
    s.outbox_set_state(running, OutboxState::Running, None, None).unwrap();
    s.outbox_record(&detect(Update, Some(&n), Some(inode(7)), "n.txt", Some("R"))).unwrap();
    let answered = Row { etag: Some("e-N2".into()), ..n.clone() };
    assert_eq!(s.outbox_commit(running, Committed::Item { row: &answered, handle: inode(7).handle.as_ref() }, None).unwrap(), 2);
    let rows = s.outbox_rows().unwrap();
    assert_eq!((rows.len(), rows[0].base.as_ref().and_then(|b| b.etag.as_deref())), (1, Some("e-N2")));
}

/// Rows run in detection order; a row waits for its parent's mkdir, and a
/// folder's delete waits for every row inside it, even a later one.
#[test]
fn rows_wait_for_their_parents_mkdir_and_a_folder_delete_for_what_is_inside() {
    use OutboxKind::*;
    let d = base_row("D", "R", "d", Kind::Folder);
    let x = base_row("X", "D", "x.txt", Kind::File);
    let y = base_row("Y", "R", "y.txt", Kind::File);
    let mut s = store(&[d.clone(), x.clone(), y.clone()]);
    let seq = |r: Recorded| match r {
        Recorded::Inserted(seq) | Recorded::Merged(seq) => seq,
        other => panic!("{other:?}"),
    };
    let delete_d = seq(s.outbox_record(&detect(Delete, Some(&d), None, "d", None)).unwrap());
    let mkdir = seq(s.outbox_record(&detect(Mkdir, None, Some(inode(20)), "new", Some("R"))).unwrap());
    let create = seq(s.outbox_record(&detect(Create, None, Some(inode(21)), "new/f.txt", None)).unwrap());
    // X left d for the new folder before d went: detected after d's delete.
    let move_x = seq(s.outbox_record(&detect(Move, Some(&x), Some(inode(22)), "new/x.txt", None)).unwrap());
    let update_y = seq(s.outbox_record(&detect(Update, Some(&y), Some(inode(23)), "y.txt", Some("R"))).unwrap());

    assert_eq!(s.outbox_blockers(create).unwrap(), vec![mkdir]);
    assert_eq!(s.outbox_blockers(move_x).unwrap(), vec![mkdir]);
    assert_eq!(s.outbox_blockers(delete_d).unwrap(), vec![move_x], "structural: a later row inside the folder");
    let runnable: Vec<i64> = s.outbox_runnable(0).unwrap().iter().map(|r| r.seq).collect();
    assert_eq!(runnable, vec![mkdir, update_y]);

    s.outbox_set_state(update_y, OutboxState::Retry, Some("503"), Some(100)).unwrap();
    assert_eq!(s.outbox_runnable(99).unwrap().iter().map(|r| r.seq).collect::<Vec<_>>(), vec![mkdir]);
    assert!(s.outbox_runnable(100).unwrap().iter().any(|r| r.seq == update_y), "its time has come");
    // A merge keeps the backoff.
    s.outbox_record(&detect(Update, Some(&y), Some(inode(23)), "y.txt", Some("R"))).unwrap();
    assert_eq!(s.outbox_row(update_y).unwrap().unwrap().state, OutboxState::Retry);
}

/// A swap (`a` and `b` exchanged) is a circle of names only: its waits
/// are dropped and both rows can run, the first through a temporary
/// name (§4.4). A name freed by a later row is still waited for.
#[test]
fn a_swap_waits_on_nothing_and_a_later_freer_is_still_waited_for() {
    use OutboxKind::*;
    let a = base_row("A", "R", "a", Kind::File);
    let b = base_row("B", "R", "b", Kind::File);
    let c = base_row("C", "R", "c", Kind::File);
    let mut s = store(&[a.clone(), b.clone(), c.clone()]);
    s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "b", Some("R"))).unwrap();
    s.outbox_record(&detect(Move, Some(&b), Some(inode(2)), "a", Some("R"))).unwrap();
    assert_eq!(s.outbox_runnable(0).unwrap().len(), 2);
    let Recorded::Inserted(create) = s.outbox_record(&detect(Create, None, Some(inode(3)), "c", Some("R"))).unwrap() else { panic!() };
    let Recorded::Inserted(delete) = s.outbox_record(&detect(Delete, Some(&c), None, "c", None)).unwrap() else { panic!() };
    assert_eq!(s.outbox_blockers(create).unwrap(), vec![delete]);
}

/// A directory that moves takes the rows inside it along.
#[test]
fn rows_follow_a_directory_that_moved() {
    use OutboxKind::*;
    let mut s = store(&[]);
    s.outbox_record(&detect(Mkdir, None, Some(inode(1)), "a", Some("R"))).unwrap();
    s.outbox_record(&detect(Create, None, Some(inode(2)), "a/f", None)).unwrap();
    s.outbox_record(&detect(Create, None, Some(inode(3)), "ab/g", None)).unwrap();
    s.outbox_apply(&[OutboxOp::Rebase { from: "a".into(), to: "b/a".into() }], 0).unwrap();
    let rels: Vec<String> = s.outbox_rows().unwrap().iter().map(|r| r.rel.display().to_string()).collect();
    assert_eq!(rels, vec!["a", "b/a/f", "ab/g"], "the directory's own row is its detection's to move, and ab is not under a");
}

/// The mass-delete guard's rows wait until confirmed; restoring drops them.
#[test]
fn held_deletes_wait_for_a_decision() {
    use OutboxKind::*;
    let a = base_row("A", "R", "a", Kind::File);
    let b = base_row("B", "R", "b", Kind::File);
    let mut s = store(&[a.clone(), b.clone()]);
    for item in [&a, &b] {
        s.outbox_record(&Detection { state: OutboxState::Held, reason: Some("mass-delete".into()), ..detect(Delete, Some(item), None, &item.name, None) })
            .unwrap();
    }
    assert!(s.outbox_runnable(0).unwrap().is_empty());
    // A new detection of the same delete keeps it held.
    s.outbox_record(&detect(Delete, Some(&a), None, "a", None)).unwrap();
    assert_eq!(s.outbox_rows().unwrap()[0].state, OutboxState::Held);
    assert_eq!(s.outbox_release_held().unwrap(), 2);
    assert_eq!(s.outbox_runnable(0).unwrap().len(), 2);
    s.outbox_set_state(1, OutboxState::Held, None, None).unwrap();
    for id in ["A", "B"] {
        s.set_local_handle(id, inode(1).handle.as_ref()).unwrap();
    }
    let dropped = s.outbox_drop_held().unwrap();
    assert_eq!(dropped.iter().map(|r| r.item_id.clone().unwrap()).collect::<Vec<_>>(), vec!["A".to_owned()]);
    assert_eq!(s.outbox_rows().unwrap().len(), 1);
    // Restored items forget their inode until they are placed again: no
    // examination can prove them deleted meanwhile.
    assert_eq!(s.local_handle("A").unwrap(), None);
    assert!(s.local_handle("B").unwrap().is_some());
}

/// A forced switch's drop keeps a rename half-done under a temporary
/// name — sent there, or the base has the item there — and drops the rest.
#[test]
fn a_forced_drop_keeps_a_rename_half_done() {
    use OutboxKind::*;
    let a = base_row("A", "R", "a.txt", Kind::File);
    let b = base_row("B", "R", "b.txt", Kind::File);
    let swapped = base_row("S", "R", &format!("{SWAP_PREFIX}1"), Kind::File);
    let mut s = store(&[a.clone(), b.clone(), swapped.clone()]);
    s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "b.txt", Some("R"))).unwrap();
    s.outbox_record(&detect(Move, Some(&swapped), Some(inode(2)), "c.txt", Some("R"))).unwrap();
    s.outbox_record(&detect(Update, Some(&b), Some(inode(3)), "b.txt", None)).unwrap();
    let sending = s.outbox_rows().unwrap().into_iter().find(|r| r.item_id.as_deref() == Some("A")).unwrap();
    s.outbox_set_target(sending.seq, Some("R"), Some(&format!("{SWAP_PREFIX}2"))).unwrap();
    let dropped = s.outbox_drop_all().unwrap();
    assert_eq!(dropped.iter().map(|r| r.item_id.clone().unwrap()).collect::<Vec<_>>(), vec!["B".to_owned()]);
    let mut kept: Vec<String> = s.outbox_rows().unwrap().into_iter().map(|r| r.item_id.unwrap()).collect();
    kept.sort();
    assert_eq!(kept, vec!["A".to_owned(), "S".to_owned()]);
}

/// the outbox on the bus: restoring held deletes forgets the items' local objects
/// for good, even with a cycle between staging and swap: its swap cannot
/// give them back.
#[test]
fn dropping_held_rows_survives_a_cycles_swap() {
    use OutboxKind::*;
    let d = base_row("D", "R", "d", Kind::Folder);
    let a = base_row("A", "D", "a", Kind::File);
    let mut s = store(&[d.clone(), a.clone()]);
    for id in ["D", "A"] {
        s.set_local_handle(id, inode(1).handle.as_ref()).unwrap();
    }
    s.outbox_record(&Detection { state: OutboxState::Held, ..detect(Delete, Some(&d), None, "d", None) }).unwrap();
    s.begin_staging(true).unwrap();
    assert_eq!(s.outbox_drop_held().unwrap().len(), 1);
    s.commit_staging("link-2").unwrap();
    assert_eq!((s.local_handle("D").unwrap(), s.local_handle("A").unwrap()), (None, None));
}

/// A kind or state no konedrive writes fails closed: blocked, never run.
#[test]
fn an_unreadable_row_is_blocked() {
    let mut s = store(&[]);
    let Recorded::Inserted(seq) = s.outbox_record(&detect(OutboxKind::Create, None, Some(inode(1)), "x", Some("R"))).unwrap() else { panic!() };
    s.conn.execute("UPDATE outbox SET kind = 'frobnicate' WHERE seq = ?1", [seq]).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!(row.state, OutboxState::Blocked);
    assert!(row.reason.unwrap().contains("frobnicate"));
    assert!(s.outbox_runnable(i64::MAX).unwrap().is_empty());
}

/// A name Linux allows and JSON cannot carry is still listed where it is.
#[test]
fn a_path_that_is_not_utf8_is_kept_as_it_is() {
    let mut s = store(&[]);
    let rel = PathBuf::from(OsStr::from_bytes(b"dir/caf\xe9.txt"));
    s.outbox_apply(&[OutboxOp::Skip { rel: rel.clone(), reason: "fifo".into(), size: 0 }], 5).unwrap();
    s.outbox_record(&Detection { rel: rel.clone(), ..detect(OutboxKind::Create, None, Some(inode(4)), "x", None) }).unwrap();
    s.outbox_apply(&[OutboxOp::Rebase { from: "dir".into(), to: "moved".into() }], 5).unwrap();
    assert_eq!(s.outbox_rows().unwrap()[0].rel, PathBuf::from(OsStr::from_bytes(b"moved/caf\xe9.txt")));
    assert_eq!(s.local_skipped().unwrap(), vec![LocalSkipped { rel: rel.clone(), reason: "fifo".into(), at: 5 }]);
    s.outbox_apply(&[OutboxOp::Skip { rel: rel.clone(), reason: "socket".into(), size: 0 }], 9).unwrap();
    assert_eq!(s.local_skipped().unwrap()[0].at, 5, "listed once, when first seen");
    s.outbox_apply(&[OutboxOp::Unskip(rel)], 9).unwrap();
    assert!(s.local_skipped().unwrap().is_empty());
}

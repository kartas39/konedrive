use std::ffi::OsStr;
use std::path::PathBuf;

use super::*;
use crate::Placement;

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
    store.begin_staging(crate::NewTree::Whole).unwrap();
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
    assert_eq!(s.checked_blockers(rows[1].seq).unwrap(), vec![first], "the follow-up waits for the running row");

    let committed = base_row("N", "R", "n.txt", Kind::File);
    let local_seq = s.outbox_commit(first, Committed::Item { row: &committed, handle: inode(7).handle.as_ref() }, None).unwrap();
    assert_eq!((local_seq, s.outbox_seq().unwrap()), (1, 1));
    let rows = s.outbox_rows().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].item_id.as_deref(), Some("N"), "the follow-up is now an update of the new item");
    assert_eq!(rows[0].base.as_ref().and_then(|b| b.etag.as_deref()), Some("e-N"));
    assert_eq!(s.item_by_handle(inode(7).handle.as_ref().unwrap()).unwrap().map(|r| r.id), Some("N".into()));
    assert!(s.checked_blockers(rows[0].seq).unwrap().is_empty());

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

    assert_eq!(s.checked_blockers(create).unwrap(), vec![mkdir]);
    assert_eq!(s.checked_blockers(move_x).unwrap(), vec![mkdir]);
    assert_eq!(s.checked_blockers(delete_d).unwrap(), vec![move_x], "structural: a later row inside the folder");
    let runnable: Vec<i64> = s.outbox_runnable(0).unwrap().iter().map(|r| r.seq).collect();
    assert_eq!(runnable, vec![mkdir, update_y]);

    s.outbox_set_state(update_y, OutboxState::Retry, Some(&"503".into()), Some(100)).unwrap();
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
    assert_eq!(s.checked_blockers(create).unwrap(), vec![delete]);
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

/// The outbox on the bus: restoring held deletes, and dropping a row for
/// what OneDrive decided, forget the item's local objects for good — the
/// item, what the base has below it, and what a delta staged meanwhile moves
/// or adds below it (`TR2`) — so that the cycle's swap gives none back.
#[test]
fn dropping_a_row_forgets_its_item_and_what_either_tree_has_below_it() {
    use OutboxKind::*;
    for restore in [true, false] {
        let d = base_row("D", "R", "d", Kind::Folder);
        let a = base_row("A", "D", "a", Kind::File);
        let t = base_row("T", "R", "t", Kind::File);
        let mut s = store(&[d.clone(), a.clone(), t.clone()]);
        for (id, n) in [("D", 1), ("A", 2), ("T", 3)] {
            s.set_local_handle(id, inode(n).handle.as_ref()).unwrap();
        }
        let state = if restore { OutboxState::Held } else { OutboxState::Ready };
        let Recorded::Inserted(seq) = s.outbox_record(&Detection { state, ..detect(Delete, Some(&d), None, "d", None) }).unwrap() else { panic!() };
        // A delta staged meanwhile moves `T` into `D` and adds `N` there.
        s.begin_staging(crate::NewTree::Delta).unwrap();
        s.stage(&[Change::Upsert(Row { parent_id: Some("D".into()), ..t.clone() }), Change::Upsert(base_row("N", "D", "n", Kind::File))]).unwrap();
        s.set_local_handle("N", inode(4).handle.as_ref()).unwrap();
        if restore {
            assert_eq!(s.outbox_drop_held().unwrap().len(), 1);
        } else {
            s.outbox_drop(seq, Some("D"), None).unwrap();
        }
        s.commit_staging("link-2").unwrap();
        for id in ["D", "A", "T", "N"] {
            assert_eq!(s.local_handle(id).unwrap(), None, "{id}, restore={restore}");
        }
    }
}

/// An upload's answer names its item as the folder cannot hold it (renamed
/// in OneDrive to a name too long while the content went up): the base keeps
/// the item where the disk has it, with the version just committed and its
/// object, and the answer waits as the item's deferred change, which this
/// commit does not supersede. An item the base did not place has no place to
/// keep: the answer is its row, with no object (I1).
#[test]
fn an_answer_the_folder_cannot_hold_waits_and_the_item_keeps_its_place() {
    use OutboxKind::*;
    let x = base_row("X", "R", "x", Kind::File);
    let answer = Row { name: "long".into(), ctag: Some("c2".into()), etag: Some("e2".into()), size: 9, placement: Placement::Skipped(crate::SkipReason::NameTooLong), ..x.clone() };
    let mut s = store(std::slice::from_ref(&x));
    let Recorded::Inserted(seq) = s.outbox_record(&detect(Update, Some(&x), Some(inode(7)), "x", Some("R"))).unwrap() else { panic!() };
    let handle = inode(7).handle.unwrap();
    let at = s.outbox_commit(seq, Committed::Item { row: &answer, handle: Some(&handle) }, None).unwrap();
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap(), Row { ctag: Some("c2".into()), etag: Some("e2".into()), size: 9, ..x.clone() }, "the version, at the base's place");
    assert_eq!(s.local_handle("X").unwrap(), Some(handle.clone()));
    assert_eq!(s.committed_since(at - 1).unwrap().get("X").map(|c| c.etag.clone()), Some(Some("e2".into())), "a commit as any other");
    assert_eq!(s.live_deferred().unwrap(), vec![Change::Upsert(answer.clone())], "the answer waits, and the commit leaves it");
    assert!(s.outbox_rows().unwrap().is_empty());

    // The next cycle stages what waits, and takes it: nothing is placed here.
    let staged = s.stage_rw(&[], at, false).unwrap().unwrap();
    assert!(staged.ids.contains(&"X".to_owned()));
    s.commit_staging_deferring("link-2", &crate::reconcile::Deferrals { consumed: &staged.consumed, whole: &[], content: &[], fetched_at: at, waits: &[] }).unwrap();
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap(), answer);
    assert_eq!(s.local_handle("X").unwrap(), None);

    // Not placed by the base already: it has no place to keep, and the
    // answer is its row.
    let Recorded::Inserted(seq) = s.outbox_record(&detect(Update, Some(&answer), Some(inode(7)), "x", Some("R"))).unwrap() else { panic!() };
    let again = Row { ctag: Some("c3".into()), ..answer.clone() };
    s.outbox_commit(seq, Committed::Item { row: &again, handle: Some(&handle) }, None).unwrap();
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap(), again);
    assert_eq!(s.local_handle("X").unwrap(), None, "no object for a row the base does not place");
    assert!(s.deferred_ids().unwrap().is_empty());
}

/// The same for an answer that names a folder the base does not place (the
/// item moved in OneDrive into the Personal Vault while its content went
/// up), and for a row behind the one committed: it was detected against the
/// place the disk has, and carries that place as its base, so that it sends
/// no name and no folder of OneDrive's side back. A move the user made into
/// a folder the base places is the base's row as before.
#[test]
fn an_answer_in_a_folder_the_base_does_not_place_waits_and_the_row_behind_keeps_the_place() {
    use OutboxKind::*;
    let d = base_row("D", "R", "d", Kind::Folder);
    let vault = Row { placement: Placement::Skipped(crate::SkipReason::PersonalVault), ..base_row("V", "R", "Personal Vault", Kind::Folder) };
    let x = base_row("X", "R", "x", Kind::File);
    let mut s = store(&[d.clone(), vault, x.clone()]);
    let Recorded::Inserted(seq) = s.outbox_record(&detect(Update, Some(&x), Some(inode(7)), "x", Some("R"))).unwrap() else { panic!() };
    s.outbox_claim(seq, OutboxState::Ready).unwrap();
    let Recorded::Inserted(behind) = s.outbox_record(&detect(Update, Some(&x), Some(inode(7)), "x", Some("R"))).unwrap() else { panic!() };
    let handle = inode(7).handle.unwrap();
    let answer = Row { parent_id: Some("V".into()), ctag: Some("c2".into()), etag: Some("e2".into()), ..x.clone() };
    s.outbox_commit(seq, Committed::Item { row: &answer, handle: Some(&handle) }, None).unwrap();
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap(), Row { ctag: Some("c2".into()), etag: Some("e2".into()), ..x.clone() });
    assert_eq!(s.local_handle("X").unwrap(), Some(handle.clone()), "still the item's object, where the disk has it");
    assert_eq!(s.live_deferred().unwrap(), vec![Change::Upsert(answer)]);
    let follower = s.outbox_row(behind).unwrap().unwrap();
    assert_eq!(follower.base, Some(Base { etag: Some("e2".into()), ctag: Some("c2".into()), parent: Some("R".into()), name: Some("x".into()) }));

    let moved = Row { parent_id: Some("D".into()), ..x.clone() };
    s.outbox_commit(behind, Committed::Item { row: &moved, handle: Some(&handle) }, None).unwrap();
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap(), moved, "a folder the base places is a place");
    assert!(s.live_deferred().unwrap().is_empty(), "and the commit supersedes what waited");

    // A rename the user made, sent as a name alone while OneDrive has the
    // item in the Vault: the kept place is where the object stands now —
    // the new name in the base's folder — and so is the base of the row
    // behind it. OneDrive's place is in the deferred change alone.
    let Recorded::Inserted(rename) = s.outbox_record(&detect(Move, Some(&moved), Some(inode(7)), "d/y", Some("D"))).unwrap() else { panic!() };
    s.outbox_claim(rename, OutboxState::Ready).unwrap();
    let Recorded::Inserted(behind) = s.outbox_record(&detect(Update, Some(&moved), Some(inode(7)), "d/y", Some("D"))).unwrap() else { panic!() };
    let renamed = Row { parent_id: Some("V".into()), name: "y".into(), etag: Some("e3".into()), ..x.clone() };
    s.outbox_commit(rename, Committed::Item { row: &renamed, handle: Some(&handle) }, None).unwrap();
    let kept = s.get(Table::Items, "X").unwrap().unwrap();
    assert_eq!((kept.parent_id.as_deref(), kept.name.as_str(), s.local_handle("X").unwrap()), (Some("D"), "y", Some(handle.clone())));
    assert_eq!(s.outbox_row(behind).unwrap().unwrap().base.map(|base| (base.parent, base.name)), Some((Some("D".into()), Some("y".into()))));
    assert_eq!(s.live_deferred().unwrap(), vec![Change::Upsert(renamed)]);
    s.outbox_drop(behind, None, None).unwrap();

    // A temporary step is a commit like any other in this: the item is
    // under its temporary name in the folder the object stands in, and the
    // move that follows is made against that — it sends a name, no folder.
    let y = s.get(Table::Items, "X").unwrap().unwrap();
    let Recorded::Inserted(swap) = s.outbox_record(&detect(Move, Some(&y), Some(inode(7)), "d/z", Some("D"))).unwrap() else { panic!() };
    let swapped = Row { parent_id: Some("V".into()), name: format!("{SWAP_PREFIX}X"), ..x.clone() };
    s.outbox_commit_temporary(swap, &swapped, Some(&handle), "D", "z", None).unwrap();
    let kept = s.get(Table::Items, "X").unwrap().unwrap();
    assert_eq!((kept.parent_id.as_deref(), kept.name.as_str(), s.local_handle("X").unwrap()), (Some("D"), swapped.name.as_str(), Some(handle)));
    let rows = s.outbox_rows().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].base.clone().map(|base| (base.parent, base.name)), Some((Some("D".into()), Some(swapped.name.clone()))));
    assert_eq!((rows[0].target_parent.as_deref(), rows[0].target_name.as_deref()), (Some("D"), Some("z")));
    assert_eq!(s.live_deferred().unwrap(), vec![Change::Upsert(swapped)]);
}

/// The item a bad upload left in OneDrive (quality finding `UP2`) is kept
/// through every change of the row's state and reason and through an
/// examination's merge, and goes only when it is cleared, or with the row.
#[test]
fn a_rows_bad_item_survives_a_settle_and_a_merge() {
    let mut s = store(&[]);
    let d = detect(OutboxKind::Create, None, Some(inode(1)), "a.txt", Some("R"));
    let Recorded::Inserted(seq) = s.outbox_record(&d).unwrap() else { panic!() };
    assert_eq!(s.outbox_bad_item(seq).unwrap(), None);
    let bad = BadItem::answered("BAD", Some("c-BAD"), Some("e-BAD"));
    assert_eq!((bad.ctag.as_deref(), bad.etag.as_deref()), (Some("c-BAD"), None), "the cTag alone when the answer has one");
    assert!(bad.still(Some("c-BAD"), Some("e-moved")) && !bad.still(Some("c-moved"), Some("e-BAD")));
    let by_etag = BadItem::answered("BAD", None, Some("e-BAD"));
    assert!(by_etag.still(Some("c"), Some("e-BAD")) && !by_etag.still(Some("c"), Some("e-moved")));
    s.outbox_set_bad_item(seq, Some(&bad)).unwrap();
    s.outbox_set_state(seq, OutboxState::Retry, Some(&"network".into()), Some(5)).unwrap();
    assert_eq!(s.outbox_record(&Detection { rel: "b.txt".into(), target_name: Some("b.txt".into()), state: OutboxState::Waiting, ..d.clone() }).unwrap(), Recorded::Merged(seq));
    s.outbox_amend(seq, |row| row.snapshot = Some(Snapshot::content(1, 0, 2))).unwrap();
    assert_eq!(s.outbox_bad_item(seq).unwrap(), Some(bad));
    s.outbox_set_bad_item(seq, None).unwrap();
    assert_eq!(s.outbox_bad_item(seq).unwrap(), None);

    assert_eq!(s.outbox_bad_item(seq + 1).unwrap(), None, "no such row");
}

#[test]
fn an_unreadable_row_is_blocked() {
    let mut s = store(&[]);
    let Recorded::Inserted(seq) = s.outbox_record(&detect(OutboxKind::Create, None, Some(inode(1)), "x", Some("R"))).unwrap() else { panic!() };
    s.conn.execute("UPDATE outbox SET kind = 'frobnicate' WHERE seq = ?1", [seq]).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!(row.state, OutboxState::Blocked);
    assert!(row.reason_text().unwrap().contains("frobnicate"));
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

/// A reason no variant spells, in the database, is read, kept through a
/// change of the row's state, counted and listed, as stored.
#[test]
fn a_row_with_a_reason_the_enum_does_not_know_is_kept() {
    let mut s = TreeStore::in_memory().unwrap();
    let d = Detection {
        kind: OutboxKind::Create,
        item_id: None,
        inode: None,
        rel: "a.txt".into(),
        base: None,
        target_parent: None,
        target_name: Some("a.txt".into()),
        same_content: false,
        state: OutboxState::Retry,
        reason: None,
        next_try: None,
        size: None,
    };
    s.outbox_apply(&[OutboxOp::Record(d), OutboxOp::Skip { rel: "odd".into(), reason: LocalSkip::Symlink, size: 0 }], 1).unwrap();
    s.conn.execute("UPDATE outbox SET reason = 'error sending request for url'", []).unwrap();
    s.conn.execute("UPDATE local_skipped SET reason = 'from-the-future'", []).unwrap();
    let row = s.outbox_rows().unwrap().remove(0);
    assert_eq!(row.reason, Some(Reason::Other("error sending request for url".into())));
    s.outbox_set_state(row.seq, OutboxState::Retry, row.reason.as_ref(), Some(7)).unwrap();
    let stored: String = s.conn.query_row("SELECT reason FROM outbox", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, "error sending request for url");
    assert_eq!(s.outbox_groups().unwrap()[0].reason(), row.reason);
    assert_eq!(s.local_skipped().unwrap()[0].reason, LocalSkip::Other("from-the-future".into()));
    let groups = s.skipped_groups().unwrap();
    assert_eq!(s.skipped_places_of(&[&groups[0].reason], 0).unwrap(), vec![("odd".into(), LocalSkip::Other("from-the-future".into()))]);
}

/// The rows that wait for space are found by the two spellings.
#[test]
fn the_rows_waiting_for_space_are_found_by_their_reasons() {
    let mut s = TreeStore::in_memory().unwrap();
    let create = |rel: &str, reason: Option<Reason>| {
        OutboxOp::Record(Detection {
            kind: OutboxKind::Create,
            item_id: None,
            inode: None,
            rel: rel.into(),
            base: None,
            target_parent: None,
            target_name: Some(rel.into()),
            same_content: false,
            state: OutboxState::Ready,
            reason,
            next_try: None,
            size: None,
        })
    };
    let ops = [create("a", Some(Reason::WaitingForSpace)), create("b", Some(Reason::TooBig(Some((9, 1))))), create("c", Some(Reason::Quota)), create("d", None)];
    s.outbox_apply(&ops, 1).unwrap();
    let waiting: Vec<String> = s.outbox_waiting_for_space().unwrap().into_iter().map(|r| r.rel.display().to_string()).collect();
    assert_eq!(waiting, ["a", "b"]);
    let stored: Vec<String> =
        s.conn.prepare("SELECT reason FROM outbox WHERE reason IS NOT NULL ORDER BY seq").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(stored, ["waiting-for-space", "too-big:9:1", "quota-exceeded"]);
}

/// A row leaves the outbox through `stored::remove` alone, which keeps the
/// record of the opening it made (issue #89): a `DELETE FROM outbox`
/// written anywhere else in the crate would leave that record pointing at
/// nothing. Read from the sources, tests aside.
#[test]
fn no_statement_but_removes_deletes_an_outbox_row() {
    fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "tests") {
                    sources(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") && path.file_name().is_some_and(|name| name != "tests.rs") {
                out.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(files.len() > 20, "the sources were found: {}", files.len());
    let mut deleting = Vec::new();
    for file in files {
        // Whitespace folded, so that a statement broken over lines is seen.
        let text = std::fs::read_to_string(&file).unwrap().split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        for (at, _) in text.match_indices("into outbox").chain(text.match_indices("from outbox")) {
            let before = &text[..at];
            let after = text[at + "from outbox".len()..].chars().next();
            let whole_name = !after.is_some_and(|c| c.is_alphanumeric() || c == '_');
            let removes = before.ends_with("delete ") || before.ends_with("replace ");
            if whole_name && removes {
                deleting.push(file.strip_prefix(&src).unwrap().to_owned());
            }
        }
    }
    deleting.sort();
    // And the step to version 8, which keeps the opening the same way.
    assert_eq!(deleting, [Path::new("outbox/stored.rs"), Path::new("schema/migrations.rs")], "the one delete is `remove`'s");
}

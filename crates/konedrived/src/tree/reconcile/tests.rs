use super::*;

fn file(id: &str, parent: &str, name: &str, ctag: &str) -> Row {
    Row {
        id: id.into(),
        parent_id: Some(parent.into()),
        name: name.into(),
        kind: Kind::File,
        size: 3,
        mtime: 0,
        etag: Some(format!("e-{ctag}")),
        ctag: Some(ctag.into()),
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    }
}

fn root() -> Row {
    Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed }
}

/// A deferred change waits in `deferred` while the base keeps its row, is
/// staged again later, and goes once an outbox commit after its fetch
/// supersedes it.
#[test]
fn a_deferred_change_waits_and_a_later_commit_supersedes_it() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(false).unwrap();
    s.stage(&[Change::Root(root()), Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
    s.commit_staging("L1").unwrap();

    s.begin_staging(true).unwrap();
    s.stage(&[Change::Upsert(file("X", "R", "x", "c2"))]).unwrap();
    s.commit_staging_deferring("L2", &[], &["X".to_owned()], &[], 5).unwrap();
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap().ctag.as_deref(), Some("c1"), "the base keeps the disk's version");
    assert_eq!(s.live_deferred().unwrap(), vec![Change::Upsert(file("X", "R", "x", "c2"))]);

    // A commit at 6 (after the fetch at 5): the deferred change is older.
    s.conn.execute("UPDATE items SET local_seq = 6 WHERE id = 'X'", []).unwrap();
    assert!(s.live_deferred().unwrap().is_empty());
    assert!(s.deferred_ids().unwrap().is_empty(), "dropped for good");
}

/// A replacement that landed with the deferred version makes it the base,
/// and records the new inode; another version leaves the base alone.
#[test]
fn a_landed_replacement_takes_its_deferred_version_into_the_base() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(false).unwrap();
    s.stage(&[Change::Root(root()), Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
    s.commit_staging("L1").unwrap();
    s.begin_staging(true).unwrap();
    s.stage(&[Change::Upsert(file("X", "R", "x", "c2"))]).unwrap();
    s.commit_staging_deferring("L2", &[], &["X".to_owned()], &[], 1).unwrap();

    assert!(!s.land_deferred("X", Some("c3"), None).unwrap(), "another version");
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap().ctag.as_deref(), Some("c1"));
    assert!(s.land_deferred("X", Some("c2"), None).unwrap());
    assert_eq!(s.get(Table::Items, "X").unwrap().unwrap().ctag.as_deref(), Some("c2"));
    assert!(s.deferred_ids().unwrap().is_empty());
}

/// Tombstones say what the outbox deleted after a commit count, and go
/// with the first cycle whose fetch started after them.
#[test]
fn a_tombstone_is_committed_since_until_a_later_fetch_commits() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(false).unwrap();
    s.stage(&[Change::Root(root())]).unwrap();
    s.commit_staging("L1").unwrap();
    {
        let tx = s.conn.transaction().unwrap();
        tombstone(&tx, &["X"], 4).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(s.committed_since(3).unwrap().get("X"), Some(&Committed { etag: None, gone: true }));
    assert!(s.committed_since(4).unwrap().is_empty());
    s.begin_staging(true).unwrap();
    s.commit_staging_deferring("L2", &[], &[], &[], 4).unwrap();
    assert!(s.committed_since(0).unwrap().is_empty(), "pruned");
}

fn folder(id: &str, parent: &str, name: &str) -> Row {
    Row { kind: Kind::Folder, size: 0, ..file(id, parent, name, "c") }
}

fn handle(n: u8) -> FileHandle {
    FileHandle { kind: 1, bytes: vec![n, n, n] }
}

/// Issue #104, decision 5: one call forgets the local objects of an item
/// and of everything below it — by `items` and by the new tree — in both
/// tables, and of whatever records one of the objects given.
#[test]
fn forgetting_an_item_forgets_everything_below_it_in_both_tables() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(false).unwrap();
    s.stage(&[Change::Root(root()), Change::Upsert(folder("D", "R", "d")), Change::Upsert(file("F", "D", "f", "c1")), Change::Upsert(file("T", "R", "t", "c1")), Change::Upsert(file("U", "R", "u", "c1"))]).unwrap();
    s.commit_staging("L1").unwrap();
    for (id, n) in [("D", 1), ("F", 2), ("T", 3), ("U", 4)] {
        s.set_local_handle(id, Some(&handle(n))).unwrap();
    }
    // A delta staged meanwhile moves `T` into `D`.
    s.begin_staging(true).unwrap();
    s.stage(&[Change::Upsert(file("T", "D", "t", "c1"))]).unwrap();
    s.forget_local_objects(&["D".to_owned()], &[handle(4)]).unwrap();
    for id in ["D", "F", "T", "U"] {
        assert_eq!(s.local_handle(id).unwrap(), None, "{id} in items");
        let staged: Option<Vec<u8>> = s.conn.query_row("SELECT local_handle FROM staging WHERE id = ?1", [id], |r| r.get(0)).optional().unwrap().flatten();
        assert_eq!(staged, None, "{id} in staging");
    }
    s.commit_staging("L2").unwrap();
    assert_eq!(s.local_handle("T").unwrap(), None, "the swap gives none back");
}

/// Third review, point 5: leaving an object is dropped whole — its row
/// and the items remembered with it, in one transaction.
#[test]
fn dropping_what_is_leaving_drops_its_items_with_it() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(false).unwrap();
    s.stage(&[Change::Root(root()), Change::Upsert(folder("D", "R", "d")), Change::Upsert(file("F", "D", "f", "c1"))]).unwrap();
    s.commit_staging("L1").unwrap();
    s.leaving_add("D", std::path::Path::new("d"), None).unwrap();
    assert!(s.leaving_had("F").unwrap());
    // A failure between the two statements leaves both tables as they
    // were: the row and its items go together or not at all.
    s.conn.execute_batch("CREATE TEMP TRIGGER fail_items BEFORE DELETE ON leaving_items BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    assert!(s.leaving_drop("D").is_err());
    assert_eq!(s.leaving().unwrap().len(), 1, "the leaving row is still there");
    assert!(s.leaving_had("F").unwrap());
    s.conn.execute_batch("DROP TRIGGER fail_items;").unwrap();
    s.leaving_drop("D").unwrap();
    assert!(s.leaving().unwrap().is_empty());
    assert!(!s.leaving_had("F").unwrap() && !s.leaving_had("D").unwrap());
}

/// Review fix 7 of issue #104: many subtree roots and handles at once —
/// thousands, some unknown — are forgotten together, each subtree whole.
#[test]
fn forgetting_takes_many_roots_and_handles_at_once() {
    let mut s = TreeStore::in_memory().unwrap();
    let mut changes = vec![Change::Root(root())];
    for n in 0..300 {
        changes.push(Change::Upsert(folder(&format!("D{n}"), "R", &format!("d{n}"))));
        changes.push(Change::Upsert(file(&format!("F{n}"), &format!("D{n}"), "f", "c1")));
    }
    changes.push(Change::Upsert(file("K", "R", "k", "c1")));
    s.begin_staging(false).unwrap();
    s.stage(&changes).unwrap();
    s.commit_staging("L1").unwrap();
    for n in 0..300u16 {
        s.set_local_handle(&format!("F{n}"), Some(&FileHandle { kind: 1, bytes: n.to_be_bytes().to_vec() })).unwrap();
    }
    s.set_local_handle("K", Some(&handle(200))).unwrap();
    let mut roots: Vec<String> = (0..300).map(|n| format!("D{n}")).collect();
    roots.extend((0..2000).map(|n| format!("unknown-{n}")));
    s.forget_local_objects(&roots, &[handle(200), handle(201)]).unwrap();
    for n in [0, 150, 299] {
        assert_eq!(s.local_handle(&format!("F{n}")).unwrap(), None);
    }
    assert_eq!(s.local_handle("K").unwrap(), None, "by its handle");
}

/// Issue #104, decision 5: a row that turns placed again over an `items`
/// row that is not placed carries no local object — staged by a delta,
/// swapped in whole, landed from what waited, or applied by a folder
/// turned read-only.
#[test]
fn a_row_placed_again_carries_no_local_object() {
    let skipped = || Row { placement: Placement::Skipped(super::super::SkipReason::NameTooLong), ..file("X", "R", "long", "c1") };
    let base = || {
        let mut s = TreeStore::in_memory().unwrap();
        s.begin_staging(false).unwrap();
        s.stage(&[Change::Root(root()), Change::Upsert(skipped())]).unwrap();
        s.commit_staging("L1").unwrap();
        s.set_local_handle("X", Some(&handle(9))).unwrap();
        s
    };
    // A delta.
    let mut s = base();
    s.begin_staging(true).unwrap();
    s.stage(&[Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
    assert_eq!(s.unplaced(Table::Staging).unwrap(), vec!["X".to_owned()], "placed again, with no object");
    s.commit_staging("L2").unwrap();
    assert_eq!(s.local_handle("X").unwrap(), None);
    // A full listing.
    let mut s = base();
    s.begin_staging(false).unwrap();
    s.stage(&[Change::Root(root()), Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
    s.commit_staging("L2").unwrap();
    assert_eq!(s.local_handle("X").unwrap(), None);
    // What waited, applied by a folder turned read-only, or landed.
    for land in [false, true] {
        let mut s = base();
        s.begin_staging(true).unwrap();
        s.stage(&[Change::Upsert(file("X", "R", "x", "c2"))]).unwrap();
        s.commit_staging_deferring("L2", &[], &["X".to_owned()], &[], 1).unwrap();
        s.set_local_handle("X", Some(&handle(9))).unwrap();
        if land {
            assert!(s.land_deferred("X", Some("c2"), None).unwrap());
        } else {
            s.apply_deferred().unwrap();
        }
        assert_eq!(s.get(Table::Items, "X").unwrap().unwrap().placement, Placement::Placed, "land={land}");
        assert_eq!(s.local_handle("X").unwrap(), None, "land={land}");
    }
    // A row that stays placed keeps its object.
    let mut s = base();
    s.begin_staging(true).unwrap();
    s.stage(&[Change::Upsert(Row { name: "longer".into(), ..skipped() })]).unwrap();
    s.commit_staging("L2").unwrap();
    assert_eq!(s.local_handle("X").unwrap(), Some(handle(9)), "not placed again: as it was");
}


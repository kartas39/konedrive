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
    s.begin_staging(crate::NewTree::Whole).unwrap();
    s.stage(&[Change::Root(root()), Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
    s.commit_staging("L1", &[]).unwrap();

    s.begin_staging(crate::NewTree::Delta).unwrap();
    s.stage(&[Change::Upsert(file("X", "R", "x", "c2"))]).unwrap();
    s.commit_staging_deferring("L2", &Deferrals { consumed: &[], whole: &["X".to_owned()], content: &[], fetched_at: 5, waits: &[], copies_on_disk: &[] }).unwrap();
    assert_eq!(s.get(Table::OneDriveItems, "X").unwrap().unwrap().ctag.as_deref(), Some("c1"), "the base keeps the disk's version");
    assert_eq!(s.live_deferred().unwrap(), vec![Change::Upsert(file("X", "R", "x", "c2"))]);

    // A commit at 6 (after the fetch at 5): the deferred change is older.
    s.conn.execute("UPDATE onedrive_items SET local_seq = 6 WHERE id = 'X'", []).unwrap();
    assert!(s.live_deferred().unwrap().is_empty());
    assert!(s.deferred_ids().unwrap().is_empty(), "dropped for good");
}

/// Tombstones say what the outbox deleted after a commit count, and go
/// with the first cycle whose fetch started after them.
#[test]
fn a_tombstone_is_committed_since_until_a_later_fetch_commits() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(crate::NewTree::Whole).unwrap();
    s.stage(&[Change::Root(root())]).unwrap();
    s.commit_staging("L1", &[]).unwrap();
    {
        let tx = s.conn.transaction().unwrap();
        tombstone(&tx, &["X"], 4).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(s.committed_since(3).unwrap().get("X"), Some(&Committed { etag: None, gone: true }));
    assert!(s.committed_since(4).unwrap().is_empty());
    s.begin_staging(crate::NewTree::Delta).unwrap();
    s.commit_staging_deferring("L2", &Deferrals { consumed: &[], whole: &[], content: &[], fetched_at: 4, waits: &[], copies_on_disk: &[] }).unwrap();
    assert!(s.committed_since(0).unwrap().is_empty(), "pruned");
}

/// A read-write cycle's swap is all or nothing: when it fails after the
/// deferrals are written, the deferred changes it staged still wait, and
/// the next cycle brings them into the base. The delta cursor never sends
/// them again.
#[test]
fn a_swap_that_fails_keeps_the_deferred_changes_it_consumed() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(crate::NewTree::Whole).unwrap();
    s.stage(&[Change::Root(root()), Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
    s.commit_staging("L1", &[]).unwrap();
    // A cycle defers the new version of `X`: the link moves on past it.
    s.begin_staging(crate::NewTree::Delta).unwrap();
    s.stage(&[Change::Upsert(file("X", "R", "x", "c2"))]).unwrap();
    s.commit_staging_deferring("L2", &Deferrals { consumed: &[], whole: &["X".to_owned()], content: &[], fetched_at: 1, waits: &[], copies_on_disk: &[] }).unwrap();
    assert_eq!(s.deferred_ids().unwrap(), vec!["X".to_owned()]);

    // The next cycle stages it again, the folder takes it, and the swap
    // fails (a full disk, a crash) once the deferrals are written.
    let RwStaged { ids, consumed } = s.stage_rw(&[], 0, true).unwrap().unwrap();
    assert_eq!((ids, consumed.clone()), (vec!["X".to_owned()], vec!["X".to_owned()]));
    s.conn.execute_batch("CREATE TEMP TRIGGER fail_swap BEFORE INSERT ON onedrive_items BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    assert!(s.commit_staging_deferring("L3", &Deferrals { consumed: &consumed, whole: &[], content: &[], fetched_at: 1, waits: &[], copies_on_disk: &[] }).is_err());
    s.conn.execute_batch("DROP TRIGGER fail_swap;").unwrap();
    assert_eq!(s.delta_link().unwrap().as_deref(), Some("L2"), "the link stays");
    assert_eq!(s.get(Table::OneDriveItems, "X").unwrap().unwrap().ctag.as_deref(), Some("c1"), "the base stays");
    assert_eq!(s.deferred_ids().unwrap(), vec!["X".to_owned()], "what the failed swap consumed still waits");

    // The cycle after it: the same link answers with nothing new.
    let RwStaged { consumed, .. } = s.stage_rw(&[], 0, true).unwrap().unwrap();
    s.commit_staging_deferring("L3", &Deferrals { consumed: &consumed, whole: &[], content: &[], fetched_at: 1, waits: &[], copies_on_disk: &[] }).unwrap();
    assert_eq!(s.get(Table::OneDriveItems, "X").unwrap().unwrap().ctag.as_deref(), Some("c2"), "the base has the version OneDrive has");
}

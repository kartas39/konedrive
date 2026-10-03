use serde_json::json;

use super::*;

/// Issue #38: while a long job holds the store's thread, tasks waiting for
/// the store — more than the runtime has workers — hold up no other task.
#[test]
fn a_long_job_does_not_starve_the_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let store = Store::new(TreeStore::in_memory().unwrap());
    let (held, release) = std::sync::mpsc::channel();
    let holder = store.clone();
    let holding = std::thread::spawn(move || {
        holder.call_blocking(move |_| {
            held.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1500));
            Ok(())
        })
    });
    release.recv().unwrap();
    runtime.block_on(async {
        let waiting: Vec<_> = (0..4)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move { store.call(move |s| s.meta("x")).await })
            })
            .collect();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let start = std::time::Instant::now();
        let ran = tokio::spawn(async move { start.elapsed() }).await.unwrap();
        assert!(ran < std::time::Duration::from_millis(500), "a task waited {ran:?} behind the store");
        for task in waiting {
            task.await.unwrap().unwrap();
        }
    });
    holding.join().unwrap().unwrap();
}

/// Jobs run one at a time, in the order they arrive.
#[test]
fn jobs_run_in_arrival_order() {
    let store = Store::new(TreeStore::in_memory().unwrap());
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let calls: Vec<_> = (0..50)
            .map(|n| {
                let order = std::sync::Arc::clone(&order);
                store.call(move |_| {
                    order.lock().unwrap().push(n);
                    Ok(())
                })
            })
            .collect();
        for call in calls {
            call.await.unwrap();
        }
    });
    assert_eq!(*order.lock().unwrap(), (0..50).collect::<Vec<_>>());
}

/// A job that panics answers its caller with an error; the store's thread
/// goes on with the next job, and the panicking job's transaction is gone.
#[test]
fn a_panicking_job_is_an_error_and_the_next_job_runs() {
    let store = Store::new(TreeStore::in_memory().unwrap());
    let failed = store.call_blocking(|s| -> Result<(), TreeError> {
        let tx = s.conn.transaction()?;
        tx.execute("INSERT INTO meta (key, value) VALUES ('half', 'done')", [])?;
        panic!("a job's bug");
    });
    assert!(failed.is_err());
    assert_eq!(store.call_blocking(|s| s.meta("half")).unwrap(), None, "rolled back");
    store.call_blocking(|s| s.set_meta("after", Some("yes"))).unwrap();
    assert_eq!(store.call_blocking(|s| s.meta("after")).unwrap().as_deref(), Some("yes"));
}

/// A job that calls the store would wait for itself: caught (a panic in
/// tests, so the job fails), and the store goes on.
#[test]
fn a_call_from_inside_a_job_is_caught() {
    let store = Store::new(TreeStore::in_memory().unwrap());
    let inner = store.clone();
    let nested = store.call_blocking(move |_| inner.call_blocking(|s| s.meta("x")));
    assert!(nested.is_err());
    assert_eq!(store.call_blocking(|s| s.meta("x")).unwrap(), None, "the store still answers");
}

/// The last clone gone, the store's thread ends and closes its connection:
/// what its jobs wrote is there for the next open.
#[test]
fn the_store_finishes_its_queue_when_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    {
        let store = Store::new(TreeStore::open(&path).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let queued = store.call(|s| s.set_meta("queued", Some("kept")));
        let written = runtime.block_on(queued);
        written.unwrap();
    }
    assert_eq!(TreeStore::open(&path).unwrap().meta("queued").unwrap().as_deref(), Some("kept"));
}

impl TreeStore {
    /// Rows in `staging` and `staging_gone` themselves.
    fn staged_rows(&self) -> i64 {
        self.conn.query_row("SELECT (SELECT count(*) FROM staging) + (SELECT count(*) FROM staging_gone)", [], |r| r.get(0)).unwrap()
    }
}

fn item(value: serde_json::Value) -> DriveItem {
    serde_json::from_value(value).unwrap()
}

fn folder(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

fn file(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind: Kind::File, size: 1, mtime: 0, etag: None, ctag: Some(format!("c-{id}")), quickxor: None, mime: None, placement: Placement::Placed })
}

fn root() -> Change {
    Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

fn committed(changes: &[Change]) -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    store.begin_staging(false).unwrap();
    store.stage(changes).unwrap();
    store.commit_staging("link-1").unwrap();
    store
}

#[test]
fn a_file_item_becomes_a_placed_file_row() {
    let change = classify(&item(json!({
        "id": "F", "name": "a.jpg", "size": 7, "eTag": "e", "cTag": "c",
        "parentReference": {"id": "R"},
        "file": {"mimeType": "image/jpeg", "hashes": {"quickXorHash": "q"}},
        "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
    })));
    let Change::Upsert(row) = change else { panic!("{change:?}") };
    assert_eq!((row.kind, row.size, row.mtime, row.placement), (Kind::File, 7, 1_714_557_600, Placement::Placed));
    assert_eq!((row.ctag.as_deref(), row.quickxor.as_deref(), row.mime.as_deref()), (Some("c"), Some("q"), Some("image/jpeg")));
    assert_eq!(row.parent_id.as_deref(), Some("R"));
}

#[test]
fn the_root_deletions_and_folders_are_told_apart() {
    assert!(matches!(classify(&item(json!({"id": "R", "root": {}, "folder": {}}))), Change::Root(_)));
    assert_eq!(classify(&item(json!({"id": "X", "deleted": {"state": "deleted"}}))), Change::Delete("X".into()));
    let Change::Upsert(row) = classify(&item(json!({"id": "D", "name": "d", "size": 999, "folder": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!((row.kind, row.size), (Kind::Folder, 0), "a folder's size is its content's, not a file size");
}

#[test]
fn every_skip_reason_is_recognised() {
    let cases = [
        (json!({"id": "1", "name": "я".repeat(128), "file": {}, "parentReference": {"id": "R"}}), SkipReason::NameTooLong),
        (json!({"id": "2", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}), SkipReason::PersonalVault),
        (json!({"id": "3", "name": "shared", "folder": {}, "remoteItem": {"id": "x"}, "parentReference": {"id": "R"}}), SkipReason::Shared),
        (json!({"id": "4", "name": "Notes", "package": {"type": "oneNote"}, "parentReference": {"id": "R"}}), SkipReason::OneNote),
        (json!({"id": "5", "name": ".konedrive-holding", "folder": {}, "parentReference": {"id": "R"}}), SkipReason::ReservedName),
        (json!({"id": "6", "name": "odd", "parentReference": {"id": "R"}}), SkipReason::Unsupported),
        (json!({"id": "7", "name": "..", "file": {}, "parentReference": {"id": "R"}}), SkipReason::Unsupported),
    ];
    for (value, reason) in cases {
        let Change::Upsert(row) = classify(&item(value.clone())) else { panic!("{value}") };
        assert_eq!(row.placement, Placement::Skipped(reason), "{value}");
    }
    let Change::Upsert(fits) = classify(&item(json!({"id": "8", "name": "я".repeat(127), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(fits.placement, Placement::Placed, "127 Cyrillic letters are 254 bytes and fit");
}

#[test]
fn the_255_byte_name_is_the_exact_boundary() {
    let Change::Upsert(exact) = classify(&item(json!({"id": "9", "name": "a".repeat(255), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(exact.placement, Placement::Placed, "255 bytes is Linux's limit, inclusive");
    let Change::Upsert(over) = classify(&item(json!({"id": "10", "name": "a".repeat(256), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(over.placement, Placement::Skipped(SkipReason::NameTooLong), "256 bytes is one over");
}

/// An item id becomes a name in the holding directory: an id that cannot
/// be one keeps the item out of the folder.
#[test]
fn an_id_that_cannot_be_a_file_name_is_not_placed() {
    for id in ["", ".", "..", "a/b", "a\0b"] {
        let Change::Upsert(row) = classify(&item(json!({"id": id, "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!("{id:?}") };
        assert_eq!(row.placement, Placement::Skipped(SkipReason::Unsupported), "{id:?}");
    }
    let Change::Upsert(fine) = classify(&item(json!({"id": "8F6C!101", "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(fine.placement, Placement::Placed);
}

#[test]
fn staged_rows_are_invisible_until_committed() {
    let mut store = TreeStore::in_memory().unwrap();
    store.begin_staging(false).unwrap();
    store.stage(&[root(), file("A", "R", "a")]).unwrap();
    assert!(store.get(Table::Items, "A").unwrap().is_none());
    assert!(store.get(Table::Staging, "A").unwrap().is_some());
    store.commit_staging("link-1").unwrap();
    assert!(store.get(Table::Items, "A").unwrap().is_some());
    assert_eq!(store.staged_rows(), 0, "the staged rows are gone into items");
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
    assert_eq!(store.root_item_id().unwrap().as_deref(), Some("R"));
}

/// A page of a first listing goes into `items` as it was placed,
/// with the link to the page after it, in one transaction. `staging` is
/// not written, and a deletion takes what is inside the folder, as it
/// does in `staging`.
#[test]
fn a_placed_page_is_committed_with_where_the_listing_goes_on() {
    let mut store = TreeStore::in_memory().unwrap();
    store.commit_page(&[root(), folder("D", "R", "docs"), file("F", "D", "f")], "next-2").unwrap();
    assert!(store.get(Table::Items, "F").unwrap().is_some());
    assert_eq!(store.listing_next().unwrap().as_deref(), Some("next-2"));
    assert_eq!(store.root_item_id().unwrap().as_deref(), Some("R"));
    assert_eq!(store.delta_link().unwrap(), None, "the listing has not ended");

    store.commit_page(&[Change::Delete("D".into()), file("G", "R", "g")], "next-3").unwrap();
    assert!(store.get(Table::Items, "D").unwrap().is_none());
    assert!(store.get(Table::Items, "F").unwrap().is_none(), "what was inside the deleted folder goes with it");
    assert!(store.get(Table::Items, "G").unwrap().is_some());
    assert_eq!(store.listing_next().unwrap().as_deref(), Some("next-3"));
    assert_eq!(store.staged_rows(), 0, "staging is not written");
}

/// An entry whose folder has not come yet is committed all the same, not
/// reaching the root: it is the only record of it once its page is
/// committed, and it is placed when its folder comes.
#[test]
fn an_entry_whose_folder_has_not_come_survives_a_page_commit() {
    let mut store = TreeStore::in_memory().unwrap();
    store.commit_page(&[root(), file("C", "P", "c")], "next-2").unwrap();
    assert!(store.get(Table::Items, "C").unwrap().is_some(), "kept, though nowhere yet");
    assert_eq!(store.locate(Table::Items, "C").unwrap(), None);
    store.commit_page(&[folder("P", "R", "p")], "next-3").unwrap();
    assert_eq!(store.locate(Table::Items, "C").unwrap(), Some(Located { rel: "p/c".into(), placed: true, depth: 2 }));
}

/// A first listing placed page by page is under way from the moment it
/// begins, before anything of it is placed: at the start until its first
/// page is committed.
#[test]
fn a_listing_begun_is_at_the_start_until_its_first_page() {
    let mut store = TreeStore::in_memory().unwrap();
    store.begin_placing().unwrap();
    assert_eq!(store.listing_next().unwrap().as_deref(), Some(""));
    store.commit_page(&[root()], "next-2").unwrap();
    assert_eq!(store.listing_next().unwrap().as_deref(), Some("next-2"));
}

/// The swap that ends every listing ends one placed page by page too: the
/// delta link comes and the place in the listing goes, together.
#[test]
fn the_swap_ends_a_listing_placed_page_by_page() {
    let mut store = TreeStore::in_memory().unwrap();
    store.commit_page(&[root(), file("A", "R", "a")], "next-2").unwrap();
    store.begin_staging(true).unwrap();
    store.stage(&[file("B", "R", "b")]).unwrap();
    store.commit_staging("link-1").unwrap();
    assert_eq!(store.listing_next().unwrap(), None);
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
    assert!(store.get(Table::Items, "A").unwrap().is_some());
    assert!(store.get(Table::Items, "B").unwrap().is_some());
}

#[test]
fn a_table_is_empty_until_a_row_is_in_it() {
    let mut store = TreeStore::in_memory().unwrap();
    assert!(store.is_empty(Table::Items).unwrap());
    store.begin_staging(false).unwrap();
    store.stage(&[root()]).unwrap();
    assert!(store.is_empty(Table::Items).unwrap(), "a staged row is not in items");
    assert!(!store.is_empty(Table::Staging).unwrap());
    store.commit_staging("link-1").unwrap();
    assert!(!store.is_empty(Table::Items).unwrap());
}

#[test]
fn a_path_is_the_chain_of_names_and_placed_only_if_every_link_is() {
    let store = committed(&[
        root(),
        folder("D", "R", "docs"),
        file("F", "D", "f.txt"),
        Change::Upsert(Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..match folder("L", "R", "long") { Change::Upsert(r) => r, _ => unreachable!() } }),
        file("G", "L", "g.txt"),
        file("O", "missing-parent", "o.txt"),
    ]);
    assert_eq!(store.locate(Table::Items, "F").unwrap(), Some(Located { rel: "docs/f.txt".into(), placed: true, depth: 2 }));
    assert_eq!(store.locate(Table::Items, "R").unwrap(), Some(Located { rel: "".into(), placed: true, depth: 0 }));
    assert_eq!(store.locate(Table::Items, "G").unwrap().unwrap().placed, false, "inside a skipped folder");
    assert_eq!(store.locate(Table::Items, "O").unwrap(), None, "an orphan is nowhere");
}

#[test]
fn deleting_a_folder_takes_what_is_still_inside_it() {
    let mut store = committed(&[root(), folder("D", "R", "d"), folder("E", "D", "e"), file("F", "E", "f"), file("K", "D", "keep")]);
    store.begin_staging(true).unwrap();
    // K moves out, then D goes — in the other order too, in the next test.
    store.stage(&[file("K", "R", "keep"), Change::Delete("D".into())]).unwrap();
    for gone in ["D", "E", "F"] {
        assert!(store.get(Table::Staging, gone).unwrap().is_none(), "{gone}");
    }
    assert!(store.get(Table::Staging, "K").unwrap().is_some());
}

#[test]
fn an_item_moved_out_after_its_old_folder_was_deleted_survives() {
    let mut store = committed(&[root(), folder("D", "R", "d"), file("K", "D", "keep")]);
    store.begin_staging(true).unwrap();
    store.stage(&[Change::Delete("D".into())]).unwrap();
    store.stage(&[file("K", "R", "keep")]).unwrap();
    assert_eq!(store.locate(Table::Staging, "K").unwrap().unwrap().rel, PathBuf::from("keep"));
}

#[test]
fn counts_and_the_skipped_list_see_only_what_is_reachable() {
    let store = committed(&[
        root(),
        folder("D", "R", "docs"),
        file("F", "D", "f.txt"),
        Change::Upsert(Row { placement: Placement::Skipped(SkipReason::PersonalVault), ..match folder("V", "R", "Personal Vault") { Change::Upsert(r) => r, _ => unreachable!() } }),
        file("VF", "V", "secret.txt"),
        Change::Upsert(Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..match file("N", "D", "n") { Change::Upsert(r) => r, _ => unreachable!() } }),
    ]);
    assert_eq!(store.counts().unwrap(), Counts { listed: 5, placed: 2, skipped: 2 });
    assert_eq!(
        store.skipped().unwrap(),
        vec![(PathBuf::from("Personal Vault"), SkipReason::PersonalVault), (PathBuf::from("docs/n"), SkipReason::NameTooLong)],
        "what is inside a skipped folder is not listed item by item"
    );
}

/// What a delta changed, read back from the two tables: an upsert, a
/// rename and a delete are the three ids; a row the delta left alone is
/// not one of them.
#[test]
fn the_changed_ids_are_what_staging_differs_from_items_by() {
    let mut store = committed(&[root(), folder("D", "R", "docs"), file("F", "D", "f"), file("G", "D", "g"), file("K", "D", "keep")]);
    store.begin_staging(true).unwrap();
    store.stage(&[file("N", "D", "new"), file("F", "D", "renamed"), Change::Delete("G".into())]).unwrap();
    let mut ids = store.changed_ids().unwrap();
    ids.sort();
    assert_eq!(ids, vec!["F".to_owned(), "G".to_owned(), "N".to_owned()]);
}

/// Issue #39: a delta changing 10 of 100 000 items stages those 10 and
/// the swap writes those 10 — not the whole tree; until the swap `items`
/// is the old tree, also after a crash (the store dropped and opened
/// again), and the next cycle stages afresh over it.
#[test]
fn a_delta_of_ten_writes_ten_rows_and_a_crash_before_the_swap_keeps_the_old_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut tree = vec![root()];
    for d in 0..100 {
        tree.push(folder(&format!("D{d}"), "R", &format!("d{d}")));
        tree.extend((0..999).map(|i| file(&format!("F{d}-{i}"), &format!("D{d}"), &format!("f{i}"))));
    }
    let delta: Vec<Change> = (0..10).map(|i| file(&format!("F7-{i}"), "D7", &format!("renamed{i}"))).collect();
    {
        let mut store = TreeStore::open(&path).unwrap();
        store.begin_staging(false).unwrap();
        store.stage(&tree).unwrap();
        store.commit_staging("link-1").unwrap();
        store.begin_staging(true).unwrap();
        store.stage(&delta).unwrap();
        assert_eq!(store.staged_rows(), 10, "only the delta's rows are staged");
        assert_eq!(store.changed_ids().unwrap().len(), 10);
        assert_eq!(store.locate(Table::Staging, "F7-3").unwrap().unwrap().rel, PathBuf::from("d7/renamed3"));
        assert_eq!(store.locate(Table::Items, "F7-3").unwrap().unwrap().rel, PathBuf::from("d7/f3"));
        // The daemon dies here.
    }
    let mut store = TreeStore::open(&path).unwrap();
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
    assert_eq!(store.get(Table::Items, "F7-3").unwrap().unwrap().name, "f3", "the old tree");
    store.begin_staging(true).unwrap();
    assert_eq!(store.staged_rows(), 0, "the next cycle stages afresh");
    store.stage(&delta).unwrap();
    let before = store.conn.total_changes();
    store.commit_staging("link-2").unwrap();
    let written = store.conn.total_changes() - before;
    assert!(written < 40, "the swap wrote {written} rows");
    assert_eq!(store.get(Table::Items, "F7-3").unwrap().unwrap().name, "renamed3");
    assert_eq!(store.get(Table::Items, "F7-99").unwrap().unwrap().name, "f99");
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-2"));
}

/// A delta that removes a folder, and a row it stages that equals the
/// base: the removal takes what is inside, and the equal row is no change.
#[test]
fn a_delta_laid_over_items_removes_and_changes_what_it_says() {
    let mut store = committed(&[root(), folder("D", "R", "d"), file("F", "D", "f"), file("K", "R", "k")]);
    store.begin_staging(true).unwrap();
    store.stage(&[Change::Delete("D".into()), file("K", "R", "k")]).unwrap();
    assert!(store.get(Table::Staging, "F").unwrap().is_none());
    assert!(store.descendants(Table::Staging, "R").unwrap() == vec!["K".to_owned()]);
    let mut ids = store.changed_ids().unwrap();
    ids.sort();
    assert_eq!(ids, vec!["D".to_owned(), "F".to_owned()], "K is staged as it was");
    store.commit_staging("link-2").unwrap();
    assert!(store.get(Table::Items, "F").unwrap().is_none());
    assert!(store.get(Table::Items, "K").unwrap().is_some());
}

#[test]
fn descendants_are_every_level_below() {
    let store = committed(&[root(), folder("D", "R", "d"), folder("E", "D", "e"), file("F", "E", "f")]);
    let mut below = store.descendants(Table::Items, "D").unwrap();
    below.sort();
    assert_eq!(below, vec!["E".to_owned(), "F".to_owned()]);
}

#[test]
fn a_store_survives_reopening_and_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/tree.sqlite");
    {
        let mut store = TreeStore::open(&path).unwrap();
        store.begin_staging(false).unwrap();
        store.stage(&[root(), file("A", "R", "a")]).unwrap();
        store.commit_staging("link-1").unwrap();
    }
    let store = TreeStore::open(&path).unwrap();
    assert!(store.get(Table::Items, "A").unwrap().is_some());
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

/// A permission failure is not corruption: `open` must fail rather than
/// silently discard a good store (rebuild trigger is missing,
/// unreadable-as-a-database, or an unknown schema version — never a
/// transient or permission failure). Skipped under root, which chmod 000
/// never refuses.
#[test]
fn a_permission_error_does_not_rebuild_a_good_store() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, which chmod 000 cannot refuse");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    {
        let mut store = TreeStore::open(&path).unwrap();
        store.begin_staging(false).unwrap();
        store.stage(&[root(), file("A", "R", "a")]).unwrap();
        store.commit_staging("link-1").unwrap();
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(TreeStore::open(&path).is_err(), "a permission failure must be returned, not treated as corruption");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let store = TreeStore::open(&path).unwrap();
    assert!(store.get(Table::Items, "A").unwrap().is_some(), "the good store must survive a transient open failure");
}

#[test]
fn an_unknown_schema_version_is_rebuilt_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    {
        let mut store = TreeStore::open(&path).unwrap();
        store.begin_staging(false).unwrap();
        store.stage(&[root(), file("A", "R", "a")]).unwrap();
        store.commit_staging("link-1").unwrap();
        store.set_meta("schema_version", Some("99")).unwrap();
    }
    let store = TreeStore::open(&path).unwrap();
    assert!(store.get(Table::Items, "A").unwrap().is_none());
    assert_eq!(store.delta_link().unwrap(), None, "a rebuilt store starts with a full listing");
    assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
}

/// a schema whose creation a crash cut short —
/// some tables, no `meta` — is rebuilt, not an error at every open.
#[test]
fn a_store_with_tables_and_no_meta_is_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    rusqlite::Connection::open(&path).unwrap().execute_batch("CREATE TABLE items (id TEXT PRIMARY KEY);").unwrap();
    let store = TreeStore::open(&path).unwrap();
    assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
}

/// Version 2 added `activity` and `conflicts`, so a store
/// written by the daemon before them is rebuilt once — from a full
/// listing, since it comes back with no delta link — and
/// then kept. With the version left at 1 the old store opens as it is
/// and has nowhere to put an event.
#[test]
fn a_store_from_before_the_activity_log_is_rebuilt_once_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                     size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0, etag TEXT, ctag TEXT,
                     quickxor TEXT, mime TEXT, placement TEXT NOT NULL, thumb_key TEXT);
                 CREATE TABLE staging AS SELECT * FROM items;
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 INSERT INTO meta VALUES ('schema_version', '1'), ('delta_link', 'link-1');",
        )
        .unwrap();
    }
    let mut store = TreeStore::open(&path).unwrap();
    assert_eq!(store.delta_link().unwrap(), None, "the old store is rebuilt, so the next cycle lists in full");
    let event = ActivityRow { at: 1, kind: "listed".into(), path: "/f".into(), detail: "1 item".into() };
    store.add_activity(std::slice::from_ref(&event)).unwrap();
    store.set_meta("delta_link", Some("link-2")).unwrap();
    drop(store);
    let store = TreeStore::open(&path).unwrap();
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-2"), "rebuilt once, then kept");
    assert_eq!(store.recent_activity(10).unwrap(), vec![event]);
}

/// Version 3 added the outbox: a version 2 store — a read-only
/// folder's, with nothing waiting to upload — is rebuilt once, from a
/// full listing, and comes back with the new tables.
#[test]
fn a_version_2_store_is_rebuilt_once_with_the_outbox() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    {
        let mut store = TreeStore::open(&path).unwrap();
        store.begin_staging(false).unwrap();
        store.stage(&[root(), file("A", "R", "a")]).unwrap();
        store.commit_staging("link-1").unwrap();
        store.conn.execute_batch("DROP TABLE outbox; DROP TABLE local_skipped;").unwrap();
        store.set_meta("schema_version", Some("2")).unwrap();
    }
    let store = TreeStore::open(&path).unwrap();
    assert_eq!(store.delta_link().unwrap(), None, "rebuilt: the next cycle lists in full");
    assert!(store.get(Table::Items, "A").unwrap().is_none());
    assert!(store.outbox_rows().unwrap().is_empty(), "the outbox is there, empty");
    assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
}

/// The inode an item was placed as survives the swap that ends a cycle,
/// a full listing's included (it stages from nothing), and a first
/// listing's page commit.
#[test]
fn the_local_handle_travels_with_its_row() {
    let handle = konedrive_fs::handle::FileHandle { kind: 1, bytes: vec![1, 2, 3] };
    let mut store = committed(&[root(), file("A", "R", "a")]);
    store.set_local_handle("A", Some(&handle)).unwrap();
    store.begin_staging(false).unwrap();
    store.stage(&[root(), file("A", "R", "renamed")]).unwrap();
    store.commit_staging("link-2").unwrap();
    assert_eq!(store.local_handle("A").unwrap(), Some(handle.clone()), "a full listing");
    store.begin_staging(true).unwrap();
    store.stage(&[file("A", "R", "again")]).unwrap();
    store.commit_staging("link-3").unwrap();
    assert_eq!(store.local_handle("A").unwrap(), Some(handle.clone()), "a delta");
    assert_eq!(store.item_by_handle(&handle).unwrap().map(|r| r.id), Some("A".into()));

    // A page placed: the handle was recorded in `staging` before the
    // item was in `items`.
    let mut store = TreeStore::in_memory().unwrap();
    store.begin_staging(true).unwrap();
    store.stage(&[root(), file("B", "R", "b")]).unwrap();
    store.set_local_handle("B", Some(&handle)).unwrap();
    store.commit_page(&[root(), file("B", "R", "b")], "next-2").unwrap();
    assert_eq!(store.local_handle("B").unwrap(), Some(handle));
}

/// Review fix 2 of issue #104: a version 3 store, as a build before
/// #104 left it — a folder not placed whose children keep their local
/// objects — is brought to version 4 in place on open: the children
/// forget them, the rest of the store stays.
#[test]
fn a_version_3_store_forgets_the_objects_below_a_folder_not_placed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let handle = |n: u8| konedrive_fs::handle::FileHandle { kind: 1, bytes: vec![n; 4] };
    {
        let mut store = TreeStore::open(&path).unwrap();
        let Change::Upsert(placed) = folder("D", "R", "long") else { unreachable!() };
        let skipped = Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..placed };
        store.begin_staging(false).unwrap();
        store.stage(&[root(), Change::Upsert(skipped), folder("F", "D", "f"), file("G", "F", "g"), file("T", "R", "t")]).unwrap();
        store.commit_staging("link-1").unwrap();
        for (id, n) in [("D", 1), ("F", 2), ("G", 3), ("T", 4)] {
            store.set_local_handle(id, Some(&handle(n))).unwrap();
        }
        store.set_meta("schema_version", Some("3")).unwrap();
    }
    let store = TreeStore::open(&path).unwrap();
    assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
    assert_eq!(store.local_handle("F").unwrap(), None);
    assert_eq!(store.local_handle("G").unwrap(), None, "every level below");
    assert_eq!(store.local_handle("T").unwrap(), Some(handle(4)), "a placed item keeps its object");
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"), "not rebuilt");
}

/// Review fixes, round 2, of issue #104: forgetting below a row that
/// turns placed again stays cheap in a store of 30,000 rows — a file
/// (nothing below it) costs no recursive query, a folder one. The times
/// are printed (`--nocapture`); the bounds are loose.
#[test]
fn forgetting_below_a_row_placed_again_is_cheap_at_scale() {
    let mut changes = vec![root()];
    for d in 0..100 {
        changes.push(folder(&format!("D{d}"), "R", &format!("d{d}")));
        for f in 0..299 {
            changes.push(file(&format!("F{d}-{f}"), &format!("D{d}"), &format!("f{f}")));
        }
    }
    let mut store = committed(&changes);
    store.begin_staging(true).unwrap();
    let tx = store.conn.transaction().unwrap();
    let time = |root: &str| {
        let started = std::time::Instant::now();
        for _ in 0..10 {
            forget_subtrees(&tx, &[root.to_owned()], false, &[]).unwrap();
        }
        started.elapsed() / 10
    };
    let (a_file, a_folder) = (time("F50-7"), time("D50"));
    eprintln!("forget_subtrees on 30,000 rows: below a file {a_file:?}, below a folder of 299 {a_folder:?}");
    assert!(a_file < std::time::Duration::from_secs(1) && a_folder < std::time::Duration::from_secs(1));
}

/// The last 200 events are kept; the oldest go.
#[test]
fn the_activity_log_keeps_the_newest_two_hundred() {
    let mut store = TreeStore::in_memory().unwrap();
    let event = |n: i64| ActivityRow { at: n, kind: "downloaded".into(), path: format!("/f{n}"), detail: String::new() };
    store.add_activity(&(1..=150).map(event).collect::<Vec<_>>()).unwrap();
    store.add_activity(&(151..=205).map(event).collect::<Vec<_>>()).unwrap();
    let rows: i64 = store.conn.query_row("SELECT count(*) FROM activity", [], |row| row.get(0)).unwrap();
    assert_eq!(rows, ACTIVITY_KEPT as i64, "the table itself holds no more, not just what is read back");
    let kept = store.recent_activity(1000).unwrap();
    assert_eq!(kept.len(), ACTIVITY_KEPT);
    assert_eq!((kept[0].at, kept[ACTIVITY_KEPT - 1].at), (205, 6), "newest first, the five oldest gone");
    assert_eq!(store.recent_activity(3).unwrap().iter().map(|e| e.at).collect::<Vec<_>>(), vec![205, 204, 203]);
}

#[test]
fn a_conflict_is_listed_until_it_is_removed() {
    let mut store = TreeStore::in_memory().unwrap();
    let row = ConflictRow { at: 7, original: "/root/a.txt".into(), rescued: "/rescued/now/a.txt".into(), kind: ConflictKind::Copy };
    store.add_conflicts(std::slice::from_ref(&row)).unwrap();
    assert_eq!(store.conflicts().unwrap(), vec![row]);
    assert!(!store.remove_conflict("/elsewhere").unwrap());
    assert!(store.remove_conflict("/rescued/now/a.txt").unwrap());
    assert!(store.conflicts().unwrap().is_empty());
}

#[test]
fn a_file_that_is_not_a_database_is_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    std::fs::write(&path, b"this is not sqlite at all, not even a little").unwrap();
    let store = TreeStore::open(&path).unwrap();
    assert_eq!(store.delta_link().unwrap(), None);
}

use super::*;

/// While a long job holds the store's thread, tasks waiting for
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

#[test]
fn a_store_survives_reopening_and_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/tree.sqlite");
    {
        let store = TreeStore::open(&path).unwrap();
        store.set_drive_id("d1").unwrap();
    }
    let store = TreeStore::open(&path).unwrap();
    assert_eq!(store.drive_id().unwrap().as_deref(), Some("d1"));
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
        let store = TreeStore::open(&path).unwrap();
        store.set_drive_id("d1").unwrap();
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(TreeStore::open(&path).is_err(), "a permission failure must be returned, not treated as corruption");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let store = TreeStore::open(&path).unwrap();
    assert_eq!(store.drive_id().unwrap().as_deref(), Some("d1"), "the good store must survive a transient open failure");
}

/// The last 200 events are kept; the oldest go.
#[test]
fn the_activity_log_keeps_the_newest_two_hundred() {
    let mut store = TreeStore::in_memory().unwrap();
    let event = |n: i64| ActivityRow { at: n, kind: ActivityKind::Downloaded, path: format!("/f{n}"), detail: String::new() };
    store.add_activity(&(1..=150).map(event).collect::<Vec<_>>()).unwrap();
    store.add_activity(&(151..=205).map(event).collect::<Vec<_>>()).unwrap();
    let rows: i64 = store.conn.query_row("SELECT count(*) FROM activity", [], |row| row.get(0)).unwrap();
    assert_eq!(rows, ACTIVITY_KEPT as i64, "the table itself holds no more, not just what is read back");
    let kept = store.recent_activity(1000).unwrap();
    assert_eq!(kept.len(), ACTIVITY_KEPT);
    assert_eq!((kept[0].at, kept[ACTIVITY_KEPT - 1].at), (205, 6), "newest first, the five oldest gone");
    assert_eq!(store.recent_activity(3).unwrap().iter().map(|e| e.at).collect::<Vec<_>>(), vec![205, 204, 203]);
}

/// A store another version wrote opens and reads: an event whose kind this
/// version does not name is left out of the log, and the rest are read.
#[test]
fn an_event_of_a_kind_this_version_does_not_name_is_left_out() {
    let mut store = TreeStore::in_memory().unwrap();
    store.conn.execute("INSERT INTO activity (at, kind, path, detail) VALUES (1, 'a-kind-of-another-version', '/f', '')", []).unwrap();
    store.add_activity(&[ActivityRow { at: 2, kind: ActivityKind::Uploaded, path: "/g".into(), detail: String::new() }]).unwrap();
    let read = store.recent_activity(10).unwrap();
    assert_eq!(read.iter().map(|e| (e.at, e.kind)).collect::<Vec<_>>(), vec![(2, ActivityKind::Uploaded)]);
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

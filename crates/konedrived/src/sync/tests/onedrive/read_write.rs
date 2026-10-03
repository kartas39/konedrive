use super::*;

/// Write design §3.9: the account turning read-write takes the read-only lock off
/// its folder — files `0644`, directories `0755`, the folder itself last — and the
/// sync that starts again leaves it off through a Full reconcile; turning read-only
/// puts it back on at once. A folder brought up read-write with the lock still on — a
/// switch cut short — loses it as it comes up.
#[tokio::test]
async fn the_lock_comes_off_and_goes_back_on_with_the_mode() {
    use crate::config::Mode;
    let w = world().await;
    let modes = || {
        let folder = w.folder.path();
        (mode(&folder.join("docs/f.txt")), mode(&folder.join("docs")), mode(folder))
    };
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    assert_eq!(modes(), (0o444, 0o555, 0o555));

    let before = deltas(&w).await;
    service.follow_mode(Mode::ReadWrite).await;
    assert_eq!(modes(), (0o644, 0o755, 0o755));
    // The sync runs again: its first cycle, a Full reconcile, is over once a second
    // cycle has asked for changes.
    wait_for_deltas(&w, before).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, before + 1).await;
    assert_eq!(modes(), (0o644, 0o755, 0o755), "a read-write folder's reconcile leaves the lock off");

    service.follow_mode(Mode::ReadOnly).await;
    assert_eq!(modes(), (0o444, 0o555, 0o555));
    service.stop_sync().await;
    service.set_link(None);

    // Read-write in config.toml again, but the walk never ran: the next bring-up runs it.
    let restarted = connected(&w, true).await;
    restarted.start_in_mode(Mode::ReadWrite);
    restarted.restore().await;
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
    assert_eq!(modes(), (0o644, 0o755, 0o755));
    restarted.stop_sync().await;
    restarted.set_link(None);

    // Read-only again, and the lock walk never ran — the daemon stopped
    // first. The bring-up puts the lock back at once, with no Full reconcile to do it:
    // this one cannot reach OneDrive.
    let offline = Arc::new(StaticToken::new("T"));
    offline.invalidate().await;
    let read_only = service_with(&w, account(true), Some(link(&w).await), offline);
    read_only.restore().await;
    read_only.resume().await;
    assert_eq!(read_only.mode(), Mode::ReadOnly);
    assert_eq!(modes(), (0o444, 0o555, 0o555), "never writable while the account is read-only");
    read_only.stop_sync().await;
}

/// the watcher on the mode switch's hooks: a read-write folder's sync runs the watcher (the lock came off
/// once it had walked the folder), and a file another process makes in the folder is
/// counted as waiting to be uploaded the moment the switch to read-only asks, quiet
/// spell or not (the watcher); a read-only folder has no watcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_made_in_a_read_write_folder_waits_to_be_uploaded() {
    use crate::account::PendingUploads;
    use crate::config::Mode;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let watching = |service: &SyncService| service.syncing.lock().unwrap().as_ref().is_some_and(|s| s.watcher.is_some());
    assert!(!watching(&service), "read-only");

    service.follow_mode(Mode::ReadWrite).await;
    assert!(watching(&service));
    assert_eq!(mode(&w.folder.path().join("docs")), 0o755);
    let made = std::process::Command::new("sh")
        .args(["-c", "echo new > docs/new.txt"])
        .current_dir(w.folder.path())
        .status()
        .unwrap();
    assert!(made.success());
    assert_eq!(service.pending_uploads().await, 1);
    // `PendingCount` as the worker counts it.
    wait_until("the worker counted the change", || service.state().get().pending_count == 1).await;

    // A forced switch (only that drops them): the drop, then the
    // folder follows.
    service.drop_pending_uploads().await;
    service.follow_mode(Mode::ReadOnly).await;
    assert!(!watching(&service), "stopped with the sync");
    // A read-only folder uploads nothing; its rows are dropped, the file stays.
    assert_eq!(service.pending_uploads().await, 0);
    // the outbox on the bus: and the bus says so.
    assert_eq!(service.state().get().pending_count, 0);
    assert!(w.folder.path().join("docs/new.txt").exists());
    service.stop_sync().await;
}

/// the outbox worker on the mode switch's and the watcher's hooks: a read-write folder's sync runs the outbox worker beside
/// the watcher; a file made in the folder is examined, the worker is woken, and the
/// file goes up and is committed — its item id on it, nothing left waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_made_in_a_read_write_folder_is_uploaded() {
    use crate::account::PendingUploads;
    use crate::config::Mode;
    use wiremock::matchers::path_regex;
    let w = world().await;
    let mut hasher = konedrive_graph::quickxor::QuickXor::new();
    hasher.update(b"new\n");
    Mock::given(method("POST"))
        .and(path_regex("/me/drive/items/D:/new.txt:/createUploadSession$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadUrl": format!("{}/upload/s1", w.server.uri()),
            "expirationDateTime": "2099-01-01T00:00:00Z"
        })))
        .mount(&w.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/s1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "N1", "name": "new.txt", "size": 4, "eTag": "e-N1", "cTag": "c-N1",
            "parentReference": {"id": "D"},
            "file": {"hashes": {"quickXorHash": hasher.finish_base64()}}
        })))
        .mount(&w.server)
        .await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let_write(&service);
    service.follow_mode(Mode::ReadWrite).await;
    assert!(service.syncing.lock().unwrap().as_ref().is_some_and(|s| s.outbox.is_some()), "beside the watcher");
    let mut announced = service.report().activity.subscribe();
    let made = std::process::Command::new("sh")
        .args(["-c", "echo new > docs/new.txt"])
        .current_dir(w.folder.path())
        .status()
        .unwrap();
    assert!(made.success());
    let file = w.folder.path().join("docs/new.txt");
    let committed = || xattr::get(&file, konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while committed().is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(committed().as_deref(), Some(&b"N1"[..]), "uploaded and committed");
    assert_eq!(service.pending_uploads().await, 0);
    // The live signal, and the counts the bus publishes.
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = announced.recv().await.unwrap();
            if event.kind == "uploaded" {
                return event;
            }
        }
    })
    .await
    .expect("ActivityLog.Added for the upload");
    assert_eq!(event.path, file.display().to_string());
    wait_until("the outbox counted empty", || service.state().get().pending_count == 0).await;
    assert!(service.state().get().uploads.is_empty());

    service.follow_mode(Mode::ReadOnly).await;
    assert!(service.syncing.lock().unwrap().as_ref().is_some_and(|s| s.outbox.is_none()), "stopped with the sync");
    service.stop_sync().await;
}

/// the outbox on the bus, `Pause`: a paused account asks OneDrive for nothing — not on
/// `Refresh`, not after a restart, since the pause is kept in the tree
/// store — until `Resume`; a timed pause ends by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_holds_the_poll_outlasts_a_restart_and_ends_by_itself() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.pause_syncing(0).await.unwrap();
    assert_eq!(service.state().get().paused_until, Some(0));
    let before = deltas(&w).await;
    service.refresh().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(deltas(&w).await, before, "a paused account asks OneDrive for nothing");
    service.stop_sync().await;
    service.set_link(None);

    let restarted = connected(&w, true).await;
    restarted.restore().await;
    restarted.resume().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(restarted.state().get().paused_until, Some(0), "the pause outlasts a restart");
    assert_eq!(deltas(&w).await, before);
    restarted.resume_syncing().await.unwrap();
    wait_for_deltas(&w, before).await;
    assert_eq!(restarted.state().get().paused_until, None);

    restarted.pause_syncing(1).await.unwrap();
    assert!(restarted.state().get().paused_until.is_some_and(|until| until > 0));
    wait_until("the timed pause ends by itself", || restarted.state().get().paused_until.is_none()).await;
    restarted.stop_sync().await;
}

/// the outbox on the bus: the outbox as the bus shows it — `Changes()`, `NotUploaded()`, the
/// mass-delete guard's two answers — and a free-up of a file whose change
/// waits to be uploaded, refused `NotUploaded`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_outbox_is_listed_decided_on_and_its_files_are_not_freed_up() {
    use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxState};
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let store = service.store.lock().unwrap().clone().unwrap();
    let file = w.folder.path().join("docs/f.txt");
    let base = Base { etag: None, ctag: Some("c1".into()), parent: Some("D".into()), name: Some("f.txt".into()) };
    let change = Detection {
        kind: OutboxKind::Update,
        item_id: Some("F".into()),
        inode: None,
        rel: "docs/f.txt".into(),
        base: Some(base.clone()),
        target_parent: Some("D".into()),
        target_name: Some("f.txt".into()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    };
    store.call(move |s| s.outbox_record(&change)).await.unwrap();

    let rows = service.outbox(0).await.unwrap();
    assert_eq!(rows.len(), 1);
    let (_, kind, path, state, _, _, _, _) = &rows[0];
    assert_eq!((kind.as_str(), path.as_str(), state.as_str()), ("update", file.to_str().unwrap(), "ready"));
    // A placeholder has nothing to lose: its own refusal.
    assert!(matches!(service.dehydrate(&file).await, Err(SyncError::NotHydrated)));
    // Downloaded (by hand: the world serves no content), it is refused
    // NotUploaded, whole calls included, and stays downloaded.
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    {
        let opened = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        use std::io::Write as _;
        (&opened).write_all(b"abc").unwrap();
        konedrive_fs::placeholder::write_state(&opened, konedrive_fs::placeholder::State::Hydrated).unwrap();
        konedrive_fs::placeholder::write_stamp(&opened).unwrap();
    }
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
    let refused = service.dehydrate(&file).await.unwrap_err();
    assert!(matches!(refused, SyncError::NotUploaded(_)), "{refused:?}");
    assert!(matches!(service.check_free_up(std::slice::from_ref(&file)).await, Err(SyncError::NotUploaded(_))), "before anything changes");
    assert!(matches!(service.free_up(std::slice::from_ref(&file)).await, Err(SyncError::NotUploaded(_))));
    assert_eq!(std::fs::read(&file).unwrap(), b"abc", "still downloaded");

    let seq = rows[0].0 as i64;
    store.call(move |s| s.outbox_set_state(seq, OutboxState::Blocked, Some("name-characters"), None)).await.unwrap();
    assert_eq!(service.not_uploaded().await.unwrap(), vec![(file.display().to_string(), "name-characters".to_owned())]);

    store.call(move |s| s.outbox_set_state(seq, OutboxState::Held, Some("mass-delete"), None)).await.unwrap();
    assert_eq!(service.confirm_deletes().await.unwrap(), 1);
    assert_eq!(service.outbox(0).await.unwrap()[0].3, "ready");
    store.call(move |s| s.outbox_set_state(seq, OutboxState::Held, Some("mass-delete"), None)).await.unwrap();
    assert_eq!(service.restore_deletes().await.unwrap(), 1);
    assert!(service.outbox(0).await.unwrap().is_empty());
    service.stop_sync().await;
}

/// Issue #38: while an examination's apply holds the store, the bus still
/// answers at once — the counts and the Not Uploaded summary from memory,
/// `Changes()` and `NotUploadedFiles()` through the read-only connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bus_answers_while_the_store_is_held() {
    use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxState};
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    let store = service.store.lock().unwrap().clone().unwrap();
    let blocked = Detection {
        kind: OutboxKind::Update,
        item_id: Some("F".into()),
        inode: None,
        rel: "docs/f.txt".into(),
        base: Some(Base { etag: None, ctag: Some("c1".into()), parent: Some("D".into()), name: Some("f.txt".into()) }),
        target_parent: Some("D".into()),
        target_name: Some("f.txt".into()),
        same_content: false,
        state: OutboxState::Blocked,
        reason: Some("name-characters".into()),
        next_try: None,
        size: Some(3),
    };
    store.call(move |s| s.outbox_record(&blocked)).await.unwrap();
    wait_until("BlockedCount counts it", || service.state().get().blocked_count == 1).await;
    wait_until("the summary is summed", || service.kept_back.lock().unwrap().as_ref().is_some_and(|k| k.iter().any(|r| r.1 == "name-characters"))).await;

    // An apply that holds the store for two seconds.
    let (held, release) = std::sync::mpsc::channel::<()>();
    let holder = store.clone();
    let holding = std::thread::spawn(move || {
        holder.call_blocking(move |_| {
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(2));
            Ok(())
        })
    });
    release.recv().unwrap();
    let start = std::time::Instant::now();
    assert_eq!(service.state().get().blocked_count, 1);
    let summary = service.not_uploaded_summary().await.unwrap();
    assert_eq!(summary, vec![("per-file".to_owned(), "name-characters".to_owned(), 1, 3)]);
    assert_eq!(service.outbox(21).await.unwrap().len(), 1);
    assert_eq!(service.not_uploaded_files("name-characters".into(), 20).await.unwrap().1, 1);
    let took = start.elapsed();
    assert!(took < Duration::from_millis(500), "the bus waited for the store: {took:?}");
    holding.join().unwrap().unwrap();
    service.stop_sync().await;
}

/// the outbox on the bus: deletes held by the mass-delete guard are counted
/// on the bus (`HeldCount`), and `RestoreDeletes` brings the files back
/// at once — a Full reconcile, though OneDrive did not change them — and
/// deletes nothing in OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoring_held_deletes_brings_the_files_back_at_once() {
    use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxState};
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    let file = w.folder.path().join("docs/f.txt");
    std::fs::remove_file(&file).unwrap();
    let store = service.store.lock().unwrap().clone().unwrap();
    let held = Detection {
        kind: OutboxKind::Delete,
        item_id: Some("F".into()),
        inode: None,
        rel: "docs/f.txt".into(),
        base: Some(Base { etag: None, ctag: Some("c1".into()), parent: Some("D".into()), name: Some("f.txt".into()) }),
        target_parent: None,
        target_name: None,
        same_content: false,
        state: OutboxState::Held,
        reason: Some("mass-delete".into()),
        next_try: None,
        size: None,
    };
    store.call(move |s| s.outbox_record(&held)).await.unwrap();
    service.wake_outbox();
    wait_until("HeldCount counts it", || service.state().get().held_count == 1).await;

    assert_eq!(service.restore_deletes().await.unwrap(), 1);
    wait_until("the file is placed again at once", || file.exists()).await;
    wait_until("HeldCount is 0 again", || service.state().get().held_count == 0).await;
    let deleted = w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "DELETE").count();
    assert_eq!(deleted, 0, "nothing is deleted in OneDrive");
    service.stop_sync().await;
}

/// the outbox on the bus: a free-up of a downloaded file in a OneDrive folder whose
/// outbox cannot be read is refused, not let through; the file stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_free_up_that_cannot_tell_whether_a_change_waits_refuses() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    let file = w.folder.path().join("docs/f.txt");
    {
        let opened = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        use std::io::Write as _;
        (&opened).write_all(b"abc").unwrap();
        konedrive_fs::placeholder::write_state(&opened, konedrive_fs::placeholder::State::Hydrated).unwrap();
        konedrive_fs::placeholder::write_stamp(&opened).unwrap();
    }
    service.stop_sync().await;
    *service.store.lock().unwrap() = None;
    let refused = service.dehydrate(&file).await.unwrap_err();
    assert!(matches!(&refused, SyncError::Io(why) if why.contains("cannot tell")), "{refused:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"abc", "still downloaded");
}

/// the outbox on the bus: a directory of the user's own that is newly ignored
/// takes the changes waiting inside it along — the outbox empties — and
/// nothing in it is uploaded, then or later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ignored_directory_keeps_everything_in_it_local() {
    use crate::account::PendingUploads;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    service.pause_syncing(0).await.unwrap();
    let made = std::process::Command::new("sh")
        .args(["-c", "mkdir -p build/obj && echo a > build/a.o && echo b > build/obj/b.o"])
        .current_dir(w.folder.path())
        .status()
        .unwrap();
    assert!(made.success());
    assert_eq!(service.pending_uploads().await, 4, "the folders and the files wait");

    let mut patterns = service.ignore_patterns();
    patterns.push("build".into());
    service.set_ignore_patterns(patterns).await.unwrap();
    let mut left = u64::MAX;
    for _ in 0..200 {
        left = service.pending_uploads().await;
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(left, 0, "the outbox empties");
    std::fs::write(w.folder.path().join("build/c.o"), b"c").unwrap();
    assert_eq!(service.pending_uploads().await, 0, "and stays empty");

    service.resume_syncing().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let uploads = w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() != "GET").count();
    assert_eq!(uploads, 0, "nothing in it is sent");
    service.stop_sync().await;
}

/// the outbox on the bus: a new file whose folder OneDrive no longer has asks
/// for a cycle, not a Full reconcile — which, before the read-write reconcile, put a rename
/// still waiting to go up back where the base has it (F63, closed). The
/// rename waiting beside it stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_folder_asks_for_a_cycle_that_leaves_waiting_renames_alone() {
    use crate::config::Mode;
    let w = world().await;
    // The rename cannot reach OneDrive yet; the new file's folder is
    // gone there (nothing mocked for it: 404).
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&w.server)
        .await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let_write(&service);
    let before = deltas(&w).await;
    service.follow_mode(Mode::ReadWrite).await;
    // The switch's own Full cycle is over once a later one has begun.
    wait_for_deltas(&w, before).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, before + 1).await;

    let made = std::process::Command::new("sh")
        .args(["-c", "mv docs/f.txt docs/g.txt && echo new > docs/new.txt"])
        .current_dir(w.folder.path())
        .status()
        .unwrap();
    assert!(made.success());
    async fn asked(w: &World) -> usize {
        w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("createUploadSession")).count()
    }
    for _ in 0..300 {
        if asked(&w).await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(asked(&w).await > 0, "the new file was sent");
    // The cycle the missing folder asked for is over once a later one
    // has begun.
    let seen = deltas(&w).await;
    wait_for_deltas(&w, seen).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, seen + 1).await;
    let docs = w.folder.path().join("docs");
    assert!(docs.join("g.txt").exists() && !docs.join("f.txt").exists(), "the rename waiting to go up stands");
    assert!(docs.join("new.txt").exists());
    service.stop_sync().await;
}

/// the outbox on the bus: a `Pause` that lands after the timer read the
/// store's pause as over is not undone on the bus: the timer looks
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_that_lands_as_the_last_one_ends_stands() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.pause_syncing(3600).await.unwrap();
    // The timer has read the store's pause as over…
    let seen = service.pause_shown.load(std::sync::atomic::Ordering::SeqCst);
    // …when a new `Pause` lands.
    service.pause_syncing(7200).await.unwrap();
    assert!(!service.pause_timer_done(seen, true), "the timer looks again");
    assert!(service.state().get().paused_until.is_some_and(|until| until > 0), "still paused on the bus");
    service.stop_sync().await;
}

/// The outbox worker asks the write gate before each row. A drive
/// taken off `write_test_drive_ids` while it runs sends nothing more: the change waits,
/// and the folder's `LastError` says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drive_taken_off_the_list_while_the_worker_runs_sends_nothing_more() {
    use crate::account::PendingUploads;
    use crate::config::{ConfigError, Mode};
    use wiremock::matchers::path_regex;
    let w = world().await;
    let mut hasher = konedrive_graph::quickxor::QuickXor::new();
    hasher.update(b"new\n");
    Mock::given(method("POST"))
        .and(path_regex("createUploadSession$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadUrl": format!("{}/upload/s1", w.server.uri()),
            "expirationDateTime": "2099-01-01T00:00:00Z"
        })))
        .mount(&w.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/s1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "N1", "name": "new.txt", "size": 4, "eTag": "e-N1", "cTag": "c-N1",
            "parentReference": {"id": "D"},
            "file": {"hashes": {"quickXorHash": hasher.finish_base64()}}
        })))
        .mount(&w.server)
        .await;
    async fn sent(w: &World) -> usize {
        w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() != "GET").count()
    }
    let make = |script: &str| {
        let made = std::process::Command::new("sh").args(["-c", script]).current_dir(w.folder.path()).status().unwrap();
        assert!(made.success());
    };
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let_write(&service);
    service.follow_mode(Mode::ReadWrite).await;
    make("echo new > docs/new.txt");
    let first = w.folder.path().join("docs/new.txt");
    wait_until("the first change goes up", || {
        xattr::get(&first, konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten().is_some()
    })
    .await;
    let before = sent(&w).await;

    let persist = service.persist.as_ref().unwrap();
    persist
        .store
        .update(|c| {
            c.write_test_drive_ids.clear();
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    make("echo second > docs/second.txt");
    assert_eq!(service.pending_uploads().await, 1, "the change is recorded");
    wait_until("the folder says why nothing goes", || {
        crate::status::snapshot::published_error(&service.state().get()).contains("write_test_drive_ids")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sent(&w).await, before, "nothing more is sent");
    assert_eq!(service.pending_uploads().await, 1, "the change waits");
    service.stop_sync().await;
}

/// A switch to read-only nobody forced keeps the changes waiting to
/// upload, the folder is locked, and its sync holds its cycles while they wait: no
/// read-only reconcile puts back what they describe. `expired`: the sign-in expired
/// (`invalid_grant`, as the token manager records it); otherwise a sign-out.
async fn a_switch_nobody_forced_keeps_the_changes(expired: bool) {
    use crate::account::PendingUploads;
    use crate::config::Mode;
    let w = world().await;
    // The rename cannot reach OneDrive yet.
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&w.server)
        .await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let_write(&service);
    let account = service.account.clone().unwrap();
    let follower = tokio::spawn(crate::sync::write_mode::follow(account.subscribe(), Arc::downgrade(&service)));
    let docs = w.folder.path().join("docs");
    wait_until("the folder is read-write", || service.mode() == Mode::ReadWrite && mode(&docs) == 0o755).await;
    let made = std::process::Command::new("sh").args(["-c", "mv docs/f.txt docs/g.txt"]).current_dir(w.folder.path()).status().unwrap();
    assert!(made.success());
    assert_eq!(service.pending_uploads().await, 1);

    account.update(|s| {
        s.state = SignInState::SignedOut;
        if expired {
            s.last_error = konedrive_graph::token::SESSION_EXPIRED.into();
        }
        s.clear_account();
    });
    wait_until("the folder holds its cycles", || {
        crate::status::snapshot::published_error(&service.state().get()).contains("wait to be uploaded")
    })
    .await;
    assert_eq!(service.mode(), Mode::ReadOnly);
    let before = deltas(&w).await;
    service.nudge();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(deltas(&w).await, before, "no cycle while the change waits");
    assert!(docs.join("g.txt").exists() && !docs.join("f.txt").exists(), "nothing local is put back");
    assert_eq!(mode(&docs), 0o555, "the folder is locked");
    assert_eq!(service.pending_uploads().await, 1, "the change still waits");
    follower.abort();
    service.stop_sync().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_out_keeps_the_changes_waiting_to_upload() {
    a_switch_nobody_forced_keeps_the_changes(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_sign_in_keeps_the_changes_waiting_to_upload() {
    a_switch_nobody_forced_keeps_the_changes(true).await;
}

/// A Forget — and so `Accounts.Remove`, which forgets first — is
/// refused `PendingUploads` while changes wait to be uploaded, and changes nothing; once
/// a forced switch to read-only has dropped them, it goes through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_whose_changes_wait_is_not_forgotten() {
    use crate::account::PendingUploads;
    use crate::config::Mode;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    // The gate stays closed: the rename waits.
    service.follow_mode(Mode::ReadWrite).await;
    let made = std::process::Command::new("sh").args(["-c", "mv docs/f.txt docs/g.txt"]).current_dir(w.folder.path()).status().unwrap();
    assert!(made.success());
    assert_eq!(service.pending_uploads().await, 1);

    let refused = service.unregister_root().await.unwrap_err();
    assert!(matches!(&refused, SyncError::PendingUploads(why) if why.starts_with("1 change")), "{refused:?}");
    assert!(matches!(crate::dbus::fault::to_fault(refused), crate::dbus::fault::SyncFault::PendingUploads(_)));
    assert!(matches!(service.retire().await, Err(SyncError::PendingUploads(_))), "Remove's first step too");
    assert!(service.registration().is_some(), "still registered");
    assert_eq!(service.pending_uploads().await, 1, "the change still waits");
    assert!(w.folder.path().join("docs/g.txt").exists());

    // A forced switch drops them: the drop, then the folder follows.
    service.drop_pending_uploads().await;
    service.follow_mode(Mode::ReadOnly).await;
    service.unregister_root().await.unwrap();
}

/// the outbox on the bus: a forgotten folder is no longer paused on the bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forgotten_folder_is_not_paused() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.pause_syncing(3600).await.unwrap();
    assert!(service.state().get().paused_until.is_some());
    service.unregister_root().await.unwrap();
    assert_eq!(service.state().get().paused_until, None);
}

/// the watcher: a folder turning read-write whose watcher cannot start stays locked,
/// and says why; nothing is ever made in it unwatched.
#[tokio::test]
async fn a_read_write_folder_whose_watcher_cannot_start_stays_locked() {
    use crate::config::Mode;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    write_mode::FAIL_WATCHER.with(|fail| fail.set(true));
    service.follow_mode(Mode::ReadWrite).await;
    write_mode::FAIL_WATCHER.with(|fail| fail.set(false));
    assert_eq!((mode(w.folder.path()), mode(&w.folder.path().join("docs"))), (0o555, 0o555));
    assert!(service.last_error().contains("stays read-only"), "{}", service.last_error());
    service.stop_sync().await;
}

/// the watcher: a read-write folder an earlier run left unlocked, whose sync
/// cannot start now, is locked again: no watcher looks at it.
#[tokio::test]
async fn a_read_write_folder_whose_sync_cannot_start_is_locked_again() {
    use crate::config::Mode;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    assert_eq!(mode(w.folder.path()), 0o755);
    service.stop_sync().await;
    service.set_link(None);

    // The next run cannot open its tree store.
    let tree = w.config.path().join("tree.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", tree.display()));
    }
    std::fs::create_dir(&tree).unwrap();
    let restarted = connected(&w, true).await;
    restarted.start_in_mode(Mode::ReadWrite);
    restarted.restore().await;
    restarted.resume().await;
    assert!(restarted.last_error().contains("tree store"), "{}", restarted.last_error());
    assert_eq!((mode(w.folder.path()), mode(&w.folder.path().join("docs"))), (0o555, 0o555));
    restarted.stop_sync().await;
}

/// SY1: a forced switch to read-only and a bring-up end, with a cycle under way. The set-up:
/// the test holds the tree lock, as a commit of the outbox worker does; a cycle has fetched
/// and waits for that lock; the forced switch stops the folder's tasks, which ends the cycle,
/// takes `lifecycle` for writing and waits for the tree lock; a bring-up after the helper
/// reconnects waits for `lifecycle` behind it. Then the test lets go, and both must end.
/// When the switch took `lifecycle` for reading and left the cycle running, the cycle, the
/// switch and the bring-up waited for each other for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cycle_a_forced_switch_and_a_bring_up_at_once_all_end() {
    use crate::account::PendingUploads;
    use crate::config::Mode;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let before = deltas(&w).await;
    service.follow_mode(Mode::ReadWrite).await;
    // The read-write sync's first cycle has asked OneDrive: the watcher's first scan is over.
    wait_for_deltas(&w, before).await;

    let held = Arc::clone(&service.tree_lock).lock_owned().await;
    // A cycle that reconciles: it has fetched, and is given time to reach the tree lock.
    let asked = deltas(&w).await;
    assert!(service.nudge_full());
    wait_for_deltas(&w, asked).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The forced switch: it holds `lifecycle`, and cannot end while the test holds the tree
    // lock. Waited for however long a loaded machine takes to stop the folder's tasks.
    let switching = Arc::clone(&service);
    let switch = tokio::spawn(async move { switching.drop_pending_uploads().await });
    let taken = tokio::time::timeout(Duration::from_secs(60), async {
        while service.lifecycle.try_write().is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(taken.is_ok(), "the switch never took `lifecycle`");
    // The helper reconnected: the bring-up waits for `lifecycle` for writing.
    let resuming = Arc::clone(&service);
    let bring_up = tokio::spawn(async move { resuming.resume().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!switch.is_finished() && !bring_up.is_finished(), "neither ends while the tree lock is held");
    drop(held);

    let ended = tokio::time::timeout(Duration::from_secs(15), async {
        switch.await.unwrap();
        bring_up.await.unwrap();
    })
    .await;
    assert!(
        ended.is_ok(),
        "the forced switch and the bring-up never end: a cycle, the switch and the bring-up wait for each other"
    );
    service.stop_sync().await;
}

/// A forced switch's drop turns a read-write folder read-only itself: its watcher is stopped
/// and none starts, so the change it dropped is not recorded again, whether or not the folder
/// is told to follow afterwards; and its sync runs on, read-only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forced_drop_turns_the_folder_read_only_and_records_nothing_again() {
    use crate::account::PendingUploads;
    use crate::config::Mode;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    let made = std::process::Command::new("sh").args(["-c", "echo new > docs/new.txt"]).current_dir(w.folder.path()).status().unwrap();
    assert!(made.success());
    assert_eq!(service.pending_uploads().await, 1);

    service.drop_pending_uploads().await;
    let running = |service: &SyncService| service.syncing.lock().unwrap().as_ref().map(|s| s.watcher.is_some());
    assert_eq!(service.mode(), Mode::ReadOnly);
    assert_eq!(running(&service), Some(false), "a sync runs, with no watcher to scan the folder");
    assert_eq!(mode(&w.folder.path().join("docs")), 0o555, "locked again");
    assert_eq!(service.changes_in_store().await.unwrap(), 0, "nothing waits in the store");
    assert!(w.folder.path().join("docs/new.txt").exists());

    // The folder told to follow finds itself turned, and its sync is left running.
    service.follow_mode(Mode::ReadOnly).await;
    assert_eq!(running(&service), Some(false));
    assert_eq!(service.changes_in_store().await.unwrap(), 0);
    service.stop_sync().await;
}

use super::*;
use crate::sync::outbox::KeptBack;
use crate::config::Mode;
use crate::sync::menu::FreeUpWhy;

/// Write design §3.9: the account turning read-write takes the read-only lock off
/// its folder — files `0644`, directories `0755`, the folder itself last — and the
/// sync that starts again leaves it off through a Full reconcile; turning read-only
/// puts it back on at once. A folder brought up read-write with the lock still on — a
/// switch cut short — loses it as it comes up.
#[tokio::test]
async fn the_lock_comes_off_and_goes_back_on_with_the_mode() {
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
    service.hub().set_link(None);

    // Read-write in config.toml again, but the walk never ran: the next bring-up runs it.
    let restarted = connected(&w, true).await;
    restarted.follow_mode(Mode::ReadWrite).await;
    restarted.restore().await;
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
    assert_eq!(modes(), (0o644, 0o755, 0o755));
    restarted.stop_sync().await;
    restarted.hub().set_link(None);

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
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let watching = |service: &SyncService| service.writable();
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
    wait_until("the worker counted the change", || service.state().get().outbox.pending_count == 1).await;

    // A forced switch (only that drops them): the drop, then the
    // folder follows.
    service.drop_pending_uploads().await;
    service.follow_mode(Mode::ReadOnly).await;
    assert!(!watching(&service), "stopped with the sync");
    // A read-only folder uploads nothing; its rows are dropped, the file stays.
    assert_eq!(service.pending_uploads().await, 0);
    // the outbox on the bus: and the bus says so.
    assert_eq!(service.state().get().outbox.pending_count, 0);
    assert!(w.folder.path().join("docs/new.txt").exists());
    service.stop_sync().await;
}

/// the outbox worker on the mode switch's and the watcher's hooks: a read-write folder's sync runs the outbox worker beside
/// the watcher; a file made in the folder is examined, the worker is woken, and the
/// file goes up and is committed — its item id on it, nothing left waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_made_in_a_read_write_folder_is_uploaded() {
    use crate::account::PendingUploads;
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
    assert!(service.writable(), "watched, and its worker runs");
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
            if event.kind == konedrive_tree::ActivityKind::Uploaded {
                return event;
            }
        }
    })
    .await
    .expect("ActivityLog.Added for the upload");
    assert_eq!(event.path, file.display().to_string());
    wait_until("the outbox counted empty", || service.state().get().outbox.pending_count == 0).await;
    assert!(service.state().get().outbox.uploads.is_empty());

    service.follow_mode(Mode::ReadOnly).await;
    assert!(!service.writable(), "stopped with the sync");
    service.stop_sync().await;
}

/// the outbox on the bus, `Pause`: a paused account asks OneDrive for nothing — not on
/// `Refresh`, not after a restart, since the pause is kept in the tree
/// store — until `Resume`; a timed pause ends by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_holds_the_poll_outlasts_a_restart_and_ends_by_itself() {
    const START: i64 = 1_700_000_000;
    let w = world().await;
    // The account's one clock, moved by hand: the pause's timer, its keeper and the poll
    // all read it.
    let clock = testing::ManualClock::at(START);
    let on_the_clock = |link| made(&w, wiring(&w, account(true), Arc::new(StaticToken::new("T"))).clock(&clock).link(Some(link)));
    let service = on_the_clock(link(&w).await);
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.pause_syncing(0).await.unwrap();
    assert_eq!(service.state().get().pause.paused_until, Some(0));
    let before = deltas(&w).await;
    service.refresh().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(deltas(&w).await, before, "a paused account asks OneDrive for nothing");
    service.stop_sync().await;
    service.hub().set_link(None);

    let restarted = on_the_clock(link(&w).await);
    restarted.restore().await;
    restarted.resume().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(restarted.state().get().pause.paused_until, Some(0), "the pause outlasts a restart");
    assert_eq!(deltas(&w).await, before);
    restarted.resume_syncing().await.unwrap();
    wait_for_deltas(&w, before).await;
    assert_eq!(restarted.state().get().pause.paused_until, None);

    restarted.pause_syncing(3600).await.unwrap();
    assert_eq!(restarted.state().get().pause.paused_until, Some(START + 3600));
    let before = deltas(&w).await;
    clock.advance(3599);
    restarted.refresh().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!((restarted.state().get().pause.paused_until, deltas(&w).await), (Some(START + 3600), before), "not before its time");
    clock.advance(1);
    wait_until("the timed pause ends by itself", || restarted.state().get().pause.paused_until.is_none()).await;
    wait_for_deltas(&w, before).await;
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
    let store = testing::tree_store(&service).unwrap();
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
    let row = &rows[0];
    assert_eq!((row.kind.as_str(), row.path.as_str(), row.state.as_str()), ("update", file.to_str().unwrap(), "ready"));
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
    let part = service.menu_part(std::slice::from_ref(&file)).await;
    assert!(part.taken[0].is_some() && part.kept_by.is_none(), "{part:?}");
    assert_eq!(part.free_up_refused, Some(FreeUpWhy::NotUploaded), "the menu is told");
    // And it is told while the store's writer is busy: the menu's question goes through
    // the read-only connection, which does not wait for it.
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
    let part = service.menu_part(std::slice::from_ref(&file)).await;
    let took = start.elapsed();
    assert_eq!(part.free_up_refused, Some(FreeUpWhy::NotUploaded));
    assert!(took < Duration::from_millis(500), "the menu waited for the store's writer: {took:?}");
    holding.join().unwrap().unwrap();
    assert!(matches!(service.free_up(std::slice::from_ref(&file)).await, Err(SyncError::NotUploaded(_))));
    assert_eq!(std::fs::read(&file).unwrap(), b"abc", "still downloaded");

    let seq = rows[0].seq as i64;
    store.call(move |s| s.outbox_set_state(seq, OutboxState::Blocked, Some(&"name-characters".into()), None)).await.unwrap();
    assert_eq!(service.not_uploaded().await.unwrap(), vec![KeptBack { path: file.display().to_string(), reason: "name-characters".to_owned() }]);

    store.call(move |s| s.outbox_set_state(seq, OutboxState::Held, Some(&"mass-delete".into()), None)).await.unwrap();
    assert_eq!(service.confirm_deletes().await.unwrap(), 1);
    assert_eq!(service.outbox(0).await.unwrap()[0].state, "ready");
    store.call(move |s| s.outbox_set_state(seq, OutboxState::Held, Some(&"mass-delete".into()), None)).await.unwrap();
    assert_eq!(service.restore_deletes().await.unwrap(), 1);
    assert!(service.outbox(0).await.unwrap().is_empty());
    service.stop_sync().await;
}

/// While an examination's apply holds the store, the bus still
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
    let store = testing::tree_store(&service).unwrap();
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
    wait_until("BlockedCount counts it", || service.state().get().outbox.blocked_count == 1).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while !service.not_uploaded_summary().await.unwrap().iter().any(|r| r.1 == "name-characters") {
        assert!(std::time::Instant::now() < deadline, "the summary never names the blocked row");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
    assert_eq!(service.state().get().outbox.blocked_count, 1);
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
    let store = testing::tree_store(&service).unwrap();
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
    // `Refresh()` has the worker look at its outbox.
    service.refresh().await.unwrap();
    wait_until("HeldCount counts it", || service.state().get().outbox.held_count == 1).await;

    assert_eq!(service.restore_deletes().await.unwrap(), 1);
    wait_until("the file is placed again at once", || file.exists()).await;
    wait_until("HeldCount is 0 again", || service.state().get().outbox.held_count == 0).await;
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
    service.hub().set_link(None);
    // The next run cannot open its tree store: nothing can tell what waits.
    let tree = w.config.path().join("tree.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", tree.display()));
    }
    std::fs::create_dir(&tree).unwrap();
    let restarted = connected(&w, true).await;
    restarted.resume().await;
    let refused = restarted.dehydrate(&file).await.unwrap_err();
    assert!(matches!(&refused, SyncError::Io(why) if why.contains("cannot tell")), "{refused:?}");
    // The menu says the same: disabled, and that nothing can tell now.
    let part = restarted.menu_part(std::slice::from_ref(&file)).await;
    assert!(part.taken[0].is_some_and(|taken| taken.hydrated), "{part:?}");
    assert_eq!(part.free_up_refused, Some(FreeUpWhy::Unknown));
    assert_eq!(std::fs::read(&file).unwrap(), b"abc", "still downloaded");
    restarted.stop_sync().await;
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

/// The outbox sends for an account whose drive `write_test_drive_ids` does not list: the
/// list is empty here. The worker asks the write gate before each row, and a
/// token seen to reach another drive than the recorded one while it runs sends nothing
/// more: the change waits, and the folder's `LastError` says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlisted_drive_is_sent_to_and_a_token_seen_to_reach_another_drive_sends_nothing_more() {
    use crate::account::PendingUploads;
    use crate::config::Mode;
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

    let parts = testing::parts(&service);
    assert!(parts.persist.store.snapshot().write_test_drive_ids.is_empty(), "the first change went up with no drive listed");
    parts.account.state().update(|s| s.live_drive = "D9".into());
    make("echo second > docs/second.txt");
    assert_eq!(service.pending_uploads().await, 1, "the change is recorded");
    wait_until("the folder says why nothing goes", || {
        crate::status::snapshot::published_error(&service.state().get()).contains("was last seen to reach drive \"D9\"")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sent(&w).await, before, "nothing more is sent");
    assert_eq!(service.pending_uploads().await, 1, "the change waits");
    assert!(testing::parts(&service).account.mode_rechecks() > 0, "and the account is asked to work its mode out again");
    service.stop_sync().await;
}

/// §4.10: while OneDrive asks the uploads to wait, the folder's `LastError` says so, with
/// the time they go on; when the wait is over the change goes up and the note is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_throttle_is_said_in_last_error_until_it_ends() {
    use crate::status::snapshot::published_error;
    use wiremock::matchers::path_regex;
    let w = world().await;
    let mut hasher = konedrive_graph::quickxor::QuickXor::new();
    hasher.update(b"new\n");
    Mock::given(method("POST"))
        .and(path_regex("createUploadSession$"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "5"))
        .up_to_n_times(1)
        .mount(&w.server)
        .await;
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
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let_write(&service);
    service.follow_mode(Mode::ReadWrite).await;
    let made = std::process::Command::new("sh").args(["-c", "echo new > docs/new.txt"]).current_dir(w.folder.path()).status().unwrap();
    assert!(made.success());
    let said = || published_error(&service.state().get());
    wait_until("the folder says OneDrive asked to slow down", || {
        said().starts_with("OneDrive asked to slow down; uploads continue at ")
    })
    .await;
    let file = w.folder.path().join("docs/new.txt");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let done = || xattr::get(&file, konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten().is_some() && said().is_empty();
    while !done() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done(), "the change goes up after the wait, and the note goes: {:?}", said());
    service.stop_sync().await;
}

/// A switch to read-only nobody forced keeps the changes waiting to
/// upload, the folder is locked, and its sync holds its cycles while they wait: no
/// read-only reconcile puts back what they describe. `expired`: the sign-in expired
/// (`invalid_grant`, as the token manager records it); otherwise a sign-out.
async fn a_switch_nobody_forced_keeps_the_changes(expired: bool) {
    use crate::account::PendingUploads;
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
    let account = testing::parts(&service).account.state().clone();
    let follower = tokio::spawn(crate::sync::mode::follow(account.subscribe(), Arc::downgrade(&service)));
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
    service.refresh_now();
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

/// A Forget — and so `Accounts.Remove`, which forgets first — is
/// refused `PendingUploads` while changes wait to be uploaded, and changes nothing; once
/// a forced switch to read-only has dropped them, it goes through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_whose_changes_wait_is_not_forgotten() {
    use crate::account::PendingUploads;
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
    assert!(matches!(crate::dbus::fault::Fault::from(refused), crate::dbus::fault::Fault::Refused(konedrive_dbus::Refusal::PendingUploads, _)));
    assert!(matches!(service.retire().await, Err(SyncError::PendingUploads(_))), "Remove's first step too");
    assert!(service.root().is_some(), "still registered");
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
    assert!(service.state().get().pause.paused_until.is_some());
    service.unregister_root().await.unwrap();
    assert_eq!(service.state().get().pause.paused_until, None);
}

/// the watcher: a folder turning read-write whose watcher cannot start stays locked,
/// and says why; nothing is ever made in it unwatched.
#[tokio::test]
async fn a_read_write_folder_whose_watcher_cannot_start_stays_locked() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    testing::parts(&service).watchers.fail(true);
    service.follow_mode(Mode::ReadWrite).await;
    assert_eq!((mode(w.folder.path()), mode(&w.folder.path().join("docs"))), (0o555, 0o555));
    assert!(service.last_error().contains("stays read-only"), "{}", service.last_error());
    // The account is read-write and the folder is not: `Writable` says so.
    assert_eq!(service.mode(), Mode::ReadWrite);
    assert!(!service.writable());

    // A watcher that starts: the folder is writable, and the sentence is gone.
    testing::parts(&service).watchers.fail(false);
    service.refresh().await.unwrap();
    service.follow_mode(Mode::ReadOnly).await;
    service.follow_mode(Mode::ReadWrite).await;
    assert!(service.writable());
    assert!(!service.last_error().contains("stays read-only"), "{}", service.last_error());
    assert_eq!(mode(&w.folder.path().join("docs")), 0o755);
    service.stop_sync().await;
}

/// Stopping is dropping: a change of the folder that is cut before its end leaves no part
/// of the sync running and none that reads as running, and the next `Refresh()`, or the
/// next change, starts the sync again. Cut at two points: while the change waits for the
/// folder's state (a free-up holds it, waiting for the helper), with the sync told to stop
/// and still in the state; and once it has the state (a bring-up waiting for the helper),
/// with the sync taken out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_cut_before_its_end_leaves_no_sync_and_the_next_one_starts_it() {
    use crate::account::PendingUploads;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let file = w.folder.path().join("docs/f.txt");
    {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let opened = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        use std::io::Write as _;
        (&opened).write_all(b"abc").unwrap();
        konedrive_fs::placeholder::write_state(&opened, konedrive_fs::placeholder::State::Hydrated).unwrap();
        konedrive_fs::placeholder::write_stamp(&opened).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    // Before the lock: a free-up holds the folder's state while the helper does not answer.
    w.helper.forget();
    w.helper.hold(Seen::ClearIgnore);
    let (freeing, target) = (Arc::clone(&service), file.clone());
    let free_up = tokio::spawn(async move { freeing.dehydrate(&target).await });
    wait_until("the free-up asked the helper", || w.helper.seen().contains(&Seen::ClearIgnore)).await;
    let switching = Arc::clone(&service);
    let switch = tokio::spawn(async move { switching.follow_mode(Mode::ReadWrite).await });
    // The switch tells the sync to stop and then waits for the state behind the free-up:
    // a reader of the state (`Skipped()`) stops being answered once it does.
    let waits = tokio::time::timeout(Duration::from_secs(60), async {
        while tokio::time::timeout(Duration::from_millis(200), service.skipped()).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waits.is_ok(), "the switch never came to wait for the folder's state");
    switch.abort();
    let _ = switch.await;
    w.helper.release(Seen::ClearIgnore);
    free_up.await.unwrap().unwrap();
    assert_eq!(service.mode(), Mode::ReadOnly, "the switch never happened");
    let before = deltas(&w).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, before).await;

    // With the lock: the helper is back, and does not answer the registration; the
    // bring-up waits, with the sync taken out of the state, and is dropped there.
    w.helper.forget();
    w.helper.hold(Seen::RegisterRoot);
    let resuming = Arc::clone(&service);
    let bring_up = tokio::spawn(async move { resuming.resume().await });
    wait_until("the bring-up asked the helper", || w.helper.seen().contains(&Seen::RegisterRoot)).await;
    bring_up.abort();
    let _ = bring_up.await;
    w.helper.release(Seen::RegisterRoot);

    // The next change of the folder, whatever it is for, starts the sync it finds stopped.
    let before = deltas(&w).await;
    service.drop_pending_uploads().await;
    wait_for_deltas(&w, before).await;
    service.stop_sync().await;
}

/// the watcher: a read-write folder an earlier run left unlocked, whose sync
/// cannot start now, is locked again: no watcher looks at it.
#[tokio::test]
async fn a_read_write_folder_whose_sync_cannot_start_is_locked_again() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    assert_eq!(mode(w.folder.path()), 0o755);
    service.stop_sync().await;
    service.hub().set_link(None);

    // The next run cannot open its tree store.
    let tree = w.config.path().join("tree.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", tree.display()));
    }
    std::fs::create_dir(&tree).unwrap();
    let restarted = connected(&w, true).await;
    restarted.follow_mode(Mode::ReadWrite).await;
    restarted.restore().await;
    restarted.resume().await;
    assert!(restarted.last_error().contains("tree store"), "{}", restarted.last_error());
    assert_eq!((mode(w.folder.path()), mode(&w.folder.path().join("docs"))), (0o555, 0o555));
    restarted.stop_sync().await;
}

/// The folder itself moved away under a read-write sync (§3.3): the folder reads `error`
/// and says so, its sync stops, what needs the sync is refused `NotUp`, and OneDrive is
/// asked for nothing more — nothing is deleted there because the folder went. `Refresh()`
/// tries to bring it up again: refused while it is gone, refused too while another directory
/// stands at its path (an empty one made in its place is never adopted: nothing is stamped
/// on it, the helper is not told, and no sync starts on it, at a `Refresh()` or at the
/// helper's reconnect), and once the folder is back it is up and in step again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_moved_away_stops_its_sync_and_says_so() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let before = deltas(&w).await;
    service.follow_mode(Mode::ReadWrite).await;
    wait_for_deltas(&w, before).await;

    std::fs::rename(w.folder.path(), w.config.path().join("moved")).unwrap();
    wait_until("the folder reads error", || service.root_state() == "error" && service.last_error().contains("moved or deleted")).await;
    // Its store is still open, and no call is served from it.
    let refused = service.pause_syncing(0).await.unwrap_err();
    assert!(matches!(&refused, SyncError::NotUp(why) if why.contains("moved or deleted")), "{refused:?}");
    let refused = service.outbox(0).await.unwrap_err();
    assert!(matches!(refused, SyncError::NotUp(_)), "{refused:?}");
    let gone = |refused: &SyncError| matches!(refused, SyncError::NotUp(why) if why.contains("tried just now") && why.contains("another folder stands in its place"));
    let refused = service.refresh().await.unwrap_err();
    assert!(gone(&refused), "{refused:?}");
    assert_eq!(service.root_state(), "error");
    let asked = requests(&w).await;

    // An empty directory where the folder was is not the folder.
    std::fs::create_dir(w.folder.path()).unwrap();
    w.helper.forget();
    let refused = service.refresh().await.unwrap_err();
    assert!(gone(&refused), "{refused:?}");
    service.resume().await;
    assert_eq!(service.root_state(), "error");
    assert!(service.last_error().contains("another folder stands in its place"), "{}", service.last_error());
    assert_eq!(xattr::get(w.folder.path(), "user.konedrive.root").unwrap(), None, "the directory was stamped");
    assert!(!w.helper.seen().contains(&Seen::RegisterRoot), "the helper was told of it");
    assert_eq!(std::fs::read_dir(w.folder.path()).unwrap().count(), 0, "something was placed in it");
    std::fs::remove_dir(w.folder.path()).unwrap();
    service.refresh_now();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&w).await, asked, "OneDrive was asked on behalf of a folder that is gone");
    let received = w.server.received_requests().await.unwrap();
    assert!(received.iter().all(|r| r.method.as_str() == "GET"), "something was changed in OneDrive");

    // Put back, it comes up at a `Refresh()`, with no helper's reconnect to wait for.
    std::fs::rename(w.config.path().join("moved"), w.folder.path()).unwrap();
    let before = deltas(&w).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, before).await;
    assert_ne!(service.root_state(), "error", "{}", service.last_error());
    service.stop_sync().await;
}

/// A forced switch to read-only and a bring-up end, with a cycle under way. The switch
/// and the bring-up are each a change of the folder: the change tells the cycle to stop
/// before it waits for the folder's state, waits for the cycle to end once it has the
/// state, and takes no tree lock, so none of the three can wait for another for good.
/// When the switch took the state for reading and left the cycle running, the cycle (the
/// tree lock held, waiting for the state behind the bring-up), the switch (the state held,
/// waiting for the tree lock) and the bring-up waited for each other for good.
///
/// How far the cycle has come when the two changes arrive is not the test's to choose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cycle_a_forced_switch_and_a_bring_up_at_once_all_end() {
    use crate::account::PendingUploads;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let before = deltas(&w).await;
    service.follow_mode(Mode::ReadWrite).await;
    // The read-write sync's first cycle has asked OneDrive: the watcher's first scan is over.
    wait_for_deltas(&w, before).await;

    // A cycle is asked for, and the switch and the bring-up come at once, while it runs.
    service.refresh().await.unwrap();
    let switching = Arc::clone(&service);
    let switch = tokio::spawn(async move { switching.drop_pending_uploads().await });
    // The helper reconnected: the bring-up waits for the state too.
    let resuming = Arc::clone(&service);
    let bring_up = tokio::spawn(async move { resuming.resume().await });

    let ended = tokio::time::timeout(Duration::from_secs(15), async {
        switch.await.unwrap();
        bring_up.await.unwrap();
    })
    .await;
    assert!(
        ended.is_ok(),
        "the forced switch and the bring-up never end: a cycle, the switch and the bring-up wait for each other"
    );
    assert_eq!(service.mode(), Mode::ReadOnly);
    // The sync runs again: a cycle asked for comes.
    let before = deltas(&w).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, before).await;
    service.stop_sync().await;
}

/// A forced switch's drop turns a read-write folder read-only itself: its watcher is stopped
/// and none starts, so the change it dropped is not recorded again, whether or not the folder
/// is told to follow afterwards; and its sync runs on, read-only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forced_drop_turns_the_folder_read_only_and_records_nothing_again() {
    use crate::account::PendingUploads;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    service.follow_mode(Mode::ReadWrite).await;
    let made = std::process::Command::new("sh").args(["-c", "echo new > docs/new.txt"]).current_dir(w.folder.path()).status().unwrap();
    assert!(made.success());
    assert_eq!(service.pending_uploads().await, 1);

    service.drop_pending_uploads().await;
    // Nothing scans the folder, and nothing waits in its store.
    let waiting = |service: Arc<SyncService>| async move { service.outbox(0).await.map(|rows| rows.len()) };
    assert_eq!(service.mode(), Mode::ReadOnly);
    assert!(!service.writable(), "no watcher to scan the folder");
    assert_eq!(mode(&w.folder.path().join("docs")), 0o555, "locked again");
    assert!(matches!(waiting(Arc::clone(&service)).await, Ok(0)), "nothing waits in the store");
    // And its sync runs on, read-only: a cycle asked for comes.
    let before = deltas(&w).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, before).await;
    assert!(w.folder.path().join("docs/new.txt").exists());

    // The folder told to follow finds itself turned, and its sync is left running.
    service.follow_mode(Mode::ReadOnly).await;
    assert!(!service.writable());
    assert!(matches!(waiting(Arc::clone(&service)).await, Ok(0)));
    service.stop_sync().await;
}

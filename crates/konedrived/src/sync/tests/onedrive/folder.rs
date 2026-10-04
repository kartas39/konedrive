use super::*;

/// `WebUrl` (issue #53): the address of the page of a file, of a folder and of
/// the account's folder itself, each from one GET and nothing else.
#[tokio::test]
async fn web_url_asks_onedrive_for_the_items_page_with_one_get() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    mount_page(&w, "/me/drive/items/F", "F", "https://onedrive.example/f").await;
    mount_page(&w, "/me/drive/items/D", "D", "https://onedrive.example/docs").await;
    mount_page(&w, "/me/drive/root", "R", "https://onedrive.example/root").await;
    let before = requests(&w).await;

    let file = w.folder.path().join("docs/f.txt");
    assert_eq!(service.web_url(&file).await.unwrap(), "https://onedrive.example/f");
    assert_eq!(service.web_url(&w.folder.path().join("docs")).await.unwrap(), "https://onedrive.example/docs");
    assert_eq!(service.root_web_url().await.unwrap(), "https://onedrive.example/root");

    let asked: Vec<(String, String)> = w.server.received_requests().await.unwrap()[before..]
        .iter()
        .map(|r| (r.method.to_string(), r.url.path().to_owned()))
        .collect();
    let get = |route: &str| ("GET".to_owned(), route.to_owned());
    assert_eq!(asked, [get("/me/drive/items/F"), get("/me/drive/items/D"), get("/me/drive/root")]);
    assert_eq!(
        konedrive_fs::placeholder::read_state(&std::fs::File::open(&file).unwrap()).unwrap(),
        Some(konedrive_fs::placeholder::State::OnlineOnly),
        "asking for the page downloads nothing"
    );
    service.stop_sync().await;
}

/// A file with no item id is not in OneDrive yet: refused by that name, and
/// OneDrive is not asked.
#[tokio::test]
async fn web_url_of_a_file_not_uploaded_yet_is_refused_without_asking() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    std::fs::set_permissions(w.folder.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let new = w.folder.path().join("new.txt");
    std::fs::write(&new, b"new").unwrap();
    konedrive_fs::placeholder::write_state(&std::fs::File::open(&new).unwrap(), konedrive_fs::placeholder::State::Hydrated).unwrap();
    let before = requests(&w).await;

    let refused = service.web_url(&new).await.unwrap_err();
    assert!(matches!(refused, SyncError::NotInOneDrive(_)), "{refused:?}");
    assert!(matches!(crate::dbus::fault::to_fault(refused), crate::dbus::fault::Fault::Refused(konedrive_dbus::Refusal::NotUploaded, _)));
    assert_eq!(requests(&w).await, before);
    service.stop_sync().await;
}

/// OneDrive answering 503 until the retries run out is `Unreachable`, not a
/// plain failure; an answer without an address, and an item gone, are failures.
#[tokio::test]
async fn web_url_says_when_onedrive_could_not_be_reached() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path("/me/drive/items/D"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D"})))
        .mount(&w.server).await;

    let refused = service.web_url(&w.folder.path().join("docs/f.txt")).await.unwrap_err();
    assert!(matches!(refused, SyncError::Unreachable(_)), "{refused:?}");
    assert!(matches!(crate::dbus::fault::to_fault(refused), crate::dbus::fault::Fault::Refused(konedrive_dbus::Refusal::Unreachable, _)));
    let refused = service.web_url(&w.folder.path().join("docs")).await.unwrap_err();
    assert!(matches!(&refused, SyncError::Io(why) if why.contains("no address")), "{refused:?}");
    service.stop_sync().await;
}

#[tokio::test]
async fn a_folder_registered_while_signed_in_shows_onedrive_read_only() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let file = w.folder.path().join("docs/f.txt");
    assert!(file.is_file());
    assert_eq!(config_of(&w).sync_root_source, "onedrive");
    assert_eq!((mode(&file), mode(&w.folder.path().join("docs"))), (0o444, 0o555));
    assert_eq!(service.root_state(), "ready");
    // A fresh OneDrive folder is excluded from KDE's
    // Baloo indexer, so reading a placeholder to index it does not
    // download the whole drive.
    let folder = std::fs::canonicalize(w.folder.path()).unwrap();
    assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));
    assert!(config_of(&w).sync_root_baloo_excluded);
    service.stop_sync().await;
}

/// Design §8.3 (test 7): a OneDrive folder remembers its account's
/// drive — written once the first cycle has recorded it, and at the
/// bring-up of a folder from before multiple accounts, which carries
/// none — and, forgotten, it is refused `NotEmpty` to another account,
/// while its own account may register it again.
#[tokio::test]
async fn a_onedrive_folder_remembers_its_drive_and_is_refused_to_another_account() {
    use std::os::unix::fs::PermissionsExt;
    let w = world().await;
    let drive = || xattr::get(w.folder.path(), konedrive_fs::placeholder::XATTR_DRIVE).unwrap();
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    assert_eq!(drive().as_deref(), Some(&b"D1"[..]), "written with the drive the first cycle recorded");
    service.stop_sync().await;
    drop(service);

    // A folder from before carries no drive: its first bring-up writes it.
    let open = |mode| std::fs::set_permissions(w.folder.path(), std::fs::Permissions::from_mode(mode)).unwrap();
    open(0o755);
    xattr::remove(w.folder.path(), konedrive_fs::placeholder::XATTR_DRIVE).unwrap();
    open(0o555);
    {
        let restarted = connected(&w, true).await;
        restarted.restore().await;
        restarted.resume().await;
        assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
        assert_eq!(drive().as_deref(), Some(&b"D1"[..]));
        restarted.unregister_root().await.unwrap();
    }

    // The world's helper serves one connection at a time: each service
    // here goes before the next one connects.
    {
        let elsewhere = tempfile::tempdir().unwrap();
        let other = persist(&elsewhere.path().join("config.toml"));
        other.store.record_drive(&other.account, &crate::config::DriveId::new("D2").unwrap()).unwrap();
        let stranger = testing::service(Some(link(&w).await), Some(account(true)), Some(other));
        let refused = stranger.register_root(w.folder.path()).await;
        assert!(matches!(refused, Err(SyncError::ForeignFolder)), "{refused:?}");
    }

    let own = connected(&w, true).await;
    own.register_root(w.folder.path()).await.unwrap();
    own.stop_sync().await;
}

/// A folder the user has already excluded from Baloo —
/// themselves, or through a parent directory — is never added again,
/// and a later Forget must not remove an exclusion this daemon did
/// not add.
#[tokio::test]
async fn baloo_leaves_a_folder_the_user_already_excluded_alone() {
    let w = world().await;
    let folder = std::fs::canonicalize(w.folder.path()).unwrap();
    mark_already_excluded(&w, &folder);
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    assert_eq!(baloo_calls(&w), "", "already excluded, so nothing is added");
    assert!(!config_of(&w).sync_root_baloo_excluded);

    service.unregister_root().await.unwrap();
    assert_eq!(baloo_calls(&w), "", "we never added it, so Forget must not remove it");
}

/// Whether this daemon added the exclusion is persisted
/// (`sync_root_baloo_excluded` in `config.toml`), so a restart
/// between a registration and its Forget still gets the Forget
/// right — the exclusion comes off, and it is not re-checked or
/// re-added at the restart in between.
#[tokio::test]
async fn baloo_exclusion_survives_a_restart_and_is_still_removed_on_forget() {
    let w = world().await;
    let folder = std::fs::canonicalize(w.folder.path()).unwrap();
    {
        let first = connected(&w, true).await;
        first.register_root(w.folder.path()).await.unwrap();
        first.stop_sync().await;
    }
    let after_first = format!("config add excludeFolders {}\n", folder.display());
    assert_eq!(baloo_calls(&w), after_first);
    assert!(config_of(&w).sync_root_baloo_excluded);

    let second = connected(&w, false).await;
    second.restore().await;
    second.resume().await;
    assert_eq!(baloo_calls(&w), after_first, "not re-checked or re-added at a restart");
    assert!(config_of(&w).sync_root_baloo_excluded, "the flag survives the restart");

    second.unregister_root().await.unwrap();
    assert_eq!(baloo_calls(&w), format!("{after_first}config rm excludeFolders {}\n", folder.display()));
}

/// the exclusion used to be tried only by a
/// fresh registration's commit. A registration kept after it failed
/// (the helper could not confirm it let go) commits nothing, and when
/// it was brought up later nothing asked Baloo again — the folder
/// stayed indexed, and Baloo downloaded the whole drive. Every commit
/// of a folder not recorded as excluded asks now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_kept_after_a_failed_registration_is_kept_out_of_baloo_when_brought_up() {
    let w = world().await;
    let folder = std::fs::canonicalize(w.folder.path()).unwrap();
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let service = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
    helper.refuse(Seen::RegisterRoot, libc::EIO);
    helper.refuse(Seen::UnregisterRoot, libc::EIO);
    service.register_root(w.folder.path()).await.unwrap_err();
    assert!(service.root().is_some(), "kept: the helper may still hold it");
    assert_eq!(baloo_calls(&w), "");

    helper.refuse(Seen::RegisterRoot, 0);
    service.resume().await;

    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));
    assert!(config_of(&w).sync_root_baloo_excluded);
    service.stop_sync().await;
}

/// A `SyncService` made with a wiring that says nothing of Baloo — as a
/// test that forgot to, would be — has a `Baloo` that runs
/// no program at all, so it never reaches the real `balooctl6` or
/// `~/.config/baloofilerc`, on this host or the one running CI. This
/// deliberately does not go through `service`/`service_with`, which
/// always give the fake.
#[tokio::test]
async fn a_service_made_with_no_baloo_runs_no_program_on_registration() {
    let w = world().await;
    // No Baloo said: the default `Baloo::disabled()` stands.
    let wiring = testing::wiring()
        .link(Some(link(&w).await))
        .account(account(true))
        .persist(persist(&w.config.path().join("config.toml")))
        .onedrive(drive(&w, Arc::new(StaticToken::new("T"))), sync_paths(&w));
    let service = made(&w, wiring);

    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;

    assert!(!w.baloo.path().join("calls").exists(), "the fake was never even pointed to");
    assert!(!config_of(&w).sync_root_baloo_excluded, "nothing ran, so nothing was excluded");
    service.stop_sync().await;
}

/// A folder registered signed out is local, as in part 1 — and, since
/// HS2, so is every folder registered without interception, signed
/// in or not: that is the developer's mode, filled from a directory.
#[tokio::test]
async fn a_folder_registered_while_signed_out_or_without_interception_is_local() {
    for signed_in in [false, true] {
        let w = world().await;
        let service = service(&w, signed_in);
        service.register_root_without_interception(w.folder.path()).await.unwrap();
        assert_eq!(config_of(&w).sync_root_source, "local", "signed in: {signed_in}");
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("a.txt"), b"abc").unwrap();
        assert_eq!(service.populate_from_directory(source.path()).await.unwrap(), 1);
        assert_eq!(mode(&w.folder.path().join("a.txt")), 0o644, "no lock on a local folder");
        assert_eq!(requests(&w).await, 0, "a local folder never asks OneDrive");
    }
}

#[tokio::test]
async fn populating_a_onedrive_folder_from_a_directory_is_refused() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    let source = tempfile::tempdir().unwrap();
    let err = service.populate_from_directory(source.path()).await.unwrap_err();
    assert!(matches!(err, SyncError::Unsupported(_)), "{err:?}");
    service.stop_sync().await;
}

#[tokio::test]
async fn forgetting_a_onedrive_folder_stops_its_sync_unlocks_it_and_drops_its_tree() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    // A reader of the test's own — another program reading the store,
    // `sqlite3` say — so that the daemon's connection is not the last
    // one: SQLite then leaves its journal files when that closes, and
    // only the Forget itself removes them.
    let reader = rusqlite::Connection::open_with_flags(w.config.path().join("tree.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    reader.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0)).unwrap();
    for name in ["tree.sqlite-wal", "tree.sqlite-shm"] {
        assert!(w.config.path().join(name).exists(), "no {name} to remove");
    }
    service.unregister_root().await.unwrap();
    let file = w.folder.path().join("docs/f.txt");
    assert!(file.is_file(), "the files stay (spec §3.1)");
    assert_eq!((mode(&file), mode(&w.folder.path().join("docs"))), (0o644, 0o755));
    for name in ["tree.sqlite", "tree.sqlite-wal", "tree.sqlite-shm"] {
        assert!(!w.config.path().join(name).exists(), "{name} was left");
    }
    drop(reader);
    assert_eq!(service.items(), (0, 0, 0));
    assert_eq!(service.root_state(), "none");
    assert_eq!(config_of(&w).sync_root_source, "local");
    let before = requests(&w).await;
    service.refresh_now();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(requests(&w).await, before, "nothing syncs any more");
}

/// A restart brings a OneDrive folder back syncing, and its first
/// cycle reconciles the whole folder: the stored link has
/// no changes since, so only a Full reconcile puts back the file
/// removed while the daemon was down.
///
/// The restarted daemon reads "signed out" (its Graph token here is
/// static, so the cycle still succeeds): a restored folder keeps the
/// source `config.toml` records, and a restart after a sign-out must
/// not turn a OneDrive folder into a local one.
#[tokio::test]
async fn a_restart_brings_a_onedrive_folder_back_and_repairs_it() {
    let w = world().await;
    {
        let first = connected(&w, true).await;
        first.register_root(w.folder.path()).await.unwrap();
        listed(&first).await;
        first.stop_sync().await;
    }
    assert_eq!(config_of(&w).sync_root_source, "onedrive");
    std::process::Command::new("chmod").args(["-R", "u+w"]).arg(w.folder.path()).status().unwrap();
    std::fs::remove_file(w.folder.path().join("docs/f.txt")).unwrap();
    let listings = full_listings(&w).await;

    let second = connected(&w, false).await;
    second.restore().await;
    second.resume().await;
    wait_until("repaired by the first cycle's Full reconcile", || {
        w.folder.path().join("docs/f.txt").is_file()
    })
    .await;
    assert_eq!(full_listings(&w).await, listings, "asked from the stored link, not listed again");
    assert_eq!(config_of(&w).sync_root_source, "onedrive");
    second.stop_sync().await;
}

/// A folder that reads "signed out" is brought up to date the moment
/// the account signs in again, not up to a poll interval later (an
/// hour here).
#[tokio::test]
async fn signing_in_brings_a_folder_that_reads_signed_out_up_to_date_at_once() {
    let w = world().await;
    let account = account(true);
    let tokens = Arc::new(AccountTokens { account: account.clone(), refused: AtomicUsize::new(0) });
    let service = service_with(&w, account.clone(), Some(link(&w).await), Arc::clone(&tokens) as Arc<dyn TokenSource>);
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;

    account.update(|s| s.state = SignInState::SignedOut);
    service.refresh_now();
    wait_until("the folder reads signed out", || service.root_state() == "error").await;
    assert!(service.last_error().contains("signed out"), "{}", service.last_error());
    // The one retry the schedule has, and then the hour-long wait.
    wait_until("the retry failed too", || tokens.refused.load(Ordering::SeqCst) >= 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(tokens.refused.load(Ordering::SeqCst), 2, "the poller waits out its interval now");

    let before = deltas(&w).await;
    account.update(|s| s.state = SignInState::SigningIn);
    account.update(|s| s.state = SignInState::SignedIn);
    wait_until("the folder is in step again", || service.root_state() == "ready").await;
    assert_eq!(deltas(&w).await, before + 1);
    service.stop_sync().await;
}

/// A Forget the helper refuses keeps the folder registered — and so
/// locked, and kept in step.
#[tokio::test]
async fn a_forget_the_helper_refuses_leaves_the_folder_locked_and_in_step() {
    let w = world().await;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let service = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    helper.refuse(Seen::UnregisterRoot, libc::EIO);

    let refused = service.unregister_root().await;

    assert!(matches!(refused, Err(SyncError::Helper(_))), "{refused:?}");
    assert_eq!(mode(&w.folder.path().join("docs/f.txt")), 0o444);
    assert!(w.config.path().join("tree.sqlite").exists());
    assert_eq!(config_of(&w).sync_root_source, "onedrive");
    let before = deltas(&w).await;
    service.refresh().await.unwrap();
    wait_for_deltas(&w, before).await;
    service.stop_sync().await;
}

/// Two changes at once leave one folder and no sync nobody could stop: a Forget whose
/// answer the helper has not given yet, and the bring-up of a helper's reconnect asked
/// meanwhile. The bring-up waits for the Forget, finds no folder, and starts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bring_up_asked_while_a_forget_waits_starts_no_sync() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;

    w.helper.forget();
    w.helper.hold(Seen::UnregisterRoot);
    let forgetting = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.unregister_root().await })
    };
    wait_until("the Forget reached the helper", || w.helper.seen().contains(&Seen::UnregisterRoot)).await;
    let resuming = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.resume().await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!resuming.is_finished(), "the bring-up did not wait for the Forget");
    w.helper.release(Seen::UnregisterRoot);
    forgetting.await.unwrap().unwrap();
    resuming.await.unwrap();

    assert_eq!(service.root_state(), "none");
    assert!(!w.helper.seen().contains(&Seen::RegisterRoot), "the forgotten folder was registered again");
    let before = requests(&w).await;
    service.refresh_now();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&w).await, before, "a sync runs on a forgotten folder");
    assert_eq!(mode(&w.folder.path().join("docs/f.txt")), 0o644);
}

/// A restarted daemon whose helper is connecting: the bring-up waits for the helper's
/// answer, and holds the folder's state meanwhile.
async fn bringing_up(w: &World) -> (Arc<SyncService>, tokio::task::JoinHandle<()>) {
    {
        let first = connected(w, true).await;
        first.register_root(w.folder.path()).await.unwrap();
        listed(&first).await;
        first.stop_sync().await;
        first.hub().set_link(None);
    }
    let restarted = connected(w, true).await;
    restarted.restore().await;
    w.helper.forget();
    w.helper.hold(Seen::RegisterRoot);
    let resuming = {
        let service = Arc::clone(&restarted);
        tokio::spawn(async move { service.resume().await })
    };
    wait_until("the bring-up reached the helper", || w.helper.seen().contains(&Seen::RegisterRoot)).await;
    (restarted, resuming)
}

/// A Forget that arrives while a bring-up is under way waits for it, and then stops the
/// sync the bring-up started before the folder and its tree store go: no sync is left on a
/// forgotten folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forget_that_waited_for_a_bring_up_stops_the_sync_it_started() {
    let w = world().await;
    let (service, resuming) = bringing_up(&w).await;
    let forgetting = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.unregister_root().await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!forgetting.is_finished(), "the Forget did not wait for the bring-up");
    w.helper.release(Seen::RegisterRoot);
    resuming.await.unwrap();
    forgetting.await.unwrap().unwrap();

    assert_eq!(service.root_state(), "none");
    assert!(!w.config.path().join("tree.sqlite").exists(), "the tree store stays");
    let before = requests(&w).await;
    service.refresh_now();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&w).await, before, "a sync runs on a forgotten folder");
}

/// A registration that arrives while a bring-up is under way waits for it and is refused:
/// the folder is the account's already. The refusal leaves the sync the bring-up started
/// running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_refused_after_a_bring_up_leaves_its_sync_running() {
    let w = world().await;
    let (service, resuming) = bringing_up(&w).await;
    let registering = {
        let (service, path) = (Arc::clone(&service), w.folder.path().to_path_buf());
        tokio::spawn(async move { service.register_root(&path).await })
    };
    // Not the refusal made from the view: the call is inside `change()`, behind the
    // bring-up, and ends only once that has.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!registering.is_finished(), "the registration did not wait for the bring-up");
    w.helper.release(Seen::RegisterRoot);
    resuming.await.unwrap();
    let refused = registering.await.unwrap();
    assert!(matches!(refused, Err(SyncError::AlreadyRegistered)), "{refused:?}");

    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    // Only a running sync answers a nudge.
    let before = deltas(&w).await;
    service.refresh_now();
    wait_for_deltas(&w, before).await;
    service.stop_sync().await;
}

/// A folder whose sync could not start (F18: its tree store could not
/// be opened) is not reported as refreshed: `Refresh()` tries to start
/// it again, says why when it still cannot, and starts it once it can.
#[tokio::test]
async fn refresh_starts_a_sync_that_could_not_start_or_says_why() {
    let w = world().await;
    // A file where the tree store's directory has to be.
    let blocker = w.config.path().join("state");
    std::fs::write(&blocker, b"").unwrap();
    let paths = SyncPaths { tree_db: blocker.join("tree.sqlite"), ..sync_paths(&w) };
    let tokens: Arc<dyn TokenSource> = Arc::new(StaticToken::new("T"));
    let wiring = wiring(&w, account(true), Arc::clone(&tokens)).onedrive(drive(&w, tokens), paths).link(Some(link(&w).await));
    let service = made(&w, wiring);
    service.register_root(w.folder.path()).await.unwrap();
    assert_eq!(service.root_state(), "error");

    let refused = service.refresh().await;
    assert!(
        matches!(&refused, Err(SyncError::NotUp(why)) if why.contains("the tree store cannot be opened")),
        "{refused:?}"
    );
    assert_eq!(requests(&w).await, 0, "nothing synced");

    std::fs::remove_file(&blocker).unwrap();
    service.refresh().await.unwrap();
    // The state leaves `error` when the cycle ends, after its counts are published.
    listed(&service).await;
    wait_until("the cycle ended", || service.root_state() == "ready").await;
    service.stop_sync().await;
}

/// A folder held at startup until its helper is back has not been
/// brought up — nor recovered — yet: `Refresh()` says so rather than
/// start its sync ahead of that.
#[tokio::test]
async fn refresh_of_a_folder_waiting_for_its_helper_says_so() {
    let w = world().await;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = FakeHelper::start(socket_path.clone());
    {
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let first = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
        first.register_root(w.folder.path()).await.unwrap();
        listed(&first).await;
        first.stop_sync().await;
    }
    let restarted = service(&w, true);
    restarted.restore().await;
    let before = requests(&w).await;

    let refused = restarted.refresh().await;

    assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(requests(&w).await, before, "a sync started ahead of the bring-up");
    // What needs the folder's sync says the truth: a folder is recorded, and it is not up.
    let waits = |refused: SyncError| matches!(&refused, SyncError::NotUp(why) if why.contains("waits for the konedrive helper"));
    assert!(waits(restarted.pause_syncing(0).await.unwrap_err()));
    assert!(waits(restarted.resume_syncing().await.unwrap_err()));
    assert!(waits(restarted.outbox(0).await.unwrap_err()));
    restarted.stop_sync().await;
}

/// A OneDrive folder is locked read-only after its first listing,
/// the folder itself too, and bringing it up again after a restart
/// re-checked it with a write probe — refused, so no locked folder came
/// back after a restart, in either mode: "cannot bring up the sync
/// folder: Permission denied". Found by, whose switch to
/// interception goes through the same check. A folder that already
/// carries its root id was probed when it was first registered, and is
/// not probed again (the helper probes nothing). The mode without interception is
/// a folder recorded that way before HS2 (`legacy_without_interception`):
/// no new OneDrive folder is made so.
#[tokio::test]
async fn a_locked_onedrive_folder_comes_back_after_a_restart_in_either_mode() {
    for intercepted in [false, true] {
        let w = world().await;
        {
            let first = connected(&w, true).await;
            first.register_root(w.folder.path()).await.unwrap();
            listed(&first).await;
            first.stop_sync().await;
            first.hub().set_link(None);
        }
        if !intercepted {
            legacy_without_interception(&w);
        }
        assert_eq!(mode(w.folder.path()), 0o555, "the folder itself is locked");

        let restarted = connected(&w, true).await;
        restarted.restore().await;
        restarted.resume().await;

        assert!(
            !restarted.last_error().contains("cannot bring up"),
            "intercepted = {intercepted}: {}",
            restarted.last_error()
        );
        assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
        assert_eq!(mode(w.folder.path()), 0o555, "and it stays locked");
        restarted.stop_sync().await;
    }
}

/// Rewrites `config.toml` as a daemon from before HS2 left a folder
/// that shows OneDrive registered without interception on purpose —
/// with a helper connected, so not one to switch.
fn legacy_without_interception(w: &World) {
    let persist = persist(&w.config.path().join("config.toml"));
    persist
        .store
        .update_account(&persist.account, |account| {
            let root = account.root.as_mut().expect("a folder");
            root.intercepted = false;
            root.upgrade_when_helper = Some(false);
            Ok::<_, crate::config::ConfigError>(())
        })
        .unwrap();
}

/// HS2: a folder that shows OneDrive and is not intercepted — as a
/// daemon from before HS left one registered on purpose, with a helper
/// connected — is not kept in step while there is no helper: OneDrive
/// is not asked, `Refresh()` is refused `NoHelper`, and the folder
/// reads `error` with the helper's advice first in `LastError`. When
/// the helper connects it switches to interception whatever it was
/// registered as (switch; there is no "on purpose" for a
/// OneDrive folder any more). And, Ruling 1: the switch keeps
/// invariant M1 for everything its sync places afterwards — the sync
/// starts intercepted, so a folder that arrives from the drive later is
/// marked before it is filled.
#[tokio::test]
async fn a_onedrive_folder_without_interception_waits_for_the_helper_then_switches() {
    let w = world().await;
    {
        let first = connected(&w, true).await;
        first.register_root(w.folder.path()).await.unwrap();
        listed(&first).await;
        first.stop_sync().await;
    }
    legacy_without_interception(&w);

    // From now on the drive holds a new folder, `new/g.txt`.
    w.server.reset().await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                {"id": "N", "name": "new", "folder": {}, "parentReference": {"id": "R"}},
                {"id": "G", "name": "g.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "N"}}
            ],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L2", w.server.uri())
        })))
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L2", w.server.uri())})))
        .mount(&w.server).await;

    // A restart with no helper.
    let service = service(&w, true);
    service.restore().await;
    service.resume().await;
    assert_eq!(service.root_state(), "error");
    assert!(service.last_error().starts_with("the konedrive helper is not connected"), "{}", service.last_error());
    assert!(matches!(service.refresh().await, Err(SyncError::NoHelper)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(deltas(&w).await, 0, "OneDrive was asked with no helper");

    // The helper starts, and connects.
    w.helper.forget();
    service.hub().set_link(Some(link(&w).await));
    service.resume().await;

    wait_until("the new folder was placed", || w.folder.path().join("new/g.txt").exists()).await;
    let seen = w.helper.seen();
    assert_eq!(seen.first(), Some(&Seen::RegisterRoot), "{seen:?}: {}", service.last_error());
    let marks: Vec<_> = seen.iter().filter(|s| matches!(s, Seen::MarkDir { .. })).collect();
    assert!(!marks.is_empty(), "the sync placed a directory after the switch without marking it: {seen:?}");
    assert!(
        marks.iter().all(|s| matches!(s, Seen::MarkDir { entries: 0 })),
        "a directory was filled before it was marked: {seen:?}"
    );
    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    service.stop_sync().await;
}

/// `Skipped()` and a Forget, which removes the tree store, never have the store at the
/// same time: a `Skipped()` asked while a Forget is under way — the helper has not
/// answered it yet — waits for it, and then answers for a forgotten folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skipped_asked_during_a_forget_waits_for_it() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;

    w.helper.hold(Seen::UnregisterRoot);
    let forgetting = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.unregister_root().await })
    };
    wait_until("the Forget asked the helper", || w.helper.seen().contains(&Seen::UnregisterRoot)).await;
    let reading = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.skipped().await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!reading.is_finished(), "Skipped() read the tree while the Forget was under way");
    w.helper.release(Seen::UnregisterRoot);
    forgetting.await.unwrap().unwrap();
    assert_eq!(reading.await.unwrap().unwrap(), Vec::<(String, String)>::new());
}

/// Whose file an open is, and whose item an id is, by the tree store of a running sync
/// (design §2.4 step 3, write design §8.3): with two folders on one filesystem, a file
/// unlinked while its open waits is routed to the account whose tree knows its item id;
/// and that id, and any id that names that account's drive, is claimed for every other
/// account, never for its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_whose_tree_knows_an_item_is_routed_its_opens_and_claims_it() {
    use std::os::fd::OwnedFd;
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let hub = Arc::clone(service.hub());
    let other_dir = tempfile::tempdir().unwrap();
    let other = testing::wiring().hub(&hub).build();
    other.register_root_without_interception(other_dir.path()).await.unwrap();

    // A file of the other folder, carrying an id the OneDrive folder's tree knows, unlinked.
    std::fs::write(other_dir.path().join("h"), b"").unwrap();
    xattr::set(other_dir.path().join("h"), konedrive_fs::placeholder::XATTR_ITEM_ID, b"F").unwrap();
    let unlinked: OwnedFd = std::fs::File::open(other_dir.path().join("h")).unwrap().into();
    std::fs::remove_file(other_dir.path().join("h")).unwrap();
    let routed = hub.route(&unlinked).await;
    assert!(routed.is_some_and(|account| Arc::ptr_eq(&account, &service)), "found by its item id");

    let (ours, theirs) = (Arc::downgrade(&service), Arc::downgrade(&other));
    let claimed = |of: &std::sync::Weak<SyncService>, id: &str| konedrive_tree::off_runtime(|| hub.claimed_elsewhere(of, id));
    assert!(claimed(&theirs, "F"), "the OneDrive folder's tree knows it");
    assert!(claimed(&theirs, "d1!42"), "the id names its drive");
    assert!(!claimed(&theirs, "DEF456!42"));
    assert!(!claimed(&ours, "F"), "never one's own");

    // The store is the folder's, not the running sync's: with the sync stopped, the tree
    // still claims its items, and what waits is still answered.
    service.stop_sync().await;
    assert!(claimed(&theirs, "F"), "claimed while the sync is stopped");
    assert!(service.outbox(0).await.unwrap().is_empty());

    // Kept while the folder is down too: a bring-up that fails leaves the store open and
    // the source in place, so the tree still claims its items (another account's reconcile
    // sets a file of this account aside rather than remove it), what waits is still
    // counted, and a file is still downloaded.
    w.helper.refuse(Seen::RegisterRoot, libc::EIO);
    service.resume().await;
    assert_eq!(service.root_state(), "error", "{}", service.last_error());
    assert!(matches!(service.outbox(0).await, Err(SyncError::NotUp(_))), "down: what needs the sync is refused");
    assert!(claimed(&theirs, "F"), "claimed while the folder is down");
    {
        use crate::account::PendingUploads;
        assert_eq!(service.pending_uploads().await, 0, "the store answers");
    }
    let content = tempfile::tempdir().unwrap();
    std::fs::write(content.path().join("F"), b"abc").unwrap();
    super::super::install_source(&service, Arc::new(crate::hydration::source::LocalDir::new(content.path())));
    let file = w.folder.path().join("docs/f.txt");
    service.hydrate_now(&file).await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"abc", "filled from the folder's source while it is down");

    // And a Forget closes it: nothing of the stopped sync keeps the store's file open.
    service.unregister_root().await.unwrap();
    let tree = w.config.path().join("tree.sqlite").display().to_string();
    let open = || {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .any(|target| target.to_string_lossy().starts_with(&tree))
    };
    wait_until("the tree store is closed", || !open()).await;
    assert!(!claimed(&theirs, "F"), "a forgotten folder claims nothing");
}

/// The same for a read-write sync, whose parts hold more of each other: once the folder is
/// forgotten nothing keeps the tree store open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forgotten_read_write_folder_leaves_no_tree_store_open() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let before = deltas(&w).await;
    service.follow_mode(crate::config::Mode::ReadWrite).await;
    wait_for_deltas(&w, before).await;
    wait_until("the folder is writable", || service.writable()).await;
    let tree = w.config.path().join("tree.sqlite").display().to_string();
    let open = || {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .any(|target| target.to_string_lossy().starts_with(&tree))
    };
    assert!(open(), "open while the folder is up");
    service.unregister_root().await.unwrap();
    wait_until("the tree store is closed", || !open()).await;
}

/// Two changes of the folder, the second queued behind the first: a bring-up that waits
/// for the helper, and a Forget that comes meanwhile. The bring-up starts the sync again
/// and the Forget takes it out; while the Forget waits for the helper nothing reads as
/// running or writable, and both end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_queued_behind_another_leaves_no_sync_that_reads_as_running() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let_write(&service);
    let before = deltas(&w).await;
    service.follow_mode(crate::config::Mode::ReadWrite).await;
    wait_for_deltas(&w, before).await;
    wait_until("the folder is writable", || service.writable()).await;

    w.helper.forget();
    w.helper.hold(Seen::RegisterRoot);
    w.helper.hold(Seen::UnregisterRoot);
    let resuming = Arc::clone(&service);
    let bring_up = tokio::spawn(async move { resuming.resume().await });
    wait_until("the bring-up asked the helper", || w.helper.seen().contains(&Seen::RegisterRoot)).await;
    assert!(!service.writable(), "told to stop");
    let forgetting = Arc::clone(&service);
    let forget = tokio::spawn(async move { forgetting.unregister_root().await });
    // The Forget waits for the folder's state: a reader of it stops being answered.
    let waits = tokio::time::timeout(Duration::from_secs(60), async {
        while tokio::time::timeout(Duration::from_millis(200), service.skipped()).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waits.is_ok(), "the Forget never came to wait for the folder's state");
    w.helper.release(Seen::RegisterRoot);
    bring_up.await.unwrap();
    wait_until("the Forget asked the helper", || w.helper.seen().contains(&Seen::UnregisterRoot)).await;
    // The sync the bring-up started is taken out again, and nothing says otherwise.
    assert!(!service.writable(), "no sync reads as writable while the Forget waits");
    w.helper.release(Seen::UnregisterRoot);
    forget.await.unwrap().unwrap();
    assert_eq!(service.root_state(), "none");
}

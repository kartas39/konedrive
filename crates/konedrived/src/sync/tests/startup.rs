use super::*;

// --- Startup and the helper supervisor ----------

/// §3.1's "persisted, so it survives a restart" — and §4.4's walk, which
/// without it never ran at a startup at all: recovery only ever ran
/// inside a `RegisterRoot` call, so after a crash a file left
/// `hydrating` stayed that way until a human registered the folder
/// again.
///
/// order is pinned here too, for the first time: the helper
/// records what it was asked, and the registration must come before the
/// `ClearIgnore` recovery sends for the interrupted file. Both fake
/// helpers used to discard everything they received, so nothing could
/// tell the two orders apart.
#[tokio::test]
async fn a_persisted_root_comes_back_at_the_next_start_and_is_recovered_after_it() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    let resolved = std::fs::canonicalize(root_dir.path()).unwrap();

    {
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let service = SyncService::new(Some(link), None, Some(persist(&config_file)));
        service.register_root(root_dir.path()).await.unwrap();
    }
    assert_eq!(
        Config::load(&config_file).unwrap().sync_root,
        resolved.display().to_string(),
        "the root must be written down where the next start can find it"
    );

    // What a crash during a hydration leaves behind.
    let stuck = root_dir.path().join("stuck.bin");
    std::fs::write(&stuck, vec![1u8; 4096]).unwrap();
    {
        let file = std::fs::File::options().read(true).write(true).open(&stuck).unwrap();
        konedrive_fs::placeholder::write_state(&file, State::Hydrating).unwrap();
    }

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
    helper.forget();
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "ready");
    assert_eq!(restarted.root().unwrap().path, resolved, "the root must come back");
    assert_eq!(
        state_of_path(&stuck),
        Some(State::OnlineOnly),
        "startup recovery never ran: a file a crash left mid-hydration stays that way, \
         holding content nothing may trust"
    );

    let seen = helper.seen();
    let registered = seen.iter().position(|s| *s == Seen::RegisterRoot);
    let cleared = seen.iter().position(|s| *s == Seen::ClearIgnore);
    assert!(registered.is_some(), "the root must be registered again: {seen:?}");
    assert!(cleared.is_some(), "recovery must have cleared the ignore mark: {seen:?}");
    assert!(
        registered < cleared,
        "recovery ran before the registration it depends on: {seen:?}"
    );

    // And forgetting the folder must un-persist it, or the next start
    // brings back a root the user got rid of.
    restarted.unregister_root().await.unwrap();
    assert_eq!(
        Config::load(restarted.persist.as_ref().unwrap().store.file()).unwrap().sync_root,
        "",
        "a forgotten root must not come back at the next start"
    );
}

/// §8 step 2: an intercepted root's files may carry the ignore mark, and
/// a file punched while it still carries one is empty **and**
/// permanently un-intercepted — zeros with nothing left to notice them.
/// So a dehydration in that root needs a helper, and refuses without
/// one, however convenient it would be to carry on.
#[tokio::test]
async fn dehydrate_is_refused_when_an_intercepted_root_has_lost_its_helper() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![7u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();

    service.set_link(None);

    let error = service.dehydrate(&file).await.unwrap_err();
    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        vec![7u8; 4096],
        "and the file must be exactly as it was"
    );
}

/// `HelperLink` fails outstanding and later calls loudly,
/// which is right — but nothing reconnected, and the published state
/// said `ready` with an empty `LastError` the whole time the folder was
/// dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_that_goes_away_is_published_and_reconnected_to() {
    a_helper_that_goes_away_is_published_and_reconnected_to_with(false).await;
}

/// The supervisor used to find out
/// the helper was gone only when `serve_hydrations` returned, and that
/// joined every fill still running first — so with one long download in
/// flight, the loss was not published (`RootState` stayed `ready`, the
/// dead link was still handed out) and nothing reconnected until the
/// download ended, while a restarted helper, re-marking the tree from
/// `roots.json`, had no daemon to ask and denied every open `EIO` after
/// 30 s. Here the download never ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_the_helper_is_published_and_reconnected_while_a_fill_still_runs() {
    a_helper_that_goes_away_is_published_and_reconnected_to_with(true).await;
}

async fn a_helper_that_goes_away_is_published_and_reconnected_to_with(fill_running: bool) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let root_dir = tempfile::tempdir().unwrap();
    let service = SyncService::new(None, None, None);
    // Long enough that the window in which the helper is gone is
    // comfortably observable, short enough to keep the test quick.
    tokio::spawn(supervise_helper(
        Arc::clone(&service),
        socket_path.clone(),
        Duration::from_millis(300),
    ));
    wait_until("the supervisor connected", || service.link().is_some()).await;
    service.register_root(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "ready");

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), [1u8; 64]).unwrap();
    let slow = Arc::new(LocalDir::new(remote.path()).delay(Duration::from_secs(3600)));
    let files = tempfile::tempdir().unwrap();
    let _held = fill_running.then(|| {
        install_source(&service, Arc::clone(&slow) as Arc<dyn ContentSource>);
        let fd = placeholder(files.path(), "slow.bin", "ITEM", 64);
        helper.send_request(1, &fd);
        fd
    });
    if fill_running {
        wait_until("the fill began", || slow.fetches() > 0).await;
    }
    helper.forget();

    helper.hang_up();

    wait_until("the helper's absence was published", || service.root_state() == "error").await;
    assert!(
        service.last_error().contains("not connected"),
        "the published error must say what happened: {}",
        service.last_error()
    );
    wait_until("the root was registered again", || {
        helper.seen().contains(&Seen::RegisterRoot)
    })
    .await;
    wait_until("the folder came back", || service.root_state() == "ready").await;
}

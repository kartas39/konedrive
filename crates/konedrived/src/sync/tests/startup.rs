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
    let helper = FakeHelper::start(socket_path.clone());
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    let resolved = std::fs::canonicalize(root_dir.path()).unwrap();

    {
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let service = testing::service(Some(link), None, Some(persist(&config_file)));
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
    let restarted = testing::service(Some(link), None, Some(persist(&config_file)));
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
        Config::load(testing::parts(&restarted).persist.store.file()).unwrap().sync_root,
        "",
        "a forgotten root must not come back at the next start"
    );
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
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let root_dir = tempfile::tempdir().unwrap();
    let service = testing::service(None, None, None);
    // Long enough that the window in which the helper is gone is
    // comfortably observable, short enough to keep the test quick.
    tokio::spawn(hub::supervise(Arc::clone(service.hub()), socket_path.clone(), Duration::from_millis(300)));
    wait_until("the supervisor connected", || service.link().is_some()).await;
    service.register_root(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "ready");

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), [1u8; 64]).unwrap();
    let slow = Arc::new(LocalDir::new(remote.path()).delay(Duration::from_secs(3600)));
    let files = tempfile::tempdir().unwrap();
    // The folder has a source, from a directory with nothing in it; the fill is served
    // by the slow one.
    let nothing = tempfile::tempdir().unwrap();
    service.populate_from_directory(nothing.path()).await.unwrap();
    install_source(&service, Arc::clone(&slow) as Arc<dyn ContentSource>);
    let _held = placeholder(files.path(), "slow.bin", "ITEM", 64);
    helper.send_request(1, &_held);
    wait_until("the fill began", || slow.fetches() > 0).await;
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

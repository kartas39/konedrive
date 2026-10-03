use super::*;

// --- Guards over verified-correct behaviour ------------

/// R3. Unregistering a root must tell the helper, or the helper keeps
/// the tree marked — and, with the uid no longer owning a root, answers
/// every placeholder open in it `EIO` (measurement).
#[tokio::test]
async fn unregister_root_tells_the_helper() {
    let (service, _sockets, helper) = service_with_helper().await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    helper.forget();

    service.unregister_root().await.unwrap();

    assert!(
        helper.seen().contains(&Seen::UnregisterRoot),
        "the helper was never told the root is gone: {:?}",
        helper.seen()
    );
}

/// R4. Unregistering a root must forget its content source. Item ids are
/// paths relative to the source, so a placeholder in the *next* root with
/// the same relative name matches the old source exactly: `Hydrate`
/// would fill it with the old folder's bytes and report success.
#[tokio::test]
async fn unregister_root_forgets_the_content_source() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let first_source = tempfile::tempdir().unwrap();
    std::fs::write(first_source.path().join("f.bin"), vec![0xAAu8; 4096]).unwrap();
    let first = tempfile::tempdir().unwrap();
    service.register_root(first.path()).await.unwrap();
    service.populate_from_directory(first_source.path()).await.unwrap();
    service.unregister_root().await.unwrap();

    let second = tempfile::tempdir().unwrap();
    service.register_root(second.path()).await.unwrap();
    drop(placeholder(second.path(), "f.bin", "f.bin", 4096));
    let file = second.path().join("f.bin");

    let outcome = service.hydrate_now(&file).await;

    assert!(
        matches!(outcome, Err(SyncError::NoSource)),
        "the second root was hydrated from the first root's source: {outcome:?}"
    );
    assert_ne!(
        std::fs::read(&file).unwrap(),
        vec![0xAAu8; 4096],
        "and it holds the first folder's bytes"
    );
    assert_eq!(service.item_state(&file).await, "online-only");
}

/// Design §8.3, review I2: an empty folder that carries another account's
/// drive holds nothing to adopt — the usual Remove, then Add, on the same
/// folder — so it is taken, and the stale drive comes off; a folder with
/// anything in it is still refused.
#[tokio::test]
async fn an_empty_folder_that_carries_another_drive_is_taken_and_a_full_one_is_not() {
    let config_dir = tempfile::tempdir().unwrap();
    let persist = persist(&config_dir.path().join("config.toml"));
    persist.store.record_drive(&persist.account, "DB").unwrap();
    let service = SyncService::new(None, None, Some(persist));
    service.set_helper_socket(config_dir.path().join("no-helper.sock"));
    let (full, empty) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for dir in [full.path(), empty.path()] {
        xattr::set(dir, "user.konedrive.drive", b"DA").unwrap();
    }
    std::fs::write(full.path().join("theirs.txt"), b"x").unwrap();

    let refused = service.register_root_without_interception(full.path()).await;
    assert!(matches!(refused, Err(SyncError::ForeignFolder)), "{refused:?}");
    assert_eq!(xattr::get(full.path(), "user.konedrive.drive").unwrap().as_deref(), Some(&b"DA"[..]));

    service.register_root_without_interception(empty.path()).await.unwrap();
    assert_eq!(xattr::get(empty.path(), "user.konedrive.drive").unwrap(), None, "the stale drive is taken off");
}

/// Review M2: an account being removed is retired under its lifecycle
/// lock, and registers nothing from then on — not even a call that was
/// waiting for that lock.
#[tokio::test]
async fn a_retired_account_registers_nothing() {
    let service = SyncService::new(None, None, None);
    service.retire().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let refused = service.register_root_without_interception(dir.path()).await;
    assert!(matches!(&refused, Err(SyncError::Io(why)) if why.contains("being removed")), "{refused:?}");
    assert_eq!(xattr::get(dir.path(), "user.konedrive.root").unwrap(), None, "the folder is not touched");
}

/// SY5: a removal taken back leaves the account as it was: one held back is held back
/// again, for its own reason, and one that was not registers a folder.
#[tokio::test]
async fn a_removal_taken_back_gives_the_account_its_standing_back() {
    let dir = tempfile::tempdir().unwrap();
    let held = SyncService::new(None, None, None);
    held.hold_back("its label repeats");
    held.retire().await.unwrap();
    held.unretire().await;
    let refused = held.register_root_without_interception(dir.path()).await;
    assert!(matches!(&refused, Err(SyncError::Io(why)) if why.contains("its label repeats")), "{refused:?}");
    assert!(held.last_error().contains("its label repeats"), "{}", held.last_error());

    let free = SyncService::new(None, None, None);
    free.retire().await.unwrap();
    free.unretire().await;
    free.register_root_without_interception(dir.path()).await.unwrap();
}

/// N6. The persisted "intercepted" flag must survive a restart. A root
/// registered without interception on a machine with no helper would
/// otherwise be restored as an
/// intercepted root, which waits for a helper that never comes: the
/// folder simply would not come back.
#[tokio::test]
async fn a_root_persisted_without_interception_comes_back_without_a_helper() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    {
        let service = SyncService::new(None, None, Some(persist(&config_file)));
        service.register_root_without_interception(root_dir.path()).await.unwrap();
    }
    assert!(
        !Config::load(&config_file).unwrap().sync_root_intercepted,
        "the mode must be written down with the root"
    );

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.resume().await;

    assert_eq!(
        restarted.root().map(|r| r.path),
        Some(std::fs::canonicalize(root_dir.path()).unwrap()),
        "a root registered without interception did not come back after a restart"
    );
    assert_eq!(restarted.root_state(), "no-interception");
}

/// Q3, narrowed by. A root registered without interception
/// *while a helper was connected* stays that way when the helper connects
/// again — after a reconnect, and after a restart: the user asked for
/// this mode by name with interception on offer, and quietly changing
/// what protects their files is not this daemon's call. (One registered
/// that way because no helper was connected does switch; see below.)
#[tokio::test]
async fn a_helper_reconnecting_does_not_upgrade_a_root_registered_without_interception_on_purpose() {
    let (service, helper, config_file, sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let socket_path = sockets.path().join("helper.sock");
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    assert_eq!(
        Config::load(&config_file).unwrap().sync_root_upgrade_when_helper,
        Some(false),
        "a choice made with a helper connected must be written down as one"
    );

    // What `supervise_helper` does when the connection drops and the
    // helper answers again.
    service.set_link(None);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    helper.forget();
    service.set_link(Some(link));
    service.resume().await;
    drop(service);

    // And a restart, with the helper there from the start.
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
    restarted.restore().await;
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "no-interception");
    assert!(
        restarted.last_error().contains("read as zeros"),
        "the warning must still be there: {}",
        restarted.last_error()
    );
    assert!(
        !helper.seen().contains(&Seen::RegisterRoot),
        "the root was registered with the helper behind the user's back: {:?}",
        helper.seen()
    );
}

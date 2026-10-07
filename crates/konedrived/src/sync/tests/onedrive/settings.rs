use super::*;

/// The thumbnail setting is absent from `config.toml` until set, reads its
/// default then, is written when set and taken at once — thumbnails off stop nothing
/// else — and is read back by the next start; a local folder has no settings to set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_thumbnail_setting_is_kept_in_config_toml_and_taken_at_once() {
    let local = world().await;
    let other = service(&local, true);
    other.register_root_without_interception(local.folder.path()).await.unwrap();
    assert!(matches!(other.change_run_settings(|s| s.thumbnails = false).await, Err(SyncError::Unsupported(_))));

    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    // Read afresh each time: what the file holds now.
    let written = || {
        let persist = persist(&w.config.path().join("config.toml"));
        persist.store.account(&persist.account).unwrap()
    };
    assert_eq!(service.run_settings(), running::Settings::default(), "absent means the default");
    assert_eq!(written().thumbnails, None);

    service.change_run_settings(|s| s.thumbnails = false).await.unwrap();
    assert!(!service.run_settings().thumbnails);
    assert_eq!(written().thumbnails, Some(false));
    assert!(!service.state().get().stopped(), "thumbnails off stop nothing else");

    service.stop_sync().await;
    service.hub().set_link(None);
    drop(service);
    let restarted = connected(&w, true).await;
    assert_eq!(restarted.run_settings(), running::Settings { thumbnails: false, paused_until: None });
}

/// With the notification socket up, a change in OneDrive gives one delta
/// within the debounce, and `LiveChanges` says `connected`; the user's pause closes the
/// socket (`off`) and Resume opens it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_in_onedrive_arrives_through_the_socket_and_a_pause_closes_it() {
    use crate::remote::live::Timing;
    use crate::status::snapshot::LiveChanges;
    use crate::fake_onedrive::{FakeGraph, ROOT};
    let w = world().await;
    // The fake OneDrive for its socket only; the world's own server serves the rest.
    let graph = FakeGraph::start().await;
    let endpoint = graph.client().socket_endpoint().await.unwrap();
    Mock::given(method("GET")).and(path("/me/drive/root/subscriptions/socketIo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"notificationUrl": endpoint.notification_url.as_str()})))
        .mount(&w.server).await;
    let live = Timing { debounce: Duration::from_millis(300), settle: Duration::from_millis(100), ..Timing::default() };
    let schedule = Schedule { live: Some(live), ..Schedule::polled(Duration::from_secs(3600), vec![Duration::from_millis(50)]) };
    let wiring = wiring(&w, account(true), Arc::new(StaticToken::new("T"))).schedule(schedule).link(Some(link(&w).await));
    let service = made(&w, wiring);
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    wait_until("connected", || service.state().get().cycle.live_changes == LiveChanges::Connected).await;
    let before = deltas(&w).await;

    graph.with(|c| c.add_file("X", ROOT, "x.txt", b"x"));
    wait_for_deltas(&w, before).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(deltas(&w).await, before + 1, "one delta for the event");

    service.pause_syncing(0).await.unwrap();
    wait_until("closed by the pause", || service.state().get().cycle.live_changes == LiveChanges::Off && graph.sockets.open() == 0).await;
    service.resume_syncing().await.unwrap();
    wait_until("open again", || service.state().get().cycle.live_changes == LiveChanges::Connected).await;
    service.stop_sync().await;
    assert_eq!(service.state().get().cycle.live_changes, LiveChanges::Off, "no sync, no socket");
}

/// On a metered connection the account holds back — no upload, no poll, no
/// pinned download or thumbnail (the pool gives no slot but for opens) — while an open
/// still gets its slot, `HeldBack` says why and `Paused` stays false; when the
/// connection is no longer metered, what waited goes at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_metered_connection_holds_the_account_back_until_it_ends() {
    use crate::account::PendingUploads;
    use crate::config::Mode;
    use konedrive_graph::pool::{Class, Size};
    use wiremock::matchers::path_regex;
    let w = world().await;
    Mock::given(method("POST"))
        .and(path_regex("/me/drive/items/D:/new.txt:/createUploadSession$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&w.server)
        .await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let_write(&service);
    service.follow_mode(Mode::ReadWrite).await;
    let before = deltas(&w).await;
    wait_for_deltas(&w, before).await;

    service.registry().set_conditions(running::Conditions { metered: true, ..running::Conditions::default() });
    assert_eq!(service.state().get().pause.held_back, "metered");
    assert_eq!(service.state().get().pause.paused_until, None, "a hold is not the user's pause");
    assert!(service.pool().try_acquire_sized(Class::Download, Size::Small).is_none(), "no pinned download");
    assert!(service.pool().try_acquire_sized(Class::Open, Size::Small).is_some(), "an open still downloads");
    let made = std::process::Command::new("sh").args(["-c", "echo new > docs/new.txt"]).current_dir(w.folder.path()).status().unwrap();
    assert!(made.success());
    let seen = deltas(&w).await;
    service.refresh().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(deltas(&w).await, seen, "OneDrive is not asked");
    assert_eq!(service.pending_uploads().await, 1, "the change waits");
    let asked = || async { w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST").count() };
    assert_eq!(asked().await, 0, "nothing is uploaded");
    assert!(service.outbox(0).await.unwrap().iter().all(|row| row.state == "paused"), "the rows read paused");

    service.registry().set_conditions(running::Conditions::default());
    assert_eq!(service.state().get().pause.held_back, "");
    wait_for_deltas(&w, seen).await;
    for _ in 0..250 {
        if asked().await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(asked().await > 0, "the upload is tried at once");
    service.stop_sync().await;
}

/// The user's pause and a hold are both on — `Resume` alone does not start
/// the account while the hold is on, nor does the hold's end alone while the pause is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_account_runs_only_when_neither_a_pause_nor_a_hold_is_on() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let metered = running::Conditions { metered: true, ..running::Conditions::default() };
    let quiet = |what: &'static str| {
        let (w, service) = (&w, &service);
        async move {
            let seen = deltas(w).await;
            service.refresh().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(deltas(w).await, seen, "{what}");
        }
    };

    service.pause_syncing(0).await.unwrap();
    service.registry().set_conditions(metered);
    service.resume_syncing().await.unwrap();
    quiet("resumed, still held").await;
    service.pause_syncing(0).await.unwrap();
    service.registry().set_conditions(running::Conditions::default());
    quiet("the hold ended, still paused").await;
    let seen = deltas(&w).await;
    service.resume_syncing().await.unwrap();
    wait_for_deltas(&w, seen).await;
    service.stop_sync().await;
}

/// `SyncAnyway` lifts the hold at once, until a source or the global hold
/// settings change; then the hold is worked out again. After a restart with the
/// condition still on, the account is held again and `Paused` stays false.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_anyway_lifts_the_hold_until_something_changes_and_a_restart_holds_again() {
    use crate::config::OnBattery;
    use running::HoldSettings;
    let hold = |on_battery| HoldSettings { on_battery, ..HoldSettings::default() };
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    let on_battery = running::Conditions { on_battery: true, ..running::Conditions::default() };
    service.registry().set_hold_settings(hold(OnBattery::Pause));
    service.registry().set_conditions(on_battery);
    assert_eq!(service.state().get().pause.held_back, "on-battery");

    let seen = deltas(&w).await;
    service.sync_anyway().unwrap();
    assert_eq!(service.state().get().pause.held_back, "");
    wait_for_deltas(&w, seen).await;
    service.registry().set_conditions(running::Conditions { power_saver: true, ..on_battery });
    assert_eq!(service.state().get().pause.held_back, "on-battery", "the profile changed: held again");

    service.sync_anyway().unwrap();
    service.registry().set_hold_settings(hold(OnBattery::PowerSaver));
    assert_eq!(service.state().get().pause.held_back, "power-saver", "the setting changed: worked out again");
    service.registry().set_hold_settings(hold(OnBattery::Sync));
    assert_eq!(service.state().get().pause.held_back, "", "sync on battery");

    service.stop_sync().await;
    service.hub().set_link(None);
    drop(service);
    let registry = registry::Registry::with_link(Some(link(&w).await));
    registry.set_conditions(on_battery);
    registry.set_hold_settings(hold(OnBattery::Pause));
    let restarted = service_on(&w, &registry);
    restarted.restore().await;
    let seen = deltas(&w).await;
    restarted.resume().await;
    wait_until("held again after a restart", || restarted.state().get().pause.held_back == "on-battery").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(deltas(&w).await, seen, "and asks OneDrive for nothing");
    assert_eq!(restarted.state().get().pause.paused_until, None, "and not paused");
    restarted.stop_sync().await;
}

/// the outbox on the bus, `SetIgnorePatterns`: the list is written to `config.toml`, read
/// back by the next start, and a pattern that cannot match a name is
/// refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ignore_list_is_kept_in_config_toml() {
    let w = world().await;
    let service = connected(&w, true).await;
    service.register_root(w.folder.path()).await.unwrap();
    listed(&service).await;
    assert!(service.ignore_patterns().contains(&"*.swp".to_owned()), "the defaults");
    service.set_ignore_patterns(vec!["*.bak".into(), "*.bak".into(), "build-*".into()]).await.unwrap();
    assert_eq!(service.ignore_patterns(), vec!["*.bak".to_owned(), "build-*".to_owned()]);
    let persist = persist(&w.config.path().join("config.toml"));
    assert_eq!(persist.store.account(&persist.account).unwrap().ignore, Some(vec!["*.bak".to_owned(), "build-*".to_owned()]));
    assert!(matches!(service.set_ignore_patterns(vec!["a/b".into()]).await, Err(SyncError::InvalidArgs(_))));
    service.stop_sync().await;
    service.hub().set_link(None);
    let restarted = connected(&w, true).await;
    assert_eq!(restarted.ignore_patterns(), vec!["*.bak".to_owned(), "build-*".to_owned()]);
    assert!(!restarted.machine_name().is_empty());
}

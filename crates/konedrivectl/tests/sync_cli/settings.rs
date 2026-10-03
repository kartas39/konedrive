use super::*;

// --- the outbox on the bus: the outbox on the command line -----

/// `sync pause`, `sync resume`, `sync ignore`, `sync outbox`, `sync not-uploaded`
/// and `sync deletes`, end to end through the binary on a OneDrive folder; and a
/// duration that is none is a usage error (exit status 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_pauses_resumes_and_keeps_the_ignore_list() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    let addr = f._bus.address();

    let out = run(addr, &["sync", "pause", "--for", "2h"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Paused until "), "{}", out_text(&out));
    // The proxy's cache follows the coalesced PropertiesChanged.
    let paused_is = |wanted: bool| {
        let proxy = &f.proxy;
        async move {
            for _ in 0..100 {
                if proxy.folder.paused().await.unwrap() == wanted {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            false
        }
    };
    assert!(paused_is(true).await);
    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(status.lines().any(|l| l.starts_with("Paused until:")), "{status}");
    let out = run(addr, &["sync", "pause", "--for", "soon"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = run(addr, &["sync", "resume"]);
    assert_eq!(out_text(&out).trim(), "Resumed.");
    // Right after the pause, within one coalescing window: still signalled.
    assert!(paused_is(false).await);

    let out = run(addr, &["sync", "ignore", "add", "*.bak"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&run(addr, &["sync", "ignore"])).lines().any(|l| l == "*.bak"));
    assert!(run(addr, &["sync", "ignore", "remove", "*.bak"]).status.success());
    assert!(!f.proxy.folder.ignore_patterns().await.unwrap().contains(&"*.bak".to_owned()));
    assert_eq!(run(addr, &["sync", "ignore", "remove", "*.bak"]).status.code(), Some(2));

    assert_eq!(out_text(&run(addr, &["sync", "outbox"])).trim(), "Uploading:    0 now, 0 B/s\nNothing is waiting to upload.");
    assert_eq!(out_text(&run(addr, &["sync", "not-uploaded"])).trim(), "Everything here is uploaded or waits to be.");
    assert_eq!(out_text(&run(addr, &["sync", "deletes", "confirm"])).trim(), "No delete is waiting for confirmation.");
}

/// Issue #80: `sync thumbnails` prints the setting without an argument and changes it with
/// one; a choice that is none is a usage error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_shows_and_changes_the_thumbnail_setting() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    let addr = f._bus.address();

    assert!(out_text(&run(addr, &["sync", "thumbnails"])).starts_with("Thumbnails: on"));
    let out = run(addr, &["sync", "thumbnails", "off"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Thumbnails: off — Dolphin downloads"), "{}", out_text(&out));
    assert!(!f.proxy.folder.thumbnails().await.unwrap());
    assert!(out_text(&run(addr, &["sync", "thumbnails"])).starts_with("Thumbnails: off"));

    assert_eq!(run(addr, &["sync", "thumbnails", "maybe"]).status.code(), Some(2));
    assert_eq!(run(addr, &["sync", "on-battery"]).status.code(), Some(2), "moved to `settings`");
}

/// Issue #95: `settings on-metered` and `settings on-battery` print the setting every account
/// shares without an argument and change it with one — written to `config.toml` and taken by
/// the account at once. A choice that is none, and `--account`, are usage errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_shows_and_changes_the_settings_every_account_shares() {
    use konedrived::config::OnBattery;
    let f = harness().await;
    let addr = f._bus.address();
    let config = || std::fs::read_to_string(f._config_dir.path().join("config.toml")).unwrap();

    assert_eq!(out_text(&run(addr, &["settings", "on-metered"])).trim(), "On a metered connection: pause.");
    let out = run(addr, &["settings", "on-metered", "sync"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "On a metered connection: sync as usual.");
    assert_eq!(out_text(&run(addr, &["settings", "on-metered"])).trim(), "On a metered connection: sync as usual.");
    assert!(!f.service.hold_settings().pause_on_metered);
    assert!(config().contains("pause_on_metered = false"), "{}", config());

    assert_eq!(out_text(&run(addr, &["settings", "on-battery"])).trim(), "On battery: pause in power-saver mode.");
    assert_eq!(out_text(&run(addr, &["settings", "on-battery", "pause"])).trim(), "On battery: pause.");
    assert_eq!(out_text(&run(addr, &["settings", "on-battery"])).trim(), "On battery: pause.");
    assert_eq!(f.service.hold_settings().on_battery, OnBattery::Pause);
    assert!(config().contains("on_battery = \"pause\""), "{}", config());

    assert_eq!(run(addr, &["settings", "on-battery", "sometimes"]).status.code(), Some(2));
    let out = run(addr, &["--account", konedrivectl::FIRST_LABEL, "settings", "on-battery"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(err_text(&out).contains("one for every account"), "{}", err_text(&out));
}

/// Issue #57: while the account holds back by itself, `sync status` says why; `sync anyway`
/// lifts the hold, and the line goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_says_why_the_account_paused_by_itself_and_anyway_lifts_it() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    let addr = f._bus.address();
    let line = |status: &str| status.lines().find(|l| l.starts_with("Paused by itself:")).map(str::to_owned);

    assert_eq!(out_text(&run(addr, &["sync", "anyway"])).trim(), "Not paused by itself: nothing to lift.");
    f.service.set_conditions(konedrived::conditions::running::Conditions { metered: true, ..Default::default() });
    let status = out_text(&run(addr, &["sync", "status"]));
    let said = line(&status).unwrap_or_else(|| panic!("{status}"));
    assert!(said.contains("metered connection (`konedrivectl sync anyway` syncs now)"), "{said}");
    assert!(!status.lines().any(|l| l.starts_with("Paused until:")), "not the user's pause: {status}");

    let out = run(addr, &["sync", "anyway"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Syncing anyway (metered connection)"), "{}", out_text(&out));
    assert_eq!(line(&out_text(&run(addr, &["sync", "status"]))), None);
}

/// Issue #54: `sync status` says how changes from OneDrive arrive — every minute while the
/// socket cannot connect (this drive serves no notification endpoint), nothing while the
/// account is held back — and `sync anyway --all` lifts the hold of every account that holds
/// back by itself, but not the user's pause; with `--account` it is a usage error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_says_how_changes_arrive_and_anyway_all_lifts_every_hold() {
    use konedrived::remote::live::LiveChanges;
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    let addr = f._bus.address();
    let line = |status: &str| status.lines().find(|l| l.starts_with("Changes from OneDrive:")).map(str::to_owned);
    let live = || f.service.state().get().live_changes;

    wait_for(|| live() == LiveChanges::Connecting).await;
    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(line(&status).is_some_and(|l| l.ends_with("every minute (connecting)")), "{status}");

    let out = run(addr, &["--account", "Personal", "sync", "anyway", "--all"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(out_text(&run(addr, &["sync", "anyway", "--all"])).trim(), "No account is paused by itself: nothing to lift.");

    f.service.set_conditions(konedrived::conditions::running::Conditions { metered: true, ..Default::default() });
    wait_for(|| live() == LiveChanges::Off).await;
    let status = out_text(&run(addr, &["sync", "status"]));
    assert_eq!(line(&status), None, "{status}");

    // The user's pause is not lifted by it.
    assert!(run(addr, &["sync", "pause"]).status.success());
    assert_eq!(out_text(&run(addr, &["sync", "anyway", "--all"])).trim(), "No account is paused by itself: nothing to lift.");
    assert!(run(addr, &["sync", "resume"]).status.success());

    let out = run(addr, &["sync", "anyway", "--all"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Syncing anyway (metered connection)"), "{}", out_text(&out));
    assert_eq!(f.service.state().get().held_back, "");
    wait_for(|| live() == LiveChanges::Connecting).await;
}

/// A folder not connected to OneDrive has no sync settings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_sync_settings_of_a_local_folder_say_it_has_none() {
    let f = harness().await;
    let root = f.dir.path().join("local");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    let out = run(f._bus.address(), &["sync", "thumbnails", "off"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(err_text(&out).contains("not connected to OneDrive, so it has no sync settings"), "{}", err_text(&out));
}

/// A folder not connected to OneDrive uploads nothing: the outbox commands say
/// so, by the refusal's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_outbox_of_a_local_folder_says_nothing_is_uploaded_from_it() {
    let f = harness().await;
    let root = f.dir.path().join("local");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    let out = run(f._bus.address(), &["sync", "outbox"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(err_text(&out).contains("not connected to OneDrive, so nothing is uploaded from it"), "{}", err_text(&out));
}

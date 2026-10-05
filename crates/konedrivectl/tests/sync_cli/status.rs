use super::*;

// --- `sync status` and the no-interception mode ---------------------------
//
// On a machine without the helper, `no-interception` is the state a folder
// registered there stays in — and its cost (a file
// that is not downloaded reads as zeros) must be on screen every time the
// user asks, in the CLI's own words, not left to a daemon property that
// happens to restate it or to the user's memory.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_says_plainly_that_nothing_intercepts_opens() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register_without_interception(root.to_str().unwrap()).await.unwrap();

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    let opens = text
        .lines()
        .find(|line| line.starts_with("Opens:"))
        .unwrap_or_else(|| panic!("no Opens: line: {text}"));
    assert!(opens.to_lowercase().contains("not intercepted"), "{text}");
    assert!(opens.contains("zeros"), "{text}");
    assert!(opens.contains("konedrivectl sync hydrate"), "say how to get the real bytes: {text}");
}

/// The daemon has started and not brought the folder up yet: it has its path, its state is
/// `none`, and nothing has measured it, so no size and no count of pins is printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_of_a_folder_not_brought_up_yet_says_no_size() {
    let f = harness_with_helper(false).await;
    f.service.state().update(|s| s.folder.root_path = "/home/u/OneDrive".into());

    let out = run(f._bus.address(), &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    assert!(text.lines().any(|l| l.starts_with("Folder:") && l.ends_with("/home/u/OneDrive")), "{text}");
    assert!(text.lines().any(|l| l.starts_with("State:") && l.ends_with("none")), "{text}");
    assert!(!text.contains("On this computer:") && !text.contains("Always on this device:"), "{text}");
}

/// The contrast: an intercepted folder must not carry the zeros warning, or
/// the warning becomes noise a user learns to skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_of_an_intercepted_folder_does_not_warn_of_zeros() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    let opens = text
        .lines()
        .find(|line| line.starts_with("Opens:"))
        .unwrap_or_else(|| panic!("no Opens: line: {text}"));
    assert!(opens.contains("intercepted"), "{text}");
    assert!(!opens.to_lowercase().contains("not intercepted"), "{text}");
    assert!(!text.contains("zeros"), "{text}");
}

// --- `sync skipped`, `sync refresh`, and OneDrive-folder `sync status` ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_lists_what_is_not_in_the_folder_and_why() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;

    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    assert!(text.contains(&root.join("Personal Vault").display().to_string()), "{text}");
    assert!(text.contains("locked separately"), "says why: {text}");
}

/// The daemon's own wiring (`daemon/manager.rs`, `sync::mode::follow`) makes a
/// registered OneDrive folder follow `Account.Mode`: read-write takes the read-only lock off,
/// read-only puts it back. (How `SetMode` turns `Mode` is `konedrived`'s `tests/mode.rs`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_folder_follows_the_accounts_mode() {
    use std::os::unix::fs::PermissionsExt;
    use konedrived::config::Mode;
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    let file = root.join("docs/f.txt");
    let modes = || {
        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        (mode(&file), mode(&root.join("docs")), mode(&root))
    };
    wait_for(|| file.is_file() && modes() == (0o444, 0o555, 0o555)).await;

    f.account.state().update(|s| s.mode = Mode::ReadWrite);
    wait_for(|| modes() == (0o644, 0o755, 0o755)).await;
    assert_eq!(f.service.mode(), Mode::ReadWrite);
    // `Folder.Writable`, and `sync status`'s `Mode:` line with it.
    wait_for(|| f.service.writable()).await;
    assert!(f.proxy.folder.writable().await.unwrap());
    let text = out_text(&run(f._bus.address(), &["sync", "status"]));
    assert!(text.lines().any(|l| l.starts_with("Mode:") && l.contains("changes made here are uploaded")), "{text}");
    f.account.state().update(|s| s.mode = Mode::ReadOnly);
    wait_for(|| modes() == (0o444, 0o555, 0o555)).await;
    assert_eq!(f.service.mode(), Mode::ReadOnly);
    assert!(!f.proxy.folder.writable().await.unwrap());
}

/// `sync status` says how the local scan goes: a read-only folder has none.
/// (How a read-write folder's scan reads is `text::status::local_scan_text`'s test.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_says_a_read_only_folder_has_no_local_scan() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    let text = out_text(&run(f._bus.address(), &["sync", "status"]));
    let line = text.lines().find(|l| l.starts_with("Local scan:")).unwrap_or_else(|| panic!("{text}"));
    assert_eq!(line, "Local scan:             none — read-only");
}

/// `sync skipped` with no folder registered at all: there is nothing to be
/// signed in about, and nothing OneDrive-related to say either — a plain
/// statement of the actual reason, not the empty "Nothing is skipped."
/// that would otherwise print (§4's "an unrecognised state shows no
/// emblem" kind of silent-looking success).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_with_no_folder_registered_says_so() {
    let f = harness().await;
    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out), "No folder is registered.\n");
}

/// `sync skipped` of a folder filled with `PopulateFrom` (a local folder,
/// `RootSource = local`): there is no OneDrive listing behind it, so
/// nothing is "skipped" in the sense this command means, and that has to be
/// said plainly rather than as an empty list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_of_a_local_folder_says_it_is_not_connected_to_onedrive() {
    let f = harness().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();

    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out), "This folder is not connected to OneDrive.\n");
}

/// `sync skipped` asked for while the initial listing is still running: the
/// list `Skipped()` can return at that moment is not wrong, only
/// incomplete — pages not listed yet have not reported what they skip — so
/// the output has to say that rather than let the (possibly empty) list
/// read as final. The delta response is delayed well past the time the
/// subprocess needs to start and call `sync skipped`, so `Folder.State` is
/// still `listing` for the whole call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_of_a_onedrive_folder_still_listing_says_the_list_may_be_partial() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let graph = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "D1"})))
        .mount(&graph)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(2))
                .set_body_json(serde_json::json!({
                    "value": [{"id": "R", "root": {}, "folder": {}}],
                    "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", graph.uri())
                })),
        )
        .mount(&graph)
        .await;
    let f = harness_showing(&graph).await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    for _ in 0..250 {
        if f.proxy.folder.state().await.unwrap() == "listing" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(f.proxy.folder.state().await.unwrap(), "listing", "the delay must still be in effect");

    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("may be partial"), "{}", out_text(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_of_a_onedrive_folder_counts_its_items_and_says_it_is_read_only() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await; // counters are coalesced

    let text = out_text(&run(f._bus.address(), &["sync", "status"]));
    assert!(
        text.lines().any(|l| l.starts_with("Items:") && l.contains("3 in OneDrive") && l.contains("2 in the folder")),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l == "Skipped:                1 (see `konedrivectl sync skipped`)"),
        "{text}"
    );
    assert!(text.lines().any(|l| l.starts_with("Mode:") && l.contains("read-only")), "{text}");
    assert!(!text.lines().any(|l| l.starts_with("Waiting to upload:")), "nothing uploads from a read-only folder: {text}");
}

/// `sync status` and `status` say the state of the account as a whole as the daemon decided
/// it (`Folder.Overall`), with the sentence of `Folder.Trouble`: the line follows the daemon
/// through a folder at rest, trouble that leaves it running, and a pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_says_the_overall_state_the_daemon_decided() {
    use konedrived::status::snapshot::{SyncTrouble, TroubleKind};
    let f = harness().await;
    let addr = f._bus.address();
    let overall = |command: &[&str]| {
        let text = out_text(&run(addr, command));
        let line = text.lines().find(|line| line.starts_with("Overall:")).unwrap_or_else(|| panic!("no Overall: line: {text}"));
        line.trim_start_matches("Overall:").trim().to_owned()
    };
    assert_eq!(overall(&["sync", "status"]), "offline — no OneDrive folder yet");

    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    assert_eq!(overall(&["sync", "status"]), "ok — up to date");
    assert_eq!(overall(&["status"]), "ok — up to date");

    // Out of reach by its kind, in a sentence this build has never said.
    let reworded = "OneDrive does not answer; the next try is in a minute";
    f.service.state().update(|s| s.cycle.sync_trouble = Some(SyncTrouble { text: reworded.into(), blocking: false, kind: TroubleKind::Unreachable }));
    assert_eq!(overall(&["sync", "status"]), format!("offline — {reworded}"));
    assert_eq!(overall(&["status"]), format!("offline — {reworded}"));

    f.service.state().update(|s| s.pause.paused_until = Some(0));
    assert_eq!(overall(&["sync", "status"]), "paused — you paused syncing");
}

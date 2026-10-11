use super::*;

// --- Activity, transfers, conflicts, free-up, status lines -----

/// The binary, with `TZ` set so the times it prints are UTC.
fn run_utc(bus_addr: &str, args: &[&str]) -> std::process::Output {
    common::run_env(bus_addr, args, &[("TZ", "UTC")])
}

/// `sync transfers`: each download under way with how far it has got, or
/// that there is none; the summary line says what is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_transfers_lists_the_downloads_under_way() {
    let f = harness().await;
    let addr = f._bus.address();
    let out = run(addr, &["sync", "transfers"]);
    assert!(out.status.success(), "{out:?}");
    let idle = "Downloading:  0 now, 0 B/s\nUploading:    0 now, 0 B/s\nPool: 0 of 16 · large files: 0 (0 of 4 streams)\nNothing is downloading or uploading.";
    assert_eq!(out_text(&out).trim(), idle);

    // A directory of twelve files was pinned: all of them are left, and one downloads now.
    f.service.state().update(|s| s.transfers.pinned_download = konedrive_tree::PinnedDownloadTotal { count: 12, bytes: 40 << 20 });
    let entry = f.service.report().transfers.start("/home/u/OneDrive/big.bin".into(), 4 << 20);
    entry.progress(1 << 20, 4 << 20);
    // The totals are counted at most once a second.
    wait_for(|| f.service.state().get().transfers.queue.down.left_bytes == 39 << 20).await;
    let out = run(addr, &["sync", "transfers"]);
    let text = out_text(&out);
    assert!(out.status.success(), "{out:?}");
    // "N now" counts files, not slots: the one downloading, which holds none here. What is left is every pinned file not here yet.
    assert!(text.starts_with("Downloading:  1 now, 12 files left (39.0 MiB), 0 B done, 0 B/s\n"), "{text}");
    let list = text.lines().nth(3).unwrap_or_default();
    assert!(list.starts_with("down ") && list.contains("/home/u/OneDrive/big.bin") && list.contains("25%") && list.contains("4.0 MiB"), "{text}");
    drop(entry);
    f.service.state().update(|s| s.transfers.pinned_download = konedrive_tree::PinnedDownloadTotal::default());
    wait_for(|| f.service.state().get().transfers.queue.down.left_count == 0).await;
    assert_eq!(out_text(&run(addr, &["sync", "transfers"])).trim(), idle);
}

/// `sync conflicts` lists each local version moved out of the way — where
/// it was, where it is, when — and `sync dismiss` takes one off the list,
/// leaving the file; a path that is not a conflict is refused by name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_conflicts_are_listed_and_dismissed() {
    let f = harness().await;
    let addr = f._bus.address();
    assert_eq!(out_text(&run_utc(addr, &["sync", "conflicts"])).trim(), "No conflicts.");

    let kept_copy = f.dir.path().join("docs/f-here.txt");
    std::fs::create_dir_all(kept_copy.parent().unwrap()).unwrap();
    std::fs::write(&kept_copy, b"mine").unwrap();
    let activity = &f.service.report().activity;
    // The activity log is blocking code: a plain thread of its own, off the runtime.
    std::thread::scope(|scope| scope.spawn(|| activity.attach(konedrive_tree::Store::new(konedrive_tree::TreeStore::in_memory().unwrap()), f.dir.path())).join().unwrap());
    let conflict = konedrive_tree::ConflictRow {
        at: 1_700_000_000,
        original: "/home/u/OneDrive/docs/f.txt".into(),
        kept_copy: kept_copy.display().to_string(),
    };
    std::thread::scope(|scope| scope.spawn(|| activity.add_conflicts(vec![conflict])).join().unwrap());

    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(
        status.lines().any(|l| l == "Conflicts:              1 (see `konedrivectl sync conflicts`)"),
        "{status}"
    );
    let text = out_text(&run_utc(addr, &["sync", "conflicts"]));
    assert!(text.contains("/home/u/OneDrive/docs/f.txt"), "{text}");
    assert!(text.contains(kept_copy.to_str().unwrap()), "{text}");
    assert!(text.contains("2023-11-14 22:13:20"), "{text}");

    let out = run(addr, &["sync", "dismiss", kept_copy.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Dismissed"), "{out:?}");
    assert!(kept_copy.exists(), "the file itself is left where it is");
    assert_eq!(out_text(&run(addr, &["sync", "conflicts"])).trim(), "No conflicts.");

    let text = refused(addr, &["sync", "dismiss", "/nowhere/f.txt"]);
    assert!(text.contains("/nowhere/f.txt"), "{text}");
}

/// `sync open` prints the address of the item's page — of a file, and of the
/// account's folder itself, which is the drive's root — and starts no opener: a stand-in
/// `xdg-open` that records what it is given is alone in `PATH`, `KONEDRIVE_NO_BROWSER` is
/// empty, and the piped stdout alone, or `--print`, keeps it from being called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "mounts a filesystem: run in the VM"]
async fn binary_open_prints_the_address_and_starts_no_opener() {
    use std::os::unix::fs::PermissionsExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let (f, graph) = harness_onedrive().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    let file = root.join("docs/f.txt");
    wait_for(|| file.is_file()).await;
    for (route, url) in [("/me/drive/items/F", "https://onedrive.example/f"), ("/me/drive/root", "https://onedrive.example/root")] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "X", "webUrl": url})))
            .mount(&graph)
            .await;
    }

    let opener = tempfile::tempdir().unwrap();
    let opened = opener.path().join("opened");
    let script = opener.path().join("xdg-open");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n", opened.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("PATH", opener.path().to_str().unwrap()), (konedrivectl::NO_BROWSER_VARIABLE, "")];

    let out = common::run_env(addr, &["sync", "open", file.to_str().unwrap()], &env);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out), "https://onedrive.example/f\n");
    let out = common::run_env(addr, &["sync", "open", "--print", file.to_str().unwrap()], &env);
    assert_eq!(out_text(&out), "https://onedrive.example/f\n", "{out:?}");
    let out = common::run_env(addr, &["sync", "open", root.to_str().unwrap()], &env);
    assert_eq!(out_text(&out), "https://onedrive.example/root\n", "{out:?}");
    // Long enough for an opener, had one been started, to have written its line.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!opened.exists(), "an opener was started: {:?}", std::fs::read_to_string(&opened));

    let outside = f.dir.path().join("elsewhere.txt");
    std::fs::write(&outside, b"x").unwrap();
    let told = refused(addr, &["sync", "open", outside.to_str().unwrap()]);
    assert!(told.contains("is not inside the sync folder") && told.contains("has a page in OneDrive"), "{told}");
}

/// `sync status` says when the folder was last checked with OneDrive, and
/// how much of this computer's disk it takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "mounts a filesystem: run in the VM"]
async fn binary_status_says_when_it_last_checked_and_what_the_folder_takes() {
    let (f, _graph) = harness_onedrive().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| f.service.state().get().cycle.last_checked > 0).await;

    let text = out_text(&run(addr, &["sync", "status"]));
    let checked = text.lines().find(|l| l.starts_with("Last checked:")).unwrap_or_else(|| panic!("{text}"));
    assert!(checked.ends_with(" s ago"), "{text}");
    let space = text.lines().find(|l| l.starts_with("On this computer:")).unwrap_or_else(|| panic!("{text}"));
    assert!(space.ends_with(" B") || space.ends_with("iB"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "mounts a filesystem: run in the VM"]
async fn binary_refresh_asks_onedrive_now() {
    let (f, graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    let before = graph.received_requests().await.unwrap().len();

    let out = run(f._bus.address(), &["sync", "refresh"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Asked OneDrive for changes"), "{out:?}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(graph.received_requests().await.unwrap().len() > before);
}


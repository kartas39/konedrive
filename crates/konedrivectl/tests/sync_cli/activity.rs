use super::*;

// --- Activity, transfers, conflicts, free-up, status lines -----

/// The binary, with `TZ` set so the times it prints are UTC.
fn run_utc(bus_addr: &str, args: &[&str]) -> std::process::Output {
    common::run_env(bus_addr, args, &[("TZ", "UTC")])
}

/// A folder registered without interception, with no helper anywhere, and
/// `names` in it downloaded, 64 KiB each.
async fn downloaded_files(f: &Harness, names: &[&str]) -> PathBuf {
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for name in names {
        std::fs::write(source.join(name), vec![6u8; 64 * 1024]).unwrap();
    }
    f.proxy.folder.register_without_interception(root.to_str().unwrap()).await.unwrap();
    f.proxy.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    for name in names {
        f.files.hydrate(root.join(name).to_str().unwrap()).await.unwrap();
    }
    root
}

/// `sync activity`: time, kind, path and detail, newest first, at most
/// `--limit` of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_activity_lists_what_happened_newest_first() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let out = run_utc(addr, &["sync", "activity"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "Nothing has happened yet.");

    let root = downloaded_files(&f, &["a.bin"]).await;
    let file = root.join("a.bin");
    f.files.dehydrate(file.to_str().unwrap()).await.unwrap();

    let out = run_utc(addr, &["sync", "activity", "--limit", "5"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].contains("freed") && lines[0].contains(file.to_str().unwrap()), "newest first: {text}");
    assert!(lines[1].contains("downloaded") && lines[1].contains("64.0 KiB"), "{text}");
    let time = lines[1].split("  ").next().unwrap();
    assert_eq!(time.len(), "2026-09-24 10:00:00".len(), "a time first: {text}");

    let out = run_utc(addr, &["sync", "activity", "--limit", "1"]);
    assert_eq!(out_text(&out).lines().count(), 1, "{}", out_text(&out));
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

    let entry = f.service.report().transfers.start("/home/u/OneDrive/big.bin".into(), 4 << 20);
    entry.progress(1 << 20, 4 << 20);
    // The totals are counted at most once a second.
    wait_for(|| f.service.state().get().transfers.queue.down.left_count == 1).await;
    let out = run(addr, &["sync", "transfers"]);
    let text = out_text(&out);
    assert!(out.status.success(), "{out:?}");
    // "N now" counts files, not slots: the one downloading, which holds none here.
    assert!(text.starts_with("Downloading:  1 now, 1 file left (3.0 MiB), 0 B done, 0 B/s\n"), "{text}");
    let list = text.lines().nth(3).unwrap_or_default();
    assert!(list.starts_with("down ") && list.contains("/home/u/OneDrive/big.bin") && list.contains("25%") && list.contains("4.0 MiB"), "{text}");
    drop(entry);
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

    let rescued = f.dir.path().join("rescued/2023-11-14T22-13-20Z/docs/f.txt");
    std::fs::create_dir_all(rescued.parent().unwrap()).unwrap();
    std::fs::write(&rescued, b"mine").unwrap();
    let activity = &f.service.report().activity;
    // The activity log is blocking code: a plain thread of its own, off the runtime.
    std::thread::scope(|scope| scope.spawn(|| activity.attach(konedrive_tree::Store::new(konedrive_tree::TreeStore::in_memory().unwrap()), f.dir.path())).join().unwrap());
    let conflict = konedrive_tree::ConflictRow {
        at: 1_700_000_000,
        original: "/home/u/OneDrive/docs/f.txt".into(),
        rescued: rescued.display().to_string(),
        kind: konedrive_tree::ConflictKind::Rescued,
    };
    std::thread::scope(|scope| scope.spawn(|| activity.add_conflicts(vec![conflict])).join().unwrap());

    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(
        status.lines().any(|l| l == "Conflicts:              1 (see `konedrivectl sync conflicts`)"),
        "{status}"
    );
    let text = out_text(&run_utc(addr, &["sync", "conflicts"]));
    assert!(text.contains("/home/u/OneDrive/docs/f.txt"), "{text}");
    assert!(text.contains(rescued.to_str().unwrap()), "{text}");
    assert!(text.contains("2023-11-14 22:13:20"), "{text}");

    let out = run(addr, &["sync", "dismiss", rescued.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Dismissed"), "{out:?}");
    assert!(rescued.exists(), "the file itself is left where it is");
    assert_eq!(out_text(&run(addr, &["sync", "conflicts"])).trim(), "No conflicts.");

    let text = refused(addr, &["sync", "dismiss", "/nowhere/f.txt"]);
    assert!(text.contains("/nowhere/f.txt"), "{text}");
}

/// `account remove` says what it kept: the folder, and where the files the conflicts list
/// named were rescued to — read from the list, not assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_remove_says_where_the_listed_rescues_are() {
    let f = harness_with_helper(false).await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register_without_interception(root.to_str().unwrap()).await.unwrap();
    let batch = f.dir.path().join("elsewhere/2023-11-14T22-13-20Z");
    let rescued = batch.join("docs/f.txt");
    std::fs::create_dir_all(rescued.parent().unwrap()).unwrap();
    std::fs::write(&rescued, b"mine").unwrap();
    let activity = &f.service.report().activity;
    // The activity log is blocking code: a plain thread of its own, off the runtime.
    std::thread::scope(|scope| scope.spawn(|| activity.attach(konedrive_tree::Store::new(konedrive_tree::TreeStore::in_memory().unwrap()), f.dir.path())).join().unwrap());
    let conflict = konedrive_tree::ConflictRow {
        at: 1_700_000_000,
        original: root.join("docs/f.txt").display().to_string(),
        rescued: rescued.display().to_string(),
        kind: konedrive_tree::ConflictKind::Rescued,
    };
    std::thread::scope(|scope| scope.spawn(|| activity.add_conflicts(vec![conflict])).join().unwrap());

    let out = run(f._bus.address(), &["account", "remove", "Personal"]);
    assert!(out.status.success(), "{out:?}");
    let said = out_text(&out);
    assert!(said.contains(&format!("Kept: the folder {}", root.display())), "{said}");
    assert!(said.contains(&format!("Kept: the 1 file the conflicts list named, rescued in {}\n", batch.display())), "{said}");
    assert!(!said.contains(".local/share"), "no place it cannot know: {said}");
    assert!(rescued.exists());
}

/// `sync free-up-space`: what it freed, and what it kept because it was in
/// use.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_free_up_space_says_what_it_freed_and_what_was_in_use() {
    let f = harness_with_helper(false).await;
    let root = downloaded_files(&f, &["a.bin", "b.bin"]).await;
    use std::os::unix::fs::MetadataExt;
    let freed = std::fs::metadata(root.join("a.bin")).unwrap().blocks() * 512;
    let _in_use = std::fs::File::open(root.join("b.bin")).unwrap();

    let out = run(f._bus.address(), &["sync", "free-up-space"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out_text(&out).trim(),
        format!("Freed 1 file ({}). 1 file was in use and kept.", konedrivectl::text::formats::human_bytes(freed))
    );
}

/// `sync pin` keeps a folder on this device, and everything in it
/// downloads; `sync status` counts it; `sync free` of a file the folder keeps
/// is refused, naming the folder; `sync free` of the folder stops keeping it
/// and frees up what is in it.
/// A folder registered without interception, with no helper, holding
/// `docs/a.bin` and `docs/b.bin`, 64 KiB each, neither downloaded: the paths
/// of `docs`, `a.bin` and `b.bin`.
async fn docs_to_pin(f: &Harness) -> (PathBuf, PathBuf, PathBuf) {
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let source = f.dir.path().join("source");
    std::fs::create_dir_all(source.join("docs")).unwrap();
    for name in ["docs/a.bin", "docs/b.bin"] {
        std::fs::write(source.join(name), vec![4u8; 64 * 1024]).unwrap();
    }
    f.proxy.folder.register_without_interception(root.to_str().unwrap()).await.unwrap();
    f.proxy.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    (root.join("docs"), root.join("docs/a.bin"), root.join("docs/b.bin"))
}

/// A file's `user.konedrive.state`, read by name.
fn state(path: &PathBuf) -> Vec<u8> {
    xattr::get(path, "user.konedrive.state").unwrap().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_pin_keeps_a_folder_here_and_free_lets_it_go() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let (docs, a, b) = docs_to_pin(&f).await;

    let out = run(addr, &["sync", "pin", docs.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "Kept on this device. 2 files are downloading (`konedrivectl sync transfers`).");
    wait_for(|| state(&a) == b"hydrated" && state(&b) == b"hydrated").await;
    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(status.lines().any(|l| l == "Always on this device:  1"), "{status}");

    let told = refused(addr, &["sync", "free", a.to_str().unwrap()]);
    assert!(told.contains(&format!("because the folder {} is", docs.display())), "{told}");
    assert_eq!(state(&a), b"hydrated");

    let out = run(addr, &["sync", "free", docs.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Freed 2 files ("), "{}", out_text(&out));
    assert_eq!((state(&a), state(&b)), (b"online-only".to_vec(), b"online-only".to_vec()));
    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(status.lines().any(|l| l == "Always on this device:  0"), "{status}");
}

/// `sync open` prints the address of the item's page — of a file, and of the
/// account's folder itself, which is the drive's root — and starts no opener: a stand-in
/// `xdg-open` that records what it is given is alone in `PATH`, `KONEDRIVE_NO_BROWSER` is
/// empty, and the piped stdout alone, or `--print`, keeps it from being called. A file that
/// is not uploaded yet is explained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let new = root.join("new.txt");
    std::fs::write(&new, b"new").unwrap();
    write_state(&std::fs::File::open(&new).unwrap(), State::Hydrated).unwrap();
    let told = refused(addr, &["sync", "open", new.to_str().unwrap()]);
    assert!(told.contains(&format!("{} is not uploaded yet, so it has no page in OneDrive", new.display())), "{told}");

    let outside = f.dir.path().join("elsewhere.txt");
    std::fs::write(&outside, b"x").unwrap();
    let told = refused(addr, &["sync", "open", outside.to_str().unwrap()]);
    assert!(told.contains("is not inside the sync folder") && told.contains("has a page in OneDrive"), "{told}");
}

/// `sync unpin` takes a folder's pin off and leaves its files downloaded.
/// Asked of files the folder keeps, it is refused, naming the first such
/// file alone and the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_unpin_stops_keeping_a_folder_and_leaves_its_files() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let (docs, a, b) = docs_to_pin(&f).await;
    f.files.pin(&[docs.to_str().unwrap()]).await.unwrap();
    wait_for(|| state(&a) == b"hydrated" && state(&b) == b"hydrated").await;

    let told = refused(addr, &["sync", "unpin", b.to_str().unwrap(), a.to_str().unwrap()]);
    let expected = format!("{} is kept on this device because the folder {} is", b.display(), docs.display());
    assert!(told.contains(&expected), "{told}");
    assert!(told.contains(&format!("`konedrivectl sync unpin {}`", docs.display())), "{told}");

    let out = run(addr, &["sync", "unpin", docs.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("No longer kept on this device."), "{}", out_text(&out));
    assert_eq!(xattr::get(&docs, "user.konedrive.pin").unwrap(), None);
    assert_eq!((state(&a), state(&b)), (b"hydrated".to_vec(), b"hydrated".to_vec()), "the files stay");
    assert_eq!(f.proxy.folder.pinned_count().await.unwrap(), 0);
}

/// `sync status` says when the folder was last checked with OneDrive, and
/// how much of this computer's disk it takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_refresh_of_a_local_folder_says_it_is_not_connected_to_onedrive() {
    let f = harness().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();

    let text = refused(f._bus.address(), &["sync", "refresh"]);
    assert!(text.contains("not connected to OneDrive"), "{text}");
}

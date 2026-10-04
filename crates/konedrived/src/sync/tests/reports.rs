use super::*;

// --- What a download or a free-up reports -----------------

/// `Hydrate` finishing is a `downloaded` event with the
/// file's size; one that fails is a `failed` event saying why.
#[tokio::test]
async fn hydrate_is_recorded_as_downloaded_and_a_failed_one_as_failed() {
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 2048]).await;
    let target = root_dir.path().join("f.bin");
    service.hydrate_now(&target).await.unwrap();
    // A placeholder whose item the source does not have.
    drop(placeholder(root_dir.path(), "gone.bin", "gone.bin", 100));
    let gone = root_dir.path().join("gone.bin");
    assert!(service.hydrate_now(&gone).await.is_err());

    let shown = |path: &Path| path.display().to_string();
    assert_eq!(
        activity_of(&service).await,
        vec![
            ("downloaded".to_owned(), shown(&target), "2.0 KiB".to_owned()),
            ("failed".to_owned(), shown(&gone), "it could not be downloaded".to_owned()),
        ]
    );
}

/// For a fill on open: the helper's request, filled through
/// `serve_hydrations_reporting`, is a `downloaded` event under the name
/// the file has — sent after the opener is answered.
#[tokio::test]
async fn a_fill_on_open_is_recorded_as_downloaded() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("ITEM"), vec![3u8; 4096]).unwrap();
    let fd = placeholder(&folder, "opened.bin", "ITEM", 4096);
    // Events are kept only for the folder registered now.
    let report = Report::new(SyncStateHandle::new(SyncSnapshot {
        root_path: folder.display().to_string(),
        ..SyncSnapshot::default()
    }));
    let mut added = report.activity.subscribe();

    let socket_path = folder.join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));
    tx.send(HydrateRequest { req_id: 9, fd }).await.unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv()).await.unwrap().unwrap();
    assert_eq!(answered, (9, 0));

    let event = tokio::time::timeout(Duration::from_secs(10), added.recv()).await.unwrap().unwrap();
    let opened = folder.join("opened.bin").display().to_string();
    assert_eq!((event.kind.as_str(), event.path.as_str(), event.detail.as_str()), ("downloaded", opened.as_str(), "4.0 KiB"));
}

/// A source that answers "not found" once it is let go.
struct Gated(std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>);

#[async_trait]
impl ContentSource for Gated {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let gate = self.0.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        Err(SourceError::NotFound("not there".into()))
    }
}

/// `Transfers`: a download is listed, with how far it has
/// got, for as long as it runs — and not a moment after, whether it
/// finished or failed.
#[tokio::test]
async fn a_download_shows_in_transfers_until_it_ends_however_it_ends() {
    use tokio::io::AsyncWriteExt;
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 64 * 1024]).await;
    let mut transfers = service.report().transfers.subscribe();
    let target = root_dir.path().join("f.bin");
    let shown = target.display().to_string();
    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    install_source(&service, Arc::new(Piped { reader: std::sync::Mutex::new(Some(reader)), size: 64 * 1024 }));
    let filling = {
        let (service, target) = (Arc::clone(&service), target.clone());
        tokio::spawn(async move { service.hydrate_now(&target).await })
    };
    writer.write_all(&[9u8; 16 * 1024]).await.unwrap();
    let halfway = |all: &std::collections::BTreeMap<u64, activity::Transfer>| {
        all.values().any(|t| t.path == shown && (t.done, t.total) == (16 * 1024, 64 * 1024))
    };
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(halfway)).await.unwrap().unwrap();
    writer.write_all(&[9u8; 48 * 1024]).await.unwrap();
    drop(writer);
    filling.await.unwrap().unwrap();
    assert_eq!(service.transfers(), Vec::new(), "a finished download is not listed");

    drop(placeholder(root_dir.path(), "g.bin", "g.bin", 100));
    let failing_target = root_dir.path().join("g.bin");
    let failing_shown = failing_target.display().to_string();
    let (open, gate) = tokio::sync::oneshot::channel();
    install_source(&service, Arc::new(Gated(std::sync::Mutex::new(Some(gate)))));
    let failing = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.hydrate_now(&failing_target).await })
    };
    let listed = |all: &std::collections::BTreeMap<u64, activity::Transfer>| all.values().any(|t| t.path == failing_shown);
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(listed)).await.unwrap().unwrap();
    open.send(()).unwrap();
    assert!(failing.await.unwrap().is_err());
    assert_eq!(service.transfers(), Vec::new(), "a failed download is not listed either");
}

/// A service with a folder registered without interception and no
/// helper anywhere, filled from `files` (name, size), each downloaded
/// when `hydrated` says so.
async fn local_folder(files: &[(&str, usize, bool)]) -> (Arc<SyncService>, tempfile::TempDir, tempfile::TempDir) {
    let service = testing::service(None, None, None);
    let dir = tempfile::tempdir().unwrap();
    service.hub().set_socket(dir.path().join("no-helper.sock"));
    let source_dir = dir.path().join("source");
    std::fs::create_dir(&source_dir).unwrap();
    for (name, size, _) in files {
        std::fs::write(source_dir.join(name), vec![5u8; *size]).unwrap();
    }
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(&source_dir).await.unwrap();
    for (name, _, hydrated) in files {
        if *hydrated {
            service.hydrate_now(&root_dir.path().join(name)).await.unwrap();
        }
    }
    (service, root_dir, dir)
}

/// `FreeUpSpace`: every downloaded file freed up through the
/// per-file path, except one that is open — counted as busy, left as it
/// is, and no error. The bytes are the blocks given back, as `stat`
/// reads them before and after: how many a 64 KiB file takes is the
/// filesystem's own business (btrfs gives it 64 KiB, the runner's ext4
/// 68), so the activity is checked against that, not a fixed figure.
/// And a Forget of the folder takes its activity with it.
#[tokio::test]
async fn free_up_space_frees_what_is_not_in_use_and_counts_what_is() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true), ("b.bin", 64 * 1024, true)]).await;
    let (a, b) = (root_dir.path().join("a.bin"), root_dir.path().join("b.bin"));
    let before = data_blocks(&a);
    let _in_use = std::fs::File::open(&b).unwrap();

    let freed = service.free_up_space().await.unwrap();

    let given_back = (before - data_blocks(&a)) * 512;
    assert_eq!(freed, FreedUp { files: 1, bytes: given_back, busy: 1, modified: 0, pinned: 0 });
    assert!(freed.bytes >= 64 * 1024, "{freed:?}");
    assert_eq!(service.item_state(&a).await, "online-only");
    assert_eq!(service.item_state(&b).await, "hydrated", "an open file is left as it is");
    let folder = root_dir.path().display().to_string();
    let detail = format!("1 file, {}", activity::human_size(given_back));
    assert_eq!(activity_of(&service).await.pop().unwrap(), ("freed".to_owned(), folder, detail));

    service.unregister_root().await.unwrap();
    assert!(activity_of(&service).await.is_empty(), "a Forget drops the activity");
}

/// `LocalBytes`, for a folder with a placeholder and a
/// downloaded file: what the downloaded file takes (and the placeholder
/// its next to nothing), measured on its own after the download. On the
/// paused clock, so the five seconds between two walks cost nothing.
#[tokio::test(start_paused = true)]
async fn local_bytes_are_what_the_downloaded_file_takes() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true), ("b.bin", 64 * 1024, false)]).await;
    let (a, b) = (root_dir.path().join("a.bin"), root_dir.path().join("b.bin"));
    assert!(data_blocks(&b) < 8, "b.bin is a placeholder");
    let expected = (data_blocks(&a) + data_blocks(&b)) * 512;
    assert!(expected >= 64 * 1024);
    let mut state = service.state().subscribe();
    tokio::time::timeout(Duration::from_secs(60), state.wait_for(|s| s.local_bytes == expected))
        .await
        .unwrap_or_else(|_| panic!("LocalBytes stayed {}, not {expected}", service.status().1))
        .unwrap();
}

// --- Activity and space accounting corner cases ---------------------------

/// Item 4: a download that ends after its folder was forgotten records
/// nothing in the folder registered next — `Hydrate` takes no lifecycle
/// lock, so a Forget does not wait for it.
#[tokio::test]
async fn a_download_that_ends_after_its_folder_is_forgotten_is_not_in_the_next_ones_activity() {
    let (service, root_a, _dir) = local_folder(&[("a.bin", 4096, false)]).await;
    let (open, gate) = tokio::sync::oneshot::channel();
    install_source(&service, Arc::new(Gated(std::sync::Mutex::new(Some(gate)))));
    let mut transfers = service.report().transfers.subscribe();
    let filling = {
        let (service, a) = (Arc::clone(&service), root_a.path().join("a.bin"));
        tokio::spawn(async move { service.hydrate_now(&a).await })
    };
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| !all.is_empty())).await.unwrap().unwrap();

    service.unregister_root().await.unwrap();
    let root_b = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_b.path()).await.unwrap();
    open.send(()).unwrap();
    assert!(filling.await.unwrap().is_err());

    assert_eq!(activity_of(&service).await, Vec::new(), "folder A's download is not folder B's activity");
}

/// Item 5: the walker measuring `LocalBytes` ends with the service —
/// it held the state, so nothing waiting on it ever saw the end.
#[tokio::test(start_paused = true)]
async fn dropping_the_service_ends_its_walker() {
    let (service, _root, _dir) = local_folder(&[("a.bin", 4096, true)]).await;
    assert!(service.report().space.running(), "the registration started it");
    let mut state = service.state().subscribe();
    drop(service);
    let ended = tokio::time::timeout(Duration::from_secs(60), async { while state.changed().await.is_ok() {} }).await;
    assert!(ended.is_ok(), "something still holds the state: the walker");
}

/// Item 6: a fill gives its slot back before it records what it did. The
/// log is held still here, so every recording waits: with four slots
/// held by fills that are only recording, a fifth request was never
/// filled at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fill_lets_go_of_its_slot_before_it_records() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let report = Report::new(SyncStateHandle::new(SyncSnapshot {
        root_path: folder.display().to_string(),
        ..SyncSnapshot::default()
    }));
    let socket_path = folder.join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(8);
    let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));

    let held = report.activity.hold();
    for n in 0..5u64 {
        std::fs::write(source_dir.path().join(format!("ITEM{n}")), vec![1u8; 1024]).unwrap();
        let fd = placeholder(&folder, &format!("f{n}.bin"), &format!("ITEM{n}"), 1024);
        tx.send(HydrateRequest { req_id: n, fd }).await.unwrap();
    }
    for _ in 0..5 {
        let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("a request waited for a slot held by a fill that was only recording");
        assert_eq!(answered.unwrap().1, 0);
    }
    drop(held);
}

/// Item 8: a file a download or another free-up holds the per-inode lock
/// of is busy, not waited for.
#[tokio::test]
async fn free_up_space_counts_a_file_whose_lock_is_taken_as_busy() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true)]).await;
    let a = root_dir.path().join("a.bin");
    let key = InodeKey::of(&std::fs::File::open(&a).unwrap()).unwrap();
    // The descriptor is closed again: only the lock stands in the way.
    let _held = service.locks().lock(key).await;
    let freed = tokio::time::timeout(Duration::from_secs(10), service.free_up_space())
        .await
        .expect("it waited for the lock")
        .unwrap();
    assert_eq!(freed, FreedUp { files: 0, bytes: 0, busy: 1, modified: 0, pinned: 0 });
    assert_eq!(service.item_state(&a).await, "hydrated");
}

/// Item 8: a downloaded file changed here is neither freed nor busy:
/// it is left, as `Dehydrate` would leave it.
#[tokio::test]
async fn free_up_space_counts_a_file_changed_here_in_neither() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true)]).await;
    let a = root_dir.path().join("a.bin");
    std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&a).unwrap(), b"mine").unwrap();
    let freed = service.free_up_space().await.unwrap();
    assert_eq!(freed, FreedUp { files: 0, bytes: 0, busy: 0, modified: 1, pinned: 0 });
    assert!(std::fs::read(&a).unwrap().ends_with(b"mine"), "the change is kept");
}

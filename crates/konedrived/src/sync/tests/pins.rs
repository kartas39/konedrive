use super::*;

// --- "Always keep on this device" -----------------------------------------

/// A folder registered without interception, with no helper anywhere,
/// filled with `docs/a.bin`, `docs/b.bin` and `c.bin`, 64 KiB each, none
/// of them downloaded. Returns the folder's path as registered.
async fn folder_to_pin() -> (Arc<SyncService>, PathBuf, tempfile::TempDir, tempfile::TempDir) {
    let service = testing::service(None, None, None);
    let dir = tempfile::tempdir().unwrap();
    service.hub().set_socket(dir.path().join("no-helper.sock"));
    let source = dir.path().join("source");
    std::fs::create_dir_all(source.join("docs")).unwrap();
    for name in ["docs/a.bin", "docs/b.bin", "c.bin"] {
        std::fs::write(source.join(name), vec![3u8; 64 * 1024]).unwrap();
    }
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(&source).await.unwrap();
    (service, root_dir.path().canonicalize().unwrap(), root_dir, dir)
}

/// Waits until nothing a pin asked for is pending or downloading: each
/// download recorded, since a file leaves the queue only after that.
async fn pinned_downloads_done(service: &SyncService) {
    for _ in 0..1000 {
        if service.pins.queued().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("still queued: {:?}", service.pins.queued());
}

fn pin_of(path: &Path) -> Option<Vec<u8>> {
    xattr::get(path, konedrive_fs::placeholder::XATTR_PIN).unwrap()
}

/// Records the range every fetch asks for.
struct RecordsRanges {
    inner: LocalDir,
    asked: Mutex<Vec<(String, Option<u64>)>>,
}

#[async_trait]
impl ContentSource for RecordsRanges {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        self.asked.lock().unwrap().push((item_id.to_string(), end));
        self.inner.fetch(item_id, from, end).await
    }
}

/// Issue #28: a large file being opened (`Hydrate`) keeps one stream, open-ended; the
/// same kind of file pinned downloads in parts, each asking for a bounded range.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_file_being_opened_keeps_one_stream_and_a_pinned_one_goes_in_parts() {
    let service = testing::service(None, None, None);
    let dir = tempfile::tempdir().unwrap();
    service.hub().set_socket(dir.path().join("no-helper.sock"));
    let source = dir.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    // Placeholders as large as a large file (sparse, nothing on disk)...
    for name in ["opened.bin", "pinned.bin"] {
        File::create(source.join(name)).unwrap().set_len(konedrive_graph::pool::LARGE_FROM).unwrap();
    }
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(&source).await.unwrap();
    // ...whose content turns out small: the fill takes the source's size.
    for name in ["opened.bin", "pinned.bin"] {
        std::fs::write(source.join(name), vec![7u8; 300_000]).unwrap();
    }
    let recording = Arc::new(RecordsRanges { inner: LocalDir::new(&source), asked: Mutex::default() });
    install_source(&service, Arc::clone(&recording) as Arc<dyn ContentSource>);
    let root = root_dir.path().canonicalize().unwrap();

    service.hydrate_now(&root.join("opened.bin")).await.unwrap();
    service.pin(&[root.join("pinned.bin")]).await.unwrap();
    pinned_downloads_done(&service).await;

    for name in ["opened.bin", "pinned.bin"] {
        assert_eq!(std::fs::read(root.join(name)).unwrap(), vec![7u8; 300_000], "{name}");
    }
    let asked = recording.asked.lock().unwrap().clone();
    let ends = |name: &str| asked.iter().filter(|(id, _)| id == name).map(|(_, end)| *end).collect::<Vec<_>>();
    assert_eq!(ends("opened.bin"), vec![None], "a file being opened is not split");
    assert_eq!(ends("pinned.bin"), vec![Some(source::parts::PIECE)], "a pinned one asks for its first piece");
}

/// Pinning a folder queues every online-only file in it, and each is
/// downloaded through the ordinary fill, `downloaded` event and all.
/// What is outside the folder is left alone, and a file inside it is not
/// pinned again on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinning_a_folder_downloads_what_is_in_it() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let (a, b, c) = (root.join("docs/a.bin"), root.join("docs/b.bin"), root.join("c.bin"));

    assert_eq!(service.pin(&[root.join("docs")]).await.unwrap(), 2);
    pinned_downloads_done(&service).await;

    assert_eq!(std::fs::read(&a).unwrap(), vec![3u8; 64 * 1024]);
    assert_eq!(service.item_state(&b).await, "hydrated");
    assert_eq!(service.item_state(&c).await, "online-only");
    assert_eq!(pin_of(&root.join("docs")), Some(b"1".to_vec()));
    assert_eq!(service.pinned_count(), 1);
    let downloaded: Vec<String> =
        activity_of(&service).await.into_iter().filter(|(kind, ..)| kind == "downloaded").map(|(_, path, _)| path).collect();
    assert_eq!(downloaded.len(), 2, "{downloaded:?}");

    assert_eq!(service.pin(&[a.clone()]).await.unwrap(), 0);
    assert_eq!(pin_of(&a), None, "the folder pins it already");
    assert_eq!(service.pinned_count(), 1);
}

/// Free up space on a file a pinned folder keeps is refused, naming the
/// folder — through `FreeUp` and through `Dehydrate` alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeing_up_what_a_pinned_folder_keeps_is_refused_naming_the_folder() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let (docs, a) = (root.join("docs"), root.join("docs/a.bin"));
    service.pin(&[docs.clone()]).await.unwrap();
    pinned_downloads_done(&service).await;

    let refused = service.free_up(&[a.clone()]).await.unwrap_err();
    let expected = format!("{} is pinned by {}: unpin it first", a.display(), docs.display());
    assert!(matches!(&refused, SyncError::NotAllowed(why) if *why == expected), "{refused:?}");
    assert!(matches!(service.dehydrate(&a).await, Err(SyncError::NotAllowed(_))));
    assert!(matches!(service.unpin(&[a.clone()]).await, Err(SyncError::NotAllowed(_))));
    assert_eq!(service.item_state(&a).await, "hydrated");
    assert_eq!(pin_of(&docs), Some(b"1".to_vec()));

    // With the folder in the same call, whose pin that call takes off.
    let freed = service.free_up(&[docs.clone(), a.clone()]).await.unwrap();
    assert_eq!((freed.files, freed.pinned), (2, 0), "{freed:?}");
    assert_eq!((pin_of(&docs), service.pinned_count()), (None, 0));
}

/// A file queued while pinned whose pin is gone by its turn is not
/// downloaded.
#[tokio::test]
async fn a_queued_file_no_longer_pinned_is_not_downloaded() {
    use pin::PinFill;
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let a = root.join("docs/a.bin");

    assert_eq!(service.fill_pinned(&a).await, pin::Filled::Skipped);

    assert_eq!(service.item_state(&a).await, "online-only");
    assert!(activity_of(&service).await.is_empty(), "nothing was fetched");
}

/// Free up space on a pinned folder takes its pin off and frees what is
/// in it — but a file with a pin of its own stays, counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeing_up_a_pinned_folder_unpins_it_and_frees_all_but_a_pin_below() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let (docs, a, b) = (root.join("docs"), root.join("docs/a.bin"), root.join("docs/b.bin"));
    service.pin(&[b.clone()]).await.unwrap();
    service.pin(&[docs.clone()]).await.unwrap();
    pinned_downloads_done(&service).await;
    assert_eq!(service.pinned_count(), 2);

    let freed = service.free_up(&[docs.clone()]).await.unwrap();

    assert_eq!((freed.files, freed.busy, freed.pinned), (1, 0, 1), "{freed:?}");
    assert!(freed.bytes >= 64 * 1024, "{freed:?}");
    assert_eq!(service.item_state(&a).await, "online-only");
    assert_eq!(service.item_state(&b).await, "hydrated", "its own pin keeps it");
    assert_eq!((pin_of(&docs), pin_of(&b)), (None, Some(b"1".to_vec())));
    assert_eq!(service.pinned_count(), 1);
}

/// `FreeUpSpace` leaves every file a pin keeps, and counts them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn free_up_space_leaves_pinned_files_and_counts_them() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let c = root.join("c.bin");
    service.pin(&[root.join("docs")]).await.unwrap();
    service.hydrate_now(&c).await.unwrap();
    pinned_downloads_done(&service).await;

    let freed = service.free_up_space().await.unwrap();

    assert_eq!((freed.files, freed.busy, freed.pinned), (1, 0, 2), "{freed:?}");
    assert_eq!(service.item_state(&c).await, "online-only");
    assert_eq!(service.item_state(&root.join("docs/a.bin")).await, "hydrated");
    assert_eq!(service.item_state(&root.join("docs/b.bin")).await, "hydrated");
}

/// Item 8: a download whose caller goes away — a D-Bus call dropped, a
/// replacement stopped with its poller — leaves `Transfers` with it.
#[tokio::test]
async fn a_cancelled_download_leaves_transfers() {
    use tokio::io::AsyncWriteExt;
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 64 * 1024]).await;
    let mut transfers = service.report().transfers.subscribe();
    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    install_source(&service, Arc::new(Piped { reader: std::sync::Mutex::new(Some(reader)), size: 64 * 1024 }));
    let filling = {
        let (service, target) = (Arc::clone(&service), root_dir.path().join("f.bin"));
        tokio::spawn(async move { service.hydrate_now(&target).await })
    };
    writer.write_all(&[9u8; 16 * 1024]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| !all.is_empty())).await.unwrap().unwrap();
    filling.abort();
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| all.is_empty()))
        .await
        .expect("the cancelled download is still listed")
        .unwrap();
}

/// `populate_walk` walks a directory the *user* names — unlike
/// `root::recover`'s hardened, descriptor-based walk, it is explicitly
/// the offline test path, and its threat model does not include a
/// racing or adversarial filesystem. It does include an ordinary
/// mistake, though: a symlink somewhere in a source tree that points
/// back at one of its own ancestors. If the walk ever decided to
/// recurse on the strength of what a symlink points at, this would
/// never return. `tokio::time::timeout` is the backstop in case the fix
/// regresses; on a passing run it never comes close to firing.
#[tokio::test]
async fn populate_from_directory_does_not_follow_a_symlink_cycle_in_the_source() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("real.txt"), b"hello").unwrap();
    std::os::unix::fs::symlink(source_dir.path(), source_dir.path().join("loop")).unwrap();

    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    let created = tokio::time::timeout(
        Duration::from_secs(10),
        service.populate_from_directory(source_dir.path()),
    )
    .await
    .expect(
        "populate_from_directory did not return: a symlink cycle in the source was \
         descended",
    )
    .unwrap();

    assert_eq!(created, 1, "only the real file may produce a placeholder");
    assert!(
        !root_dir.path().join("loop").exists(),
        "a symlink to a directory must not be mirrored as one"
    );
}

/// The other half of the same fix: a symlink is never descended to
/// decide whether it is a directory, but a symlink to a *regular* file
/// is still worth a placeholder — the walk's read side, not its
/// recursion decision, follows it.
#[tokio::test]
async fn populate_from_directory_creates_a_placeholder_for_a_symlink_to_a_regular_file() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("real.bin"), vec![5u8; 4096]).unwrap();
    std::os::unix::fs::symlink(
        source_dir.path().join("real.bin"),
        source_dir.path().join("link.bin"),
    )
    .unwrap();

    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    let created = service.populate_from_directory(source_dir.path()).await.unwrap();

    assert_eq!(
        created, 2,
        "both the real file and the symlink to a regular file must be mirrored"
    );
    let placeholder = root_dir.path().join("link.bin");
    assert_eq!(
        std::fs::metadata(&placeholder).unwrap().len(),
        4096,
        "the placeholder must use the symlink's target's size"
    );
    assert_eq!(service.item_state(&placeholder).await, "online-only");
}

/// `item_state` must judge a file by the *currently* registered root,
/// not by whether the file happens to carry konedrive xattrs — a file
/// left behind by a root this daemon un-registered still carries them,
/// but it is not this root's business any more. This is the specific
/// claim `populate_from_directory_mirrors_the_tree_as_placeholders`
/// (in `tests/sync_dbus.rs`) does *not* actually pin: there, the
/// "outside the root" file has no konedrive xattrs at all, so it reads
/// `not-managed` even with the containment check deleted (`read_state`
/// returns `None` on its own). This test gives the file real, valid
/// xattrs, so only the containment check can produce `not-managed`.
#[tokio::test]
async fn item_state_of_a_file_outside_the_current_root_is_not_managed_even_with_real_xattrs() {
    let (service, _sockets, _helper) = service_with_helper().await;

    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("f.bin"), vec![3u8; 512]).unwrap();
    let root_a = tempfile::tempdir().unwrap();
    service.register_root(root_a.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    let left_behind = root_a.path().join("f.bin");
    assert_eq!(service.item_state(&left_behind).await, "online-only", "sanity check");

    service.unregister_root().await.unwrap();
    let root_b = tempfile::tempdir().unwrap();
    service.register_root(root_b.path()).await.unwrap();

    assert_eq!(
        service.item_state(&left_behind).await,
        "not-managed",
        "a real konedrive-managed file left behind by a different, no-longer-registered \
         root must not be reported as this root's own"
    );
}

#[tokio::test]
async fn a_file_recovery_finds_in_use_does_not_make_the_root_an_error() {
    // an interrupted file that something has open
    // — on reconnect, the suspended opener whose request is not served
    // yet, or a fill still running from the connection before
    // — refused recovery's lease and published `RootState = error`
    // with "could not reset", about a file that was then filled normally.
    let (service, _sockets, _helper) = service_with_helper().await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    let handle = std::fs::File::open(root_dir.path()).unwrap();
    konedrive_fs::placeholder::create_placeholder(
        &handle,
        "busy.bin",
        "ITEM",
        4096,
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    )
    .unwrap();
    let path = root_dir.path().join("busy.bin");
    let busy = std::fs::File::options().read(true).write(true).open(&path).unwrap();
    konedrive_fs::placeholder::write_state(&busy, State::Hydrating).unwrap();

    service.resume().await;

    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    // logged, not a `LastError` that outlives it.
    assert_eq!(service.last_error(), "");
    assert_eq!(service.item_state(&path).await, "hydrating", "and it is left as found");
}

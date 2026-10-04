use super::*;

/// `Hydrate()` asks the same question: in an intercepted root it clears the
/// mark before refilling a file that may carry one, and with the helper
/// gone it refuses rather than fill a file it could then have to punch.
#[tokio::test]
async fn hydrate_now_clears_the_ignore_mark_before_refilling_a_file_that_may_carry_one() {
    let (service, root_dir, _source_dir, _sockets, helper) =
        populated_service(&vec![5u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    let dehydrating = || {
        let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
        konedrive_fs::placeholder::write_state(&handle, State::Dehydrating).unwrap();
    };

    dehydrating();
    let link = service.link();
    service.hub().set_link(None);
    let refused = service.hydrate_now(&file).await;
    assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
    assert_eq!(service.item_state(&file).await, "dehydrating", "and nothing changed");

    service.hub().set_link(link);
    helper.forget();
    service.hydrate_now(&file).await.unwrap();
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark is cleared first");
    assert_eq!(std::fs::read(&file).unwrap(), vec![5u8; 4096]);
    assert_eq!(service.item_state(&file).await, "hydrated");
}

// --- Per-inode serialization, measured through the service -----------

/// The whole of C1. Two names for one inode must serialize.
/// Measured the way the review measured it: a hard link, a source slow
/// enough for both fills to overlap, and a count of how many were ever
/// in flight at once. A path-keyed table gives two — and a failing
/// source then has one fill's roll-back (`online-only` + `punch_all`)
/// land on top of the other's committed `hydrated`, which is a file
/// labelled `hydrated` over a hole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_names_for_one_inode_are_never_filled_at_the_same_time() {
    let (service, root_dir, source_dir, _sockets, _helper) =
        populated_service(&vec![9u8; 2048]).await;
    let one = root_dir.path().join("f.bin");
    let another = root_dir.path().join("g.bin");
    std::fs::hard_link(&one, &another).unwrap();
    let (source, peak) = CountingSource::new(source_dir.path(), Duration::from_millis(300));
    install_source(&service, source);

    let first = {
        let service = Arc::clone(&service);
        let path = one.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    let second = {
        let service = Arc::clone(&service);
        let path = another.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "two fills were in flight on one inode: the lock is keyed by something other than \
         the inode, so two names for one file do not serialize"
    );
    assert_eq!(std::fs::read(&one).unwrap(), vec![9u8; 2048]);
}

/// The other pair names: "a hydration request for a file being
/// dehydrated runs after the dehydration finishes", and the reverse.
/// The discriminator is the *outcome*, not the timing: a dehydration
/// that runs while the fill is still in flight sees `state=hydrating`
/// and is refused, so a `Dehydrate` that succeeds is one that waited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dehydration_waits_for_the_fill_of_the_same_inode() {
    let (service, root_dir, source_dir, _sockets, _helper) =
        populated_service(&vec![7u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_millis(400));
    install_source(&service, source);

    let fill = {
        let service = Arc::clone(&service);
        let path = file.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    wait_for_state(&service, &file, "hydrating").await;

    service.dehydrate(&file).await.expect(
        "the dehydration ran while the fill was still in flight: it saw `hydrating` and \
         refused, so nothing serialized the two",
    );
    fill.await.unwrap().unwrap();

    use std::os::unix::fs::MetadataExt;
    assert_eq!(service.item_state(&file).await, "online-only");
    assert!(std::fs::metadata(&file).unwrap().blocks() < 8, "the content is gone");
}

/// The same table, from the interception side: two suspended opens of
/// one inode must not be filled at once either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_hydrations_fills_one_inode_one_fill_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let first = placeholder(local.path(), "file.bin", "ITEM", 4096);
    // A second descriptor for the very same inode, exactly as two
    // suspended opens of one file would arrive.
    let second = std::fs::File::options()
        .read(true)
        .write(true)
        .open(local.path().join("file.bin"))
        .unwrap()
        .into();

    let (source, peak) = CountingSource::new(remote.path(), Duration::from_millis(300));
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 1, fd: first }).await.unwrap();
    tx.send(HydrateRequest { req_id: 2, fd: second }).await.unwrap();

    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("both fills must answer")
            .unwrap();
    }
    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "two suspended opens of one inode were filled at once"
    );
}

/// The other half of that, and the reason the key has to be the inode
/// rather than anything coarser: two *different* files must still be
/// filled at the same time. A lock that over-matches turns four
/// concurrent hydrations into a queue of one, which no other test here
/// would notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_hydrations_fills_two_different_inodes_at_the_same_time() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let one = placeholder(local.path(), "one.bin", "ITEM", 4096);
    let another = placeholder(local.path(), "another.bin", "ITEM", 4096);

    let (source, peak) = CountingSource::new(remote.path(), Duration::from_millis(300));
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 1, fd: one }).await.unwrap();
    tx.send(HydrateRequest { req_id: 2, fd: another }).await.unwrap();

    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("both fills must answer")
            .unwrap();
    }
    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "two unrelated files were filled one after the other: the lock matches more than \
         the inode it is supposed to"
    );
}

// --- The gate `hydrate_now` writes through -------------

/// A registration that has been removed or replaced no longer authorises
/// writing inside that folder. `dehydrate` checks this
/// — through `SyncRoot::open_inside`, which verifies the folder
/// still carries *this* root's id — and `hydrate_now` reached its target
/// through a `starts_with` on a canonicalized string, which cannot.
#[tokio::test]
async fn hydrate_now_refuses_a_root_that_no_longer_carries_its_registration() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![1u8; 2048]).await;
    let file = root_dir.path().join("f.bin");
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

    let error = service.hydrate_now(&file).await.unwrap_err();
    assert!(
        matches!(error, SyncError::OutsideRoot),
        "expected a refusal, got {error:?}"
    );
    use std::os::unix::fs::MetadataExt;
    assert!(
        std::fs::metadata(&file).unwrap().blocks() < 8,
        "nothing may be written through a registration that is gone"
    );
}

/// C3, as it was measured: the window is not a microsecond race. The
/// old order canonicalized the caller's path, checked *that string*
/// against the root, awaited the per-inode lock — which has no time
/// limit, because the fill it waits for has none — and then opened the
/// checked string by name. An ordinary directory rename inside the root
/// in that window sent the write outside the root, and `Hydrate`
/// reported success.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_swapped_while_a_hydration_waits_cannot_redirect_it() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let base = tempfile::tempdir().unwrap();
    let root_dir = base.path().join("root");
    let outside_dir = base.path().join("outside");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::create_dir(&outside_dir).unwrap();

    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("f.bin"), vec![5u8; 1024]).unwrap();
    service.register_root(&root_dir).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();

    // The file the measured probe emptied: outside the root, and
    // carrying exactly the xattrs that made the old code treat whatever
    // it opened as one of ours.
    let victim = outside_dir.join("f.bin");
    std::fs::write(&victim, b"the user's own data").unwrap();
    {
        let file = std::fs::File::options().read(true).write(true).open(&victim).unwrap();
        xattr::FileExt::set_xattr(&file, "user.konedrive.item-id", b"sub/f.bin").unwrap();
        konedrive_fs::placeholder::write_state(&file, State::OnlineOnly).unwrap();
    }

    let target = root_dir.join("sub").join("f.bin");
    let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_millis(400));
    install_source(&service, source);

    let first = {
        let service = Arc::clone(&service);
        let path = target.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    wait_for_state(&service, &target, "hydrating").await;
    let second = {
        let service = Arc::clone(&service);
        let path = target.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    // Long enough for the second call to be waiting, short enough to be
    // well inside the first fill's 400 ms.
    tokio::time::sleep(Duration::from_millis(60)).await;
    std::fs::rename(root_dir.join("sub"), base.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(&outside_dir, root_dir.join("sub")).unwrap();

    let _ = first.await.unwrap();
    let _ = second.await.unwrap();

    assert_eq!(
        std::fs::read(&victim).unwrap(),
        b"the user's own data",
        "a file outside the sync root was overwritten with hydration content"
    );
}

// --- `Hydrate` never reports success without the bytes ---------------

/// A source that cannot serve the item must not be reported as success:
/// the file is still a hole afterwards, and the caller was told so.
#[tokio::test]
async fn hydrate_now_reports_a_source_that_could_not_serve_the_file() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![2u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    // A source directory with nothing in it: every fetch is NotFound.
    let empty = tempfile::tempdir().unwrap();
    install_source(&service, Arc::new(LocalDir::new(empty.path())));

    let error = service.hydrate_now(&file).await.unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "expected a failure, got {error:?}");
    assert_eq!(
        service.item_state(&file).await,
        "online-only",
        "a failed fill must leave the file where the next open can retry it"
    );
    use std::os::unix::fs::MetadataExt;
    assert!(std::fs::metadata(&file).unwrap().blocks() < 8, "and holding nothing");
}

/// A file labelled `hydrated` over a hole is §9's named
/// failure, and a manual "download it now" is what repairs it — so
/// `Hydrate` must not believe the label on its own.
#[tokio::test]
async fn hydrate_now_fills_a_hydrated_label_that_has_no_stamp_behind_it() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![6u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    {
        let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
        konedrive_fs::placeholder::write_state(&handle, State::Hydrated).unwrap();
    }
    assert_eq!(service.item_state(&file).await, "hydrated", "the label says it is there");

    service.hydrate_now(&file).await.unwrap();

    assert_eq!(
        std::fs::read(&file).unwrap(),
        vec![6u8; 4096],
        "the file was labelled `hydrated` over a hole and `Hydrate` did nothing about it"
    );
}

/// The other side of the same check, and the reason it is a stamp
/// comparison rather than a size one: a `hydrated` file whose stamp does
/// **not** match was edited locally, and there is no upload in this
/// sub-project, so that edit is the only copy. Refuse loudly; never
/// overwrite it with remote content.
#[tokio::test]
async fn hydrate_now_refuses_a_hydrated_file_that_was_edited_locally() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![6u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();
    std::fs::write(&file, b"what the user typed").unwrap();

    let error = service.hydrate_now(&file).await.unwrap_err();
    assert!(
        matches!(error, SyncError::ModifiedLocally),
        "expected a refusal, got {error:?}"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"what the user typed",
        "a local edit is the only copy of that data and must not be overwritten"
    );
}

/// A zero-byte file is created
/// `hydrated` with no stamp (there is nothing to download), so freeing it
/// up answered `ModifiedLocally` and the command line told the user their
/// edits would be lost. It takes no space and there is nothing to free:
/// it succeeds and changes nothing.
#[tokio::test]
async fn freeing_up_a_zero_byte_file_succeeds_and_changes_nothing() {
    let (service, root_dir, _source_dir, _sockets, helper) = populated_service(&[]).await;
    let file = root_dir.path().join("f.bin");
    assert_eq!(service.item_state(&file).await, "hydrated");
    helper.forget();

    let freed = service.dehydrate(&file).await;

    assert!(freed.is_ok(), "{freed:?}");
    assert_eq!(service.item_state(&file).await, "hydrated");
    assert!(helper.seen().is_empty(), "nothing needed the helper: {:?}", helper.seen());
}

/// Freeing up a file under the read-only lock works and leaves it 0444.
/// The root is registered the way the test above has it; the file is
/// put in it by hand, hydrated and stamped, and then locked.
#[tokio::test]
async fn freeing_up_a_locked_file_works_and_leaves_it_locked() {
    use std::os::unix::fs::PermissionsExt;
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&[]).await;
    let root = service.root().unwrap();
    let path = root.path.join("locked.bin");
    std::fs::write(&path, vec![1u8; 8192]).unwrap();
    {
        let file = File::options().read(true).write(true).open(&path).unwrap();
        konedrive_fs::placeholder::write_item_id(&file, "L").unwrap();
        konedrive_fs::placeholder::write_state(&file, State::Hydrated).unwrap();
        konedrive_fs::placeholder::write_stamp(&file).unwrap();
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    service.dehydrate(&path).await.unwrap();
    assert_eq!(state_of_path(&path), Some(State::OnlineOnly));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444);
    drop(root_dir);
}

/// But a file that was *made* empty here is an edit like any other.
#[tokio::test]
async fn freeing_up_a_file_emptied_here_is_still_refused_as_modified() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![3u8; 2048]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();
    std::fs::File::options().write(true).open(&file).unwrap().set_len(0).unwrap();

    let freed = service.dehydrate(&file).await;

    assert!(matches!(freed, Err(SyncError::ModifiedLocally)), "{freed:?}");
}

/// A populate source inside the
/// root is a source whose files are placeholders: `LocalDir` reads them
/// through the daemon's own exemption, or with nothing intercepting at
/// all, so a fill copies zeros into a file it then stamps `hydrated` —
/// on the offline route the user will actually run. And a source that
/// contains the root is mirrored into itself. Both are refused, and
/// nothing is created.
#[tokio::test]
async fn a_populate_source_that_overlaps_the_root_is_refused() {
    let service = testing::service(None, None, None);
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    service.register_root_without_interception(&root).await.unwrap();
    let inside = root.join("source");
    std::fs::create_dir(&inside).unwrap();
    std::fs::write(inside.join("a.bin"), [1u8; 64]).unwrap();
    std::fs::write(outer.path().join("b.bin"), [2u8; 64]).unwrap();

    let from_inside = service.populate_from_directory(&inside).await;
    assert!(matches!(from_inside, Err(SyncError::Unsupported(_))), "{from_inside:?}");
    assert!(!root.join("a.bin").exists(), "nothing may be created from inside the root");

    let from_around = service.populate_from_directory(outer.path()).await;
    assert!(matches!(from_around, Err(SyncError::Unsupported(_))), "{from_around:?}");
    assert!(!root.join("b.bin").exists(), "nor from a directory containing it");
}

/// A root registered without interception, with one `online-only`
/// placeholder `b.bin` in it, populated from a source outside it.
async fn root_with_a_placeholder() -> (Arc<SyncService>, tempfile::TempDir, PathBuf, PathBuf) {
    let service = testing::service(None, None, None);
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    service.register_root_without_interception(&root).await.unwrap();
    let first = outer.path().join("first");
    std::fs::create_dir(&first).unwrap();
    std::fs::write(first.join("b.bin"), vec![7u8; 64 * 1024]).unwrap();
    service.populate_from_directory(&first).await.unwrap();
    let placeholder = root.join("b.bin");
    assert_eq!(service.item_state(&placeholder).await, "online-only", "sanity check");
    (service, outer, root, placeholder)
}

/// Whether `path` holds `hydrated` over nothing but zeros: the outcome
/// every guard in this module exists to prevent.
fn hydrated_over_zeros(path: &Path) -> bool {
    let file = File::open(path).unwrap();
    let hydrated = read_state(&file).unwrap() == Some(State::Hydrated);
    let content = std::fs::read(path).unwrap();
    hydrated && !content.is_empty() && content.iter().all(|&b| b == 0)
}

/// A source *directory* that overlaps the root is refused, but a source
/// *file* can still lead into it: a symlink in the source pointing at a
/// placeholder in the root, or a hardlink to one. `LocalDir` reads it
/// with nothing intercepting — or through the daemon's own exemption —
/// so a fill copies the placeholder's zeros into the file it fills, and
/// stamps it `hydrated`. Such a source is refused, and nothing is
/// created from it.
#[tokio::test]
async fn a_populate_source_file_that_leads_into_the_root_is_refused() {
    let (service, outer, root, placeholder) = root_with_a_placeholder().await;
    let second = outer.path().join("second");
    std::fs::create_dir(&second).unwrap();
    std::os::unix::fs::symlink(&placeholder, second.join("a.bin")).unwrap();

    let populated = service.populate_from_directory(&second).await;
    let mirrored = root.join("a.bin");
    let filled = if mirrored.exists() { Some(service.hydrate_now(&mirrored).await) } else { None };
    assert!(
        !(mirrored.exists() && hydrated_over_zeros(&mirrored)),
        "a symlink in the source to a placeholder in the root was filled with its zeros and \
         stamped hydrated (populate → {populated:?}, Hydrate → {filled:?})"
    );
    assert!(matches!(populated, Err(SyncError::Unsupported(_))), "{populated:?}");
    assert!(!mirrored.exists(), "nothing may be created from a source that leads into the root");

    let third = outer.path().join("third");
    std::fs::create_dir(&third).unwrap();
    std::fs::hard_link(&placeholder, third.join("h.bin")).unwrap();
    let populated = service.populate_from_directory(&third).await;
    let mirrored = root.join("h.bin");
    let filled = if mirrored.exists() { Some(service.hydrate_now(&mirrored).await) } else { None };
    assert!(
        !(mirrored.exists() && hydrated_over_zeros(&mirrored)),
        "a hardlink in the source to a placeholder was filled with its zeros and stamped \
         hydrated (populate → {populated:?}, Hydrate → {filled:?})"
    );
    assert!(matches!(populated, Err(SyncError::Unsupported(_))), "{populated:?}");
}

/// The same, decided again where the bytes are read: a
/// source file that pointed somewhere harmless when the folder was
/// populated and into the root by the time it is fetched is refused
/// there, and the file it would have filled stays `online-only`.
#[tokio::test]
async fn a_source_file_that_leads_into_the_root_by_the_time_it_is_fetched_is_refused() {
    let (service, outer, root, placeholder) = root_with_a_placeholder().await;
    let second = outer.path().join("second");
    std::fs::create_dir(&second).unwrap();
    let elsewhere = outer.path().join("elsewhere.bin");
    std::fs::write(&elsewhere, vec![9u8; 64 * 1024]).unwrap();
    std::os::unix::fs::symlink(&elsewhere, second.join("a.bin")).unwrap();
    service.populate_from_directory(&second).await.unwrap();
    let mirrored = root.join("a.bin");
    assert_eq!(service.item_state(&mirrored).await, "online-only", "sanity check");

    std::fs::remove_file(second.join("a.bin")).unwrap();
    std::os::unix::fs::symlink(&placeholder, second.join("a.bin")).unwrap();
    let filled = service.hydrate_now(&mirrored).await;

    assert!(
        !hydrated_over_zeros(&mirrored),
        "a source file that now leads into the root was read, and its zeros stamped \
         hydrated (Hydrate → {filled:?})"
    );
    assert!(filled.is_err(), "{filled:?}");
    assert_eq!(service.item_state(&mirrored).await, "online-only");
}

/// And the case that must stay a no-op: a file that really is there.
#[tokio::test]
async fn hydrate_now_does_nothing_to_a_file_that_is_already_there() {
    let (service, root_dir, source_dir, _sockets, _helper) =
        populated_service(&vec![3u8; 2048]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();

    let counted = Arc::new(LocalDir::new(source_dir.path()));
    install_source(&service, Arc::clone(&counted) as Arc<dyn ContentSource>);
    service.hydrate_now(&file).await.unwrap();
    assert_eq!(counted.fetches(), 0, "a complete file must not be fetched again");
}

// --- `ItemState` is a query ----------------------------

/// `ItemState` must answer without opening the file. In production the
/// reason is that an open of an `online-only` file under a marked
/// directory *is* a download — the whole file is fetched as a side
/// effect of asking what state it is in, and where nothing can serve it
/// the denial makes the open fail and the answer comes back
/// `not-managed` for a genuinely managed placeholder.
///
/// Unprivileged, interception cannot be reached at all, so the measured
/// property here is the open itself: a FIFO inside the folder answers
/// promptly if nothing opens it, and never answers at all if something
/// does — `open(O_RDONLY)` on a FIFO blocks until a writer arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn item_state_answers_without_opening_the_file() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![1u8; 512]).await;
    let pipe = root_dir.path().join("pipe");
    nix::unistd::mkfifo(&pipe, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

    let answer = tokio::time::timeout(Duration::from_secs(3), service.item_state(&pipe)).await;
    // Unblocks anything that did open it, so the runtime can shut down
    // even when this assertion fails. `O_NONBLOCK` on the write side
    // returns `ENXIO` when there is no reader, which is the passing case.
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&pipe);
    assert_eq!(
        answer.expect("ItemState opened the file and blocked on it"),
        "not-managed"
    );
    // ... and the ordinary answer still works.
    assert_eq!(service.item_state(&root_dir.path().join("f.bin")).await, "online-only");
}

use super::*;

// --- Invariant M1, through the helper's own eyes ---------------------

/// M1: "every directory under a root is marked, and a new directory is
/// marked before anything is created inside it". Both halves are
/// measured from the helper's side — it counts the entries in the very
/// descriptor it was handed — because that is the only place the order
/// is observable. A mark that arrives after the directory has been
/// filled reports a non-zero count; a directory that is never marked
/// reports nothing at all.
#[tokio::test]
async fn every_new_directory_is_marked_before_anything_is_created_in_it() {
    let (service, _sockets, helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source_dir.path().join("sub").join("deeper")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("a.bin"), vec![1u8; 64]).unwrap();
    std::fs::write(
        source_dir.path().join("sub").join("deeper").join("b.bin"),
        vec![2u8; 64],
    )
    .unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    helper.forget();

    service.populate_from_directory(source_dir.path()).await.unwrap();

    let marks: Vec<Seen> = helper
        .seen()
        .into_iter()
        .filter(|s| matches!(s, Seen::MarkDir { .. }))
        .collect();
    assert_eq!(marks.len(), 2, "both new directories must be marked: {marks:?}");
    assert!(
        marks.iter().all(|s| matches!(s, Seen::MarkDir { entries: 0 })),
        "a directory was marked after its contents were created: {marks:?}"
    );
}

/// The crash window the same code left open: a directory that exists but
/// was never marked — `create_dir` succeeded, `mark_dir` did not, the
/// daemon died — is skipped by every later run, so it stays unmarked and
/// everything under it stays uninterceptable. Marking is idempotent;
/// skipping is not recoverable.
#[tokio::test]
async fn a_directory_that_already_exists_is_marked_again() {
    let (service, _sockets, helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("a.bin"), vec![1u8; 64]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    // Exactly what a crash between `create_dir` and `mark_dir` leaves.
    std::fs::create_dir(root_dir.path().join("sub")).unwrap();
    helper.forget();

    service.populate_from_directory(source_dir.path()).await.unwrap();

    assert!(
        helper.seen().iter().any(|s| matches!(s, Seen::MarkDir { .. })),
        "a directory left behind unmarked by a crash was never marked: {:?}",
        helper.seen()
    );
}

/// The item id is the path relative to the source root, which is what
/// the content source resolves a fetch by. A bare file name gives every
/// nested placeholder an id that fetches nothing — and the failure only
/// shows up later, at the one moment the user is waiting for their file.
#[tokio::test]
async fn a_nested_placeholder_carries_an_item_id_that_can_fetch_it() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![8u8; 1024]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();

    let nested = root_dir.path().join("sub").join("b.bin");
    assert_eq!(
        xattr::get(&nested, "user.konedrive.item-id").unwrap().unwrap(),
        b"sub/b.bin",
        "the id must name the file inside the source, not just its last component"
    );
    service.hydrate_now(&nested).await.unwrap();
    assert_eq!(std::fs::read(&nested).unwrap(), vec![8u8; 1024]);
}

// --- Who may register, and what a failed registration leaves ---------

async fn service_with_account(
    account: StateHandle,
) -> (Arc<SyncService>, tempfile::TempDir, FakeHelper) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    (SyncService::new(Some(link), Some(account), None), sockets, helper)
}

/// §3.1 refuses a registration "when nobody is signed in", which nothing
/// checked: both interfaces live on the same object, and this is what
/// wires the one to the other.
#[tokio::test]
async fn register_root_is_refused_while_nobody_is_signed_in() {
    let account = StateHandle::new(crate::account::state::AccountSnapshot::default());
    let (service, _sockets, _helper) = service_with_account(account.clone()).await;
    let root_dir = tempfile::tempdir().unwrap();

    let error = service.register_root(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::NotSignedIn), "{error:?}");
    assert!(
        xattr::get(root_dir.path(), "user.konedrive.root").unwrap().is_none(),
        "a refused registration must not have stamped the folder"
    );

    account.update(|s| s.state = SignInState::SignedIn);
    service.register_root(root_dir.path()).await.unwrap();
}

/// §3.1 refuses a registration that "overlaps another root". A second
/// one used to be accepted and to replace the first silently: the first
/// stayed registered with the helper — still marked, still walked — and
/// `ItemState` started calling its files `not-managed`.
#[tokio::test]
async fn register_root_refuses_a_second_root() {
    let (service, _sockets, helper) = service_with_helper().await;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    service.register_root(first.path()).await.unwrap();
    helper.forget();

    let error = service.register_root(second.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    assert_eq!(
        service.root().unwrap().path,
        std::fs::canonicalize(first.path()).unwrap(),
        "the first root must still be the root"
    );
    assert!(
        helper.seen().is_empty(),
        "the helper was told about a root the daemon refused: {:?}",
        helper.seen()
    );
    assert!(
        xattr::get(second.path(), "user.konedrive.root").unwrap().is_none(),
        "and the refused folder must not have been stamped"
    );
}

/// A `RegisterRoot` that fails must leave nothing behind —
/// no stored root, no published `RootPath` — or, now that a second root
/// is refused, one failed call would make every retry answer "already
/// registered". Measured through the only window a test has: the helper
/// holds its `RegisterRoot` ack open, and the folder's registration is
/// taken away while it does, so the recovery that follows fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_whose_recovery_fails_leaves_no_root_behind() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = FakeHelper::start(socket_path.clone(), Duration::from_millis(400));
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let service = SyncService::new(Some(link), None, None);
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

    let error = registering.await.unwrap().unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_none(), "a failed registration stored a root anyway");
    assert_eq!(service.state().get().root_path, "", "and published it");
    assert_eq!(service.root_state(), "error");
    // The retry a user would make next must not be refused.
    service.register_root(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "ready");
}

// --- Registering without interception ------------------

/// The default stays fail-closed, and for the reason that outranks
/// everything else here: no helper means no interception, and a
/// placeholder nobody intercepts reads as zeros.
#[tokio::test]
async fn register_root_is_refused_without_a_helper() {
    let service = SyncService::new(None, None, None);
    let root_dir = tempfile::tempdir().unwrap();

    let error = service.register_root(root_dir.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    assert_eq!(service.root_state(), "none");
}

/// ...and the whole surface works in the mode that says so out loud.
/// Before this, the helper-optionality already written into
/// `populate_walk`, `unregister_root` and `hydrate_now` was unreachable
/// dead code, because `RegisterRoot` gated all of it.
#[tokio::test]
async fn the_whole_flow_works_without_a_helper_when_it_is_asked_for_explicitly() {
    let service = SyncService::new(None, None, None);
    // No helper anywhere — not only no link — so a free-up goes ahead
    //, whatever this machine has at the real socket path.
    let no_helper = tempfile::tempdir().unwrap();
    service.set_helper_socket(no_helper.path().join("helper.sock"));
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![4u8; 4096]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();

    service.register_root_without_interception(root_dir.path()).await.unwrap();

    assert_eq!(service.root_state(), "no-interception");
    assert!(
        service.last_error().contains("read as zeros"),
        "the one thing a user must not have to infer: {}",
        service.last_error()
    );

    assert_eq!(service.populate_from_directory(source_dir.path()).await.unwrap(), 1);
    let file = root_dir.path().join("sub").join("b.bin");
    assert_eq!(service.item_state(&file).await, "online-only");
    service.hydrate_now(&file).await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), vec![4u8; 4096]);
    assert_eq!(service.item_state(&file).await, "hydrated");
    use std::os::unix::fs::MetadataExt;
    let hydrated = std::fs::metadata(&file).unwrap().blocks();
    service.dehydrate(&file).await.unwrap();
    assert_eq!(service.item_state(&file).await, "online-only");

    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.len(), 4096, "the size survives");
    // Every block of the 4 KiB of content is given back (8 sectors of
    // 512 bytes). Whatever else the file holds — ext4 counts an external
    // xattr block in `st_blocks`, btrfs does not — stays and is not the
    // content.
    assert!(meta.blocks() + 8 <= hydrated, "the content is gone: {hydrated} -> {} blocks", meta.blocks());
}

// --- A folder registered without the helper, and the helper arriving

/// A service that persists into a config file of its own, with no link
/// and no helper at `helper.sock` yet — the machine before the helper is
/// installed — and a folder registered there without interception.
async fn registered_before_the_helper() -> (Arc<SyncService>, PathBuf, PathBuf, [tempfile::TempDir; 3]) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let service = SyncService::new(None, None, Some(persist(&config_file)));
    service.set_helper_socket(&socket_path);
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "no-interception");
    (service, socket_path, config_file, [sockets, config_dir, root_dir])
}

/// Found in real use: a folder registered while the helper was
/// not installed ("Use Without the Helper") stayed that way once it was,
/// and every file in it read as zeros until a Forget and a new
/// registration. The helper connecting switches it: the root is
/// registered with the helper — whose walk marks every directory in it,
/// as at every restart — recovered, and written down as intercepted.
/// What is placed in it afterwards is marked first (invariant M1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_registered_without_the_helper_switches_to_interception_when_the_helper_connects() {
    let (service, socket_path, config_file, dirs) = registered_before_the_helper().await;
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("a/b")).unwrap();
    std::fs::write(source.path().join("a/b/f.bin"), [7u8; 64]).unwrap();
    service.populate_from_directory(source.path()).await.unwrap();

    // The helper is installed and started after the folder was registered.
    let supervisor =
        tokio::spawn(supervise_helper(Arc::clone(&service), socket_path.clone(), Duration::from_millis(10)));
    let helper = FakeHelper::start(socket_path, Duration::ZERO);
    wait_until("the folder switched to interception", || service.root_state() == "ready").await;

    assert_eq!(
        helper.seen().first(),
        Some(&Seen::RegisterRoot),
        "the root must be registered with the helper, whose walk marks every directory: {:?}",
        helper.seen()
    );
    assert_eq!(service.last_error(), "", "the no-interception warning must go");
    let config = Config::load(&config_file).unwrap();
    assert!(config.sync_root_intercepted, "the switch must be written down, or a restart undoes it");
    assert_eq!(config.sync_root, resolved(dirs[2].path()));

    // `a/` and `a/b/` are marked again as they are passed (see
    // `a_directory_that_already_exists_is_marked_again`); `c/` is new.
    helper.forget();
    std::fs::create_dir(source.path().join("c")).unwrap();
    std::fs::write(source.path().join("c/g.bin"), [8u8; 64]).unwrap();
    service.populate_from_directory(source.path()).await.unwrap();
    assert!(
        helper.seen().contains(&Seen::MarkDir { entries: 0 }),
        "a directory placed after the switch must be marked before anything is created in \
         it: {:?}",
        helper.seen()
    );
    supervisor.abort();
}

/// A daemon that has started and not brought its folders up yet (`restore` runs before the
/// bus name is claimed, `resume` after): a folder registered without interception has no
/// registration then, and it is published by its path all the same, so that a client can
/// tell it from an account with no folder. An account with none has no path.
#[tokio::test]
async fn a_folder_not_brought_up_yet_is_published_by_its_path() {
    let (service, _socket_path, config_file, dirs) = registered_before_the_helper().await;
    let folder = dirs[2].path().display().to_string();
    drop(service);

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.restore().await;
    assert!(restarted.root().is_none(), "not brought up yet");
    assert_eq!(restarted.root_state(), "none");
    assert_eq!(restarted.state().get().root_path, folder);

    let other = tempfile::tempdir().unwrap();
    let empty = SyncService::new(None, None, Some(persist(&other.path().join("config.toml"))));
    empty.restore().await;
    assert_eq!(empty.state().get().root_path, "");
}

/// A folder registered without interception because no helper was
/// connected is written down as one to switch, so that a restart before
/// the helper arrives still switches it when the helper does.
#[tokio::test]
async fn a_registration_made_with_no_helper_is_written_down_to_switch_and_switches_after_a_restart() {
    let (service, socket_path, config_file, dirs) = registered_before_the_helper().await;
    assert_eq!(Config::load(&config_file).unwrap().sync_root_upgrade_when_helper, Some(true));
    drop(service);

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.set_helper_socket(&socket_path);
    restarted.restore().await;
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "no-interception");

    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    restarted.set_link(Some(link));
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
    assert_eq!(helper.seen().first(), Some(&Seen::RegisterRoot));
    let config = Config::load(&config_file).unwrap();
    assert!(config.sync_root_intercepted);
    assert_eq!(config.sync_root_upgrade_when_helper, Some(false), "nothing is left to switch");
    assert_eq!(config.sync_root, resolved(dirs[2].path()));
}

/// Ruling 4 of: a `config.toml` written before the flag existed
/// cannot say why its folder is without interception. It is read as a
/// folder to switch — the user's own registration is exactly that case,
/// and must switch once they restart the daemon or the helper reconnects
/// — while an intercepted one has nothing to switch.
#[tokio::test]
async fn a_folder_without_interception_recorded_before_the_flag_existed_switches_when_the_helper_connects() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
    xattr::set(root_dir.path(), "user.konedrive.root", root_id.as_bytes()).unwrap();
    // What the daemon before wrote for such a folder (as migrated).
    write_config(
        &config_file,
        &format!(
            "path = \"{}\"\nid = \"{root_id}\"\nintercepted = false\nsource = \"local\"\nbaloo_excluded = false\n",
            resolved(root_dir.path())
        ),
    );
    assert_eq!(Config::load(&config_file).unwrap().sync_root_upgrade_when_helper, None);

    // The daemon restarts; the helper connects.
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.set_helper_socket(&socket_path);
    restarted.restore().await;
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "no-interception");
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    restarted.set_link(Some(link));
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
    assert_eq!(helper.seen().first(), Some(&Seen::RegisterRoot));
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
}

/// Ruling 2 of: a switch that fails leaves the folder exactly as
/// it was — without interception, written down that way — says why in
/// `LastError`, and is tried again the next time the helper connects.
#[tokio::test]
async fn a_switch_the_helper_refuses_leaves_the_folder_as_it_was_and_is_tried_again_at_the_next_connect() {
    let (service, socket_path, config_file, _dirs) = registered_before_the_helper().await;
    let before = Config::load(&config_file).unwrap();
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    helper.refuse(Seen::RegisterRoot, libc::EIO);

    // What `supervise_helper` does the moment a helper answers.
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(service.root_state(), "no-interception");
    let said = service.last_error();
    assert!(said.starts_with(NO_INTERCEPTION_WARNING), "the warning must stay: {said}");
    assert!(
        said.contains("switching this folder to interception failed") && said.contains("errno 5"),
        "LastError must say why the folder is still without interception: {said}"
    );
    assert_eq!(Config::load(&config_file).unwrap(), before, "config.toml must say what it said before");
    assert!(service.root().is_some());

    // The connection drops (`supervise_helper` lets go of the link), and
    // the helper connects again, and this time accepts.
    service.set_link(None);
    helper.refuse(Seen::RegisterRoot, 0);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    assert_eq!(service.last_error(), "");
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
}

/// A failed switch the helper may still hold — its registration failed,
/// and it could not confirm it let go — is kept intercepted instead
///: a folder the helper may hold must never be one the
/// daemon holds without interception. It is brought up at the next
/// connect, as every intercepted folder is.
#[tokio::test]
async fn a_failed_switch_the_helper_may_still_hold_is_kept_intercepted_and_brought_up_at_the_next_connect() {
    let (service, socket_path, config_file, _dirs) = registered_before_the_helper().await;
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    helper.refuse(Seen::RegisterRoot, libc::EIO);
    helper.refuse(Seen::UnregisterRoot, libc::EIO);

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(service.root_state(), "error");
    assert!(
        service.last_error().contains("could not be told to let go"),
        "{}",
        service.last_error()
    );
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
    // Held as intercepted: its Forget goes through the helper, which is
    // still refusing — a folder without interception would never ask.
    let error = service.unregister_root().await.unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_some());

    service.set_link(None);
    helper.refuse(Seen::RegisterRoot, 0);
    helper.refuse(Seen::UnregisterRoot, 0);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;
    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
}

use super::*;

// --- The mode boundary --------------------

/// H133. An intercepted root may carry ignore marks, and only the
/// helper's `UnregisterRoot` takes them off — its walk clears the mark of
/// every file in the tree. A Forget that could not tell the helper used
/// to be accepted anyway: the daemon forgot the folder while the helper
/// kept it, marks, ignore marks, `roots.json` entry and all, and a later
/// registration of the same folder without interception then punched a
/// file that was still ignored. Measured in the VM suite: a reader got
/// 65536 zero bytes and nothing was fetched.
#[tokio::test]
async fn forgetting_an_intercepted_root_needs_the_helper() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    let link = service.link().unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    helper.forget();
    service.hub().set_link(None);

    let error = service.unregister_root().await.unwrap_err();

    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    assert!(service.root().is_some(), "the refused Forget forgot the folder anyway");
    assert_eq!(
        recorded_root(&config_file),
        resolved(root_dir.path()),
        "and it must still be there at the next start"
    );
    let error =
        service.register_root_without_interception(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");

    // With the helper back, the same Forget goes through — to the helper.
    service.hub().set_link(Some(link));
    service.unregister_root().await.unwrap();
    assert_eq!(helper.seen(), vec![Seen::UnregisterRoot]);
    assert!(service.root().is_none());
    assert_eq!(recorded_root(&config_file), "");
}

/// systemd's answer for a helper that is installed and not running.
struct StoppedUnit;

#[async_trait::async_trait]
impl crate::helper::status::HelperUnit for StoppedUnit {
    async fn states(&self) -> Option<(String, String)> {
        Some(("loaded".into(), "inactive".into()))
    }
}

/// A Forget the helper answers `EPERM` has nothing left to undo: the
/// helper holds no root of this uid under that id — it lost it, or never
/// kept it — so no mark of that registration can be left, and keeping
/// the folder would only make it impossible to forget. Any other refusal
/// still keeps it, because then the helper may well hold it.
#[tokio::test]
async fn a_forget_the_helper_answers_eperm_goes_through_and_any_other_refusal_does_not() {
    let (service, helper, _config_file, _sockets, _config_dir) =
        service_with_config().await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    helper.refuse(Seen::UnregisterRoot, libc::EIO);
    let error = service.unregister_root().await.unwrap_err();
    assert!(matches!(error, SyncError::Helper(_)), "{error:?}");
    assert!(service.root().is_some(), "a helper that may still hold it was ignored");

    helper.refuse(Seen::UnregisterRoot, libc::EPERM);
    service.unregister_root().await.unwrap();
    assert!(service.root().is_none());
}

/// H134. A root registered without interception was never announced to
/// the helper, so forgetting it has nothing to tell the helper — and
/// telling it anyway made it impossible to forget while a helper was
/// connected: the helper refuses to unregister a root the uid does not
/// hold (`EPERM`), and the daemon kept the registration. Measured in
/// small round 3, and again in the VM suite.
#[tokio::test]
async fn forgetting_a_root_registered_without_interception_never_asks_the_helper() {
    let (service, _sockets, helper) = service_with_helper().await;
    helper.refuse(Seen::UnregisterRoot, libc::EIO);
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();

    service.unregister_root().await.unwrap();

    assert!(service.root().is_none());
    assert_eq!(service.root_state(), "none");
    assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());
}

/// H135, first half. `PopulateFromDirectory` used to mark every
/// directory it created whenever a link merely existed — in a root
/// registered without interception too. On a filesystem where the uid
/// owns no helper root the helper refuses that `EPERM`, and the populate
/// failed; where it owns one, the mark landed, the directory was
/// intercepted, and an intercepted hydration ignore-marks the file —
/// which is exactly what H135's skipped `ClearIgnore` relies on never
/// happening. Both measured in the VM suite.
#[tokio::test]
async fn populating_a_root_registered_without_interception_marks_nothing() {
    let (service, _sockets, helper) = service_with_helper().await;
    helper.refuse(Seen::MarkDir { entries: 0 }, libc::EPERM);
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![4u8; 4096]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();

    assert_eq!(service.populate_from_directory(source_dir.path()).await.unwrap(), 1);

    assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());
}

/// A root registered without interception, populated with one file
/// `b.bin` and filled, ready to be freed up.
async fn filled_without_interception(service: &SyncService) -> (tempfile::TempDir, PathBuf) {
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("b.bin"), vec![4u8; 64 * 1024]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    let file = root_dir.path().join("b.bin");
    service.hydrate_now(&file).await.unwrap();
    // The source goes; the file is what it holds now.
    std::mem::forget(source_dir);
    (root_dir, file)
}

/// The daemon's local rule at a dehydration in a root registered
/// without interception, with a link: the helper is asked to clear the
/// file's ignore mark, as in any other root, and the punch follows. It
/// used to be skipped, on the strength of a chain of reasoning — nothing
/// in such a folder is intercepted, and interception resumes only through
/// a walk that clears every mark — but that assumption failed again: a
/// stale-marked file emptied here read zeros once the
/// folder was intercepted again. The helper grants the clear on ownership
/// alone now, so the `EPERM` that made this mode skip it is gone too.
#[tokio::test]
async fn a_dehydration_without_interception_clears_the_mark_through_its_link() {
    let (service, _sockets, helper) = service_with_helper().await;
    let (_root, file) = filled_without_interception(&service).await;

    service.dehydrate(&file).await.unwrap();

    assert_eq!(service.item_state(&file).await, "online-only");
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark must be cleared first");
}

/// And stopped by a clear that fails, as §8 step 2 has it everywhere
/// else: the file is left hydrated, content and all.
#[tokio::test]
async fn a_dehydration_without_interception_whose_mark_is_not_cleared_changes_nothing() {
    let (service, _sockets, helper) = service_with_helper().await;
    let (_root, file) = filled_without_interception(&service).await;
    helper.refuse(Seen::ClearIgnore, libc::EIO);

    let refused = service.dehydrate(&file).await;

    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(service.item_state(&file).await, "hydrated");
    assert!(data_blocks(&file) > 64, "a file whose mark was not cleared was emptied");
}

/// with no link. A helper running with no link to this
/// daemon — at startup before the first connection, or between a
/// helper's restart and the reconnect — has a group that may hold a mark
/// on the file, and nothing here can clear it: refused `NoHelper`, the
/// file untouched. With no helper bound to the socket at all — the file
/// a helper that exited left behind — no group of ours exists, and the
/// punch goes ahead.
#[tokio::test]
async fn with_no_link_a_running_helper_stops_a_dehydration_and_an_exited_one_does_not() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let service = testing::service(None, None, None);
    service.hub().set_socket(&socket_path);
    let (_root, file) = filled_without_interception(&service).await;

    let running = FakeHelper::start(socket_path.clone());
    let refused = service.dehydrate(&file).await;
    assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
    assert_eq!(service.item_state(&file).await, "hydrated");
    assert!(data_blocks(&file) > 64, "emptied while a helper ran with no link to it");
    assert!(running.seen().is_empty(), "the helper was connected to: {:?}", running.seen());

    // The helper exits: its socket file stays, with nothing bound to it.
    let stale = sockets.path().join("stale.sock");
    drop(socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .and_then(|fd| {
            bind(fd.as_raw_fd(), &UnixAddr::new(&stale).unwrap())?;
            Ok(fd)
        })
        .unwrap());
    assert!(stale.exists());
    service.hub().set_socket(&stale);
    service.dehydrate(&file).await.unwrap();
    assert_eq!(service.item_state(&file).await, "online-only");
}

/// at the other punch site: recovery of a root registered
/// without interception clears an interrupted file's mark through its
/// link, like any other recovery.
#[tokio::test]
async fn recovery_without_interception_clears_the_mark_through_its_link() {
    let (service, _sockets, helper) = service_with_helper().await;
    let (root_dir, stuck) = root_with_a_stuck_file();

    service.register_root_without_interception(root_dir.path()).await.unwrap();

    assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "recovery did not run");
    assert_eq!(service.root_state(), "no-interception");
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark must be cleared first");
}

/// A folder with the root id already on it and one file a crash left
/// `dehydrating`.
fn root_with_a_stuck_file() -> (tempfile::TempDir, PathBuf) {
    let root_dir = tempfile::tempdir().unwrap();
    xattr::set(
        root_dir.path(),
        "user.konedrive.root",
        b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d",
    )
    .unwrap();
    let stuck = root_dir.path().join("stuck.bin");
    std::fs::write(&stuck, vec![1u8; 64 * 1024]).unwrap();
    {
        let file = std::fs::File::options().read(true).write(true).open(&stuck).unwrap();
        konedrive_fs::placeholder::write_state(&file, State::Dehydrating).unwrap();
    }
    (root_dir, stuck)
}

/// With no link while a helper runs, recovery of such a root leaves the
/// interrupted file exactly as found — deferred, not failed — and runs
/// again, clearing the mark, once the link is up. Without
/// the second run the file would stay `dehydrating` until the next start.
///
/// Here the folder is one registered without interception on purpose
/// (`config.toml` says so), restored at a start that finds a helper
/// running and no link to it yet.
#[tokio::test]
async fn recovery_deferred_while_an_unlinked_helper_runs_finishes_once_linked() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let (root_dir, stuck) = root_with_a_stuck_file();
    write_config(
        &config_file,
        &format!("path = \"{}\"\nintercepted = false\nupgrade_when_helper = false\n", resolved(root_dir.path())),
    );
    let service = testing::service(None, None, Some(persist(&config_file)));
    service.hub().set_socket(&socket_path);

    service.resume().await;

    assert_eq!(state_of_path(&stuck), Some(State::Dehydrating), "reset with a mark unclearable");
    assert!(data_blocks(&stuck) > 64);
    assert_eq!(service.root_state(), "no-interception", "deferred is not an error");
    assert!(service.last_error().contains("not connected to it yet"), "{}", service.last_error());

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.hub().set_link(Some(link));
    service.resume().await;

    assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "the deferred reset never ran");
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore]);
    assert_eq!(service.last_error(), NO_INTERCEPTION_WARNING);
}

/// The same deferred file in a folder registered without interception
/// because no helper was connected: the link's arrival switches the
/// folder to interception, and the switch's own recovery —
/// with the link, after the helper registered the root — resets it.
#[tokio::test]
async fn a_switch_to_interception_resets_what_recovery_deferred() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let service = testing::service(None, None, None);
    service.hub().set_socket(&socket_path);
    let (root_dir, stuck) = root_with_a_stuck_file();

    service.register_root_without_interception(root_dir.path()).await.unwrap();
    assert_eq!(state_of_path(&stuck), Some(State::Dehydrating), "reset with a mark unclearable");

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.hub().set_link(Some(link));
    service.resume().await;

    assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "the deferred reset never ran");
    assert_eq!(helper.seen(), vec![Seen::RegisterRoot, Seen::ClearIgnore]);
    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    assert_eq!(service.last_error(), "");
}

/// The route into no-interception mode that H133 alone leaves open. A
/// root restored from `config.toml` used to exist nowhere in the daemon
/// until the helper came back — `resume` returned early — so
/// `RegisterWithoutInterception` of the very folder the helper still
/// held (marks, ignore marks and all) was accepted, and the next
/// dehydration there punched files that were still ignored. Measured in
/// the VM suite: 65536 zero bytes. The root is held as registered now,
/// and everything that has to go through the helper waits for it.
#[tokio::test]
async fn an_intercepted_root_restored_before_its_helper_is_back_is_held() {
    let (first, helper, config_file, sockets, _config_dir) =
        service_with_config().await;
    let root_dir = tempfile::tempdir().unwrap();
    first.register_root(root_dir.path()).await.unwrap();
    let root_id = first.root().unwrap().root_id;
    drop(first);
    helper.forget();

    let restarted = testing::service(None, None, Some(persist(&config_file)));
    restarted.resume().await;

    let held = restarted.root().expect("a restored root must be held before the helper");
    assert_eq!(held.path.display().to_string(), resolved(root_dir.path()));
    assert_eq!(held.root_id, root_id, "under the id the helper holds it by");
    // Nothing is known to be wrong yet: it waits, calmly.
    assert_eq!((restarted.root_state().as_str(), restarted.last_error().as_str()), ("waiting", ""));
    // Once the helper is known not to run, that is an error, and it says what to do.
    restarted.hub().set_unit(Arc::new(StoppedUnit));
    restarted.hub().check().await;
    assert_eq!(restarted.root_state(), "error");
    assert!(
        restarted.last_error().contains("not running"),
        "the published error must say what is missing: {}",
        restarted.last_error()
    );
    let error =
        restarted.register_root_without_interception(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    let elsewhere = tempfile::tempdir().unwrap();
    let error =
        restarted.register_root_without_interception(elsewhere.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    let error = restarted.unregister_root().await.unwrap_err();
    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    let config = Config::load(&config_file).unwrap();
    assert_eq!(config.sync_root, resolved(root_dir.path()));
    assert!(config.sync_root_intercepted);
    assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());

    // The helper comes back: the same root is brought up, not a new one.
    let (link, _requests) =
        HelperLink::connect(&sockets.path().join("helper.sock")).await.unwrap();
    restarted.hub().set_link(Some(link));
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "ready");
    assert_eq!(helper.seen(), vec![Seen::RegisterRoot]);
}

/// ...and it stays held when bringing it up fails. A failed startup
/// `bind` used to leave the daemon holding no root while the helper still
/// held the folder, which is the same open door. Forgetting it still
/// works, through the helper, under the id `config.toml` recorded — the
/// folder is gone, so nothing could be read from it.
#[tokio::test]
async fn a_restored_root_that_cannot_be_brought_up_is_still_held() {
    let (first, helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    let root_dir = tempfile::tempdir().unwrap();
    first.register_root(root_dir.path()).await.unwrap();
    let root_path = std::fs::canonicalize(root_dir.path()).unwrap();
    let link = first.link().unwrap();
    drop(first);
    drop(root_dir);
    helper.forget();

    let restarted = testing::service(Some(link), None, Some(persist(&config_file)));
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "error");
    assert_eq!(restarted.root().map(|r| r.path), Some(root_path.clone()));
    let elsewhere = tempfile::tempdir().unwrap();
    let error =
        restarted.register_root_without_interception(elsewhere.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");

    restarted.unregister_root().await.unwrap();
    assert_eq!(helper.seen(), vec![Seen::UnregisterRoot]);
    assert_eq!(recorded_root(&config_file), "");
}

/// A config written before the root id was recorded still restores its
/// root as held: the id is read from the folder instead.
#[tokio::test]
async fn a_restored_root_with_no_recorded_id_takes_it_from_the_folder() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
    xattr::set(root_dir.path(), "user.konedrive.root", root_id.as_bytes()).unwrap();
    write_config(&config_file, &format!("path = \"{}\"\n", resolved(root_dir.path())));

    let restarted = testing::service(None, None, Some(persist(&config_file)));
    restarted.resume().await;

    assert_eq!(restarted.root().map(|r| r.root_id), Some(root_id.to_owned()));
    assert_eq!(restarted.root_state(), "waiting");
}

/// The id the helper holds a root by is what a restored root has to be
/// forgotten by, so it is written down with the root, and removed with it.
#[tokio::test]
async fn the_root_id_is_recorded_with_the_root() {
    let (service, _helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    let config = Config::load(&config_file).unwrap();
    assert_eq!(config.sync_root_id, service.root().unwrap().root_id);

    service.unregister_root().await.unwrap();
    assert_eq!(Config::load(&config_file).unwrap().sync_root_id, "");
}

/// A root the helper holds and `config.toml` does not name is one the
/// daemon cannot see after a restart, and so one it would accept for
/// registration without interception. `RegisterRoot` used to write the
/// root down only after the helper had saved it *and* recovery had
/// walked the whole tree, so a crash anywhere in between left exactly
/// that. It is written down first now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_root_is_written_down_before_the_helper_hears_of_it() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    helper.hold(Seen::RegisterRoot);
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;

    assert_eq!(
        recorded_root(&config_file),
        resolved(root_dir.path()),
        "the helper was told about a root config.toml does not name"
    );
    helper.release(Seen::RegisterRoot);
    registering.await.unwrap().unwrap();
}

/// And a root that cannot be written down is not registered at all: the
/// helper is never told about it.
///. `config.toml` is the account
/// sub-project's file too — it holds the `client_id` — and a copy that
/// could not be read used to be treated as empty and written back from
/// defaults, erasing everything in it. What could not be read is never
/// overwritten: an intercepted registration is refused (its record must
/// exist before the helper is told), and a registration without
/// interception stands but is not recorded.
#[tokio::test]
async fn an_unreadable_config_is_never_overwritten() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    let unreadable = "client_id = \"the account's own\"\nthis is not [toml\n";
    std::fs::write(&config_file, unreadable).unwrap();
    let root_dir = tempfile::tempdir().unwrap();

    let refused = service.register_root(root_dir.path()).await;
    assert!(matches!(refused, Err(SyncError::Config(_))), "{refused:?}");
    assert!(helper.seen().is_empty(), "the helper was told: {:?}", helper.seen());
    assert_eq!(std::fs::read_to_string(&config_file).unwrap(), unreadable);

    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.unregister_root().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&config_file).unwrap(),
        unreadable,
        "config.toml was rewritten from defaults"
    );
}

#[tokio::test]
async fn a_root_that_cannot_be_written_down_is_not_registered() {
    let (service, _sockets, helper) = service_with_helper().await;
    let config_dir = tempfile::tempdir().unwrap();
    let blocker = config_dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"").unwrap();
    let service = testing::service(service.link(), None, Some(persist(&blocker.join("config.toml"))));
    let root_dir = tempfile::tempdir().unwrap();

    let error = service.register_root(root_dir.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::Config(_)), "{error:?}");
    assert!(service.root().is_none());
    assert!(helper.seen().is_empty(), "the helper was told: {:?}", helper.seen());
}

/// on both sides: a `RegisterRoot` that fails after the
/// helper saved the root is undone at the helper, and in `config.toml`,
/// so that neither is left holding a root the daemon does not. Nothing of it
/// is published either, and the retry a user would make next is taken. The
/// folder's root id is taken away while the helper has not answered yet, so
/// the recovery that follows fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_registration_is_undone_at_the_helper_and_in_the_config() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    helper.hold(Seen::RegisterRoot);
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();
    helper.release(Seen::RegisterRoot);

    let error = registering.await.unwrap().unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_none(), "a failed registration stored a root anyway");
    assert_eq!(helper.seen(), vec![Seen::RegisterRoot, Seen::UnregisterRoot]);
    assert_eq!(recorded_root(&config_file), "");
    assert_eq!((service.state().get().root_path.as_str(), service.root_state().as_str(), service.last_error().as_str()), ("", "none", ""), "nor published");
    service.register_root(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "ready");
}

/// ...unless the helper cannot confirm it let go. Then the root is kept,
/// intercepted, so it can only leave the way H133 allows — through the
/// helper — and not by a registration without interception on top of
/// a folder the helper may still be marking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_registration_the_helper_may_still_hold_is_kept() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    helper.hold(Seen::RegisterRoot);
    helper.refuse(Seen::UnregisterRoot, libc::EIO);
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();
    helper.release(Seen::RegisterRoot);

    // The refusal says the folder is kept, and what to do about it.
    let error = registering.await.unwrap().unwrap_err();
    assert!(
        matches!(&error, SyncError::Helper(why) if why.contains("brought up the next time the helper connects") && why.contains("forget it")),
        "{error:?}"
    );
    assert!(service.root().is_some(), "a root the helper may hold was let go");
    assert_eq!(service.root_state(), "error");
    assert_eq!(recorded_root(&config_file), resolved(root_dir.path()));
    let error =
        service.register_root_without_interception(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    assert!(service.last_error().contains("forget it if you do not want it"), "{}", service.last_error());

    // The next connect brings it up, as the refusal said.
    helper.refuse(Seen::UnregisterRoot, 0);
    service.resume().await;
    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
}

/// F241: an intercepted folder whose root id is recorded nowhere cannot be named to the
/// helper. It is held, and says why; a Forget takes the daemon's record of it away with no
/// helper, and touches nothing in the folder.
#[tokio::test]
async fn an_intercepted_folder_nobody_can_name_is_forgotten_on_the_daemons_side() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    std::fs::write(root_dir.path().join("mine.txt"), b"x").unwrap();
    write_config(&config_file, &format!("path = \"{}\"\n", resolved(root_dir.path())));

    let restarted = testing::service(None, None, Some(persist(&config_file)));
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "error");
    assert!(restarted.last_error().contains("does not record its root id"), "{}", restarted.last_error());
    let elsewhere = tempfile::tempdir().unwrap();
    let refused = restarted.register_root_without_interception(elsewhere.path()).await;
    assert!(matches!(refused, Err(SyncError::AlreadyRegistered)), "{refused:?}");

    restarted.unregister_root().await.unwrap();
    assert_eq!((restarted.root_state().as_str(), restarted.last_error().as_str()), ("none", ""));
    assert_eq!(recorded_root(&config_file), "");
    assert_eq!(std::fs::read(root_dir.path().join("mine.txt")).unwrap(), b"x");
    restarted.register_root_without_interception(elsewhere.path()).await.unwrap();
}

/// The deterministic form of a D-Bus-activated first call: the bus name
/// is claimed before `resume` runs, so a `RegisterWithoutInterception`
/// can reach a restarted daemon before anything has looked at
/// `config.toml`. It must find the restored root all the same.
#[tokio::test]
async fn a_registration_that_arrives_before_resume_still_finds_the_restored_root() {
    let (first, helper, config_file, _sockets, _config_dir) =
        service_with_config().await;
    let root_dir = tempfile::tempdir().unwrap();
    first.register_root(root_dir.path()).await.unwrap();
    drop(first);
    helper.forget();

    let restarted = testing::service(None, None, Some(persist(&config_file)));
    let error =
        restarted.register_root_without_interception(root_dir.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
}

/// zbus runs every method call in a task of its own, so two
/// registrations can be in flight at once. Both used to pass the "no
/// root yet" check before either had committed, both reached the
/// helper, and the last commit won: the helper then held a root the
/// daemon did not — the state a later registration without interception
/// of that folder turns into zeros. Registrations and Forgets now take
/// turns, and the second one sees the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_registrations_at_once_leave_one_root_at_the_helper() {
    let (service, helper, _config_file, _sockets, _config_dir) =
        service_with_config().await;
    // The helper answers the first only once both are under way.
    helper.hold(Seen::RegisterRoot);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    let both = {
        let (service, first, second) = (Arc::clone(&service), first.path().to_path_buf(), second.path().to_path_buf());
        tokio::spawn(async move { tokio::join!(service.register_root(&first), service.register_root(&second)) })
    };
    wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;
    helper.release(Seen::RegisterRoot);
    let (a, b) = both.await.unwrap();

    assert!(
        a.is_ok() != b.is_ok(),
        "exactly one of two concurrent registrations may succeed: {a:?}, {b:?}"
    );
    let refused = a.err().or(b.err()).unwrap();
    assert!(matches!(refused, SyncError::AlreadyRegistered), "{refused:?}");
    assert_eq!(
        helper.seen(),
        vec![Seen::RegisterRoot],
        "the helper was told about a root the daemon does not hold"
    );
}

/// A dehydration decides whether to send `ClearIgnore` from the root's
/// mode, and then may wait — for a fill of the same inode — before it
/// punches. The mode must not change under it in the meantime: a root
/// forgotten and registered again with interception while it waits
/// could have the file ignore-marked by then, and the punch would skip
/// the `ClearIgnore` that is suddenly needed.
/// it waits for the fill without the lifecycle lock — a Forget is not
/// held up by a download — and decides the mode only after, under the
/// lock: a folder forgotten meanwhile is refused, nothing punched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dehydration_waiting_for_a_fill_does_not_hold_up_a_forget() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("b.bin"), vec![4u8; 4096]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    let file = root_dir.path().join("b.bin");
    service.hydrate_now(&file).await.unwrap();

    // A fill of the same inode, in progress.
    let fill = service.locks().lock(key_of(&file)).await;
    let dehydrating = {
        let service = Arc::clone(&service);
        let file = file.clone();
        tokio::spawn(async move { service.dehydrate(&file).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(Duration::from_secs(2), service.unregister_root())
        .await
        .expect("the Forget waited for a dehydration waiting for a fill")
        .unwrap();

    drop(fill);
    let refused = dehydrating.await.unwrap();
    assert!(matches!(refused, Err(SyncError::NoRoot)), "{refused:?}");
    assert_eq!(std::fs::read(&file).unwrap(), vec![4u8; 4096], "nothing was punched");
}

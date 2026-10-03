//! End-to-end tests for the interfaces of an account's folder (`org.konedrive.Folder`,
//! `Transfers`, `UploadQueue`, `Conflicts`, `LocalScan`, `ActivityLog`), over a private
//! test bus, the way `tests/dbus_api.rs` exercises `org.konedrive.Account`: one account's
//! folder at `/org/konedrive/Accounts/<id>`, and the per-file calls through
//! `org.konedrive.Files`, routed by path. No fanotify is involved: a fake
//! helper thread speaks the wire protocol and acknowledges everything, but
//! never actually marks anything or sends a `HydrateRequest` — see
//! `konedrived::sync::SyncService`'s own doc comment for why `Hydrate()` does
//! not depend on that to fill a file.
//!
//! The daemon is started here exactly as `main.rs` starts it
//! (`daemon::startup::start`), so every account's objects are on the bus before the
//! name is claimed, and the helper is reached through the hub's supervisor
//! rather than inline.

mod common;

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{introspect, signature_lines, start_daemon};
use futures_util::StreamExt;
#[cfg(feature = "dev-tools")]
use konedrive_dbus::accounts::TokenExportProxy;
use konedrive_dbus::accounts::{
    AccountsProxy, ActivityLogProxy, ConflictsProxy, FilesProxy, FolderProxies, FolderProxy, LocalScanProxy,
};
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::{
    error_name, ACCOUNTS_INTERFACE_NAME, ACCOUNTS_PATH, ACTIVITY_LOG_INTERFACE_NAME, CONFLICTS_INTERFACE_NAME,
    FOLDER_INTERFACE_NAME, LOCAL_SCAN_INTERFACE_NAME, SERVICE_NAME, TOKEN_EXPORT_INTERFACE_NAME, TRANSFERS_INTERFACE_NAME,
    UPLOAD_QUEUE_INTERFACE_NAME,
};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use konedrive_graph::oauth::Endpoints;
use konedrived::account::secret::MemoryWallet;
use konedrived::account::state::{SignInState, StateHandle};
use konedrived::helper::HelperLink;
use konedrived::sync::SyncService;
use konedrived::status::snapshot::SyncTrouble;
use nix::sys::socket::{
    accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};
use zbus::zvariant::OwnedObjectPath;

/// Each interface of the account's object but `Account`'s (`tests/dbus_api.rs`) and
/// `TokenExport`'s, with its checked-in definition.
const XML: [(&str, &str); 6] = [
    (FOLDER_INTERFACE_NAME, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Folder.xml"))),
    (TRANSFERS_INTERFACE_NAME, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Transfers.xml"))),
    (UPLOAD_QUEUE_INTERFACE_NAME, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.UploadQueue.xml"))),
    (CONFLICTS_INTERFACE_NAME, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Conflicts.xml"))),
    (LOCAL_SCAN_INTERFACE_NAME, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.LocalScan.xml"))),
    (ACTIVITY_LOG_INTERFACE_NAME, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.ActivityLog.xml"))),
];

/// Served only by a development build (the `dev-tools` feature).
#[cfg(feature = "dev-tools")]
const TOKEN_EXPORT_XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.TokenExport.xml"));

struct Setup {
    /// The account's folder.
    folder: FolderProxy<'static>,
    conflicts: ConflictsProxy<'static>,
    scan: LocalScanProxy<'static>,
    activity: ActivityLogProxy<'static>,
    /// The per-file calls.
    files: FilesProxy<'static>,
    manager: AccountsProxy<'static>,
    /// The account's object.
    path: OwnedObjectPath,
    client: zbus::Connection,
    dir: tempfile::TempDir,
    account: StateHandle,
    /// The daemon's own half, for what no method can reach: the state a sync
    /// with OneDrive publishes.
    sync: Arc<SyncService>,
    _daemon: konedrived::daemon::startup::Daemon,
    _config: tempfile::TempDir,
    _helper_dir: tempfile::TempDir,
    _bus: TestBus,
}

/// Accepts one connection on a `SOCK_SEQPACKET` socket at `path`, greets,
/// acknowledges `Hello`, and after that acknowledges every request with
/// `Ack { errno: 0 }` — never sending a `HydrateRequest` of its own, since no
/// fanotify group backs any of this. Built the same way
/// `helper`'s own test module builds its fake helpers.
fn fake_helper(path: PathBuf) {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    let addr = UnixAddr::new(&path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    std::thread::spawn(move || {
        let accepted = accept(fd.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this process now solely owns.
        let stream = unsafe { UnixStream::from_raw_fd(accepted) };
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
            if matches!(message, ToHelper::HydrateDone { .. }) {
                // Nothing in these tests ever sends a HydrateRequest, so
                // this daemon should never send this either; keep answering
                // anyway rather than assert, so a stray one does not hang
                // the connection.
            }
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });
    // Give the accept() loop a moment to be listening; `bind`+`listen` above
    // already happened synchronously on the caller's thread, so `connect`
    // below cannot race the socket's existence, only the `accept()` call,
    // which the kernel's own listen backlog queues for.
    let _ = &path;
}

/// A helper that accepts the connection and then says nothing at all — the
/// shape that makes `HelperLink::connect` take its full 30 s call timeout
///, and therefore the shape that exposes anything the daemon
/// does *after* connecting but before it is ready to answer.
fn silent_helper(path: PathBuf) {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    let addr = UnixAddr::new(&path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    std::thread::spawn(move || {
        let held: Vec<_> = std::iter::from_fn(|| accept(fd.as_raw_fd()).ok()).take(4).collect();
        // Hold the accepted descriptors open, saying nothing, until the test
        // is over.
        std::thread::sleep(Duration::from_secs(60));
        drop(held);
    });
}

async fn setup() -> Setup {
    setup_with_helper(true).await
}

/// The daemon with one account, `Personal`, signed in; the fake helper
/// connected when `with_helper` says so.
async fn setup_with_helper(with_helper: bool) -> Setup {
    let bus = TestBus::start();
    let config = tempfile::tempdir().unwrap();
    let daemon = start_daemon(
        &bus,
        config.path(),
        Endpoints::microsoft(),
        Arc::new(MemoryWallet::default()),
        Duration::from_secs(5),
    )
    .await;
    let added = daemon.manager.add("Personal", &daemon.connection).await.unwrap();

    let helper_dir = tempfile::tempdir().unwrap();
    let socket_path = helper_dir.path().join("helper.sock");
    let hub = daemon.manager.hub();
    // Without a helper, nothing is bound at this path: a punch with no link
    // goes ahead whatever this machine runs at the real one.
    hub.set_socket(&socket_path);
    if with_helper {
        fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        hub.set_link(Some(link));
    }
    // §3.1 refuses a registration when nobody is signed in, so the tests
    // that register a folder start from a signed-in daemon. The one that
    // measures the refusal signs out again.
    added.account.state().update(|s| s.state = SignInState::SignedIn);

    let client = bus.connect().await;
    // Property reads go to the daemon every time. A caching proxy — which is
    // what `konedrivectl` uses — would answer some of these assertions from
    // the value it cached before the change, which is a property of the
    // proxy, not of the daemon this file is about.
    let FolderProxies { folder, conflicts, scan, activity, .. } =
        FolderProxies::uncached(&client, added.path.clone()).await.unwrap();
    let files = FilesProxy::new(&client).await.unwrap();
    let manager = AccountsProxy::builder(&client)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    Setup {
        folder,
        conflicts,
        scan,
        activity,
        files,
        manager,
        path: added.path.clone(),
        client,
        dir,
        account: added.account.state().clone(),
        sync: Arc::clone(&added.sync),
        _daemon: daemon,
        _config: config,
        _helper_dir: helper_dir,
        _bus: bus,
    }
}

/// The D-Bus error name a refused call came back with — never its message.
/// Matching on prose is exactly what a named error exists to replace.
#[track_caller]
fn refusal<T: std::fmt::Debug>(result: zbus::Result<T>) -> String {
    let error = result.expect_err("this call was supposed to be refused");
    error_name(&error)
        .unwrap_or_else(|| panic!("not a D-Bus method error: {error}"))
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registering_an_empty_folder_makes_it_the_root() {
    let f = setup().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    f.folder.register(root.to_str().unwrap()).await.unwrap();

    assert_eq!(f.folder.path().await.unwrap(), root.to_str().unwrap());
    assert_eq!(f.folder.state().await.unwrap(), "ready");
    assert!(
        xattr::get(&root, "user.konedrive.root").unwrap().is_some(),
        "the root must be stamped with its id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_empty_folder_is_refused_with_a_useful_error() {
    let f = setup().await;
    let root = f.dir.path().join("NotEmpty");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("stray.txt"), b"x").unwrap();

    let name = refusal(f.folder.register(root.to_str().unwrap()).await);
    assert_eq!(name, "org.konedrive.Error.NotEmpty");
    assert_eq!(f.folder.state().await.unwrap(), "none");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn populate_from_directory_mirrors_the_tree_as_placeholders() {
    let f = setup().await;
    let source = f.dir.path().join("source");
    std::fs::create_dir_all(source.join("sub")).unwrap();
    std::fs::write(source.join("a.bin"), vec![1u8; 4096]).unwrap();
    std::fs::write(source.join("sub/b.bin"), vec![2u8; 100]).unwrap();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.folder.register(root.to_str().unwrap()).await.unwrap();

    let created = f.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    assert_eq!(created, 2);

    use std::os::unix::fs::MetadataExt;
    let placeholder = root.join("a.bin");
    let meta = std::fs::metadata(&placeholder).unwrap();
    assert_eq!(meta.len(), 4096, "the placeholder reports the real size");
    assert!(meta.blocks() < 8, "and takes no space");
    assert_eq!(f.files.item_state(placeholder.to_str().unwrap()).await.unwrap(), "online-only");
    assert_eq!(
        f.files.item_state(root.join("sub/b.bin").to_str().unwrap()).await.unwrap(),
        "online-only"
    );
    assert_eq!(
        f.files.item_state(source.join("a.bin").to_str().unwrap()).await.unwrap(),
        "not-managed",
        "a file outside the root is not ours"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrate_then_dehydrate_round_trips_one_file() {
    let f = setup().await;
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![7u8; 8192]).unwrap();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.folder.register(root.to_str().unwrap()).await.unwrap();
    f.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");

    f.files.hydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(f.files.item_state(file.to_str().unwrap()).await.unwrap(), "hydrated");
    assert_eq!(std::fs::read(&file).unwrap(), vec![7u8; 8192]);

    f.files.dehydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(f.files.item_state(file.to_str().unwrap()).await.unwrap(), "online-only");
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.len(), 8192, "the size survives");
    assert!(meta.blocks() < 8, "the content is gone");
}

/// Over the bus: a download is announced as it happens
/// (`ActivityLog.Added`), kept (`ActivityLog.Recent`), and `FreeUpSpace` answers
/// (files, bytes, busy) in three out arguments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_is_announced_kept_and_freed_up_again() {
    let f = setup().await;
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![7u8; 8192]).unwrap();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.folder.register(root.to_str().unwrap()).await.unwrap();
    f.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");
    let shown = file.to_str().unwrap();
    let mut added = f.activity.receive_added().await.unwrap();

    f.files.hydrate(shown).await.unwrap();

    let signal = tokio::time::timeout(Duration::from_secs(5), added.next())
        .await
        .expect("no ActivityLog.Added for the download")
        .unwrap();
    let args = signal.args().unwrap();
    assert_eq!((args.kind().as_str(), args.path().as_str(), args.detail().as_str()), ("downloaded", shown, "8.0 KiB"));
    let recent = f.activity.recent(10).await.unwrap();
    assert_eq!(recent.len(), 1, "{recent:?}");
    assert_eq!((recent[0].1.as_str(), recent[0].2.as_str()), ("downloaded", shown));
    assert_eq!(recent[0].0, *args.time());

    let (files, bytes, busy) = f.folder.free_up_space().await.unwrap();
    assert_eq!((files, busy), (1, 0));
    assert!(bytes >= 8192, "{bytes}");
    assert_eq!(f.files.item_state(shown).await.unwrap(), "online-only");
}

/// Dismissing a conflict that is not there is an error, and the
/// error names the path it was asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismissing_a_conflict_that_is_not_there_names_the_path() {
    let f = setup().await;
    let error = f.conflicts.dismiss("/nowhere/rescued.txt").await.unwrap_err();
    assert_eq!(error_name(&error), Some("org.konedrive.Error.NoConflict"));
    assert!(error.to_string().contains("/nowhere/rescued.txt"), "{error}");
    assert!(f.conflicts.list().await.unwrap().is_empty());
    assert_eq!(f.conflicts.count().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn introspection_matches_the_checked_in_xml() {
    let f = setup().await;
    let live = introspect(&f.client, f.path.as_str()).await;
    for (interface, xml) in XML {
        assert_eq!(signature_lines(&live, interface), signature_lines(xml, interface), "{interface}");
    }
    #[cfg(feature = "dev-tools")]
    assert_eq!(signature_lines(&live, TOKEN_EXPORT_INTERFACE_NAME), signature_lines(TOKEN_EXPORT_XML, TOKEN_EXPORT_INTERFACE_NAME));
    // A release build hands out no token at all (issue #79).
    #[cfg(not(feature = "dev-tools"))]
    assert!(!live.contains(TOKEN_EXPORT_INTERFACE_NAME), "{live}");
}

#[cfg(feature = "dev-tools")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_to_export_while_signed_out() {
    let f = setup().await;
    let export = TokenExportProxy::new(&f.client, f.path.clone()).await.unwrap();
    let err = export.read_only().await.unwrap_err();
    assert_eq!(error_name(&err), Some("org.konedrive.Error.NotSignedIn"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_folder_has_no_counters_and_refuses_refresh() {
    let f = setup().await;
    let root = f.dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    f.folder.register(root.to_str().unwrap()).await.unwrap();
    assert_eq!(f.folder.source().await.unwrap(), "local");
    assert_eq!((f.folder.items_listed().await.unwrap(), f.folder.skipped_count().await.unwrap()), (0, 0));
    assert!(f.folder.skipped().await.unwrap().is_empty());
    let err = f.folder.refresh().await.unwrap_err();
    assert_eq!(error_name(&err), Some("org.konedrive.Error.Unsupported"));
}

/// Every refusal a caller can act on arrives as its own D-Bus error name
///. They all used to collapse into
/// `org.freedesktop.DBus.Error.Failed` with the reason in the message, which
/// leaves a client nothing to branch on but English prose — and "the file
/// was modified locally" and "the file is not downloaded" call for two
/// different answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refusal_arrives_as_its_own_named_error() {
    let f = setup().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![7u8; 8192]).unwrap();
    let outside = f.dir.path().join("elsewhere.bin");
    std::fs::write(&outside, b"not ours").unwrap();

    assert_eq!(
        refusal(f.files.hydrate(outside.to_str().unwrap()).await),
        "org.konedrive.Error.OutsideRoot",
        "before any root is registered, a path is in no account's folder"
    );
    assert_eq!(
        refusal(f.folder.register(outside.to_str().unwrap()).await),
        "org.konedrive.Error.Unsupported",
        "a file is not a folder"
    );

    f.folder.register(root.to_str().unwrap()).await.unwrap();

    let second = f.dir.path().join("Another");
    std::fs::create_dir(&second).unwrap();
    assert_eq!(
        refusal(f.folder.register(second.to_str().unwrap()).await),
        "org.konedrive.Error.AlreadyRegistered"
    );
    assert_eq!(
        refusal(f.files.hydrate(root.join("doc.bin").to_str().unwrap()).await),
        "org.konedrive.Error.NoSource",
        "nothing has been populated, so there is nowhere to fetch from"
    );

    f.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");

    assert_eq!(
        refusal(f.files.hydrate(outside.to_str().unwrap()).await),
        "org.konedrive.Error.OutsideRoot"
    );
    assert_eq!(
        refusal(f.files.hydrate(root.join("missing.bin").to_str().unwrap()).await),
        "org.konedrive.Error.Failed",
        "an I/O failure has no name of its own, and must not borrow one"
    );
    std::fs::write(root.join("stray.txt"), b"mine").unwrap();
    assert_eq!(
        refusal(f.files.hydrate(root.join("stray.txt").to_str().unwrap()).await),
        "org.konedrive.Error.NotManaged"
    );
    assert_eq!(
        refusal(f.files.dehydrate(file.to_str().unwrap()).await),
        "org.konedrive.Error.NotHydrated",
        "an online-only file has nothing to free"
    );

    f.files.hydrate(file.to_str().unwrap()).await.unwrap();
    {
        // Somebody else holds it open: the write lease is refused.
        let _open = std::fs::File::open(&file).unwrap();
        assert_eq!(
            refusal(f.files.dehydrate(file.to_str().unwrap()).await),
            "org.konedrive.Error.InUse"
        );
    }
    std::fs::write(&file, b"what the user typed").unwrap();
    assert_eq!(
        refusal(f.files.dehydrate(file.to_str().unwrap()).await),
        "org.konedrive.Error.ModifiedLocally"
    );
}

/// The refusal that needs a daemon in a different state — no helper at all —
/// and the explicit mode that is offered instead, which deliberately works
/// while signed out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_with_no_helper_refuses_to_register_but_offers_the_explicit_mode() {
    let f = setup_with_helper(false).await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    assert_eq!(
        refusal(f.folder.register(root.to_str().unwrap()).await),
        "org.konedrive.Error.NoHelper",
        "no helper means no interception, and a placeholder nobody intercepts reads as zeros"
    );
    assert_eq!(f.folder.state().await.unwrap(), "none");

    // Deliberately signed out. `RegisterRoot` requires a sign-in because §3.1
    // binds the folder to the signed-in drive; this mode must not, because it
    // exists for a machine with no helper and no drive, filled from a local
    // directory. Requiring a Microsoft sign-in here would put the one path
    // that works without the cloud behind the cloud.
    f.account.update(|s| s.state = SignInState::SignedOut);
    f.folder.register_without_interception(root.to_str().unwrap()).await.unwrap();

    assert_eq!(f.folder.state().await.unwrap(), "no-interception");
    assert!(
        f.folder.last_error().await.unwrap().contains("read as zeros"),
        "the mode must say what it costs: {}",
        f.folder.last_error().await.unwrap()
    );

    // And the folder is fully usable in that mode.
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![3u8; 2048]).unwrap();
    f.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");
    f.files.hydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), vec![3u8; 2048]);
    f.files.dehydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(f.files.item_state(file.to_str().unwrap()).await.unwrap(), "online-only");
}

/// Every property emits `PropertiesChanged`, and only the ones that actually
/// changed do. The mechanism had no test at all: the signal task, the
/// baseline it compares against, and each of the three properties could be
/// deleted with the suite still green.
///
/// The second step is the one that pins the baseline: a daemon that never
/// advanced `previous` would emit nothing at all for the return to `none`,
/// having already absorbed those values at the first step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn properties_changed_reports_exactly_what_changed() {
    let f = setup().await;
    let first = f.dir.path().join("OneDrive");
    let second = f.dir.path().join("Offline");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();

    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(f.path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.folder.register(first.to_str().unwrap()).await.unwrap();
    assert_eq!(
        changed_within(&mut changes, Duration::from_millis(600)).await,
        vec!["Path", "Source", "State"],
        "registering a root changes where it is, what it shows, and what state it is in"
    );

    f.folder.unregister().await.unwrap();
    assert_eq!(
        changed_within(&mut changes, Duration::from_millis(600)).await,
        vec!["Path", "Source", "State"],
        "forgetting it changes them back — a daemon comparing against a stale baseline \
         would say nothing here"
    );

    f.folder.register_without_interception(second.to_str().unwrap()).await.unwrap();
    assert_eq!(
        changed_within(&mut changes, Duration::from_millis(600)).await,
        vec!["LastError", "Path", "Source", "State"],
        "and this mode also publishes why it is dangerous"
    );
}

/// What a test says systemd says of the helper's unit — never the real
/// system bus.
struct FakeUnit(std::sync::Mutex<(String, String)>);

impl FakeUnit {
    fn says(&self, load: &str, active: &str) {
        *self.0.lock().unwrap() = (load.to_owned(), active.to_owned());
    }
}

#[async_trait::async_trait]
impl konedrived::helper::status::HelperUnit for FakeUnit {
    async fn states(&self) -> Option<(String, String)> {
        Some(self.0.lock().unwrap().clone())
    }
}

/// HS1: `HelperState` is `connected` while the daemon holds a link; when the
/// link drops, it is what systemd says of `konedrive-helper.service` — asked
/// at once, and again on the re-check interval while there is no link — and
/// every change is signalled. With a folder registered, `LastError` then
/// says how to start the helper (HS3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_state_follows_the_link_and_then_what_systemd_says() {
    let f = setup().await;
    let unit = Arc::new(FakeUnit(std::sync::Mutex::new(("loaded".into(), "inactive".into()))));
    f.sync.set_helper_unit(Arc::clone(&unit) as Arc<dyn konedrived::helper::status::HelperUnit>);
    let watching = tokio::spawn(konedrived::sync::watch_helper_every(Arc::clone(&f.sync), Duration::from_millis(100)));
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.folder.register(root.to_str().unwrap()).await.unwrap();
    assert_eq!(f.manager.helper_state().await.unwrap(), "connected");
    // `HelperState` is the manager's: one helper serves every account.
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(ACCOUNTS_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.sync.set_link(None);
    f.sync.report_helper_lost();
    assert!(changed_on(&mut changes, ACCOUNTS_INTERFACE_NAME, Duration::from_millis(600))
        .await
        .contains(&"HelperState".to_owned()));
    let state = || async { f.manager.helper_state().await.unwrap() };
    for _ in 0..100 {
        if state().await == "stopped" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state().await, "stopped");
    assert_eq!(f.folder.state().await.unwrap(), "error");
    assert!(f.folder.last_error().await.unwrap().contains("sudo systemctl start konedrive-helper"));

    unit.says("loaded", "failed");
    for _ in 0..100 {
        if state().await == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state().await, "failed", "asked again while there is no link");
    assert!(f.folder.last_error().await.unwrap().contains("systemctl status konedrive-helper"));
    watching.abort();
}

/// `Folder.State` and `LastError` are what the registration and the
/// folder's sync say together, so a change in the sync alone changes them —
/// and has to be signalled like any other change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_in_the_sync_alone_is_signalled_as_what_it_publishes() {
    let f = setup_with_helper(false).await;
    let folder = f.dir.path().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    f.folder.register_without_interception(folder.to_str().unwrap()).await.unwrap();
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(f.path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.sync.state().update(|s| {
        s.sync_trouble = Some(SyncTrouble { text: "signed out".into(), blocking: true })
    });
    assert_eq!(
        changed_within(&mut changes, Duration::from_millis(600)).await,
        vec!["LastError", "State"]
    );
    assert_eq!(f.folder.state().await.unwrap(), "error");
    assert!(f.folder.last_error().await.unwrap().ends_with(". signed out"));

    f.sync.state().update(|s| s.replacement_note = "1 file(s) changed in OneDrive could not be updated here yet".into());
    assert_eq!(changed_within(&mut changes, Duration::from_millis(600)).await, vec!["LastError"]);
}

/// The coalescing (at most four `PropertiesChanged` a second, since a
/// listing changes the counters with every page) is about *signals on the
/// bus*, not about properties: three counters changing at once must arrive
/// as one `PropertiesChanged` carrying all three, not three separate
/// messages that each happen to land inside the same window. `changed_within`
/// above (built for `Path`/`State`/`LastError`, each still its own
/// `_changed()` call and so its own message) cannot tell those apart — it
/// merges every message in the window into one set of names — so this uses
/// [`messages_within`] instead, which keeps each message separate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_counters_travel_in_one_properties_changed_message() {
    let f = setup_with_helper(false).await;
    let folder = f.dir.path().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    f.folder.register_without_interception(folder.to_str().unwrap()).await.unwrap();
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(f.path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.sync.state().update(|s| {
        s.items_listed = 10;
        s.items_placed = 5;
        s.skipped_count = 2;
    });

    // Only the messages that carry a counter: what the registration itself changed (`Path`,
    // `Source`, `State`, `LastError`) is sent by another task, and may come after the
    // subscription above (limitations log D31).
    let counters = ["ItemsListed", "ItemsPlaced", "SkippedCount"];
    let mut carrying = messages_within(&mut changes, FOLDER_INTERFACE_NAME, Duration::from_millis(600)).await;
    carrying.retain(|names| names.iter().any(|name| counters.contains(&name.as_str())));
    assert_eq!(
        carrying,
        vec![vec!["ItemsListed".to_owned(), "ItemsPlaced".to_owned(), "SkippedCount".to_owned()]],
        "all three counters must travel in one PropertiesChanged message, not one each"
    );
}

/// Each property's `PropertiesChanged` goes out under the interface that holds it: one
/// change of the state that touches every interface of the folder sends one message for
/// each, carrying only its own properties.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_property_changes_under_its_own_interface() {
    let f = setup_with_helper(false).await;
    let folder = f.dir.path().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    f.folder.register_without_interception(folder.to_str().unwrap()).await.unwrap();
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(f.path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.sync.state().update(|s| {
        s.items_listed = 3;
        s.throughput.size = 7;
        s.pending_count = 2;
        s.conflict_count = 1;
        s.scan.directories = 5;
    });

    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    let mut seen = Vec::new();
    while let Ok(Some(signal)) = tokio::time::timeout_at(deadline, changes.next()).await {
        let args = signal.args().unwrap();
        let mut names: Vec<String> = args.changed_properties.keys().map(|k| k.to_string()).collect();
        names.sort();
        seen.push((args.interface_name.to_string(), names));
    }
    // Only the messages that carry one of the properties changed here: what the registration
    // itself changed is sent by another task, and may come after the subscription above
    // (limitations log D31).
    let changed = ["Count", "ItemsListed", "Directories", "PoolSize", "PendingCount"];
    seen.retain(|(_, names)| names.iter().any(|name| changed.contains(&name.as_str())));
    seen.sort();
    let one = |interface: &str, name: &str| (interface.to_owned(), vec![name.to_owned()]);
    assert_eq!(
        seen,
        vec![
            one(CONFLICTS_INTERFACE_NAME, "Count"),
            one(FOLDER_INTERFACE_NAME, "ItemsListed"),
            one(LOCAL_SCAN_INTERFACE_NAME, "Directories"),
            one(TRANSFERS_INTERFACE_NAME, "PoolSize"),
            one(UPLOAD_QUEUE_INTERFACE_NAME, "PendingCount"),
        ]
    );
}

/// One quota per account (issue #78): what the folder's uploads read shows in `Account`'s
/// four quota properties, with their `PropertiesChanged` — the uploads keep no copy of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_folders_quota_read_is_the_accounts_quota() {
    let f = setup().await;
    let account = konedrive_dbus::accounts::AccountProxy::builder(&f.client)
        .path(f.path.clone())
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(f.path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.sync.quota().read(&konedrive_graph::drive::DriveQuota { total: 100, used: 40, remaining: Some(60), state: "nearing".into() });

    assert_eq!(
        changed_on(&mut changes, konedrive_dbus::ACCOUNT_INTERFACE_NAME, Duration::from_millis(600)).await,
        vec!["QuotaRemaining", "QuotaState", "QuotaTotal", "QuotaUsed"]
    );
    assert_eq!(
        (account.quota_used().await.unwrap(), account.quota_total().await.unwrap(), account.quota_remaining().await.unwrap()),
        (40, 100, 60)
    );
    assert_eq!(account.quota_state().await.unwrap(), "nearing");
    f.sync.quota().uploaded(10);
    assert_eq!((account.quota_used().await.unwrap(), account.quota_remaining().await.unwrap()), (50, 50));
}

/// The Full local scan on the bus (issue #8): a read-only folder has none, and a scan's
/// progress travels with the counters, in one message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_local_scan_is_on_the_bus() {
    use konedrived::status::snapshot::ScanState;
    let f = setup().await;
    assert_eq!(f.scan.state().await.unwrap(), "none", "a read-only folder has no local scan");
    assert_eq!(f.scan.finished().await.unwrap(), 0);
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(f.path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.sync.state().update(|s| {
        s.scan.state = ScanState::Running;
        s.scan.reason = "overflow".into();
        s.scan.files = 7;
    });

    assert_eq!(
        messages_within(&mut changes, LOCAL_SCAN_INTERFACE_NAME, Duration::from_millis(600)).await,
        vec![vec!["Files".to_owned(), "Reason".to_owned(), "State".to_owned()]]
    );
    assert_eq!(
        (f.scan.state().await.unwrap(), f.scan.reason().await.unwrap(), f.scan.files().await.unwrap()),
        ("running".to_owned(), "overflow".to_owned(), 7)
    );
}

/// The names of the `Folder` properties that reported a change within
/// `window`, sorted and de-duplicated. One `PropertiesChanged` arrives per
/// property, so a window is what a caller has to work with.
async fn changed_within(
    changes: &mut zbus::fdo::PropertiesChangedStream,
    window: Duration,
) -> Vec<String> {
    changed_on(changes, FOLDER_INTERFACE_NAME, window).await
}

/// As [`changed_within`], for `interface`'s properties.
async fn changed_on(
    changes: &mut zbus::fdo::PropertiesChangedStream,
    interface: &str,
    window: Duration,
) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + window;
    let mut names = Vec::new();
    while let Ok(Some(signal)) = tokio::time::timeout_at(deadline, changes.next()).await {
        let args = signal.args().unwrap();
        if args.interface_name != interface {
            continue;
        }
        names.extend(args.changed_properties.keys().map(|k| k.to_string()));
        names.extend(args.invalidated_properties.iter().map(|k| k.to_string()));
    }
    names.sort();
    names.dedup();
    names
}

/// As [`changed_on`], but one entry per `PropertiesChanged` message
/// (its own changed/invalidated names, sorted) rather than merged across the
/// whole window — what tells "one message with three keys" apart from
/// "three messages with one key each".
async fn messages_within(
    changes: &mut zbus::fdo::PropertiesChangedStream,
    interface: &str,
    window: Duration,
) -> Vec<Vec<String>> {
    let deadline = tokio::time::Instant::now() + window;
    let mut messages = Vec::new();
    while let Ok(Some(signal)) = tokio::time::timeout_at(deadline, changes.next()).await {
        let args = signal.args().unwrap();
        if args.interface_name != interface {
            continue;
        }
        let mut names: Vec<String> = args.changed_properties.keys().map(|k| k.to_string()).collect();
        names.extend(args.invalidated_properties.iter().map(|k| k.to_string()));
        names.sort();
        messages.push(names);
    }
    messages
}

/// `Folder` has to be on the object before the bus name is, so
/// that a D-Bus-activated client's very first call cannot be answered with
/// `UnknownInterface` by a daemon that already owns the name.
///
/// The helper here accepts the connection and then says nothing, which is
/// the shape that makes `HelperLink::connect` take its full 30 s bound
///. The daemon used to claim the name, then connect, then
/// attach the folder's interface — so that silence was a 30 s window in which the daemon
/// was on the bus and this interface was not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_folder_answers_from_the_moment_the_daemon_is_on_the_bus() {
    let bus = TestBus::start();
    let helper_dir = tempfile::tempdir().unwrap();
    let socket_path = helper_dir.path().join("helper.sock");
    silent_helper(socket_path.clone());
    // An account the daemon finds in config.toml at its start.
    let config = tempfile::tempdir().unwrap();
    let paths = konedrived::config::Paths::in_dir(config.path());
    let id = konedrived::config::ConfigStore::open(&paths, async { false }).await.add_account("Personal").unwrap().id;

    let daemon = start_daemon(
        &bus,
        config.path(),
        Endpoints::microsoft(),
        Arc::new(MemoryWallet::default()),
        Duration::from_secs(5),
    )
    .await;
    tokio::spawn(konedrived::sync::hub::supervise(
        Arc::clone(daemon.manager.hub()),
        socket_path,
        Duration::from_millis(50),
    ));

    let client = bus.connect().await;
    let proxy = FolderProxy::new(&client, konedrive_dbus::account_path(&id).unwrap()).await.unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(3), proxy.state()).await;

    assert_eq!(
        answered
            .expect("Folder did not answer while the daemon was waiting on a silent helper")
            .unwrap(),
        "none"
    );
}

//! End-to-end tests for `org.konedrive.Sync1`, over a private test bus, the
//! way `tests/dbus_api.rs` exercises `org.konedrive.Account1`. No fanotify is
//! involved: a fake helper thread speaks the wire protocol and acknowledges
//! everything, but never actually marks anything or sends a `HydrateRequest`
//! — see `konedrived::sync::SyncService`'s own doc comment for why
//! `Hydrate()` does not depend on that to fill a file.
//!
//! The daemon is wired here exactly as `main.rs` wires it: both interfaces
//! go through `konedrived::dbus::serve`, so `Sync1` is on the object before
//! the bus name is claimed, and the helper is reached through
//! `sync::supervise_helper` rather than inline.

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::{error_name, Dev1Proxy, Sync1Proxy, OBJECT_PATH, SERVICE_NAME, SYNC_INTERFACE_NAME};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use konedrived::account::AccountService;
use konedrived::config::Paths;
use konedrived::oauth::Endpoints;
use konedrived::secret::MemoryStore;
use konedrived::state::{SignInState, StateHandle};
use konedrived::sync::helper::HelperLink;
use konedrived::sync::{SyncService, SyncTrouble};
use nix::sys::socket::{
    accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};

const XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Sync1.xml"));
const DEV_XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Dev1.xml"));

struct Setup {
    proxy: Sync1Proxy<'static>,
    client: zbus::Connection,
    _server: zbus::Connection,
    dir: tempfile::TempDir,
    account: StateHandle,
    /// The daemon's own half, for what no method can reach: the state a sync
    /// with OneDrive publishes.
    sync: Arc<SyncService>,
    _helper_dir: tempfile::TempDir,
    _bus: TestBus,
}

/// Accepts one connection on a `SOCK_SEQPACKET` socket at `path`, greets,
/// acknowledges `Hello`, and after that acknowledges every request with
/// `Ack { errno: 0 }` — never sending a `HydrateRequest` of its own, since no
/// fanotify group backs any of this. Built the same way
/// `sync::helper`'s own test module builds its fake helpers.
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

fn account_service() -> Arc<AccountService> {
    let account_dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    AccountService::new(
        Paths::in_dir(account_dir.path()),
        Endpoints::microsoft(),
        Arc::new(MemoryStore::default()),
        Duration::from_secs(5),
    )
    .unwrap()
}

async fn setup() -> Setup {
    setup_with_helper(true).await
}

async fn setup_with_helper(with_helper: bool) -> Setup {
    let bus = TestBus::start();

    let helper_dir = tempfile::tempdir().unwrap();
    let socket_path = helper_dir.path().join("helper.sock");
    let link = if with_helper {
        fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        Some(link)
    } else {
        None
    };

    let account_service = account_service();
    // §3.1 refuses a registration when nobody is signed in, so the tests
    // that register a folder start from a signed-in daemon. The one that
    // measures the refusal signs out again.
    account_service.state().update(|s| s.state = SignInState::SignedIn);
    let account = account_service.state().clone();
    let sync_service = SyncService::new(link, Some(account.clone()), None);
    // Without a helper, nothing is bound at this path: a punch with no link
    // goes ahead whatever this machine runs at the real one.
    sync_service.set_helper_socket(&socket_path);

    let server =
        konedrived::dbus::serve(bus.builder(), account_service, Some(Arc::clone(&sync_service)))
            .await
            .unwrap();

    let client = bus.connect().await;
    // Property reads go to the daemon every time. A caching proxy — which is
    // what `konedrivectl` uses — would answer some of these assertions from
    // the value it cached before the change, which is a property of the
    // proxy, not of the daemon this file is about.
    let proxy = Sync1Proxy::builder(&client)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    Setup {
        proxy,
        client,
        _server: server,
        dir,
        account,
        sync: sync_service,
        _helper_dir: helper_dir,
        _bus: bus,
    }
}

async fn introspect(client: &zbus::Connection, path: &str) -> String {
    let introspectable = zbus::fdo::IntrospectableProxy::builder(client)
        .destination(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .path(path)
        .unwrap()
        .build()
        .await
        .unwrap();
    introspectable.introspect().await.unwrap()
}

/// Normalizes one interface to sorted lines such as `method Hydrate in=s
/// out=` and `property RootState s read`. Argument names are ignored. Lifted
/// from `tests/dbus_api.rs`, which has the canonical copy for `Account1`.
fn signature_lines(xml: &str, interface: &str) -> Vec<String> {
    let start = xml
        .find(&format!("<interface name=\"{interface}\""))
        .unwrap_or_else(|| panic!("interface {interface} missing in:\n{xml}"));
    let end = start + xml[start..].find("</interface>").expect("unterminated interface");
    let mut lines = Vec::new();
    let mut method: Option<(String, String, String)> = None;
    let flush = |method: &mut Option<(String, String, String)>, lines: &mut Vec<String>| {
        if let Some((name, input, output)) = method.take() {
            lines.push(format!("method {name} in={input} out={output}"));
        }
    };
    for raw in xml[start..end].split('<').skip(1) {
        let tag = raw.split('>').next().unwrap_or_default().trim();
        let attr = |key: &str| -> String {
            let pattern = format!("{key}=\"");
            tag.find(&pattern)
                .map(|i| {
                    let rest = &tag[i + pattern.len()..];
                    rest[..rest.find('"').unwrap()].to_owned()
                })
                .unwrap_or_default()
        };
        if tag.starts_with("method ") {
            flush(&mut method, &mut lines);
            method = Some((attr("name"), String::new(), String::new()));
        } else if tag.starts_with("arg ") {
            if let Some((_, input, output)) = method.as_mut() {
                if attr("direction") == "out" {
                    output.push_str(&attr("type"));
                } else {
                    input.push_str(&attr("type"));
                }
            }
        } else if tag.starts_with("/method") {
            flush(&mut method, &mut lines);
        } else if tag.starts_with("property ") {
            flush(&mut method, &mut lines);
            lines.push(format!("property {} {} {}", attr("name"), attr("type"), attr("access")));
        }
    }
    flush(&mut method, &mut lines);
    lines.sort();
    lines
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

    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();

    assert_eq!(f.proxy.root_path().await.unwrap(), root.to_str().unwrap());
    assert_eq!(f.proxy.root_state().await.unwrap(), "ready");
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

    let name = refusal(f.proxy.register_root(root.to_str().unwrap()).await);
    assert_eq!(name, "org.konedrive.Error.NotEmpty");
    assert_eq!(f.proxy.root_state().await.unwrap(), "none");
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
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();

    let created = f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    assert_eq!(created, 2);

    use std::os::unix::fs::MetadataExt;
    let placeholder = root.join("a.bin");
    let meta = std::fs::metadata(&placeholder).unwrap();
    assert_eq!(meta.len(), 4096, "the placeholder reports the real size");
    assert!(meta.blocks() < 8, "and takes no space");
    assert_eq!(f.proxy.item_state(placeholder.to_str().unwrap()).await.unwrap(), "online-only");
    assert_eq!(
        f.proxy.item_state(root.join("sub/b.bin").to_str().unwrap()).await.unwrap(),
        "online-only"
    );
    assert_eq!(
        f.proxy.item_state(source.join("a.bin").to_str().unwrap()).await.unwrap(),
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
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");

    f.proxy.hydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(f.proxy.item_state(file.to_str().unwrap()).await.unwrap(), "hydrated");
    assert_eq!(std::fs::read(&file).unwrap(), vec![7u8; 8192]);

    f.proxy.dehydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(f.proxy.item_state(file.to_str().unwrap()).await.unwrap(), "online-only");
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.len(), 8192, "the size survives");
    assert!(meta.blocks() < 8, "the content is gone");
}

/// Over the bus: a download is announced as it happens
/// (`ActivityAdded`), kept (`RecentActivity`), and `FreeUpSpace` answers
/// (files, bytes, busy) in three out arguments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_is_announced_kept_and_freed_up_again() {
    let f = setup().await;
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![7u8; 8192]).unwrap();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");
    let shown = file.to_str().unwrap();
    let mut added = f.proxy.receive_activity_added().await.unwrap();

    f.proxy.hydrate(shown).await.unwrap();

    let signal = tokio::time::timeout(Duration::from_secs(5), added.next())
        .await
        .expect("no ActivityAdded for the download")
        .unwrap();
    let args = signal.args().unwrap();
    assert_eq!((args.kind().as_str(), args.path().as_str(), args.detail().as_str()), ("downloaded", shown, "8.0 KiB"));
    let recent = f.proxy.recent_activity(10).await.unwrap();
    assert_eq!(recent.len(), 1, "{recent:?}");
    assert_eq!((recent[0].1.as_str(), recent[0].2.as_str()), ("downloaded", shown));
    assert_eq!(recent[0].0, *args.time());

    let (files, bytes, busy) = f.proxy.free_up_space().await.unwrap();
    assert_eq!((files, busy), (1, 0));
    assert!(bytes >= 8192, "{bytes}");
    assert_eq!(f.proxy.item_state(shown).await.unwrap(), "online-only");
}

/// Dismissing a conflict that is not there is an error, and the
/// error names the path it was asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismissing_a_conflict_that_is_not_there_names_the_path() {
    let f = setup().await;
    let error = f.proxy.dismiss_conflict("/nowhere/rescued.txt").await.unwrap_err();
    assert_eq!(error_name(&error), Some("org.konedrive.Error.NoConflict"));
    assert!(error.to_string().contains("/nowhere/rescued.txt"), "{error}");
    assert!(f.proxy.conflicts().await.unwrap().is_empty());
    assert_eq!(f.proxy.conflict_count().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn introspection_matches_the_checked_in_xml() {
    let f = setup().await;
    let live = introspect(&f.client, "/org/konedrive/Daemon").await;
    assert_eq!(signature_lines(&live, SYNC_INTERFACE_NAME), signature_lines(XML, SYNC_INTERFACE_NAME));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev1_matches_its_checked_in_xml() {
    let f = setup().await;
    let live = introspect(&f.client, OBJECT_PATH).await;
    assert_eq!(
        signature_lines(&live, "org.konedrive.Dev1"),
        signature_lines(DEV_XML, "org.konedrive.Dev1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_to_export_while_signed_out() {
    let f = setup().await;
    let dev = Dev1Proxy::new(&f.client).await.unwrap();
    let err = dev.access_token().await.unwrap_err();
    assert_eq!(error_name(&err), Some("org.konedrive.Error.NotSignedIn"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_folder_has_no_counters_and_refuses_refresh() {
    let f = setup().await;
    let root = f.dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    assert_eq!(f.proxy.root_source().await.unwrap(), "local");
    assert_eq!((f.proxy.items_listed().await.unwrap(), f.proxy.skipped_count().await.unwrap()), (0, 0));
    assert!(f.proxy.skipped().await.unwrap().is_empty());
    let err = f.proxy.refresh().await.unwrap_err();
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
        refusal(f.proxy.hydrate(outside.to_str().unwrap()).await),
        "org.konedrive.Error.NoRoot",
        "before any root is registered"
    );
    assert_eq!(
        refusal(f.proxy.register_root(outside.to_str().unwrap()).await),
        "org.konedrive.Error.Unsupported",
        "a file is not a folder"
    );

    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();

    let second = f.dir.path().join("Another");
    std::fs::create_dir(&second).unwrap();
    assert_eq!(
        refusal(f.proxy.register_root(second.to_str().unwrap()).await),
        "org.konedrive.Error.AlreadyRegistered"
    );
    assert_eq!(
        refusal(f.proxy.hydrate(root.join("doc.bin").to_str().unwrap()).await),
        "org.konedrive.Error.NoSource",
        "nothing has been populated, so there is nowhere to fetch from"
    );

    f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");

    assert_eq!(
        refusal(f.proxy.hydrate(outside.to_str().unwrap()).await),
        "org.konedrive.Error.OutsideRoot"
    );
    assert_eq!(
        refusal(f.proxy.hydrate(root.join("missing.bin").to_str().unwrap()).await),
        "org.konedrive.Error.Failed",
        "an I/O failure has no name of its own, and must not borrow one"
    );
    std::fs::write(root.join("stray.txt"), b"mine").unwrap();
    assert_eq!(
        refusal(f.proxy.hydrate(root.join("stray.txt").to_str().unwrap()).await),
        "org.konedrive.Error.NotManaged"
    );
    assert_eq!(
        refusal(f.proxy.dehydrate(file.to_str().unwrap()).await),
        "org.konedrive.Error.NotHydrated",
        "an online-only file has nothing to free"
    );

    f.proxy.hydrate(file.to_str().unwrap()).await.unwrap();
    {
        // Somebody else holds it open: the write lease is refused.
        let _open = std::fs::File::open(&file).unwrap();
        assert_eq!(
            refusal(f.proxy.dehydrate(file.to_str().unwrap()).await),
            "org.konedrive.Error.InUse"
        );
    }
    std::fs::write(&file, b"what the user typed").unwrap();
    assert_eq!(
        refusal(f.proxy.dehydrate(file.to_str().unwrap()).await),
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
        refusal(f.proxy.register_root(root.to_str().unwrap()).await),
        "org.konedrive.Error.NoHelper",
        "no helper means no interception, and a placeholder nobody intercepts reads as zeros"
    );
    assert_eq!(f.proxy.root_state().await.unwrap(), "none");

    // Deliberately signed out. `RegisterRoot` requires a sign-in because §3.1
    // binds the folder to the signed-in drive; this mode must not, because it
    // exists for a machine with no helper and no drive, filled from a local
    // directory. Requiring a Microsoft sign-in here would put the one path
    // that works without the cloud behind the cloud.
    f.account.update(|s| s.state = SignInState::SignedOut);
    f.proxy.register_root_without_interception(root.to_str().unwrap()).await.unwrap();

    assert_eq!(f.proxy.root_state().await.unwrap(), "no-interception");
    assert!(
        f.proxy.last_error().await.unwrap().contains("read as zeros"),
        "the mode must say what it costs: {}",
        f.proxy.last_error().await.unwrap()
    );

    // And the folder is fully usable in that mode.
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![3u8; 2048]).unwrap();
    f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");
    f.proxy.hydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), vec![3u8; 2048]);
    f.proxy.dehydrate(file.to_str().unwrap()).await.unwrap();
    assert_eq!(f.proxy.item_state(file.to_str().unwrap()).await.unwrap(), "online-only");
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
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.proxy.register_root(first.to_str().unwrap()).await.unwrap();
    assert_eq!(
        changed_within(&mut changes, Duration::from_millis(600)).await,
        vec!["RootPath", "RootSource", "RootState"],
        "registering a root changes where it is, what it shows, and what state it is in"
    );

    f.proxy.unregister_root().await.unwrap();
    assert_eq!(
        changed_within(&mut changes, Duration::from_millis(600)).await,
        vec!["RootPath", "RootSource", "RootState"],
        "forgetting it changes them back — a daemon comparing against a stale baseline \
         would say nothing here"
    );

    f.proxy.register_root_without_interception(second.to_str().unwrap()).await.unwrap();
    assert_eq!(
        changed_within(&mut changes, Duration::from_millis(600)).await,
        vec!["LastError", "RootPath", "RootSource", "RootState"],
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
impl konedrived::sync::helper_status::HelperUnit for FakeUnit {
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
    f.sync.set_helper_unit(Arc::clone(&unit) as Arc<dyn konedrived::sync::helper_status::HelperUnit>);
    let watching = tokio::spawn(konedrived::sync::watch_helper_every(Arc::clone(&f.sync), Duration::from_millis(100)));
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    assert_eq!(f.proxy.helper_state().await.unwrap(), "connected");
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = properties.receive_properties_changed().await.unwrap();

    f.sync.set_link(None);
    f.sync.report_helper_lost();
    assert!(changed_within(&mut changes, Duration::from_millis(600)).await.contains(&"HelperState".to_owned()));
    let state = || async { f.proxy.helper_state().await.unwrap() };
    for _ in 0..100 {
        if state().await == "stopped" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state().await, "stopped");
    assert_eq!(f.proxy.root_state().await.unwrap(), "error");
    assert!(f.proxy.last_error().await.unwrap().contains("sudo systemctl start konedrive-helper"));

    unit.says("loaded", "failed");
    for _ in 0..100 {
        if state().await == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state().await, "failed", "asked again while there is no link");
    assert!(f.proxy.last_error().await.unwrap().contains("systemctl status konedrive-helper"));
    watching.abort();
}

/// `RootState` and `LastError` are what the registration and the
/// folder's sync say together, so a change in the sync alone changes them —
/// and has to be signalled like any other change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_in_the_sync_alone_is_signalled_as_what_it_publishes() {
    let f = setup_with_helper(false).await;
    let folder = f.dir.path().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    f.proxy.register_root_without_interception(folder.to_str().unwrap()).await.unwrap();
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(OBJECT_PATH)
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
        vec!["LastError", "RootState"]
    );
    assert_eq!(f.proxy.root_state().await.unwrap(), "error");
    assert!(f.proxy.last_error().await.unwrap().ends_with(". signed out"));

    f.sync.state().update(|s| s.replacement_note = "1 file(s) changed in OneDrive could not be updated here yet".into());
    assert_eq!(changed_within(&mut changes, Duration::from_millis(600)).await, vec!["LastError"]);
}

/// The coalescing (at most four `PropertiesChanged` a second, since a
/// listing changes the counters with every page) is about *signals on the
/// bus*, not about properties: three counters changing at once must arrive
/// as one `PropertiesChanged` carrying all three, not three separate
/// messages that each happen to land inside the same window. `changed_within`
/// above (built for `RootPath`/`RootState`/`LastError`, each still its own
/// `_changed()` call and so its own message) cannot tell those apart — it
/// merges every message in the window into one set of names — so this uses
/// [`messages_within`] instead, which keeps each message separate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_counters_travel_in_one_properties_changed_message() {
    let f = setup_with_helper(false).await;
    let folder = f.dir.path().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    f.proxy.register_root_without_interception(folder.to_str().unwrap()).await.unwrap();
    let properties = zbus::fdo::PropertiesProxy::builder(&f.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(OBJECT_PATH)
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

    assert_eq!(
        messages_within(&mut changes, Duration::from_millis(600)).await,
        vec![vec!["ItemsListed".to_owned(), "ItemsPlaced".to_owned(), "SkippedCount".to_owned()]],
        "all three counters must travel in one PropertiesChanged message, not one each"
    );
}

/// The names of the `Sync1` properties that reported a change within
/// `window`, sorted and de-duplicated. One `PropertiesChanged` arrives per
/// property, so a window is what a caller has to work with.
async fn changed_within(
    changes: &mut zbus::fdo::PropertiesChangedStream,
    window: Duration,
) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + window;
    let mut names = Vec::new();
    while let Ok(Some(signal)) = tokio::time::timeout_at(deadline, changes.next()).await {
        let args = signal.args().unwrap();
        if args.interface_name != SYNC_INTERFACE_NAME {
            continue;
        }
        names.extend(args.changed_properties.keys().map(|k| k.to_string()));
        names.extend(args.invalidated_properties.iter().map(|k| k.to_string()));
    }
    names.sort();
    names.dedup();
    names
}

/// As [`changed_within`], but one entry per `PropertiesChanged` message
/// (its own changed/invalidated names, sorted) rather than merged across the
/// whole window — what tells "one message with three keys" apart from
/// "three messages with one key each".
async fn messages_within(
    changes: &mut zbus::fdo::PropertiesChangedStream,
    window: Duration,
) -> Vec<Vec<String>> {
    let deadline = tokio::time::Instant::now() + window;
    let mut messages = Vec::new();
    while let Ok(Some(signal)) = tokio::time::timeout_at(deadline, changes.next()).await {
        let args = signal.args().unwrap();
        if args.interface_name != SYNC_INTERFACE_NAME {
            continue;
        }
        let mut names: Vec<String> = args.changed_properties.keys().map(|k| k.to_string()).collect();
        names.extend(args.invalidated_properties.iter().map(|k| k.to_string()));
        names.sort();
        messages.push(names);
    }
    messages
}

/// `Sync1` has to be on the object before the bus name is, so
/// that a D-Bus-activated client's very first call cannot be answered with
/// `UnknownInterface` by a daemon that already owns the name.
///
/// The helper here accepts the connection and then says nothing, which is
/// the shape that makes `HelperLink::connect` take its full 30 s bound
///. The daemon used to claim the name, then connect, then
/// attach `Sync1` — so that silence was a 30 s window in which the daemon
/// was on the bus and this interface was not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync1_answers_from_the_moment_the_daemon_is_on_the_bus() {
    let bus = TestBus::start();
    let helper_dir = tempfile::tempdir().unwrap();
    let socket_path = helper_dir.path().join("helper.sock");
    silent_helper(socket_path.clone());

    let account = account_service();
    account.state().update(|s| s.state = SignInState::SignedIn);
    let sync_service = SyncService::new(None, Some(account.state().clone()), None);
    let _server = konedrived::dbus::serve(bus.builder(), account, Some(Arc::clone(&sync_service)))
        .await
        .unwrap();
    tokio::spawn(konedrived::sync::supervise_helper(
        sync_service,
        socket_path,
        Duration::from_millis(50),
    ));

    let client = bus.connect().await;
    let proxy = Sync1Proxy::new(&client).await.unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(3), proxy.root_state()).await;

    assert_eq!(
        answered
            .expect("Sync1 did not answer while the daemon was waiting on a silent helper")
            .unwrap(),
        "none"
    );
}

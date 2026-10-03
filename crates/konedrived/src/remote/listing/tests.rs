use crate::helper::HelperLink;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use konedrive_fs::placeholder::{self, State, XATTR_ROOT};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use serde_json::{json, Value};
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use xattr::FileExt;

use super::*;
use konedrive_graph::drive::RetryPolicy;
use crate::hydration::graph_source::GraphSource;
use crate::remote::materialize::Rescued;
use crate::status::snapshot::SyncSnapshot;
use konedrive_graph::token::StaticToken;
use konedrive_tree::TreeStore;

pub(super) struct Setup {
    pub(super) server: MockServer,
    pub(super) _dir: tempfile::TempDir,
    pub(super) root: SyncRoot,
    pub(super) store: Store,
    pub(super) state: SyncStateHandle,
    /// Where every listing made from this setup reports,
    /// its activity kept in `store`.
    pub(super) report: Report,
    /// The preferred rescue directory (`ListingContext::rescue_dir`).
    pub(super) rescue_dir: PathBuf,
    pub(super) _rescue: Option<tempfile::TempDir>,
    /// A link to a helper that acknowledges everything: a folder that
    /// shows OneDrive is kept in step only with one (HS2).
    pub(super) link: HelperLink,
    pub(super) _helper: tempfile::TempDir,
    /// What every listing made from this setup queues for pins, and
    /// never downloads.
    pub(super) pins: Arc<Pins>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Ok(disk) = Disk::open(&self.root, false) {
            let _ = disk.unlock_tree();
        }
    }
}

pub(super) async fn setup() -> Setup {
    let rescue = tempfile::tempdir().unwrap();
    let mut s = setup_rescuing_into(rescue.path().to_path_buf()).await;
    s._rescue = Some(rescue);
    s
}

/// The folder is `OneDrive` inside a temporary directory, so that a
/// rescue directory made beside it is cleaned up with it.
pub(super) async fn setup_rescuing_into(rescue_dir: PathBuf) -> Setup {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    let root_id = "8f6c0a3e-3b0e-4d7a-9c1e-5b2d7e4f1a90".to_owned();
    File::open(&folder).unwrap().set_xattr(XATTR_ROOT, root_id.as_bytes()).unwrap();
    let store = Store::new(TreeStore::in_memory().unwrap());
    // The folder is what `SyncService` has registered: what its events are about.
    let state = SyncStateHandle::new(SyncSnapshot { root_path: folder.display().to_string(), ..SyncSnapshot::default() });
    let report = Report::new(state.clone());
    let pins = Pins::detached(state.clone());
    konedrive_tree::off_runtime(|| report.activity.attach(store.clone(), &folder));
    let helper = tempfile::tempdir().unwrap();
    let socket_path = helper.path().join("helper.sock");
    recording_helper(&socket_path);
    let link = HelperLink::connect(&socket_path).await.unwrap().0;
    Setup {
        server,
        _dir: dir,
        root: SyncRoot { path: folder, root_id },
        store,
        state,
        report,
        rescue_dir,
        _rescue: None,
        link,
        _helper: helper,
        pins,
    }
}

impl Setup {
    pub(super) fn drive(&self) -> DriveClient {
        DriveClient::new(Url::parse(&format!("{}/", self.server.uri())).unwrap(), Arc::new(StaticToken::new("T")))
            .unwrap()
            .with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(10) })
    }

    pub(super) fn context(&self) -> ListingContext {
        ListingContext {
            root: self.root.clone(),
            intercepted: true,
            store: self.store.clone(),
            drive: self.drive(),
            drive_record: None,
            source: Arc::new(GraphSource::new(self.drive())),
            link: Arc::new(std::sync::Mutex::new(Some(self.link.clone()))),
            locks: InodeLocks::new(),
            state: self.state.clone(),
            lifecycle: Arc::new(tokio::sync::RwLock::new(())),
            rescue_dir: self.rescue_dir.clone(),
            full_threshold: FULL_THRESHOLD,
            after_cycle: None,
            report: self.report.clone(),
            pins: Arc::clone(&self.pins),
            locked: true,
            writes: None,
            neighbours: None,
            running: Arc::default(),
        }
    }

    /// Every event recorded so far, oldest first, as (kind, path, detail).
    pub(super) fn activity(&self) -> Vec<(String, String, String)> {
        let mut events = konedrive_tree::off_runtime(|| self.report.activity.recent(1000)).unwrap();
        events.reverse();
        events.into_iter().map(|e| (e.kind, e.path, e.detail)).collect()
    }

    /// A full path in the folder, as events name it.
    pub(super) fn full(&self, rel: &str) -> String {
        self.root.path.join(rel).display().to_string()
    }

    pub(super) fn listing(&self) -> Arc<Listing> {
        Listing::new(self.context())
    }

    pub(super) fn listing_with(&self, full_threshold: usize) -> Arc<Listing> {
        Listing::new(ListingContext { full_threshold, ..self.context() })
    }

    pub(super) fn link(&self, token: &str) -> String {
        format!("{}/me/drive/root/delta?token={token}", self.server.uri())
    }

    /// The delta feed from `from` (None: the start) answers `items` and
    /// ends with the link `next`, once.
    pub(super) async fn feed(&self, from: Option<&str>, items: Value, next: &str) {
        self.feed_after(from, items, next, Duration::ZERO).await;
    }

    /// [`Self::feed`], answering only after `delay`.
    pub(super) async fn feed_after(&self, from: Option<&str>, items: Value, next: &str, delay: Duration) {
        let mock = Mock::given(method("GET")).and(path("/me/drive/root/delta"));
        let mock = match from {
            Some(token) => mock.and(query_param("token", token)),
            None => mock,
        };
        mock.respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"value": items, "@odata.deltaLink": self.link(next)}))
                .set_delay(delay),
        )
        .up_to_n_times(1)
        .with_priority(if from.is_some() { 1 } else { 5 })
        .mount(&self.server)
        .await;
    }

    /// Page `from` of the delta feed (None: the first) holds `items`,
    /// and the page after it is at the link `next`, once.
    pub(super) async fn page(&self, from: Option<&str>, items: Value, next: &str) {
        let body = json!({"value": items, "@odata.nextLink": self.link(next)});
        self.answer(from, ResponseTemplate::new(200).set_body_json(body)).await;
    }

    /// The delta request from `from` (None: the start) is answered by
    /// `respond`, once.
    pub(super) async fn answer(&self, from: Option<&str>, respond: impl wiremock::Respond + 'static) {
        let mock = Mock::given(method("GET")).and(path("/me/drive/root/delta"));
        let mock = match from {
            Some(token) => mock.and(query_param("token", token)),
            None => mock,
        };
        mock.respond_with(respond)
            .up_to_n_times(1)
            .with_priority(if from.is_some() { 1 } else { 5 })
            .mount(&self.server)
            .await;
    }

    /// The delta request from `from`, held: the channel says when it is
    /// asked, and the answer — no page at all — comes only after twice
    /// [`PATIENCE`], longer than any test step waits (see [`within`]),
    /// so a test sees the request still open for as long as it looks.
    pub(super) async fn held(&self, from: Option<&str>) -> tokio::sync::mpsc::UnboundedReceiver<()> {
        let (asked, heard) = tokio::sync::mpsc::unbounded_channel();
        self.answer(from, move |_: &Request| {
            let _ = asked.send(());
            ResponseTemplate::new(200).set_delay(2 * PATIENCE)
        })
        .await;
        heard
    }

    /// The `token` of every delta request so far, in order; `None` for
    /// one from the start.
    pub(super) async fn delta_tokens(&self) -> Vec<Option<String>> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/me/drive/root/delta")
            .map(|r| r.url.query_pairs().find(|(k, _)| k == "token").map(|(_, v)| v.into_owned()))
            .collect()
    }

    /// Graph's metadata for F at version `ctag`, holding `content`.
    pub(super) fn version(&self, ctag: &str, content: &[u8]) -> ResponseTemplate {
        let mut hash = konedrive_graph::quickxor::QuickXor::new();
        hash.update(content);
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "F", "name": "f.txt", "size": content.len(), "cTag": ctag,
            "file": {"hashes": {"quickXorHash": hash.finish_base64()}},
            "@microsoft.graph.downloadUrl": format!("{}/dl/F/{ctag}", self.server.uri())
        }))
    }

    /// Graph's metadata for F at version c2, holding `content`.
    pub(super) fn new_version(&self, content: &[u8]) -> ResponseTemplate {
        self.version("c2", content)
    }

    /// The bytes of F's version `ctag`.
    pub(super) async fn serve_download(&self, ctag: &str, content: &[u8]) {
        Mock::given(method("GET")).and(path(format!("/dl/F/{ctag}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&self.server).await;
    }

    /// Serves `content` as F's version c2, its metadata answered by `metadata`.
    pub(super) async fn serve_new_version(&self, content: &[u8], metadata: impl wiremock::Respond + 'static) {
        Mock::given(method("GET")).and(path("/me/drive/items/F"))
            .respond_with(metadata)
            .with_priority(2)
            .mount(&self.server).await;
        self.serve_download("c2", content).await;
    }
}

pub(super) fn root_item() -> Value {
    json!({"id": "R", "root": {}, "folder": {}})
}

pub(super) fn folder(id: &str, parent: &str, name: &str) -> Value {
    json!({"id": id, "name": name, "folder": {}, "parentReference": {"id": parent}})
}

pub(super) fn file(id: &str, parent: &str, name: &str, ctag: &str) -> Value {
    json!({"id": id, "name": name, "size": 10, "cTag": ctag, "file": {}, "parentReference": {"id": parent},
           "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}})
}

pub(super) fn vault() -> Value {
    json!({"id": "V", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}})
}

pub(super) async fn delta_requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/root/delta").count()
}

pub(super) async fn listed(s: &Setup) -> Arc<Listing> {
    listed_with(s, s.context()).await
}

/// [`listed`], through a listing made from `context`.
pub(super) async fn listed_with(s: &Setup, context: ListingContext) -> Arc<Listing> {
    s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()]), "L1").await;
    let listing = Listing::new(context);
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing
}

/// The longest a page-by-page test waits for anything: a regression that
/// would hang it fails it in seconds instead.
pub(super) const PATIENCE: Duration = Duration::from_secs(10);

/// `work`, or a failure once [`PATIENCE`] is out.
pub(super) async fn within<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(PATIENCE, work).await.expect("waited longer than a test should")
}

/// A cycle of `listing` in the background, stopped by `cancel`.
pub(super) fn spawn_cycle(listing: &Arc<Listing>, cancel: &CancellationToken) -> tokio::task::JoinHandle<Result<CycleReport, CycleError>> {
    let (listing, cancel) = (Arc::clone(listing), cancel.clone());
    tokio::spawn(async move { listing.cycle(&cancel).await })
}

/// Everything beneath `root`, hidden names too, as sorted relative paths.
pub(super) fn tree_of(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(rel) = pending.pop() {
        for entry in std::fs::read_dir(root.join(&rel)).unwrap() {
            let entry = entry.unwrap();
            let child = rel.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                pending.push(child.clone());
            }
            out.push(child.display().to_string());
        }
    }
    out.sort();
    out
}

pub(super) fn ino(at: &Path) -> u64 {
    std::fs::symlink_metadata(at).unwrap().ino()
}

pub(super) fn mode(at: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(at).unwrap().permissions().mode() & 0o7777
}

/// Downloads the file at `at` by hand, as a finished fill leaves it:
/// `content` at version c1.
pub(super) fn hydrate_by_hand(at: &Path, content: &[u8]) {
    use std::os::unix::fs::FileExt as _;
    let file = placeholder::reopen_writable(&File::open(at).unwrap()).unwrap();
    file.write_all_at(content, 0).unwrap();
    placeholder::write_ctag(&file, "c1").unwrap();
    placeholder::write_state(&file, State::Hydrated).unwrap();
    placeholder::write_stamp(&file).unwrap();
}

/// Renames `docs` to `papers` in the (locked) folder, as a user with
/// their own chmod might while a replacement downloads.
pub(super) fn move_docs_away(root: &Path) {
    let dir = File::open(root).unwrap();
    placeholder::with_owner_write(&dir, || std::fs::rename(root.join("docs"), root.join("papers"))).unwrap();
}

/// A helper that acknowledges everything at once, except the marking of
/// the directory whose path ends in `stall_on`: it says so on the first
/// channel, and acknowledges only when told to on the second.
pub(super) fn stalling_helper(socket_path: &Path, stall_on: &'static str) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let listener = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(listener.as_raw_fd(), &UnixAddr::new(socket_path).unwrap()).unwrap();
    listen(&listener, Backlog::new(4).unwrap()).unwrap();
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: a descriptor `accept` just returned, owned by nothing else.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let _ = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        while let Ok((message, dir)) = channel.recv::<ToHelper>() {
            if let (ToHelper::MarkDir, Some(dir)) = (&message, dir) {
                let at = std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd())).unwrap();
                if at.to_string_lossy().ends_with(stall_on) {
                    let _ = reached_tx.send(());
                    let _ = release_rx.recv();
                }
            }
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });
    (reached_rx, release_tx)
}

/// What a [`recording_helper`] was asked to mark, in order: each
/// directory's item id (None for the holding directory), and how many
/// entries it held right then.
pub(super) type Marks = Arc<std::sync::Mutex<Vec<(Option<String>, usize)>>>;

/// A helper that acknowledges everything, and keeps what it marked.
pub(super) fn recording_helper(socket_path: &Path) -> Marks {
    let listener = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(listener.as_raw_fd(), &UnixAddr::new(socket_path).unwrap()).unwrap();
    listen(&listener, Backlog::new(4).unwrap()).unwrap();
    let marks: Marks = Arc::default();
    let kept = Arc::clone(&marks);
    std::thread::spawn(move || {
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: a descriptor `accept` just returned, owned by nothing else.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let _ = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        while let Ok((message, dir)) = channel.recv::<ToHelper>() {
            if let (ToHelper::MarkDir, Some(dir)) = (&message, dir) {
                let dir = File::from(dir);
                let id = dir.get_xattr(placeholder::XATTR_ITEM_ID).unwrap().map(|v| String::from_utf8(v).unwrap());
                let inside = std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())).unwrap().count();
                kept.lock().unwrap().push((id, inside));
            }
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });
    marks
}

#[tokio::test]
async fn an_initial_listing_fills_the_folder_and_stores_the_link() {
    let s = setup().await;
    let mut states = s.state.subscribe();
    let seen_listing = tokio::spawn(async move {
        loop {
            if states.borrow_and_update().listing {
                return true;
            }
            if states.changed().await.is_err() {
                return false;
            }
        }
    });
    // Slow enough that `listing = true` is still published when the
    // watcher looks.
    Mock::given(method("GET")).and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(200)
            .set_body_json(json!({"value": [root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()],
                                  "@odata.deltaLink": s.link("L1")}))
            .set_delay(Duration::from_millis(300)))
        .up_to_n_times(1)
        .mount(&s.server).await;
    s.listing().cycle(&CancellationToken::new()).await.unwrap();
    assert!(s.root.path.join("docs/f.txt").is_file());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link("L1")));
    let snapshot = s.state.get();
    assert!(!snapshot.listing);
    assert_eq!((snapshot.items_listed, snapshot.items_placed, snapshot.skipped_count), (3, 2, 1));
    assert_eq!(snapshot.sync_trouble, None);
    assert!(tokio::time::timeout(Duration::from_secs(1), seen_listing).await.unwrap().unwrap(), "`listing` was published while it ran");
}

#[tokio::test]
async fn a_later_cycle_applies_only_the_changes() {
    let s = setup().await;
    let listing = listed(&s).await;
    s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(!report.full);
    assert!(s.root.path.join("docs/renamed.txt").is_file());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link("L2")));
}

#[tokio::test]
async fn an_empty_delta_touches_nothing_but_the_link() {
    let s = setup().await;
    let listing = listed(&s).await;
    let ctime = |p: PathBuf| {
        let m = std::fs::metadata(p).unwrap();
        (m.ctime(), m.ctime_nsec())
    };
    let before = ctime(s.root.path.join("docs"));
    s.feed(Some("L1"), json!([]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert_eq!((report.full, report.changes), (false, 0));
    assert_eq!(ctime(s.root.path.join("docs")), before);
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link("L2")));
}

/// A feed that has expired is listed again, and what the new
/// listing no longer has is deleted here.
#[tokio::test]
async fn an_expired_feed_lists_again_and_deletes_what_is_gone() {
    let s = setup().await;
    let listing = listed(&s).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(410))
        .with_priority(1)
        .mount(&s.server).await;
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L9").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full);
    assert!(!s.root.path.join("docs/f.txt").exists());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link("L9")));
}

#[tokio::test]
async fn a_very_large_delta_is_reconciled_in_full() {
    let s = setup().await;
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
    let listing = s.listing_with(2);
    listing.cycle(&CancellationToken::new()).await.unwrap();
    s.feed(Some("L1"), json!([file("A", "D", "a", "c"), file("B", "D", "b", "c"), file("C", "D", "c", "c")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full);
    assert!(s.root.path.join("docs/c").is_file());
}

/// Pins the folder at `rel` in the (locked) folder, as `Pin` does.
pub(super) fn pin_by_hand(root: &Path, rel: &str) {
    placeholder::write_pin(&File::open(root.join(rel)).unwrap()).unwrap();
}

/// A file the cloud adds to a pinned folder is queued for download once
/// it is placed; nothing else is.
#[tokio::test]
async fn a_new_file_placed_in_a_pinned_folder_is_queued() {
    let s = setup().await;
    let listing = listed(&s).await;
    pin_by_hand(&s.root.path, "docs");
    s.feed(Some("L1"), json!([file("G", "D", "g.txt", "c1"), file("H", "R", "h.txt", "c1")]), "L2").await;

    let report = listing.cycle(&CancellationToken::new()).await.unwrap();

    assert!(!report.full);
    assert_eq!(report.applied.pinned, vec![PathBuf::from("docs/g.txt")]);
    assert_eq!(s.pins.queued(), vec![s.root.path.join("docs/g.txt")]);
}

/// After a restart, the first cycle's Full reconcile is followed by the
/// sweep: a pinned file still online-only — its download lost to the
/// restart — is queued again, and the pins are counted.
#[tokio::test]
async fn the_sweep_after_a_restart_queues_a_pinned_file_not_downloaded_yet() {
    let s = setup().await;
    listed(&s).await;
    pin_by_hand(&s.root.path, "docs");
    assert!(s.pins.queued().is_empty());
    s.feed(Some("L1"), json!([]), "L2").await;

    let restarted = s.listing();
    let report = restarted.cycle(&CancellationToken::new()).await.unwrap();

    assert!(report.full);
    assert_eq!(s.pins.queued(), vec![s.root.path.join("docs/f.txt")]);
    assert_eq!(s.state.get().pinned_count, 1);
}

#[tokio::test]
async fn a_new_listing_starts_with_a_full_reconcile() {
    let s = setup().await;
    listed(&s).await;
    Disk::open(&s.root, false).unwrap().unlock_tree().unwrap();
    std::fs::remove_file(s.root.path.join("docs/f.txt")).unwrap();
    s.feed(Some("L1"), json!([]), "L2").await;
    let restarted = s.listing();
    let report = restarted.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full);
    assert!(s.root.path.join("docs/f.txt").is_file(), "the folder was repaired from the tree");
}

#[tokio::test]
async fn another_account_blocks_the_folder_and_touches_nothing() {
    let s = setup().await;
    s.store.call(|t| t.set_meta("drive_id", Some("D0"))).await.unwrap();
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
    // The account hears which drive its token reaches.
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let neighbours = Neighbours {
        claimed: Arc::new(|_| false),
        drive_seen: Arc::new({
            let seen = Arc::clone(&seen);
            move |drive| seen.lock().unwrap().push(drive.to_owned())
        }),
    };
    let listing = Listing::new(ListingContext { neighbours: Some(neighbours), ..s.context() });
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::OtherAccount(_)), "{err:?}");
    assert!(err.blocking());
    assert!(!s.root.path.join("docs").exists());
    assert_eq!(s.state.get().sync_trouble, Some(SyncTrouble { text: err.to_string(), blocking: true }));
    assert_eq!(*seen.lock().unwrap(), vec!["D1".to_owned()]);
}

#[tokio::test]
async fn no_network_is_said_and_is_not_blocking() {
    let s = setup().await;
    let listing = listed(&s).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&s.server).await;
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::Offline(_)), "{err:?}");
    assert!(!err.blocking());
    assert_eq!(s.state.get().sync_trouble, Some(SyncTrouble { text: err.to_string(), blocking: false }));
    s.feed(Some("L1"), json!([]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "a cycle after a failed one reconciles in full (Ruling R7)");
    assert_eq!(s.state.get().sync_trouble, None);
}

#[tokio::test]
async fn a_failed_reconcile_makes_the_next_cycle_full() {
    let s = setup().await;
    let listing = listed(&s).await;
    let root = File::open(&s.root.path).unwrap();
    placeholder::with_owner_write(&root, || root.remove_xattr(XATTR_ROOT)).unwrap();
    s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::Apply(_)), "{err:?}");
    placeholder::with_owner_write(&root, || root.set_xattr(XATTR_ROOT, s.root.root_id.as_bytes())).unwrap();
    s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the stored link was not advanced, and the folder is reconciled in full");
    assert!(s.root.path.join("docs/renamed.txt").is_file());
}

/// A helper's reconnect takes the lifecycle lock for writing before it
/// serves fills again; a listing that takes minutes must not keep it
/// waiting (Z1: opens meanwhile would not be intercepted).
#[tokio::test]
async fn a_cycle_asking_graph_does_not_hold_the_lifecycle_lock() {
    let s = setup().await;
    s.feed_after(None, json!([root_item(), folder("D", "R", "docs")]), "L1", Duration::from_secs(30)).await;
    let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
    let listing = Listing::new(ListingContext { lifecycle: Arc::clone(&lifecycle), ..s.context() });
    let cancel = CancellationToken::new();
    let running = tokio::spawn({
        let (listing, cancel) = (Arc::clone(&listing), cancel.clone());
        async move { listing.cycle(&cancel).await }
    });
    for _ in 0..100 {
        if delta_requests(&s.server).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(delta_requests(&s.server).await, 1, "the listing has asked");
    let writing = tokio::time::timeout(Duration::from_secs(1), lifecycle.write()).await;
    assert!(writing.is_ok(), "a writer is not kept waiting by a cycle asking Graph");
    drop(writing);
    cancel.cancel();
    assert!(matches!(running.await.unwrap(), Err(CycleError::Cancelled)));
}

/// A rescue is one rename, never a copy (ruling): with the
/// preferred rescue directory on another filesystem than the folder, the
/// files go beside the folder instead — and the conflict says so, not
/// where they would have gone. The preferred one here is under `/proc`,
/// which is never the folder's filesystem, and nothing is ever written
/// there.
#[tokio::test]
async fn the_conflict_names_the_directory_the_files_really_went_to() {
    let preferred = PathBuf::from("/proc/konedrive-nonexistent/rescued");
    let s = setup_rescuing_into(preferred.clone()).await;
    let listing = listed(&s).await;
    // A file of the user's own where the cloud now puts one.
    let docs = File::open(s.root.path.join("docs")).unwrap();
    placeholder::with_owner_write(&docs, || std::fs::write(s.root.path.join("docs/new.txt"), b"mine")).unwrap();
    s.feed(Some("L1"), json!([file("N", "D", "new.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();

    let beside = s.root.path.parent().unwrap().join(".konedrive-rescued-OneDrive");
    assert_eq!(report.applied.rescued.len(), 1);
    let kept = &report.applied.rescued[0].rescued;
    assert!(kept.starts_with(&beside), "{}", kept.display());
    assert_eq!(std::fs::read(kept).unwrap(), b"mine");
    let conflicts = konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].rescued, kept.display().to_string());
    assert!(!conflicts[0].rescued.starts_with(&preferred.display().to_string()));
}

/// `conflict` events are capped like every other
/// kind — 50 and "and N more" — while every conflict is still listed.
#[tokio::test]
async fn conflict_events_are_capped_like_the_other_kinds() {
    let s = setup().await;
    let kept = tempfile::tempdir().unwrap();
    let rescued = (0..53)
        .map(|n| {
            let at = kept.path().join(format!("f{n:02}.txt"));
            std::fs::write(&at, b"mine").unwrap();
            Rescued { original: format!("docs/f{n:02}.txt").into(), rescued: at }
        })
        .collect();
    konedrive_tree::off_runtime(|| record(&s.report, &s.store, &s.root.path, &Applied { rescued, ..Applied::default() }, Said::EachChange));

    let folder = s.root.path.display().to_string();
    let events = s.activity();
    assert_eq!(events.iter().filter(|(kind, at, _)| kind == "conflict" && *at != folder).count(), 50);
    assert!(events.contains(&("conflict".to_owned(), folder, "and 3 more".to_owned())), "{events:?}");
    assert_eq!(konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap().len(), 53, "every conflict is still listed");
}

/// A Changed pass that moves a local file out of
/// the way and then hands over to a Full reconcile. The Full pass finds
/// nothing left to rescue, so what the first pass rescued must be the
/// conflict — a row and an event — or it is lost.
#[tokio::test]
async fn a_rescue_made_before_a_full_hand_over_is_still_a_conflict() {
    let s = setup().await;
    let listing = listed(&s).await;
    let root = File::open(&s.root.path).unwrap();
    placeholder::with_owner_write(&root, || std::fs::write(s.root.path.join("top.txt"), b"mine")).unwrap();
    // A new file for `docs`, which is not where the tree has it: the
    // Changed pass rescues `top.txt` first (shallower), then hands over.
    move_docs_away(&s.root.path);
    s.feed(Some("L1"), json!([file("T", "R", "top.txt", "c1"), file("M", "D", "m.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the Changed pass handed over to a Full one");

    let original = s.full("top.txt");
    let rows = konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap();
    assert_eq!(rows.iter().map(|c| c.original.as_str()).collect::<Vec<_>>(), vec![original.as_str()]);
    assert_eq!(std::fs::read(&rows[0].rescued).unwrap(), b"mine");
    assert!(s.activity().contains(&("conflict".to_owned(), original, rows[0].rescued.clone())), "{:?}", s.activity());
    assert_eq!(s.state.get().conflict_count, 1);
}

/// A first listing, and any Full reconcile, is ONE summary
/// event — "N items" — for the whole folder, not one event per item.
#[tokio::test]
async fn a_full_reconcile_is_one_listed_event() {
    let s = setup().await;
    let _first = listed(&s).await;
    let folder = s.root.path.display().to_string();
    assert_eq!(s.activity(), vec![("listed".to_owned(), folder.clone(), "3 items".to_owned())]);
    // A new `Listing` reconciles its first cycle in full, whatever the
    // delta holds: two new files are still one event.
    s.feed(Some("L1"), json!([file("N", "D", "n.txt", "c1"), file("M", "D", "m.txt", "c1")]), "L2").await;
    let report = s.listing().cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full);
    assert_eq!(
        s.activity(),
        vec![("listed".to_owned(), folder.clone(), "3 items".to_owned()), ("listed".to_owned(), folder, "5 items".to_owned())]
    );
}

/// An incremental cycle logs what it did item by item, but
/// at most 50 events of a kind, then one "and N more" for the rest.
#[tokio::test]
async fn an_incremental_cycle_logs_at_most_fifty_of_a_kind_and_how_many_more() {
    let s = setup().await;
    let listing = listed(&s).await;
    let folder = s.root.path.display().to_string();
    let mut items: Vec<Value> = (0..53).map(|n| file(&format!("N{n}"), "D", &format!("n{n:02}.txt"), "c1")).collect();
    items.push(json!({"id": "F", "deleted": {"state": "deleted"}}));
    s.feed(Some("L1"), json!(items), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(!report.full, "the Changed scope is what is capped");
    let events = s.activity().split_off(1);
    let added_each = events.iter().filter(|(kind, at, _)| kind == "added" && *at != folder).count();
    assert_eq!(added_each, 50);
    assert!(events.contains(&("added".to_owned(), folder.clone(), "and 3 more".to_owned())), "{events:?}");
    assert!(events.contains(&("removed".to_owned(), s.full("docs/f.txt"), String::new())), "{events:?}");
    assert_eq!(events.len(), 52, "{events:?}");
}

/// A rescue is a conflict — a row, a `conflict` event saying
/// where the file was and where it is now — and the row drops off by
/// itself once the rescued file is gone.
#[tokio::test]
async fn a_rescue_is_a_conflict_until_its_file_is_gone() {
    let s = setup().await;
    let listing = listed(&s).await;
    let docs = File::open(s.root.path.join("docs")).unwrap();
    placeholder::with_owner_write(&docs, || std::fs::write(s.root.path.join("docs/new.txt"), b"mine")).unwrap();
    s.feed(Some("L1"), json!([file("N", "D", "new.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    let rescued = report.applied.rescued[0].rescued.display().to_string();
    let original = s.full("docs/new.txt");

    let conflicts = konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap();
    let rows: Vec<_> = conflicts.iter().map(|c| (c.original.clone(), c.rescued.clone())).collect();
    assert_eq!(rows, vec![(original.clone(), rescued.clone())]);
    assert_eq!(s.state.get().conflict_count, 1);
    assert_eq!(
        crate::status::snapshot::published_error(&s.state.get()),
        "",
        "a conflict is not a problem: LastError says nothing of it, Conflicts() says it all"
    );
    assert!(s.activity().contains(&("conflict".to_owned(), original, rescued.clone())), "{:?}", s.activity());

    std::fs::remove_file(&rescued).unwrap();
    s.feed(Some("L2"), json!([]), "L3").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    assert_eq!(s.state.get().conflict_count, 0, "a conflict whose file is gone drops off by the next cycle");
    assert!(konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap().is_empty());
}

/// `LastChecked` is when a cycle last succeeded — kept in the
/// store for the next start — and a cycle that fails leaves it alone.
#[tokio::test]
async fn last_checked_moves_only_when_a_cycle_succeeds() {
    let s = setup().await;
    let before = activity::unix_now();
    let listing = listed(&s).await;
    let checked = s.state.get().last_checked;
    assert!(checked >= before, "{checked} < {before}");
    assert_eq!(s.store.call(move |x| x.meta("last_checked")).await.unwrap(), Some(checked.to_string()));

    // Marked, so that a failed cycle writing the time it ran — the same
    // second, most likely — could not pass for leaving it alone.
    s.state.update(|x| x.last_checked = 7);
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D2"})))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.server).await;
    assert!(matches!(listing.cycle(&CancellationToken::new()).await, Err(CycleError::OtherAccount(_))));
    assert_eq!(s.state.get().last_checked, 7, "a failed cycle checked nothing");

    s.feed(Some("L1"), json!([]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(s.state.get().last_checked >= before);
}

/// A file being filled when its change arrives is left for later
/// (`Applied::deferred`); a Changed scope never looks at it again, so the
/// next cycle is a Full one.
#[tokio::test]
async fn a_cycle_that_leaves_a_file_for_later_makes_the_next_one_full() {
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    placeholder::write_state(&File::open(&f_txt).unwrap(), State::Hydrating).unwrap();
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert_eq!(report.applied.deferred, 1);
    // The fill ends without the file: it is online-only again.
    placeholder::write_state(&File::open(&f_txt).unwrap(), State::OnlineOnly).unwrap();
    s.feed(Some("L2"), json!([]), "L3").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the file left for later is looked at again");
    assert_eq!(placeholder::read_ctag(&File::open(&f_txt).unwrap()).unwrap().as_deref(), Some("c2"));
}

/// at every cycle: signing out and in as another account
/// between two cycles stops the folder before anything is placed.
#[tokio::test]
async fn a_sign_in_as_another_account_between_cycles_blocks_the_folder() {
    let s = setup().await;
    let listing = listed(&s).await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D2"})))
        .with_priority(1)
        .mount(&s.server).await;
    s.feed(Some("L1"), json!([folder("N", "R", "new")]), "L2").await;
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::OtherAccount(_)), "{err:?}");
    assert!(err.blocking());
    assert!(!s.root.path.join("new").exists());
}

/// the drive a folder was listed from is kept
/// in `config.toml` as the account's too, so a tree store rebuilt empty —
/// its `meta` has forgotten the drive — still refuses another account.
/// The first cycle writes it there.
#[tokio::test]
async fn the_drive_kept_beside_the_root_outlives_a_rebuilt_store() {
    let s = setup().await;
    let config_dir = tempfile::tempdir().unwrap();
    let config = crate::config::Paths::in_dir(config_dir.path());
    let store = Arc::new(crate::config::ConfigStore::open(&config, async { false }).await);
    let account = store.add_account("Personal").unwrap().id;
    let record = DriveRecord { store: Arc::clone(&store), account: account.clone(), recorded: None };
    listed_with(&s, ListingContext { drive_record: Some(record), ..s.context() }).await;
    assert_eq!(store.account(&account).unwrap().drive_id, "D1", "written by the first cycle");

    let rebuilt = Store::new(TreeStore::in_memory().unwrap());
    let record = DriveRecord { store, account, recorded: Some("D0".into()) };
    let listing = Listing::new(ListingContext { store: rebuilt, drive_record: Some(record), ..s.context() });
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(&err, CycleError::OtherAccount(drive) if drive == "D0"), "{err:?}");
}

/// A drive is one account (design §8.2, review M1): an account with no drive recorded
/// yet, signed in to a drive another account has, does not list it into a second folder
/// — the folder is blocked, naming that account, and nothing is placed.
#[tokio::test]
async fn a_drive_another_account_has_is_not_listed_into_a_second_folder() {
    let s = setup().await;
    let config_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::config::ConfigStore::open(&crate::config::Paths::in_dir(config_dir.path()), async { false }).await);
    let first = store.add_account("Work").unwrap().id;
    store.record_drive(&first, "D1").unwrap();
    let account = store.add_account("Personal").unwrap().id;
    let record = DriveRecord { store: Arc::clone(&store), account: account.clone(), recorded: None };
    let listing = Listing::new(ListingContext { drive_record: Some(record), ..s.context() });

    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();

    assert!(matches!(&err, CycleError::DriveTaken(label) if label == "Work"), "{err:?}");
    assert!(err.blocking());
    assert_eq!(store.account(&account).unwrap().drive_id, "");
    assert!(std::fs::read_dir(&s.root.path).unwrap().next().is_none(), "nothing is placed");
}

/// A cycle whose future is dropped part-way is a failed one: the Full
/// reconcile it had taken is asked for again, and a listing it was
/// running is no longer said to run.
#[tokio::test]
async fn a_dropped_cycle_leaves_a_full_reconcile_and_no_listing_behind() {
    let s = setup().await;
    listed(&s).await;
    let restarted = s.listing();
    // The feed has expired, and the listing that follows is slow.
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(410))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.server).await;
    s.feed_after(None, json!([root_item()]), "L9", Duration::from_secs(30)).await;
    let dropped = tokio::time::timeout(Duration::from_millis(500), restarted.cycle(&CancellationToken::new())).await;
    assert!(dropped.is_err(), "still listing when dropped");
    assert!(!s.state.get().listing, "a dropped listing is not said to run");
    s.feed(Some("L1"), json!([]), "L2").await;
    let report = restarted.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the Full reconcile the dropped cycle had taken is asked for again");
}

/// Cycles of one listing never overlap (a `refresh` and the poller's
/// own, say): the second starts from the link the first left.
#[tokio::test]
async fn two_cycles_at_once_run_one_after_the_other() {
    let s = setup().await;
    let listing = listed(&s).await;
    s.feed_after(Some("L1"), json!([folder("N", "R", "new")]), "L2", Duration::from_millis(300)).await;
    s.feed(Some("L2"), json!([]), "L3").await;
    let token = CancellationToken::new();
    let (first, second) = tokio::join!(listing.cycle(&token), listing.cycle(&token));
    first.unwrap();
    second.unwrap();
    assert!(s.root.path.join("new").is_dir());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link("L3")));
}

/// A cycle dropped while its reconcile runs keeps the lifecycle lock, and
/// its turn, until the reconcile has stopped: a Forget must not take the
/// lock off a folder something is still changing, and no other cycle may
/// rebuild `staging` under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_cycle_keeps_its_locks_until_its_reconcile_stops() {
    let s = setup().await;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-N");
    let link = HelperLink::connect(&socket_path).await.unwrap().0;
    let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
    let listing = Listing::new(ListingContext {
        intercepted: true,
        link: Arc::new(std::sync::Mutex::new(Some(link))),
        lifecycle: Arc::clone(&lifecycle),
        ..s.context()
    });
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    s.feed(Some("L1"), json!([folder("N", "R", "new")]), "L2").await;
    let reached = tokio::task::spawn_blocking(move || reached.recv().unwrap());
    let token = CancellationToken::new();
    tokio::select! {
        _ = listing.cycle(&token) => panic!("the reconcile cannot end before the helper answers"),
        _ = reached => {}
    }
    s.feed(Some("L2"), json!([]), "L3").await;
    let second = tokio::spawn({
        let listing = Arc::clone(&listing);
        async move { listing.cycle(&CancellationToken::new()).await }
    });
    let early = tokio::time::timeout(Duration::from_millis(300), lifecycle.write()).await;
    assert!(early.is_err(), "the lock is held while the folder is still being changed");
    assert!(!second.is_finished(), "no other cycle runs while the dropped one's reconcile does");
    release.send(()).unwrap();
    let report = second.await.unwrap().unwrap();
    assert!(report.full, "the dropped cycle counts as a failed one");
    let later = tokio::time::timeout(Duration::from_secs(5), lifecycle.write()).await;
    assert!(later.is_ok(), "and the lock is let go once the reconcile has stopped");
    drop(later);
    assert!(s.root.path.join("new").is_dir());
}

/// a download cut off by a restart keeps its
/// checkpoint through startup recovery AND through the Full reconcile
/// that the restarted sync's first cycle is. The fill's writes moved the
/// time to now, and that reconcile took the file for a new version and
/// punched the partial download away — 1.5 GB, seconds after login. Now
/// only the time is put back.
#[tokio::test]
async fn a_partial_download_survives_recovery_and_the_full_reconcile_after_it() {
    use std::os::unix::fs::FileExt as _;
    let s = setup().await;
    listed(&s).await;
    let path = s.root.path.join("docs/f.txt");
    {
        let file = placeholder::reopen_writable(&File::open(&path).unwrap()).unwrap();
        placeholder::write_state(&file, State::Hydrating).unwrap();
        file.write_all_at(b"abcd", 0).unwrap();
        placeholder::write_progress(&file, &placeholder::Progress { ctag: "c1".into(), bytes: 4 }).unwrap();
        file.write_all_at(b"ef", 4).unwrap();
    }
    let nowhere = crate::helper::Clearance::NoLink(s.rescue_dir.join("no-helper.sock"));
    let recovered = crate::hydration::recovery::recover(&nowhere, &s.root, &InodeLocks::new()).await.unwrap();
    assert_eq!(recovered.reset, 1, "{recovered:?}");

    s.feed(Some("L1"), json!([]), "L2").await;
    let restarted = s.listing();
    let report = restarted.cycle(&CancellationToken::new()).await.unwrap();

    assert!(report.full, "a restarted sync reconciles in full first");
    let file = File::open(&path).unwrap();
    assert_eq!(placeholder::read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(placeholder::read_progress(&file).unwrap(), Some(placeholder::Progress { ctag: "c1".into(), bytes: 4 }));
    assert_eq!(&std::fs::read(&path).unwrap()[..6], b"abcd\0\0", "the checkpointed bytes stay, the rest is punched");
    assert_eq!(file.metadata().unwrap().mtime(), 1_714_557_600, "the cloud's time is back");
}

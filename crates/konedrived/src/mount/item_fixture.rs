//! The one fixture the tests of the mount over the rows, of the steps and of the plan's
//! task share: a store in a file of a temporary directory (so that a reading connection
//! is a real second connection), a temporary backing directory under no mount, the leaf
//! state, and the fake OneDrive holding what the rows say OneDrive has.
//!
//! Test code. Filled in with step 0, before any piece, so that no piece builds its own
//! "store + backing directory + fake OneDrive": each piece's tests make this fixture and
//! the ONE part they test, and reach every neighbour through the fixture's finished leaf
//! state (`SyncPlanWake`, `PublishedPlan`, `RunningSteps`, `SendingHolds`,
//! `OneDriveSpace`, `MountTakeDown`, `ItemHandles`, `ReadConnections`, `PlaceChanges`,
//! `ItemDownloadStates`, the bridge's queue, the range fetcher), never through another
//! piece's unfinished part. What a piece outside `fuse` cannot make itself the fixture
//! makes for it: a mount that is built and not served (`build_mount`: its
//! `ItemContentFiles`, `ItemFilesInterface` and the parts of the folder steps through the
//! accessors), a content source and
//! a clock the test holds, a `GraphAccess` over the fake OneDrive, a poll handle that
//! counts what it was asked, and a stand-in for the runners of steps.
//!
//! **Two sets of leaves, never mixed.** A mount built by [`ItemFixture::build_mount`] has
//! its OWN write-operation thread, place changes, reading connections, handles, download
//! states, access mode and take-down request (`item_mount::build_order` makes them); the
//! fixture's fields of those names are another set, over the same backing directory and
//! store. A test builds its part over the fixture's leaves, or works through a built
//! mount's accessors, and does not hand one set's part to the other: a place change begun
//! on the fixture's thread through the mount's `PlaceChanges` trips its assertion. What
//! both sets share, and may be mixed: the store, the tree lock, the ignore list,
//! `sync_plan_wake`, `running_steps`, `published_plan`, the content source, the pool, the
//! clock and the take-downs.
//!
//! **Threads.** Every function here may be called from a plain test and from a
//! `#[tokio::test]`: what waits for the store's thread or for the write-operation thread
//! waits on a thread of its own, never on the caller's. The fixture has a runtime of its
//! own, which the parts spawn on ([`ItemFixture::runtime`]): a test's paused clock is not
//! that runtime's.
//!
//! **Rows are made by the store's real operations** — a listing's page, the commit of the
//! step that shows an item, the landing's commit, a local change — and the disk beside
//! them is put as those operations' callers leave it, marks and times included: the check
//! at start, which a mount's build runs, finds nothing to change in what a builder made.
// The surface is for the pieces' tests: not every function has a caller yet.
#![allow(dead_code, unused_imports)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use konedrive_fs::placeholder::{self, State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{
    Change, ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemReader, ItemVersions, Kind, ListedChangeOutcome, ListingLink, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference,
    Placement, Row, StepCommitOutcome, Store, TreeError, TreeStore,
};

use super::access_mode_switch::{AccessMode, AccessModeSwitch};
use super::backing_directory::{restore_write_bits, BackingDirectory};
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::item_inotify_bridge::ItemInotifyBridge;
use super::item_kernel_requests::ItemKernelRequests;
use super::item_mount::{build_order, ItemMount, ItemMountSetup};
use super::item_range_fetcher::ItemRangeFetcher;
use super::mount_take_down::{MountTakeDown, TakeDownCause, TakeDownReport};
use super::place_changes::PlaceChanges;
use super::range_readers::RangeReaders;
use super::read_connections::ReadConnections;
use super::start_check::{check_backing_directory_at_start, StartCheckFindings};
use super::upload_rules::UploadRules;
use super::write_operation_thread::WriteOperationThread;
use crate::account::quota::Quota;
use crate::fake_onedrive::{qx, FakeGraph, FakeItem};
use crate::sync_parts::published_plan::PublishedPlan;
use crate::folder::root::SyncRoot;
use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::sync_plan_wake::SyncPlanWake;
use crate::content_source::source::{ContentSource, Share, Version};
use crate::local::{IgnoreList, SharedIgnore};
use crate::listing::listing_poller::ListingPollHandle;
use crate::status::report::Report;
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};
use crate::status::throttle_wait::ThrottleWait;
use crate::sync_plan_task::step_runners::StepRunners;
use crate::sync_parts::onedrive_space::OneDriveSpace;
use crate::onedrive_steps::GraphAccess;
use crate::sync_parts::sending_holds::SendingHolds;

/// The content source of the fixture.
mod content_source;
/// The rules of the rows, as the store's own tests ask them.
mod row_rules;
/// The stand-in for the runners of steps.
mod step_stand_in;
mod tests;

pub(crate) use content_source::{ContentFetch, ContentRefusal, FixtureContentSource};
pub(crate) use step_stand_in::make_stand_in_hash;

/// The drive the fixture's store knows.
pub(crate) const DRIVE_ID: &str = "d1";
/// The id of the drive's root: the fake OneDrive's.
pub(crate) const ROOT_ONEDRIVE_ID: &str = crate::fake_onedrive::ROOT;
/// Unix seconds: the time OneDrive says every file the fixture lists was modified at.
pub(crate) const ONEDRIVE_MTIME: i64 = 1_600_000_000;
/// Unix seconds: what the fixture's clock answers until the test sets another.
pub(crate) const START_TIME: i64 = 1_700_000_000;
/// `[transfers] download_after` of the fixture's download states and of a built mount.
pub(crate) const DOWNLOAD_AFTER: u64 = 1 << 20;
/// The names the fixture's folder never uploads.
pub(crate) const IGNORED_NAMES: [&str; 2] = ["*.tmp", "cache"];

/// The marks an entry of the backing directory carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskMarksRead {
    /// The OneDrive item id it is marked with.
    pub(crate) onedrive_item_id: Option<String>,
    /// Whether it is marked as a whole downloaded copy.
    pub(crate) is_whole: bool,
    /// The cTag it is marked as downloaded with.
    pub(crate) ctag: Option<String>,
}

/// One job of a thread the fixture runs jobs on.
type ThreadJob = Box<dyn FnOnce() + Send>;

/// A thread like one of the mount's own: not the write-operation thread and no thread of
/// a runtime, and the same one for every job, so that it has one reading connection.
struct MountLikeThread {
    jobs: Mutex<Option<mpsc::Sender<ThreadJob>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl MountLikeThread {
    fn start() -> Self {
        let (jobs, queued) = mpsc::channel::<ThreadJob>();
        let thread = std::thread::Builder::new().name("fixture-mount-like".into()).spawn(move || queued.into_iter().for_each(|job| job())).expect("the fixture's mount-like thread starts");
        MountLikeThread { jobs: Mutex::new(Some(jobs)), thread: Mutex::new(Some(thread)) }
    }

    fn run(&self, job: ThreadJob) {
        crate::panic::lock(&self.jobs).as_ref().expect("the fixture's mount-like thread runs").send(job).expect("the fixture's mount-like thread runs");
    }

    fn stop(&self) {
        drop(crate::panic::lock(&self.jobs).take());
        if let Some(thread) = crate::panic::lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

/// The fake OneDrive's answer to `GET me/drive` with the fixture's drive in it: the
/// fake's own says the drive `D`, which is not the store's.
struct FixtureDriveAnswer {
    cloud: Arc<Mutex<crate::fake_onedrive::Cloud>>,
}

impl wiremock::Respond for FixtureDriveAnswer {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let mut cloud = crate::panic::lock(&self.cloud);
        cloud.log.push(("GET".to_owned(), "me/drive".to_owned()));
        wiremock::ResponseTemplate::new(200).set_body_json(match cloud.free {
            Some(free) => serde_json::json!({ "id": DRIVE_ID, "quota": { "remaining": free, "state": if free == 0 { "exceeded" } else { "normal" } } }),
            None => serde_json::json!({ "id": DRIVE_ID }),
        })
    }
}

/// Runs `job` on a thread of its own and waits for it: what waits for the store's thread
/// (`Store::call_blocking`) must not do so on a thread of a runtime, and the caller may
/// be one. A panic of the job is the caller's.
fn wait_off_runtime<T: Send>(job: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(job).join()).unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Has `run` run `job` (wrapped so that a panic of it comes back whole) and waits for
/// what it gives; a panic of the job is the caller's, and the thread goes on.
fn run_and_wait<T: Send + 'static>(run: impl FnOnce(ThreadJob), job: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, waited) = mpsc::channel();
    run(Box::new(move || drop(done.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))))));
    match waited.recv() {
        Ok(Ok(answer)) => answer,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("the job was dropped: its thread is stopped"),
    }
}

/// The OneDrive item id the fixture gives the item at `path` of the drive: the root's for
/// the empty path, and otherwise the path itself with `!` between its names. One path,
/// one id: an item listed again at its path is the same item.
pub(crate) fn onedrive_id_of(path: &str) -> String {
    match path {
        "" => ROOT_ONEDRIVE_ID.to_owned(),
        path => path.replace('/', "!"),
    }
}

fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// A folder with no mount, no kernel and no daemon.
pub(crate) struct ItemFixture {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) store: Store,
    pub(crate) store_path: PathBuf,
    pub(crate) tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list ([`IGNORED_NAMES`]): the upload rules' and a built
    /// mount's.
    pub(crate) ignore: SharedIgnore,
    pub(crate) backing_directory: Arc<BackingDirectory>,
    pub(crate) upload_rules: Arc<UploadRules>,
    /// The access mode of the fixture's own leaves: read-write from the start (a built
    /// mount starts read-only, as every mount does).
    pub(in crate::mount) access_mode_switch: Arc<AccessModeSwitch>,
    pub(crate) write_operation_thread: Arc<WriteOperationThread>,
    pub(crate) place_changes: Arc<PlaceChanges>,
    pub(in crate::mount) read_connections: Arc<ReadConnections>,
    pub(crate) download_states: Arc<ItemDownloadStates>,
    /// The bridge's queue; its thread is not started.
    pub(crate) inotify_bridge: Arc<ItemInotifyBridge>,
    pub(crate) range_fetcher: Arc<ItemRangeFetcher>,
    pub(crate) item_handles: Arc<ItemHandles>,
    pub(crate) mount_take_down: Arc<MountTakeDown>,
    pub(crate) sync_plan_wake: Arc<SyncPlanWake>,
    pub(crate) running_steps: Arc<RunningSteps>,
    pub(crate) published_plan: Arc<PublishedPlan>,
    /// As the folder's start makes them: the first listing has not ended, which holds
    /// what is sent until the test says otherwise (`SendingHolds::set_listing_ended`).
    pub(crate) sending_holds: Arc<SendingHolds>,
    /// Over a quota of its own, never read: OneDrive is not full.
    pub(crate) onedrive_space: Arc<OneDriveSpace>,
    pub(crate) pool: Arc<TransferPool>,
    /// The folder's published state, as far as the fixture's leaves read it: whether
    /// OneDrive asks the account to wait (`start_throttle_wait`). Nothing publishes it.
    pub(crate) sync_state: SyncStateHandle,
    pub(crate) split_download_share: Arc<Share>,
    /// Unix seconds: what `clock()` answers; the test sets it.
    pub(crate) now: Arc<AtomicI64>,
    /// The take-downs asked for, as the report was given them: by the fixture's own
    /// `mount_take_down` and by a built mount's.
    pub(crate) take_downs: Arc<Mutex<Vec<TakeDownCause>>>,
    content_source: Arc<FixtureContentSource>,
    /// How many versions of items the fixture listed: what an eTag is made of.
    listed_version_count: AtomicU64,
    /// `None` once the fixture is dropped.
    runtime: Option<tokio::runtime::Runtime>,
    mount_like_thread: MountLikeThread,
}

impl ItemFixture {
    /// A folder whose store knows its drive ([`DRIVE_ID`]) and whose root a full listing
    /// told of and ended ([`ROOT_ONEDRIVE_ID`]), with nothing in it, on disk or in the
    /// rows.
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let (folder, store_path) = (directory.path().join("folder"), directory.path().join("tree.db"));
        fs::create_dir(&folder).expect("the folder");
        let mut tree_store = TreeStore::open(&store_path).expect("the store");
        tree_store.set_drive_id(DRIVE_ID).expect("the drive");
        let listing = tree_store.item_store().begin_full_listing().expect("a full listing");
        let root = Change::Root(Row { id: ROOT_ONEDRIVE_ID.into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: Some("e-R".into()), ctag: None, quickxor: None, mime: None, placement: Placement::Placed });
        let listed = tree_store.item_store().write_listed_changes(listing.id, &[root], listing.began_at_count, &ListingLink::DeltaLink("link-0".into())).expect("the root is listed");
        assert!(listed.is_link_written(), "the first listing ended");
        let store = Store::new(tree_store);

        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("the fixture's runtime");
        let tree_lock = Arc::new(tokio::sync::Mutex::new(()));
        let ignore = IgnoreList::new(IGNORED_NAMES).shared();
        let backing_directory = Arc::new(BackingDirectory::open(&folder).expect("the backing directory"));
        let upload_rules = Arc::new(UploadRules::new(Arc::clone(&ignore)));
        let access_mode_switch = Arc::new(AccessModeSwitch::new(AccessMode::ReadWrite));
        let take_downs = Arc::new(Mutex::new(Vec::new()));
        let mount_take_down = Arc::new(MountTakeDown::new(Arc::clone(&access_mode_switch), make_take_down_report(&take_downs)));
        let write_operation_thread = Arc::new(WriteOperationThread::start(Arc::clone(&tree_lock)).expect("the write-operation thread"));
        let place_changes = Arc::new(PlaceChanges::new(Arc::clone(&backing_directory), write_operation_thread.thread_id()));
        let content_source = Arc::new(FixtureContentSource::default());
        let pool = TransferPool::new(4);
        let range_readers = Arc::new(RangeReaders::new(Arc::clone(&pool), runtime.handle().clone()));
        let sync_state = SyncStateHandle::new(SyncSnapshot::default());
        let range_fetcher = Arc::new(ItemRangeFetcher::new(range_readers, Arc::clone(&content_source) as Arc<dyn ContentSource>, ThrottleWait::new(sync_state.clone())));
        let sync_plan_wake = Arc::new(SyncPlanWake::new());
        ItemFixture {
            store,
            tree_lock,
            ignore,
            backing_directory,
            upload_rules,
            access_mode_switch,
            write_operation_thread,
            place_changes,
            read_connections: Arc::new(ReadConnections::new(store_path.clone())),
            download_states: Arc::new(ItemDownloadStates::new(DOWNLOAD_AFTER)),
            inotify_bridge: Arc::new(ItemInotifyBridge::new(folder)),
            range_fetcher,
            item_handles: Arc::new(ItemHandles::new()),
            mount_take_down,
            sending_holds: Arc::new(SendingHolds::new(Arc::clone(&sync_plan_wake))),
            sync_plan_wake,
            running_steps: Arc::new(RunningSteps::new()),
            published_plan: Arc::new(PublishedPlan::new()),
            onedrive_space: Arc::new(OneDriveSpace::new(Quota::detached())),
            pool,
            sync_state,
            split_download_share: Share::new(),
            now: Arc::new(AtomicI64::new(START_TIME)),
            take_downs,
            content_source,
            listed_version_count: AtomicU64::new(0),
            runtime: Some(runtime),
            mount_like_thread: MountLikeThread::start(),
            store_path,
            directory,
        }
    }

    // What the parts are made with.

    /// The folder's path: the backing directory, which no mount lies over.
    pub(crate) fn folder(&self) -> PathBuf {
        self.directory.path().join("folder")
    }

    /// The fixture's own runtime, which the parts are given to spawn on and a plain test
    /// may `block_on`. A `#[tokio::test]` awaits on its own runtime instead: `block_on`
    /// panics on a thread of a runtime.
    pub(crate) fn runtime(&self) -> tokio::runtime::Handle {
        self.runtime.as_ref().expect("the fixture lives").handle().clone()
    }

    /// The drive of the folder.
    pub(crate) fn drive_id(&self) -> DriveId {
        DriveId::new(DRIVE_ID).expect("a drive id")
    }

    /// The clock every part is made with: answers `now`.
    pub(crate) fn clock(&self) -> Arc<dyn Fn() -> i64 + Send + Sync> {
        let now = Arc::clone(&self.now);
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    /// Who a take-down is reported to: it is kept in `take_downs`.
    pub(crate) fn take_down_report(&self) -> TakeDownReport {
        make_take_down_report(&self.take_downs)
    }

    /// The content source: it serves the bytes every listed file was listed with
    /// ([`list_onedrive_file`](Self::list_onedrive_file)) or the test gave since
    /// ([`serve_content`](Self::serve_content)), and refuses as the test asks
    /// ([`refuse_content`](Self::refuse_content)).
    pub(crate) fn content_source(&self) -> Arc<dyn ContentSource> {
        Arc::clone(&self.content_source) as Arc<dyn ContentSource>
    }

    /// The same source, for what a test asks of it: the fetches it was asked for, the
    /// items it was told to forget.
    pub(crate) fn fixture_content_source(&self) -> &Arc<FixtureContentSource> {
        &self.content_source
    }

    /// The content source serves `content` for the OneDrive item of `item` from now on,
    /// as the content OneDrive would tell for these bytes
    /// ([`onedrive_content_of`](Self::onedrive_content_of)); the rows are not touched.
    pub(crate) fn serve_content(&self, item: LocalItemId, content: &[u8]) {
        let onedrive_item = self.find_item(item).and_then(|row| row.onedrive_item).unwrap_or_else(|| panic!("item {item} has no binding: nothing of OneDrive to serve"));
        self.serve_onedrive_content(onedrive_item.item_id.as_str(), content);
    }

    /// The content source answers `refusal` for the OneDrive item of `item` from now on;
    /// `None` takes the refusal away.
    pub(crate) fn refuse_content(&self, item: LocalItemId, refusal: Option<ContentRefusal>) {
        let onedrive_item = self.find_item(item).and_then(|row| row.onedrive_item).unwrap_or_else(|| panic!("item {item} has no binding: nothing of OneDrive to refuse"));
        self.content_source.refuse(onedrive_item.item_id.as_str(), refusal);
    }

    fn serve_onedrive_content(&self, onedrive_item_id: &str, content: &[u8]) {
        let mut hash = konedrive_graph::quickxor::QuickXor::new();
        hash.update(content);
        let version = Version { ctag: format!("c-{}", qx(content)), quick_xor: Some(hash.finish()) };
        self.content_source.serve(onedrive_item_id, content, ONEDRIVE_MTIME, Some(version));
    }

    /// The fake OneDrive (wiremock) with the drive the fixture's store knows, holding
    /// what the rows say OneDrive has now, each file with the bytes the content source
    /// serves for it: for the steps towards OneDrive and the listing. Its `cloud` is the
    /// test's to look at and to script.
    ///
    /// `GET me/drive` answers the store's drive ([`DRIVE_ID`]) with the fake's own quota
    /// (`cloud.free`), and is in `cloud.log`; what a test scripts, throttles or delays for
    /// that one request is not applied to it. The store's delta link is the fake's from
    /// here on, unless a listing is under way or the store has none: a listing cycle over
    /// the fixture asks the changes made in `cloud` after this call.
    pub(crate) async fn start_fake_onedrive(&self) -> FakeGraph {
        let graph = FakeGraph::start().await;
        let items = self.store.call(|store| store.item_store().list_items()).await.expect("the rows");
        let onedrive_id_of_row = |id: LocalItemId| items.iter().find(|item| item.local_item_id == id).and_then(|item| item.onedrive_item.as_ref()).map(|reference| reference.item_id.as_str().to_owned());
        graph.with(|cloud| {
            for item in &items {
                let (Some(reference), Some(onedrive)) = (&item.onedrive_item, &item.onedrive) else { continue };
                // The root is in the fake drive already.
                let Some(place) = &onedrive.place else { continue };
                let id = reference.item_id.as_str().to_owned();
                let (hash, ctag) = match &onedrive.content {
                    Some(ContentVersion::Hashed { hash, ctag, .. }) => (hash.clone(), ctag.clone()),
                    _ => (None, None),
                };
                let size = onedrive.content.as_ref().map_or(0, ContentVersion::size);
                let content = self.content_source.find_served_bytes(&id).unwrap_or_else(|| vec![0; size as usize]);
                cloud.add(FakeItem {
                    parent: onedrive_id_of_row(place.parent_local_item_id),
                    name: place.name.to_string_lossy().into_owned(),
                    folder: item.kind == ItemKind::Directory,
                    content,
                    hash,
                    size,
                    etag: item.onedrive_etag.clone().unwrap_or_else(|| format!("e-{id}")),
                    ctag: ctag.unwrap_or_else(|| format!("c-{id}")),
                    mtime: onedrive.content.as_ref().map_or(0, |content| content.mtime().0),
                    id,
                });
            }
        });
        wiremock::Mock::given(wiremock::matchers::method("GET")).and(wiremock::matchers::path("/me/drive")).respond_with(FixtureDriveAnswer { cloud: Arc::clone(&graph.cloud) }).mount(&graph.server).await;
        let delta_link = graph.with(|cloud| cloud.delta_body())["@odata.deltaLink"].as_str().expect("the fake's delta link").to_owned();
        self.write_rows(move |store| {
            if store.delta_link()?.is_some() && store.item_store().find_listing_under_way()?.is_none() {
                let listing = store.item_store().begin_delta_listing()?;
                store.item_store().write_listed_changes(listing.id, &[], listing.began_at_count, &ListingLink::DeltaLink(delta_link))?;
            }
            Ok(())
        });
        graph
    }

    /// What every part that asks Graph is made with, over `fake_onedrive`, with the
    /// fixture's store, drive and clock.
    pub(crate) fn graph_access(&self, fake_onedrive: &FakeGraph) -> GraphAccess {
        GraphAccess { drive: fake_onedrive.client(), drive_id: self.drive_id(), store: self.store.clone(), clock: self.clock() }
    }

    /// The local contents of the folder as a step towards OneDrive reads and marks them,
    /// over the fixture's own leaves (its place changes and handles): what a runner of
    /// steps is made with in a test that builds no mount.
    pub(crate) fn make_content_files(&self) -> Arc<super::item_content_files::ItemContentFiles> {
        Arc::new(super::item_content_files::ItemContentFiles::new(Arc::clone(&self.place_changes), self.store_path.clone(), Arc::clone(&self.item_handles)))
    }

    /// A poll handle with no poller behind it (`ListingPollHandle::for_tests`), and how
    /// many cycles it was asked for.
    pub(crate) fn poll_handle(&self) -> (ListingPollHandle, Arc<AtomicUsize>) {
        ListingPollHandle::for_tests()
    }

    /// A stand-in for the runners of steps, as the plan's task takes them
    /// (`sync_plan_task::step_runners::StepRunners`): each step is committed by its own
    /// commit of `step_commit`, under the tree lock, with answers made up as OneDrive
    /// would give them from what the rows say it has (a new id, a fresh eTag, the content
    /// sent; the name taken where another item of OneDrive has it), and nothing on disk:
    /// after a `DownloadContent` it ran, the row says on disk over no file. Written here,
    /// in the fixture (`item_fixture::step_stand_in`); the convergence test's stand-in in
    /// `konedrive-tree` is its model, not its code.
    pub(crate) fn step_runners_stand_in(&self) -> StepRunners {
        step_stand_in::StepStandIn::new(self.store.clone(), Arc::clone(&self.tree_lock), Arc::clone(&self.upload_rules)).into_step_runners()
    }

    /// A report that goes nowhere but for the wait OneDrive asked for, which is the
    /// fixture's (`sync_state`): what a downloader over the fixture's leaves is made
    /// with when a test starts a wait.
    pub(crate) fn report(&self) -> Report {
        Report { throttle_wait: ThrottleWait::new(self.sync_state.clone()), ..Report::nowhere() }
    }

    /// OneDrive asks the account to wait, and `seconds_left` of the wait are left, as the
    /// pool would publish it; 0 ends the wait.
    pub(crate) fn start_throttle_wait(&self, seconds_left: u32) {
        self.sync_state.set_throughput(konedrive_graph::pool::Throughput { retry_after: seconds_left, ..Default::default() });
    }

    /// What a mount over the fixture's folder is made with: the fixture's store, tree
    /// lock, ignore list, content source, pool, clock, and the leaf state the folder's
    /// start hands a mount (`sync_plan_wake`, `running_steps`, `published_plan`, the
    /// report into `take_downs`).
    pub(crate) fn mount_setup(&self) -> ItemMountSetup {
        ItemMountSetup {
            root: SyncRoot { path: self.folder(), root_id: ROOT_ONEDRIVE_ID.into() },
            store_path: self.store_path.clone(),
            store: self.store.clone(),
            drive_id: self.drive_id(),
            source: self.content_source(),
            report: self.report(),
            pool: Arc::clone(&self.pool),
            split_download_share: Arc::clone(&self.split_download_share),
            download_after: DOWNLOAD_AFTER,
            tree_lock: Arc::clone(&self.tree_lock),
            ignore: Arc::clone(&self.ignore),
            sync_plan_wake: Arc::clone(&self.sync_plan_wake),
            running_steps: Arc::clone(&self.running_steps),
            published_plan: Arc::clone(&self.published_plan),
            take_down_report: self.take_down_report(),
            clock: self.clock(),
        }
    }

    /// A mount over the fixture's folder, built through `build_order::build` with
    /// [`mount_setup`](Self::mount_setup), and NOT served: its accessors give the parts a
    /// piece outside `fuse` is tested with or over. The build runs the check at start
    /// over the rows and the disk as they are now, and leaves the mount read-only. Its
    /// leaves are its own, not the fixture's (the module's doc). A mount that is stopped
    /// or dropped leaves the folder on disk without its write bits: a builder that puts
    /// something on disk is called before that, or a mount is built again.
    pub(crate) fn build_mount(&self) -> Arc<ItemMount> {
        Arc::new(self.build_mount_with_kernel_requests().0)
    }

    /// As [`build_mount`](Self::build_mount), with what answers the kernel's requests over
    /// the mount's own parts: for the tests of the requests.
    pub(in crate::mount) fn build_mount_with_kernel_requests(&self) -> (ItemMount, ItemKernelRequests) {
        self.try_build_mount().unwrap_or_else(|error| panic!("the mount is built: {error:?}"))
    }

    /// The build itself, with its error.
    pub(in crate::mount) fn try_build_mount(&self) -> Result<(ItemMount, ItemKernelRequests), super::item_mount::ItemMountError> {
        let (setup, runtime) = (self.mount_setup(), self.runtime());
        wait_off_runtime(move || build_order::build(setup, runtime))
    }

    /// Runs the check at start over the fixture's folder and rows, as a mount's build
    /// does, with no mount built.
    pub(crate) fn run_start_check(&self) -> StartCheckFindings {
        let (backing_directory, upload_rules, drive_id, store) = (Arc::clone(&self.backing_directory), Arc::clone(&self.upload_rules), self.drive_id(), &self.store);
        wait_off_runtime(move || check_backing_directory_at_start(backing_directory, upload_rules, drive_id, store)).expect("the check at start")
    }

    // Threads.

    /// Runs `job` on the fixture's write-operation thread, under the tree lock, after
    /// every job it was given before, and waits for what it gives: where a place change
    /// of the fixture's `place_changes` may begin. A panic of the job is the caller's, and
    /// the thread goes on. The caller must not hold the tree lock.
    pub(crate) fn run_on_write_operation_thread<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> T {
        run_and_wait(|job| self.write_operation_thread.run(job), job)
    }

    /// Runs `job` on the fixture's mount-like thread and waits for what it gives: a thread
    /// that is not the write-operation thread and no thread of a runtime, as a thread of
    /// the mount is — where `PlaceChanges::open_at_row_path` and a read through
    /// `ReadConnections` belong, and the same thread at every call, so with one reading
    /// connection. A panic of the job is the caller's.
    pub(crate) fn run_on_mount_like_thread<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> T {
        run_and_wait(|job| self.mount_like_thread.run(job), job)
    }

    /// Runs `job` as one job of the store's thread, over the writer, and waits for it; an
    /// error of the store is a panic. From any thread, a runtime's among them.
    pub(crate) fn write_rows<T: Send + 'static>(&self, job: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> T {
        let store = &self.store;
        wait_off_runtime(move || store.call_blocking(job)).unwrap_or_else(|error| panic!("the store's job failed: {error}"))
    }

    /// Runs `read` over the rows in one read transaction of the store's reading
    /// connection (`Store::read_items_blocking`), and waits for it. From any thread.
    pub(crate) fn read_rows<T: Send + 'static>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError> + Send + 'static) -> T {
        let store = &self.store;
        wait_off_runtime(move || store.read_items_blocking(read)).unwrap_or_else(|error| panic!("the read of the rows failed: {error}"))
    }

    /// Runs `read` through the fixture's `read_connections` on the mount-like thread, as
    /// a lookup of the mount reads.
    pub(in crate::mount) fn read_through_read_connection<T: Send + 'static>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        let read_connections = Arc::clone(&self.read_connections);
        self.run_on_mount_like_thread(move || read_connections.read(read))
    }

    // The rows, made through the store's operations, and the disk beside them.

    /// The content version OneDrive would tell for `content`: its size, a fixed time
    /// ([`ONEDRIVE_MTIME`]), its QuickXorHash and a cTag made of it.
    pub(crate) fn onedrive_content_of(content: &[u8]) -> ContentVersion {
        let hash = qx(content);
        ContentVersion::Hashed { size: content.len() as u64, mtime: ONEDRIVE_MTIME, mtime_nsec: 0, ctag: Some(format!("c-{hash}")), hash: Some(hash) }
    }

    /// The reference of the OneDrive item the fixture lists at `path`.
    pub(crate) fn onedrive_reference_of(path: &str) -> OneDriveItemReference {
        OneDriveItemReference { drive_id: DriveId::new(DRIVE_ID).expect("a drive id"), item_id: OneDriveItemId::new(onedrive_id_of(path)) }
    }

    /// A listing of changes that begins now, says `changes` in one page and ends: what a
    /// cycle of the delta listing writes. Every change must be written whole. It ends
    /// with the delta link the store had, which stays the store's.
    pub(crate) fn write_listing(&self, changes: Vec<Change>) {
        let listed = self.write_rows(move |store| {
            let link = store.delta_link()?.unwrap_or_else(|| "link-0".to_owned());
            let listing = store.item_store().begin_delta_listing()?;
            store.item_store().write_listed_changes(listing.id, &changes, listing.began_at_count, &ListingLink::DeltaLink(link))
        });
        assert!(listed.is_link_written(), "the listing left something to read again: {listed:?}");
        assert!(listed.changes.iter().all(|change| *change == ListedChangeOutcome::Written), "the listing was not written whole: {listed:?}");
    }

    fn make_listed_row(&self, path: &str, kind: Kind, content: Option<&[u8]>) -> Row {
        let (parent, name) = split_path(path);
        let version = self.listed_version_count.fetch_add(1, Ordering::SeqCst) + 1;
        let hash = content.map(qx);
        Row {
            id: onedrive_id_of(path),
            parent_id: Some(onedrive_id_of(parent)),
            name: name.to_owned(),
            kind,
            size: content.map_or(0, |content| content.len() as u64),
            mtime: ONEDRIVE_MTIME,
            etag: Some(format!("e-{}-{version}", onedrive_id_of(path))),
            ctag: hash.as_ref().map(|hash| format!("c-{hash}")),
            quickxor: hash,
            mime: None,
            placement: Placement::Placed,
        }
    }

    fn find_listed(&self, path: &str) -> Item {
        let reference = Self::onedrive_reference_of(path);
        self.read_rows(move |reader| reader.find_bound_item(&reference)).unwrap_or_else(|| panic!("the item OneDrive has at {path} has no row"))
    }

    /// OneDrive has a file at `path` with `content` (the directory it is in was listed
    /// before): a listing says so, and the folder does not show it — an item new in
    /// OneDrive. Called again for the same path, OneDrive changed the file's content:
    /// what the folder shows stays. The content source serves `content` for it. Its row.
    pub(crate) fn list_onedrive_file(&self, path: &str, content: &[u8]) -> Item {
        self.write_listing(vec![Change::Upsert(self.make_listed_row(path, Kind::File, Some(content)))]);
        self.serve_onedrive_content(&onedrive_id_of(path), content);
        self.find_listed(path)
    }

    /// As [`list_onedrive_file`](Self::list_onedrive_file) for a file the folder cannot
    /// show, for `reason`: no step ever shows it.
    pub(crate) fn list_onedrive_file_not_placed(&self, path: &str, content: &[u8], reason: konedrive_tree::SkipReason) -> Item {
        self.write_listing(vec![Change::Upsert(Row { placement: Placement::Skipped(reason), ..self.make_listed_row(path, Kind::File, Some(content)) })]);
        self.find_listed(path)
    }

    /// As [`list_onedrive_file`](Self::list_onedrive_file) for a directory.
    pub(crate) fn list_onedrive_directory(&self, path: &str) -> Item {
        self.write_listing(vec![Change::Upsert(self.make_listed_row(path, Kind::Folder, None))]);
        self.find_listed(path)
    }

    /// A listing says the item OneDrive had at `from` is at `to` now (the directory `to` is
    /// in was listed before), with the content it had. The item keeps the id of the path
    /// it was first listed at: [`onedrive_id_of`] of `from`, not of `to`. Its row.
    pub(crate) fn list_onedrive_move(&self, from: &str, to: &str) -> Item {
        let listed = self.find_listed(from);
        let (parent, name) = split_path(to);
        let moved = self.make_listed_row(from, if listed.kind == ItemKind::Directory { Kind::Folder } else { Kind::File }, None);
        let (size, quickxor, ctag) = match listed.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref()) {
            Some(ContentVersion::Hashed { size, hash, ctag, .. }) => (*size, hash.clone(), ctag.clone()),
            _ => (0, None, None),
        };
        self.write_listing(vec![Change::Upsert(Row { parent_id: Some(onedrive_id_of(parent)), name: name.to_owned(), size, quickxor, ctag, ..moved })]);
        self.find_listed(from)
    }

    /// A listing says OneDrive does not have the item it had at `path` any more.
    pub(crate) fn list_onedrive_removal(&self, path: &str) {
        self.write_listing(vec![Change::Delete(onedrive_id_of(path))]);
    }

    /// The folder shows the item of `listed` as OneDrive has it: the commit of the step
    /// that shows it (`commit_show_item`). The directory it is in must be shown. Its row.
    pub(crate) fn show_listed_item(&self, listed: &Item) -> Item {
        let (versions, id) = (ItemVersions::of(listed), listed.local_item_id);
        let name = listed.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("an item OneDrive has").name;
        let is_name_ignored = self.upload_rules.is_name_ignored(&name);
        let shown = self.write_rows(move |store| store.item_store().commit_show_item(&versions, is_name_ignored));
        assert_eq!(shown, StepCommitOutcome::Written, "the folder shows item {id}: is the directory it is in shown?");
        self.find_item(id).expect("the shown item's row")
    }

    /// Lists a file of OneDrive at `path` with `content` into the rows
    /// ([`list_onedrive_file`](Self::list_onedrive_file)) and shows it
    /// ([`show_listed_item`](Self::show_listed_item)): synced, its content not on disk,
    /// nothing in the backing directory. Its row.
    pub(crate) fn show_onedrive_file(&self, path: &str, content: &[u8]) -> Item {
        let listed = self.list_onedrive_file(path, content);
        self.show_listed_item(&listed)
    }

    /// As [`show_onedrive_file`](Self::show_onedrive_file) for a directory: shown, with
    /// nothing in the backing directory ([`make_directory_on_disk`](Self::make_directory_on_disk)
    /// puts it there).
    pub(crate) fn show_onedrive_directory(&self, path: &str) -> Item {
        let listed = self.list_onedrive_directory(path);
        self.show_listed_item(&listed)
    }

    /// The path of `item` in the folder and the marks of the directories on it
    /// (`ItemReader::find_path_with_marks`); a panic for an item the folder does not show.
    fn find_path_with_marks(&self, item: LocalItemId) -> (PathBuf, Vec<Option<OneDriveItemId>>) {
        self.read_rows(move |reader| reader.find_path_with_marks(item)).unwrap_or_else(|| panic!("the folder does not show item {item}: it has no path"))
    }

    /// Makes on disk every directory of the first `count` names of `path` that is not
    /// there, and marks each with the item id `marks` has for it, as a place change does
    /// for the directories above a place.
    fn make_marked_directories(&self, path: &Path, marks: &[Option<OneDriveItemId>], count: usize) {
        let mut at = self.folder();
        for (name, mark) in path.components().zip(marks).take(count) {
            at.push(name);
            if !at.is_dir() {
                fs::create_dir(&at).unwrap_or_else(|error| panic!("the directory {} is made: {error}", at.display()));
            }
            if let Some(mark) = mark {
                xattr::set(&at, XATTR_ITEM_ID, mark.as_str().as_bytes()).expect("the directory's mark");
            }
        }
    }

    /// Puts the directory `item`, which the folder shows, on disk with the directories
    /// above it, each marked with the id of its binding: an agreed directory that is in
    /// the backing directory.
    pub(crate) fn make_directory_on_disk(&self, item: LocalItemId) {
        let (path, marks) = self.find_path_with_marks(item);
        self.make_marked_directories(&path, &marks, marks.len());
    }

    /// Puts the whole copy of the shown file `item` on disk — in its directory, made and
    /// marked where it was not there, with the marks of a downloaded copy (the item id,
    /// the whole state, the cTag) and the time of the content — and says so in its row,
    /// as a landing does (`commit_landed_content`): an agreed file with a clean copy.
    /// `content` must be the bytes of the row's local content, by its size.
    pub(crate) fn land_copy(&self, item: LocalItemId, content: &[u8]) {
        let row = self.find_item(item).unwrap_or_else(|| panic!("item {item} has no row"));
        let downloaded = row.local.as_ref().and_then(|local| local.content.clone()).unwrap_or_else(|| panic!("item {item} is no file the folder shows"));
        let binding = row.onedrive_item.as_ref().unwrap_or_else(|| panic!("item {item} has no binding: nothing was downloaded for it"));
        assert_eq!(content.len() as u64, downloaded.size(), "the copy of item {item} has the size its row says");
        let (path, marks) = self.find_path_with_marks(item);
        self.make_marked_directories(&path, &marks, marks.len().saturating_sub(1));
        let on_disk = self.folder().join(&path);
        fs::write(&on_disk, content).expect("the copy");
        xattr::set(&on_disk, XATTR_ITEM_ID, binding.item_id.as_str().as_bytes()).expect("the copy's id mark");
        xattr::set(&on_disk, XATTR_STATE, State::Hydrated.as_str().as_bytes()).expect("the copy's whole mark");
        if let ContentVersion::Hashed { ctag: Some(ctag), .. } = &downloaded {
            xattr::set(&on_disk, XATTR_CTAG, ctag.as_bytes()).expect("the copy's cTag mark");
        }
        self.set_disk_mtime(&path.to_string_lossy(), downloaded.mtime().0);
        let landed = self.write_rows(move |store| store.item_store().commit_landed_content(item, &downloaded, None));
        assert_eq!(landed, StepCommitOutcome::Written, "the copy of item {item} landed");
    }

    /// The row the folder shows at the directory `path`, made on disk with what is above
    /// it when it is not there.
    fn find_directory_for_new_entry(&self, path: &str) -> LocalItemId {
        let directory = self.find_item_at(path).unwrap_or_else(|| panic!("the folder shows no directory at {path:?}"));
        assert_eq!(directory.kind, ItemKind::Directory, "{path:?} is a directory");
        self.make_directory_on_disk(directory.local_item_id);
        directory.local_item_id
    }

    fn apply_made(&self, path: &str, kind: ItemKind, content: Option<ContentVersion>) -> Item {
        let (parent, name) = split_path(path);
        let place = ItemPlace { parent_local_item_id: self.find_directory_for_new_entry(parent), name: name.into() };
        let is_name_ignored = self.upload_rules.is_name_ignored(OsStr::new(name));
        let made = self.write_rows(move |store| store.item_store().apply(&LocalVersionChange::Made { kind, place, content, is_name_ignored, replaced: None }));
        self.find_item(made).expect("the made item's row")
    }

    /// Makes a file of the user's own at `path` on disk and in the rows, as the mount's
    /// `make` and a close do: unmarked, last modified at the clock's time, its row with a
    /// content changed here and nothing of OneDrive — a new local file. The directory it
    /// is in must be shown; it is put on disk when it is not there. Its row.
    pub(crate) fn make_own_file(&self, path: &str, content: &[u8]) -> Item {
        let now = self.now.load(Ordering::SeqCst);
        let made = self.apply_made(path, ItemKind::File, Some(ContentVersion::ChangedHere { size: content.len() as u64, mtime: now, mtime_nsec: 0, change_count: 1 }));
        fs::write(self.disk_path(path), content).expect("the file");
        self.set_disk_mtime(path, now);
        made
    }

    /// As [`make_own_file`](Self::make_own_file) for a directory: on disk, unmarked, and
    /// in the rows.
    pub(crate) fn make_own_directory(&self, path: &str) -> Item {
        let made = self.apply_made(path, ItemKind::Directory, None);
        fs::create_dir(self.disk_path(path)).expect("the directory");
        made
    }

    /// A program changed the content of the file `item`, whose content is on disk, to
    /// `content` and closed it: the first writer's open, the bytes, and the last close, as
    /// the mount's content writes leave the row (`OpenedForWriting`, `ClosedChanged`) and
    /// the disk (the file's marks stay; its time is the clock's). Its row: for an item
    /// OneDrive has, changed here and not sent.
    pub(crate) fn change_file_here(&self, item: LocalItemId, content: &[u8]) -> Item {
        let (path, _) = self.find_path_with_marks(item);
        let path = path.to_string_lossy().into_owned();
        assert!(self.disk_path(&path).is_file(), "the content of item {item} is on disk before a program changes it");
        let (now, size) = (self.now.load(Ordering::SeqCst), content.len() as u64);
        self.write_rows(move |store| store.item_store().apply(&LocalVersionChange::OpenedForWriting { item }));
        fs::write(self.disk_path(&path), content).expect("the changed file");
        self.set_disk_mtime(&path, now);
        self.write_rows(move |store| store.item_store().apply(&LocalVersionChange::ClosedChanged { item, size, mtime: now, mtime_nsec: 0 }));
        self.find_item(item).expect("the changed file's row")
    }

    /// A program moved `item` to `to` in the folder (the directory `to` is in must be
    /// shown; nothing is at `to`): on disk, when the item is there, and in its row
    /// (`Moved`). Its row: for an item OneDrive has, a move to send.
    pub(crate) fn move_here(&self, item: LocalItemId, to: &str) -> Item {
        let (from, _) = self.find_path_with_marks(item);
        let (parent, name) = split_path(to);
        let place = ItemPlace { parent_local_item_id: self.find_directory_for_new_entry(parent), name: name.into() };
        let is_name_ignored = self.upload_rules.is_name_ignored(OsStr::new(name));
        if fs::symlink_metadata(self.folder().join(&from)).is_ok() {
            fs::rename(self.folder().join(&from), self.disk_path(to)).expect("the entry is moved on disk");
        }
        self.write_rows(move |store| store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored }));
        self.find_item(item).expect("the moved item's row")
    }

    /// A program removed `item` from the folder: from disk, with what is in it, and from
    /// the rows (`Removed`). What is left of its row: nothing for an item OneDrive never
    /// had; for one it has, the row with a removal to send.
    pub(crate) fn remove_here(&self, item: LocalItemId) -> Option<Item> {
        let (path, _) = self.find_path_with_marks(item);
        let on_disk = self.folder().join(&path);
        match fs::symlink_metadata(&on_disk) {
            Ok(entry) if entry.is_dir() => fs::remove_dir_all(&on_disk).expect("the directory is removed from disk"),
            Ok(_) => fs::remove_file(&on_disk).expect("the file is removed from disk"),
            Err(_) => {}
        }
        self.write_rows(move |store| store.item_store().apply(&LocalVersionChange::Removed { item }));
        self.find_item(item)
    }

    // What the rows say and what is on disk.

    /// The row of `item` now.
    pub(crate) fn find_item(&self, item: LocalItemId) -> Option<Item> {
        self.read_rows(move |reader| reader.find_item(item))
    }

    /// The row the folder shows at `path` now; the root's for the empty path.
    pub(crate) fn find_item_at(&self, path: &str) -> Option<Item> {
        let path = PathBuf::from(path);
        self.read_rows(move |reader| reader.find_item_by_path(&path))
    }

    /// Every row now.
    pub(crate) fn list_items(&self) -> Vec<Item> {
        self.write_rows(|store| store.item_store().list_items())
    }

    /// The versions of `item` now, as a step planned at this moment carries them.
    pub(crate) fn versions_of(&self, item: LocalItemId) -> ItemVersions {
        ItemVersions::of(&self.find_item(item).unwrap_or_else(|| panic!("item {item} has no row")))
    }

    /// The path in the backing directory of `path` of the folder.
    pub(crate) fn disk_path(&self, path: &str) -> PathBuf {
        self.folder().join(path)
    }

    /// The bytes of the file at `path` in the backing directory; `None` when no file is
    /// there.
    pub(crate) fn read_disk_file(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.disk_path(path)).ok()
    }

    /// The marks of the entry at `path` in the backing directory.
    pub(crate) fn read_disk_marks(&self, path: &str) -> DiskMarksRead {
        let on_disk = self.disk_path(path);
        let read = |mark: &str| xattr::get(&on_disk, mark).ok().flatten().map(|value| String::from_utf8_lossy(&value).into_owned());
        DiskMarksRead { onedrive_item_id: read(XATTR_ITEM_ID), is_whole: read(XATTR_STATE).as_deref() == Some(State::Hydrated.as_str()), ctag: read(XATTR_CTAG) }
    }

    /// Puts a file with `content` at `path` in the backing directory, with no mark and no
    /// row, last modified at the clock's time: what a person, or a part under test, leaves
    /// on disk. The directory it is in must be on disk.
    pub(crate) fn write_disk_file(&self, path: &str, content: &[u8]) {
        fs::write(self.disk_path(path), content).unwrap_or_else(|error| panic!("the file at {path:?} is written: {error}"));
        self.set_disk_mtime(path, self.now.load(Ordering::SeqCst));
    }

    /// Writes `marks` on the entry at `path` in the backing directory, and takes off each
    /// mark they do not have.
    pub(crate) fn write_disk_marks(&self, path: &str, marks: &DiskMarksRead) {
        let on_disk = self.disk_path(path);
        let whole = marks.is_whole.then(|| State::Hydrated.as_str().to_owned());
        for (mark, value) in [(XATTR_ITEM_ID, &marks.onedrive_item_id), (XATTR_STATE, &whole), (XATTR_CTAG, &marks.ctag)] {
            match value {
                Some(value) => xattr::set(&on_disk, mark, value.as_bytes()).unwrap_or_else(|error| panic!("the mark {mark} of {path:?} is written: {error}")),
                None => drop(xattr::remove(&on_disk, mark)),
            }
        }
    }

    /// The file at `path` in the backing directory was last modified at `seconds`.
    pub(crate) fn set_disk_mtime(&self, path: &str, seconds: i64) {
        let file = fs::File::open(self.disk_path(path)).expect("the file");
        placeholder::set_mtime(&file, UNIX_EPOCH + Duration::from_secs(seconds as u64)).expect("the file's time");
    }

    /// Asserts that the store's rules of the rows hold (`row_rules::find_broken_rule`),
    /// and that the disk has what the rows say of every file the folder shows: a file of
    /// the row's size at the path of its local version where the row says on disk, and
    /// nothing there where it says not on disk.
    pub(crate) fn assert_rows_and_disk_agree(&self) {
        let (items, is_listing_under_way) = self.write_rows(|store| Ok((store.item_store().list_items()?, store.item_store().reader().find_listing_under_way()?.is_some())));
        assert_eq!(row_rules::find_broken_rule(&items, is_listing_under_way), None, "the rules of the rows");
        for item in items.iter().filter(|item| item.kind == ItemKind::File) {
            let Some(content) = item.local.as_ref().and_then(|local| local.content.as_ref()) else { continue };
            let id = item.local_item_id;
            let path = self.read_rows(move |reader| reader.find_path(id)).unwrap_or_else(|| panic!("item {id} is shown and has no path"));
            let on_disk = fs::symlink_metadata(self.folder().join(&path)).ok();
            match item.content_presence {
                ContentPresence::OnDisk => {
                    let on_disk = on_disk.unwrap_or_else(|| panic!("the row of item {id} says on disk, and nothing is at {}", path.display()));
                    assert!(on_disk.is_file(), "the row of item {id} says a file on disk, and {} is none", path.display());
                    assert_eq!(on_disk.len(), content.size(), "the size of {} is its row's", path.display());
                }
                ContentPresence::NotOnDisk => assert!(on_disk.is_none(), "the row of item {id} says not on disk, and something is at {}", path.display()),
            }
        }
    }
}

fn make_take_down_report(take_downs: &Arc<Mutex<Vec<TakeDownCause>>>) -> TakeDownReport {
    let take_downs = Arc::clone(take_downs);
    Arc::new(move |cause| crate::panic::lock(&take_downs).push(cause))
}

/// The fixture's threads end, and the folder gets its write bits back — a mount that was
/// stopped took them off — so that the temporary directory can go.
impl Drop for ItemFixture {
    fn drop(&mut self) {
        self.write_operation_thread.stop();
        self.inotify_bridge.stop();
        self.read_connections.close();
        self.mount_like_thread.stop();
        if let Some(runtime) = self.runtime.take() {
            // Not waited for: the fixture may be dropped on a thread of a test's runtime.
            runtime.shutdown_background();
        }
        restore_write_bits(&self.folder());
    }
}

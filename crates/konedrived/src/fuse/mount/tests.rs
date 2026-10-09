//! A mount's parts built without a mount: the drive of the directory contents' tests in a
//! store on disk, over a folder that holds nothing, with a source whose files hold nothing.
//! What is built is driven from the test's own thread, which is no thread of the runtime the
//! parts wait and sweep on.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use konedrive_fs::placeholder::{State, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Change, Store, TreeStore};

use super::{Mount, MountSetup, WriteSetup};
use crate::folder::root::SyncRoot;
use crate::fuse::directory_contents::tests::drive;
use crate::fuse::files_interface::FilesRefusal;
use crate::fuse::kernel_requests::KernelRequests;
use crate::fuse::node_table::NodeId;
use crate::hydration::source::{ContentSource, Fetched, Share, SourceError};
use crate::local::IgnoreList;
use crate::status::report::Report;

/// A source whose every file holds nothing.
struct EmptyFiles;

#[async_trait::async_trait]
impl ContentSource for EmptyFiles {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        Ok(Fetched { served_from: from, size: 0, mtime: UNIX_EPOCH, version: None, stream: Box::new(std::io::Cursor::new(Vec::new())) })
    }
}

/// The parts of a mount, built over a folder that no mount lies over.
pub(crate) struct BuiltMount {
    pub(crate) mount: Arc<Mount>,
    pub(in crate::fuse) kernel_requests: KernelRequests,
    /// The backing directory, by its own path.
    pub(crate) disk: PathBuf,
    /// The store's one writer, which the parts were given.
    pub(crate) store: Store,
    /// The folder's tree lock, which the write-operation thread runs each job under.
    pub(crate) tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The runtime the parts wait and sweep on.
    _runtime: tokio::runtime::Runtime,
    _tmp: tempfile::TempDir,
}

/// The parts of the mount of the fixture's drive (`docs` with `c.txt`; `a.txt` and `b.txt`
/// in the root), with `more` committed after it and a pin on `pinned_ids`, on the machine
/// `fedora`, in a folder that ignores `*.tmp` and `cache`.
pub(in crate::fuse) fn build_mount_with(more: &[Change], pinned_ids: &[&str]) -> BuiltMount {
    let tmp = tempfile::tempdir().unwrap();
    let (disk, store_path) = (tmp.path().join("folder"), tmp.path().join("tree.db"));
    fs::create_dir(&disk).unwrap();
    let mut written = TreeStore::open(&store_path).unwrap();
    drive(&mut written, Vec::new());
    if !more.is_empty() {
        written.commit_page(more, "after").unwrap();
    }
    let store = Store::new(written);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let tree_lock = Arc::new(tokio::sync::Mutex::new(()));
    let setup = MountSetup {
        root: SyncRoot { path: disk.clone(), root_id: "R".into() },
        store_path,
        store: store.clone(),
        source: Arc::new(EmptyFiles),
        report: Report::nowhere(),
        pool: TransferPool::new(4),
        split_download_share: Share::new(),
        download_after: 1 << 20,
        write_setup: WriteSetup { tree_lock: Arc::clone(&tree_lock), ignore: IgnoreList::new(["*.tmp", "cache"]).shared(), machine_name: "fedora".into() },
    };
    let pinned_ids = pinned_ids.iter().map(|id| (*id).to_owned()).collect();
    let (mount, kernel_requests) = Mount::build(setup, pinned_ids, runtime.handle().clone()).unwrap();
    BuiltMount { mount: Arc::new(mount), kernel_requests, disk, store, tree_lock, _runtime: runtime, _tmp: tmp }
}

pub(crate) fn build_mount() -> BuiltMount {
    build_mount_with(&[], &[])
}

impl BuiltMount {
    /// The whole downloaded copy of the OneDrive file `id`, with `content`, at `rel` in the
    /// backing directory, whose directory is there already.
    pub(crate) fn download(&self, rel: &str, id: &str, content: &[u8]) {
        let path = self.disk.join(rel);
        fs::write(&path, content).unwrap();
        xattr::set(&path, XATTR_ITEM_ID, id.as_bytes()).unwrap();
        xattr::set(&path, XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
    }

    /// A directory at `rel` in the backing directory, marked with the OneDrive id `id`.
    pub(in crate::fuse) fn make_marked_directory(&self, rel: &str, id: &str) {
        fs::create_dir(self.disk.join(rel)).unwrap();
        xattr::set(self.disk.join(rel), XATTR_ITEM_ID, id.as_bytes()).unwrap();
    }

    /// Runs `job` on the mount's write-operation thread, and waits for it: every job the
    /// thread was given before has run then.
    pub(in crate::fuse) fn run_on_write_operation_thread(&self, job: impl FnOnce() + Send + 'static) {
        self.mount.write_operation_thread.run_and_wait(job).blocking_recv().unwrap();
    }

    /// Puts `job` on the mount's write-operation thread, behind every job it was given
    /// before; what it gives comes once it ran.
    pub(crate) fn queue_on_write_operation_thread<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> tokio::sync::oneshot::Receiver<T> {
        self.mount.write_operation_thread.run_and_wait(job)
    }

    /// Stops the mount's write-operation thread, as the stop of the mount does.
    pub(in crate::fuse) fn stop_write_operation_thread(&self) {
        self.mount.write_operation_thread.stop();
    }

    /// Waits until `count` jobs wait for the mount's write-operation thread, which runs
    /// none of them while the test holds the tree lock.
    pub(in crate::fuse) fn wait_for_queued_write_operations(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.mount.write_operation_thread.count_queued_jobs() < count {
            assert!(Instant::now() < deadline, "{count} jobs were to wait for the write-operation thread, and {} do", self.mount.write_operation_thread.count_queued_jobs());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The node id the kernel is given for `name` of the folder itself.
    pub(in crate::fuse) fn node_id_in_root(&self, name: &str) -> NodeId {
        self.kernel_requests.find_node_id_in_root(name)
    }

    /// The folder's path, which a mount would lie over: what the `Files` interface is asked
    /// about is a path below it.
    pub(in crate::fuse) fn path(&self, rel: &str) -> PathBuf {
        self.disk.join(rel)
    }
}

/// The parts are built in an order in which each is given only what stands before it, and
/// what is built answers without a mount over the folder.
#[test]
fn the_parts_of_a_mount_are_built_over_a_folder_that_no_mount_lies_over() {
    let built = build_mount_with(&[], &["D"]);
    assert!(!built.mount.access_mode_switch().is_read_write(), "a mount starts every folder read-only");
    assert_eq!(built.mount.files_interface().count_pins(), 1);
    let status = built.mount.files_interface().find_path_status(&built.path("docs/c.txt")).unwrap();
    assert_eq!((status.onedrive_id.as_str(), status.own_pin, status.pinned_above.as_deref()), ("C", false, Some(built.path("docs").as_path())));
    assert!(matches!(built.mount.files_interface().find_path_status(Path::new("/elsewhere/a.txt")), Err(FilesRefusal::OutsideFolder)));
    // No session: nothing was mounted, and there is nothing to take off.
    assert!(crate::panic::lock(&built.mount.session).is_none() && built.mount.fuse_connection.is_none());
    built.mount.stop();
}

/// To read-write the mode is switched at once, also while a cycle holds the tree lock, which
/// the write-operation thread waits for; back to read-only it is switched on that thread,
/// and waited for.
#[test]
fn a_mount_is_switched_to_read_write_at_once_and_to_read_only_on_the_write_operation_thread() {
    let built = build_mount();
    let held = built.tree_lock.try_lock().unwrap();
    assert!(built.mount.switch_to_read_write(Arc::new(|| {})));
    assert!(built.mount.access_mode_switch().is_read_write());
    // Switched again, it changes nothing.
    assert!(!built.mount.switch_to_read_write(Arc::new(|| {})));
    drop(held);
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    assert!(runtime.block_on(built.mount.switch_to_read_only()));
    assert!(!built.mount.access_mode_switch().is_read_write());
    assert!(!runtime.block_on(built.mount.switch_to_read_only()));
}

/// A mount that is stopped runs no write operation any more: its mode is switched at once.
#[test]
fn the_access_mode_of_a_mount_whose_thread_is_stopped_is_switched_at_once() {
    let built = build_mount();
    assert!(built.mount.access_mode_switch.switch_to_read_write());
    built.mount.write_operation_thread.stop();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let held = built.tree_lock.try_lock().unwrap();
    assert!(runtime.block_on(built.mount.switch_to_read_only()));
    assert!(!built.mount.access_mode_switch().is_read_write());
    drop(held);
}

/// The write-operation thread is stopped first, and waited for: the write operation in
/// flight has ended while the bridge still takes what it announces and a download is still
/// waited for; then the bridge is stopped, and whoever waits for a download is failed.
#[test]
fn a_mount_that_is_stopped_stops_its_write_operation_thread_before_its_other_parts() {
    let built = build_mount();
    let (begin, began) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (inotify_bridge, download_states) = (Arc::clone(&built.mount.inotify_bridge), Arc::clone(&built.mount.download_states));
    let (waiter, mut downloaded) = tokio::sync::oneshot::channel();
    let in_flight = built.queue_on_write_operation_thread(move || {
        assert!(matches!(download_states.start_or_join("A", Some(waiter)), crate::fuse::download_states::DownloadStart::Started { .. }));
        begin.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(5));
        (inotify_bridge.is_stopped(), downloaded.try_recv().is_ok())
    });
    began.recv_timeout(Duration::from_secs(5)).unwrap();
    std::thread::scope(|scope| {
        let stop = scope.spawn(|| built.mount.stop());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!stop.is_finished(), "the stop waits for the write operation in flight");
        release.send(()).unwrap();
    });
    assert_eq!(in_flight.blocking_recv(), Ok((false, false)), "neither the bridge nor the downloads were stopped while the operation ran");
    assert!(built.mount.inotify_bridge.is_stopped());
    assert!(built.mount.write_operation_thread.run_and_wait(|| ()).blocking_recv().is_err(), "no write operation runs any more");
    // Stopped again, as its drop does: nothing is left to stop.
    built.mount.stop();
}

/// Whoever waits for a download when the mount is stopped is failed, and not kept waiting.
#[test]
fn a_mount_that_is_stopped_fails_whoever_waits_for_a_download() {
    let built = build_mount();
    let (waiter, downloaded) = tokio::sync::oneshot::channel();
    assert!(matches!(built.mount.download_states.start_or_join("A", Some(waiter)), crate::fuse::download_states::DownloadStart::Started { .. }));
    built.mount.stop();
    assert_eq!(downloaded.blocking_recv(), Ok(Err(libc::EIO)));
}

/// The switch to read-write sets what wakes the outbox's worker, and the switch to read-only
/// clears it: a change recorded in a folder that uploads wakes the worker, and nothing is
/// woken once the folder takes no change.
#[test]
fn the_switches_of_a_mount_set_and_clear_what_wakes_the_outbox() {
    let built = build_mount();
    let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = || woken.load(std::sync::atomic::Ordering::SeqCst);
    built.mount.outbox_recorder.wake_outbox();
    assert_eq!(count(), 0, "nothing wakes a worker before the folder uploads");
    let waker = Arc::clone(&woken);
    assert!(built.mount.switch_to_read_write(Arc::new(move || {
        waker.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    })));
    built.mount.outbox_recorder.wake_outbox();
    assert_eq!(count(), 1);
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    assert!(runtime.block_on(built.mount.switch_to_read_only()));
    built.mount.outbox_recorder.wake_outbox();
    assert_eq!(count(), 1, "the waker was cleared");
}

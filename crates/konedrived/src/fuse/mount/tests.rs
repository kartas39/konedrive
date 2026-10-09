//! A mount's parts built without a mount: the drive of the directory contents' tests in a
//! store on disk, over a folder that holds nothing, with a source whose files hold nothing.
//! What is built is driven from the test's own thread, which is no thread of the runtime the
//! parts wait and sweep on.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use konedrive_fs::placeholder::{State, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Change, Store, TreeStore};

use super::{Mount, MountSetup, WriteSetup};
use crate::folder::root::SyncRoot;
use crate::fuse::directory_contents::tests::drive;
use crate::fuse::files_interface::FilesRefusal;
use crate::fuse::kernel_requests::KernelRequests;
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
pub(in crate::fuse) struct BuiltMount {
    pub(in crate::fuse) mount: Mount,
    pub(in crate::fuse) kernel_requests: KernelRequests,
    /// The backing directory, by its own path.
    pub(in crate::fuse) disk: PathBuf,
    /// The store's one writer, which the parts were given.
    pub(in crate::fuse) store: Store,
    /// The folder's tree lock, which the write-operation thread runs each job under.
    pub(in crate::fuse) tree_lock: Arc<tokio::sync::Mutex<()>>,
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
    BuiltMount { mount, kernel_requests, disk, store, tree_lock, _runtime: runtime, _tmp: tmp }
}

pub(in crate::fuse) fn build_mount() -> BuiltMount {
    build_mount_with(&[], &[])
}

impl BuiltMount {
    /// The whole downloaded copy of the OneDrive file `id`, with `content`, at `rel` in the
    /// backing directory, whose directory is there already.
    pub(in crate::fuse) fn download(&self, rel: &str, id: &str, content: &[u8]) {
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
    assert!(!built.mount.is_read_write(), "a mount starts every folder read-only");
    assert_eq!(built.mount.files_interface().count_pins(), 1);
    let status = built.mount.files_interface().find_path_status(&built.path("docs/c.txt")).unwrap();
    assert_eq!((status.id.as_str(), status.own_pin, status.pinned_above.as_deref()), ("C", false, Some(built.path("docs").as_path())));
    assert!(matches!(built.mount.files_interface().find_path_status(Path::new("/elsewhere/a.txt")), Err(FilesRefusal::OutsideFolder)));
    // No session: nothing was mounted, and there is nothing to take off.
    assert!(crate::panic::lock(&built.mount.session).is_none() && built.mount.fuse_connection.is_none());
    built.mount.stop();
}

/// The switch is made on the write-operation thread, and waited for: the mode is the new one
/// when the call returns.
#[test]
fn the_access_mode_of_a_mount_is_switched_on_the_write_operation_thread() {
    let built = build_mount();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    assert!(runtime.block_on(built.mount.switch_to_read_write(Arc::new(|| {}))));
    assert!(built.mount.is_read_write());
    // Switched again, it changes nothing.
    assert!(!runtime.block_on(built.mount.switch_to_read_write(Arc::new(|| {}))));
    assert!(runtime.block_on(built.mount.switch_to_read_only()));
    assert!(!built.mount.is_read_write());
}

/// A mount that is stopped runs no write operation any more: its mode is switched at once.
#[test]
fn the_access_mode_of_a_mount_whose_thread_is_stopped_is_switched_at_once() {
    let built = build_mount();
    built.mount.write_operation_thread.stop();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let held = built.tree_lock.try_lock().unwrap();
    assert!(runtime.block_on(built.mount.switch_to_read_write(Arc::new(|| {}))));
    assert!(built.mount.is_read_write());
    drop(held);
}

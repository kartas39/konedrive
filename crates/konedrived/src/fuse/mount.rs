//! A mount: made, served, and taken off.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use fuser::{Config, MountOption};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Store, TreeStore};

use crate::folder::root::SyncRoot;
use crate::hydration::source::{ContentSource, Share};
use crate::status::report::Report;

use super::announce::{announce, Announce};
use super::fill::Waiter;
use super::backing_directory::{set_tree, BackingDirectory};
use super::directory_contents::DirectoryContents;
use super::node_table::NodeTable;
use super::upload_rules::UploadRules;
use super::requests::Requests;
use super::node_lookup::NodeLookup;
use super::recorder::Recorder;
use super::shared::{other, Shared};
use super::write_operations::WriteOperations;
use super::writers::Writers;

/// A folder served as a mount, for as long as this is held.
pub struct Mount {
    pub(super) shared: Arc<Shared>,
    pub(super) session: Mutex<Option<fuser::BackgroundSession>>,
    pub(super) announcer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The mount's connection, as the kernel numbers it (`/sys/fs/fuse/connections/<n>`).
    pub(super) connection: u32,
}

/// What a folder that takes changes shares with its sync.
pub struct Writing {
    /// The folder's tree lock: a change is recorded under it, as a cycle of the delta listing
    /// holds it from staging to its swap.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list: what stays on this computer only.
    pub ignore: crate::local::SharedIgnore,
    /// For conflict copies: `name-<machine>.ext`.
    pub machine_name: String,
}

/// What a mount is made with.
pub struct Parts {
    pub root: SyncRoot,
    /// The folder's tree store: its file, for the mount's own reading connection, and its one
    /// writer.
    pub store_path: PathBuf,
    pub store: Store,
    pub source: Arc<dyn ContentSource>,
    pub report: Report,
    pub pool: Arc<TransferPool>,
    pub parts: Arc<Share>,
    /// `[transfers] download_after`: see [`Shared::download_after`].
    pub download_after: u64,
    /// What the folder shares with its sync when it takes changes.
    pub writing: Writing,
}

impl Mount {
    /// Mounts over `parts.root` and serves it. The folder on disk must carry its root id
    /// already: after this its path is the mount.
    pub async fn start(parts: Parts) -> io::Result<Arc<Mount>> {
        let Parts { root, store_path, store, source, report, pool, parts, download_after, writing } = parts;
        let pins: HashSet<String> = store.call(|s| s.pins()).await.map_err(other)?.into_iter().collect();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            // Before the mount: afterwards this path is the mount.
            let under = Arc::new(BackingDirectory::open(&root.path)?);
            let view = Arc::new(DirectoryContents::new(TreeStore::open_read_only(&store_path).map_err(other)?, Arc::clone(&under)));
            let nodes = Arc::new(Mutex::new(NodeTable { next: 1, ..NodeTable::default() }));
            let resolve = Arc::new(NodeLookup::new(Arc::clone(&view), Arc::clone(&nodes)));
            let writable = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let recorder = Arc::new(Recorder::start(store.clone(), writing.tree_lock, Arc::clone(&writable))?);
            let upload_rules = Arc::new(UploadRules::new(writing.ignore, writing.machine_name));
            let writers = Arc::new(Writers::default());
            let write_operations = WriteOperations::new(
                Arc::clone(&view),
                Arc::clone(&under),
                Arc::clone(&resolve),
                Arc::clone(&writers),
                Arc::clone(&recorder),
                Arc::clone(&upload_rules),
            );
            let shared = Arc::new(Shared {
                root: root.clone(),
                under,
                view,
                store,
                nodes,
                resolve,
                source,
                report,
                pool,
                parts,
                fills: Mutex::default(),
                running: Mutex::default(),
                unfinished: Mutex::default(),
                handles: Mutex::default(),
                next_handle: AtomicU64::new(1),
                listings: Mutex::default(),
                asked: Mutex::default(),
                download_after,
                readers: Mutex::default(),
                reads: Mutex::default(),
                pins: Mutex::new(pins),
                announcer: AtomicI32::new(0),
                told: Mutex::new(None),
                tell: Mutex::new(None),
                pending: Mutex::default(),
                stopping: std::sync::atomic::AtomicBool::new(false),
                writable,
                upload_rules,
                recorder,
                writers,
                write_operations,
                runtime,
            });
            let mut config = Config::default();
            config.mount_options = vec![
                MountOption::FSName("konedrive".into()),
                MountOption::Subtype("konedrive".into()),
                MountOption::NoSuid,
                MountOption::NoDev,
            ];
            config.n_threads = Some(4);
            config.clone_fd = true;
            let session = fuser::Session::new(Requests(Arc::clone(&shared)), &root.path, &config)?;
            let notifier = session.notifier();
            // From here on the daemon opens the folder by this descriptor: its path is the mount.
            crate::folder::root::mounted_over(&root.path, Arc::new(shared.under.descriptor()?));
            let (tell, told) = mpsc::channel::<Vec<Announce>>();
            *crate::panic::lock(&shared.tell) = Some(tell);
            let session = match session.spawn() {
                Ok(session) => session,
                Err(e) => {
                    *crate::panic::lock(&shared.tell) = None;
                    return Err(e);
                }
            };
            let connection = fs::metadata(&root.path).map_or(0, |m| libc::minor(m.dev()));
            // BackingDirectory the mount the folder is the daemon's alone, and open to it.
            shared.under.unlock();
            let saying = Arc::clone(&shared);
            let announcer = std::thread::Builder::new().name("konedrive-announce".into()).spawn(move || announce(&saying, &notifier, told))?;
            shared.sweep();
            Ok(Arc::new(Mount { shared, session: Mutex::new(Some(session)), announcer: Mutex::new(Some(announcer)), connection }))
        })
        .await
        .map_err(other)?
    }

    /// Takes the mount off. The folder on disk then shows the downloaded files alone.
    ///
    /// In this order: the announcer stops while the mount still answers it (what it says by
    /// path would otherwise land in the folder itself), every open that waits for a download
    /// is failed, and the connection is aborted before the mount is detached — a program that
    /// still holds a file of the mount open would otherwise keep the session, and this call,
    /// waiting for it.
    pub fn stop(&self) {
        let Some(session) = crate::panic::lock(&self.session).take() else { return };
        let s = &self.shared;
        s.stopping.store(true, Ordering::SeqCst);
        *crate::panic::lock(&s.tell) = None;
        // The recorder ends with the last change it was given; what comes later is refused.
        s.recorder.stop();
        if let Some(announcer) = crate::panic::lock(&self.announcer).take() {
            let _ = announcer.join();
        }
        let waiting: Vec<Waiter> = crate::panic::lock(&s.fills).drain().flat_map(|(_, waiting)| waiting).collect();
        waiting.into_iter().for_each(|waiter| {
            let _ = waiter.send(Err(libc::EIO));
        });
        crate::panic::lock(&s.reads).drain().for_each(|(_, read)| read.abort());
        if let Err(e) = fs::write(format!("/sys/fs/fuse/connections/{}/abort", self.connection), "1") {
            tracing::warn!("the connection of {} could not be aborted ({e}); its mount is taken off all the same", s.root.path.display());
            // Not waited for: it ends when the last program lets go of the mount.
            let folder = s.root.path.clone();
            std::thread::spawn(move || {
                let _ = session.umount_and_join();
                crate::folder::root::mount_gone(&folder);
                set_tree(&folder, true);
            });
            return;
        }
        if let Err(e) = session.umount_and_join() {
            tracing::warn!("taking the mount off {}: {e}", s.root.path.display());
        }
        crate::folder::root::mount_gone(&s.root.path);
        // A plain directory again: locked, so that nothing is put into it by mistake.
        set_tree(&s.root.path, true);
    }

    /// [`stop`](Self::stop), for a folder that is forgotten: it is the user's again, to keep
    /// or to remove, and is left unlocked.
    pub fn forget(&self) {
        self.stop();
        set_tree(&self.shared.root.path, false);
    }

    /// The downloads that failed and can be continued: each one's size on disk and its
    /// checkpoint.
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn staged(&self) -> Vec<(u64, Option<u64>)> {
        let seen = |(_, file): &(String, Arc<fs::File>)| Some((file.metadata().ok()?.len(), konedrive_fs::placeholder::read_progress(file).ok().flatten().map(|p| p.bytes)));
        crate::panic::lock(&self.shared.unfinished).iter().filter_map(seen).collect()
    }

    /// The folder under the mount, for what must not go through the mount: a walk of what is
    /// downloaded.
    pub fn under(&self) -> PathBuf {
        self.shared.under.root()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.stop();
    }
}

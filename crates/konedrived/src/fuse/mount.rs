//! A mount: made, served, and taken off.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fuser::{Config, MountOption};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Store, TreeError, TreeStore};

use crate::folder::root::SyncRoot;
use crate::hydration::source::{ContentSource, Share};
use crate::local::held::Rw;
use crate::status::report::Report;

use super::access_mode::AccessMode;
use super::download_states::DownloadStates;
use super::file_downloader::{FileChangeAnnouncer, FileDownloader};
use super::backing_directory::{set_tree, BackingDirectory};
use super::directory_contents::DirectoryContents;
use super::conflict_copies::ConflictCopies;
use super::inotify_bridge::{InotifyBridge, RemoteChange};
use super::upload_rules::UploadRules;
use super::requests::Requests;
use super::node_lookup::NodeLookup;
use super::outbox_recorder::OutboxRecorder;
use super::pins::{download_pinned_files, Pins};
use super::range_readers::RangeReaders;
use super::remote_changes::{self, CycleOutcome, UncommittedCycle};
use super::shared::{other, Shared};
use super::write_operations::WriteOperations;
use super::open_writers::OpenWriters;

/// A folder served as a mount, for as long as this is held.
pub struct Mount {
    pub(super) shared: Arc<Shared>,
    pub(super) session: Mutex<Option<fuser::BackgroundSession>>,
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
    /// `[transfers] download_after`: once that many bytes of a file were read as ranges, the
    /// file is downloaded whole.
    pub download_after: u64,
    /// What the folder shares with its sync when it takes changes.
    pub writing: Writing,
}

impl Mount {
    /// Mounts over `parts.root` and serves it. The folder on disk must carry its root id
    /// already: after this its path is the mount.
    pub async fn start(parts: Parts) -> io::Result<Arc<Mount>> {
        let Parts { root, store_path, store, source, report, pool, parts, download_after, writing } = parts;
        let pinned_ids = Pins::load_pinned_ids(&store).await.map_err(other)?;
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            // Before the mount: afterwards this path is the mount.
            let backing_directory = Arc::new(BackingDirectory::open(&root.path)?);
            let directory_contents = Arc::new(DirectoryContents::new(TreeStore::open_read_only(&store_path).map_err(other)?, Arc::clone(&backing_directory)));
            let node_lookup = Arc::new(NodeLookup::new(Arc::clone(&directory_contents)));
            let access_mode = Arc::new(AccessMode::read_only());
            let outbox_recorder = Arc::new(OutboxRecorder::start(store.clone(), writing.tree_lock, Arc::clone(&access_mode))?);
            let upload_rules = Arc::new(UploadRules::new(writing.ignore));
            let conflict_copies = ConflictCopies::new(Arc::clone(&backing_directory), Arc::clone(&directory_contents), Arc::clone(&upload_rules), writing.machine_name);
            let open_writers = Arc::new(OpenWriters::default());
            let write_operations = WriteOperations::new(
                Arc::clone(&directory_contents),
                Arc::clone(&backing_directory),
                Arc::clone(&node_lookup),
                Arc::clone(&open_writers),
                Arc::clone(&outbox_recorder),
                upload_rules,
                Arc::clone(&access_mode),
            );
            let download_states = Arc::new(DownloadStates::new(download_after));
            let pins = Arc::new(Pins::new(pinned_ids, store, Arc::clone(&directory_contents), runtime.clone()));
            let inotify_bridge = Arc::new(InotifyBridge::new(root.path.clone(), Arc::clone(&node_lookup), Arc::clone(&directory_contents)));
            let announcing = Arc::clone(&inotify_bridge);
            let announce_file_change: FileChangeAnnouncer = Arc::new(move |rel, id| announcing.announce(vec![RemoteChange::ContentChanged { rel, id }]));
            let file_downloader = Arc::new(FileDownloader {
                backing_directory: Arc::clone(&backing_directory),
                directory_contents: Arc::clone(&directory_contents),
                download_states: Arc::clone(&download_states),
                source: Arc::clone(&source),
                pool: Arc::clone(&pool),
                parts_share: parts,
                report: report.clone(),
                folder: root.path.clone(),
                announce_file_change,
                runtime: runtime.clone(),
            });
            let shared = Arc::new(Shared {
                root: root.clone(),
                backing_directory,
                directory_contents,
                node_lookup,
                report,
                range_readers: Arc::new(RangeReaders::new(pool, source, runtime.clone())),
                download_states,
                file_downloader,
                open_files: Arc::default(),
                reads: Mutex::default(),
                pins,
                inotify_bridge,
                uncommitted_cycle: UncommittedCycle::default(),
                access_mode,
                conflict_copies,
                outbox_recorder,
                open_writers,
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
            crate::folder::root::mounted_over(&root.path, Arc::new(shared.backing_directory.descriptor()?));
            let session = session.spawn()?;
            let connection = fs::metadata(&root.path).map_or(0, |m| libc::minor(m.dev()));
            // BackingDirectory the mount the folder is the daemon's alone, and open to it.
            shared.backing_directory.unlock();
            shared.inotify_bridge.start(notifier)?;
            download_pinned_files(&shared.pins, &shared.directory_contents, &shared.file_downloader, &shared.runtime);
            Ok(Arc::new(Mount { shared, session: Mutex::new(Some(session)), connection }))
        })
        .await
        .map_err(other)?
    }

    /// Takes the mount off. The folder on disk then shows the downloaded files alone.
    ///
    /// In this order: the inotify bridge stops while the mount still answers it (what it
    /// does by path would otherwise land in the backing directory), every open that waits for a download
    /// is failed, and the connection is aborted before the mount is detached — a program that
    /// still holds a file of the mount open would otherwise keep the session, and this call,
    /// waiting for it.
    pub fn stop(&self) {
        let Some(session) = crate::panic::lock(&self.session).take() else { return };
        let s = &self.shared;
        // The recorder ends with the last change it was given; what comes later is refused.
        s.outbox_recorder.stop();
        s.inotify_bridge.stop();
        s.download_states.fail_waiters();
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

    /// The folder takes changes from now on, and `wake` is called for each one recorded:
    /// its sync uploads. Whether the modes shown changed with that.
    pub fn switch_to_read_write(&self, wake: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.shared.outbox_recorder.set_wake(Some(wake));
        let changed = self.shared.access_mode.switch_to_read_write();
        if changed {
            // What a lost row left unsent is found again.
            let s = Arc::clone(&self.shared);
            self.shared.outbox_recorder.run(move || s.write_operations.record_unrecorded_entries());
        }
        changed
    }

    /// The folder takes no change from now on. Whether the modes shown changed with that.
    pub fn switch_to_read_only(&self) -> bool {
        self.shared.outbox_recorder.set_wake(None);
        self.shared.access_mode.switch_to_read_only()
    }

    /// Whether the folder takes changes now.
    pub fn is_read_write(&self) -> bool {
        self.shared.access_mode.is_read_write()
    }

    /// Makes the backing directory follow the cycle of the delta listing that `store` has
    /// staged and not yet committed ([`remote_changes::apply_staged_cycle`]); `rw` is what
    /// the outbox holds, in a read-write folder.
    pub fn apply_staged_cycle(&self, store: &mut TreeStore, rw: Option<&Rw>) -> Result<CycleOutcome, TreeError> {
        let s = &self.shared;
        remote_changes::apply_staged_cycle(store, rw, &s.backing_directory, &s.range_readers, &s.conflict_copies, &s.uncommitted_cycle)
    }

    /// The cycle [`apply_staged_cycle`](Self::apply_staged_cycle) applied is committed: its
    /// changes are announced to whoever watches the folder, and what is pinned and not
    /// downloaded is downloaded ([`remote_changes::announce_committed_cycle`]).
    pub fn announce_committed_cycle(&self) {
        let s = &self.shared;
        remote_changes::announce_committed_cycle(&s.uncommitted_cycle, &s.pins, &s.inotify_bridge, &s.directory_contents, &s.file_downloader, &s.runtime, &s.outbox_recorder);
    }

    /// The downloads that failed and can be continued: each one's size on disk and its
    /// checkpoint.
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn staged(&self) -> Vec<(u64, Option<u64>)> {
        let seen = |file: &Arc<fs::File>| Some((file.metadata().ok()?.len(), konedrive_fs::placeholder::read_progress(file).ok().flatten().map(|p| p.bytes)));
        self.shared.download_states.list_unfinished_files().iter().filter_map(seen).collect()
    }

    /// The folder under the mount, for what must not go through the mount: a walk of what is
    /// downloaded.
    pub fn under(&self) -> PathBuf {
        self.shared.backing_directory.root()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.stop();
    }
}

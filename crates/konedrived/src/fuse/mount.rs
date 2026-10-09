//! A mount: made, served, and taken off.
//!
//! [`Mount`] holds the parts of a served folder and nothing else: what it is asked — by the
//! folder's sync, by a cycle of the delta listing, by the daemon's `Files` interface — it
//! hands on to the part that answers. [`Mount::start`] builds the parts in the order they
//! depend on each other, and [`Mount::stop`] stops each one that runs something.
//!
//! The order the parts are built in; each is given only what stands before it:
//!
//! 1. [`BackingDirectory`]: opened before the mount lies over it;
//! 2. [`DirectoryContents`]: over the store's reader and the backing directory;
//! 3. [`NodeResolver`]: over the directory contents;
//! 4. [`AccessMode`], read-only at first;
//! 5. [`OutboxRecorder`]: the store's writer, the tree lock, the access mode;
//! 6. [`UploadRules`], then [`ConflictCopies`]: the backing directory, the directory
//!    contents, the rules;
//! 7. [`OpenWriters`], then [`WriteOperations`]: the directory contents, the backing
//!    directory, the node resolver, the open writers, the recorder, the rules, the access mode;
//! 8. [`DownloadStates`], and [`Pins`] over the directory contents;
//! 9. [`InotifyBridge`]: the node resolver and the directory contents — its thread is not
//!    started yet;
//! 10. [`FileDownloader`]: the backing directory, the directory contents, the download
//!     states, and the way to announce a file's change, which is the bridge's;
//! 11. [`RangeReaders`], [`OpenFiles`], [`InterruptibleReads`], which depend on no part;
//! 12. [`FilesInterface`] and [`KernelRequests`], each given exactly the parts it uses;
//! 13. the session over the kernel requests, and the bridge's thread, started with the
//!     session's notifier once the mount answers.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fuser::{Config, MountOption};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Store, TreeError, TreeStore};

use crate::folder::root::SyncRoot;
use crate::hydration::source::{ContentSource, Share};
use crate::local::held::Rw;
use crate::status::report::Report;

use super::access_mode::AccessMode;
use super::backing_directory::{remove_write_bits, restore_write_bits, BackingDirectory};
use super::conflict_copies::ConflictCopies;
use super::directory_contents::DirectoryContents;
use super::download_states::DownloadStates;
use super::file_downloader::{FileChangeAnnouncer, FileDownloader};
use super::files_interface::{FilesInterface, FilesRefusal, FreedSpace, PathStatus};
use super::inotify_bridge::{InotifyBridge, Announcement};
use super::kernel_requests::{InterruptibleReads, KernelRequests};
use super::node_resolver::NodeResolver;
use super::open_files::OpenFiles;
use super::open_writers::OpenWriters;
use super::outbox_recorder::{OutboxRecorder, OutboxWaker};
use super::pins::{download_pinned_files, Pins};
use super::range_readers::RangeReaders;
use super::cycle_step::{self, CycleOutcome, UncommittedCycle};
use super::upload_rules::UploadRules;
use super::write_operations::WriteOperations;

/// A folder served as a mount, for as long as this is held.
pub struct Mount {
    /// The folder's path, which is the mount.
    folder: PathBuf,
    /// Whether programs may change the folder ([`super::access_mode`]).
    access_mode: Arc<AccessMode>,
    /// The directory on disk the mount lies over ([`super::backing_directory`]).
    backing_directory: Arc<BackingDirectory>,
    /// What each directory of the mount holds ([`super::directory_contents`]).
    directory_contents: Arc<DirectoryContents>,
    /// The thread that makes and records each change ([`super::outbox_recorder`]).
    outbox_recorder: Arc<OutboxRecorder>,
    /// The operations that change the folder ([`super::write_operations`]).
    write_operations: Arc<WriteOperations>,
    /// What moves the user's own out of the way of a change from OneDrive
    /// ([`super::conflict_copies`]).
    conflict_copies: ConflictCopies,
    /// The state of each file's download ([`super::download_states`]).
    download_states: Arc<DownloadStates>,
    /// What downloads a file whole ([`super::file_downloader`]).
    file_downloader: Arc<FileDownloader>,
    /// The files being read as ranges ([`super::range_readers`]).
    range_readers: Arc<RangeReaders>,
    /// The pins ([`super::pins`]).
    pins: Arc<Pins>,
    /// What brings a change from OneDrive to whoever watches the folder
    /// ([`super::inotify_bridge`]).
    inotify_bridge: Arc<InotifyBridge>,
    /// What the cycle now being committed leaves to be done once it is
    /// ([`super::cycle_step`]).
    uncommitted_cycle: UncommittedCycle,
    /// The reads that wait for a range from OneDrive, which the kernel requests keep
    /// ([`super::kernel_requests`]).
    interruptible_reads: Arc<InterruptibleReads>,
    /// What the daemon's bus interface `Files` asks ([`super::files_interface`]).
    files_interface: FilesInterface,
    /// Where what a pin keeps is downloaded.
    runtime: tokio::runtime::Handle,
    /// The session the kernel's requests are answered in; `None` once the mount is stopped.
    session: Mutex<Option<fuser::BackgroundSession>>,
    /// The mount's connection, as the kernel numbers it (`/sys/fs/fuse/connections/<n>`).
    connection: u32,
}

/// What a folder that takes changes shares with its sync.
pub struct WriteSetup {
    /// The folder's tree lock: a change is recorded under it, as a cycle of the delta listing
    /// holds it from staging to its swap.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list: what stays on this computer only.
    pub ignore: crate::local::SharedIgnore,
    /// For conflict copies: `name-<machine>.ext`.
    pub machine_name: String,
}

/// What a mount is made with.
pub struct MountSetup {
    pub root: SyncRoot,
    /// The folder's tree store: its file, for the mount's own reading connection, and its one
    /// writer.
    pub store_path: PathBuf,
    pub store: Store,
    pub source: Arc<dyn ContentSource>,
    pub report: Report,
    pub pool: Arc<TransferPool>,
    /// What the daemon's large downloads in parallel streams share.
    pub split_download_share: Arc<Share>,
    /// `[transfers] download_after`: once that many bytes of a file were read as ranges, the
    /// file is downloaded whole.
    pub download_after: u64,
    /// What the folder shares with its sync when it takes changes.
    pub write_setup: WriteSetup,
}

fn to_io_error(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}

impl Mount {
    /// Mounts over `setup.root` and serves it. The folder on disk must carry its root id
    /// already: after this its path is the mount.
    pub async fn start(setup: MountSetup) -> io::Result<Arc<Mount>> {
        let MountSetup { root, store_path, store, source, report, pool, split_download_share, download_after, write_setup } = setup;
        let pinned_ids = Pins::load_pinned_ids(&store).await.map_err(to_io_error)?;
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let folder = root.path;
            // Before the mount: afterwards this path is the mount.
            let backing_directory = Arc::new(BackingDirectory::open(&folder)?);
            let directory_contents = Arc::new(DirectoryContents::new(TreeStore::open_read_only(&store_path).map_err(to_io_error)?, Arc::clone(&backing_directory)));
            let node_resolver = Arc::new(NodeResolver::new(Arc::clone(&directory_contents)));
            let access_mode = Arc::new(AccessMode::read_only());
            let outbox_recorder = Arc::new(OutboxRecorder::start(store.clone(), write_setup.tree_lock, Arc::clone(&access_mode))?);
            let upload_rules = Arc::new(UploadRules::new(write_setup.ignore));
            let conflict_copies = ConflictCopies::new(Arc::clone(&backing_directory), Arc::clone(&directory_contents), Arc::clone(&upload_rules), write_setup.machine_name);
            let open_writers = Arc::new(OpenWriters::default());
            let write_operations = Arc::new(WriteOperations::new(
                Arc::clone(&directory_contents),
                Arc::clone(&backing_directory),
                Arc::clone(&node_resolver),
                Arc::clone(&open_writers),
                Arc::clone(&outbox_recorder),
                upload_rules,
                Arc::clone(&access_mode),
            ));
            let download_states = Arc::new(DownloadStates::new(download_after));
            let pins = Arc::new(Pins::new(pinned_ids, store, Arc::clone(&directory_contents), runtime.clone()));
            let inotify_bridge = Arc::new(InotifyBridge::new(folder.clone(), Arc::clone(&node_resolver), Arc::clone(&directory_contents)));
            // All the downloader knows of the bridge: the way to announce a file's change.
            let announcing = Arc::clone(&inotify_bridge);
            let announce_file_change: FileChangeAnnouncer = Arc::new(move |rel, id| announcing.announce(vec![Announcement::FileShownChanged { rel, id }]));
            let file_downloader = Arc::new(FileDownloader {
                backing_directory: Arc::clone(&backing_directory),
                directory_contents: Arc::clone(&directory_contents),
                download_states: Arc::clone(&download_states),
                source: Arc::clone(&source),
                pool: Arc::clone(&pool),
                split_download_share,
                report: report.clone(),
                folder: folder.clone(),
                announce_file_change,
                runtime: runtime.clone(),
            });
            let range_readers = Arc::new(RangeReaders::new(pool, source, runtime.clone()));
            let open_files = Arc::new(OpenFiles::default());
            let interruptible_reads = Arc::new(InterruptibleReads::default());
            let files_interface = FilesInterface {
                folder: folder.clone(),
                directory_contents: Arc::clone(&directory_contents),
                download_states: Arc::clone(&download_states),
                file_downloader: Arc::clone(&file_downloader),
                open_files: Arc::clone(&open_files),
                pins: Arc::clone(&pins),
                inotify_bridge: Arc::clone(&inotify_bridge),
                report,
                runtime: runtime.clone(),
            };
            let kernel_requests = KernelRequests {
                access_mode: Arc::clone(&access_mode),
                backing_directory: Arc::clone(&backing_directory),
                directory_contents: Arc::clone(&directory_contents),
                node_resolver,
                open_files,
                open_writers,
                outbox_recorder: Arc::clone(&outbox_recorder),
                write_operations: Arc::clone(&write_operations),
                download_states: Arc::clone(&download_states),
                file_downloader: Arc::clone(&file_downloader),
                range_readers: Arc::clone(&range_readers),
                pins: Arc::clone(&pins),
                inotify_bridge: Arc::clone(&inotify_bridge),
                interruptible_reads: Arc::clone(&interruptible_reads),
                runtime: runtime.clone(),
            };
            let mut config = Config::default();
            config.mount_options = vec![
                MountOption::FSName("konedrive".into()),
                MountOption::Subtype("konedrive".into()),
                MountOption::NoSuid,
                MountOption::NoDev,
            ];
            config.n_threads = Some(4);
            config.clone_fd = true;
            let session = fuser::Session::new(kernel_requests, &folder, &config)?;
            let notifier = session.notifier();
            // From here on the daemon opens the folder by this descriptor: its path is the mount.
            crate::folder::root::mounted_over(&folder, Arc::new(backing_directory.clone_descriptor()?));
            let session = session.spawn()?;
            let connection = fs::metadata(&folder).map_or(0, |m| libc::minor(m.dev()));
            // With the mount over it the backing directory is the daemon's alone.
            backing_directory.restore_write_bits();
            inotify_bridge.start(notifier)?;
            download_pinned_files(&pins, &directory_contents, &file_downloader, &runtime);
            Ok(Arc::new(Mount {
                folder,
                access_mode,
                backing_directory,
                directory_contents,
                outbox_recorder,
                write_operations,
                conflict_copies,
                download_states,
                file_downloader,
                range_readers,
                pins,
                inotify_bridge,
                uncommitted_cycle: UncommittedCycle::default(),
                interruptible_reads,
                files_interface,
                runtime,
                session: Mutex::new(Some(session)),
                connection,
            }))
        })
        .await
        .map_err(to_io_error)?
    }

    /// Takes the mount off. The folder on disk then shows the downloaded copies alone.
    ///
    /// In this order: the recorder takes no more changes; the inotify bridge stops while the
    /// mount still answers it (what it does by path would otherwise land in the backing
    /// directory); every open that waits for a download is failed, and every read that waits
    /// for OneDrive is given up; and the connection is aborted before the mount is detached
    /// — a program that still holds a file of the mount open would otherwise keep the
    /// session, and this call, waiting for it.
    pub fn stop(&self) {
        let Some(session) = crate::panic::lock(&self.session).take() else { return };
        // The recorder ends with the last change it was given; what comes later is refused.
        self.outbox_recorder.stop();
        self.inotify_bridge.stop();
        self.download_states.fail_waiters();
        self.interruptible_reads.abort_all();
        if let Err(e) = fs::write(format!("/sys/fs/fuse/connections/{}/abort", self.connection), "1") {
            tracing::warn!("the connection of {} could not be aborted ({e}); its mount is taken off all the same", self.folder.display());
            // Not waited for: it ends when the last program lets go of the mount.
            let folder = self.folder.clone();
            std::thread::spawn(move || {
                let _ = session.umount_and_join();
                crate::folder::root::mount_gone(&folder);
                remove_write_bits(&folder);
            });
            return;
        }
        if let Err(e) = session.umount_and_join() {
            tracing::warn!("taking the mount off {}: {e}", self.folder.display());
        }
        crate::folder::root::mount_gone(&self.folder);
        // A plain directory again: without its write bits, so that nothing is put into it by
        // mistake.
        remove_write_bits(&self.folder);
    }

    /// [`stop`](Self::stop), for a folder that is forgotten: it is the user's again, to keep
    /// or to remove, and has its write bits.
    pub fn stop_and_restore_write_bits(&self) {
        self.stop();
        restore_write_bits(&self.folder);
    }

    /// The folder takes changes from now on, and `outbox_waker` is called for each one recorded:
    /// its sync uploads. Whether the modes shown changed with that.
    pub fn switch_to_read_write(&self, outbox_waker: OutboxWaker) -> bool {
        self.outbox_recorder.set_outbox_waker(Some(outbox_waker));
        let changed = self.access_mode.switch_to_read_write();
        if changed {
            // What a lost row left unsent is found again.
            let write_operations = Arc::clone(&self.write_operations);
            self.outbox_recorder.run(move || write_operations.record_unrecorded_entries());
        }
        changed
    }

    /// The folder takes no change from now on. Whether the modes shown changed with that.
    pub fn switch_to_read_only(&self) -> bool {
        self.outbox_recorder.set_outbox_waker(None);
        self.access_mode.switch_to_read_only()
    }

    /// Whether the folder takes changes now.
    pub fn is_read_write(&self) -> bool {
        self.access_mode.is_read_write()
    }

    /// Makes the backing directory follow the cycle of the delta listing that `store` has
    /// staged and not yet committed ([`cycle_step::apply_staged_cycle`]); `rw` is what
    /// the outbox holds, in a read-write folder.
    pub fn apply_staged_cycle(&self, store: &mut TreeStore, rw: Option<&Rw>) -> Result<CycleOutcome, TreeError> {
        cycle_step::apply_staged_cycle(store, rw, &self.backing_directory, &self.range_readers, &self.conflict_copies, &self.uncommitted_cycle)
    }

    /// The cycle [`apply_staged_cycle`](Self::apply_staged_cycle) applied is committed: its
    /// changes are announced to whoever watches the folder, and what is pinned and not
    /// downloaded is downloaded ([`cycle_step::announce_committed_cycle`]).
    pub fn announce_committed_cycle(&self) {
        cycle_step::announce_committed_cycle(&self.uncommitted_cycle, &self.pins, &self.inotify_bridge, &self.directory_contents, &self.file_downloader, &self.runtime, &self.outbox_recorder);
    }

    /// Downloads the file at `path` now, and waits for it.
    pub async fn download(&self, path: &Path) -> Result<(), FilesRefusal> {
        self.files_interface.download(path).await
    }

    /// Frees the space of the downloaded copies at and below `path`; what a pin keeps, a
    /// download is filling, or holds a change that is not uploaded yet, is left and counted.
    pub async fn free_up(&self, path: &Path) -> Result<FreedSpace, FilesRefusal> {
        self.files_interface.free_up(path).await
    }

    /// Frees the space of every downloaded copy that no pin keeps.
    pub async fn free_up_all(&self) -> FreedSpace {
        self.files_interface.free_up_all().await
    }

    /// How the file or directory of OneDrive at `path` stands: its state, its own pin, and
    /// the directory above that pins it.
    pub fn find_path_status(&self, path: &Path) -> Result<PathStatus, FilesRefusal> {
        self.files_interface.find_path_status(path)
    }

    /// Puts the pin on what is at `path`, or takes it off. A pin put on downloads what it
    /// keeps.
    pub async fn set_pin(&self, path: &Path, pinned: bool) -> Result<(), FilesRefusal> {
        self.files_interface.set_pin(path, pinned).await
    }

    /// How many files and directories carry a pin of their own.
    pub fn count_pins(&self) -> u64 {
        self.files_interface.count_pins()
    }

    /// How many downloaded copies, and bytes on disk, there are at and below `path`.
    pub fn count_downloaded_below(&self, path: &Path) -> Result<(u64, u64), FilesRefusal> {
        self.files_interface.count_downloaded_below(path)
    }

    /// The downloads that failed and can be continued: each one's size on disk and its
    /// checkpoint.
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn list_unfinished_downloads(&self) -> Vec<(u64, Option<u64>)> {
        let measure = |file: &Arc<fs::File>| Some((file.metadata().ok()?.len(), konedrive_fs::placeholder::read_progress(file).ok().flatten().map(|p| p.bytes)));
        self.download_states.list_unfinished_files().iter().filter_map(measure).collect()
    }

    /// The path of the backing directory, for what must not go through the mount: a walk of
    /// what is downloaded.
    pub fn backing_directory_path(&self) -> PathBuf {
        self.backing_directory.resolve_root_path()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.stop();
    }
}

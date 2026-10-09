//! A mount: made, served, and taken off.
//!
//! [`Mount`] holds the session the kernel's requests are answered in, the
//! [`FuseConnection`], and the parts of a served folder, and nothing else. What the folder's
//! sync, a cycle of the delta listing and the daemon's `Files` interface ask is answered by
//! a part: the [`CycleStep`], and the [`FilesInterface`], which it hands out. What it does
//! itself is call the parts in order, three times:
//!
//! - at the start ([`Mount::start`]): the parts are built ([`build_order`]), the folder is
//!   mounted over, and what waits for the mount — the write bits of the backing directory,
//!   the inotify bridge's thread, the downloads of what is pinned — is started;
//! - at the stop ([`Mount::stop`]): each part that runs something is stopped, and the mount
//!   is taken off;
//! - at a switch of the access mode: to read-write ([`Mount::switch_to_read_write`]), the
//!   outbox's waker, the switch, and the scan for unrecorded entries; to read-only
//!   ([`Mount::switch_to_read_only`]), the waker, and the switch on the write-operation
//!   thread, waited for.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fuser::{Config, MountOption};
use konedrive_tree::{TreeError, TreeStore};

use crate::local::held::Rw;

use super::access_mode_switch::{AccessMode, AccessModeSwitch};
use super::backing_directory::{remove_write_bits, restore_write_bits, BackingDirectory};
use super::cycle_step::{CycleOutcome, CycleStep};
use super::download_states::DownloadStates;
use super::files_interface::FilesInterface;
use super::fuse_connection::FuseConnection;
use super::inotify_bridge::InotifyBridge;
use super::kernel_requests::{InterruptibleReads, KernelRequests};
use super::outbox_recorder::{OutboxRecorder, OutboxWaker};
use super::pinned_file_downloader::PinnedFileDownloader;
use super::pins::Pins;
use super::unrecorded_entry_scan::UnrecordedEntryScan;
use super::write_operation_thread::WriteOperationThread;

mod build_order;
mod mount_setup;

pub use mount_setup::{MountSetup, WriteSetup};

/// A folder served as a mount. It lies over the folder from [`Mount::start`] until
/// [`Mount::stop`], or until this is dropped.
pub struct Mount {
    /// The folder's path, which is the mount.
    folder: PathBuf,
    /// The directory on disk the mount lies over ([`super::backing_directory`]).
    backing_directory: Arc<BackingDirectory>,
    /// Whether programs may change the folder ([`super::access_mode_switch`]).
    access_mode_switch: Arc<AccessModeSwitch>,
    /// The thread the write operations, and the switch of the access mode, run on
    /// ([`super::write_operation_thread`]).
    write_operation_thread: Arc<WriteOperationThread>,
    /// What records a change, and is told what wakes the outbox's worker
    /// ([`super::outbox_recorder`]).
    outbox_recorder: Arc<OutboxRecorder>,
    /// What finds the local-only entries the outbox has no row for
    /// ([`super::unrecorded_entry_scan`]).
    unrecorded_entry_scan: Arc<UnrecordedEntryScan>,
    /// The step of a cycle of the delta listing over the backing directory
    /// ([`super::cycle_step`]).
    cycle_step: Arc<CycleStep>,
    /// What the daemon's bus interface `Files` asks ([`super::files_interface`]).
    files_interface: FilesInterface,
    /// What brings a change from OneDrive to whoever watches the folder
    /// ([`super::inotify_bridge`]).
    inotify_bridge: Arc<InotifyBridge>,
    /// The state of each file's download ([`super::download_states`]).
    download_states: Arc<DownloadStates>,
    /// The reads that wait for a range from OneDrive ([`super::kernel_requests`]).
    interruptible_reads: Arc<InterruptibleReads>,
    /// What downloads what a pin keeps ([`super::pinned_file_downloader`]).
    pinned_file_downloader: Arc<PinnedFileDownloader>,
    /// The session the kernel's requests are answered in; `None` while the mount lies over
    /// nothing: before it is started, and once it is stopped.
    session: Mutex<Option<fuser::BackgroundSession>>,
    /// The mount's connection to the kernel; `None` before the mount is started.
    fuse_connection: Option<FuseConnection>,
}

fn to_io_error(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}

/// The end of every stop: the session is unmounted and waited for, the folder is opened by
/// its path again, and is a plain directory without its write bits, so that nothing is put
/// into it by mistake.
fn take_off(session: fuser::BackgroundSession, folder: &Path) {
    if let Err(e) = session.umount_and_join() {
        tracing::warn!("taking the mount off {}: {e}", folder.display());
    }
    crate::folder::root::mount_gone(folder);
    remove_write_bits(folder);
}

impl Mount {
    /// Mounts over `setup.root` and serves it. The folder on disk must carry its root id
    /// already: after this its path is the mount.
    pub async fn start(setup: MountSetup) -> io::Result<Arc<Mount>> {
        let pinned_ids = Pins::load_pinned_ids(&setup.store).await.map_err(to_io_error)?;
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            // Before the mount: afterwards the folder's path is the mount.
            let (mut mount, kernel_requests) = Mount::build(setup, pinned_ids, runtime)?;
            mount.mount_over_folder(kernel_requests)?;
            Ok(Arc::new(mount))
        })
        .await
        .map_err(to_io_error)?
    }

    /// Puts the mount over the folder, with `kernel_requests` answering it, and starts what
    /// waits for that.
    fn mount_over_folder(&mut self, kernel_requests: KernelRequests) -> io::Result<()> {
        let mut config = Config::default();
        config.mount_options = vec![MountOption::FSName("konedrive".into()), MountOption::Subtype("konedrive".into()), MountOption::NoSuid, MountOption::NoDev];
        config.n_threads = Some(4);
        config.clone_fd = true;
        let session = fuser::Session::new(kernel_requests, &self.folder, &config)?;
        let notifier = session.notifier();
        // From here on the daemon opens the folder by this descriptor: its path is the mount.
        crate::folder::root::mounted_over(&self.folder, Arc::new(self.backing_directory.clone_descriptor()?));
        let session = session.spawn()?;
        self.fuse_connection = Some(FuseConnection::of_mount_at(&self.folder));
        // With the mount over it the backing directory is the daemon's alone.
        self.backing_directory.restore_write_bits();
        self.inotify_bridge.start(notifier)?;
        self.pinned_file_downloader.download_pinned_files();
        *crate::panic::lock(&self.session) = Some(session);
        Ok(())
    }

    /// Takes the mount off. The folder on disk then shows the downloaded copies alone.
    ///
    /// In this order: the write-operation thread is stopped, which waits for the write
    /// operation that runs and drops those that wait; the inotify bridge
    /// stops while the mount still answers it (what it does by path would otherwise land in
    /// the backing directory); every open that waits for a download is failed, and every
    /// read that waits for OneDrive is given up; and the connection is aborted before the
    /// mount is detached — a program that still holds a file of the mount open would
    /// otherwise keep the session, and this call, waiting for it.
    pub fn stop(&self) {
        let Some(session) = crate::panic::lock(&self.session).take() else { return };
        // No write operation runs from here on: the one in flight has ended, those that
        // waited are dropped, and what comes later is refused.
        self.write_operation_thread.stop();
        self.inotify_bridge.stop();
        self.download_states.fail_waiters();
        self.interruptible_reads.abort_all();
        let aborted = self.fuse_connection.map_or(Ok(()), |fuse_connection| fuse_connection.abort());
        match aborted {
            Ok(()) => take_off(session, &self.folder),
            Err(e) => {
                tracing::warn!("the connection of {} could not be aborted ({e}); its mount is taken off all the same", self.folder.display());
                // Not waited for: it ends when the last program lets go of the mount.
                let folder = self.folder.clone();
                std::thread::spawn(move || take_off(session, &folder));
            }
        }
    }

    /// [`stop`](Self::stop), for a folder that is taken out of sync: it is the user's again, to keep
    /// or to remove, and has its write bits.
    pub fn stop_and_restore_write_bits(&self) {
        self.stop();
        restore_write_bits(&self.folder);
    }

    /// The folder takes changes from now on, and `outbox_waker` is called for each one recorded:
    /// its sync uploads. Whether the modes shown changed with that. When it changed the
    /// mode, what a lost row left unsent is found again by the scan, on the write-operation
    /// thread.
    ///
    /// This switch is made at once, and not on the write-operation thread: a write operation
    /// that began in a read-only folder was refused at its start, so none is under way that
    /// the switch could overtake. And the folder's sync switches after it started its first
    /// cycle, which holds the tree lock while it waits for the folder's state: a switch that
    /// waited for the thread there would wait for that cycle, and the cycle for it.
    pub fn switch_to_read_write(&self, outbox_waker: OutboxWaker) -> bool {
        self.outbox_recorder.set_outbox_waker(Some(outbox_waker));
        let changed = self.access_mode_switch.switch_to_read_write();
        if changed {
            let unrecorded_entry_scan = Arc::clone(&self.unrecorded_entry_scan);
            self.write_operation_thread.run(move || unrecorded_entry_scan.record_unrecorded_entries());
        }
        changed
    }

    /// The folder takes no change from now on. Whether the modes shown changed with that.
    ///
    /// The switch is made on the write-operation thread, and waited for: every write
    /// operation that was asked for before it has recorded what it did by the time this
    /// returns, and none records after it. The thread runs under the folder's tree lock, so
    /// the caller must not hold what a cycle that holds that lock waits for, unless it has
    /// told the cycle to stop: the folder's sync calls this with its cycle cancelled.
    pub async fn switch_to_read_only(&self) -> bool {
        self.outbox_recorder.set_outbox_waker(None);
        self.access_mode_switch.switch_to_read_only_on_thread(&self.write_operation_thread).await
    }

    /// Whether the folder takes changes now.
    pub fn is_read_write(&self) -> bool {
        self.access_mode_switch.access_mode() == AccessMode::ReadWrite
    }

    /// Makes the backing directory follow the cycle of the delta listing that `store` has
    /// staged and not yet committed ([`CycleStep::apply_staged_cycle`]); `rw` is what the
    /// outbox holds, in a read-write folder.
    pub fn apply_staged_cycle(&self, store: &mut TreeStore, rw: Option<&Rw>) -> Result<CycleOutcome, TreeError> {
        self.cycle_step.apply_staged_cycle(store, rw)
    }

    /// The cycle [`apply_staged_cycle`](Self::apply_staged_cycle) applied is committed: its
    /// changes are announced to whoever watches the folder, and what is pinned and not
    /// downloaded is downloaded ([`CycleStep::announce_committed_cycle`]).
    pub fn announce_committed_cycle(&self) {
        self.cycle_step.announce_committed_cycle();
    }

    /// The step of a cycle, for the tests that drive it without a mount.
    #[cfg(test)]
    pub(super) fn cycle_step(&self) -> &Arc<CycleStep> {
        &self.cycle_step
    }

    /// What answers the daemon's bus interface `Files` for this folder.
    pub fn files_interface(&self) -> &FilesInterface {
        &self.files_interface
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

#[cfg(test)]
pub(super) mod tests;

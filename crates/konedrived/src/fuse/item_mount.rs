//! A folder served as a FUSE mount over the rows: the holder of the mount's parts, what
//! it is made with, and the parts it hands to the folder's sync. What `mount` is today
//! over the directory contents and the outbox.
//!
//! **State owned:** the session and its connection, whether the mount was stopped, and
//! the parts, each of which owns its own state. [`ItemMount`] holds them, calls them in
//! order at its build, its stop and a switch of the access mode, and has no behaviour of
//! its own (`item_mount::build_order` has the order they are built in, and is the one
//! place a part of the mount is made).
//!
//! **The write bits of the folder on disk** are given back by the build, before the check
//! at start, and taken off again by the stop (and by a build that fails): the folder on
//! disk can be changed only while a mount is built over it. A Forget gives them back for
//! good.
//!
//! **Built, then served.** [`ItemMount::build`] makes every part, runs the check at start
//! and answers a mount that is NOT over the folder yet, with what answers the kernel's
//! requests. [`ItemMount::serve`] takes those and puts the mount over the folder with a
//! `fuser::Filesystem` it makes over them. Before the switch nothing implements that
//! `Filesystem` over `ItemKernelRequests`: the build and everything else here are filled
//! and tested now (step 0c); `serve`'s body is the switch's. The folder's start, its stop, its take-down and the switch
//! of the access mode are tested over a mount that was built and not served.
//!
//! **What the folder's sync takes from it:** the runner of the steps towards the folder,
//! the local contents for the steps towards OneDrive, the open handles (whether a file is
//! open for writing), the `Files` interface, the findings of the check at start, and the
//! switch of the access mode.
//!
//! **Tested without the rest**, over the fixture, with no kernel:
// test: the_parts_are_built_in_an_order_in_which_each_has_what_it_is_made_with
// test: the_check_at_start_runs_before_any_part_reads_the_rows
// test: the_entries_still_set_aside_are_removed_after_the_check_and_never_before
// test: a_mount_that_stops_aborts_the_downloads_fails_the_waiters_closes_the_reading_connections_and_drops_the_queued_jobs
// test: after_a_stop_and_the_drop_of_the_mount_no_clone_of_the_store_is_held_by_it
// test: a_switch_to_read_only_is_made_on_the_write_operation_thread
// test: a_build_that_fails_leaves_the_folder_without_its_write_bits
// test: a_stopped_mount_leaves_the_folder_without_its_write_bits_and_a_forget_gives_them_back
// test: the_flags_of_ignored_names_are_gone_over_as_a_job_of_the_write_operation_thread
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use konedrive_graph::pool::TransferPool;
use konedrive_tree::{DriveId, Store, TreeError};

use super::access_mode_switch::AccessModeSwitch;
use super::backing_directory::{remove_write_bits, BackingDirectory};
use super::folder_steps::FolderSteps;
use super::item_content_files::ItemContentFiles;
use super::item_files_interface::ItemFilesInterface;
use super::item_handles::ItemHandles;
use super::item_kernel_requests::ItemKernelRequests;
use super::mount_take_down::TakeDownReport;
use super::start_check::StartCheckFindings;
use super::upload_rules::UploadRules;
use crate::folder::published_plan::PublishedPlan;
use crate::folder::root::SyncRoot;
use crate::folder::running_steps::RunningSteps;
use crate::folder::sync_plan_wake::SyncPlanWake;
use crate::hydration::source::{ContentSource, Share};
use crate::local::SharedIgnore;
use crate::status::report::Report;

/// The order the parts of a mount are built in.
pub(in crate::fuse) mod build_order;

/// What a mount over the rows is made with: what the folder's start hands it.
pub struct ItemMountSetup {
    pub root: SyncRoot,
    /// The folder's tree store: its file, for the reading connections, and its one writer.
    pub store_path: PathBuf,
    pub store: Store,
    /// The folder's drive, for the check at start (`sync::sync_tasks::start_order`).
    pub drive_id: DriveId,
    pub source: Arc<dyn ContentSource>,
    pub report: Report,
    pub pool: Arc<TransferPool>,
    pub split_download_share: Arc<Share>,
    /// `[transfers] download_after`.
    pub download_after: u64,
    /// The folder's tree lock: each job of the write-operation thread runs under it, and
    /// each commit of a step.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list: what is never uploaded.
    pub ignore: SharedIgnore,
    /// Woken after every row a write operation writes.
    pub sync_plan_wake: Arc<SyncPlanWake>,
    /// Read for the sync mark and by `make`.
    pub running_steps: Arc<RunningSteps>,
    pub published_plan: Arc<PublishedPlan>,
    /// Who is told that the mount must be taken down: the owner of the folder's state.
    pub take_down_report: TakeDownReport,
    /// Unix seconds: the account's clock, which the tests hold. Asked once, at the build:
    /// the time the mount started at, which a directory with nothing on disk shows as its
    /// own (`item_attributes::decide_directory_mtime`).
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

/// A folder's mount over the rows.
pub struct ItemMount {
    backing_directory: Arc<BackingDirectory>,
    access_mode_switch: Arc<AccessModeSwitch>,
    folder_steps: Arc<FolderSteps>,
    item_content_files: Arc<ItemContentFiles>,
    item_handles: Arc<ItemHandles>,
    files_interface: Arc<ItemFilesInterface>,
    start_check_findings: StartCheckFindings,
    /// The store's writer and the upload rules: what the go-over of the ignored names
    /// runs with ([`rewrite_ignored_names`](Self::rewrite_ignored_names)).
    store: Store,
    upload_rules: Arc<UploadRules>,
    /// The parts a stop reaches, in the order it stops them (`build_order::PartsToStop`).
    parts_to_stop: build_order::PartsToStop,
    /// `None` until served.
    session: std::sync::Mutex<Option<fuser::BackgroundSession>>,
    /// Whether the mount was stopped: a second stop, and the drop after one, do nothing.
    is_stopped: AtomicBool,
}

fn to_io_error(error: impl ToString) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

impl ItemMount {
    /// Builds the parts (`build_order::build`, which runs the check at start, off the
    /// runtime) and answers the mount, not yet over the folder and read-only, with what
    /// answers the kernel's requests. The folder on disk must carry its root id already.
    /// Called by the folder's start (`sync::sync_tasks::start_order::start_folder_sync`);
    /// the store's error is that of the check at start. Step 0c's.
    pub async fn build(setup: ItemMountSetup) -> Result<(Arc<ItemMount>, ItemKernelRequests), ItemMountError> {
        let runtime = tokio::runtime::Handle::current();
        // Off the runtime: the check at start walks the backing directory and waits for
        // the store's thread.
        let (mount, kernel_requests) = tokio::task::spawn_blocking(move || build_order::build(setup, runtime)).await.map_err(|error| ItemMountError::Io(to_io_error(error)))??;
        Ok((Arc::new(mount), kernel_requests))
    }

    /// Mounts over the folder and serves it: `kernel_requests` is what the build gave
    /// with this mount, taken whole, and the `fuser::Filesystem` that answers the kernel
    /// through it is made here; then starts the bridge's thread with the session's
    /// notifier as the kernel's cached pages
    /// (`ItemInotifyBridge::start(read_connections, Some(Arc::new(session.notifier())))`):
    /// without it a read through the mount is answered the old content of a file a step
    /// changed, from the kernel's cache.
    /// After this the folder's path is the mount. Called by the folder's start, right
    /// after the build (`sync::sync_tasks::start_order::start_folder_sync`, stage 3); a
    /// failure leaves the mount built and not served, for the caller to stop. THE
    /// SWITCH'S: its body and the `Filesystem` are written with the switch.
    pub fn serve(self: &Arc<Self>, kernel_requests: ItemKernelRequests) -> std::io::Result<()> {
        todo!()
    }

    /// Stops the mount, once: the write-operation thread (the job in flight is waited
    /// for, the jobs that wait are dropped), the bridge, every download aborted and its
    /// waiters failed, and none started from then on (`ItemDownloader::stop`, which is
    /// `ItemDownloadStates::stop`), the session (when served)
    /// unmounted and waited for, the reading connections closed; then the write bits are
    /// taken off the folder on disk, through the backing directory's own descriptor: no
    /// mount lies over it, and nothing is put into it by mistake. After it, and once
    /// every `Arc` of the mount and of the parts it handed out is dropped — the
    /// `ItemKernelRequests` of the build among them —, the mount holds no clone of the
    /// store. Called at a Forget, at a take-down and when the mount is dropped. Step 0c's,
    /// but for what only a served mount has: the abort of its connection before the
    /// unmount and the daemon's own way to the folder (`folder::root::mount_gone`) come
    /// with `serve`.
    pub fn stop(&self) {
        if self.is_stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let session = crate::panic::lock(&self.session).take();
        let parts = &self.parts_to_stop;
        // No write operation, landing or step towards the folder runs from here on.
        parts.write_operation_thread.stop();
        parts.inotify_bridge.stop();
        parts.downloader.stop();
        if let Some(session) = session {
            if let Err(error) = session.umount_and_join() {
                tracing::warn!("taking the mount off its folder: {error}");
            }
        }
        parts.read_connections.close();
        remove_write_bits(&self.backing_directory.resolve_root_directory_path());
    }

    /// As [`stop`](Self::stop), and the folder left a plain directory with its write
    /// bits: a Forget.
    pub fn stop_and_restore_write_bits(&self) {
        self.stop();
        self.backing_directory.restore_write_bits();
    }

    /// The folder takes changes from now on. Whether it was read-only before. Made at
    /// once, not on the write-operation thread: a write operation that began in a
    /// read-only folder was refused at its start, so none is under way that the switch
    /// could overtake.
    pub fn switch_to_read_write(&self) -> bool {
        self.access_mode_switch.switch_to_read_write()
    }

    /// The folder refuses every change from now on: made as a job of the write-operation
    /// thread and awaited, so that no write operation is half done across it. Whether it
    /// was read-write before. After it the mount writes only the last closes of files
    /// that were open.
    pub async fn switch_to_read_only(&self) -> bool {
        self.access_mode_switch.switch_to_read_only_on_thread(&self.parts_to_stop.write_operation_thread).await
    }

    /// Goes over the flag of an ignored name of every row
    /// (`ItemStore::rewrite_ignored_names` with `UploadRules::is_name_ignored`), as one
    /// job of the write-operation thread, so under the tree lock: no write operation
    /// gives a name its flag by the old list after it. The mount takes no change for the
    /// one scan (reading 100 000 rows whole was measured at 0.02 s: design §13; only the
    /// rows whose flag changes are written). How many rows changed. A mount that is
    /// stopped answers an error: its thread runs no job.
    pub async fn rewrite_ignored_names(&self) -> Result<u64, TreeError> {
        let (store, upload_rules) = (self.store.clone(), Arc::clone(&self.upload_rules));
        let rewritten = self.parts_to_stop.write_operation_thread.run_and_wait(move || store.call_blocking(move |tree_store| tree_store.item_store().rewrite_ignored_names(&|name| upload_rules.is_name_ignored(name))));
        rewritten.await.map_err(|_| TreeError::Io(to_io_error("the mount is stopped: its write-operation thread runs no job")))?
    }

    /// The runner of the steps towards the folder: the plan's task's.
    pub fn folder_steps(&self) -> &Arc<FolderSteps> {
        &self.folder_steps
    }

    /// The local contents: the steps towards OneDrive's and the row steps'.
    pub fn item_content_files(&self) -> &Arc<ItemContentFiles> {
        &self.item_content_files
    }

    /// What programs hold open: whether a file is open for writing.
    pub fn item_handles(&self) -> &Arc<ItemHandles> {
        &self.item_handles
    }

    /// The `Files` interface of the bus.
    pub fn files_interface(&self) -> &Arc<ItemFilesInterface> {
        &self.files_interface
    }

    /// The holder of the access mode: the bus's `Writable`.
    pub fn access_mode_switch(&self) -> &AccessModeSwitch {
        &self.access_mode_switch
    }

    /// What the check at start found, before this mount went over the folder.
    pub fn start_check_findings(&self) -> &StartCheckFindings {
        &self.start_check_findings
    }

    /// The directory on disk the mount lies over: for the measure of the folder's space.
    pub fn backing_directory(&self) -> &BackingDirectory {
        &self.backing_directory
    }
}

/// A mount nobody holds any more is stopped.
impl Drop for ItemMount {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Why a mount could not be built or served.
#[derive(Debug)]
pub enum ItemMountError {
    /// The backing directory, the mount or a thread.
    Io(std::io::Error),
    /// The store, under the check at start: nothing was written.
    Store(TreeError),
}

impl From<std::io::Error> for ItemMountError {
    fn from(error: std::io::Error) -> Self {
        ItemMountError::Io(error)
    }
}

impl From<TreeError> for ItemMountError {
    fn from(error: TreeError) -> Self {
        ItemMountError::Store(error)
    }
}

#[cfg(test)]
mod tests;

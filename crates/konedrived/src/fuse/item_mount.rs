//! A folder served as a FUSE mount over the rows: the holder of the mount's parts, what
//! it is made with, and the parts it hands to the folder's sync. What `mount` is today
//! over the directory contents and the outbox.
//!
//! **State owned:** the session and its connection, and the parts, each of which owns its
//! own state. [`ItemMount`] holds them, calls them in order at its build, its stop and a
//! switch of the access mode, and has no behaviour of its own (`item_mount::build_order`
//! has the order they are built in, and is the one place a part of the mount is made).
//!
//! **Built, then served.** [`ItemMount::build`] makes every part, runs the check at start
//! and answers a mount that is NOT over the folder yet, with what answers the kernel's
//! requests. [`ItemMount::serve`] puts it over the folder with a `fuser::Filesystem`.
//! Before the switch nothing implements that `Filesystem` over `ItemKernelRequests`: the
//! build and everything else here are filled and tested now (step 0c); `serve`'s body and
//! its caller are the switch's. The folder's start, its stop, its take-down and the switch
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
// test: the_entries_left_set_aside_are_removed_before_the_check
// test: a_mount_that_stops_aborts_the_downloads_fails_the_waiters_closes_the_reading_connections_and_drops_the_queued_jobs
// test: after_a_stop_and_the_drop_of_the_mount_no_clone_of_the_store_is_held_by_it
// test: a_switch_to_read_only_is_made_on_the_write_operation_thread
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;
use std::sync::Arc;

use konedrive_graph::pool::TransferPool;
use konedrive_tree::{DriveId, Store, TreeError};

use super::access_mode_switch::AccessModeSwitch;
use super::backing_directory::BackingDirectory;
use super::folder_steps::FolderSteps;
use super::item_content_files::ItemContentFiles;
use super::item_files_interface::ItemFilesInterface;
use super::item_handles::ItemHandles;
use super::item_kernel_requests::ItemKernelRequests;
use super::mount_take_down::TakeDownReport;
use super::start_check::StartCheckFindings;
use crate::folder::published_plan::PublishedPlan;
use crate::folder::root::SyncRoot;
use crate::folder::running_steps::RunningSteps;
use crate::folder::sync_plan_wake::SyncPlanWake;
use crate::hydration::source::{ContentSource, Share};
use crate::local::SharedIgnore;
use crate::status::report::Report;

/// The order the parts of a mount are built in.
mod build_order;

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
    /// The parts a stop reaches, in the order it stops them (`build_order::PartsToStop`).
    parts_to_stop: build_order::PartsToStop,
    /// `None` until served.
    session: std::sync::Mutex<Option<fuser::BackgroundSession>>,
}

impl ItemMount {
    /// Builds the parts (`build_order::build`, which runs the check at start, off the
    /// runtime) and answers the mount, not yet over the folder and read-only, with what
    /// answers the kernel's requests. The folder on disk must carry its root id already.
    /// Called by the folder's start (`sync::sync_tasks::start_order::start_folder_sync`);
    /// the store's error is that of the check at start. Step 0c's.
    pub async fn build(setup: ItemMountSetup) -> Result<(Arc<ItemMount>, ItemKernelRequests), ItemMountError> {
        todo!()
    }

    /// Mounts over the folder and serves it with `filesystem`, which answers the kernel
    /// through the `ItemKernelRequests` the build gave; then starts the bridge's thread.
    /// After this the folder's path is the mount. THE SWITCH'S: its body, the
    /// `Filesystem` and its caller are written with the switch.
    pub fn serve<F: fuser::Filesystem + Send + 'static>(self: &Arc<Self>, filesystem: F) -> std::io::Result<()> {
        todo!()
    }

    /// Stops the mount: the write-operation thread (the jobs that wait are dropped), the
    /// bridge, every download aborted and its waiters failed (`ItemDownloader::stop`), the
    /// reads aborted, the session (when served) aborted and unmounted, the reading
    /// connections closed. After it, and once every `Arc` of the mount and of the parts it
    /// handed out is dropped, the mount holds no clone of the store. Called at a Forget,
    /// at a take-down and when the mount is dropped. Step 0c's.
    pub fn stop(&self) {
        todo!()
    }

    /// As [`stop`](Self::stop), and the folder left a plain directory with its write
    /// bits: a Forget.
    pub fn stop_and_restore_write_bits(&self) {
        todo!()
    }

    /// The folder takes changes from now on. Whether it was read-only before.
    pub fn switch_to_read_write(&self) -> bool {
        todo!()
    }

    /// The folder refuses every change from now on: made as a job of the write-operation
    /// thread and awaited, so that no write operation is half done across it. Whether it
    /// was read-write before. After it the mount writes only the last closes of files
    /// that were open.
    pub async fn switch_to_read_only(&self) -> bool {
        todo!()
    }

    /// Goes over the flag of an ignored name of every row
    /// (`ItemStore::rewrite_ignored_names` with `UploadRules::is_name_ignored`), as one
    /// job of the write-operation thread, so under the tree lock: no write operation
    /// gives a name its flag by the old list after it. The mount takes no change for the
    /// one scan (reading 100 000 rows whole was measured at 0.02 s: design §13; only the
    /// rows whose flag changes are written). How many rows changed.
    pub async fn rewrite_ignored_names(&self) -> Result<u64, TreeError> {
        todo!()
    }

    /// The runner of the steps towards the folder: the plan's task's.
    pub fn folder_steps(&self) -> &Arc<FolderSteps> {
        todo!()
    }

    /// The local contents: the steps towards OneDrive's and the row steps'.
    pub fn item_content_files(&self) -> &Arc<ItemContentFiles> {
        todo!()
    }

    /// What programs hold open: whether a file is open for writing.
    pub fn item_handles(&self) -> &Arc<ItemHandles> {
        todo!()
    }

    /// The `Files` interface of the bus.
    pub fn files_interface(&self) -> &Arc<ItemFilesInterface> {
        todo!()
    }

    /// The holder of the access mode: the bus's `Writable`.
    pub fn access_mode_switch(&self) -> &AccessModeSwitch {
        todo!()
    }

    /// What the check at start found, before this mount went over the folder.
    pub fn start_check_findings(&self) -> &StartCheckFindings {
        todo!()
    }

    /// The directory on disk the mount lies over: for the measure of the folder's space.
    pub fn backing_directory(&self) -> &BackingDirectory {
        todo!()
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

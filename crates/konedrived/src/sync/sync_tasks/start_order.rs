//! A folder's start: one function over a typed input, which runs the stages in their
//! order, each stage a function of its own.
//!
//! **State owned:** none.
//!
//! **Who calls it, and with what:** the owner of the folder's state (`SyncService`), inside
//! a change of the folder, with a [`FolderStartInput`]: everything the folder's record,
//! the account and the daemon's wiring hold that a start needs. The answer
//! ([`StartedFolder`]) is kept in the folder's state by that owner.
//!
//! **The stages, in order** ([`start_folder_sync`]):
//!
//! 1. [`open_folder_store`]: the store opened, and how it came to be (or the one the
//!    folder keeps from an earlier sync);
//! 2. [`decide_check_drive`]: the drive for the check at start;
//! 3. the leaf state made (`SyncPlanWake`, `RunningSteps`, `PublishedPlan`), then the
//!    mount built (`fuse::ItemMount::build`, or the one the folder keeps): inside it the
//!    check at start; read-only. Serving it over the folder (`ItemMount::serve`) is the
//!    switch's: until then this function is filled and tested over a mount that is built
//!    and not served;
//! 4. [`request_first_listing`]: a full listing asked for where the check bound entries
//!    by their marks;
//! 5. [`apply_wanted_access_mode`](super::access_mode_change::apply_wanted_access_mode):
//!    the mount's access mode and the store's state of sending, BEFORE any task plans;
//! 6. [`read_quota_before_first_plan`];
//! 7. [`start_sync_tasks`]: the listing's poller, then the plan's task. In either access
//!    mode.
//!
//! **Tested without the rest:**
// test: a_store_that_was_kept_asks_for_no_full_listing
// test: a_store_made_empty_lists_in_full_by_itself_and_is_asked_nothing
// test: entries_bound_by_their_marks_ask_for_a_full_listing
// test: the_drive_for_the_check_is_the_stores_else_the_recorded_else_the_accounts
// test: a_folder_whose_drive_is_not_known_is_not_mounted
// test: the_plans_task_is_started_in_a_read_only_folder_too
// test: the_access_mode_is_applied_before_the_first_plan
// test: a_folder_that_keeps_its_mount_runs_no_check_and_builds_no_mount
// test: every_part_is_made_once_and_given_to_whoever_holds_it
#![allow(dead_code, unused_variables)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_graph::drive::DriveClient;
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{DriveId, FullListingCause, Store, StoreOrigin};

use super::SyncTasks;
use crate::account::quota::Quota;
use crate::conditions::running::Running;
use crate::config::Mode;
use crate::folder::published_plan::PublishedPlan;
use crate::folder::root::SyncRoot;
use crate::folder::running_steps::RunningSteps;
use crate::folder::sync_plan_wake::SyncPlanWake;
use crate::fuse::{ItemMount, StartCheckFindings, TakeDownReport};
use crate::hydration::source::{ContentSource, Share};
use crate::local::SharedIgnore;
use crate::remote::listing::{DriveRecord, Lease, Neighbours, Schedule};
use crate::status::report::Report;
use crate::status::snapshot::SyncStateHandle;
use crate::sync::sync_plan_task::plan_input::PlanSettings;
use crate::upload::onedrive_space::OneDriveSpace;
use crate::upload::Limits;

/// What a folder keeps from an earlier sync of this run of the daemon: a mount outlives
/// a sync, and so does its store and what the mount was made with.
pub struct KeptFolderParts {
    pub store: Store,
    pub mount: Arc<ItemMount>,
    pub sync_plan_wake: Arc<SyncPlanWake>,
    pub running_steps: Arc<RunningSteps>,
    pub published_plan: Arc<PublishedPlan>,
}

/// Everything a folder's start needs.
pub struct FolderStartInput {
    pub root: SyncRoot,
    /// The store's file.
    pub tree_db: PathBuf,
    pub drive: DriveClient,
    /// The drive `config.toml` recorded for the account, and the one the account's
    /// profile says: for the check at start.
    pub recorded_drive: Option<DriveId>,
    pub account_drive: Option<DriveId>,
    pub drive_record: Option<DriveRecord>,
    pub neighbours: Option<Neighbours>,
    pub source: Arc<dyn ContentSource>,
    pub report: Report,
    pub pool: Arc<TransferPool>,
    pub split_download_share: Arc<Share>,
    /// `[transfers] download_after`.
    pub download_after: u64,
    /// The folder's tree lock, kept with the folder's record.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The folder's lease: a step towards the folder holds it.
    pub lease: Lease,
    pub ignore: SharedIgnore,
    pub state: SyncStateHandle,
    /// The account's one quota.
    pub quota: Quota,
    pub limits: Limits,
    pub settings: PlanSettings,
    pub schedule: Schedule,
    pub running: Arc<Running>,
    /// The thumbnail filler's nudge.
    pub after_listing: Option<Arc<tokio::sync::Notify>>,
    /// Unix seconds: the account's clock.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The access mode the account wants.
    pub wanted: Mode,
    /// Who is told that the mount must be taken down: the caller itself, which then calls
    /// `stop_order::take_folder_down` and this function again, inside a change of the
    /// folder.
    pub take_down_report: TakeDownReport,
    /// What the folder keeps from an earlier sync; `None` at the folder's first start and
    /// after a take-down.
    pub kept: Option<KeptFolderParts>,
}

/// A folder whose sync runs: what the owner of the folder's state keeps.
pub struct StartedFolder {
    pub store: Store,
    pub mount: Arc<ItemMount>,
    pub tasks: SyncTasks,
    pub sync_plan_wake: Arc<SyncPlanWake>,
    pub running_steps: Arc<RunningSteps>,
    pub published_plan: Arc<PublishedPlan>,
    pub onedrive_space: Arc<OneDriveSpace>,
    /// How the store came to be: said to the user where a rebuild lost something.
    pub store_origin: StoreOrigin,
}

/// Why a folder's sync did not start: said as the folder's blocking trouble; nothing
/// tries again by itself but for `DriveNotKnown`, which the account's sign-in nudges.
#[derive(Debug)]
pub enum StartRefusal {
    Store(String),
    /// Neither the store, nor `config.toml`, nor the account knows a drive yet: the check
    /// at start has none to bind an unknown mark in, and the mount is not built until it
    /// is known.
    DriveNotKnown,
    Mount(String),
}

/// Starts the folder's sync: the stages of the module's doc, in their order. Called by
/// the owner of the folder's state, inside a change of the folder, at the daemon's start,
/// at a registration, after a sync was stopped, and after a take-down.
pub async fn start_folder_sync(input: FolderStartInput) -> Result<StartedFolder, StartRefusal> {
    todo!()
}

/// The drive the check at start binds an unknown mark in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckDrive {
    Known(DriveId),
    NotKnownYet,
}

/// Opens the folder's store, making it or rebuilding it where
/// `konedrive_tree::decide_old_store` says, and answers how it came to be
/// (`TreeStore::open_with_origin`). Off the runtime. Today `kept_store`
/// (`outbox.rs:325-335`), which rebuilds silently.
async fn open_folder_store(tree_db: &Path) -> Result<(Store, StoreOrigin), StartRefusal> {
    todo!()
}

/// The drive for the check: the store's own (`meta` `drive_id`), else the one
/// `config.toml` recorded for the account, else the one the account's profile says. Pure.
pub fn decide_check_drive(store_drive: Option<DriveId>, recorded_drive: Option<DriveId>, account_drive: Option<DriveId>) -> CheckDrive {
    todo!()
}

/// Whether a full listing is asked for at this start, and why: a store that was kept, in
/// which the check bound entries by marks no row had. A store that was made empty has no
/// delta link and lists in full by itself. Pure.
pub fn decide_first_listing(origin: &StoreOrigin, findings: &StartCheckFindings) -> Option<FullListingCause> {
    todo!()
}

/// Writes [`decide_first_listing`]'s answer (`ItemStore::request_full_listing`). After the
/// mount was built, before the listing's poller starts.
async fn request_first_listing(store: &Store, origin: &StoreOrigin, findings: &StartCheckFindings) -> Result<(), StartRefusal> {
    todo!()
}

/// Reads the account's quota once, so that the first plan knows whether OneDrive is full
/// (`OneDriveSpace::apply_quota`); a read that fails leaves it as not full, and the first
/// 507 says otherwise.
async fn read_quota_before_first_plan(drive: &DriveClient, space: &OneDriveSpace, now: i64) {
    todo!()
}

/// Makes the parts of the sync, each once, in this order, and starts the tasks:
/// `SendingHolds::new(wake)`; `OneDriveSpace` (given, read before); `GraphAccess`;
/// `SessionCancellations::new`, `LostAnswers::new`, `MadeItems::new`, `NameHolders::new`,
/// `ContentUpload::new`; `OneDriveSteps::new` (with the tree lock and the mount's
/// `item_content_files`); `RowSteps::new`; `ListingCycle::new` and
/// `ListingPoller::start` (a first cycle at once; a full listing found under way goes on
/// from its link); `SyncPlanTask::new` with the runners and the mount's
/// `start_check_findings`, and its loop spawned. The plan's task sends nothing to
/// OneDrive until the first listing of this start ended
/// (`SendingHold::FirstListingNotEnded`).
fn start_sync_tasks(input: &FolderStartInput, store: &Store, mount: &Arc<ItemMount>, sync_plan_wake: &Arc<SyncPlanWake>, running_steps: &Arc<RunningSteps>, published_plan: &Arc<PublishedPlan>, onedrive_space: &Arc<OneDriveSpace>, drive_id: &DriveId) -> SyncTasks {
    todo!()
}

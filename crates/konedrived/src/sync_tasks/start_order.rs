//! A folder's start: one function over a typed input, which runs the stages in their
//! order, each stage a function of its own.
//!
//! **State owned:** none.
//!
//! **Who calls it, and with what:** the owner of the folder's state (`SyncService`), inside
//! a change of the folder, with a [`FolderStartInput`]: everything the folder's record,
//! the account and the daemon's wiring hold that a start needs. The answer
//! ([`StartedFolder`]) is kept in the folder's state by that owner; a start that is
//! refused answers a [`RefusedStart`].
//!
//! **The stages, in order** ([`start_folder_sync`]). A folder that keeps its parts from an
//! earlier sync (`FolderStartInput::kept`) skips the stages before 1 and 1 to 4: its store
//! is open, its mount is served, its check was run, and nothing asks for a full listing;
//! `StartedFolder::store_origin` is then `StoreOrigin::Kept`.
//!
//! 0. [`cancel_upload_sessions_of_old_store`]: the upload sessions a store of an older
//!    version holds are cancelled in OneDrive before stage 1 removes that store;
//! 1. [`open_folder_store`]: the store opened, and how it came to be. Then the activity
//!    log is attached to it (`Report::activity`, `Activity::attach`, off the runtime's
//!    threads) and the store's `last_checked` put into `state`: the listing's cycle
//!    prunes the log through the store, and the bus shows the time before the first cycle
//!    ends;
//! 2. [`decide_check_drive`]: the drive for the check at start;
//! 3. the leaf state made (`SyncPlanWake`, `RunningSteps`, `PublishedPlan`), then the
//!    mount built (`mount::ItemMount::build`: inside it the check at start; read-only) and
//!    served (`ItemMount::serve`, which takes the `ItemKernelRequests` the build
//!    answered): from here on the folder's path is the mount. Around the build
//!    `LocalScan` on the bus says the check (`state`'s `local.scan`: running with the
//!    time it began, then finished with the time it took and, from
//!    `StartCheckFindings`, how much it left) — this function is the one writer of it;
//! 4. [`request_first_listing`]: a full listing asked for where the check bound entries
//!    by their marks;
//! 5. `SendingHolds::new(wake)` with `input.standing_holds` set on it, then
//!    [`apply_wanted_access_mode`](super::access_mode_change::apply_wanted_access_mode):
//!    the mount's access mode and the hold of a read-only folder, BEFORE any task plans;
//! 6. `OneDriveSpace::new(input.quota)`, then [`read_quota_before_first_plan`];
//! 7. [`start_sync_tasks`]: the listing's poller, the thumbnail filler, the plan's task.
//!    In either access mode. Nothing is awaited in it.
//!
//! **Two functions.** Stages 5 to 7 are [`start_sync_over_parts`], over the parts of a
//! folder whether they were kept or made in this start, and whether the mount is served
//! or only built: it stops nothing and gives the parts back when it refuses.
//! [`start_folder_sync`] is stages 0 to 4 for a folder that keeps nothing, then that
//! function, and what is undone after a refusal. It is never cut: the owner runs it in a
//! task of its own (`sync_tasks`' module doc).
//!
//! **Until `ItemMount::serve` is written** (the switch's; its body is `todo!()`), a test
//! cannot pass stage 3 of `start_folder_sync` with nothing kept. So the tests of stages 5
//! to 7 call `start_sync_over_parts` with a mount built and not served (the fixture's);
//! the tests of stages 0 to 2 and 4 call the stage's own function; the tests of
//! `start_folder_sync` itself run with kept parts, or end at a refusal of stage 1 or 2.
//! The three that need the whole function past stage 3 with nothing kept are written
//! `#[ignore]`d with the reason "waits for ItemMount::serve", and are marked below.
//!
//! **Tested without the rest:**
// test: a_store_that_was_kept_asks_for_no_full_listing
// test: a_store_made_empty_lists_in_full_by_itself_and_is_asked_nothing
// test: entries_bound_by_their_marks_ask_for_a_full_listing
// test: the_drive_for_the_check_is_the_stores_else_the_recorded_else_the_accounts
// test: a_folder_whose_drive_is_not_known_is_not_mounted
// test: the_plans_task_is_started_in_a_read_only_folder_too
// test: the_access_mode_is_applied_before_the_first_plan
// test: a_folder_that_keeps_its_parts_opens_no_store_runs_no_check_builds_no_mount_and_asks_for_no_full_listing
// test: every_part_is_made_once_and_given_to_whoever_holds_it
// test: the_holds_that_stand_at_the_start_are_set_before_the_first_plan
// test: a_refused_start_gives_back_the_parts_it_was_given_as_kept
// test: a_refused_start_stops_the_mount_it_built (ignored: waits for ItemMount::serve)
// test: a_mount_that_cannot_be_served_is_stopped_and_refused_as_the_mounts (ignored: waits for ItemMount::serve)
// test: a_start_with_nothing_kept_runs_every_stage_in_its_order (ignored: waits for ItemMount::serve)
// test: the_parts_are_given_back_by_a_start_over_them_that_is_refused
// test: a_refused_start_says_what_the_rebuild_of_its_store_lost
// test: the_upload_sessions_of_a_store_of_an_older_version_are_cancelled_before_it_is_removed
// test: a_quota_read_at_the_start_that_has_space_forgets_the_refusals_that_fit
#![allow(dead_code, unused_variables)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_graph::drive::DriveClient;
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{DriveId, FullListingCause, RebuildLoss, Store, StoreOrigin};

use super::SyncTasks;
use crate::account::quota::Quota;
use crate::conditions::running::Running;
use crate::config::Mode;
use crate::sync_parts::published_plan::PublishedPlan;
use crate::folder::root::SyncRoot;
use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::sync_plan_wake::SyncPlanWake;
use crate::mount::{ItemMount, StartCheckFindings, TakeDownReport};
use crate::content_source::source::{ContentSource, Share};
use crate::local::SharedIgnore;
use crate::sync_parts::lease::Lease;
use crate::listing::listing_cycle::{DriveRecord, Neighbours};
use crate::listing::listing_poller::Schedule;
use crate::status::report::Report;
use crate::status::snapshot::SyncStateHandle;
use crate::sync_plan_task::plan_input::PlanSettings;
use crate::sync_parts::onedrive_space::OneDriveSpace;
use crate::onedrive_steps::content_upload::Limits;

/// What a folder keeps from an earlier sync of this run of the daemon: a mount outlives
/// a sync, and so does its store and what the mount was made with. Answered by
/// `stop_order::stop_folder_sync`, given back by a start that is refused, and ended by
/// `stop_order::take_folder_down` or `stop_order::forget_folder`.
///
/// The ONE type of what a stopped sync keeps: the owner of the folder's state holds this
/// very value between a stop and the next start (`account_folder::folder::Kept::parts`),
/// never its fields apart, and gives it back as `FolderStartInput::kept`. A clone shares
/// every part (each is a handle); the owner drops every clone before a take-down or a
/// Forget closes the store.
#[derive(Clone)]
pub struct KeptFolderParts {
    pub store: Store,
    /// Built and served.
    pub mount: Arc<ItemMount>,
    pub sync_plan_wake: Arc<SyncPlanWake>,
    pub running_steps: Arc<RunningSteps>,
    pub published_plan: Arc<PublishedPlan>,
    /// The folder's drive: the one the check at start was given when the mount was built
    /// (`CheckDrive::Known`). The store's own `drive_id` may still be empty when a sync
    /// was stopped before its first cycle, so the next start takes it from here.
    pub drive_id: DriveId,
}

/// What holds all sending when a folder's sync starts, as the account has it then: set
/// on the sync's `SendingHolds` before its plan's loop is spawned, so that the first plan
/// starts nothing a pause or a sign-out holds (a download of a pinned file is held by
/// nothing else). What changes afterwards is set by its own cause through
/// `SyncTasks::sending_holds`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StandingSendingHolds {
    /// `None`: not paused. `Some(None)`: paused until resumed. `Some(Some(t))`: paused
    /// until `t`, Unix seconds. As `SendingHolds::set_paused` takes it. The account's
    /// background work is stopped by the user's pause and by the automatic hold alike
    /// (metered connection, battery: `Running::stop`), as today: the user's pause is
    /// given with its end, a hold as `Some(None)` — it ends when the owner says so
    /// (`show_pause`), by no time. Both read `SendingHold::Paused`.
    pub paused_until: Option<Option<i64>>,
    pub is_signed_out: bool,
    /// Why the account may not write to OneDrive now, as the account says it; `None` when
    /// it may. As `SendingHolds::set_writes_refusal` takes it. Computed by the owner with
    /// the same function its watcher of the account sets it by afterwards
    /// (`account_folder::mode::decide_account_holds`), from the account's snapshot read
    /// when the start begins.
    pub writes_refusal: Option<String>,
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
    /// What the plan is given that does not change while the sync runs
    /// (`PlanSettings::of` the account's machine name).
    pub settings: PlanSettings,
    pub schedule: Schedule,
    pub running: Arc<Running>,
    /// Where the thumbnails of the folder's files are written; `None` with no cache to
    /// fill: no thumbnail filler is started then.
    pub thumbnail_cache: Option<PathBuf>,
    /// What holds all sending now.
    pub standing_holds: StandingSendingHolds,
    /// Unix seconds: the account's clock.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The access mode the account wants.
    pub wanted: Mode,
    /// Who is told that the mount must be taken down: the caller itself
    /// (`SyncService::take_down_report`), whose task then stops the sync, calls
    /// `stop_order::take_folder_down` inside a change of the folder, and starts the folder
    /// again after its pause.
    pub take_down_report: TakeDownReport,
    /// Called by the plan's task after every published plan with how many changes are
    /// unsent (`SyncPlanTaskParts::unsent_count_report`): the owner tells the account
    /// (`FolderAccount::tell_unsent_count`), which may show the count and wait for 0 with
    /// a switch to read-only. It must not block.
    pub unsent_count_report: Arc<dyn Fn(u64) + Send + Sync>,
    /// What the folder keeps from an earlier sync; `None` at the folder's first start and
    /// after a take-down. With kept parts the fields the mount was made with at its build
    /// are not read: `tree_db`, `recorded_drive`, `account_drive`, `source`,
    /// `split_download_share`, `download_after`, `take_down_report`, and `root` and
    /// `clock` as far as the mount took them. `tree_lock`, `ignore` and `lease` must be
    /// the very values of that first start: the kept mount holds them, and the runners
    /// made now must share them with it.
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
    /// The folder's drive, as [`KeptFolderParts::drive_id`]: `stop_order::stop_folder_sync`
    /// passes it on.
    pub drive_id: DriveId,
    /// How the store came to be: said to the user where a rebuild lost something. `Kept`
    /// for a folder that was started with the parts it kept. Read by the owner's
    /// `start_sync` right after the start, once (`describe_rebuild_loss`).
    pub store_origin: StoreOrigin,
}

/// Why a folder's sync did not start: said as the folder's blocking trouble. Nothing of
/// this layer tries again by itself: no task exists after a refusal.
#[derive(Debug)]
pub enum StartRefusal {
    /// The store: it cannot be opened, or a read or a write of it failed in a stage.
    Store(String),
    /// Neither the store, nor `config.toml`, nor the account knows a drive yet: the check
    /// at start has none to bind an unknown mark in, and the mount is not built until it
    /// is known. No task of the sync exists then, so nothing here watches the account:
    /// the owner of the folder's state starts the folder again when the account changes
    /// (its sign-in, its profile read): its watcher of the account does
    /// (`account_folder::mode::watch_account`, for a sync that is
    /// `Why::CannotStart` with `StartRefusal::DriveNotKnown`).
    DriveNotKnown,
    /// The backing directory, the mount or a thread of it.
    Mount(String),
}

/// A start that was refused: why, and what is left of it.
pub struct RefusedStart {
    pub refusal: StartRefusal,
    /// Read by the owner's `start_sync`, as `StartedFolder::store_origin` is.
    /// What the rebuild of the store lost, when stage 1 of THIS start rebuilt a store: the
    /// next start finds the new store as one that was kept and cannot say it any more.
    /// `None` when no store was rebuilt, and when nothing could be counted in the one
    /// that was.
    pub rebuild_loss: Option<RebuildLoss>,
    /// The parts the start was given as kept, given back as they were: the mount still
    /// served, the store open. `None` when none were given.
    pub kept: Option<KeptFolderParts>,
}

/// Starts the folder's sync: the stages of the module's doc, in their order. Called by
/// the owner of the folder's state, inside a change of the folder, at the daemon's start,
/// at a registration, after a sync was stopped, and at the user's refresh of a folder
/// that is in trouble. It relies on never being cut between two stages: the owner runs it
/// in a task of its own and awaits that task (`sync_tasks`' module doc).
///
/// With kept parts it is [`start_sync_over_parts`] over them, with `StoreOrigin::Kept`.
/// With nothing kept it runs stages 0 to 4, makes the parts of what it opened and built
/// (the drive that of stage 2), and calls the same function.
///
/// **On refusal**, by the stage that failed:
///
/// - 0 fails nothing: a session that cannot be cancelled is logged.
/// - 1: the store cannot be opened — `Store`. Nothing to undo. The attach of the activity
///   log and the read of `last_checked` fail nothing: a warning, and the time stays 0.
/// - 2: the read of the store's own drive fails — `Store`; no drive is known —
///   `DriveNotKnown`. The activity log is detached and the store dropped.
/// - 3: `ItemMountError::Store` (the check at start) — `Store`; any other
///   `ItemMountError`, and a `serve` that fails — `Mount`. The build takes the write bits
///   off again by itself; a mount that was built and not served is stopped.
/// - 4: `request_first_listing` — `Store`.
/// - 5: `apply_wanted_access_mode`'s `Err` — `Mount` (the mount's thread is gone).
/// - 6 fails nothing: a quota that cannot be read leaves OneDrive as not full.
/// - 7 cannot fail.
///
/// What is undone after 3: a mount built in THIS start is stopped (`ItemMount::stop`, off
/// the runtime's threads: it joins a thread and unmounts), the activity log detached, and
/// the store of this start dropped. Parts given as kept are never stopped or dropped
/// here: they are given back in [`RefusedStart::kept`], and with them only 5 and 6 can be
/// reached. `RefusedStart::rebuild_loss` is what stage 1 answered.
pub async fn start_folder_sync(input: FolderStartInput) -> Result<StartedFolder, RefusedStart> {
    todo!()
}

/// Stages 5 to 7 over the parts of a folder: the holds made and the access mode applied
/// (`SendingHolds::new`, `input.standing_holds`, `apply_wanted_access_mode`),
/// `OneDriveSpace::new(input.quota)` and
/// [`read_quota_before_first_plan`], then [`start_sync_tasks`] with `parts.drive_id`. The
/// answer holds the parts, the tasks, the space and `store_origin` as given.
///
/// `input.kept` is not read: the parts are `parts`, kept or made by the caller in this
/// start. The mount of `parts` is built; whether it is served is not asked, so a test
/// gives one that is not. A refusal (stage 5's `Err`: `StartRefusal::Mount`) stops
/// nothing and drops nothing: the parts are given back with it, and the caller —
/// [`start_folder_sync`] — gives back the ones it was given as kept and ends the ones it
/// made.
async fn start_sync_over_parts(input: &FolderStartInput, parts: KeptFolderParts, store_origin: StoreOrigin) -> Result<StartedFolder, (StartRefusal, KeptFolderParts)> {
    todo!()
}

/// How long the start waits for OneDrive to cancel the upload sessions of a store of an
/// older version, all together. A guess.
const OLD_SESSIONS_CANCELLED_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// The drive the check at start binds an unknown mark in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckDrive {
    Known(DriveId),
    NotKnownYet,
}

/// Cancels in OneDrive the upload sessions a store of an older version holds
/// (`konedrive_tree::list_old_store_upload_sessions`, off the runtime's threads; then
/// `DriveClient::cancel_upload` for each), so that their empty files do not hold names
/// there once the store that knew them is removed (design §13 (6)). Before
/// [`open_folder_store`], which removes that store. Nothing for a store of today's
/// version, for no store, and for one whose sessions cannot be read. A session that
/// cannot be cancelled now — no network, signed out — is logged and left to expire in
/// OneDrive, and the start is not refused for it. All are asked at once and awaited
/// together under ONE bound ([`OLD_SESSIONS_CANCELLED_WITHIN`]): what has not answered by
/// then is dropped with its request, logged as not cancelled, and left to expire. No
/// task outlives this function.
async fn cancel_upload_sessions_of_old_store(tree_db: &Path, drive: &DriveClient) {
    todo!()
}

/// Opens the folder's store, making it or rebuilding it where
/// `konedrive_tree::decide_old_store` says, and answers how it came to be
/// (`TreeStore::open_with_origin`). Off the runtime.
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
/// delta link and lists in full by itself. Pure. Not asked for a folder that was started
/// with the parts it kept: its check ran at an earlier start, which asked already.
pub fn decide_first_listing(origin: &StoreOrigin, findings: &StartCheckFindings) -> Option<FullListingCause> {
    todo!()
}

/// Writes [`decide_first_listing`]'s answer (`ItemStore::request_full_listing`). After the
/// mount was built, before the listing's poller starts. A write that fails is
/// `StartRefusal::Store`.
async fn request_first_listing(store: &Store, origin: &StoreOrigin, findings: &StartCheckFindings) -> Result<(), StartRefusal> {
    todo!()
}

/// Reads the account's quota once, so that the first plan knows whether OneDrive is full
/// (`OneDriveSpace::apply_quota`); a read that fails leaves it as not full, and the first
/// 507 says otherwise. On `SpaceChange::HasSpace` the refusals for lack of space that fit
/// now are forgotten (`StepMemory::forget_fitting_space_refusals(free_space)`, through
/// `store`): the step memory keeps them across a restart, and after a start that finds
/// OneDrive not full no later read of the quota is due that would. A forgetting that
/// fails is a warning.
async fn read_quota_before_first_plan(drive: &DriveClient, space: &OneDriveSpace, store: &Store, now: i64) {
    todo!()
}

/// Makes the parts of the sync, each once, in this order, and starts the tasks. Nothing
/// is awaited here.
///
/// The stop's token is made first: every task and the step runners get a clone, and
/// `SyncTasks::stop_handle` hands it out. `sending_holds` is given: stage 5 made it and
/// set on it what stands (`input.standing_holds`: `set_paused`, `set_signed_out`,
/// `set_writes_refusal`; a read-only folder's hold) — BEFORE the plan's loop is spawned;
/// `OneDriveSpace` (given, read before); `GraphAccess`;
/// `SessionCancellations::new`, `LostAnswers::new`, `MadeItems::new`, `NameHolders::new`,
/// `ContentUpload::new`; `OneDriveSteps::new` (with the tree lock and the mount's
/// `item_content_files`); `RowSteps::new`; `StepRunners::of_runners` (the mount's
/// `folder_steps`, the lease, the stop's token: a step towards the folder that waits for
/// the lease when the stop comes ends as `NotRun(SendingStopped)`).
///
/// The tasks, in this order: `ListingCycle::new` (its `after_listing` a `Notify` made
/// here, the thumbnail filler's nudge) and `ListingPoller::start` (a first cycle at once;
/// a full listing found under way goes on from its link); the thumbnail filler where
/// `input.thumbnail_cache` names a cache (`ThumbnailFiller::new(..).spawn(nudge, stop)`);
/// `SyncPlanTask::new` with the runners, the mount's `start_check_findings`, a
/// `quota_reader` that asks `input.drive` for the quota (`DriveClient::quota`, `None`
/// when it could not be read), the activity log (`input.report.activity`), the
/// announcer of sync marks (`FolderSteps::announce_marks_changed` of the mount's
/// `folder_steps`), the thumbnail filler's nudge as `after_folder_steps` (the same
/// `Notify` the cycle was given) and `input.unsent_count_report`, and its loop spawned
/// with the stop's token. No task watches the account: what its state holds is set and
/// ended by the owner's watcher. The poller's handle is cloned for the plan's task and
/// `SyncTasks::poll`. The plan's
/// task sends nothing to OneDrive until the first listing of this start ended
/// (`SendingHold::FirstListingNotEnded`).
#[allow(clippy::too_many_arguments)]
fn start_sync_tasks(input: &FolderStartInput, store: &Store, mount: &Arc<ItemMount>, sync_plan_wake: &Arc<SyncPlanWake>, running_steps: &Arc<RunningSteps>, published_plan: &Arc<PublishedPlan>, sending_holds: &Arc<crate::sync_parts::sending_holds::SendingHolds>, onedrive_space: &Arc<OneDriveSpace>, drive_id: &DriveId) -> SyncTasks {
    todo!()
}

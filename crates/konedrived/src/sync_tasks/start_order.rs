//! A folder's start: one function over a typed input, which runs the stages in their
//! order, each stage a function of its own.
//!
//! **State owned:** none.
//!
//! **Who calls it, and with what:** the owner of the folder's state (`SyncService`), inside
//! a change of the folder, with a `FolderStartInput`: everything the folder's record,
//! the account and the daemon's wiring hold that a start needs. The answer
//! (`StartedFolder`) is kept in the folder's state by that owner; a start that is
//! refused answers a `RefusedStart`.
//!
//! **The stages, in order** (`start_folder_sync`). A folder that keeps its parts from an
//! earlier sync (`FolderStartInput::kept`) skips the stages before 1 and 1 to 4: its store
//! is open, its mount is served, its check was run, and nothing asks for a full listing;
//! `StartedFolder::store_origin` is then `StoreOrigin::Kept`.
//!
//! 0. `cancel_upload_sessions_of_old_store`: the upload sessions a store of an older
//!    version holds are cancelled in OneDrive before stage 1 removes that store;
//! 1. `open_folder_store`: the store opened, and how it came to be. Then the activity
//!    log is attached to it (`Report::activity`, `Activity::attach`, off the runtime's
//!    threads) and the store's `last_checked` put into `state`: the listing's cycle
//!    prunes the log through the store, and the bus shows the time before the first cycle
//!    ends;
//! 2. `decide_check_drive`: the drive for the check at start;
//! 3. the leaf state made (`SyncPlanWake`, `RunningSteps`, `PublishedPlan`), then the
//!    mount built (`mount::ItemMount::build`: inside it the check at start; read-only) and
//!    served (`ItemMount::serve`, which takes the `ItemKernelRequests` the build
//!    answered): from here on the folder's path is the mount. Around the build
//!    `LocalScan` on the bus says the check (`state`'s `local.scan`: running with the
//!    time it began, then finished with the time it took and, from
//!    `StartCheckFindings`, how much it left) — this function is the one writer of it;
//! 4. `request_first_listing`: a full listing asked for where the check bound entries
//!    by their marks;
//! 5. `SendingHolds::new(wake)` with `input.standing_holds` set on it, then
//!    `apply_wanted_access_mode`:
//!    the mount's access mode and the hold of a read-only folder, BEFORE any task plans;
//! 6. `OneDriveSpace::new(input.quota)`, then `read_quota_before_first_plan`;
//! 7. `start_sync_tasks`: the runner of the steps towards the folder made of the mount's
//!    parts (`make_folder_steps`), then the listing's poller, the thumbnail filler, the
//!    plan's task. In either access mode. Nothing is awaited in it.
//!
//! **Two functions.** Stages 5 to 7 are `start_sync_over_parts`, over the parts of a
//! folder whether they were kept or made in this start, and whether the mount is served
//! or only built: it stops nothing and gives the parts back when it refuses.
//! `start_folder_sync` is stages 0 to 4 for a folder that keeps nothing, then that
//! function, and what is undone after a refusal. It is never cut: the owner runs it in a
//! task of its own (`sync_tasks`' module doc).
//!
//! **A test puts no mount over a folder** (`ItemMount::serve` mounts, which a test of the
//! daemon does not do outside the VM). So the tests of stages 5 to 7 call
//! `start_sync_over_parts` with a mount built and not served (the fixture's); the tests
//! of stages 0 to 2 and 4 call the stage's own function; the tests of `start_folder_sync`
//! itself run with kept parts, or end at a refusal of stage 1 or 2. The whole function
//! past stage 3 with nothing kept runs only in the VM suite.
//!
//! **Tested without the rest:**
// test: the_drive_for_the_check_is_the_stores_else_the_recorded_else_the_accounts
// test: a_folder_whose_drive_is_not_known_is_not_mounted
// test: a_folder_that_keeps_its_parts_opens_no_store_runs_no_check_builds_no_mount_and_asks_for_no_full_listing
// test: the_holds_that_stand_at_the_start_are_set_before_the_first_plan
// test: a_refused_start_gives_back_the_parts_it_was_given_as_kept
// test: the_parts_are_given_back_by_a_start_over_them_that_is_refused
// test: a_refused_start_says_what_the_rebuild_of_its_store_lost
// test: the_upload_sessions_of_a_store_of_an_older_version_are_cancelled_before_it_is_removed
// test: a_full_listing_is_asked_for_only_where_the_check_bound_entries_of_a_kept_store_by_their_marks
// test: the_check_at_start_is_said_as_a_scan_that_runs_and_then_as_one_that_finished
// test: the_folders_store_is_made_where_there_is_none_kept_where_there_is_one_and_refused_where_it_cannot_be_opened
// test: entries_bound_by_their_marks_ask_the_store_for_a_full_listing_and_nothing_else_does
// test: a_quota_read_at_the_start_says_whether_onedrive_is_full_before_the_first_plan
// test: the_sessions_of_a_store_of_todays_version_and_of_no_store_are_not_cancelled
// test: a_onedrive_that_does_not_answer_holds_the_start_for_the_one_bound_and_no_longer
// test: the_plans_task_is_started_in_a_read_only_folder_too_and_the_access_mode_is_applied_before_its_first_plan
// test: a_start_over_parts_answers_them_with_its_tasks_and_the_origin_it_was_given
// test: a_start_whose_store_cannot_be_opened_is_refused_with_nothing_left

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_graph::drive::{DriveClient, DriveQuota};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{DriveId, FullListingCause, RebuildLoss, Store, StoreOrigin};

use tokio_util::sync::CancellationToken;

use super::access_mode_change::apply_wanted_access_mode;
use super::SyncTasks;
use crate::desktop::thumbs::ThumbnailFiller;
use crate::listing::listing_cycle::{ListingCycle, ListingCycleSetup};
use crate::listing::listing_poller::ListingPoller;
use crate::mount::{ItemMountError, ItemMountSetup};
use crate::onedrive_steps::content_upload::ContentUpload;
use crate::onedrive_steps::lost_answers::LostAnswers;
use crate::onedrive_steps::made_items::MadeItems;
use crate::onedrive_steps::name_holders::NameHolders;
use crate::onedrive_steps::session_cancellations::SessionCancellations;
use crate::onedrive_steps::{GraphAccess, OneDriveSteps};
use crate::status::snapshot::{LocalScan, ScanState};
use crate::sync_parts::onedrive_space::SpaceChange;
use crate::sync_parts::sending_holds::SendingHolds;
use crate::sync_plan_task::row_steps::RowSteps;
use crate::sync_plan_task::step_runners::{RunningFuture, StepRunners};
use crate::sync_plan_task::{SyncPlanTask, SyncPlanTaskParts};
use crate::account::quota::Quota;
use crate::account::state::ModeNote;
use crate::conditions::running::Running;
use crate::config::Mode;
use crate::sync_parts::published_plan::PublishedPlan;
use crate::folder::root::SyncRoot;
use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::sync_plan_wake::SyncPlanWake;
use crate::folder_steps::FolderSteps;
use crate::mount::{FolderStepParts, ItemMount, StartCheckFindings, TakeDownReport};
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
pub(crate) struct KeptFolderParts {
    pub(crate) store: Store,
    /// Built and served.
    pub(crate) mount: Arc<ItemMount>,
    pub(crate) sync_plan_wake: Arc<SyncPlanWake>,
    pub(crate) running_steps: Arc<RunningSteps>,
    pub(crate) published_plan: Arc<PublishedPlan>,
    /// The folder's drive: the one the check at start was given when the mount was built
    /// (`CheckDrive::Known`). The store's own `drive_id` may still be empty when a sync
    /// was stopped before its first cycle, so the next start takes it from here.
    pub(crate) drive_id: DriveId,
}

/// What holds all sending when a folder's sync starts, as the account has it then: set
/// on the sync's `SendingHolds` before its plan's loop is spawned, so that the first plan
/// starts nothing a pause or a sign-out holds (a download of a pinned file is held by
/// nothing else). What changes afterwards is set by its own cause through
/// `SyncTasks::sending_holds`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StandingSendingHolds {
    /// `None`: not paused. `Some(None)`: paused until resumed. `Some(Some(t))`: paused
    /// until `t`, Unix seconds. As `SendingHolds::set_paused` takes it. The account's
    /// background work is stopped by the user's pause and by the automatic hold alike
    /// (metered connection, battery: `Running::stop`): the user's pause is
    /// given with its end, a hold as `Some(None)` — it ends when the owner says so
    /// (`show_pause`), by no time. Both read `SendingHold::Paused`.
    pub(crate) paused_until: Option<Option<i64>>,
    pub(crate) is_signed_out: bool,
    /// Why the account may not write to OneDrive now, as the account says it; `None` when
    /// it may. As `SendingHolds::set_writes_refusal` takes it. Computed by the owner with
    /// the same function its watcher of the account sets it by afterwards
    /// (`account_folder::mode::decide_account_holds`), from the account's snapshot read
    /// when the start begins.
    pub(crate) writes_refusal: Option<ModeNote>,
}

/// Everything a folder's start needs.
pub(crate) struct FolderStartInput {
    pub(crate) root: SyncRoot,
    /// The store's file.
    pub(crate) tree_db: PathBuf,
    pub(crate) drive: DriveClient,
    /// The drive `config.toml` recorded for the account, and the one the account's token
    /// was last seen to reach (`AccountSnapshot::live_drive`, read from `GET /me/drive` at
    /// a sign-in; none before the first): for the check at start.
    pub(crate) recorded_drive: Option<DriveId>,
    pub(crate) account_drive: Option<DriveId>,
    pub(crate) drive_record: Option<DriveRecord>,
    pub(crate) neighbours: Option<Neighbours>,
    pub(crate) source: Arc<dyn ContentSource>,
    pub(crate) report: Report,
    pub(crate) pool: Arc<TransferPool>,
    pub(crate) split_download_share: Arc<Share>,
    /// `[transfers] download_after`.
    pub(crate) download_after: u64,
    /// The folder's tree lock, kept with the folder's record.
    pub(crate) tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The folder's lease: a step towards the folder holds it.
    pub(crate) lease: Lease,
    pub(crate) ignore: SharedIgnore,
    pub(crate) state: SyncStateHandle,
    /// The account's one quota.
    pub(crate) quota: Quota,
    pub(crate) limits: Limits,
    /// What the plan is given that does not change while the sync runs
    /// (`PlanSettings::of` the account's machine name).
    pub(crate) settings: PlanSettings,
    pub(crate) schedule: Schedule,
    pub(crate) running: Arc<Running>,
    /// Where the thumbnails of the folder's files are written; `None` with no cache to
    /// fill: no thumbnail filler is started then.
    pub(crate) thumbnail_cache: Option<PathBuf>,
    /// What holds all sending now.
    pub(crate) standing_holds: StandingSendingHolds,
    /// Unix seconds: the account's clock.
    pub(crate) clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The access mode the account wants.
    pub(crate) wanted: Mode,
    /// Who is told that the mount must be taken down: the caller itself
    /// (`SyncService::take_down_report`), whose task then stops the sync, calls
    /// `stop_order::take_folder_down` inside a change of the folder, and starts the folder
    /// again after its pause.
    pub(crate) take_down_report: TakeDownReport,
    /// Called by the plan's task after every published plan with how many changes are
    /// unsent and will be sent by themselves (`SyncPlanTaskParts::unsent_count_report`, which says what is left out): the owner tells the account
    /// (`FolderAccount::tell_unsent_count`), which may show the count and wait for 0 with
    /// a switch to read-only. It must not block.
    pub(crate) unsent_count_report: Arc<dyn Fn(u64) + Send + Sync>,
    /// What the folder keeps from an earlier sync; `None` at the folder's first start and
    /// after a take-down. With kept parts the fields the mount was made with at its build
    /// are not read: `tree_db`, `recorded_drive`, `account_drive`, `source`,
    /// `split_download_share`, `download_after`, `take_down_report`, and `root` and
    /// `clock` as far as the mount took them. `tree_lock`, `ignore` and `lease` must be
    /// the very values of that first start: the kept mount holds them, and the runners
    /// made now must share them with it.
    pub(crate) kept: Option<KeptFolderParts>,
}

/// A folder whose sync runs: what the owner of the folder's state keeps.
pub(crate) struct StartedFolder {
    pub(crate) store: Store,
    pub(crate) mount: Arc<ItemMount>,
    pub(crate) tasks: SyncTasks,
    pub(crate) sync_plan_wake: Arc<SyncPlanWake>,
    pub(crate) running_steps: Arc<RunningSteps>,
    pub(crate) published_plan: Arc<PublishedPlan>,
    pub(crate) onedrive_space: Arc<OneDriveSpace>,
    /// The folder's drive, as [`KeptFolderParts::drive_id`]: `stop_order::stop_folder_sync`
    /// passes it on.
    pub(crate) drive_id: DriveId,
    /// How the store came to be: said to the user where a rebuild lost something. `Kept`
    /// for a folder that was started with the parts it kept. Read by the owner's
    /// `start_sync` right after the start, once (`describe_rebuild_loss`).
    pub(crate) store_origin: StoreOrigin,
}

/// Why a folder's sync did not start: said as the folder's blocking trouble. Nothing of
/// this layer tries again by itself: no task exists after a refusal.
#[derive(Debug)]
pub(crate) enum StartRefusal {
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
    /// The folder holds the placeholders of a konedrive from before the mount, which is
    /// not supported: its account is removed and added again. Nothing was written.
    LegacyPlaceholders,
}

/// A start that was refused: why, and what is left of it.
pub(crate) struct RefusedStart {
    pub(crate) refusal: StartRefusal,
    /// Read by the owner's `start_sync`, as `StartedFolder::store_origin` is.
    /// What the rebuild of the store lost, when stage 1 of THIS start rebuilt a store: the
    /// next start finds the new store as one that was kept and cannot say it any more.
    /// `None` when no store was rebuilt, and when nothing could be counted in the one
    /// that was.
    pub(crate) rebuild_loss: Option<RebuildLoss>,
    /// The parts the start was given as kept, given back as they were: the mount still
    /// served, the store open. `None` when none were given.
    pub(crate) kept: Option<KeptFolderParts>,
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
pub(crate) async fn start_folder_sync(input: FolderStartInput) -> Result<StartedFolder, RefusedStart> {
    let mut input = input;
    if let Some(kept) = input.kept.take() {
        return match start_sync_over_parts(&input, kept, StoreOrigin::Kept).await {
            Ok(started) => Ok(started),
            Err((refusal, kept)) => Err(RefusedStart { refusal, rebuild_loss: None, kept: Some(kept) }),
        };
    }
    cancel_upload_sessions_of_old_store(&input.tree_db, &input.drive).await;
    let (store, store_origin) = match open_folder_store(&input.tree_db).await {
        Ok(opened) => opened,
        Err(refusal) => return Err(RefusedStart { refusal, rebuild_loss: None, kept: None }),
    };
    let rebuild_loss = match &store_origin {
        StoreOrigin::MadeEmpty { loss } => *loss,
        StoreOrigin::Kept => None,
    };
    let refuse = |refusal: StartRefusal| RefusedStart { refusal, rebuild_loss, kept: None };
    attach_activity_log(&input, &store).await;

    let store_drive = match store.call(|store| store.drive_id()).await {
        Ok(store_drive) => store_drive.and_then(DriveId::new),
        Err(error) => {
            let_go_of_store(&input.report, store).await;
            return Err(refuse(StartRefusal::Store(format!("the drive of the tree store cannot be read: {error}"))));
        }
    };
    let CheckDrive::Known(drive_id) = decide_check_drive(store_drive, input.recorded_drive.clone(), input.account_drive.clone()) else {
        let_go_of_store(&input.report, store).await;
        return Err(refuse(StartRefusal::DriveNotKnown));
    };

    let sync_plan_wake = Arc::new(SyncPlanWake::new());
    let running_steps = Arc::new(RunningSteps::new());
    let published_plan = Arc::new(PublishedPlan::new());
    let setup = ItemMountSetup {
        root: input.root.clone(),
        store_path: input.tree_db.clone(),
        store: store.clone(),
        drive_id: drive_id.clone(),
        source: Arc::clone(&input.source),
        report: input.report.clone(),
        pool: Arc::clone(&input.pool),
        split_download_share: Arc::clone(&input.split_download_share),
        download_after: input.download_after,
        tree_lock: Arc::clone(&input.tree_lock),
        ignore: Arc::clone(&input.ignore),
        sync_plan_wake: Arc::clone(&sync_plan_wake),
        running_steps: Arc::clone(&running_steps),
        published_plan: Arc::clone(&published_plan),
        take_down_report: Arc::clone(&input.take_down_report),
        clock: Arc::clone(&input.clock),
    };
    let check_began_at = (input.clock)();
    input.state.update(|state| state.local.scan = describe_running_check(&state.local.scan, check_began_at));
    let built = ItemMount::build(setup).await;
    let check_ended_at = (input.clock)();
    // What the check left is not said here: it is in the journal (`start_check`) and, per item, in the queue's answers ("could not be read on disk"). The counts of directories and files stay 0: the check counts none.
    input.state.update(|state| state.local.scan = describe_finished_check(&state.local.scan, check_ended_at));
    let (mount, kernel_requests) = match built {
        Ok(built) => built,
        Err(error) => {
            let_go_of_store(&input.report, store).await;
            return Err(refuse(match error {
                ItemMountError::Store(error) => StartRefusal::Store(format!("the check of {} at start could not write the tree store: {error}", input.root.path.display())),
                ItemMountError::Io(error) => StartRefusal::Mount(format!("{} cannot be mounted: {error}", input.root.path.display())),
                ItemMountError::LegacyPlaceholders => StartRefusal::LegacyPlaceholders,
            }));
        }
    };
    // Off the runtime's threads: it mounts, and asks the mount for its own number.
    let served = {
        let mount = Arc::clone(&mount);
        tokio::task::spawn_blocking(move || mount.serve(kernel_requests)).await
    };
    let served = match served {
        Ok(served) => served.map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    if let Err(error) = served {
        stop_built_mount(mount).await;
        let_go_of_store(&input.report, store).await;
        return Err(refuse(StartRefusal::Mount(format!("{} cannot be mounted: {error}", input.root.path.display()))));
    }
    if let Err(refusal) = request_first_listing(&store, &store_origin, mount.start_check_findings()).await {
        stop_built_mount(mount).await;
        let_go_of_store(&input.report, store).await;
        return Err(refuse(refusal));
    }

    let parts = KeptFolderParts { store, mount, sync_plan_wake, running_steps, published_plan, drive_id };
    match start_sync_over_parts(&input, parts, store_origin).await {
        Ok(started) => Ok(started),
        Err((refusal, parts)) => {
            // Made in this start: ended here, and nothing of them is kept.
            let KeptFolderParts { store, mount, .. } = parts;
            stop_built_mount(mount).await;
            let_go_of_store(&input.report, store).await;
            Err(refuse(refusal))
        }
    }
}

/// Why the check at start runs, as `LocalScan.Reason` says it.
const CHECK_REASON: &str = "start";

/// What `LocalScan` says while the check at start runs, which began at `began_at`: the
/// last finished one's end and length stay. Pure.
fn describe_running_check(last: &LocalScan, began_at: i64) -> LocalScan {
    LocalScan { state: ScanState::Running, reason: CHECK_REASON.to_owned(), started: began_at, finished: last.finished, took: last.took }
}

/// What `LocalScan` says once the check that `running` describes ended at `ended_at`,
/// however it ended. Pure.
fn describe_finished_check(running: &LocalScan, ended_at: i64) -> LocalScan {
    LocalScan { state: ScanState::Idle, finished: ended_at, took: u32::try_from(ended_at.saturating_sub(running.started)).unwrap_or(0), ..running.clone() }
}

/// Stage 1's second half: the activity log kept in `store` from now on
/// (`Activity::attach`, off the runtime's threads) and the store's `last_checked` put
/// into `state`. Neither fails the start: a warning, and the time stays as it was.
async fn attach_activity_log(input: &FolderStartInput, store: &Store) {
    let (activity, attached, folder) = (Arc::clone(&input.report.activity), store.clone(), input.root.path.clone());
    let read = tokio::task::spawn_blocking(move || {
        activity.attach(attached.clone(), &folder);
        attached.call_blocking(|store| store.last_checked())
    });
    match read.await {
        Ok(Ok(last_checked)) => input.state.update(|state| state.cycle.last_checked = last_checked.unwrap_or(0)),
        Ok(Err(error)) => tracing::warn!("the time of the last listing cannot be read from the tree store: {error}"),
        Err(error) => tracing::warn!("the task attaching the activity log to the tree store failed: {error}"),
    }
}

/// What a refusal undoes of the store this start opened: the activity log detached
/// (`Activity::detach`, off the runtime's threads), and the store dropped.
async fn let_go_of_store(report: &Report, store: Store) {
    let activity = Arc::clone(&report.activity);
    if let Err(error) = tokio::task::spawn_blocking(move || activity.detach()).await {
        tracing::warn!("the task detaching the activity log failed: {error}");
    }
    drop(store);
}

/// What a refusal undoes of the mount this start built: stopped, served or not, and
/// dropped, off the runtime's threads (its stop joins a thread and unmounts).
async fn stop_built_mount(mount: Arc<ItemMount>) {
    let stopped = tokio::task::spawn_blocking(move || {
        mount.stop();
        drop(mount);
    });
    if let Err(error) = stopped.await {
        tracing::warn!("the task stopping the mount a refused start built failed: {error}");
    }
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
    let sending_holds = Arc::new(SendingHolds::new(Arc::clone(&parts.sync_plan_wake)));
    let standing_holds = &input.standing_holds;
    sending_holds.set_paused(standing_holds.paused_until);
    sending_holds.set_signed_out(standing_holds.is_signed_out);
    sending_holds.set_writes_refusal(standing_holds.writes_refusal.clone());
    if let Err(error) = apply_wanted_access_mode(&parts.mount, &sending_holds, input.wanted).await {
        return Err((StartRefusal::Mount(error), parts));
    }
    let onedrive_space = Arc::new(OneDriveSpace::new(input.quota.clone()));
    read_quota_before_first_plan(&input.drive, &onedrive_space, &parts.store, (input.clock)()).await;
    let tasks = start_sync_tasks(input, &parts.store, &parts.mount, &parts.sync_plan_wake, &parts.running_steps, &parts.published_plan, &sending_holds, &onedrive_space, &parts.drive_id);
    let KeptFolderParts { store, mount, sync_plan_wake, running_steps, published_plan, drive_id } = parts;
    Ok(StartedFolder { store, mount, tasks, sync_plan_wake, running_steps, published_plan, onedrive_space, drive_id, store_origin })
}

/// How long the start waits for OneDrive to cancel the upload sessions of a store of an
/// older version, all together. A guess.
const OLD_SESSIONS_CANCELLED_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// The drive the check at start binds an unknown mark in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckDrive {
    Known(DriveId),
    NotKnownYet,
}

/// Cancels in OneDrive the upload sessions a store of an older version holds
/// (`konedrive_tree::list_old_store_upload_sessions`, off the runtime's threads; then
/// `DriveClient::cancel_upload` for each), so that their empty files do not hold names
/// there once the store that knew them is removed (`docs/design/sync.md` §9.2, stage 0).
/// Before [`open_folder_store`], which removes that store. Nothing for a store of this build's
/// version, for no store, and for one whose sessions cannot be read. A session that
/// cannot be cancelled now — no network, signed out — is logged and left to expire in
/// OneDrive, and the start is not refused for it. All are asked at once and awaited
/// together under ONE bound ([`OLD_SESSIONS_CANCELLED_WITHIN`]): what has not answered by
/// then is dropped with its request, logged as not cancelled, and left to expire. No
/// task outlives this function.
async fn cancel_upload_sessions_of_old_store(tree_db: &Path, drive: &DriveClient) {
    let store_path = tree_db.to_path_buf();
    let sessions = match tokio::task::spawn_blocking(move || konedrive_tree::list_old_store_upload_sessions(&store_path)).await {
        Ok(Ok(sessions)) => sessions,
        Ok(Err(error)) => return tracing::warn!("the upload sessions of the tree store at {} cannot be read, and are left to expire in OneDrive: {error}", tree_db.display()),
        Err(error) => return tracing::warn!("the task reading the upload sessions of the tree store at {} failed: {error}", tree_db.display()),
    };
    if sessions.is_empty() {
        return;
    }
    // All at once, and dropped together at the bound: no request outlives this function.
    let cancels = futures_util::future::join_all(sessions.iter().map(|session| drive.cancel_upload(session)));
    match tokio::time::timeout(OLD_SESSIONS_CANCELLED_WITHIN, cancels).await {
        Ok(cancelled) => {
            // An address of a session lets whoever has it write: none is put into the journal.
            for error in cancelled.iter().filter_map(|cancelled| cancelled.as_ref().err()) {
                tracing::warn!("an upload session of the tree store of an older version could not be cancelled, and is left to expire in OneDrive: {error}");
            }
            tracing::info!("{} of {} upload sessions of the tree store of an older version are cancelled in OneDrive", cancelled.iter().filter(|cancelled| cancelled.is_ok()).count(), sessions.len());
        }
        Err(_) => tracing::warn!("OneDrive did not answer the cancelling of {} upload sessions of the tree store of an older version within {} s: those not cancelled are left to expire", sessions.len(), OLD_SESSIONS_CANCELLED_WITHIN.as_secs()),
    }
}

/// Opens the folder's store, making it or rebuilding it where
/// `konedrive_tree::decide_old_store` says, and answers how it came to be
/// (`TreeStore::open_with_origin`). Off the runtime.
async fn open_folder_store(tree_db: &Path) -> Result<(Store, StoreOrigin), StartRefusal> {
    let store_path = tree_db.to_path_buf();
    match tokio::task::spawn_blocking(move || konedrive_tree::TreeStore::open_with_origin(&store_path)).await {
        Ok(Ok((store, store_origin))) => Ok((Store::new(store), store_origin)),
        Ok(Err(error)) => Err(StartRefusal::Store(format!("the tree store cannot be opened: {error}"))),
        Err(error) => Err(StartRefusal::Store(format!("the tree store cannot be opened: {error}"))),
    }
}

/// The drive for the check: the store's own (`meta` `drive_id`), else the one
/// `config.toml` recorded for the account, else the one the account's token was last seen
/// to reach. Pure.
pub fn decide_check_drive(store_drive: Option<DriveId>, recorded_drive: Option<DriveId>, account_drive: Option<DriveId>) -> CheckDrive {
    match store_drive.or(recorded_drive).or(account_drive) {
        Some(drive) => CheckDrive::Known(drive),
        None => CheckDrive::NotKnownYet,
    }
}

/// Whether a full listing is asked for at this start, and why: a store that was kept, in
/// which the check bound entries by marks no row had. A store that was made empty has no
/// delta link and lists in full by itself. Pure. Not asked for a folder that was started
/// with the parts it kept: its check ran at an earlier start, which asked already.
pub(crate) fn decide_first_listing(origin: &StoreOrigin, findings: &StartCheckFindings) -> Option<FullListingCause> {
    match origin {
        StoreOrigin::Kept if findings.bound_by_mark_count > 0 => Some(FullListingCause::BoundByMarkAtStart),
        StoreOrigin::Kept | StoreOrigin::MadeEmpty { .. } => None,
    }
}

/// Writes [`decide_first_listing`]'s answer (`ItemStore::request_full_listing`). After the
/// mount was built, before the listing's poller starts. A write that fails is
/// `StartRefusal::Store`.
async fn request_first_listing(store: &Store, origin: &StoreOrigin, findings: &StartCheckFindings) -> Result<(), StartRefusal> {
    let Some(cause) = decide_first_listing(origin, findings) else { return Ok(()) };
    store.call(move |store| store.item_store().request_full_listing(cause)).await.map_err(|error| StartRefusal::Store(format!("the full listing the check at start needs cannot be asked for: {error}")))
}

/// Reads the account's quota once, so that the first plan knows whether OneDrive is full
/// (`OneDriveSpace::apply_quota`); a read that fails leaves it as not full, and the first
/// 507 says otherwise. On `SpaceChange::HasSpace` the refusals for lack of space that fit
/// now are forgotten (`StepMemory::forget_fitting_space_refusals(free_space)`, through
/// `store`): the step memory keeps them across a restart, and after a start that finds
/// OneDrive not full no later read of the quota is due that would. A forgetting that
/// fails is a warning.
async fn read_quota_before_first_plan(drive: &DriveClient, space: &OneDriveSpace, store: &Store, now: i64) {
    let quota = match drive.quota().await {
        Ok(quota) => quota,
        Err(error) => return tracing::warn!("cannot read the OneDrive quota before the first plan: {error}"),
    };
    if let SpaceChange::HasSpace { free_space, .. } = space.apply_quota(&quota, now) {
        if let Err(error) = store.call(move |store| store.step_memory().forget_fitting_space_refusals(free_space)).await {
            tracing::warn!("cannot forget the refusals for lack of space that fit now: {error}");
        }
    }
}

/// Makes the parts of the sync, each once, in this order, and starts the tasks. Nothing
/// is awaited here.
///
/// The stop's token is made first: every task and the step runners get a clone, and
/// `SyncTasks::stop_handle` hands it out. `sending_holds` is given: stage 5 made it and
/// set on it what stands (`input.standing_holds`: `set_paused`, `set_signed_out`,
/// `set_writes_refusal`; a read-only folder's hold) — BEFORE the plan's loop is spawned;
/// `OneDriveSpace` (given, read before); `GraphAccess`;
/// `LostAnswers::new` (with the tree lock), `SessionCancellations::new` (with it and the
/// running steps), `MadeItems::new`, `NameHolders::new`,
/// `ContentUpload::new`; `OneDriveSteps::new` (with the tree lock and the mount's
/// `item_content_files`); `RowSteps::new`; the runner of the steps towards the folder
/// ([`make_folder_steps`] over `mount.folder_step_parts()`: the mount holds the parts and
/// not the runner, so every start makes one, which owns no state); `StepRunners::of_runners`
/// (those folder steps, the lease, the stop's token: a step towards the folder that waits
/// for the lease when the stop comes ends as `NotRun(SendingStopped)`).
///
/// The tasks, in this order: `ListingCycle::new` (its `after_listing` a `Notify` made
/// here, the thumbnail filler's nudge) and `ListingPoller::start` (a first cycle at once;
/// a full listing found under way goes on from its link); the thumbnail filler where
/// `input.thumbnail_cache` names a cache (`ThumbnailFiller::new(..).spawn(nudge, stop)`);
/// `SyncPlanTask::new` with the runners, the mount's `start_check_findings`, a
/// `quota_reader` that asks `input.drive` for the quota (`DriveClient::quota`, `None`
/// when it could not be read), the activity log (`input.report.activity`), the
/// announcer of sync marks (`FolderSteps::announce_marks_changed` of the folder steps
/// made above), the thumbnail filler's nudge as `after_folder_steps` (the same
/// `Notify` the cycle was given) and `input.unsent_count_report`, and its loop spawned
/// with the stop's token. No task watches the account: what its state holds is set and
/// ended by the owner's watcher. The poller's handle is cloned for the plan's task and
/// `SyncTasks::poll`. The plan's
/// task sends nothing to OneDrive until the first listing of this start ended
/// (`SendingHold::FirstListingNotEnded`).
#[allow(clippy::too_many_arguments)]
fn start_sync_tasks(input: &FolderStartInput, store: &Store, mount: &Arc<ItemMount>, sync_plan_wake: &Arc<SyncPlanWake>, running_steps: &Arc<RunningSteps>, published_plan: &Arc<PublishedPlan>, sending_holds: &Arc<crate::sync_parts::sending_holds::SendingHolds>, onedrive_space: &Arc<OneDriveSpace>, drive_id: &DriveId) -> SyncTasks {
    let stop = CancellationToken::new();
    let graph = GraphAccess { drive: input.drive.clone(), drive_id: drive_id.clone(), store: store.clone(), clock: Arc::clone(&input.clock) };
    let lost_answers = LostAnswers::new(graph.clone(), Arc::clone(&input.tree_lock));
    let session_cancellations = SessionCancellations::new(graph.clone(), lost_answers.clone(), Arc::clone(running_steps));
    let made_items = MadeItems::new(graph.clone());
    let name_holders = NameHolders::new(graph.clone(), session_cancellations.clone());
    let content_upload = ContentUpload::new(graph.clone(), lost_answers.clone(), Arc::clone(sending_holds), Arc::clone(onedrive_space), Arc::clone(running_steps), input.limits.clone());
    let onedrive_steps = Arc::new(OneDriveSteps::new(graph, Arc::clone(&input.tree_lock), Arc::clone(mount.item_content_files()), content_upload, name_holders, lost_answers, made_items, Arc::clone(running_steps), Arc::clone(onedrive_space)));
    let row_steps = Arc::new(RowSteps::new(store.clone(), Arc::clone(&input.tree_lock), Arc::clone(mount.item_content_files()), Arc::clone(&input.ignore), Arc::clone(running_steps)));
    let folder_steps = make_folder_steps(mount.folder_step_parts());
    let step_runners = StepRunners::of_runners(Arc::clone(&folder_steps), onedrive_steps, row_steps, input.lease.clone(), stop.clone());

    // The thumbnail filler's nudge: the cycle's at a listing's end, the plan's task's after a round that changed the folder.
    let thumbnail_nudge = Arc::new(tokio::sync::Notify::new());
    let listing_cycle = ListingCycle::new(ListingCycleSetup {
        drive: input.drive.clone(),
        store: store.clone(),
        sync_plan_wake: Arc::clone(sync_plan_wake),
        state: input.state.clone(),
        drive_record: input.drive_record.clone(),
        neighbours: input.neighbours.clone(),
        report: input.report.clone(),
        after_listing: Some(Arc::clone(&thumbnail_nudge)),
    });
    let listing_poller = ListingPoller::start(listing_cycle, input.schedule.clone(), Arc::clone(&input.running));
    let poll = listing_poller.handle();
    let thumbnail_filler = input.thumbnail_cache.clone().map(|cache| ThumbnailFiller::new(input.drive.clone(), store.clone(), input.root.clone(), cache, Arc::clone(&input.running), input.state.clone()).spawn(Arc::clone(&thumbnail_nudge), stop.clone()));

    let quota_drive = input.drive.clone();
    let quota_reader: Arc<dyn Fn() -> RunningFuture<Option<DriveQuota>> + Send + Sync> = Arc::new(move || {
        let drive = quota_drive.clone();
        Box::pin(async move {
            match drive.quota().await {
                Ok(quota) => Some(quota),
                Err(error) => {
                    tracing::warn!("cannot read the OneDrive quota: {error}");
                    None
                }
            }
        })
    });
    let plan = SyncPlanTask::new(SyncPlanTaskParts {
        store: store.clone(),
        sync_plan_wake: Arc::clone(sync_plan_wake),
        step_runners,
        session_cancellations,
        running_steps: Arc::clone(running_steps),
        sending_holds: Arc::clone(sending_holds),
        onedrive_space: Arc::clone(onedrive_space),
        published_plan: Arc::clone(published_plan),
        poll: poll.clone(),
        pool: Arc::clone(&input.pool),
        state: input.state.clone(),
        settings: input.settings.clone(),
        start_check: mount.start_check_findings().clone(),
        folder: input.root.path.clone(),
        clock: Arc::clone(&input.clock),
        quota_reader,
        activity: Arc::clone(&input.report.activity),
        marks_announcer: Arc::new(move |items| folder_steps.announce_marks_changed(items)),
        after_folder_steps: thumbnail_filler.is_some().then(|| Arc::clone(&thumbnail_nudge)),
        unsent_count_report: Arc::clone(&input.unsent_count_report),
        free_space_reader: mount.free_space_reader(),
        free_space_reserve_reader: {
            let running = Arc::clone(&input.running);
            Arc::new(move || crate::sync_plan_task::disk_space::count_reserve_bytes(running.settings().free_space_reserve_gb))
        },
    });
    let plan_loop = tokio::spawn(Arc::clone(&plan).run(stop.clone()));
    SyncTasks { listing_poller: Some(listing_poller), poll, plan_loop, thumbnail_filler, sending_holds: Arc::clone(sending_holds), sync_plan_wake: Arc::clone(sync_plan_wake), stop }
}

/// The runner of the plan's steps towards the folder, made of the parts the mount hands out
/// for it (`ItemMount::folder_step_parts`): `FolderSteps::new` with the eleven of `parts`, in
/// the order of the struct's fields. Called by [`start_sync_tasks`] at every start, with a
/// mount kept or built in this one alike: the runner owns no state, so a second one over the
/// same parts is the first. Cannot fail.
fn make_folder_steps(parts: FolderStepParts) -> Arc<FolderSteps> {
    let FolderStepParts { place_changes, store, write_operation_thread, upload_rules, item_handles, inotify_bridge, downloader, download_states, range_fetcher, mount_take_down, folder } = parts;
    Arc::new(FolderSteps::new(place_changes, store, write_operation_thread, upload_rules, item_handles, inotify_bridge, downloader, download_states, range_fetcher, mount_take_down, folder))
}

#[cfg(test)]
mod tests;

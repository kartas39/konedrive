//! The task of a folder's sync plan: the loop that reads what differs, makes the plan,
//! runs its steps and settles what came of each. One per folder, in either access mode,
//! in place of the plan's task and of the reconcile.
//!
//! **State owned:** the loop, with the steps of the last plan that are not started yet,
//! the count of events logged since the last listing ended, and the failed tries of the
//! steps the store keeps no try of — towards the folder, of the rows, towards OneDrive
//! from a row that is not unsent, and a try the store could not be written
//! ([`LocalStepTries`], which says why they are kept here and not in the store); nothing
//! a restart must find again: after one, such a step is tried again at once. Everything else has an owner of its own: the steps running
//! (`RunningSteps`), what holds all sending (`SendingHolds`), whether OneDrive is full
//! (`OneDriveSpace`), the last plan's answers (`PublishedPlan`) — and the rows and the
//! step memory, read at each plan.
//!
//! **One round** ([`decide_round`] says which of the three it is):
//!
//! - *a plan is made*: what the daemon knows beside the rows is gathered
//!   ([`gather_daemon_facts`](SyncPlanTask::gather_daemon_facts)); then, as ONE read
//!   closure of the store's reading connection (`Store::read_items`: one read
//!   transaction, not the writer's thread — no write operation waits behind a plan),
//!   [`plan_input::plan_next_steps`] reads the bounded sets, closes the set, describes
//!   it with what the steps remember and what the check at start holds, makes the plan,
//!   and widens it by every item the plan answers it was not given; the answers are
//!   published;
//! - *the last plan is gone on with*: the steps of it that can start now are chosen
//!   ([`step_choice::decide_steps_to_start`]) and started;
//! - *sleep*: until a cause, a time, or the stop.
//!
//! **What bounds the plans.** A plan is made when a cause other than a finished step
//! came, when a time was reached, or when a step that ended changed something a plan is
//! made from (it was written, its rows changed, a try was counted, a cycle asked for, a
//! hold set) — once for all the steps that are settled together, and never for steps that
//! changed nothing. The steps the last plan left are replaced by the new plan's. Every
//! step is conditional, so a step of a
//! plan that is a few steps old is safe: its commit finds the rows changed and writes
//! nothing. The `ShowItem` steps of one plan are run as one job
//! (`FolderSteps::run_show_items`), and the outcomes that are ready together are settled
//! in one job of the store. A first listing of N items is then some plans per page and a
//! few jobs, not N of each. The one reader that scans (`list_pinned_files_to_download`)
//! is asked with every plan while a pinned file may be missing its content — a plan that
//! left the pinned files out would drop the downloads the last one planned — and not at
//! all once a reading found none ([`MissingPinnedFiles`]).
//!
//! **How many steps run at once** (one per item): of the rows, all at once, in the plan's
//! order; towards the folder, as they come — each is one job of the write-operation
//! thread, which runs one at a time; downloads of pinned files, as the pool's download
//! slots allow; towards OneDrive, content as the pool's upload slots allow and the rest
//! one at a time. What holds all sending holds the last two, and a pause and a sign-out
//! the downloads; while OneDrive is full no content is started.
//!
//! **What wakes it** (`WakeCause`): a local version written by the mount; a listing's
//! page written, a listing ended; a step finished; the user's answer; a refresh asked
//! for; a quota read; what holds all sending changed. And the time: the soonest of the plan's waits until a
//! time (the step memory's next tries among them: the plan is told), the end of a
//! throttle or a timed pause, and the quota's next read.
//!
//! **The stop.** When `stop` comes the loop itself ends its steps and then returns:
//! `SendingHolds::set_stopping`; `RunningSteps::abort_requests` — the tasks of the steps
//! towards the network and of the rows whose request or hash is in flight, none of which
//! has written anything; a step whose commit has begun (`RunningSteps::begin_commit`,
//! said by its runner) is left, as every step towards the folder is; then it goes on
//! taking and settling the outcomes of the steps that still run, until none does
//! (`RunningSteps::count_steps_to_settle`). Nobody else ends a step.
//!
//! **Tested without the rest**, over the fixture (`mount::item_fixture`) with its stand-in
//! for the runners (`step_runners_stand_in`), its poll handle and a clock the test
//! holds; the convergence through the task runs with the three real runners once they
//! are filled:
// test: a_local_change_is_planned_and_its_step_started
// test: no_plan_is_made_for_a_finished_step_while_steps_of_the_last_plan_are_left
// test: a_plan_is_made_when_the_last_plans_steps_have_all_ended
// test: the_show_steps_of_one_plan_are_run_as_one_job
// test: no_second_step_is_started_for_an_item_whose_step_runs
// test: nothing_towards_onedrive_is_started_while_something_holds_all_sending
// test: steps_towards_the_folder_run_while_sending_is_held
// test: a_download_runs_beside_the_steps_towards_the_folder
// test: the_task_sleeps_until_the_soonest_time_something_is_due
// test: a_plan_fault_that_needs_a_full_listing_asks_for_one_once
// test: a_stop_aborts_the_requests_and_settles_the_steps_towards_the_folder_before_the_loop_returns
// test: a_hash_that_runs_at_a_stop_is_aborted_and_writes_nothing
// test: the_plans_read_waits_for_no_job_of_the_stores_writer
// test: a_refresh_reads_the_quota_once_and_a_quota_read_by_another_is_not_read_again
// test: rows_changed_at_random_on_both_sides_converge_through_the_task

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::drive::DriveQuota;
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{FolderStep, FullListingCause, LocalItemId, PlanFault, Store, SyncPlan, SyncStep, WaitReason};
use tokio_util::sync::CancellationToken;

use crate::sync_parts::published_plan::{decide_is_shown_blocked, ItemAnswer, PublishedPlan};
use crate::sync_parts::running_steps::{RunningStep, RunningSteps, StepClass};
use crate::sync_parts::step_outcome::{StepFailure, StepObstacle, StepOutcome, StepRefusal};
use crate::sync_parts::sync_plan_wake::{SyncPlanWake, WakeCause};
use crate::mount::StartCheckFindings;
use crate::listing::listing_poller::ListingPollHandle;
use crate::status::activity::Activity;
use crate::status::snapshot::{UploadQueueNote, SyncStateHandle};
use crate::sync_parts::onedrive_space::{OneDriveSpace, SpaceChange};
use crate::onedrive_steps::session_cancellations::SessionCancellations;
use crate::sync_parts::sending_holds::{SendingHold, SendingHolds};

/// The failed tries of the steps towards the folder and of the rows, kept for the run.
pub mod local_step_tries;
/// Whether a pinned file may be missing its content.
pub mod missing_pinned_files;
/// What a plan is made from: read from the store, closed, described.
pub mod plan_input;
/// The steps of the rows alone.
pub mod row_steps;
/// Which steps of a plan start now.
pub mod step_choice;
/// The runners of steps, as the task takes them.
pub mod step_runners;
/// What is written and asked for after a step, by what came of it.
pub mod step_settlement;

use local_step_tries::{LocalStep, LocalStepTries};
use missing_pinned_files::MissingPinnedFiles;
use plan_input::{DaemonFacts, PlanSettings, PlannedRound};
use step_choice::StepLimits;
use step_runners::{RunningFuture, StepRunners};
use step_settlement::{CycleRequest, StepSettlement};

/// The parts a plan's task is made of: each made before it by the folder's start.
pub struct SyncPlanTaskParts {
    pub store: Store,
    pub sync_plan_wake: Arc<SyncPlanWake>,
    /// What runs the steps: the three runners behind one seam, or the tests' stand-in.
    pub step_runners: StepRunners,
    pub session_cancellations: SessionCancellations,
    pub running_steps: Arc<RunningSteps>,
    pub sending_holds: Arc<SendingHolds>,
    pub onedrive_space: Arc<OneDriveSpace>,
    pub published_plan: Arc<PublishedPlan>,
    /// Asks the listing for a cycle.
    pub poll: ListingPollHandle,
    /// The account's transfer pool: how many uploads and downloads it has slots for. The
    /// task only counts against them; the slot itself is taken by who sends or downloads.
    pub pool: Arc<TransferPool>,
    pub state: SyncStateHandle,
    pub settings: PlanSettings,
    /// What the check at start left: the rows sending is held back for.
    pub start_check: StartCheckFindings,
    /// The folder's path, which is the mount: the activity log holds full paths.
    pub folder: std::path::PathBuf,
    /// Unix seconds: the account's clock.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Reads the account's quota from OneDrive (`DriveClient::quota`); `None` when it
    /// could not be read. Asked when the quota's next read is due
    /// (`OneDriveSpace::find_next_read`) and for a refresh the user asked for
    /// (`WakeCause::RefreshAsked`: the ONE read of a refresh, nobody else reads for it),
    /// and what it answers is applied by the task (`OneDriveSpace::apply_quota`). On
    /// `WakeCause::QuotaRead` it is NOT asked: another read the quota and applied it.
    pub quota_reader: Arc<dyn Fn() -> RunningFuture<Option<DriveQuota>> + Send + Sync>,
    /// The folder's activity log: every event the settlement of a step wrote into the
    /// store is announced through it (`Activity::announce`: `ActivityLog.Added`), and
    /// `Conflicts.Count` read again after a conflict (`Activity::prune`). The task is the
    /// one writer of a step's events, so it is the one that announces them.
    pub activity: Arc<Activity>,
    /// Tells whoever watches the folder that the sync mark of these items changed
    /// (`FolderSteps::announce_marks_changed`, or a test's stand-in): the task changes
    /// what the mark is answered from (`RunningSteps`, `PublishedPlan`), so it says so.
    pub marks_announcer: Arc<dyn Fn(Vec<LocalItemId>) + Send + Sync>,
    /// Nudged after a round in which a step towards the folder was written: the
    /// thumbnail filler's, the very `Notify` the listing's cycle nudges at a listing's
    /// end (`ListingCycleSetup::after_listing`). A listing ends before the plan shows what
    /// it brought, so the nudge of the cycle alone finds no file to make a thumbnail
    /// for. `None` with no filler.
    pub after_folder_steps: Option<Arc<tokio::sync::Notify>>,
    /// Called after every published plan with how many changes are unsent and will be
    /// sent without the user doing anything (`PlannedRound::unsent_to_send_count`, of the
    /// plan's own reading): every unsent change but those OneDrive refuses for good, those
    /// the check at start left, and what waits for one of them. Who shows the count and
    /// waits for "nothing is left to send" — the account that keeps a wish for read-only
    /// (`FolderAccount::tell_unsent_count`): a switch waits for what will be sent, and
    /// what will not stays here under the hold as after a forced switch. It is NOT the
    /// count a Forget warns with (`SyncService::count_unsent`: every unsent change, as
    /// all of them stay only here). The task is the ONE that says it; nobody asks the
    /// count on a timer. It must not block.
    pub unsent_count_report: Arc<dyn Fn(u64) + Send + Sync>,
}

/// How often the bytes of the uploads that run are published while one runs, in seconds
/// (`SyncPlanTask::publish_queue_status` on the task's tick).
const UPLOAD_PROGRESS_PUBLISHED_EVERY: i64 = 1;

/// How long after the old openings of upload sessions were forgotten they are forgotten
/// again, in seconds: once a day.
const OLD_OPENINGS_FORGOTTEN_EVERY: i64 = 24 * 60 * 60;

/// How long the task waits before it reads again what it could not read: the store for
/// a plan, the quota that was due. Seconds.
const READ_RETRY_WAIT: i64 = 60;

/// The plan's task of one folder. It holds its parts and runs the loop; what each part
/// does is the part's.
pub struct SyncPlanTask {
    parts: SyncPlanTaskParts,
    /// How many events of what OneDrive changed were logged since the last listing ended:
    /// what `step_settlement::decide_events_to_log` caps by. Set to 0 at
    /// `WakeCause::ListingEnded`.
    logged_since_listing_ended: AtomicUsize,
    /// The failed tries the task keeps itself, for this run (`LocalStepTries`).
    /// Changed by the loop alone: when a plan is made, to what the rows of that plan's
    /// reading still say (`PlannedRound::local_step_tries`); at a settlement
    /// (`step_settlement::change_local_step_tries`); and at a refresh
    /// (`WakeCause::RefreshAsked`, `LocalStepTries::clear_next_tries`).
    local_step_tries: std::sync::Mutex<LocalStepTries>,
    /// Whether a pinned file may be missing its content: while it may, a plan's reading
    /// scans all below the pinned directories. Changed by the loop alone.
    missing_pinned_files: MissingPinnedFiles,
    /// The steps that ended and are not settled yet, each with what came of it: put by
    /// the task of a step, taken by the loop.
    ended_steps: std::sync::Mutex<Vec<EndedStep>>,
    /// How many plans the loop made in this run: what a step is started with, to tell
    /// whether a plan was made while it ran.
    plan_count: AtomicU64,
    /// The faults of the last plan: what a fault is said once by.
    told_faults: std::sync::Mutex<Vec<PlanFault>>,
    /// The items a full listing was asked for (`PlanFault::FullListingNeeded`) that no
    /// listing ended for since: none is asked for these again. Emptied when a listing
    /// ended and the store's request has begun.
    full_listings_asked: std::sync::Mutex<BTreeSet<LocalItemId>>,
    /// When the read of the quota that was due failed last, or answered nothing of the
    /// space, Unix seconds: it is not asked again before `READ_RETRY_WAIT` passed. None
    /// once a read said something of the space.
    quota_read_failed_at: std::sync::Mutex<Option<i64>>,
    /// The task that cancels upload sessions and removes the items uploads left, when one
    /// was started: one at a time, and awaited by the loop before it returns at a stop.
    session_cancellations_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The counts of the queue as the task last read and published them: what the tick's
    /// round, which reads no count, publishes the bytes of the uploads with.
    queue_counts: std::sync::Mutex<crate::upload_queue::QueueCounts>,
    /// When the soonest next try of what the cleaning task could not get through is due,
    /// Unix seconds (`CancellationRound::next_try_at` of its last rounds); none when
    /// nothing waits. Written by that task when it ends, read for the timer.
    next_cleaning_try: std::sync::Mutex<Option<i64>>,
}

/// A step that ended, as its task hands it to the loop.
struct EndedStep {
    step: SyncStep,
    outcome: StepOutcome,
    /// How many plans were made when the step was started (`SyncPlanTask::plan_count`).
    plan_count_at_start: u64,
}

/// What a round's settlement says of the steps it settled: what `decide_round` asks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettledSteps {
    /// One of them changed something a plan is made from
    /// (`step_settlement::has_changed_anything`).
    pub has_change: bool,
    /// A plan was made since one of them began: that plan answered its item as one a step
    /// runs for, and gave it no step.
    pub is_plan_made_since_start: bool,
}

/// What a round does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundDecision {
    /// A plan is made: a cause other than a finished step came, a time was reached, a
    /// step that was settled in this round changed something, or a plan was made while
    /// one of them ran.
    MakePlan,
    /// The last plan is gone on with: steps of it are left to start, only steps finished
    /// since, none of them changed anything, and no plan was made while one ran.
    GoOnWithLastPlan,
    /// Nothing to do until the next cause or time.
    Sleep,
}

/// What the round does, from the causes that woke it (none: a time was reached), what
/// the settlement of this round says of the steps it settled, and how many steps of the
/// last plan are not started. A round of finished steps alone makes a plan when one of
/// them changed something — also while steps of the last plan are left, which the new
/// plan's replace: what waited for the step is planned at once, whatever holds the steps
/// left. And when a plan was made since one of them began: that plan gave its item no
/// step, as one ran, so a step that then ended with nothing changed (a download stopped
/// by a pause that is over by now) would leave the item with no step and no wake. When
/// neither holds no plan is made by that round — the plan would be the one before, and a
/// step that cannot run (the mount's take-down asked for, the write-operation thread
/// gone) would be planned and ended without a wait between: the steps left that can start
/// are started, else the task sleeps until a cause or a time. Pure.
pub fn decide_round(causes: &BTreeSet<WakeCause>, settled: SettledSteps, steps_left_to_start: usize) -> RoundDecision {
    let is_only_finished_steps = !causes.is_empty() && causes.iter().all(|cause| *cause == WakeCause::StepFinished);
    match (is_only_finished_steps, settled.has_change || settled.is_plan_made_since_start, steps_left_to_start) {
        (false, _, _) | (true, true, _) => RoundDecision::MakePlan,
        (true, false, 0) => RoundDecision::Sleep,
        (true, false, _) => RoundDecision::GoOnWithLastPlan,
    }
}

/// What the end of a step's task that answered no outcome is settled as: nothing for a
/// task that was aborted (a stop: `RunningSteps::abort_requests` took its step off
/// already), a failure for any other end — a panic of the runner —, so that the step
/// ends, counts a try and waits. Pure.
pub fn decide_outcome_of_ended_task(is_cancelled: bool, detail: String) -> Option<StepOutcome> {
    (!is_cancelled).then_some(StepOutcome::Failed(StepFailure::RunnerPanicked { detail }))
}

/// When the task wakes next only to publish the bytes of the uploads that run
/// (`SyncPlanTask::publish_queue_status`), Unix seconds: `UPLOAD_PROGRESS_PUBLISHED_EVERY`
/// after `now` while a step of the class `StepClass::Content` runs, else none. A wake at
/// this time is a round that does not plan: it settles what ended, publishes, and waits
/// again — unless the plan's own time ([`decide_next_wake`]) was reached too. The loop
/// tells the two apart itself, by the times (it kept both): for a wake at the tick alone
/// it does not ask [`decide_round`], whose three cases stay as they are. Pure.
pub fn decide_progress_tick(running: &[RunningStep], now: i64) -> Option<i64> {
    let is_content_sent = running.iter().any(|step| match step.class {
        StepClass::Content => true,
        StepClass::Metadata | StepClass::Download | StepClass::Folder | StepClass::Row => false,
    });
    is_content_sent.then_some(now.saturating_add(UPLOAD_PROGRESS_PUBLISHED_EVERY))
}

/// When the task wakes by itself next, Unix seconds: the soonest of the plan's waits
/// until a time, the end of what holds all sending when it has one
/// (`SendingHolds::find_end`), the quota's next read, and the next try of what is left to
/// clean in OneDrive (`next_cleaning_try`: a cancel, a removal, a request record that
/// could not be read) while it is still to come — one that is due is asked by the round
/// that is being made; `None` when nothing is due.
/// Never sooner than a second from `now`. Pure. Today's `Engine::next_due`
/// (`upload/engine.rs:480`).
pub fn decide_next_wake(plan: &SyncPlan, hold_end: Option<i64>, next_quota_read: Option<i64>, next_cleaning_try: Option<i64>, now: i64) -> Option<i64> {
    let waits = plan.waits.iter().filter_map(|wait| if let WaitReason::Time { until, .. } = wait.reason { Some(until) } else { None });
    waits.chain(hold_end).chain(next_quota_read).chain(next_cleaning_try.filter(|at| *at > now)).min().map(|soonest| soonest.max(now.saturating_add(1)))
}

impl SyncPlanTask {
    /// Made by the folder's start (`sync_tasks::start_order::start_sync_tasks`).
    pub fn new(parts: SyncPlanTaskParts) -> Arc<Self> {
        Arc::new(SyncPlanTask { parts, logged_since_listing_ended: AtomicUsize::new(0), local_step_tries: std::sync::Mutex::new(LocalStepTries::default()), missing_pinned_files: MissingPinnedFiles::new(), ended_steps: std::sync::Mutex::default(), told_faults: std::sync::Mutex::default(), full_listings_asked: std::sync::Mutex::default(), plan_count: AtomicU64::new(0), quota_read_failed_at: std::sync::Mutex::new(None), session_cancellations_task: std::sync::Mutex::new(None), queue_counts: std::sync::Mutex::default(), next_cleaning_try: std::sync::Mutex::new(None) })
    }

    /// Runs the loop until `stop`, then ends its steps as the module's doc says, and
    /// returns only when no step towards the folder or of the rows runs. Spawned by the
    /// folder's start after the listing's poller; `SyncTasks::stop` cancels `stop` and
    /// awaits this.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        let parts = &self.parts;
        parts.session_cancellations.forget_old_openings().await;
        let mut old_openings_forgotten_at = (parts.clock)();
        let mut steps_left_to_start: Vec<SyncStep> = Vec::new();
        let mut next_wake = None;
        // The first round is one of a time reached: a plan is made.
        let mut causes = BTreeSet::new();
        loop {
            let ended = self.take_ended_steps();
            let has_settled_steps = !ended.is_empty();
            let settled = self.settle_steps(ended).await;
            // The one writer of the queue's status: after every round that settled a step, after every plan, and at the tick.
            if has_settled_steps {
                self.publish_queue_status(true).await;
            }
            match decide_round(&causes, settled, steps_left_to_start.len()) {
                RoundDecision::MakePlan => {
                    let now = (parts.clock)();
                    if now.saturating_sub(old_openings_forgotten_at) >= OLD_OPENINGS_FORGOTTEN_EVERY {
                        parts.session_cancellations.forget_old_openings().await;
                        old_openings_forgotten_at = now;
                    }
                    self.apply_wake_causes(&causes, &stop).await;
                    self.missing_pinned_files.note_wake_causes(&causes);
                    match self.make_plan(self.gather_daemon_facts()).await {
                        Ok(round) => {
                            self.plan_count.fetch_add(1, Ordering::SeqCst);
                            let now = (parts.clock)();
                            let plan = &round.plan;
                            tracing::debug!("a plan was made of {} items: {} step(s), {} wait(s), {} exclusion(s), {} fault(s)", round.input.items.len(), plan.steps.len(), plan.waits.len(), plan.exclusions.len(), plan.faults.len());
                            self.publish_round(&round, parts.sending_holds.find(now).as_ref()).await;
                            self.publish_queue_status(true).await;
                            // A read of the quota that was due and failed is tried again after a wait, not at once.
                            // The steps the last plan left are replaced by this plan's.
                            let next_quota_read = parts.onedrive_space.find_next_read().map(|at| if at > now { at } else { now.saturating_add(READ_RETRY_WAIT) });
                            let next_cleaning_try = *crate::panic::lock(&self.next_cleaning_try);
                            next_wake = decide_next_wake(&round.plan, parts.sending_holds.find_end(now), next_quota_read, next_cleaning_try, now);
                            steps_left_to_start = self.start_steps(round.plan.steps, now);
                        }
                        Err(error) => {
                            tracing::warn!("cannot make the sync plan of {}: {error}", parts.folder.display());
                            steps_left_to_start.clear();
                            // The store could not be read: the plan is tried again by itself.
                            next_wake = Some((parts.clock)().saturating_add(READ_RETRY_WAIT));
                        }
                    }
                }
                RoundDecision::GoOnWithLastPlan => steps_left_to_start = self.start_steps(steps_left_to_start, (parts.clock)()),
                RoundDecision::Sleep => {}
            }
            // While a content step runs the task also wakes at its tick, only to publish the bytes of the uploads: a wake at the tick alone makes no plan, and `decide_round` is not asked.
            let waited = loop {
                let progress_tick = decide_progress_tick(&parts.running_steps.list_steps(), (parts.clock)());
                let until = [next_wake, progress_tick].into_iter().flatten().min();
                let waited = self.wait_for_round(until, &stop).await;
                let is_tick_alone = progress_tick.is_some() && waited.as_ref().is_some_and(BTreeSet::is_empty) && next_wake.is_none_or(|at| (parts.clock)() < at);
                if !is_tick_alone {
                    break waited;
                }
                self.publish_queue_status(false).await;
            };
            match waited {
                Some(given) => causes = given,
                None => break,
            }
        }
        parts.sending_holds.set_stopping();
        parts.running_steps.abort_requests();
        // Neither wait of a step holds this loop: a step towards the folder that waits for the folder's lease, which whoever stops the sync holds for writing, hears the stop and is not run (`StepRunners::of_runners`); a step that waits for the tree lock for its commit waits for one job of whoever holds it — a write operation of the mount, another step's commit —, none of which waits for the stop or for this loop.
        loop {
            self.settle_steps(self.take_ended_steps()).await;
            if parts.running_steps.count_steps_to_settle() == 0 {
                break;
            }
            parts.sync_plan_wake.wait().await;
        }
        // A stopped sync shows no count of its own: the task, the one writer, says so last.
        crate::upload_queue::publish_stopped_queue(&parts.state);
        // The task that cancels upload sessions heard the stop: it ends after the request it is at.
        let session_cancellations_task = crate::panic::lock(&self.session_cancellations_task).take();
        if let Some(Err(error)) = match session_cancellations_task {
            Some(task) => Some(task.await),
            None => None,
        } {
            tracing::warn!("the task that cancels upload sessions ended: {error}");
        }
    }

    /// Waits for the next round: a cause given, `until` reached, or `stop`. `None` for
    /// the stop. A time reached is an empty set of causes.
    async fn wait_for_round(&self, until: Option<i64>, stop: &CancellationToken) -> Option<BTreeSet<WakeCause>> {
        let wait_until_time = async {
            match until {
                Some(until) => tokio::time::sleep(Duration::from_secs(u64::try_from(until.saturating_sub((self.parts.clock)())).unwrap_or(0))).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => None,
            causes = self.parts.sync_plan_wake.wait() => Some(causes),
            () = wait_until_time => Some(BTreeSet::new()),
        }
    }

    /// Applies what a wake's causes ask before the plan is made: a read of the quota that
    /// is due, or that a refresh asks for (`onedrive_space::decide_is_quota_read`;
    /// `quota_reader`, `OneDriveSpace::apply_quota`, then
    /// `StepMemory::forget_fitting_space_refusals`; a due one that failed, or said nothing of
    /// the space, is not asked again before `READ_RETRY_WAIT` passed); a refresh (`WakeCause::RefreshAsked`:
    /// `StepMemory::clear_next_tries`, and `LocalStepTries::clear_next_tries` of the
    /// task's own); a listing ended (`SendingHolds::set_listing_ended`, the count of
    /// logged events to 0, and the full listings asked for forgotten once the store's
    /// request has begun). And, when sending is not held: the upload sessions left to
    /// cancel and the items uploads left are asked for, off the loop
    /// ([`start_session_cancellations`](Self::start_session_cancellations)).
    async fn apply_wake_causes(self: &Arc<Self>, causes: &BTreeSet<WakeCause>, stop: &CancellationToken) {
        let parts = &self.parts;
        let now = (parts.clock)();
        // On `WakeCause::QuotaRead` nothing is asked of OneDrive: whoever read the quota applied it (`OneDriveSpace::apply_quota`) before it woke the task, which takes what is free from there.
        if causes.contains(&WakeCause::QuotaRead) {
            if let Some(free_space) = parts.onedrive_space.find_free_space() {
                if let Err(error) = parts.store.call(move |store| store.step_memory().forget_fitting_space_refusals(free_space)).await {
                    tracing::warn!("cannot forget the refusals for lack of space that fit now: {error}");
                }
            }
        }
        let is_refresh_asked = causes.contains(&WakeCause::RefreshAsked);
        let last_failed_read_at = *crate::panic::lock(&self.quota_read_failed_at);
        if crate::sync_parts::onedrive_space::decide_is_quota_read(is_refresh_asked, parts.onedrive_space.find_next_read(), last_failed_read_at, READ_RETRY_WAIT, now) {
            let change = (parts.quota_reader)().await.map(|quota| parts.onedrive_space.apply_quota(&quota, now));
            // A read that says nothing of the space leaves the read due, as one that failed: neither is asked again at once.
            let is_space_known = change.is_some_and(|change| change != SpaceChange::NotKnown);
            *crate::panic::lock(&self.quota_read_failed_at) = (!is_space_known).then_some(now);
            if let Some(SpaceChange::HasSpace { free_space, .. }) = change {
                if let Err(error) = parts.store.call(move |store| store.step_memory().forget_fitting_space_refusals(free_space)).await {
                    tracing::warn!("cannot forget the refusals for lack of space that fit now: {error}");
                }
            }
        }
        if is_refresh_asked {
            if let Err(error) = parts.store.call(|store| store.step_memory().clear_next_tries()).await {
                tracing::warn!("cannot make the next tries due: {error}");
            }
            crate::panic::lock(&self.local_step_tries).clear_next_tries();
        }
        if causes.contains(&WakeCause::ListingEnded) {
            parts.sending_holds.set_listing_ended();
            self.logged_since_listing_ended.store(0, Ordering::SeqCst);
            // A request the store still keeps has not begun: the listing that ended is not the one asked for.
            match parts.store.read_items(|items| items.is_full_listing_requested()).await {
                Ok(false) => crate::panic::lock(&self.full_listings_asked).clear(),
                Ok(true) => {}
                Err(error) => tracing::warn!("cannot read whether a full listing is asked for: {error}"),
            }
        }
        self.start_session_cancellations(stop);
    }

    /// Cancels the upload sessions left to cancel, removes the items uploads left with
    /// other content and settles the requests whose answer was never committed
    /// (`SessionCancellations::cancel_sessions`, `remove_mismatched_items`,
    /// `settle_request_records`; a record settled wakes the plan,
    /// `WakeCause::RequestRecordSettled`),
    /// as a task of its own, one at a time: the loop waits for none of these requests.
    /// It asks one of each at a time, and goes on until both lists are empty, `stop` came,
    /// sending is held, or a round got nothing through: what is left then is asked for
    /// with the next plan. It hears `stop` between two requests, and `run` awaits its end
    /// before it returns: nothing of it is sent or written after the folder's sync
    /// stopped.
    fn start_session_cancellations(self: &Arc<Self>, stop: &CancellationToken) {
        let is_held = |task: &Self| task.parts.sending_holds.is_held(StepClass::Metadata, (task.parts.clock)());
        let mut kept_task = crate::panic::lock(&self.session_cancellations_task);
        if stop.is_cancelled() || is_held(self) || kept_task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let (task, stop) = (Arc::clone(self), stop.clone());
        *kept_task = Some(tokio::spawn(async move {
            let cancellations = &task.parts.session_cancellations;
            // The next try of each kind, as its last round answered it.
            let mut next_tries: [Option<i64>; 3] = [None; 3];
            while !stop.is_cancelled() && !is_held(&task) {
                let read = |items: &konedrive_tree::ItemReader<'_>| Ok((!items.list_upload_session_cancellations(1)?.is_empty(), !items.list_mismatched_item_removals(1)?.is_empty(), !items.list_request_records(1)?.is_empty()));
                let (has_cancellation, has_removal, has_request_record) = match task.parts.store.read_items(read).await {
                    Ok(left) => left,
                    Err(error) => {
                        tracing::warn!("cannot read what uploads left to cancel and to remove: {error}");
                        break;
                    }
                };
                // One of each at a time, so that the stop is heard between two requests. What failed waits for its next try and is passed over by `SessionCancellations`, which keeps the tries: a round that gets nothing through ends this task, and a later plan starts it again.
                let mut ask = async |is_asked: bool, kind: usize| {
                    if !is_asked || stop.is_cancelled() {
                        return false;
                    }
                    let round = match kind {
                        0 => cancellations.cancel_sessions(1).await,
                        1 => cancellations.remove_mismatched_items(1).await,
                        _ => cancellations.settle_request_records(1).await,
                    };
                    next_tries[kind] = round.next_try_at;
                    // A record OneDrive refuses to be read for: the listing brings what became of its request.
                    if round.left_to_listing_count > 0 {
                        task.parts.poll.refresh();
                    }
                    round.done_count > 0
                };
                let is_cancelled = ask(has_cancellation, 0).await;
                let is_removed = ask(has_removal, 1).await;
                // A request whose answer was never committed is settled by reading OneDrive: its item and its destination are busy to the plan until then, so the plan is made again once one is.
                let is_settled = ask(has_request_record, 2).await;
                if is_settled {
                    task.parts.sync_plan_wake.wake(WakeCause::RequestRecordSettled);
                }
                if !is_cancelled && !is_removed && !is_settled {
                    break;
                }
            }
            // What could not be got through is tried again at its time: the timer takes it with the next plan, which is made now when the time is another than it was set with.
            let next_cleaning_try = next_tries.into_iter().flatten().min();
            let is_changed = std::mem::replace(&mut *crate::panic::lock(&task.next_cleaning_try), next_cleaning_try) != next_cleaning_try;
            if is_changed && next_cleaning_try.is_some() {
                task.parts.sync_plan_wake.wake(WakeCause::NextCleaningTryChanged);
            }
        }));
    }

    /// Publishes what the properties of the bus say of the queue
    /// (`upload_queue::publish_queue_status`), from ONE `Store::read_items` with the
    /// published answers and the running steps taken before it
    /// (`upload_queue::read_queue_totals`, `count_queue`, `read_running_upload_paths`,
    /// `describe_running_uploads`), `read_queue_conditions` taken before the read too.
    /// With `is_counts_read` false only the bytes of the running uploads are published,
    /// with the counts as they were last read: the tick's round, which reads no row but
    /// the paths of the uploads. The task is the ONE writer of these fields. Called by
    /// the loop after every plan it published, after every round in which a step was
    /// settled, and at the tick ([`decide_progress_tick`]); a read that fails is a warning
    /// and publishes nothing.
    async fn publish_queue_status(&self, is_counts_read: bool) {
        let parts = &self.parts;
        let conditions = crate::upload_queue::read_queue_conditions(&parts.sending_holds, &parts.onedrive_space, (parts.clock)());
        let answers = parts.published_plan.list_answers();
        let running = parts.running_steps.list_steps();
        let read = {
            let running = running.clone();
            move |items: &konedrive_tree::ItemReader<'_>| {
                let totals = if is_counts_read { Some(crate::upload_queue::read_queue_totals(items, &answers, &running)?) } else { None };
                Ok((totals, crate::upload_queue::read_running_upload_paths(items, &running)?))
            }
        };
        let (totals, upload_paths) = match parts.store.read_items(read).await {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!("cannot read the upload queue of {}: {error}", parts.folder.display());
                return;
            }
        };
        let counts = {
            let mut queue_counts = crate::panic::lock(&self.queue_counts);
            if let Some(totals) = totals {
                *queue_counts = crate::upload_queue::count_queue(&totals, &conditions);
            }
            *queue_counts
        };
        // The bytes are those the steps said before the read: a step that began since is published with the next round.
        let uploads = crate::upload_queue::describe_running_uploads(&running, &upload_paths, &parts.folder);
        crate::upload_queue::publish_queue_status(&parts.state, counts, uploads, conditions.is_onedrive_full);
    }

    /// Announces what a round's settlement wrote and changed: each event written
    /// (`Activity::announce`, in the order written; `Activity::prune` once when one of
    /// them is a conflict, so that `Conflicts.Count` says it); the sync marks of the
    /// items of the steps towards OneDrive that were settled (`marks_announcer`); the
    /// thumbnail filler nudged when a step towards the folder was written
    /// (`after_folder_steps`). Called by [`settle_steps`](Self::settle_steps) after the
    /// job of the store, with the rows that job wrote, and after each of the steps was
    /// ended (`RunningSteps::end`): who reads a mark again on the announcement must not
    /// find the step still running.
    async fn announce_settled_steps(&self, settled: &[(SyncStep, StepSettlement)], written: &[konedrive_tree::ActivityRow]) {
        let parts = &self.parts;
        for row in written {
            parts.activity.announce(row.clone());
        }
        // `Activity::prune` reads the store and asks the disk for the rescued files, through the mount for those inside the folder: off the loop's thread, and waited for only while the sync runs. Once the loop is stopping (`SendingHolds::set_stopping`, set before its last settlements) it is not asked: nothing here may wait for the mount to answer when a stop is to end, and `Conflicts.Count` is read again by the next listing and by `Conflicts.List`.
        let is_stopping = parts.sending_holds.find((parts.clock)()) == Some(SendingHold::Stopping);
        if !is_stopping && written.iter().any(|row| row.kind == konedrive_tree::ActivityKind::Conflict) {
            let activity = Arc::clone(&parts.activity);
            if let Err(error) = tokio::task::spawn_blocking(move || activity.prune()).await {
                tracing::warn!("the task that counts the conflicts ended: {error}");
            }
        }
        let sent_items: Vec<LocalItemId> = settled
            .iter()
            .filter(|(step, _)| match step {
                SyncStep::OneDrive(_) => true,
                SyncStep::Folder(_) | SyncStep::Row(_) => false,
            })
            .flat_map(|(step, _)| {
                let running_step = step_choice::describe_running_step(step);
                std::iter::once(running_step.item).chain(running_step.other_items)
            })
            .collect();
        if !sent_items.is_empty() {
            (parts.marks_announcer)(sent_items);
        }
        // A step that was written is the one whose tries are forgotten (`decide_step_settlement`): the settlement keeps no outcome.
        let is_folder_step_written = settled.iter().any(|(step, settlement)| match step {
            SyncStep::Folder(_) => settlement.local_step_tries == step_settlement::LocalStepTriesChange::Forgotten,
            SyncStep::OneDrive(_) | SyncStep::Row(_) => false,
        });
        if let (true, Some(after_folder_steps)) = (is_folder_step_written, &parts.after_folder_steps) {
            after_folder_steps.notify_one();
        }
    }

    /// What the daemon knows beside the rows, as values, read now: the items with a
    /// running step (`RunningSteps::list_items`), whether OneDrive is full, the time, the
    /// destinations of the running steps, the rows the check at start left, and a copy of
    /// the task's tries of the steps towards the folder and of the rows. What is open for
    /// writing is NOT gathered here: it is read from the rows of the plan's own moment.
    fn gather_daemon_facts(&self) -> DaemonFacts {
        let parts = &self.parts;
        // The running steps; the plan's reading adds the requests on record whose answer was never committed (`plan_input::describe_folder`, `ItemReader::list_request_records`), which no step runs for any more.
        DaemonFacts {
            items_with_running_step: parts.running_steps.list_items(),
            destinations_of_running_steps: parts.running_steps.list_destinations(),
            is_onedrive_full: parts.onedrive_space.is_full(),
            now: (parts.clock)(),
            left_by_start_up_check: parts.start_check.held_items.clone(),
            local_step_tries: crate::panic::lock(&self.local_step_tries).clone(),
            is_pinned_files_reader_asked: self.missing_pinned_files.is_reader_asked(),
            settings: parts.settings.clone(),
        }
    }

    /// Makes the plan: `Store::read_items(plan_input::plan_next_steps)` with the facts
    /// gathered, on the store's reading connection. The task's tries are from then on
    /// those the round answers (`PlannedRound::local_step_tries`): the ones the rows of
    /// that reading still say.
    async fn make_plan(&self, facts: DaemonFacts) -> Result<PlannedRound, konedrive_tree::TreeError> {
        let round = self.parts.store.read_items(move |items| plan_input::plan_next_steps(items, &facts)).await?;
        *crate::panic::lock(&self.local_step_tries) = round.local_step_tries.clone();
        self.missing_pinned_files.note_reading(round.pinned_files_reading);
        Ok(round)
    }

    /// How many steps of each class may run at once: as many uploads and downloads as the
    /// pool has slots for (the map's F19), and one for the metadata. A count only: no
    /// slot is taken by the task.
    fn read_step_limits(&self) -> StepLimits {
        let slot_count = self.parts.pool.size();
        StepLimits { content: slot_count, metadata: 1, download: slot_count }
    }

    /// The steps that ended since the loop last took them.
    fn take_ended_steps(&self) -> Vec<EndedStep> {
        std::mem::take(&mut *crate::panic::lock(&self.ended_steps))
    }

    /// Hands the steps that ended to the loop, and wakes it (`WakeCause::StepFinished`).
    /// Called when the task of a step ended: with what its runner answered, or with a
    /// failure when it answered nothing (`decide_outcome_of_ended_task`), and with the
    /// count of plans it was started at.
    fn hand_over_ended_steps(&self, ended: Vec<(SyncStep, StepOutcome)>, plan_count_at_start: u64) {
        if ended.is_empty() {
            return;
        }
        crate::panic::lock(&self.ended_steps).extend(ended.into_iter().map(|(step, outcome)| EndedStep { step, outcome, plan_count_at_start }));
        self.parts.sync_plan_wake.wake(WakeCause::StepFinished);
    }

    /// Starts the steps of `planned` that can start at `now`
    /// (`step_choice::decide_steps_to_start`): the `ShowItem` steps as one task, every
    /// other as its own. For each step towards OneDrive it starts, the sync mark of its
    /// items is announced (`marks_announcer`: the emblem turns to "uploading"). Answers the steps that stay for a later round of this plan. While
    /// OneDrive is full no content is sent: the steps of the last plan that send content
    /// stay, each would be refused in its turn.
    fn start_steps(self: &Arc<Self>, planned: Vec<SyncStep>, now: i64) -> Vec<SyncStep> {
        let parts = &self.parts;
        let mut held_classes = parts.sending_holds.list_held_classes(now);
        if parts.onedrive_space.is_full() {
            held_classes.insert(StepClass::Content);
        }
        let choice = step_choice::decide_steps_to_start(planned, &parts.running_steps.list_steps(), &self.read_step_limits(), &held_classes);
        if !choice.show_steps.is_empty() {
            self.start_show_steps(choice.show_steps);
        }
        for step in choice.steps {
            self.start_step(step);
        }
        choice.left
    }

    /// Starts `step`, any but a `ShowItem`, as a task of its own: marks every row it
    /// carries as running, with the directory it sends into (`RunningSteps::begin` with
    /// `step_choice::describe_running_step`), spawns it, keeps its abort handle
    /// (`RunningSteps::keep_abort_handle`), runs it through `StepRunners::run_step`, and
    /// hands its outcome to the loop, which settles it.
    fn start_step(self: &Arc<Self>, step: SyncStep) {
        let running_step = step_choice::describe_running_step(&step);
        let item = running_step.item;
        let rows: Vec<LocalItemId> = std::iter::once(item).chain(running_step.other_items.iter().copied()).collect();
        if !self.parts.running_steps.begin(running_step) {
            return;
        }
        tracing::debug!("a step starts: {} of item {item}", step_choice::find_step_name(&step));
        // A step towards OneDrive runs for these rows from here on: their sync mark says so.
        if matches!(step, SyncStep::OneDrive(_)) {
            (self.parts.marks_announcer)(rows);
        }
        let running = tokio::spawn((self.parts.step_runners.run_step)(step.clone()));
        self.parts.running_steps.keep_abort_handle(item, running.abort_handle());
        // Whatever ended the step's task, its outcome is handed over: a panic too.
        let (task, plan_count_at_start) = (Arc::clone(self), self.plan_count.load(Ordering::SeqCst));
        tokio::spawn(async move {
            let outcome = match running.await {
                Ok(outcome) => Some(outcome),
                Err(error) => decide_outcome_of_ended_task(error.is_cancelled(), format!("the step's task ended: {error}")),
            };
            task.hand_over_ended_steps(outcome.map(|outcome| (step, outcome)).into_iter().collect(), plan_count_at_start);
        });
    }

    /// Starts the `ShowItem` steps of one plan as ONE task: every item marked running,
    /// `StepRunners::run_show_steps`, each outcome handed to the loop. (Whose a listed
    /// file at a place of this folder's own opening is, is not asked here: nobody calls
    /// the owner decided that no layer asks: a file uploaded and then removed here whose
    /// confirmation was lost is shown again as any item of OneDrive's.)
    fn start_show_steps(self: &Arc<Self>, steps: Vec<SyncStep>) {
        let mut started = Vec::new();
        let mut items = Vec::new();
        for step in steps {
            let SyncStep::Folder(FolderStep::ShowItem { item }) = &step else { continue };
            if self.parts.running_steps.begin(step_choice::describe_running_step(&step)) {
                items.push(item.clone());
                started.push(step);
            }
        }
        if started.is_empty() {
            return;
        }
        let shown_items: Vec<LocalItemId> = started.iter().map(SyncStep::local_item_id).collect();
        tracing::debug!("{} ShowItem step(s) start as one job, of the items {shown_items:?}", shown_items.len());
        let running = tokio::spawn((self.parts.step_runners.run_show_steps)(items));
        for item in shown_items {
            self.parts.running_steps.keep_abort_handle(item, running.abort_handle());
        }
        // Whatever ended the task, an outcome of every step is handed over: a panic too.
        let (task, plan_count_at_start) = (Arc::clone(self), self.plan_count.load(Ordering::SeqCst));
        tokio::spawn(async move {
            let ended = match running.await {
                // An outcome the runner did not answer is that of a step that did not run.
                Ok(outcomes) => {
                    let mut outcomes = outcomes.into_iter();
                    started.into_iter().map(|step| (step, outcomes.next().unwrap_or(StepOutcome::NotRun(StepObstacle::SendingStopped)))).collect()
                }
                Err(error) => match decide_outcome_of_ended_task(error.is_cancelled(), format!("the task of the show steps ended: {error}")) {
                    Some(outcome) => started.into_iter().map(|step| (step, outcome.clone())).collect(),
                    None => Vec::new(),
                },
            };
            task.hand_over_ended_steps(ended, plan_count_at_start);
        });
    }

    /// Settles the steps that ended, all that are ready together: for each
    /// `step_settlement::decide_step_settlement`, with the count of tries
    /// (`step_settlement::count_failed_tries`: the task's own, and for a step towards
    /// OneDrive planned from an unsent row what the store keeps under that stamp, read
    /// here for the settlement — `step_settlement::read_failed_try_counts` — so that a
    /// step whose item the last plan was not given counts on all the same);
    /// the task's own tries changed (`step_settlement::change_local_step_tries`); all
    /// that is the store's written in ONE job of the store
    /// (`step_settlement::write_step_settlements`: the step memory and the activity log),
    /// and what could not be written there kept as a try of the task's
    /// (`step_settlement::keep_unwritten_tries`), so that no step is planned again with
    /// no wait;
    /// then what they say of all sending applied (`SendingHolds`), the listing asked for
    /// a cycle once when any says so, each step ended (`RunningSteps::end`). The loop
    /// goes on with a round of `WakeCause::StepFinished`. Answers what `decide_round` asks of
    /// the steps settled: whether any changed something a plan is made from
    /// (`step_settlement::has_changed_anything`), and whether a plan was made since any
    /// of them began. Nothing, and neither, for no step.
    async fn settle_steps(&self, ended: Vec<EndedStep>) -> SettledSteps {
        if ended.is_empty() {
            return SettledSteps::default();
        }
        let plan_count = self.plan_count.load(Ordering::SeqCst);
        let is_plan_made_since_start = ended.iter().any(|ended| ended.plan_count_at_start < plan_count);
        let ended: Vec<(SyncStep, StepOutcome)> = ended.into_iter().map(|ended| (ended.step, ended.outcome)).collect();
        let parts = &self.parts;
        let now = (parts.clock)();
        self.missing_pinned_files.note_ended_steps(&ended);
        // The tries the store keeps, of the steps whose tries are its own: read now, whatever the last plan was given.
        let kept_by_store: Vec<(LocalItemId, Option<u64>)> = ended
            .iter()
            .filter_map(|(step, _)| match step {
                SyncStep::OneDrive(sent) if LocalStep::of(step).is_none() => Some((step.local_item_id(), sent.item().unsent_since)),
                SyncStep::OneDrive(_) | SyncStep::Folder(_) | SyncStep::Row(_) => None,
            })
            .collect();
        let store_counts = if kept_by_store.is_empty() {
            BTreeMap::new()
        } else {
            // A count that cannot be read is the task's own: of the tries it kept when the store could not be written.
            parts.store.read_items(move |items| step_settlement::read_failed_try_counts(items, &kept_by_store)).await.unwrap_or_else(|error| {
                tracing::warn!("cannot read how many tries of the ended steps failed: {error}");
                BTreeMap::new()
            })
        };
        let settled: Vec<(SyncStep, StepSettlement)> = {
            let mut local_step_tries = crate::panic::lock(&self.local_step_tries);
            let settled: Vec<(SyncStep, StepSettlement)> =
                ended.iter().map(|(step, outcome)| (step.clone(), step_settlement::decide_step_settlement(step, outcome, step_settlement::count_failed_tries(step, &local_step_tries, &store_counts), now))).collect();
            step_settlement::change_local_step_tries(&mut local_step_tries, &settled);
            settled
        };
        for (step, outcome) in &ended {
            tracing::debug!("a step ended: {} of item {}: {}", step_choice::find_step_name(step), step.local_item_id(), step_settlement::describe_outcome(outcome));
            match outcome {
                StepOutcome::NotRun(obstacle @ (StepObstacle::ContentChanged | StepObstacle::BackingFileMissing | StepObstacle::PlaceHeldOnDisk { .. })) => {
                    tracing::warn!("a step of item {} did not run: {obstacle:?}", step.local_item_id());
                }
                StepOutcome::Failed(failure) => tracing::warn!("a step of item {} failed: {failure:?}", step.local_item_id()),
                StepOutcome::Written { .. } | StepOutcome::RowsChanged(_) | StepOutcome::Refused(_) | StepOutcome::NotRun(_) => {}
            }
        }
        let settlements: Vec<StepSettlement> = settled.iter().map(|(_, settlement)| settlement.clone()).collect();
        let (folder, logged_since_listing_ended) = (parts.folder.clone(), self.logged_since_listing_ended.load(Ordering::SeqCst));
        let written = match parts.store.call(move |store| step_settlement::write_step_settlements(store, &settlements, &folder, logged_since_listing_ended, now)).await {
            Ok(written) => {
                self.logged_since_listing_ended.fetch_add(written.logged_count, Ordering::SeqCst);
                step_settlement::keep_unwritten_tries(&mut crate::panic::lock(&self.local_step_tries), &settled, &written.unwritten_items, now);
                written.rows
            }
            // Nothing was kept in the store: the task keeps a try of each step that had something to keep there, so that it waits.
            Err(error) => {
                tracing::warn!("cannot write what came of {} steps: {error}", settled.len());
                let unwritten: BTreeSet<LocalItemId> = settled.iter().map(|(_, settlement)| settlement.item).collect();
                step_settlement::keep_unwritten_tries(&mut crate::panic::lock(&self.local_step_tries), &settled, &unwritten, now);
                Vec::new()
            }
        };
        for ((_, outcome), (_, settlement)) in ended.iter().zip(&settled) {
            match (&settlement.hold, outcome) {
                (Some(SendingHold::Throttled { .. }), StepOutcome::Refused(StepRefusal::Throttled { retry_after })) => parts.sending_holds.throttle(*retry_after, now),
                (Some(SendingHold::SignedOut), _) => parts.sending_holds.set_signed_out(true),
                _ => {}
            }
            if settlement.went_through {
                parts.sending_holds.pass();
            }
        }
        let full_listing_cause = settled.iter().find_map(|(_, settlement)| if let CycleRequest::Full(cause) = settlement.cycle { Some(cause) } else { None });
        if let Some(cause) = full_listing_cause {
            match parts.store.call(move |store| store.item_store().request_full_listing(cause)).await {
                Ok(()) => parts.poll.refresh_after_full_listing_request(),
                Err(error) => tracing::warn!("cannot ask for a full listing: {error}"),
            }
        } else if settled.iter().any(|(_, settlement)| settlement.cycle == CycleRequest::Changes) {
            parts.poll.refresh();
        }
        for (step, _) in &settled {
            parts.running_steps.end(step.local_item_id());
        }
        // What was written and changed is said, once no step of these runs: the events, the sync marks of what was sent, the thumbnail filler.
        self.announce_settled_steps(&settled, &written).await;
        let has_change = ended.iter().zip(&settled).any(|((_, outcome), (_, settlement))| step_settlement::has_changed_anything(outcome, settlement));
        SettledSteps { has_change, is_plan_made_since_start }
    }

    /// Publishes what a plan answered: the answers per item (`PublishedPlan::publish`),
    /// the folder's counts (`PlannedRound::item_counts`, of the plan's own reading) and
    /// note on the bus (through `state`), the faults of the plan in the journal, each
    /// once. The task is the one owner of the note (`UploadQueueStatus::note`): it writes it
    /// with every plan, also to none, and nobody else may. For
    /// `PlanFault::FullListingNeeded` the store is asked for a full listing
    /// (`ItemStore::request_full_listing`) and then the poller for a cycle
    /// (`ListingPollHandle::refresh_after_full_listing_request`): once per item until a
    /// listing asked for it ended, and again when the request failed.
    async fn publish_round(&self, round: &PlannedRound, hold: Option<&SendingHold>) {
        let parts = &self.parts;
        let plan = &round.plan;
        let mut answers = BTreeMap::new();
        answers.extend(plan.steps.iter().map(|step| (step.local_item_id(), ItemAnswer::Planned(step_choice::decide_step_class(step)))));
        answers.extend(plan.waits.iter().map(|wait| (wait.item, ItemAnswer::Waiting(wait.reason.clone()))));
        answers.extend(plan.exclusions.iter().map(|exclusion| (exclusion.item, ItemAnswer::Excluded(exclusion.reason))));
        answers.extend(plan.faults.iter().map(|fault| (fault.local_item_id(), ItemAnswer::Fault(fault.clone()))));
        // What the mark of an item is answered from changes with the answers: the items whose answer turned to or from "blocked" are said to whoever watches the folder.
        let has_refusal_for_good = |item: &LocalItemId| round.memories.get(item).is_some_and(|memory| memory.refusal.is_some());
        let answers_before = parts.published_plan.list_answers();
        let marks_changed: Vec<LocalItemId> = answers_before
            .keys()
            .chain(answers.keys())
            .copied()
            .collect::<BTreeSet<LocalItemId>>()
            .into_iter()
            .filter(|item| decide_is_shown_blocked(answers_before.get(item), has_refusal_for_good(item)) != decide_is_shown_blocked(answers.get(item), has_refusal_for_good(item)))
            .collect();
        parts.published_plan.publish(answers);
        parts.published_plan.publish_unsent_not_to_send_count(round.unsent_not_to_send_count);
        if !marks_changed.is_empty() {
            (parts.marks_announcer)(marks_changed);
        }
        (parts.unsent_count_report)(round.unsent_to_send_count);

        let note = match hold {
            Some(SendingHold::Throttled { until }) => Some(UploadQueueNote::Throttled(*until)),
            Some(SendingHold::WritesNotAllowed { reason }) => Some(UploadQueueNote::WritesNotAllowed(reason.clone())),
            Some(SendingHold::Stopping | SendingHold::SignedOut | SendingHold::FolderReadOnly | SendingHold::Paused { .. } | SendingHold::FirstListingNotEnded) | None => None,
        };
        let counts = round.item_counts;
        parts.state.update_if_changed(|state| {
            state.cycle.items_listed = counts.in_onedrive;
            state.cycle.items_placed = counts.shown;
            state.cycle.skipped_count = counts.cannot_be_shown;
            state.local.pinned_count = u32::try_from(counts.pinned).unwrap_or(u32::MAX);
            state.upload_queue.note = note;
        });

        // Each fault once: not one the plan before answered too.
        let new_faults: Vec<PlanFault> = {
            let mut told_faults = crate::panic::lock(&self.told_faults);
            let new_faults = plan.faults.iter().filter(|fault| !told_faults.contains(fault)).cloned().collect();
            told_faults.clone_from(&plan.faults);
            new_faults
        };
        for fault in new_faults {
            tracing::warn!("the sync plan of {} is at fault: {fault:?}", parts.folder.display());
        }
        let asked = crate::panic::lock(&self.full_listings_asked).clone();
        for item in plan.faults.iter().filter_map(|fault| if let PlanFault::FullListingNeeded(item) = fault { Some(*item) } else { None }).filter(|item| !asked.contains(item)) {
            match parts.store.call(move |store| store.item_store().request_full_listing(FullListingCause::PlanFault(item))).await {
                Ok(()) => {
                    crate::panic::lock(&self.full_listings_asked).insert(item);
                    parts.poll.refresh_after_full_listing_request();
                }
                // Not kept as asked: the next plan that answers the fault asks again.
                Err(error) => tracing::warn!("cannot ask for a full listing for item {item}: {error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests;

//! The task of a folder's sync plan: the loop that reads what differs, makes the plan,
//! runs its steps and settles what came of each. One per folder, in either access mode,
//! in place of the outbox worker and of the reconcile.
//!
//! **State owned:** the loop, with the steps of the last plan that are not started yet,
//! the count of events logged since the last listing ended, and the failed tries of the
//! steps towards the folder and of the rows ([`LocalStepTries`], which says why they are
//! kept here and not in the store); nothing a restart must find again: after one, such a
//! step is tried again at once. Everything else has an owner of its own: the steps running
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
//! is asked with every plan: a plan that left the pinned files out would drop the
//! downloads the last one planned.
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
//! **Tested without the rest**, over the fixture (`fuse::item_fixture`) with its stand-in
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
// test: rows_changed_at_random_on_both_sides_converge_through_the_task

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::drive::DriveQuota;
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{FolderStep, FullListingCause, ItemMemory, LocalItemId, PlanFault, Store, SyncPlan, SyncStep, WaitReason};
use tokio_util::sync::CancellationToken;

use crate::folder::published_plan::{ItemAnswer, PublishedPlan};
use crate::folder::running_steps::{RunningSteps, StepClass};
use crate::folder::step_outcome::{StepFailure, StepObstacle, StepOutcome, StepRefusal};
use crate::folder::sync_plan_wake::{SyncPlanWake, WakeCause};
use crate::fuse::StartCheckFindings;
use crate::remote::listing_poller::ListingPollHandle;
use crate::status::snapshot::{OutboxNote, SyncStateHandle};
use crate::upload::onedrive_space::{OneDriveSpace, SpaceChange};
use crate::upload::onedrive_steps::session_cancellations::SessionCancellations;
use crate::upload::sending_holds::{SendingHold, SendingHolds};

/// The failed tries of the steps towards the folder and of the rows, kept for the run.
pub mod local_step_tries;
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
    /// could not be read. Asked when the quota was read by another
    /// (`WakeCause::QuotaRead`) and when the quota's next read is due
    /// (`OneDriveSpace::find_next_read`); what it answers is applied by
    /// `OneDriveSpace::apply_quota`.
    pub quota_reader: Arc<dyn Fn() -> RunningFuture<Option<DriveQuota>> + Send + Sync>,
}

/// How many upload sessions are cancelled, and how many items uploads left with other
/// content are removed, by one call of `SessionCancellations`.
const SESSION_CANCELLATIONS_AT_ONCE: usize = 16;

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
    /// The failed tries of the steps towards the folder and of the rows, for this run.
    /// Changed by the loop alone: when a plan is made, to what the rows of that plan's
    /// reading still say (`PlannedRound::local_step_tries`); at a settlement
    /// (`step_settlement::change_local_step_tries`); and at a refresh
    /// (`WakeCause::RefreshAsked`, `LocalStepTries::clear_next_tries`).
    local_step_tries: std::sync::Mutex<LocalStepTries>,
    /// The steps that ended and are not settled yet, each with what came of it: put by
    /// the task of a step, taken by the loop.
    ended_steps: std::sync::Mutex<Vec<(SyncStep, StepOutcome)>>,
    /// The faults of the last plan: what a fault is said once by.
    told_faults: std::sync::Mutex<Vec<PlanFault>>,
    /// The items a full listing was asked for (`PlanFault::FullListingNeeded`) that no
    /// listing ended for since: none is asked for these again. Emptied when a listing
    /// ended and the store's request has begun.
    full_listings_asked: std::sync::Mutex<BTreeSet<LocalItemId>>,
    /// When the read of the quota that was due failed last, Unix seconds: it is not asked
    /// again before `READ_RETRY_WAIT` passed. None once a read answered.
    quota_read_failed_at: std::sync::Mutex<Option<i64>>,
    /// Whether the task that cancels upload sessions and removes the items uploads left
    /// runs: one at a time.
    are_session_cancellations_running: Arc<AtomicBool>,
}

/// What a round does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundDecision {
    /// A plan is made: a cause other than a finished step came, a time was reached, or a
    /// step that was settled in this round changed something.
    MakePlan,
    /// The last plan is gone on with: steps of it are left to start, only steps finished
    /// since, and none of them changed anything.
    GoOnWithLastPlan,
    /// Nothing to do until the next cause or time.
    Sleep,
}

/// What the round does, from the causes that woke it (none: a time was reached), whether
/// a step settled in this round changed anything a plan is made from
/// (`step_settlement::has_changed_anything`), and how many steps of the last plan are not
/// started. A round of finished steps alone makes a plan when one of them changed
/// something — also while steps of the last plan are left, which the new plan's replace:
/// what waited for the step is planned at once, whatever holds the steps left. When none
/// changed anything no plan is made by that round — the plan would be the one before, and
/// a step that cannot run (the mount's take-down asked for, the write-operation thread
/// gone) would be planned and ended without a wait between: the steps left that can start
/// are started, else the task sleeps until a cause or a time. Pure.
pub fn decide_round(causes: &BTreeSet<WakeCause>, has_settled_change: bool, steps_left_to_start: usize) -> RoundDecision {
    let is_only_finished_steps = !causes.is_empty() && causes.iter().all(|cause| *cause == WakeCause::StepFinished);
    match (is_only_finished_steps, has_settled_change, steps_left_to_start) {
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
    // TODO: `StepFailure` has no case for a runner that panicked; `Store` is said in its place, and the reason kept is the store's. Check whether a case of its own is wanted, and fix if needed.
    (!is_cancelled).then_some(StepOutcome::Failed(StepFailure::Store { detail }))
}

/// When the task wakes by itself next, Unix seconds: the soonest of the plan's waits
/// until a time, the end of what holds all sending when it has one
/// (`SendingHolds::find_end`), and the quota's next read; `None` when nothing is due.
/// Never sooner than a second from `now`. Pure. Today's `Engine::next_due`
/// (`upload/engine.rs:480`).
pub fn decide_next_wake(plan: &SyncPlan, hold_end: Option<i64>, next_quota_read: Option<i64>, now: i64) -> Option<i64> {
    let waits = plan.waits.iter().filter_map(|wait| if let WaitReason::Time { until, .. } = wait.reason { Some(until) } else { None });
    waits.chain(hold_end).chain(next_quota_read).min().map(|soonest| soonest.max(now.saturating_add(1)))
}

impl SyncPlanTask {
    /// Made by the folder's start (`sync_tasks::start_order::start_sync_tasks`).
    pub fn new(parts: SyncPlanTaskParts) -> Arc<Self> {
        Arc::new(SyncPlanTask { parts, logged_since_listing_ended: AtomicUsize::new(0), local_step_tries: std::sync::Mutex::new(LocalStepTries::default()), ended_steps: std::sync::Mutex::default(), told_faults: std::sync::Mutex::default(), full_listings_asked: std::sync::Mutex::default(), quota_read_failed_at: std::sync::Mutex::new(None), are_session_cancellations_running: Arc::new(AtomicBool::new(false)) })
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
        let mut memories = BTreeMap::new();
        let mut next_wake = None;
        // The first round is one of a time reached: a plan is made.
        let mut causes = BTreeSet::new();
        loop {
            let has_settled_change = self.settle_steps(self.take_ended_steps(), &memories).await;
            match decide_round(&causes, has_settled_change, steps_left_to_start.len()) {
                RoundDecision::MakePlan => {
                    let now = (parts.clock)();
                    if now.saturating_sub(old_openings_forgotten_at) >= OLD_OPENINGS_FORGOTTEN_EVERY {
                        parts.session_cancellations.forget_old_openings().await;
                        old_openings_forgotten_at = now;
                    }
                    self.apply_wake_causes(&causes).await;
                    match self.make_plan(self.gather_daemon_facts()).await {
                        Ok(round) => {
                            let now = (parts.clock)();
                            self.publish_round(&round, parts.sending_holds.find(now).as_ref()).await;
                            // A read of the quota that was due and failed is tried again after a wait, not at once.
                            // The steps the last plan left are replaced by this plan's.
                            let next_quota_read = parts.onedrive_space.find_next_read().map(|at| if at > now { at } else { now.saturating_add(READ_RETRY_WAIT) });
                            next_wake = decide_next_wake(&round.plan, parts.sending_holds.find_end(now), next_quota_read, now);
                            steps_left_to_start = self.start_steps(round.plan.steps, now);
                            memories = round.memories;
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
            match self.wait_for_round(next_wake, &stop).await {
                Some(given) => causes = given,
                None => break,
            }
        }
        parts.sending_holds.set_stopping();
        parts.running_steps.abort_requests();
        // TODO: a step towards the folder waits for the folder's lease and a step of the rows for the tree lock, and a stop aborts neither (the review's Q3): if whoever awaits this holds the lease for writing, or the tree lock, this loop does not end. Check `SyncTasks::stop` and its callers once they are filled, and fix if needed.
        loop {
            self.settle_steps(self.take_ended_steps(), &memories).await;
            if parts.running_steps.count_steps_to_settle() == 0 {
                break;
            }
            parts.sync_plan_wake.wait().await;
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
    /// is due (`quota_reader`, `OneDriveSpace::apply_quota`, then
    /// `StepMemory::forget_fitting_space_refusals`; one that failed is not asked again
    /// before `READ_RETRY_WAIT` passed); a refresh (`WakeCause::RefreshAsked`:
    /// `StepMemory::clear_next_tries`, and `LocalStepTries::clear_next_tries` of the
    /// task's own); a listing ended (`SendingHolds::set_listing_ended`, the count of
    /// logged events to 0, and the full listings asked for forgotten once the store's
    /// request has begun). And, when sending is not held: the upload sessions left to
    /// cancel and the items uploads left are asked for, off the loop
    /// ([`start_session_cancellations`](Self::start_session_cancellations)).
    async fn apply_wake_causes(self: &Arc<Self>, causes: &BTreeSet<WakeCause>) {
        let parts = &self.parts;
        let now = (parts.clock)();
        // TODO: on `WakeCause::QuotaRead` the quota another just read is to be applied (`OneDriveSpace::apply_quota`), with no second request; nothing gives the task that read (`OneDriveSpace` keeps the account's quota to itself). Check who wakes `QuotaRead` and how the figures come here, and fix if needed.
        let is_quota_read_due = parts.onedrive_space.find_next_read().is_some_and(|at| at <= now);
        let is_retry_due = crate::panic::lock(&self.quota_read_failed_at).is_none_or(|failed_at| now.saturating_sub(failed_at) >= READ_RETRY_WAIT);
        if is_quota_read_due && is_retry_due {
            let quota = (parts.quota_reader)().await;
            *crate::panic::lock(&self.quota_read_failed_at) = quota.is_none().then_some(now);
            if let Some(SpaceChange::HasSpace { free_space, .. }) = quota.map(|quota| parts.onedrive_space.apply_quota(&quota, now)) {
                if let Err(error) = parts.store.call(move |store| store.step_memory().forget_fitting_space_refusals(free_space)).await {
                    tracing::warn!("cannot forget the refusals for lack of space that fit now: {error}");
                }
            }
        }
        if causes.contains(&WakeCause::RefreshAsked) {
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
        self.start_session_cancellations();
    }

    /// Cancels the upload sessions left to cancel and removes the items uploads left with
    /// other content (`SessionCancellations::cancel_sessions`, `remove_mismatched_items`),
    /// as a task of its own, one at a time: the loop waits for none of these requests.
    /// It goes on, `SESSION_CANCELLATIONS_AT_ONCE` of each at a time, until both lists
    /// are empty, sending is held (a stop holds it), or a call got none of its own through:
    /// what is left then is asked for with the next plan.
    fn start_session_cancellations(self: &Arc<Self>) {
        let is_held = |task: &Self| task.parts.sending_holds.is_held(StepClass::Metadata, (task.parts.clock)());
        if is_held(self) || self.are_session_cancellations_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let task = Arc::clone(self);
        let cancellations = tokio::spawn(async move {
            while !is_held(&task) {
                let read = |items: &konedrive_tree::ItemReader<'_>| Ok((items.list_upload_session_cancellations(SESSION_CANCELLATIONS_AT_ONCE)?.len(), items.list_mismatched_item_removals(SESSION_CANCELLATIONS_AT_ONCE)?.len()));
                let (cancellation_count, removal_count) = match task.parts.store.read_items(read).await {
                    Ok(counts) => counts,
                    Err(error) => {
                        tracing::warn!("cannot read what uploads left to cancel and to remove: {error}");
                        break;
                    }
                };
                let mut left_count = 0;
                if cancellation_count > 0 {
                    left_count += task.parts.session_cancellations.cancel_sessions(SESSION_CANCELLATIONS_AT_ONCE).await;
                }
                if removal_count > 0 {
                    left_count += task.parts.session_cancellations.remove_mismatched_items(SESSION_CANCELLATIONS_AT_ONCE).await;
                }
                // TODO: nothing here waits between tries that fail (the review's Q5): a call that gets none through ends this task, and the next plan starts it again, so a session that cannot be cancelled is asked once per plan. Check whether `SessionCancellations` is to keep a next try, and fix if needed.
                if left_count >= cancellation_count + removal_count {
                    break;
                }
            }
        });
        // The flag is let go whatever ended the task, a panic too.
        let is_running = Arc::clone(&self.are_session_cancellations_running);
        tokio::spawn(async move {
            if let Err(error) = cancellations.await {
                tracing::warn!("the task that cancels upload sessions ended: {error}");
            }
            is_running.store(false, Ordering::SeqCst);
        });
    }

    /// What the daemon knows beside the rows, as values, read now: the items with a
    /// running step (`RunningSteps::list_items`), whether OneDrive is full, the time, the
    /// destinations of the running steps, the rows the check at start left, and a copy of
    /// the task's tries of the steps towards the folder and of the rows. What is open for
    /// writing is NOT gathered here: it is read from the rows of the plan's own moment.
    fn gather_daemon_facts(&self) -> DaemonFacts {
        let parts = &self.parts;
        // TODO: the facts name the running steps alone (the review's Q1). `DaemonFacts` says they hold a request whose answer was lost, and one a stop left, until its item was read from OneDrive: nothing here keeps such an item or its destination once its step ended (`Refused(Transient)` ends it). Check whether `OneDriveSteps` keeps the step running until then, and fix if needed.
        DaemonFacts {
            items_with_running_step: parts.running_steps.list_items(),
            destinations_of_running_steps: parts.running_steps.list_destinations(),
            is_onedrive_full: parts.onedrive_space.is_full(),
            now: (parts.clock)(),
            left_by_start_up_check: parts.start_check.held_items.clone(),
            local_step_tries: crate::panic::lock(&self.local_step_tries).clone(),
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
    fn take_ended_steps(&self) -> Vec<(SyncStep, StepOutcome)> {
        std::mem::take(&mut *crate::panic::lock(&self.ended_steps))
    }

    /// Hands the steps that ended to the loop, and wakes it (`WakeCause::StepFinished`).
    /// Called when the task of a step ended: with what its runner answered, or with a
    /// failure when it answered nothing (`decide_outcome_of_ended_task`).
    fn hand_over_ended_steps(&self, ended: Vec<(SyncStep, StepOutcome)>) {
        if ended.is_empty() {
            return;
        }
        crate::panic::lock(&self.ended_steps).extend(ended);
        self.parts.sync_plan_wake.wake(WakeCause::StepFinished);
    }

    /// Starts the steps of `planned` that can start at `now`
    /// (`step_choice::decide_steps_to_start`): the `ShowItem` steps as one task, every
    /// other as its own. Answers the steps that stay for a later round of this plan. While
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
        if !self.parts.running_steps.begin(running_step) {
            return;
        }
        let running = tokio::spawn((self.parts.step_runners.run_step)(step.clone()));
        self.parts.running_steps.keep_abort_handle(item, running.abort_handle());
        // Whatever ended the step's task, its outcome is handed over: a panic too.
        let task = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = match running.await {
                Ok(outcome) => Some(outcome),
                Err(error) => decide_outcome_of_ended_task(error.is_cancelled(), format!("the step's task ended: {error}")),
            };
            task.hand_over_ended_steps(outcome.map(|outcome| (step, outcome)).into_iter().collect());
        });
    }

    /// Starts the `ShowItem` steps of one plan as ONE task: every item marked running,
    /// `StepRunners::run_show_steps`, each outcome handed to the loop. (Whose a listed
    /// file at a place of this folder's own opening is, is not asked here: nobody calls
    /// `MadeItems::find_left_upload` until the owner said what follows from it.)
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
        let running = tokio::spawn((self.parts.step_runners.run_show_steps)(items));
        for item in shown_items {
            self.parts.running_steps.keep_abort_handle(item, running.abort_handle());
        }
        // Whatever ended the task, an outcome of every step is handed over: a panic too.
        let task = Arc::clone(self);
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
            task.hand_over_ended_steps(ended);
        });
    }

    /// Settles the steps that ended, all that are ready together: for each
    /// `step_settlement::decide_step_settlement`, with the count of tries from the round's
    /// memories for a step towards OneDrive and from the task's own tries for any other;
    /// the task's own tries changed (`step_settlement::change_local_step_tries`); all
    /// that is the store's written in ONE job of the store
    /// (`step_settlement::write_step_settlements`: the step memory and the activity log);
    /// then what they say of all sending applied (`SendingHolds`), the listing asked for
    /// a cycle once when any says so, each step ended (`RunningSteps::end`). The loop
    /// goes on with a round of `WakeCause::StepFinished`. `memories` are those of the
    /// last plan's round (`PlannedRound::memories`). Answers whether any of the steps
    /// changed something a plan is made from (`step_settlement::has_changed_anything`):
    /// what `decide_round` asks. Nothing, and `false`, for no step.
    async fn settle_steps(&self, ended: Vec<(SyncStep, StepOutcome)>, memories: &BTreeMap<LocalItemId, ItemMemory>) -> bool {
        if ended.is_empty() {
            return false;
        }
        let parts = &self.parts;
        let now = (parts.clock)();
        let settled: Vec<(SyncStep, StepSettlement)> = {
            let mut local_step_tries = crate::panic::lock(&self.local_step_tries);
            let count_tries = |step: &SyncStep| match LocalStep::of(step) {
                Some(local_step) => local_step_tries.count_tries(&local_step),
                // TODO: the count is of the last plan's memories: for a step that ran across a plan in which its item was not given it is 0, and the next try is a second away while the store's own count goes on. Check whether the store's answer of the count (`FailedTryStanding::Counted`) is to set the wait, and fix if needed.
                None => memories.get(&step.local_item_id()).and_then(|memory| memory.failed_tries.as_ref()).map_or(0, |failed_tries| failed_tries.count),
            };
            let settled: Vec<(SyncStep, StepSettlement)> = ended.iter().map(|(step, outcome)| (step.clone(), step_settlement::decide_step_settlement(step, outcome, count_tries(step), now))).collect();
            step_settlement::change_local_step_tries(&mut local_step_tries, &settled);
            settled
        };
        for (step, outcome) in &ended {
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
        match parts.store.call(move |store| step_settlement::write_step_settlements(store, &settlements, &folder, logged_since_listing_ended, now)).await {
            Ok(logged) => {
                self.logged_since_listing_ended.fetch_add(logged, Ordering::SeqCst);
            }
            // TODO: when this job fails no try of a step towards OneDrive is counted, and the step is planned again at once. Check whether such steps are to wait, and fix if needed.
            Err(error) => tracing::warn!("cannot write what came of {} steps: {error}", settled.len()),
        }
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
        ended.iter().zip(&settled).any(|((_, outcome), (_, settlement))| step_settlement::has_changed_anything(outcome, settlement))
    }

    /// Publishes what a plan answered: the answers per item (`PublishedPlan::publish`),
    /// the folder's counts (`PlannedRound::item_counts`, of the plan's own reading) and
    /// note on the bus (through `state`), the faults of the plan in the journal, each
    /// once. The task is the one owner of the note (`OutboxStatus::note`): it writes it
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
        parts.published_plan.publish(answers);

        let note = match hold {
            Some(SendingHold::Throttled { until }) => Some(OutboxNote::Throttled(*until)),
            Some(SendingHold::WritesNotAllowed { reason }) => Some(OutboxNote::GateClosed(reason.clone())),
            Some(SendingHold::Stopping | SendingHold::SignedOut | SendingHold::Paused { .. } | SendingHold::FirstListingNotEnded) | None => None,
        };
        let counts = round.item_counts;
        parts.state.update_if_changed(|state| {
            state.cycle.items_listed = counts.in_onedrive;
            state.cycle.items_placed = counts.shown;
            state.cycle.skipped_count = counts.cannot_be_shown;
            state.outbox.note = note;
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

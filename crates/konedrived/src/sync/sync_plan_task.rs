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
//! came, when a time was reached, or when every step of the last plan was started and
//! has ended — never once per finished step. Every step is conditional, so a step of a
//! plan that is a few steps old is safe: its commit finds the rows changed and writes
//! nothing. The `ShowItem` steps of one plan are run as one job
//! (`FolderSteps::run_show_items`), and the outcomes that are ready together are settled
//! in one job of the store. A first listing of N items is then some plans per page and a
//! few jobs, not N of each; the one reader that scans (`list_pinned_files_to_download`)
//! is asked only in a plan that a listing's page or the user's answer woke.
//!
//! **How many steps run at once** (one per item): of the rows, all at once, in the plan's
//! order; towards the folder, as they come — each is one job of the write-operation
//! thread, which runs one at a time; downloads of pinned files, as the pool's download
//! slots allow; towards OneDrive, content as the pool's upload slots allow and the rest
//! one at a time. What holds all sending holds the last two, and a pause and a sign-out
//! the downloads.
//!
//! **What wakes it** (`WakeCause`): a local version written by the mount; a listing's
//! page written, a listing ended; a step finished; the user's answer; a quota read; what
//! holds all sending changed. And the time: the soonest of the plan's waits until a
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
#![allow(dead_code, unused_variables)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::drive::DriveQuota;
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{FolderStep, FullListingCause, ItemMemory, LocalItemId, PlanFault, Store, SyncPlan, SyncStep, WaitReason};
use tokio_util::sync::CancellationToken;

use crate::folder::published_plan::{ItemAnswer, PublishedPlan};
use crate::folder::running_steps::{RunningSteps, StepClass};
use crate::folder::step_outcome::{StepObstacle, StepOutcome, StepRefusal};
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

/// How many upload sessions are cancelled, and how many items uploads left are removed,
/// in one round.
const LEFTOVERS_ASKED_AT_ONCE: usize = 16;

/// How long after the old openings of upload sessions were forgotten they are forgotten
/// again, in seconds: once a day.
const OLD_OPENINGS_FORGOTTEN_EVERY: i64 = 24 * 60 * 60;

/// How long the task waits before it tries again what it could not read: the store for
/// a plan, the quota that was due. Seconds.
const RETRY_AFTER: i64 = 60;

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
    /// (`step_settlement::change_local_step_tries`); and at the user's refresh
    /// (`LocalStepTries::clear_next_tries`).
    local_step_tries: std::sync::Mutex<LocalStepTries>,
    /// The steps that ended and are not settled yet, each with what came of it: put by
    /// the task of a step, taken by the loop.
    ended_steps: std::sync::Mutex<Vec<(SyncStep, StepOutcome)>>,
    /// The faults of the last plan: what a fault is said once by.
    told_faults: std::sync::Mutex<Vec<PlanFault>>,
}

/// What a round does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundDecision {
    /// A plan is made: a cause other than a finished step came, a time was reached, or no
    /// step of the last plan is left to start or running.
    MakePlan,
    /// The last plan is gone on with: steps of it are left to start, and only steps
    /// finished since.
    GoOnWithLastPlan,
    /// Nothing to do until the next cause or time.
    Sleep,
}

/// What the round does, from the causes that woke it (none: a time was reached), how many
/// steps of the last plan are not started, and how many steps run. Pure.
pub fn decide_round(causes: &BTreeSet<WakeCause>, steps_left_to_start: usize, running_step_count: usize) -> RoundDecision {
    let is_only_finished_steps = !causes.is_empty() && causes.iter().all(|cause| *cause == WakeCause::StepFinished);
    match (is_only_finished_steps, steps_left_to_start, running_step_count) {
        (false, _, _) | (true, 0, 0) => RoundDecision::MakePlan,
        (true, 0, _) => RoundDecision::Sleep,
        (true, _, _) => RoundDecision::GoOnWithLastPlan,
    }
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
        Arc::new(SyncPlanTask { parts, logged_since_listing_ended: AtomicUsize::new(0), local_step_tries: std::sync::Mutex::new(LocalStepTries::default()), ended_steps: std::sync::Mutex::default(), told_faults: std::sync::Mutex::default() })
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
            self.settle_steps(self.take_ended_steps(), &memories).await;
            match decide_round(&causes, steps_left_to_start.len(), parts.running_steps.list_steps().len()) {
                RoundDecision::MakePlan => {
                    let now = (parts.clock)();
                    if now.saturating_sub(old_openings_forgotten_at) >= OLD_OPENINGS_FORGOTTEN_EVERY {
                        parts.session_cancellations.forget_old_openings().await;
                        old_openings_forgotten_at = now;
                    }
                    self.apply_wake_causes(&causes).await;
                    match self.make_plan(self.gather_daemon_facts(&causes)).await {
                        Ok(round) => {
                            let now = (parts.clock)();
                            self.publish_round(&round, parts.sending_holds.find(now).as_ref()).await;
                            // A read of the quota that was due and failed is tried again after a wait, not at once.
                            let next_quota_read = parts.onedrive_space.find_next_read().map(|at| if at > now { at } else { now.saturating_add(RETRY_AFTER) });
                            next_wake = decide_next_wake(&round.plan, parts.sending_holds.find_end(now), next_quota_read, now);
                            steps_left_to_start = self.start_steps(round.plan.steps, now);
                            memories = round.memories;
                        }
                        Err(error) => {
                            tracing::warn!("cannot make the sync plan of {}: {error}", parts.folder.display());
                            steps_left_to_start.clear();
                            // The store could not be read: the plan is tried again by itself.
                            next_wake = Some((parts.clock)().saturating_add(RETRY_AFTER));
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

    /// Applies what a wake's causes ask before the plan is made: a quota read, and a read
    /// of the quota that is due (`quota_reader`, `OneDriveSpace::apply_quota`, then
    /// `StepMemory::forget_fitting_space_refusals`);
    /// the user's refresh (`StepMemory::clear_next_tries`, and
    /// `LocalStepTries::clear_next_tries` of the task's own); a listing ended
    /// (`SendingHolds::set_listing_ended`, the count of logged events to 0). And, when
    /// sending is not held and their lists are not empty: `SessionCancellations::
    /// cancel_sessions` and `remove_mismatched_items`.
    async fn apply_wake_causes(&self, causes: &BTreeSet<WakeCause>) {
        let parts = &self.parts;
        let now = (parts.clock)();
        let is_quota_read_due = parts.onedrive_space.find_next_read().is_some_and(|at| at <= now);
        if causes.contains(&WakeCause::QuotaRead) || is_quota_read_due {
            if let Some(quota) = (parts.quota_reader)().await {
                if let SpaceChange::HasSpace { free_space, .. } = parts.onedrive_space.apply_quota(&quota, now) {
                    if let Err(error) = parts.store.call(move |store| store.step_memory().forget_fitting_space_refusals(free_space)).await {
                        tracing::warn!("cannot forget the refusals for lack of space that fit now: {error}");
                    }
                }
            }
        }
        if causes.contains(&WakeCause::UserAnswered) {
            if let Err(error) = parts.store.call(|store| store.step_memory().clear_next_tries()).await {
                tracing::warn!("cannot make the next tries due: {error}");
            }
            crate::panic::lock(&self.local_step_tries).clear_next_tries();
        }
        if causes.contains(&WakeCause::ListingEnded) {
            parts.sending_holds.set_listing_ended();
            self.logged_since_listing_ended.store(0, Ordering::SeqCst);
        }
        if parts.sending_holds.is_held(StepClass::Metadata, (parts.clock)()) {
            return;
        }
        let leftovers = parts.store.read_items(|items| Ok((!items.list_upload_session_cancellations(1)?.is_empty(), !items.list_mismatched_item_removals(1)?.is_empty()))).await;
        match leftovers {
            Ok((has_cancellations, has_removals)) => {
                if has_cancellations {
                    parts.session_cancellations.cancel_sessions(LEFTOVERS_ASKED_AT_ONCE).await;
                }
                if has_removals {
                    parts.session_cancellations.remove_mismatched_items(LEFTOVERS_ASKED_AT_ONCE).await;
                }
            }
            Err(error) => tracing::warn!("cannot read what uploads left to cancel and to remove: {error}"),
        }
    }

    /// What the daemon knows beside the rows, as values, read now: the items with a
    /// running step (`RunningSteps::list_items`), whether OneDrive is full, the time, the
    /// destinations of the running steps, the rows the check at start left, a copy of
    /// the task's tries of the steps towards the folder and of the rows,
    /// and whether this plan asks the reader that scans (a
    /// listing's page or the user's answer is among `causes`, or there is no cause: a
    /// time reached, which the first plan of a run is too). What is open for
    /// writing is NOT gathered here: it is read from the rows of the plan's own moment.
    fn gather_daemon_facts(&self, causes: &BTreeSet<WakeCause>) -> DaemonFacts {
        let parts = &self.parts;
        DaemonFacts {
            items_with_running_step: parts.running_steps.list_items(),
            destinations_of_running_steps: parts.running_steps.list_destinations(),
            is_onedrive_full: parts.onedrive_space.is_full(),
            now: (parts.clock)(),
            left_by_start_up_check: parts.start_check.held_items.clone(),
            // No cause is a time reached, the first plan of a run among them: a pinned file's download waits for such a time.
            reads_pinned_files: causes.is_empty() || causes.contains(&WakeCause::ListingPageWritten) || causes.contains(&WakeCause::UserAnswered),
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
    /// Called by the task of a step, when its runner answered.
    fn hand_over_ended_steps(&self, ended: Vec<(SyncStep, StepOutcome)>) {
        crate::panic::lock(&self.ended_steps).extend(ended);
        self.parts.sync_plan_wake.wake(WakeCause::StepFinished);
    }

    /// Starts the steps of `planned` that can start at `now`
    /// (`step_choice::decide_steps_to_start`): the `ShowItem` steps as one task, every
    /// other as its own. Answers the steps that stay for a later round of this plan.
    fn start_steps(self: &Arc<Self>, planned: Vec<SyncStep>, now: i64) -> Vec<SyncStep> {
        let parts = &self.parts;
        let choice = step_choice::decide_steps_to_start(planned, &parts.running_steps.list_steps(), &self.read_step_limits(), &parts.sending_holds.list_held_classes(now));
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
        let task = Arc::clone(self);
        let running = (self.parts.step_runners.run_step)(step.clone());
        let spawned = tokio::spawn(async move {
            let outcome = running.await;
            task.hand_over_ended_steps(vec![(step, outcome)]);
        });
        self.parts.running_steps.keep_abort_handle(item, spawned.abort_handle());
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
        let task = Arc::clone(self);
        let running = (self.parts.step_runners.run_show_steps)(items);
        let spawned = tokio::spawn(async move {
            // An outcome the runner did not answer is that of a step that did not run.
            let mut outcomes = running.await.into_iter();
            let ended = started.into_iter().map(|step| (step, outcomes.next().unwrap_or(StepOutcome::NotRun(StepObstacle::SendingStopped)))).collect();
            task.hand_over_ended_steps(ended);
        });
        for item in shown_items {
            self.parts.running_steps.keep_abort_handle(item, spawned.abort_handle());
        }
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
    /// last plan's round (`PlannedRound::memories`). Nothing for no step.
    async fn settle_steps(&self, ended: Vec<(SyncStep, StepOutcome)>, memories: &BTreeMap<LocalItemId, ItemMemory>) {
        if ended.is_empty() {
            return;
        }
        let parts = &self.parts;
        let now = (parts.clock)();
        let settled: Vec<(SyncStep, StepSettlement)> = {
            let mut local_step_tries = crate::panic::lock(&self.local_step_tries);
            let count_tries = |step: &SyncStep| match LocalStep::of(step) {
                Some(local_step) => local_step_tries.count_tries(&local_step),
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
    }

    /// Publishes what a plan answered: the answers per item (`PublishedPlan::publish`),
    /// the folder's counts and note on the bus (through `state`, with
    /// `ItemReader::count_items`), the faults of the plan in the journal, each once;
    /// `PlanFault::FullListingNeeded` asks the store for a full listing
    /// (`ItemStore::request_full_listing`) and then the poller for a cycle
    /// (`ListingPollHandle::refresh_after_full_listing_request`).
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
        match parts.store.read_items(|items| items.count_items()).await {
            Ok(counts) => parts.state.update_if_changed(|state| {
                state.cycle.items_listed = counts.in_onedrive;
                state.cycle.items_placed = counts.shown;
                state.cycle.skipped_count = counts.cannot_be_shown;
                state.outbox.note = note;
            }),
            Err(error) => {
                tracing::warn!("cannot count the items of {}: {error}", parts.folder.display());
                parts.state.update_if_changed(|state| state.outbox.note = note);
            }
        }

        // Each fault once: not one the plan before answered too.
        let new_faults: Vec<PlanFault> = {
            let mut told_faults = crate::panic::lock(&self.told_faults);
            let new_faults = plan.faults.iter().filter(|fault| !told_faults.contains(fault)).cloned().collect();
            *told_faults = plan.faults.clone();
            new_faults
        };
        for fault in new_faults {
            tracing::warn!("the sync plan of {} is at fault: {fault:?}", parts.folder.display());
            let PlanFault::FullListingNeeded(item) = fault else { continue };
            match parts.store.call(move |store| store.item_store().request_full_listing(FullListingCause::PlanFault(item))).await {
                Ok(()) => parts.poll.refresh_after_full_listing_request(),
                Err(error) => tracing::warn!("cannot ask for a full listing for item {item}: {error}"),
            }
        }
    }
}

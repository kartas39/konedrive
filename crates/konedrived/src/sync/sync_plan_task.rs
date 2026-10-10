//! The task of a folder's sync plan: the loop that reads what differs, makes the plan,
//! runs its steps and settles what came of each. One per folder, in either access mode,
//! in place of the outbox worker and of the reconcile.
//!
//! **State owned:** the loop, with the steps of the last plan that are not started yet
//! and the count of events logged since the last listing ended; nothing a restart must
//! find again. Everything else has an owner of its own: the steps running
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

use std::collections::BTreeSet;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Store, SyncPlan, SyncStep};
use tokio_util::sync::CancellationToken;

use crate::folder::published_plan::PublishedPlan;
use crate::folder::running_steps::RunningSteps;
use crate::folder::step_outcome::StepOutcome;
use crate::folder::sync_plan_wake::{SyncPlanWake, WakeCause};
use crate::fuse::StartCheckFindings;
use crate::remote::listing_poller::ListingPollHandle;
use crate::status::snapshot::SyncStateHandle;
use crate::upload::onedrive_space::OneDriveSpace;
use crate::upload::onedrive_steps::session_cancellations::SessionCancellations;
use crate::upload::sending_holds::{SendingHold, SendingHolds};

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

use plan_input::{DaemonFacts, PlanSettings, PlannedRound};
use step_runners::StepRunners;
use step_choice::StepLimits;

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
}

/// The plan's task of one folder. It holds its parts and runs the loop; what each part
/// does is the part's.
pub struct SyncPlanTask {
    parts: SyncPlanTaskParts,
    /// How many events of what OneDrive changed were logged since the last listing ended:
    /// what `step_settlement::decide_events_to_log` caps by. Set to 0 at
    /// `WakeCause::ListingEnded`.
    logged_since_listing_ended: AtomicUsize,
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
    todo!()
}

/// When the task wakes by itself next, Unix seconds: the soonest of the plan's waits
/// until a time, the end of what holds all sending when it has one
/// (`SendingHolds::find_end`), and the quota's next read; `None` when nothing is due.
/// Never sooner than a second from `now`. Pure. Today's `Engine::next_due`
/// (`upload/engine.rs:480`).
pub fn decide_next_wake(plan: &SyncPlan, hold_end: Option<i64>, next_quota_read: Option<i64>, now: i64) -> Option<i64> {
    todo!()
}

impl SyncPlanTask {
    /// Made by the folder's start (`sync_tasks::start_order::start_sync_tasks`).
    pub fn new(parts: SyncPlanTaskParts) -> Arc<Self> {
        todo!()
    }

    /// Runs the loop until `stop`, then ends its steps as the module's doc says, and
    /// returns only when no step towards the folder or of the rows runs. Spawned by the
    /// folder's start after the listing's poller; `SyncTasks::stop` cancels `stop` and
    /// awaits this.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        todo!()
    }

    /// Waits for the next round: a cause given, `until` reached, or `stop`. `None` for
    /// the stop. A time reached is an empty set of causes.
    async fn wait_for_round(&self, until: Option<i64>, stop: &CancellationToken) -> Option<BTreeSet<WakeCause>> {
        todo!()
    }

    /// Applies what a wake's causes ask before the plan is made: a quota read
    /// (`OneDriveSpace::apply_quota`, then `StepMemory::forget_fitting_space_refusals`);
    /// the user's refresh (`StepMemory::clear_next_tries`); a listing ended
    /// (`SendingHolds::set_listing_ended`, the count of logged events to 0). And, when
    /// sending is not held and their lists are not empty: `SessionCancellations::
    /// cancel_sessions` and `remove_mismatched_items`.
    async fn apply_wake_causes(&self, causes: &BTreeSet<WakeCause>) {
        todo!()
    }

    /// What the daemon knows beside the rows, as values, read now: the items with a
    /// running step (`RunningSteps::list_items`), whether OneDrive is full, the time, the
    /// destinations of the running steps, the rows the check at start left, the begin of
    /// the window of sent removals, and whether this plan asks the reader that scans (a
    /// listing's page or the user's answer is among `causes`). What is open for
    /// writing is NOT gathered here: it is read from the rows of the plan's own moment.
    fn gather_daemon_facts(&self, causes: &BTreeSet<WakeCause>) -> DaemonFacts {
        todo!()
    }

    /// Makes the plan: `Store::read_items(plan_input::plan_next_steps)` with the facts
    /// gathered, on the store's reading connection.
    async fn make_plan(&self, facts: DaemonFacts) -> Result<PlannedRound, konedrive_tree::TreeError> {
        todo!()
    }

    /// How many steps of each class may run at once: as many uploads and downloads as the
    /// pool has slots for (the map's F19), and one for the metadata. A count only: no
    /// slot is taken by the task.
    fn read_step_limits(&self) -> StepLimits {
        todo!()
    }

    /// Starts `step`, any but a `ShowItem`, as a task of its own: marks every row it
    /// carries as running, with the directory it sends into (`RunningSteps::begin` with
    /// `step_choice::describe_running_step`), spawns it, keeps its abort handle
    /// (`RunningSteps::keep_abort_handle`), runs it through `StepRunners::run_step`, and
    /// hands its outcome to the loop, which settles it.
    fn start_step(self: &Arc<Self>, step: SyncStep) {
        todo!()
    }

    /// Starts the `ShowItem` steps of one plan as ONE task: every item marked running,
    /// `StepRunners::run_show_steps`, each outcome handed to the loop. (Whose a listed
    /// file at a place of this folder's own opening is, is not asked here: nobody calls
    /// `MadeItems::find_left_upload` until the owner said what follows from it.)
    fn start_show_steps(self: &Arc<Self>, steps: Vec<SyncStep>) {
        todo!()
    }

    /// Settles the steps that ended, all that are ready together: for each
    /// `step_settlement::decide_step_settlement`; all written in ONE job of the store
    /// (`step_settlement::write_step_settlements`: the step memory and the activity log);
    /// then what they say of all sending applied (`SendingHolds`), the listing asked for
    /// a cycle once when any says so, each step ended (`RunningSteps::end`). The loop
    /// goes on with a round of `WakeCause::StepFinished`.
    async fn settle_steps(&self, ended: Vec<(SyncStep, StepOutcome)>) {
        todo!()
    }

    /// Publishes what a plan answered: the answers per item (`PublishedPlan::publish`),
    /// the folder's counts and note on the bus (through `state`, with
    /// `ItemReader::count_items`), the faults of the plan in the journal, each once;
    /// `PlanFault::FullListingNeeded` asks the store for a full listing
    /// (`ItemStore::request_full_listing`) and then the poller for a cycle
    /// (`ListingPollHandle::refresh_after_full_listing_request`).
    async fn publish_round(&self, round: &PlannedRound, hold: Option<&SendingHold>) {
        todo!()
    }
}

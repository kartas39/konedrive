//! The steps of the sync plan that run now: one per item.
//!
//! **State owned:** which items have a step running, of which class, how far a content
//! got, whether its commit has begun, and the handle each step's task is aborted by. Written by the plan's task alone
//! ([`RunningSteps::begin`] before a step's task is spawned, [`RunningSteps::end`] when it
//! was settled) and by the upload's progress; read by the plan's input, by the mount (a
//! made file does not take over the binding of a file whose removal is being sent; the
//! sync mark) and by the bus. Nothing of it is stored: after a crash no step runs, and
//! the replay rests on the step memory and on what a step finds in OneDrive. It is here,
//! below the mount, because the mount reads it and must not know the
//! task.
//!
//! A step is of every row it carries the versions of: a directory's removal of the rows
//! below it too, a merge of both rows. Each of them has a running step.
//!
//! **Answers for:** the plan's `items_with_running_step` (one step per item at a time:
//! `docs/design/sync.md` §8.3); `Transfers.Uploads`; the stop of a folder's sync.
//!
//! **Tested without the rest:**
// test: every_row_a_step_carries_has_a_running_step_until_it_ends
// test: a_step_is_not_begun_for_an_item_that_has_one
// test: progress_told_for_an_item_with_no_running_step_is_dropped
// test: the_requests_are_aborted_at_a_stop_and_the_steps_towards_the_folder_are_left_running
// test: a_step_whose_commit_has_begun_is_left_to_a_stop_and_settled
// test: a_stop_aborts_a_request_in_flight_of_every_class_but_the_one_towards_the_folder
#![allow(dead_code, unused_variables)]

use std::collections::{BTreeMap, BTreeSet};

use konedrive_tree::{LocalItemId, OneDriveItemReference};
use tokio::task::AbortHandle;

/// What kind of step it is: what bounds how many run at once, and what a stop does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepClass {
    /// Towards OneDrive, sending a file's content: in the pool's upload slots. Held by
    /// what holds all sending.
    Content,
    /// Towards OneDrive, anything else: one at a time. Held by what holds all sending.
    Metadata,
    /// The download of a pinned file: in the pool's download slots, held by the pause and
    /// the sign-out. Its landing is a job of the write-operation thread.
    Download,
    /// Towards the folder, but a download: each one job of the write-operation thread,
    /// which runs one job at a time; no limit of its own, never held.
    Folder,
    /// Of the rows alone: at once.
    Row,
}

/// A step that runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningStep {
    pub(crate) class: StepClass,
    /// The item the step is of (`SyncStep::local_item_id`).
    pub(crate) item: LocalItemId,
    /// Every other row the step carries the versions of.
    pub(crate) other_items: Vec<LocalItemId>,
    /// The directory of OneDrive a `MakeDirectory`, `SendNewFile` or `SendMove` sends
    /// into: the plan is told the rows of these (`destinations_of_running_steps`).
    pub(crate) destination: Option<OneDriveItemReference>,
    pub(crate) sent: u64,
    pub(crate) total: u64,
}

/// How far a running step got, for what a stop does to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepPhase {
    /// Its request is in flight — or its hash, or its download: nothing of the step is
    /// written yet. A stop aborts its task, unless it is a step towards the folder.
    RequestInFlight,
    /// Its commit has begun (`RunningSteps::begin_commit`): a job of the store's thread
    /// that an abort of the task would neither stop nor hear the answer of. A stop leaves
    /// the step running, and the plan's loop settles it.
    CommitBegun,
}

/// Whether a stop aborts the task of a step of `class` that is in `phase`. Pure. Never a
/// step towards the folder: it is between a disk change and its commit. Never one whose
/// commit has begun.
pub(crate) fn decide_is_aborted_at_stop(class: StepClass, phase: StepPhase) -> bool {
    class != StepClass::Folder && phase == StepPhase::RequestInFlight
}

struct Running {
    step: RunningStep,
    abort: Option<AbortHandle>,
    phase: StepPhase,
}

impl RunningStep {
    /// Every row the step carries the versions of, its own item first.
    fn list_rows(&self) -> impl Iterator<Item = LocalItemId> + '_ {
        std::iter::once(self.item).chain(self.other_items.iter().copied())
    }
}

/// The running steps of one folder.
pub(crate) struct RunningSteps {
    steps: std::sync::Mutex<Steps>,
}

/// The steps by the item each is of, and for every row a step carries, the item of its
/// step.
#[derive(Default)]
struct Steps {
    by_item: BTreeMap<LocalItemId, Running>,
    step_items_by_row: BTreeMap<LocalItemId, LocalItemId>,
}

impl Steps {
    /// Takes the step of `item` out, with every row it carries.
    fn remove(&mut self, item: LocalItemId) -> Option<Running> {
        let running = self.by_item.remove(&item)?;
        for row in running.step.list_rows() {
            self.step_items_by_row.remove(&row);
        }
        Some(running)
    }
}

impl RunningSteps {
    /// No step runs. Made by the folder's start and given to the mount's setup and to the
    /// plan's task.
    pub fn new() -> Self {
        RunningSteps { steps: std::sync::Mutex::default() }
    }

    /// `step` runs from now on; `false`, and nothing begun, when one of its rows has a
    /// running step already. Called by the plan's task before it spawns the step's task.
    pub(crate) fn begin(&self, step: RunningStep) -> bool {
        let mut steps = crate::panic::lock(&self.steps);
        if step.list_rows().any(|row| steps.step_items_by_row.contains_key(&row)) {
            return false;
        }
        for row in step.list_rows() {
            steps.step_items_by_row.insert(row, step.item);
        }
        steps.by_item.insert(step.item, Running { step, abort: None, phase: StepPhase::RequestInFlight });
        true
    }

    /// The task of the step of `item` is aborted by `abort`. Called by the plan's task
    /// right after the spawn.
    pub(crate) fn keep_abort_handle(&self, item: LocalItemId, abort: AbortHandle) {
        if let Some(running) = crate::panic::lock(&self.steps).by_item.get_mut(&item) {
            running.abort = Some(abort);
        }
    }

    /// The step of `item` was settled: none of its rows has a running step any more.
    pub(crate) fn end(&self, item: LocalItemId) {
        crate::panic::lock(&self.steps).remove(item);
    }

    /// Every row a step runs for: the plan's `items_with_running_step`.
    pub(crate) fn list_items(&self) -> BTreeSet<LocalItemId> {
        crate::panic::lock(&self.steps).step_items_by_row.keys().copied().collect()
    }

    /// Whether a step runs for `item`. Called by the mount's `make` and its sync mark.
    pub(crate) fn has_running_step(&self, item: LocalItemId) -> bool {
        crate::panic::lock(&self.steps).step_items_by_row.contains_key(&item)
    }

    /// The directories the running steps send into: for the plan's input.
    pub(crate) fn list_destinations(&self) -> Vec<OneDriveItemReference> {
        let mut destinations = Vec::new();
        for running in crate::panic::lock(&self.steps).by_item.values() {
            if let Some(destination) = running.step.destination.as_ref().filter(|destination| !destinations.contains(*destination)) {
                destinations.push(destination.clone());
            }
        }
        destinations
    }

    /// Every running step. Called for the choice of what to start, for the queue's
    /// entries and for `Transfers.Uploads`.
    pub(crate) fn list_steps(&self) -> Vec<RunningStep> {
        crate::panic::lock(&self.steps).by_item.values().map(|running| running.step.clone()).collect()
    }

    /// The upload or download of `item` got as far as `sent` of `total`. Called by the
    /// upload's progress and the download's.
    pub(crate) fn tell_progress(&self, item: LocalItemId, sent: u64, total: u64) {
        if let Some(running) = crate::panic::lock(&self.steps).by_item.get_mut(&item) {
            (running.step.sent, running.step.total) = (sent, total);
        }
    }

    /// The step of `item` begins its commit: from here on a stop leaves it alone
    /// (`StepPhase::CommitBegun`). `false` when the step runs no more — a stop aborted it
    /// between its request and this call: the caller writes nothing and returns. Called
    /// by the runners of the steps towards OneDrive and of the rows, right before the job
    /// of the store's thread that commits, and by the landing of the plan's download
    /// (`mount::item_download_landing`), which commits as that step; any other step
    /// towards the folder need not say it.
    pub(crate) fn begin_commit(&self, item: LocalItemId) -> bool {
        match crate::panic::lock(&self.steps).by_item.get_mut(&item) {
            Some(running) => {
                running.phase = StepPhase::CommitBegun;
                true
            }
            None => false,
        }
    }

    /// Aborts the task of every step a stop may abort ([`decide_is_aborted_at_stop`]):
    /// one towards the network or of the rows whose request — or hash, or download — is
    /// in flight. Their requests are dropped, and what the step memory holds lets the
    /// next run go on or adopt; their rows have no running step afterwards. The items of
    /// the steps aborted, in their order. A step towards the folder, and one whose commit
    /// has begun, is left running: the loop settles it
    /// ([`count_steps_to_settle`](Self::count_steps_to_settle)). Called by the plan's
    /// loop when its stop comes.
    pub(crate) fn abort_requests(&self) -> Vec<LocalItemId> {
        let aborted: Vec<Running> = {
            let mut steps = crate::panic::lock(&self.steps);
            let items: Vec<LocalItemId> = steps.by_item.values().filter(|running| decide_is_aborted_at_stop(running.step.class, running.phase)).map(|running| running.step.item).collect();
            items.into_iter().filter_map(|item| steps.remove(item)).collect()
        };
        let items = aborted.iter().map(|running| running.step.item).collect();
        for abort in aborted.into_iter().filter_map(|running| running.abort) {
            abort.abort();
        }
        items
    }

    /// How many running steps a stop does not abort: those towards the folder, and those
    /// whose commit has begun. What the plan's loop goes on settling before it returns at
    /// a stop.
    pub(crate) fn count_steps_to_settle(&self) -> usize {
        crate::panic::lock(&self.steps).by_item.values().filter(|running| !decide_is_aborted_at_stop(running.step.class, running.phase)).count()
    }
}

#[cfg(test)]
mod tests;

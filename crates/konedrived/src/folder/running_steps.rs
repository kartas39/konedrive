//! The steps of the sync plan that run now: one per item.
//!
//! **State owned:** which items have a step running, of which class, how far a content
//! got, and the handle each step's task is aborted by. Written by the plan's task alone
//! ([`RunningSteps::begin`] before a step's task is spawned, [`RunningSteps::end`] when it
//! was settled) and by the upload's progress; read by the plan's input, by the mount (a
//! made file does not take over the binding of a file whose removal is being sent; the
//! sync mark) and by the bus. Nothing of it is stored: after a crash no step runs, and
//! the replay rests on the step memory and on what a step finds in OneDrive (the map's
//! F9). It is here, below the mount, because the mount reads it and must not know the
//! task.
//!
//! A step is of every row it carries the versions of: a directory's removal of the rows
//! below it too, a merge of both rows. Each of them has a running step.
//!
//! **Answers for:** the plan's `items_with_running_step` (design §6: "one step per item
//! at a time"); `Transfers.Uploads`; the stop of a folder's sync.
//!
//! **Tested without the rest:**
// test: every_row_a_step_carries_has_a_running_step_until_it_ends
// test: a_step_is_not_begun_for_an_item_that_has_one
// test: progress_told_for_an_item_with_no_running_step_is_dropped
// test: the_requests_are_aborted_at_a_stop_and_the_steps_towards_the_folder_are_left_running
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
    pub class: StepClass,
    /// The item the step is of (`SyncStep::local_item_id`).
    pub item: LocalItemId,
    /// Every other row the step carries the versions of.
    pub other_items: Vec<LocalItemId>,
    /// The directory of OneDrive a `MakeDirectory`, `SendNewFile` or `SendMove` sends
    /// into: the plan is told the rows of these (`destinations_of_running_steps`).
    pub destination: Option<OneDriveItemReference>,
    pub sent: u64,
    pub total: u64,
}

struct Running {
    step: RunningStep,
    abort: Option<AbortHandle>,
}

/// The running steps of one folder.
pub struct RunningSteps {
    by_item: std::sync::Mutex<BTreeMap<LocalItemId, Running>>,
}

impl RunningSteps {
    /// No step runs. Made by the folder's start and given to the mount's setup and to the
    /// plan's task.
    pub fn new() -> Self {
        todo!()
    }

    /// `step` runs from now on; `false`, and nothing begun, when one of its rows has a
    /// running step already. Called by the plan's task before it spawns the step's task.
    pub fn begin(&self, step: RunningStep) -> bool {
        todo!()
    }

    /// The task of the step of `item` is aborted by `abort`. Called by the plan's task
    /// right after the spawn.
    pub fn keep_abort_handle(&self, item: LocalItemId, abort: AbortHandle) {
        todo!()
    }

    /// The step of `item` was settled: none of its rows has a running step any more.
    pub fn end(&self, item: LocalItemId) {
        todo!()
    }

    /// Every row a step runs for: the plan's `items_with_running_step`.
    pub fn list_items(&self) -> BTreeSet<LocalItemId> {
        todo!()
    }

    /// Whether a step runs for `item`. Called by the mount's `make` and its sync mark.
    pub fn has_running_step(&self, item: LocalItemId) -> bool {
        todo!()
    }

    /// The directories the running steps send into: for the plan's input.
    pub fn list_destinations(&self) -> Vec<OneDriveItemReference> {
        todo!()
    }

    /// Every running step. Called for the choice of what to start, for the queue's
    /// entries and for `Transfers.Uploads`.
    pub fn list_steps(&self) -> Vec<RunningStep> {
        todo!()
    }

    /// The upload or download of `item` got as far as `sent` of `total`. Called by the
    /// upload's progress and the download's.
    pub fn tell_progress(&self, item: LocalItemId, sent: u64, total: u64) {
        todo!()
    }

    /// Aborts the tasks of the steps towards the network (`Content`, `Metadata`,
    /// `Download`) and of a `Row` step that has not reached its commit (a hash): their
    /// requests are dropped, and what the step memory holds lets the next run go on or
    /// adopt. Their rows have no running step afterwards. A step towards the folder is
    /// never aborted. Called by the plan's loop when its stop comes.
    pub fn abort_requests(&self) {
        todo!()
    }

    /// How many steps towards the folder still run: what the plan's loop settles before
    /// it returns at a stop.
    pub fn count_folder_steps(&self) -> usize {
        todo!()
    }
}

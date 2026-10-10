//! Which steps of a plan start now.
//!
//! **State owned:** none: two pure functions over the plan's steps, what runs
//! (`RunningSteps`) and what holds all sending (`SendingHolds`).
//!
//! **Tested without the rest**, a test per case of the choice:
// test: a_step_of_the_rows_is_started_at_once
// test: every_step_towards_the_folder_is_started_and_the_show_steps_together
// test: a_download_takes_a_download_slot_and_is_held_by_a_pause_and_not_by_a_throttle
// test: one_metadata_step_towards_onedrive_runs_at_a_time_and_content_as_the_slots_allow
// test: nothing_towards_onedrive_is_started_while_sending_is_held
// test: a_step_one_of_whose_rows_has_a_running_step_is_not_started
// test: the_steps_are_started_in_the_order_the_plan_gave_them
#![allow(dead_code, unused_variables)]

use std::collections::BTreeSet;

use konedrive_tree::SyncStep;

use crate::folder::running_steps::{RunningStep, StepClass};

/// How many steps of each bounded class run at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepLimits {
    /// As many as the account's transfer pool has upload slots: counted here, taken by
    /// the content upload.
    pub content: usize,
    /// One, as today's worker sends what is not content one at a time.
    pub metadata: usize,
    /// As many as the pool has download slots for pinned files: counted here, taken by
    /// the downloader, and by nobody else.
    pub download: usize,
}

/// What starts now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StartChoice {
    /// The `ShowItem` steps, run as one job, a directory before what is in it.
    pub show_steps: Vec<SyncStep>,
    /// Every other step that starts, in the plan's order.
    pub steps: Vec<SyncStep>,
    /// The steps that stay for a later round of this plan.
    pub left: Vec<SyncStep>,
}

/// The class of `step`, and with it every row the step carries beside its own (the rows
/// below a removed directory, the second row of a merge) and the directory it sends into
/// (`MakeDirectory`, `SendNewFile`, `SendMove`), as `RunningSteps` is told it.
/// Pure.
pub fn describe_running_step(step: &SyncStep) -> RunningStep {
    todo!()
}

/// The steps of `planned` to start now, in the plan's order: none one of whose rows has a
/// running step; of each bounded class as many as `limits` leave beside what runs; none
/// of a class in `held_classes` (`SendingHolds::list_held_classes`: every cause that
/// holds, not the one shown alone). Pure.
pub fn decide_steps_to_start(planned: Vec<SyncStep>, running: &[RunningStep], limits: &StepLimits, held_classes: &BTreeSet<StepClass>) -> StartChoice {
    todo!()
}

/// The class of a step, for `decide_steps_to_start` and the published answers. Pure.
pub fn decide_step_class(step: &SyncStep) -> StepClass {
    todo!()
}

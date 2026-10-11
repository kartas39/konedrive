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

use std::collections::BTreeSet;

use konedrive_tree::{FolderStep, LocalItemId, OneDriveStep, RowStep, SyncStep};

use crate::sync_parts::running_steps::{RunningStep, StepClass};

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

/// The name of the case `step` is of, with nothing it carries: what the journal says a
/// step by at `debug`, where no name of an item and no path is written. Pure.
pub fn find_step_name(step: &SyncStep) -> String {
    match step {
        SyncStep::OneDrive(OneDriveStep::MakeDirectory { .. }) => "MakeDirectory".to_owned(),
        SyncStep::OneDrive(OneDriveStep::SendNewFile { .. }) => "SendNewFile".to_owned(),
        SyncStep::OneDrive(OneDriveStep::SendContent { .. }) => "SendContent".to_owned(),
        SyncStep::OneDrive(OneDriveStep::SendMove { .. }) => "SendMove".to_owned(),
        SyncStep::OneDrive(OneDriveStep::SendFileRemoval { .. }) => "SendFileRemoval".to_owned(),
        SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { .. }) => "SendDirectoryRemoval".to_owned(),
        SyncStep::Folder(step) => format!("{:?}", step.kind()),
        SyncStep::Row(step) => format!("{:?}", step.kind()),
    }
}

/// The class of `step`, and with it every row the step carries beside its own (the rows
/// below a removed directory, the second row of a merge) and the directory it sends into
/// (`MakeDirectory`, `SendNewFile`, `SendMove`), as `RunningSteps` is told it.
/// Pure.
pub fn describe_running_step(step: &SyncStep) -> RunningStep {
    let (other_items, destination) = match step {
        SyncStep::OneDrive(OneDriveStep::MakeDirectory { parent, .. } | OneDriveStep::SendNewFile { parent, .. } | OneDriveStep::SendMove { parent, .. }) => (Vec::new(), Some(parent.clone())),
        SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { below, .. }) => (below.iter().map(|row| row.local_item_id).collect(), None),
        SyncStep::Row(RowStep::MergeItems { onedrive, .. }) => (vec![onedrive.local_item_id], None),
        SyncStep::OneDrive(_) | SyncStep::Folder(_) | SyncStep::Row(_) => (Vec::new(), None),
    };
    RunningStep { class: decide_step_class(step), item: step.local_item_id(), other_items, destination, sent: 0, total: 0 }
}

/// The steps of `planned` to start now, in the plan's order: none one of whose rows has a
/// running step; of each bounded class as many as `limits` leave beside what runs; none
/// of a class in `held_classes` (`SendingHolds::list_held_classes`: every cause that
/// holds, not the one shown alone). Pure.
pub fn decide_steps_to_start(planned: Vec<SyncStep>, running: &[RunningStep], limits: &StepLimits, held_classes: &BTreeSet<StepClass>) -> StartChoice {
    let mut rows_with_running_step: BTreeSet<LocalItemId> = running.iter().flat_map(|step| std::iter::once(step.item).chain(step.other_items.iter().copied())).collect();
    let count_running = |class: StepClass| running.iter().filter(|step| step.class == class).count();
    let mut content_count = count_running(StepClass::Content);
    let mut metadata_count = count_running(StepClass::Metadata);
    let mut download_count = count_running(StepClass::Download);
    let mut choice = StartChoice::default();
    for step in planned {
        let described = describe_running_step(&step);
        let rows: Vec<LocalItemId> = std::iter::once(described.item).chain(described.other_items.iter().copied()).collect();
        let slot = match described.class {
            StepClass::Content => Some((&mut content_count, limits.content)),
            StepClass::Metadata => Some((&mut metadata_count, limits.metadata)),
            StepClass::Download => Some((&mut download_count, limits.download)),
            StepClass::Folder | StepClass::Row => None,
        };
        let is_limit_reached = slot.as_ref().is_some_and(|(count, limit)| **count >= *limit);
        if held_classes.contains(&described.class) || is_limit_reached || rows.iter().any(|row| rows_with_running_step.contains(row)) {
            choice.left.push(step);
            continue;
        }
        if let Some((count, _)) = slot {
            *count += 1;
        }
        rows_with_running_step.extend(rows);
        if matches!(step, SyncStep::Folder(FolderStep::ShowItem { .. })) {
            choice.show_steps.push(step);
        } else {
            choice.steps.push(step);
        }
    }
    choice
}

/// The class of a step, for `decide_steps_to_start` and the published answers. Pure.
pub fn decide_step_class(step: &SyncStep) -> StepClass {
    match step {
        SyncStep::OneDrive(OneDriveStep::SendNewFile { .. } | OneDriveStep::SendContent { .. }) => StepClass::Content,
        SyncStep::OneDrive(_) => StepClass::Metadata,
        SyncStep::Folder(FolderStep::DownloadContent { .. }) => StepClass::Download,
        SyncStep::Folder(_) => StepClass::Folder,
        SyncStep::Row(_) => StepClass::Row,
    }
}

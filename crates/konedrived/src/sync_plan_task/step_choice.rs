//! Which steps of a plan start now.
//!
//! **State owned:** none: two pure functions over the plan's steps, what runs
//! (`RunningSteps`) and what holds all sending (`SendingHolds`).
//!
//! **Tested without the rest**, a test per case of the choice, and through the task:
// test: each_step_is_of_its_class
// test: nothing_of_a_held_class_starts_and_every_other_step_does
// test: no_step_starts_for_a_row_that_a_running_step_or_a_step_started_before_it_carries
// test: of_a_bounded_class_as_many_start_as_the_limits_leave_beside_what_runs
// test: the_show_steps_go_apart_and_the_others_keep_the_plans_order
// test: a_few_more_uploads_and_downloads_are_started_than_the_pool_has_slots (`sync_plan_task/tests.rs`)

use std::collections::BTreeSet;

use konedrive_tree::{FolderStep, LocalItemId, OneDriveStep, RowStep, SyncStep};

use crate::sync_parts::running_steps::{RunningStep, StepClass};

/// How many uploads, and how many downloads, are started beyond the slots the account's
/// transfer pool has: they wait at the pool for a slot. The pool grows only while a
/// transfer waits there, and the files still to move are rows of the store, which it
/// never sees: with exactly as many steps started as there are slots it stayed at its
/// start for good. A choice: one waiter is enough for the pool to grow on a success; four
/// are there so that the transfers that end before the plan's next round started their
/// successors each still find one waiting, and a freed slot is taken at once.
pub(crate) const STEPS_BEYOND_SLOTS: usize = 4;

/// How many steps of each bounded class run at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StepLimits {
    /// The pool's slots and [`STEPS_BEYOND_SLOTS`]: counted here; the slot is taken by
    /// the content upload, which waits for it.
    pub(crate) content: usize,
    /// One: what is not content is sent one at a time (`docs/design/sync.md` §8.3).
    pub(crate) metadata: usize,
    /// The pool's slots and [`STEPS_BEYOND_SLOTS`]: counted here; the slot is taken by
    /// the downloader, which waits for it.
    pub(crate) download: usize,
}

/// The limits for a pool of `slot_count` slots. Pure.
pub(crate) fn decide_step_limits(slot_count: usize) -> StepLimits {
    let transfer_count = slot_count + STEPS_BEYOND_SLOTS;
    StepLimits { content: transfer_count, metadata: 1, download: transfer_count }
}

/// What starts now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StartChoice {
    /// The `ShowItem` steps, run as one job, a directory before what is in it.
    pub(crate) show_steps: Vec<SyncStep>,
    /// Every other step that starts, in the plan's order.
    pub(crate) steps: Vec<SyncStep>,
    /// The steps that stay for a later round of this plan.
    pub(crate) left: Vec<SyncStep>,
}

/// The name of the case `step` is of, with nothing it carries: what the journal says a
/// step by at `debug`, where no name of an item and no path is written. Pure.
pub(crate) fn find_step_name(step: &SyncStep) -> String {
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
/// (`MakeDirectory`, `SendNewFile`, `SendMove`), as `RunningSteps` is told it. A download
/// is told with the size of its content as its `total`, and nothing of how far it got.
/// Pure.
pub(crate) fn describe_running_step(step: &SyncStep) -> RunningStep {
    let (other_items, destination) = match step {
        SyncStep::OneDrive(OneDriveStep::MakeDirectory { parent, .. } | OneDriveStep::SendNewFile { parent, .. } | OneDriveStep::SendMove { parent, .. }) => (Vec::new(), Some(parent.clone())),
        SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { below, .. }) => (below.iter().map(|row| row.local_item_id).collect(), None),
        SyncStep::Row(RowStep::MergeItems { onedrive, .. }) => (vec![onedrive.local_item_id], None),
        SyncStep::OneDrive(_) | SyncStep::Folder(_) | SyncStep::Row(_) => (Vec::new(), None),
    };
    RunningStep { class: decide_step_class(step), item: step.local_item_id(), other_items, destination, sent: 0, total: find_download_size(step).unwrap_or(0) }
}

/// The size of the content `step` downloads, when it is the download of a pinned file:
/// what the step takes of the disk (`disk_space`), and the `total` its running step is
/// told with. Pure.
pub(crate) fn find_download_size(step: &SyncStep) -> Option<u64> {
    match step {
        SyncStep::Folder(FolderStep::DownloadContent { item }) => Some(item.local.as_ref().and_then(|local| local.content.as_ref()).map_or(0, konedrive_tree::ContentVersion::size)),
        SyncStep::Folder(_) | SyncStep::OneDrive(_) | SyncStep::Row(_) => None,
    }
}

/// The steps of `planned` to start now, in the plan's order: none one of whose rows has a
/// running step; of each bounded class as many as `limits` leave beside what runs; none
/// of a class in `held_classes` (`SendingHolds::list_held_classes`: every cause that
/// holds, not the one shown alone). Pure.
pub(crate) fn decide_steps_to_start(planned: Vec<SyncStep>, running: &[RunningStep], limits: &StepLimits, held_classes: &BTreeSet<StepClass>) -> StartChoice {
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
pub(crate) fn decide_step_class(step: &SyncStep) -> StepClass {
    match step {
        SyncStep::OneDrive(OneDriveStep::SendNewFile { .. } | OneDriveStep::SendContent { .. }) => StepClass::Content,
        SyncStep::OneDrive(_) => StepClass::Metadata,
        SyncStep::Folder(FolderStep::DownloadContent { .. }) => StepClass::Download,
        SyncStep::Folder(_) => StepClass::Folder,
        SyncStep::Row(_) => StepClass::Row,
    }
}

#[cfg(test)]
mod tests;

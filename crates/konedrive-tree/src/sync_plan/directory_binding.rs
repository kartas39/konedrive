//! The binding of a directory that gives it up: what the two sides agreed is in the
//! directory, and what OneDrive has in it, is then in the row that takes the binding, so
//! the steps of those items wait for the directory's.

use super::item_decision::{ItemAnswers, ItemCase, ItemDecision, Overruling};
use super::sync_step::{RowStep, SyncStep};
use super::WaitReason;
use crate::model::LocalItemId;

/// Lets every step wait whose item the two sides agreed is in a directory that gives up its
/// binding in this plan, or that OneDrive has in it: the commit of that directory's step
/// writes those places into the rows of such items (they follow the binding to the row
/// that takes it), and a step that ran beside it would find its row changed. Those are
/// items the folder does not show in the directory: what it shows there went before, or
/// stays.
pub(crate) fn wait_for_directories_that_give_up_their_binding(cases: &[ItemCase<'_>], answers: &ItemAnswers) -> Vec<Overruling> {
    let gives_up_its_binding = |directory: LocalItemId| matches!(answers.find(directory), Some(ItemDecision::Step(SyncStep::Row(RowStep::GiveUpBinding { .. }))));
    let mut overrulings = Vec::new();
    for case in cases {
        let (item, id) = (case.item, case.item.local_item_id);
        match answers.find(id) {
            Some(ItemDecision::Step(_)) => {}
            // Nothing of the item runs beside the directory's step.
            Some(ItemDecision::Wait(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => continue,
        }
        let directories = [item.synced_place.as_ref(), item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref())].map(|place| place.map(|place| place.parent_local_item_id));
        if let Some(directory) = directories.into_iter().flatten().find(|directory| *directory != id && gives_up_its_binding(*directory)) {
            overrulings.push(Overruling::of(id, ItemDecision::Wait(WaitReason::AnotherItem(directory))));
        }
    }
    overrulings
}

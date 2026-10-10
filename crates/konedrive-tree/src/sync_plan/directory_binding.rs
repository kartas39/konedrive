//! The binding of a directory that moves to another row in this plan: of one that gives it
//! up, to the row that takes it, and of one of OneDrive that is merged with a new directory
//! of the folder, to that directory's row. What the two sides agreed is in the directory,
//! and what OneDrive has in it, is then in the row that has the binding, so the steps of
//! those items wait for the step that moves it.

use std::collections::HashMap;

use super::item_decision::{ItemAnswers, ItemCase, ItemDecision, Overruling};
use super::sync_step::{RowStep, SyncStep};
use super::WaitReason;
use crate::model::LocalItemId;

/// Lets every step wait whose item the two sides agreed is in a directory whose binding
/// moves in this plan, or that OneDrive has in it: the commit of the step that moves the
/// binding writes those places into the rows of such items (they follow the binding to the
/// row that has it then), and a step that ran beside it would find its row changed. The
/// item waits for the item whose step it is: the directory that gives up its binding, or
/// the directory of the folder that is merged with the one of OneDrive. Those are items
/// the folder does not show in the directory: what it shows there went before, or stays.
pub(crate) fn wait_for_directories_that_give_up_their_binding(cases: &[ItemCase<'_>], answers: &ItemAnswers) -> Vec<Overruling> {
    // By the directory whose binding moves: the item whose step moves it.
    let moved_by: HashMap<LocalItemId, LocalItemId> = cases
        .iter()
        .filter_map(|case| match answers.find(case.item.local_item_id) {
            Some(ItemDecision::Step(SyncStep::Row(RowStep::GiveUpBinding { item }))) => Some((item.local_item_id, item.local_item_id)),
            Some(ItemDecision::Step(SyncStep::Row(RowStep::MergeItems { local, onedrive }))) => Some((onedrive.local_item_id, local.local_item_id)),
            Some(ItemDecision::Step(_) | ItemDecision::Wait(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => None,
        })
        .collect();
    let mut overrulings = Vec::new();
    for case in cases {
        let (item, id) = (case.item, case.item.local_item_id);
        match answers.find(id) {
            Some(ItemDecision::Step(_)) => {}
            // Nothing of the item runs beside the directory's step.
            Some(ItemDecision::Wait(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => continue,
        }
        let directories = [item.synced_place.as_ref(), item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref())].map(|place| place.map(|place| place.parent_local_item_id));
        if let Some(waited_for) = directories.into_iter().flatten().filter(|directory| *directory != id).find_map(|directory| moved_by.get(&directory)).filter(|waited_for| **waited_for != id) {
            overrulings.push(Overruling::of(id, ItemDecision::Wait(WaitReason::AnotherItem(*waited_for))));
        }
    }
    overrulings
}

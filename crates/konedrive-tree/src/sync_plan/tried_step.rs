//! Which steps of an item a next try holds ([`TriedStep`]), and the steps towards the folder
//! and of the rows that wait for theirs ([`hold_steps_until_next_try`]). A step towards
//! OneDrive is held where everything that is not sent is decided
//! ([`sending_obstacle`](super::sending_obstacle)). A decision of values the plan was
//! given; it reads no row.

use std::collections::BTreeMap;

use super::item_decision::{ItemAnswers, ItemDecision, Overruling};
use super::sync_step::{FolderStepKind, RowStepKind, SyncStep};
use super::{NextTry, WaitReason};
use crate::model::LocalItemId;

/// The steps of an item a try that failed was a try of: what its next try holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TriedStep {
    /// What is sent of the item: any step towards OneDrive. A request that failed says
    /// that the next one, whatever it sends of the item, waits.
    TowardsOneDrive,
    /// One step towards the folder, by its kind.
    TowardsFolder(FolderStepKind),
    /// One step of the rows alone, by its kind.
    OfRows(RowStepKind),
}

impl TriedStep {
    /// What a try of `step` is a try of.
    pub fn of(step: &SyncStep) -> Self {
        match step {
            SyncStep::OneDrive(_) => TriedStep::TowardsOneDrive,
            SyncStep::Folder(step) => TriedStep::TowardsFolder(step.kind()),
            SyncStep::Row(step) => TriedStep::OfRows(step.kind()),
        }
    }
}

/// Every step towards the folder or of the rows, as the answers stand, whose own next try
/// is not before a time still to come waits until then, with the reason of the failure. A
/// step of another kind for the same item is not held, nor is one whose time has come.
pub(crate) fn hold_steps_until_next_try(next_tries: &BTreeMap<(LocalItemId, TriedStep), NextTry>, now: i64, answers: &ItemAnswers) -> Vec<Overruling> {
    if next_tries.is_empty() {
        return Vec::new();
    }
    let find_wait = |item: LocalItemId| {
        let Some(ItemDecision::Step(step @ (SyncStep::Folder(_) | SyncStep::Row(_)))) = answers.find(item) else { return None };
        let next_try = next_tries.get(&(item, TriedStep::of(step))).filter(|next_try| next_try.not_before > now)?;
        Some(Overruling::of(item, ItemDecision::Wait(WaitReason::Time { until: next_try.not_before, reason: next_try.reason.clone() })))
    };
    answers.items().iter().copied().filter_map(find_wait).collect()
}

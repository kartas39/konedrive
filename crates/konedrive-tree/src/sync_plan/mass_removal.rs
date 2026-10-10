//! The guard against a mass removal: whether so much would be removed from OneDrive at once
//! that the user confirms it first.

use std::collections::BTreeSet;

use konedrive_reason::Reason;

use super::item_decision::{ItemAnswers, ItemDecision, Overruling};
use super::sync_step::{OneDriveStep, SyncStep};
use super::WaitReason;
use crate::model::LocalItemId;

/// More removals than this are a mass removal, whatever the folder shows.
pub(crate) const MASS_REMOVAL_ITEMS: u64 = 500;
/// More than this share of what the folder shows, in percent, is a mass removal...
pub(crate) const MASS_REMOVAL_PERCENT: u64 = 20;
/// ... from this many removals up: one file of a folder of four is none.
pub(crate) const MASS_REMOVAL_LEAST: u64 = 10;

/// Whether `removal_count` items removed from OneDrive at once, out of a folder that shows
/// `shown_item_count`, are a mass removal.
pub(crate) fn is_mass_removal(removal_count: u64, shown_item_count: u64) -> bool {
    removal_count > MASS_REMOVAL_ITEMS || (removal_count >= MASS_REMOVAL_LEAST && removal_count * 100 > shown_item_count * MASS_REMOVAL_PERCENT)
}

/// Holds the removals back when the removals of a short time are together a mass removal.
///
/// `removal_count` is how many items those removals take from OneDrive that the user did
/// not confirm: the unsent ones, those of items the plan was not given too, and those that
/// were sent a short time ago — an `rm -r` arrives as single removals, and a plan made in
/// the middle of it sees a part. The plan cannot count what it was not given, so the caller
/// counts ([`SyncPlanInput`](super::SyncPlanInput)). When that is a mass removal, each
/// removal that is a step of this plan waits for the user instead; the removal of an item
/// the user confirmed is sent.
pub(crate) fn hold_mass_removal(removal_count: u64, shown_item_count: u64, confirmed: &BTreeSet<LocalItemId>, answers: &ItemAnswers) -> Vec<Overruling> {
    if !is_mass_removal(removal_count, shown_item_count) {
        return Vec::new();
    }
    answers
        .items()
        .iter()
        .copied()
        .filter(|item| matches!(answers.find(*item), Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. })))))
        .filter(|item| !confirmed.contains(item))
        .map(|item| Overruling::of(item, ItemDecision::Wait(WaitReason::User(Reason::MassDelete))))
        .collect()
}

#[cfg(test)]
mod tests;

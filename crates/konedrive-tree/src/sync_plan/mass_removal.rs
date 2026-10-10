//! The guard against a mass removal: whether so much would be removed from OneDrive at once
//! that the user confirms it first.

use std::collections::BTreeSet;

use konedrive_reason::Reason;

use super::item_decision::{ItemDecision, ItemDecisions};
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

/// Holds the removals back when the unsent removals are together a mass removal.
///
/// `unsent_removal_count` is how many items the unsent removals take from OneDrive that the
/// user did not confirm, those of items the plan was not given too: the plan cannot count
/// what it was not given, so the caller counts
/// ([`ItemStore::count_unsent_removals`](crate::ItemStore::count_unsent_removals)). When
/// that is a mass removal, each removal that is a step of this plan waits for the user
/// instead; the removal of an item the user confirmed is sent.
pub(crate) fn hold_mass_removal(unsent_removal_count: u64, shown_item_count: u64, confirmed: &BTreeSet<LocalItemId>, decisions: &mut ItemDecisions) {
    if !is_mass_removal(unsent_removal_count, shown_item_count) {
        return;
    }
    let removals: Vec<LocalItemId> = decisions
        .items()
        .iter()
        .copied()
        .filter(|item| matches!(decisions.find(*item), Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. })))))
        .filter(|item| !confirmed.contains(item))
        .collect();
    for item in removals {
        decisions.replace(item, ItemDecision::Wait(WaitReason::User(Reason::MassDelete)));
    }
}

#[cfg(test)]
mod tests;

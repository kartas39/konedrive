//! The guard against a mass removal: whether so much would be removed from OneDrive at once
//! that the user confirms it first.

use std::collections::BTreeSet;

use konedrive_reason::Reason;

use super::item_decision::{ItemCase, ItemDecision, ItemDecisions};
use super::plan_items::PlanItems;
use super::removed_directory::is_in_directory_removed_here;
use super::sync_step::{OneDriveStep, SyncStep};
use super::WaitReason;
use crate::model::{Item, ItemKind, LocalItemId};

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

/// How many items the removal of `item` removes from OneDrive: itself, and for a directory
/// all OneDrive has in it.
fn count_removed_with(items: &PlanItems<'_>, item: &Item) -> u64 {
    1 + if item.kind == ItemKind::Directory { items.list_below_in_onedrive(item.local_item_id).len() as u64 } else { 0 }
}

/// Holds the removals back when together they are a mass removal the user did not confirm.
///
/// Counted is every removal of what was given that the user did not confirm: a removal
/// that is a step of this plan, and one that waits (for the user, for a time, or, a
/// directory's, for what was moved out of it) — a removed directory with all OneDrive has
/// in it. When the count is a mass removal, each of those steps waits for the user instead;
/// what the user confirmed is sent.
pub(crate) fn hold_mass_removal(cases: &[ItemCase<'_>], items: &PlanItems<'_>, shown_item_count: u64, confirmed: &BTreeSet<LocalItemId>, decisions: &mut ItemDecisions) {
    let mut removal_count = 0;
    let mut steps = Vec::new();
    for case in cases {
        let (item, id) = (case.item, case.item.local_item_id);
        let is_removed_here = item.local.is_none() && item.onedrive.is_some() && (item.synced_place.is_some() || item.synced_content.is_some()) && !is_in_directory_removed_here(items, item);
        let is_step = match decisions.find(id) {
            Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. }))) => true,
            Some(ItemDecision::Wait(WaitReason::User(_) | WaitReason::Time { .. })) if is_removed_here => false,
            Some(ItemDecision::Wait(WaitReason::AnotherItem(_))) if is_removed_here && item.kind == ItemKind::Directory => false,
            _ => continue,
        };
        if confirmed.contains(&id) {
            continue;
        }
        removal_count += count_removed_with(items, item);
        if is_step {
            steps.push(id);
        }
    }
    if is_mass_removal(removal_count, shown_item_count) {
        for id in steps {
            decisions.replace(id, ItemDecision::Wait(WaitReason::User(Reason::MassDelete)));
        }
    }
}

#[cfg(test)]
mod tests;

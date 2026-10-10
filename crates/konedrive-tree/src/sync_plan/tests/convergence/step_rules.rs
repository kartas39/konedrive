//! What a step must never be, said here apart from the code that plans it and commits it:
//! the rules a loss follows from when they are broken, asked of every step of every run
//! before it leaves, by the rows as they are then and by what the caller says.

use std::collections::BTreeSet;

use super::drive::OneDriveChanges;
use super::plan_input::CallerFacts;
use crate::model::{FolderVisibility, Item, ItemKind, LocalItemId};
use crate::{ItemVersions, OneDriveStep, RowStep, SyncStep, TreeStore};

/// Why the removal of the directory `directory` must not be sent, or its commit written, as
/// the rows are now; `None` when all the rows say OneDrive has below it was shown here and
/// removed here. `not_settled`: the items a request that is not settled is about, or sends
/// an item into; none for a commit, which asks the rows alone.
pub(super) fn find_what_a_directory_removal_must_not_take(store: &mut TreeStore, directory: LocalItemId, not_settled: &BTreeSet<LocalItemId>) -> Option<String> {
    let rows = store.item_store().list_items().unwrap();
    let directory_of = |row: &Item| row.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).map(|place| place.parent_local_item_id);
    let mut below = vec![directory];
    for _ in 0..rows.len() {
        let deeper: Vec<LocalItemId> = rows.iter().filter(|row| directory_of(row).is_some_and(|above| below.contains(&above)) && !below.contains(&row.local_item_id)).map(|row| row.local_item_id).collect();
        below.extend(deeper);
    }
    if let Some(held) = below.iter().find(|item| not_settled.contains(item)) {
        return Some(format!("item {held}, which a request that is not settled is about or sends an item into"));
    }
    rows.iter().filter(|row| row.local_item_id != directory && below.contains(&row.local_item_id)).find_map(|row| {
        if row.local.is_some() {
            Some(format!("item {}, which the folder shows", row.local_item_id))
        } else if row.synced_place.is_none() && row.synced_content.is_none() {
            Some(format!("item {}, which the folder never showed", row.local_item_id))
        } else {
            None
        }
    })
}

/// What `step`, a step of a plan made now, must not be; `None` when it may leave.
///
/// - The removal of a directory while a listing is under way, or that takes from OneDrive
///   what the folder shows, what it never showed, or what a request not settled is about.
/// - The move of a directory the folder cannot show.
/// - A merge with an item the folder cannot show.
/// - A file that gives up its binding because OneDrive has other content, when that
///   content is the one this client sent itself (`drive`, where OneDrive is a state of its
///   own).
pub(super) fn find_step_that_must_not_leave(store: &mut TreeStore, facts: &CallerFacts, drive: Option<&OneDriveChanges>, step: &SyncStep) -> Option<String> {
    let cannot_be_shown = |store: &mut TreeStore, item: &ItemVersions| item.onedrive.is_some() && store.item_store().find_item(item.local_item_id).unwrap().is_some_and(|row| row.onedrive.is_some()) && store.item_store().find_folder_visibility(item.local_item_id).unwrap() == FolderVisibility::CannotBeShown;
    match step {
        SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { item, .. }) => {
            if store.item_store().find_listing_under_way().unwrap().is_some() {
                return Some(format!("the removal of the directory {} is sent while a listing is under way", item.local_item_id));
            }
            let not_settled: BTreeSet<LocalItemId> = facts.items_with_running_step.union(&facts.destinations_of_running_steps).copied().collect();
            find_what_a_directory_removal_must_not_take(store, item.local_item_id, &not_settled).map(|taken| format!("the removal of the directory {} is sent, and takes with it {taken}", item.local_item_id))
        }
        SyncStep::OneDrive(OneDriveStep::SendMove { item, .. }) => {
            let is_directory = store.item_store().find_item(item.local_item_id).unwrap().is_some_and(|row| row.kind == ItemKind::Directory);
            (is_directory && cannot_be_shown(store, item)).then(|| format!("the move of the directory {} is sent, which the folder cannot show", item.local_item_id))
        }
        SyncStep::Row(RowStep::MergeItems { local, onedrive }) => cannot_be_shown(store, onedrive).then(|| format!("item {} is merged with item {}, which the folder cannot show", local.local_item_id, onedrive.local_item_id)),
        SyncStep::Row(RowStep::GiveUpBinding { item }) => {
            let in_onedrive = item.onedrive.as_ref()?.content.as_ref().map(|content| (content.size(), content.mtime().0))?;
            let sent = drive?.items.get(item.onedrive_item.as_ref()?.item_id.as_str())?.sent_content?;
            (item.local.is_some() && sent == in_onedrive).then(|| format!("item {} gives up its binding because OneDrive has other content, which is the content this client sent itself", item.local_item_id))
        }
        _ => None,
    }
}

//! The stand-in for OneDrive and for the folder: nothing but the commits of the steps
//! ([`ItemStore`](crate::ItemStore), `commit_…`), each given what OneDrive or the disk would
//! plausibly answer. OneDrive has what the rows say it has: a request is answered from them,
//! and its confirmation is written into them.

use std::collections::BTreeMap;
use std::ffi::OsStr;

use crate::item_store::tests::listed_drive::reference;
use crate::model::{ContentVersion, Item, ItemPlace, ItemVersion, LocalItemId, OneDriveItemReference};
use crate::{ConfirmedItem, FoldedName, FolderStep, ItemVersions, OneDriveStep, RowStep, StepCommitOutcome, SyncStep, TreeStore};

/// The tests' upload rule: a name that begins with `~` is ignored.
pub(crate) fn is_name_ignored(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b"~")
}

/// What tells one content from another in these tests, whichever way a row tells it: the
/// hash OneDrive gave, and for a content changed here the hash its bytes would have, which
/// the tests make of its size and its time.
pub(crate) fn hash_of(content: &ContentVersion) -> String {
    match content {
        ContentVersion::Hashed { hash: Some(hash), .. } => hash.clone(),
        ContentVersion::Hashed { size, mtime, .. } | ContentVersion::ChangedHere { size, mtime, .. } => format!("h{size}-{mtime}"),
    }
}

/// What became of a step given to the stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    Written,
    RowsChanged,
    /// OneDrive answered that the name is taken; the stand-in remembers the place.
    NameTaken,
}

pub(crate) struct StepStandIn {
    answer_count: u64,
    /// What the steps remember of a name OneDrive answered is taken.
    pub(crate) places_taken_in_onedrive: BTreeMap<LocalItemId, ItemPlace>,
}

fn local_place(item: &ItemVersions) -> ItemPlace {
    item.local.as_ref().and_then(|local| local.place.clone()).expect("a step that sends an item's place is of an item the folder shows")
}

impl StepStandIn {
    pub(crate) fn new() -> Self {
        Self { answer_count: 0, places_taken_in_onedrive: BTreeMap::new() }
    }

    fn next_answer(&mut self) -> u64 {
        self.answer_count += 1;
        self.answer_count
    }

    /// The content OneDrive answers an upload of `local` with.
    fn confirm_content(&mut self, local: Option<&ItemVersion>) -> Option<ContentVersion> {
        let content = local?.content.as_ref()?;
        let hash = hash_of(content);
        Some(ContentVersion::Hashed { size: content.size(), mtime: content.mtime().0, mtime_nsec: 0, ctag: Some(format!("c-{hash}")), hash: Some(hash) })
    }

    /// Whether OneDrive has another item than `item` under `name` in `parent`, as it
    /// compares names: it then answers a request for that name that the name is taken.
    fn is_name_taken(store: &mut TreeStore, item: LocalItemId, parent: &OneDriveItemReference, name: &OsStr) -> bool {
        // A step that ran for a while may find the directory gone: its commit says so.
        let Some(directory) = store.item_store().find_bound_item(parent).unwrap() else { return false };
        store.item_store().list_children_in_onedrive_with_folded_name(directory.local_item_id, &FoldedName::of(name)).unwrap().iter().any(|holder| holder.local_item_id != item)
    }

    /// OneDrive's answer for the item of `row` as OneDrive has it, with `content` and a new
    /// eTag.
    fn confirm_where_it_is(&mut self, store: &mut TreeStore, row: &Item, content: Option<ContentVersion>) -> Option<ConfirmedItem> {
        let place = row.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone())?;
        let parent = store.item_store().find_item(place.parent_local_item_id).unwrap().and_then(|parent| parent.onedrive_item)?;
        Some(ConfirmedItem { item: row.onedrive_item.clone()?, parent, name: place.name, content, etag: format!("e-{}", self.next_answer()) })
    }

    /// Runs `step` to its end: asks OneDrive or changes the folder, as far as these tests
    /// have either, and commits.
    pub(crate) fn run(&mut self, store: &mut TreeStore, step: &SyncStep) -> StepOutcome {
        let outcome = match step {
            SyncStep::OneDrive(step) => return self.run_onedrive_step(store, step),
            SyncStep::Folder(step) => {
                let mut items = store.item_store();
                match step {
                    FolderStep::ShowItem { item } => items.commit_show_item(item, is_name_ignored(&item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("OneDrive has it").name)),
                    FolderStep::MoveItem { item } => items.commit_move_item(item, is_name_ignored(&item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("OneDrive has it").name)),
                    FolderStep::TakeMoveBack { item } => items.commit_take_move_back(item, is_name_ignored(&item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("OneDrive has it").name)),
                    FolderStep::TakeOneDriveContent { item } => items.commit_take_onedrive_content(item),
                    FolderStep::RemoveItem { item } => items.commit_remove_item(item),
                    FolderStep::DownloadContent { item } => items.commit_download_content(item),
                    FolderStep::RenameToConflictCopy { item, name } => items.commit_rename_to_conflict_copy(item, name, is_name_ignored(name)),
                    FolderStep::MoveToTemporaryName { item, place } => items.commit_move_to_temporary_name(item, place, is_name_ignored(&place.name)),
                }
            }
            SyncStep::Row(step) => {
                let mut items = store.item_store();
                match step {
                    RowStep::GiveUpBinding { item } => items.commit_give_up_binding(item, is_name_ignored(&local_place(item).name)),
                    RowStep::MergeItems { local, onedrive } => items.commit_merge_items(local, onedrive),
                    RowStep::AgreeOnRemoval { item } => items.commit_agree_on_removal(item),
                    RowStep::ForgetSyncedVersion { item } => items.commit_forget_synced_version(item),
                    RowStep::HashLocalContent { item } => items.commit_hash_local_content(item, &hash_of(item.local.as_ref().and_then(|local| local.content.as_ref()).expect("a file the folder shows"))),
                    RowStep::AgreeOnPlace { item } => items.commit_agree_on_place(item),
                    RowStep::AgreeOnContent { item } => items.commit_agree_on_content(item),
                }
            }
        };
        match outcome.unwrap_or_else(|error| panic!("the commit of {step:?} failed: {error}")) {
            StepCommitOutcome::Written => StepOutcome::Written,
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged,
        }
    }

    fn run_onedrive_step(&mut self, store: &mut TreeStore, step: &OneDriveStep) -> StepOutcome {
        let id = step.item().local_item_id;
        if let OneDriveStep::MakeDirectory { item, parent, name } | OneDriveStep::SendNewFile { item, parent, name } | OneDriveStep::SendMove { item, parent, name } = step {
            if Self::is_name_taken(store, id, parent, name) {
                self.places_taken_in_onedrive.insert(id, local_place(item));
                return StepOutcome::NameTaken;
            }
        }
        // The row OneDrive's answer is made from: the one that has the item's binding now,
        // which is another than the step's when a file was saved over it meanwhile. With
        // none, or one OneDrive does not have any more, the request is refused: the rows
        // changed.
        let row_of_binding = |store: &mut TreeStore, item: &ItemVersions| item.onedrive_item.as_ref().and_then(|binding| store.item_store().find_bound_item(binding).unwrap());
        let outcome = match step {
            OneDriveStep::MakeDirectory { item, parent, name } | OneDriveStep::SendNewFile { item, parent, name } => {
                let answer = self.next_answer();
                let made = ConfirmedItem { item: reference(&format!("N{answer}")), parent: parent.clone(), name: name.clone(), content: self.confirm_content(item.local.as_ref()), etag: format!("e-{answer}") };
                if made.content.is_some() {
                    store.item_store().commit_send_new_file(item, parent, name, &made, None)
                } else {
                    store.item_store().commit_make_directory(item, parent, name, &made)
                }
            }
            OneDriveStep::SendContent { item } => {
                let content = self.confirm_content(item.local.as_ref());
                let Some(sent) = row_of_binding(store, item).and_then(|row| self.confirm_where_it_is(store, &row, content)) else { return StepOutcome::RowsChanged };
                store.item_store().commit_send_content(item, &sent, None)
            }
            OneDriveStep::SendMove { item, parent, name } => {
                let Some(row) = row_of_binding(store, item) else { return StepOutcome::RowsChanged };
                let content = row.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone());
                let Some(moved) = self.confirm_where_it_is(store, &row, content) else { return StepOutcome::RowsChanged };
                let moved = ConfirmedItem { parent: parent.clone(), name: name.clone(), ..moved };
                store.item_store().commit_send_move(item, parent, name, &moved)
            }
            OneDriveStep::SendFileRemoval { item } => store.item_store().commit_send_file_removal(item),
            OneDriveStep::SendDirectoryRemoval { item, below } => store.item_store().commit_send_directory_removal(item, below),
        };
        match outcome.unwrap_or_else(|error| panic!("the commit of {step:?} failed: {error}")) {
            StepCommitOutcome::Written => StepOutcome::Written,
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged,
        }
    }
}

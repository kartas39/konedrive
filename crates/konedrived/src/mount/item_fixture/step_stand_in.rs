//! The stand-in for the runners of steps: each step is one job of the store's thread that
//! answers the request as OneDrive would from what the rows say OneDrive has, and commits
//! by the step's own commit. Nothing is asked of Graph and nothing is changed on disk: a
//! `DownloadContent` it ran leaves a row that says on disk over no file.
//!
//! It is the daemon's own writing of what `konedrive-tree`'s convergence test runs its
//! plans through (`sync_plan/tests/convergence/step_stand_in.rs`, test code of that
//! crate): the answers are made the same way. It calls nothing of `RunningSteps`: the
//! plan's task begins and ends its steps, and a stand-in step is never aborted between
//! its request and its commit, since it has no request.

use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use konedrive_tree::{
    ConfirmedItem, ContentVersion, FoldedName, FolderStep, Item, ItemPlace, ItemVersion, ItemVersions, LocalItemId, OneDriveItemId, OneDriveItemReference, OneDriveStep, RowStep, StepCommitOutcome, Store, SyncStep,
    TreeError, TreeStore,
};

use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepFailure, StepOutcome, StepRefusal};
use crate::mount::upload_rules::UploadRules;
use crate::sync_plan_task::step_runners::StepRunners;

/// What tells one content from another in the stand-in's answers: the hash OneDrive gave,
/// and for a content changed here the hash its bytes are taken to have, made of its size
/// and its time.
pub(crate) fn make_stand_in_hash(content: &ContentVersion) -> String {
    match content {
        ContentVersion::Hashed { hash: Some(hash), .. } => hash.clone(),
        ContentVersion::Hashed { size, mtime, .. } | ContentVersion::ChangedHere { size, mtime, .. } => format!("h{size}-{mtime}"),
    }
}

/// What one step came to, inside the store's job.
enum StandInAnswer {
    Committed(StepCommitOutcome),
    /// OneDrive has another item under the name in the directory the step sends into.
    NameTaken { place: ItemPlace, holder_name: OsString },
    /// The item the request is about has no row with its binding, or OneDrive does not
    /// have it any more: OneDrive refuses, and nothing is written.
    ItemGone,
}

fn local_place(item: &ItemVersions) -> ItemPlace {
    item.local.as_ref().and_then(|local| local.place.clone()).expect("a step that sends an item's place is of an item the folder shows")
}

fn onedrive_name(item: &ItemVersions) -> OsString {
    item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("a step towards the folder is of an item OneDrive has").name
}

/// The stand-in of one fixture.
pub(super) struct StepStandIn {
    store: Store,
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    upload_rules: Arc<UploadRules>,
    /// How many answers OneDrive gave: what a new id and a fresh eTag are made of.
    answer_count: Arc<AtomicU64>,
}

/// What a job of the store's thread is given of the stand-in.
struct Answers {
    upload_rules: Arc<UploadRules>,
    answer_count: Arc<AtomicU64>,
}

impl Answers {
    fn next_answer(&self) -> u64 {
        self.answer_count.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn is_name_ignored(&self, name: &OsStr) -> bool {
        self.upload_rules.is_name_ignored(name)
    }

    /// The content OneDrive answers an upload of `local` with.
    fn confirm_content(local: Option<&ItemVersion>) -> Option<ContentVersion> {
        let content = local?.content.as_ref()?;
        let hash = make_stand_in_hash(content);
        Some(ContentVersion::Hashed { size: content.size(), mtime: content.mtime().0, mtime_nsec: 0, ctag: Some(format!("c-{hash}")), hash: Some(hash) })
    }

    /// The name of another item than `item` that OneDrive has under `name` in `parent`, as
    /// it compares names.
    fn find_name_holder(store: &mut TreeStore, item: LocalItemId, parent: &OneDriveItemReference, name: &OsStr) -> Result<Option<OsString>, TreeError> {
        // A step that ran for a while may find the directory gone: its commit says so.
        let Some(directory) = store.item_store().find_bound_item(parent)? else { return Ok(None) };
        let holders = store.item_store().list_children_in_onedrive_with_folded_name(directory.local_item_id, &FoldedName::of(name))?;
        Ok(holders.into_iter().find(|holder| holder.local_item_id != item).and_then(|holder| holder.onedrive.and_then(|onedrive| onedrive.place).map(|place| place.name)))
    }

    /// OneDrive's answer for the item of `row` where OneDrive has it, with `content` and a
    /// new eTag.
    fn confirm_where_it_is(&self, store: &mut TreeStore, row: &Item, content: Option<ContentVersion>) -> Result<Option<ConfirmedItem>, TreeError> {
        let Some(place) = row.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()) else { return Ok(None) };
        let Some(parent) = store.item_store().find_item(place.parent_local_item_id)?.and_then(|parent| parent.onedrive_item) else { return Ok(None) };
        let Some(item) = row.onedrive_item.clone() else { return Ok(None) };
        Ok(Some(ConfirmedItem { item, parent, name: place.name, content, etag: format!("e-stand-in-{}", self.next_answer()) }))
    }

    /// The row that has the binding of `item` now: another than the step's when a file was
    /// saved over it meanwhile.
    fn find_row_of_binding(store: &mut TreeStore, item: &ItemVersions) -> Result<Option<Item>, TreeError> {
        match &item.onedrive_item {
            Some(binding) => store.item_store().find_bound_item(binding),
            None => Ok(None),
        }
    }

    fn run(&self, store: &mut TreeStore, step: &SyncStep) -> Result<StandInAnswer, TreeError> {
        let committed = match step {
            SyncStep::OneDrive(step) => return self.run_onedrive_step(store, step),
            SyncStep::Folder(step) => {
                let mut items = store.item_store();
                match step {
                    FolderStep::ShowItem { item } => items.commit_show_item(item, self.is_name_ignored(&onedrive_name(item))),
                    FolderStep::MoveItem { item } => items.commit_move_item(item, self.is_name_ignored(&onedrive_name(item)), None),
                    FolderStep::TakeMoveBack { item } => items.commit_take_move_back(item, self.is_name_ignored(&onedrive_name(item)), None),
                    FolderStep::TakeOneDriveContent { item } => items.commit_take_onedrive_content(item, None),
                    FolderStep::RemoveItem { item } => items.commit_remove_item(item, None),
                    FolderStep::DownloadContent { item } => items.commit_download_content(item, None),
                    FolderStep::RenameToConflictCopy { item, name } => items.commit_rename_to_conflict_copy(item, name, self.is_name_ignored(name)),
                    FolderStep::MoveToTemporaryName { item, place } => items.commit_move_to_temporary_name(item, place, self.is_name_ignored(&place.name), None),
                }
            }
            SyncStep::Row(step) => {
                let mut items = store.item_store();
                match step {
                    RowStep::GiveUpBinding { item } => items.commit_give_up_binding(item, self.is_name_ignored(&local_place(item).name)),
                    RowStep::MergeItems { local, onedrive } => items.commit_merge_items(local, onedrive),
                    RowStep::AgreeOnRemoval { item } => items.commit_agree_on_removal(item),
                    RowStep::ForgetSyncedVersion { item } => items.commit_forget_synced_version(item),
                    RowStep::HashLocalContent { item } => items.commit_hash_local_content(item, &make_stand_in_hash(item.local.as_ref().and_then(|local| local.content.as_ref()).expect("a file the folder shows"))),
                    RowStep::AgreeOnPlace { item } => items.commit_agree_on_place(item),
                    RowStep::AgreeOnContent { item } => items.commit_agree_on_content(item),
                }
            }
        };
        Ok(StandInAnswer::Committed(committed?))
    }

    fn run_onedrive_step(&self, store: &mut TreeStore, step: &OneDriveStep) -> Result<StandInAnswer, TreeError> {
        let id = step.item().local_item_id;
        if let OneDriveStep::MakeDirectory { item, parent, name } | OneDriveStep::SendNewFile { item, parent, name } | OneDriveStep::SendMove { item, parent, name } = step {
            if let Some(holder_name) = Self::find_name_holder(store, id, parent, name)? {
                return Ok(StandInAnswer::NameTaken { place: local_place(item), holder_name });
            }
        }
        let committed = match step {
            OneDriveStep::MakeDirectory { item, parent, name } | OneDriveStep::SendNewFile { item, parent, name } => {
                let answer = self.next_answer();
                let made_item = OneDriveItemReference { drive_id: parent.drive_id.clone(), item_id: OneDriveItemId::new(format!("stand-in-{answer}")) };
                let made = ConfirmedItem { item: made_item, parent: parent.clone(), name: name.clone(), content: Self::confirm_content(item.local.as_ref()), etag: format!("e-stand-in-{answer}") };
                match made.content {
                    Some(_) => store.item_store().commit_send_new_file(item, parent, name, &made, None)?,
                    None => store.item_store().commit_make_directory(item, parent, name, &made)?,
                }
            }
            OneDriveStep::SendContent { item } => {
                let content = Self::confirm_content(item.local.as_ref());
                let Some(row) = Self::find_row_of_binding(store, item)? else { return Ok(StandInAnswer::ItemGone) };
                let Some(sent) = self.confirm_where_it_is(store, &row, content)? else { return Ok(StandInAnswer::ItemGone) };
                store.item_store().commit_send_content(item, &sent, None)?
            }
            OneDriveStep::SendMove { item, parent, name } => {
                let Some(row) = Self::find_row_of_binding(store, item)? else { return Ok(StandInAnswer::ItemGone) };
                let content = row.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone());
                let Some(moved) = self.confirm_where_it_is(store, &row, content)? else { return Ok(StandInAnswer::ItemGone) };
                let moved = ConfirmedItem { parent: parent.clone(), name: name.clone(), ..moved };
                store.item_store().commit_send_move(item, parent, name, &moved)?
            }
            OneDriveStep::SendFileRemoval { item } => store.item_store().commit_send_file_removal(item, 0)?,
            OneDriveStep::SendDirectoryRemoval { item, below } => store.item_store().commit_send_directory_removal(item, below, 0)?,
        };
        Ok(StandInAnswer::Committed(committed))
    }
}

fn to_step_outcome(committed: StepCommitOutcome) -> StepOutcome {
    match committed {
        StepCommitOutcome::Written => StepOutcome::Written { events: Vec::new() },
        StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing),
    }
}

fn to_store_failure(error: &TreeError) -> StepOutcome {
    StepOutcome::Failed(StepFailure::Store { detail: error.to_string() })
}

impl StepStandIn {
    pub(super) fn new(store: Store, tree_lock: Arc<tokio::sync::Mutex<()>>, upload_rules: Arc<UploadRules>) -> Self {
        StepStandIn { store, tree_lock, upload_rules, answer_count: Arc::new(AtomicU64::new(0)) }
    }

    fn answers(&self) -> Answers {
        Answers { upload_rules: Arc::clone(&self.upload_rules), answer_count: Arc::clone(&self.answer_count) }
    }

    /// Runs `step` to its end, under the tree lock as a step's commit is: `Written` with no
    /// events, `RowsChanged(Nothing)`, `Refused(NameTaken)` where OneDrive has another item
    /// under the name, `Refused(ItemGone)` where it does not have the item, and
    /// `Failed(Store)` for an error of the commit.
    pub(super) async fn run_step(&self, step: SyncStep) -> StepOutcome {
        let answers = self.answers();
        let _tree = self.tree_lock.lock().await;
        match self.store.call(move |store| answers.run(store, &step)).await {
            Ok(StandInAnswer::Committed(committed)) => to_step_outcome(committed),
            Ok(StandInAnswer::NameTaken { place, holder_name }) => StepOutcome::Refused(StepRefusal::NameTaken { place, holder_name }),
            Ok(StandInAnswer::ItemGone) => StepOutcome::Refused(StepRefusal::ItemGone),
            Err(error) => to_store_failure(&error),
        }
    }

    /// Commits the `ShowItem` steps of `items` together (`ItemStore::commit_show_items`),
    /// each answered by itself, in the order given.
    pub(super) async fn run_show_steps(&self, items: Vec<ItemVersions>) -> Vec<StepOutcome> {
        let answers = self.answers();
        let count = items.len();
        let _tree = self.tree_lock.lock().await;
        let committed = self.store.call(move |store| {
            let shown: Vec<(ItemVersions, bool)> = items
                .into_iter()
                .map(|item| {
                    let is_name_ignored = answers.is_name_ignored(&onedrive_name(&item));
                    (item, is_name_ignored)
                })
                .collect();
            store.item_store().commit_show_items(&shown)
        });
        match committed.await {
            Ok(committed) => committed.into_iter().map(to_step_outcome).collect(),
            Err(error) => vec![to_store_failure(&error); count],
        }
    }

    /// The stand-in as the plan's task takes its runners.
    pub(super) fn into_step_runners(self) -> StepRunners {
        let for_steps = Arc::new(self);
        let for_show_steps = Arc::clone(&for_steps);
        StepRunners {
            run_step: Arc::new(move |step| {
                let stand_in = Arc::clone(&for_steps);
                Box::pin(async move { stand_in.run_step(step).await })
            }),
            run_show_steps: Arc::new(move |items| {
                let stand_in = Arc::clone(&for_show_steps);
                Box::pin(async move { stand_in.run_show_steps(items).await })
            }),
        }
    }
}

//! The runner of the sync plan's steps of the rows alone: one function per case of
//! `RowStep`, each its commit and nothing else — but for the hash of a local content,
//! which reads the file, and the cases that leave the marks on disk in step with the row.
//!
//! **State owned:** none.
//!
//! **Every step:** says its commit begins (`RunningSteps::begin_commit`: a stop aborts a
//! hash in flight, never a commit; told that the step runs no more, it writes nothing),
//! takes the folder's tree lock for its commit and lets it go (the wait for it is not
//! raced against the stop: the lock is held for one job at a time — a write operation of
//! the mount, a commit of another step — by nobody who waits for the stop); what it
//! reads or writes on disk — a file hashed, marks written or taken off — it does with NO
//! lock: a hash of a file of any size holds up no write operation, and the marks go
//! through `ItemContentFiles`, whose opens cannot be crossed by a rename. `RowsChanged`
//! is answered as it is: nothing was written and nothing is to undo.
//!
//! **Tested without the rest**, over the fixture (`mount::item_fixture`):
// test: each_case_calls_its_commit_and_answers_written
// test: a_step_planned_from_other_versions_answers_that_the_rows_changed
// test: the_hash_written_is_that_of_the_bytes_on_disk
// test: a_file_changed_while_it_was_hashed_writes_no_hash
// test: the_tree_lock_is_not_held_while_a_file_is_hashed
// test: an_item_that_gave_its_binding_up_has_no_onedrive_marks_on_disk
// test: a_content_agreed_on_is_marked_as_the_whole_copy_with_its_ctag

use std::sync::Arc;

use konedrive_tree::{ItemStore, ItemVersions, LocalItemId, RowStep, StepCommitOutcome, Store, TreeError};

use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepFailure, StepObstacle, StepOutcome, StepTrouble};
use crate::mount::ItemContentFiles;
use crate::local::SharedIgnore;

/// The runner of the steps of the rows, for one folder.
pub(crate) struct RowSteps {
    store: Store,
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    content_files: Arc<ItemContentFiles>,
    /// The account's list of ignored names: `GiveUpBinding`'s commit asks whether the
    /// item's name is ignored, now that nothing of OneDrive's keeps it from being left
    /// here.
    ignore: SharedIgnore,
    /// Told when a step's commit begins: from then on a stop does not abort the step.
    running_steps: Arc<RunningSteps>,
}

impl RowSteps {
    /// Made by the folder's start (`sync_tasks::start_order::start_sync_tasks`).
    pub fn new(store: Store, tree_lock: Arc<tokio::sync::Mutex<()>>, content_files: Arc<ItemContentFiles>, ignore: SharedIgnore, running_steps: Arc<RunningSteps>) -> Self {
        Self { store, tree_lock, content_files, ignore, running_steps }
    }

    /// The commit of the step of `item`, under the tree lock, as one job of the store's
    /// thread: said to begin first (`RunningSteps::begin_commit`). `Err` with the step's
    /// outcome when nothing was written: the step runs no more, the rows changed, or the
    /// store failed.
    async fn commit(&self, item: LocalItemId, commit: impl FnOnce(&mut ItemStore<'_>) -> Result<StepCommitOutcome, TreeError> + Send + 'static) -> Result<(), StepOutcome> {
        if !self.running_steps.begin_commit(item) {
            return Err(StepOutcome::NotRun(StepObstacle::SendingStopped));
        }
        let committed = {
            let _tree_lock = self.tree_lock.lock().await;
            self.store.call(move |store| commit(&mut store.item_store())).await
        };
        match committed {
            Ok(StepCommitOutcome::Written) => Ok(()),
            Ok(StepCommitOutcome::RowsChanged) => Err(StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing)),
            Err(error) => Err(StepOutcome::Failed(StepFailure::Store { detail: error.to_string() })),
        }
    }

    /// A step that is its commit and nothing else: [`commit`](Self::commit), answered as
    /// the step's outcome.
    async fn run_commit_alone(&self, item: LocalItemId, commit: impl FnOnce(&mut ItemStore<'_>) -> Result<StepCommitOutcome, TreeError> + Send + 'static) -> StepOutcome {
        match self.commit(item, commit).await {
            Ok(()) => StepOutcome::Written { events: Vec::new() },
            Err(outcome) => outcome,
        }
    }

    /// Changes the marks of what `item` has on disk, off the runtime and with no lock. The
    /// row is written by then: a mark that could not be changed is said in the journal,
    /// and the step is done all the same.
    async fn change_marks(&self, item: LocalItemId, change: impl FnOnce(LocalItemId) -> std::io::Result<()> + Send + 'static) {
        // A mark that fails here, or a stop between the commit and the mark, is not tried again by a plan: the step is written. Nothing is lost or sent twice for it, as the rows decide and a mark is read in three places only (`backing_directory_check`, the download's landing). A copy agreed on that kept the cTag of the copy the change began from reads as changed at the next check at start, is hashed, agrees again, and is marked then (`AgreeOnContent`). One left with no mark, or the id alone, is the row's entry to the check whatever its marks (a row whose content is on disk is compared by size and time), which writes the id; no download lands on a row whose content is on disk. Marks left on an item that gave its binding up stay until its sending writes its own: the check reads no mark of an entry whose row has no binding, and after a rebuilt store such an entry is bound by its mark as a guess, which the plan gives up or agrees by the hash.
        match tokio::task::spawn_blocking(move || change(item)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("cannot change the marks of item {item} on disk: {error}"),
            Err(error) => tracing::warn!("the marks of item {item} on disk were not changed: {error}"),
        }
    }

    /// What `item` has on disk is marked as the whole copy of what the two sides agree on
    /// (`ItemContentFiles::mark_agreed_content`).
    async fn mark_agreed_content(&self, item: LocalItemId) {
        let content_files = Arc::clone(&self.content_files);
        self.change_marks(item, move |item| content_files.mark_agreed_content(item)).await;
    }

    /// Runs `step` and answers what came of it. Called by the plan's task, at once and
    /// in the plan's order: a step of the rows waits for no slot.
    pub(crate) async fn run(&self, step: RowStep) -> StepOutcome {
        match step {
            RowStep::GiveUpBinding { item } => self.run_give_up_binding(&item).await,
            RowStep::MergeItems { local, onedrive } => self.run_merge_items(&local, &onedrive).await,
            RowStep::AgreeOnRemoval { item } => self.run_agree_on_removal(&item).await,
            RowStep::ForgetSyncedVersion { item } => self.run_forget_synced_version(&item).await,
            RowStep::HashLocalContent { item } => self.run_hash_local_content(&item).await,
            RowStep::AgreeOnPlace { item } => self.run_agree_on_place(&item).await,
            RowStep::AgreeOnContent { item } => self.run_agree_on_content(&item).await,
        }
    }

    /// `GiveUpBinding`: `commit_give_up_binding(item, is_name_ignored)` under the tree
    /// lock; then the OneDrive marks taken off what the item has on disk
    /// (`ItemContentFiles::remove_onedrive_marks`). The upload session the item had
    /// is abandoned by the store, with the binding.
    async fn run_give_up_binding(&self, item: &ItemVersions) -> StepOutcome {
        let name = item.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.name.clone());
        let is_name_ignored = name.is_some_and(|name| crate::panic::read(&self.ignore).matches(&name));
        let versions = item.clone();
        if let Err(outcome) = self.commit(item.local_item_id, move |items| items.commit_give_up_binding(&versions, is_name_ignored)).await {
            return outcome;
        }
        let content_files = Arc::clone(&self.content_files);
        self.change_marks(item.local_item_id, move |item| content_files.remove_onedrive_marks(item)).await;
        StepOutcome::Written { events: Vec::new() }
    }

    /// `MergeItems`: `commit_merge_items(local, onedrive)`; then what the item has on disk
    /// marked as the whole copy of what the two sides agree on
    /// (`ItemContentFiles::mark_agreed_content`; `docs/design/sync.md` §4).
    async fn run_merge_items(&self, local: &ItemVersions, onedrive: &ItemVersions) -> StepOutcome {
        let (local_versions, onedrive_versions) = (local.clone(), onedrive.clone());
        if let Err(outcome) = self.commit(local.local_item_id, move |items| items.commit_merge_items(&local_versions, &onedrive_versions)).await {
            return outcome;
        }
        self.mark_agreed_content(local.local_item_id).await;
        StepOutcome::Written { events: Vec::new() }
    }

    /// `AgreeOnRemoval`: `commit_agree_on_removal(item)`. Both sides removed the item.
    async fn run_agree_on_removal(&self, item: &ItemVersions) -> StepOutcome {
        let versions = item.clone();
        self.run_commit_alone(item.local_item_id, move |items| items.commit_agree_on_removal(&versions)).await
    }

    /// `ForgetSyncedVersion`: `commit_forget_synced_version(item)`.
    async fn run_forget_synced_version(&self, item: &ItemVersions) -> StepOutcome {
        let versions = item.clone();
        self.run_commit_alone(item.local_item_id, move |items| items.commit_forget_synced_version(&versions)).await
    }

    /// `HashLocalContent`: with no lock, opens the content the step was planned from
    /// (`ItemContentFiles::open_to_send`) and hashes it off the runtime
    /// (`LocalContentFile::hash`); then, under the tree lock,
    /// `commit_hash_local_content(item, hash)`: the plan made after it can ask whether the
    /// two sides have the same content (`docs/design/sync.md` §6.3). The commit writes the content as
    /// hashed; the check at start tells a clean copy by the local content against the
    /// synced one, not by that.
    async fn run_hash_local_content(&self, item: &ItemVersions) -> StepOutcome {
        let (content_files, versions) = (Arc::clone(&self.content_files), item.clone());
        let hash = match tokio::task::spawn_blocking(move || content_files.open_to_send(&versions)?.hash()).await {
            Ok(Ok(hash)) => hash,
            Ok(Err(StepTrouble::Obstacle(obstacle))) => return StepOutcome::NotRun(obstacle),
            Ok(Err(StepTrouble::Failure(failure))) => return StepOutcome::Failed(failure),
            // The hash panicked: a failure, which counts a try, so that the step waits.
            Err(error) if error.is_panic() => return StepOutcome::Failed(StepFailure::Store { detail: format!("the hash of item {} ended by a panic: {error}", item.local_item_id) }),
            // The hash was aborted with the runtime: nothing was written.
            Err(_) => return StepOutcome::NotRun(StepObstacle::SendingStopped),
        };
        let versions = item.clone();
        self.run_commit_alone(item.local_item_id, move |items| items.commit_hash_local_content(&versions, &hash)).await
    }

    /// `AgreeOnPlace`: `commit_agree_on_place(item)`.
    async fn run_agree_on_place(&self, item: &ItemVersions) -> StepOutcome {
        let versions = item.clone();
        self.run_commit_alone(item.local_item_id, move |items| items.commit_agree_on_place(&versions)).await
    }

    /// `AgreeOnContent`: `commit_agree_on_content(item)`; then the copy on disk marked as
    /// the whole copy with the cTag it agrees with
    /// (`ItemContentFiles::mark_agreed_content`): an id-only mark at a row that became
    /// equal to the synced content would be read as "holds nothing" by the next check.
    async fn run_agree_on_content(&self, item: &ItemVersions) -> StepOutcome {
        let versions = item.clone();
        if let Err(outcome) = self.commit(item.local_item_id, move |items| items.commit_agree_on_content(&versions)).await {
            return outcome;
        }
        self.mark_agreed_content(item.local_item_id).await;
        StepOutcome::Written { events: Vec::new() }
    }
}

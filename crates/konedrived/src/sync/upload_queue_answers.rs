//! What the bus says of what waits, what is held and what was left out
//! (`org.konedrive.UploadQueue`, `Transfers.Uploads`, the folder's `Skipped()`): pure
//! functions of the three sources of design §7 — the plan's answer for each unsent item,
//! what the steps remember, and the state of the sending as a whole.
//!
//! **State owned:** none. The functions below the line "thin" read the sources — the
//! rows and the step memory in ONE read closure of the store's reading connection
//! (`Store::read_items`), `PublishedPlan`, `RunningSteps`, `SendingHolds`,
//! `OneDriveSpace` — and call the pure ones; nothing here keeps a sum.
//!
//! **Answers for:** the map's MISSING-6.a, 6.c, 6.d and the user's two answers to a held
//! mass removal (1.k, 2.l). The members of the bus, their XML, their row types
//! (`konedrive_dbus::rows`) and every word they carry stay today's.
//!
//! **The counts** are over every unsent item, the plan's answers over the items the last
//! plan was given: `PendingCount`/`Bytes` come from one sum of the store
//! (`count_unsent`); what is blocked or too big by the step memory from its own counts
//! (`count_refusal_reasons`, `count_space_refusals`); what the plan alone knows (a name
//! OneDrive cannot hold, a file larger than OneDrive takes, the guard's hold) from the
//! answers published. An item beyond the plan's reach counts as pending until the plan
//! reaches it.
//!
//! **Tested without the rest**, a test per state and per word (today's
//! `upload/kept_back/tests.rs` and `konedrivectl`'s `text/uploads/tests.rs` are the
//! reference for the words):
// test: an_unbound_directory_is_mkdir_an_unbound_file_create
// test: a_changed_content_is_update_also_when_the_item_was_moved_too
// test: a_place_that_differs_alone_is_move_and_no_local_version_is_delete
// test: a_running_step_is_running_with_its_bytes
// test: a_planned_step_is_ready
// test: a_wait_until_a_time_is_retry_with_the_reason_of_the_last_failure_and_the_time
// test: a_wait_for_the_user_is_blocked_with_its_reason
// test: a_removal_the_mass_removal_guard_holds_is_held
// test: every_entry_that_is_not_blocked_or_held_is_paused_while_the_account_is
// test: a_change_that_sends_content_waits_for_space_while_onedrive_is_full
// test: a_wait_for_another_item_or_for_a_listing_is_waiting
// test: an_item_the_last_plan_was_not_given_is_ready
// test: the_counts_are_the_stores_sum_less_what_the_memory_and_the_answers_take_out
// test: the_summary_groups_what_is_kept_back_by_reason_in_the_groups_order
// test: what_is_not_uploaded_is_listed_per_file_with_its_reason
// test: a_removals_path_is_that_of_its_synced_place
// test: a_running_upload_is_shown_under_its_items_path
// test: confirming_writes_the_confirmation_of_every_unsent_removal_as_the_user_was_asked
// test: taking_removals_back_leaves_each_as_an_item_of_onedrive_the_folder_does_not_show
#![allow(dead_code, unused_variables)]

use std::path::{Path, PathBuf};

use konedrive_dbus::rows::{Change, KeptBack, KeptBackFiles, KeptBackReason, NotInFolder, Transfer};
use konedrive_tree::{Reason, Item, ItemMemory, ItemNotShown, ItemReader, TreeError, TreeStore, UnsentTotal};

use crate::folder::published_plan::{ItemAnswer, PublishedPlan};
use crate::folder::running_steps::{RunningStep, RunningSteps};
use crate::upload::onedrive_space::OneDriveSpace;
use crate::upload::sending_holds::{SendingHold, SendingHolds};

/// What of an item is unsent, in the five words `Changes()` has today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsentKind {
    Mkdir,
    Create,
    Update,
    Move,
    Delete,
}

impl UnsentKind {
    /// The word on the bus: `mkdir`, `create`, `update`, `move`, `delete`.
    pub fn as_str(self) -> &'static str {
        todo!()
    }

    /// Whether sending it sends a file's content.
    pub fn sends_content(self) -> bool {
        todo!()
    }
}

/// What of `item` is unsent: no binding and a directory is `Mkdir`, no binding and a file
/// `Create`, a local content that is not the synced one `Update` (also when the place
/// differs too: one entry, as today), a local place that is not the synced one `Move`, no
/// local version `Delete`. `None` for an item that agrees. Pure. MISSING-6.c.
pub fn decide_unsent_kind(item: &Item) -> Option<UnsentKind> {
    todo!()
}

/// An entry's state, in the seven words `Changes()` has today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    Ready,
    Running,
    Retry,
    Waiting,
    Blocked,
    Held,
    Paused,
}

impl QueueState {
    /// The word on the bus.
    pub fn as_str(self) -> &'static str {
        todo!()
    }
}

/// The state of the sending as a whole, as an entry is described under it: read once per
/// answer of the bus ([`read_queue_conditions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConditions {
    pub hold: Option<SendingHold>,
    pub is_onedrive_full: bool,
}

/// An unsent item with the sources read for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsentEntry {
    pub item: Item,
    /// Where it is, relative to the root: its local path, or for a removal the path of
    /// its synced place (`ItemReader::find_synced_path`).
    pub path: PathBuf,
    /// `None` for an item the last plan was not given.
    pub answer: Option<ItemAnswer>,
    /// The step that runs for it, when one does.
    pub running: Option<RunningStep>,
    pub memory: ItemMemory,
}

/// The state of one entry. Pure; the first that holds: `running`, a step runs; `held`, the
/// plan's wait for the user is the guard against a mass removal; `blocked`, any other
/// wait for the user, a wait for the check at start (`WaitReason::StartUpCheck`) and a
/// fault of the plan; `paused`, while the account is; `retry`, a
/// wait until a time that the step memory's next try explains; `waiting`, any other wait
/// (another item, a listing, an item not given, the quiet time, space); `ready`, a
/// planned step, and an item the last plan was not given. An item the plan leaves out is
/// no entry at all.
pub fn decide_queue_state(kind: UnsentKind, entry: &UnsentEntry, sending: &QueueConditions) -> QueueState {
    todo!()
}

/// The reason an entry carries and the time of its next try (0 for none): the plan's wait
/// (`WaitReason::Time { until, reason }`, which carries the reason of the next try the
/// plan was told; `WaitReason::User(reason)`; `WaitReason::StartUpCheck`), and, while OneDrive
/// is full, `WaitingForSpace` for a change that sends content and says nothing else.
/// Nothing while paused. Pure.
pub fn decide_queue_reason(kind: UnsentKind, state: QueueState, entry: &UnsentEntry, sending: &QueueConditions) -> (Option<Reason>, i64) {
    todo!()
}

/// `Changes()`'s row of one entry: `seq` is the item's `unsent_since`, `kind` and `state`
/// the words above, `path` under `folder`, `sent` and `total` the running step's bytes,
/// else the upload session's offset and the local content's size. `None` for an item that
/// agrees by now. Pure.
pub fn describe_queue_change(entry: &UnsentEntry, sending: &QueueConditions, folder: &Path) -> Option<Change> {
    todo!()
}

/// The counts of the queue's properties: `PendingCount`/`Bytes`, `BlockedCount`,
/// `HeldCount`, `QuotaWaitingCount`/`Bytes`, `TooBigCount`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCounts {
    pub pending: u32,
    pub pending_bytes: u64,
    pub blocked: u32,
    pub held: u32,
    pub space_waiting: u32,
    pub space_waiting_bytes: u64,
    pub too_big: u32,
    pub too_big_bytes: u64,
}

/// What the counts are made of, each read where the module's doc says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueTotals {
    /// `ItemReader::count_unsent`.
    pub unsent: UnsentTotal,
    /// `ItemReader::count_refusal_reasons(None)`.
    pub refused_for_good: u64,
    /// `ItemReader::count_space_refusals`.
    pub too_big: u64,
    /// The unsent items the last plan was given, with their answers.
    pub answered: Vec<UnsentEntry>,
}

/// The counts. Pure.
pub fn count_queue(totals: &QueueTotals, sending: &QueueConditions) -> QueueCounts {
    todo!()
}

/// The reason an entry is kept back for, when it is: blocked, waiting or in backoff with
/// a reason, or waiting for space; `None` for one that goes up by itself, runs, or is
/// held by the guard (which has its own question). Pure. Today's `kept_reason`
/// (`upload/kept_back.rs:60`).
pub fn decide_kept_back_reason(kind: UnsentKind, state: QueueState, entry: &UnsentEntry, sending: &QueueConditions) -> Option<Reason> {
    todo!()
}

/// `NotUploadedSummary()`: one row per reason, in the groups' order, then by reason. Pure.
/// The groups and the keys are `konedrive-reason`'s, unchanged.
pub fn summarize_kept_back(entries: &[UnsentEntry], sending: &QueueConditions) -> Vec<KeptBackReason> {
    todo!()
}

/// `NotUploaded()`: the files that need the user, each with its reason as stored: the
/// entries whose state is `blocked`. Pure. As today, what is left here (an ignored name)
/// is not among them.
pub fn list_not_uploaded(entries: &[UnsentEntry], sending: &QueueConditions, folder: &Path) -> Vec<KeptBack> {
    todo!()
}

/// `NotUploadedFiles(reason, limit)`: the files kept back for the reason of key `reason`,
/// by path, at most `limit` (0 for all), and how many there are. Pure.
pub fn list_kept_back_files(entries: &[UnsentEntry], sending: &QueueConditions, reason: &str, limit: u32, folder: &Path) -> KeptBackFiles {
    todo!()
}

/// `Transfers.Uploads`: one row per running step that sends content, under the path of
/// its item (`paths`, read with `ItemReader::find_path`), with how far it got. Pure.
pub fn describe_running_uploads(running: &[RunningStep], paths: &[(konedrive_tree::LocalItemId, PathBuf)], folder: &Path) -> Vec<Transfer> {
    todo!()
}

/// `Folder.Skipped()`'s row of an item the folder cannot show: its path in OneDrive, the
/// reason, and — while the folder still shows it — its path here and what keeps it, from
/// the plan's answer for it. Pure. MISSING-2.e.
pub fn describe_item_not_shown(not_shown: &ItemNotShown, local_path: Option<&Path>, answer: Option<&ItemAnswer>, folder: &Path) -> NotInFolder {
    todo!()
}

// ---- thin: the sources read ----

/// The state of the sending as a whole, now: `SendingHolds::find` and
/// `OneDriveSpace::is_full`. The one reader of it for the bus.
pub fn read_queue_conditions(sending_holds: &SendingHolds, onedrive_space: &OneDriveSpace, now: i64) -> QueueConditions {
    todo!()
}

/// The first `limit` unsent items in the order they came to be unsent (0 for all), each
/// with its path, what the steps remember of it, the plan's answer and its running step.
/// Called inside `Store::read_items`: the rows and the memory are of one moment, and the
/// bus waits for no writer.
pub fn read_unsent_entries(items: &ItemReader<'_>, published_plan: &PublishedPlan, running_steps: &RunningSteps, limit: usize) -> Result<Vec<UnsentEntry>, TreeError> {
    todo!()
}

/// What the counts are made of. Called inside `Store::read_items`.
pub fn read_queue_totals(items: &ItemReader<'_>, published_plan: &PublishedPlan, running_steps: &RunningSteps) -> Result<QueueTotals, TreeError> {
    todo!()
}

/// The entries the summaries and the lists of what is kept back are made of: the unsent
/// items the last plan was given, and those the step memory lists as refused for good or
/// for space (`list_refusal_reasons`, `list_space_refusal_reasons`), paged. Called inside
/// `Store::read_items`.
pub fn read_kept_back_entries(items: &ItemReader<'_>, published_plan: &PublishedPlan, running_steps: &RunningSteps) -> Result<Vec<UnsentEntry>, TreeError> {
    todo!()
}

/// `ConfirmDeletes()`: the user confirms the removals a mass removal holds. Reads every
/// unsent removal (`ItemReader::list_unsent_removals`), builds `UnsentRemoval::of` each —
/// the rows as the user was asked — and writes their confirmation
/// (`StepMemory::remember_removals_confirmed`), and empties the store's window of sent
/// removals (`ItemStore::begin_sent_removal_window`: the plan's `sent_removal_count`
/// is counted from nothing; the window ends by itself otherwise, and nothing else begins
/// one), in one job of the store's writer; how many. The caller wakes the plan
/// (`WakeCause::UserAnswered`). A removal made later has
/// another stamp and is asked about again (the map's F17). MISSING-1.k.
pub fn confirm_removals(store: &mut TreeStore) -> Result<usize, TreeError> {
    todo!()
}

/// `RestoreDeletes()`: the user takes the held removals back. For every unsent removal
/// that is not confirmed, deepest last: `commit_forget_synced_version` with the row's
/// versions as they are — the row is then an item OneDrive has that the folder never
/// showed, and the plan shows it again, not on disk (`FolderStep::ShowItem`), as today
/// the entries come back at the next listing. How many. One job of the store's writer,
/// run under the folder's tree lock by the caller, which then wakes the plan.
/// MISSING-2.l.
pub fn restore_removals(store: &mut TreeStore) -> Result<usize, TreeError> {
    todo!()
}

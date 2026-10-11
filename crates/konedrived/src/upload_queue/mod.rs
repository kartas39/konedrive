//! What the bus says of what waits, what is held and what was left out
//! (`org.konedrive.UploadQueue`, `Transfers.Uploads`, the folder's `Skipped()`): pure
//! functions of the three sources of design §7 — the plan's answer for each unsent item,
//! what the steps remember, and the state of the sending as a whole.
//!
//! **State owned:** none. The functions below the line "thin" read the sources and call
//! the pure ones; nothing here keeps a sum. What the properties of the bus are read from
//! is the folder's state (`SyncSnapshot::outbox`), which [`publish_queue_status`] writes
//! and nothing else does.
//!
//! **One answer is of two moments, and no more.** The plan's answers
//! (`PublishedPlan::list_answers`) and the running steps (`RunningSteps::list_steps`) are
//! taken once, before the read, and given to the readers as values; the rows and the step
//! memory are then read in ONE read closure of the store's reading connection
//! (`Store::read_items`), which locks nothing and waits for no writer. So the answers are
//! those of the last plan, which may be older than the rows: an answered item whose row
//! agrees by now is no entry, one whose row changed since is described by the answer it
//! had, and the plan that the change woke publishes the next. Every list is by local item
//! id, so no item is in one answer twice.
//!
//! **Answers for:** the map's MISSING-6.a, 6.c, 6.d, 2.e and the user's two answers to a
//! held mass removal (1.k, 2.l). The members of the bus, their XML, their row types
//! (`konedrive_dbus::rows`) and every word they carry stay today's.
//!
//! **Not here:** `Folder.Refresh()` is `SyncTasks::refresh` (`sync_tasks::stop_order`),
//! which wakes the plan with `WakeCause::RefreshAsked` — the plan's task makes every
//! next try due on that cause, and nobody else clears them.
//!
//! **With no running sync** (it could not start, the folder was taken down, a change of
//! the folder is under way) the answers are read from the store alone: the published
//! answers and the running steps are empty, and what holds sending and whether OneDrive
//! is full are not known ([`describe_stopped_sync`]). Every unsent item is then an entry
//! that reads `waiting`, but what the memory keeps as refused, which reads `blocked`;
//! the properties are empty, as the stop published them ([`publish_stopped_queue`]).
//!
//! **Called from below and from above.** The plan's task publishes the properties
//! ([`publish_queue_status`]: this directory is before `sync_plan_task` in the layer
//! order); the owner of the folder's state answers the methods of the bus. `org.konedrive.Conflicts`
//! reads the conflict record, which stays as it is (design §11). The interface has no
//! member that drops one unsent change or chooses a side of a conflict, today or here.
//!
//! **The counts** are over every unsent item, the plan's answers over the items the last
//! plan was given. `PendingCount`/`Bytes` begin as one sum of the store (`count_unsent`);
//! what the steps remember as refused for good or for space is counted from the memory's
//! own lists, over every item; what the plan alone knows (a name OneDrive cannot hold, a
//! file larger than OneDrive takes, the guard's hold, a wait for space while OneDrive is
//! full, an item it leaves out) from the answers. An item is counted once: an answered
//! item the memory lists too is the memory's. An item beyond the plan's reach counts as
//! pending until the plan reaches it; the plan is given the unsent items oldest first, so
//! what was refused is among the first it is given.
//!
//! **Cost.** `Changes(limit)`: `limit` rows by the index of what is unsent, with a keyed
//! read of the memory and a path each; `Changes(0)` reads every unsent row, as today. The
//! counts: one sum over that index, the memory's two lists of refusals whole (an index
//! each, as many rows as are refused), and a keyed read of the row and the memory per
//! item the last plan was given — no path. They are read once per plan, not per call of
//! the bus. The summaries: the same without the sum, and the paths of the files listed.
//!
//! **Tested without the rest**, a test per state and per word (today's
//! `upload/kept_back/tests.rs` and `konedrivectl`'s `text/uploads/tests.rs` are the
//! reference for the words):
// test: an_unbound_directory_is_mkdir_an_unbound_file_create
// test: a_changed_content_is_update_also_when_the_item_was_moved_too
// test: a_place_that_differs_alone_is_move_and_no_local_version_is_delete
// test: a_running_step_is_running_with_its_bytes
// test: a_planned_step_is_ready
// test: a_wait_until_a_time_is_retry_when_a_failed_try_is_remembered_and_waiting_otherwise
// test: a_wait_for_the_user_is_blocked_with_its_reason
// test: an_item_refused_for_good_is_blocked_whatever_its_reasons_group
// test: an_item_with_a_stamp_and_no_kind_is_no_entry_and_stays_pending
// test: a_wait_for_the_user_whose_reason_ends_by_itself_is_waiting
// test: a_wait_for_space_is_ready_with_its_reason
// test: a_removal_the_mass_removal_guard_holds_is_held
// test: every_entry_that_is_not_blocked_or_held_is_paused_while_the_account_is
// test: a_change_that_sends_content_waits_for_space_while_onedrive_is_full
// test: a_wait_for_another_item_or_for_a_listing_is_waiting
// test: an_item_the_last_plan_was_not_given_is_ready_or_what_the_memory_says
// test: an_item_the_plan_leaves_out_is_no_entry_and_not_pending
// test: the_counts_are_the_stores_sum_less_what_the_memory_and_the_answers_take_out
// test: an_item_the_memory_and_the_answers_both_refuse_is_counted_once
// test: the_summary_groups_what_is_kept_back_by_reason_in_the_groups_order
// test: what_is_not_uploaded_is_listed_per_file_with_its_reason
// test: a_removals_path_is_that_of_its_synced_place
// test: a_running_upload_is_shown_under_its_items_path
// test: an_item_not_shown_that_is_still_here_says_its_path_and_what_keeps_it
// test: a_removal_is_held_when_an_answer_waits_for_the_user_for_a_mass_removal
// test: confirming_writes_the_confirmation_of_every_unsent_removal_as_the_user_was_asked
// test: confirming_and_taking_back_do_nothing_while_no_removal_is_held
// test: taking_removals_back_leaves_each_as_an_item_of_onedrive_the_folder_does_not_show
#![allow(dead_code, unused_variables, unused_imports)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use konedrive_dbus::rows::{Change, KeptBack, KeptBackFiles, KeptBackReason, NotInFolder, Transfer};
use konedrive_tree::{Item, ItemMemory, ItemNotShown, ItemReader, LocalItemId, Reason, TreeError, TreeStore, UnsentTotal};

use crate::sync_parts::published_plan::{ItemAnswer, PublishedPlan};
use crate::sync_parts::running_steps::{RunningStep, RunningSteps};
use crate::status::snapshot::SyncStateHandle;
use crate::sync_parts::onedrive_space::OneDriveSpace;
use crate::sync_parts::sending_holds::{SendingHold, SendingHolds};

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

    /// Whether sending it sends a file's content: `Create` and `Update`.
    pub fn sends_content(self) -> bool {
        todo!()
    }
}

/// What of `item` is unsent; the first that holds: no local version, `Delete`; no synced
/// place (nothing was agreed on yet, with a binding or without) and a directory, `Mkdir`;
/// no synced place and a file, `Create`; a file whose local content is not the synced one,
/// `Update` (also when the place differs too: one entry, as today); a local place that is
/// not the synced one, `Move`. `None` for an item with no stamp `unsent_since`, and for
/// one none of these holds for. Pure. MISSING-6.c.
///
/// An item with a stamp for which this answers `None` is no entry, for every function
/// below: it is in no list, no summary and none of the counts made from the answers, and
/// it stays in `pending`, where the store's sum counted it.
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
    /// The word on the bus: `ready`, `running`, `retry`, `waiting`, `blocked`, `held`,
    /// `paused`.
    pub fn as_str(self) -> &'static str {
        todo!()
    }
}

/// The state of the sending as a whole, as an entry is described under it: read once per
/// answer of the bus ([`read_queue_conditions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConditions {
    /// The hold that is shown (`SendingHolds::find`): the first that holds. The entries
    /// read `paused` while it is `SendingHold::Paused`, and under no other hold: while
    /// the account is signed out, may not write, is throttled or has not ended its first
    /// listing, each entry keeps its own word and the folder's state says the hold, as
    /// today.
    pub hold: Option<SendingHold>,
    pub is_onedrive_full: bool,
    /// Whether a sync runs. False: `hold` and `is_onedrive_full` are not known (`None`
    /// and `false`), and say nothing ([`describe_stopped_sync`]).
    pub is_sync_running: bool,
}

/// The conditions of a folder whose sync does not run: no hold known, OneDrive not known
/// to be full, `is_sync_running` false. What the owner gives the readers in place of
/// [`read_queue_conditions`] when it has no `SendingHolds` and no `OneDriveSpace` to ask,
/// with empty answers and no running step. Pure.
pub fn describe_stopped_sync() -> QueueConditions {
    todo!()
}

/// An unsent item with the sources read for it. It has no path: the counts need none,
/// and what lists files reads the paths of those it lists ([`read_entry_paths`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsentEntry {
    pub item: Item,
    /// `None` for an item the last plan was not given.
    pub answer: Option<ItemAnswer>,
    /// The step that runs for it, when one does: the step the item is of, or one that
    /// carries its row (`RunningStep::other_items`).
    pub running: Option<RunningStep>,
    /// What the steps remember of it that its row still says
    /// (`ItemReader::find_item_memory`).
    pub memory: ItemMemory,
    /// For a directory whose removal the last plan holds for the user: how many items
    /// the removal takes from OneDrive below it (`PublishedPlan::find_held_removal_count`).
    /// `None` for every other entry.
    pub held_items_below: Option<u64>,
}

/// The state of one entry; `None` for an item the plan leaves out
/// (`ItemAnswer::Excluded`), which is no entry at all. Pure. The first that holds:
///
/// - `held`: the answer is `WaitReason::User(Reason::MassDelete)`;
/// - `blocked`: the memory keeps a refusal for good of it (`entry.memory.refusal`), given
///   to the last plan or not, and whatever the group of its reason — this comes before
///   the rule of the group: OneDrive's `NotAllowed` is of the group `waiting`, and the
///   plan answers it with the same `WaitReason::User(NotAllowed(..))` as a read-only
///   folder's wait, which the memory alone tells apart; or the answer is
///   `WaitReason::User(reason)` with a reason that neither waits for space
///   (`Reason::waits_for_space`) nor is of the group `waiting` (`Reason::group`); or
///   `WaitReason::StartUpCheck`; or `ItemAnswer::Fault`;
/// - `waiting`, with the reason that the folder is read-only: `sending.hold` is
///   `SendingHold::FolderReadOnly` — what was unsent at a forced switch stays here and
///   is neither blocked nor lost; and every entry while `sending.is_sync_running` is
///   false;
/// - `paused`: `sending.hold` is `SendingHold::Paused`, whatever else the entry would say,
///   a running step among it — as today;
/// - `running`: a step runs for it (`entry.running`);
/// - `retry`: the answer is `WaitReason::Time` and the memory keeps failed tries
///   (`ItemMemory::failed_tries`);
/// - `waiting`: any other `WaitReason::Time` (the quiet time, the next try of a step
///   towards the folder); `WaitReason::User(reason)` with a reason of the group
///   `waiting` (a file open for writing, a directory OneDrive does not have, a read-only
///   folder); `AnotherItem`, `ItemNotGiven`, `RunningStep` with no step found, `Listing`,
///   `ItemsInDirectoryNotGiven`;
/// - `ready`: a planned step of any class; a wait for the user whose reason waits for
///   space (`WaitingForSpace`, `TooBig`), which is kept back and stays in its place, as
///   today; and an item the last plan was not given.
pub fn decide_queue_state(entry: &UnsentEntry, sending: &QueueConditions) -> Option<QueueState> {
    todo!()
}

/// The reason an entry of `state` carries and the time of its next try (0 for none).
/// Pure. Nothing and 0 for `paused`, `running` and a `ready` that waits for no space.
/// Otherwise, the first that holds:
///
/// - the memory keeps a refusal for good (`entry.memory.refusal`): its reason
///   (`Refusal::reason`), 0 — the reason the plan's answer carries too, when the item was
///   given;
/// - `WaitReason::Time { until, reason }`: that reason and `until`;
/// - `WaitReason::User(reason)`: that reason, 0;
/// - `WaitReason::StartUpCheck` and `ItemAnswer::Fault`: `Reason::Blocked`, 0;
/// - an item the last plan was not given, of which the memory keeps a refusal for space
///   (`ItemMemory::space_refusal`): `Reason::TooBig` with the size of the content refused
///   and the free space, 0;
/// - any other wait: nothing, 0;
/// - and where none of these gave a reason, while OneDrive is full
///   (`sending.is_onedrive_full`) and `kind` sends content: `Reason::WaitingForSpace`.
///
/// The owner decided what the window says of two of them, and neither is called blocked:
/// an item the check at start left reads "could not be read on disk", an item the plan is
/// at fault about "being checked" (the daemon asks a full listing by itself).
// TODO: the two reasons do not exist in `konedrive-reason` yet (`Reason::NotReadOnDisk`, `Reason::BeingChecked`, with their keys, groups and texts in the catalogue): until they do both carry `Reason::Blocked`. Add them there and name them here; check and fix if needed.
pub fn decide_queue_reason(kind: UnsentKind, state: QueueState, entry: &UnsentEntry, sending: &QueueConditions) -> (Option<Reason>, i64) {
    todo!()
}

/// `Changes()`'s row of one entry: `seq` is the item's `unsent_since`, `kind` and `state`
/// the words above, the reason as stored (`Reason`'s `Display`; empty for none) and the
/// next try of [`decide_queue_reason`], the path `path` under `folder`. `sent` and
/// `total` are the running step's bytes when the step is of this item itself
/// (`RunningStep::item`; a step of another item that only carries this row says nothing
/// of this entry's bytes); else, for a kind that sends content, the remembered upload
/// session's `next_offset` (0 with none) and the local content's size; else 0 and 0.
/// `None` for an item that agrees by now ([`decide_unsent_kind`]) and for one the plan
/// leaves out ([`decide_queue_state`]). Pure. The caller leaves out an entry
/// [`read_entry_paths`] has no path for.
///
/// A held mass removal of a directory is ONE row, the directory's: `state` is `held`,
/// `sent` is 0 and `total` is the number of items the removal takes from OneDrive below
/// it (`entry.held_items_below`) — a count of items there, not of bytes, which the
/// window says as "N files".
// TODO: the number is of every item OneDrive has below the directory, its sub-directories among them, and it is shown as files: whether that is to be files alone (a second count by the plan's reading) or the word is to be "items" is the owner's to say. The rows below such a directory are no rows of their own:
/// the reader leaves them out ([`read_unsent_entries`]).
pub fn describe_queue_change(entry: &UnsentEntry, path: &Path, sending: &QueueConditions, folder: &Path) -> Option<Change> {
    todo!()
}

/// The counts of the queue's properties: `PendingCount`/`Bytes`, `BlockedCount`,
/// `HeldCount`, `QuotaWaitingCount`/`Bytes`, `TooBigCount`; `too_big_bytes` is on no
/// member of the bus and is taken off what is left to upload (`status::totals`).
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
    /// The items the memory lists as refused for good
    /// (`ItemReader::list_refusal_reasons(None, ..)`, whole), by item.
    pub refused_for_good: Vec<LocalItemId>,
    /// The files the memory lists as refused for lack of space
    /// (`ItemReader::list_space_refusal_reasons`, whole), each with the size of the
    /// content refused (`Reason::TooBig`'s first figure).
    pub refused_for_space: Vec<(LocalItemId, u64)>,
    /// The unsent items the last plan was given, with their answers.
    pub answered: Vec<UnsentEntry>,
}

/// The counts. Pure. With the state of each answered entry ([`decide_queue_state`]) and
/// the size of its local content where its kind sends content:
///
/// - `blocked`: the items refused for good by the memory, and the answered entries that
///   are `blocked` and not among them;
/// - `held`: the answered entries that are `held`;
/// - `too_big`, `too_big_bytes`: the files refused for space by the memory, and their
///   sizes;
/// - `space_waiting`, `space_waiting_bytes`: the answered entries whose reason is
///   `WaitingForSpace` ([`decide_queue_reason`]) and that are not among the files refused
///   for space;
/// - `pending`: the store's count less `blocked`, `held` and the answered entries the
///   plan leaves out; `pending_bytes`: the store's sum less the sizes of the answered
///   entries that are blocked, held or left out. An item the memory refuses for good that
///   the last plan was not given is off `pending` and its bytes stay in `pending_bytes`
///   until the plan is given it: the memory's list carries no size. What waits for space
///   or is too big stays in both, as today (`status::totals` takes it off what is left to
///   upload).
///
/// Each saturates at 0 and at the type's largest.
///
/// The owner decided `HeldCount`: a held mass removal of a directory is ONE, the
/// directory, whatever is below it; the rows below it are counted nowhere while it is
/// held — not in `held` and not in `pending` — as they are no entries. They are in the
/// store's sum and not among `answered`, so `pending` is ALSO less the sum of
/// `held_items_below` of the held entries, and `pending_bytes` less nothing for them (a
/// removal has no bytes). The number of
/// files is on the directory's row ([`describe_queue_change`]).
// TODO: `ConfirmDeletes()` answers the confirmations written (the directory and all below it), which is not this number; check which of the two the window shows after the answer and fix if needed. Former note: here it is the removals the last plan holds — a directory removed with all below it is one, and a removal beyond the plan's reach is none — where today it is one per file and directory. The number of items the held removals take from OneDrive is `ItemReader::count_unsent_removals`, a walk below every removed directory at each plan. Two numbers the user sees beside it differ too: while a removed directory is held, the items below it are entries of their own that read `waiting` (the plan answers them `AnotherItem(directory)`) and stay in `PendingCount`, where today held rows are in neither; and `ConfirmDeletes()` answers the confirmations written (the directory and all below it), which is not `HeldCount`.
pub fn count_queue(totals: &QueueTotals, sending: &QueueConditions) -> QueueCounts {
    todo!()
}

/// The reason an entry is kept back for, when it is: the reason of
/// [`decide_queue_reason`] for an entry that is `blocked`, `waiting` or `retry` with a
/// reason, and for a `ready` one whose reason waits for space; `Reason::Blocked` for a
/// `blocked` one with none. `None` for one that goes up by itself, runs, is paused, or is
/// held by the guard (which has its own question). Pure. Today's `kept_reason`
/// (`upload/kept_back.rs:60`).
pub fn decide_kept_back_reason(kind: UnsentKind, state: QueueState, entry: &UnsentEntry, sending: &QueueConditions) -> Option<Reason> {
    todo!()
}

/// `NotUploadedSummary()`: one row per key of a reason ([`decide_kept_back_reason`],
/// `Reason::key`), with how many entries are kept back for it and the sum of their local
/// contents' sizes, in the groups' order, then by key. The group is the reason's
/// (`Reason::group`), but `per-file` for a `blocked` entry whose reason is of the group
/// `waiting` or of none. Pure. The groups and the keys are `konedrive-reason`'s,
/// unchanged.
pub fn summarize_kept_back(entries: &[UnsentEntry], sending: &QueueConditions) -> Vec<KeptBackReason> {
    todo!()
}

/// The entries of `entries` that `NotUploaded()` lists: those whose state is `blocked`.
/// Pure. What the thin part reads the paths of.
pub fn list_blocked_entries<'a>(entries: &'a [UnsentEntry], sending: &QueueConditions) -> Vec<&'a UnsentEntry> {
    todo!()
}

/// The entries of `entries` kept back for the reason of key `reason`
/// ([`decide_kept_back_reason`]), by local item id. Pure. What `NotUploadedFiles` counts;
/// the thin part reads the paths of the first `limit` of them (of all for 0), and of no
/// other.
pub fn list_entries_kept_back_for<'a>(entries: &'a [UnsentEntry], sending: &QueueConditions, reason: &str) -> Vec<&'a UnsentEntry> {
    todo!()
}

/// `NotUploaded()`: the files that need the user ([`list_blocked_entries`]), each with
/// its full path (`paths`, under `folder`) and its reason as stored, by path. An entry
/// `paths` has no path for is left out. Pure. As today, what is left here (an ignored
/// name) is not among them.
pub fn list_not_uploaded(entries: &[UnsentEntry], paths: &BTreeMap<LocalItemId, PathBuf>, sending: &QueueConditions, folder: &Path) -> Vec<KeptBack> {
    todo!()
}

/// `NotUploadedFiles(reason, limit)`: the files kept back for the reason of key `reason`
/// ([`list_entries_kept_back_for`]): the first `limit` of them in that function's order,
/// by local item id (all for 0) — those `paths` was read for, as today's read with a
/// `LIMIT` — each with its full path and its reason as stored, sorted by path among
/// themselves; one `paths` has no path for is left out. `total` is how many entries are
/// kept back for the reason, whatever `limit` and whatever `paths` holds. Pure.
pub fn list_kept_back_files(entries: &[UnsentEntry], paths: &BTreeMap<LocalItemId, PathBuf>, sending: &QueueConditions, reason: &str, limit: u32, folder: &Path) -> KeptBackFiles {
    todo!()
}

/// `Transfers.Uploads`: one row per running step of the class `StepClass::Content`,
/// under the full path of its item (`paths`, under `folder`), with `sent` and `total`. A
/// step `paths` has no path for is left out. Pure.
pub fn describe_running_uploads(running: &[RunningStep], paths: &BTreeMap<LocalItemId, PathBuf>, folder: &Path) -> Vec<Transfer> {
    todo!()
}

/// `Folder.Skipped()`'s row of an item the folder cannot show: its full path in OneDrive
/// (`ItemNotShown::onedrive_path` under `folder`) and the reason as stored. `here` and
/// `waits` are empty when the folder does not show the item (`local_path` is `None`).
/// While it does: `here` is `local_path` under `folder`, and `waits` what keeps it, as
/// `WaitsFor` is stored, its path a full one: `WaitsFor::OpenForWriting(here)` for the
/// answer `WaitReason::User(Reason::OpenForWriting)`; `WaitsFor::Cycle` for no answer,
/// for `WaitReason::Listing` and for `ItemAnswer::Planned`, `Excluded` and `Fault` (the
/// step that takes the item out of the folder is planned, or no upload keeps it);
/// `WaitsFor::Uploads(1)` for any other wait. Pure.
/// MISSING-2.e.
///
/// The owner decided: an item the folder cannot show is listed at the level of the item
/// itself, one row, with no count of what waits below it (`WaitsFor::Uploads(1)` for a
/// directory too). This list is also where the user finds why a directory cannot be
/// removed here: the rows of what it holds that cannot be shown
/// (`mount`'s `rmdir` answers `ENOTEMPTY` for it and says so once in the activity log).
pub fn describe_item_not_shown(not_shown: &ItemNotShown, local_path: Option<&Path>, answer: Option<&ItemAnswer>, folder: &Path) -> NotInFolder {
    todo!()
}

/// Whether the last plan holds a removal for the user's confirmation: one of `answers` is
/// `ItemAnswer::Waiting(WaitReason::User(Reason::MassDelete))`. Pure. What
/// `ConfirmDeletes()` and `RestoreDeletes()` are given before they write.
pub fn is_removal_held(answers: &BTreeMap<LocalItemId, ItemAnswer>) -> bool {
    todo!()
}

// ---- thin: the sources read ----

/// The state of the sending as a whole, now: `SendingHolds::find(now)` and
/// `OneDriveSpace::is_full`. The one reader of it for the bus. Called before
/// `Store::read_items`, never inside it.
pub fn read_queue_conditions(sending_holds: &SendingHolds, onedrive_space: &OneDriveSpace, now: i64) -> QueueConditions {
    todo!()
}

/// The entry of `item`: its memory (`ItemReader::find_item_memory`), its answer of
/// `answers`, and of `running` the step that is of it or carries its row. Called inside
/// `Store::read_items` by the readers below. `held_items_below` is the count of
/// `held_removal_counts` for `item`. The same map is given to every reader below, and a
/// row below a held directory is no entry in any of them.
fn read_unsent_entry(items: &ItemReader<'_>, item: Item, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep], held_removal_counts: &BTreeMap<LocalItemId, u64>) -> Result<UnsentEntry, TreeError> {
    todo!()
}

/// `Changes(limit)`'s entries: the first `limit` unsent items in the order they came to
/// be unsent (`ItemReader::list_unsent(None, limit)`; `limit` 0 for all, read in pages by
/// the stamp), each as [`read_unsent_entry`] reads it. `answers` and `running` are
/// `PublishedPlan::list_answers` and `RunningSteps::list_steps`, taken by the caller
/// before the read, `held_removal_counts` `PublishedPlan::list_held_removal_counts`, taken
/// with them. A row whose answer is `WaitReason::AnotherItem(d)` with `d` a directory of
/// `held_removal_counts` is left out, and does not count towards `limit`: it is part of
/// that directory's one entry. Called inside `Store::read_items`: the rows and the memory
/// are of one moment, and the bus waits for no writer.
pub fn read_unsent_entries(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep], held_removal_counts: &BTreeMap<LocalItemId, u64>, limit: usize) -> Result<Vec<UnsentEntry>, TreeError> {
    todo!()
}

/// The path of each of `entries`, relative to the root, by item: its local path
/// (`ItemReader::find_path`), or for an item with no local version the path of its synced
/// place (`ItemReader::find_synced_path`, MISSING-6.d). An entry that has neither has no
/// path here. Called inside the `Store::read_items` that read the entries, for those of
/// them a list of files names.
pub fn read_entry_paths(items: &ItemReader<'_>, entries: &[&UnsentEntry]) -> Result<BTreeMap<LocalItemId, PathBuf>, TreeError> {
    todo!()
}

/// What the counts are made of: `ItemReader::count_unsent`; the memory's two lists of
/// refusals, whole; and for each item of `answers` its row (`ItemReader::find_item`) —
/// left out when the row is gone, has no stamp `unsent_since` or is left here
/// (`ItemReader::is_ignored`) — as [`read_unsent_entry`] reads it. Called inside
/// `Store::read_items`, with `answers` and `running` taken before it.
pub fn read_queue_totals(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep], held_removal_counts: &BTreeMap<LocalItemId, u64>) -> Result<QueueTotals, TreeError> {
    todo!()
}

/// The entries the summaries and the lists of what is kept back are made of, each item
/// once, by local item id: the unsent items of `answers`, as [`read_queue_totals`] reads
/// them, and the items the memory lists as refused for good or for space
/// (`list_refusal_reasons(None, ..)`, `list_space_refusal_reasons`, both whole) that are
/// not among them, read the same way. As many rows as the last plan was given and the
/// memory refuses; no path. Called inside `Store::read_items`.
pub fn read_kept_back_entries(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep], held_removal_counts: &BTreeMap<LocalItemId, u64>) -> Result<Vec<UnsentEntry>, TreeError> {
    todo!()
}

/// The path of the item of each step of `running` that sends content
/// (`StepClass::Content`), relative to the root (`ItemReader::find_path`), by item: what
/// [`describe_running_uploads`] is given. As many reads as uploads run. Called inside
/// `Store::read_items`.
pub fn read_running_upload_paths(items: &ItemReader<'_>, running: &[RunningStep]) -> Result<BTreeMap<LocalItemId, PathBuf>, TreeError> {
    todo!()
}

/// `Folder.Skipped()`: a row ([`describe_item_not_shown`]) per item OneDrive has that the
/// folder cannot show (`ItemReader::list_items_not_shown`, whole, in pages by item), each
/// with its local path when the folder still shows it (`ItemReader::find_path`) and its
/// answer of `answers`. Called inside `Store::read_items`, with `answers` taken before
/// it.
pub fn read_items_not_shown(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, folder: &Path) -> Result<Vec<NotInFolder>, TreeError> {
    todo!()
}

/// Writes what the properties of the bus are read from, when it changed
/// (`SyncStateHandle::update_if_changed`): `counts` into `SyncSnapshot::outbox`'s eight
/// counts, `uploads` into its `uploads` (path, done, total) and `is_onedrive_full` into
/// its `quota_full`. The note there is the plan's task's and is left as it is. The one
/// writer of these fields: the properties' change signals, the folder's overall state
/// (`status::overall`) and what is left to upload (`status::totals`) follow from it.
///
/// Called by the plan's task alone (`SyncPlanTask::publish_queue_status`), after every
/// plan it published and after every round of steps it settled, with the counts of one
/// `Store::read_items`
/// ([`read_queue_totals`], [`count_queue`], [`read_running_upload_paths`],
/// [`describe_running_uploads`]) — and, with the counts as they were, by its timer while
/// a step that sends content runs, at most once a second, for the bytes of `uploads`
/// (`RunningSteps::tell_progress` wakes nobody). The stop of a folder's sync publishes
/// through [`publish_stopped_queue`].
pub fn publish_queue_status(state: &SyncStateHandle, counts: QueueCounts, uploads: Vec<Transfer>, is_onedrive_full: bool) {
    todo!()
}

/// Publishes the queue of a sync that stopped: every count 0, no upload, OneDrive not
/// full, and the note none — the note is the plan's task's too, and a stopped task says
/// nothing. Called by the plan's loop as the last thing before it returns at a stop
/// (`SyncPlanTask::run`), so by the one writer of these fields; and by nobody else: a
/// Forget finds them empty.
pub fn publish_stopped_queue(state: &SyncStateHandle) {
    todo!()
}

/// `ConfirmDeletes()`: the user confirms the removals a mass removal holds. Nothing is
/// done and 0 answered when `is_removal_held` is false ([`is_removal_held`], asked of the
/// published answers by the caller): as today, with nothing held nothing is confirmed.
/// Otherwise, in one job of the store's writer: reads every unsent removal
/// (`ItemReader::list_unsent_removals` over the reader of the writer's connection, `ItemStore::reader`, in pages by the stamp),
/// builds `UnsentRemoval::of` each — the rows as they are when the user answers — writes
/// their confirmation (`StepMemory::remember_removals_confirmed`, which answers those it
/// did not write: rows that are another removal by now, or none, left to be asked about
/// again), and empties the store's window of sent removals
/// (`ItemStore::begin_sent_removal_window`: the plan's `sent_removal_count` is counted
/// from nothing; the window ends by itself otherwise, and nothing else begins one).
/// Answers how many confirmations were written. The caller then wakes the plan
/// (`WakeCause::UserAnswered`). A removal made later has another stamp and is asked about
/// again (the map's F17). MISSING-1.k.
pub fn confirm_removals(store: &mut TreeStore, is_removal_held: bool) -> Result<usize, TreeError> {
    todo!()
}

/// `RestoreDeletes()`: the user takes the held removals back. Nothing is done and 0
/// answered when `is_removal_held` is false, as [`confirm_removals`]. Otherwise, for
/// every unsent removal (`ItemReader::list_unsent_removals` over `ItemStore::reader`, in pages by the stamp) whose
/// confirmation the memory does not keep (`ItemReader::find_item_memory`,
/// `removal_confirmation`), in any order — each write is of one row alone:
/// `ItemStore::commit_forget_synced_version` with the row's versions as they are
/// (`ItemVersions::of`). The row is then an item OneDrive has that the folder never
/// showed, and the plan shows it again, not on disk (`FolderStep::ShowItem`), as today
/// the entries come back at the next listing. Answers how many rows were written
/// (`StepCommitOutcome::Written`). One job of the store's writer, run under the folder's
/// tree lock by the caller, which then wakes the plan (`WakeCause::UserAnswered`).
/// MISSING-2.l.
pub fn restore_removals(store: &mut TreeStore, is_removal_held: bool) -> Result<usize, TreeError> {
    todo!()
}

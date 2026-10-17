//! What the bus says of what waits and what was left out
//! (`org.konedrive.UploadQueue`, `Transfers.Uploads`, the folder's `Skipped()`): pure
//! functions of the three sources of `docs/design/writes.md` §7 — the plan's answer for each unsent item,
//! what the steps remember, and the state of the sending as a whole.
//!
//! **State owned:** none. The functions below the line "thin" read the sources and call
//! the pure ones; nothing here keeps a sum. What the properties of the bus are read from
//! is the folder's state (`SyncSnapshot::upload_queue`), which [`publish_queue_status`] writes
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
//! **Answers for:** the members of `org.konedrive.UploadQueue` as `dbus/` defines them,
//! their row types (`konedrive_dbus::rows`) and every word they carry.
//!
//! **Not here:** `Folder.Refresh()` is `SyncTasks::refresh` (`sync_tasks::stop_order`),
//! which wakes the plan with `WakeCause::RefreshAsked` — the plan's task makes every
//! next try due on that cause, and forgets what OneDrive refused as not allowed.
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
//! reads the conflict record. The interface has no member that drops one unsent change
//! or chooses a side of a conflict.
//!
//! **The counts** are over every unsent item, the plan's answers over the items the last
//! plan was given. `PendingCount`/`Bytes` begin as one sum of the store (`count_unsent`);
//! what the steps remember as refused for good or for space is counted from the memory's
//! own lists, over every item; what the plan alone knows (a name OneDrive cannot hold, a
//! wait for space while OneDrive is full, an item it leaves out) from the answers. An
//! item is counted once: an answered
//! item the memory lists too is the memory's. An item beyond the plan's reach counts as
//! pending until the plan reaches it; the plan is given the unsent items oldest first, so
//! what was refused is among the first it is given.
//!
//! **Cost.** `Changes(limit)`: `limit` rows by the index of what is unsent, with a keyed
//! read of the memory and a path each; `Changes(0)` reads every unsent row. The
//! counts: one sum over that index, the memory's two lists of refusals whole (an index
//! each, as many rows as are refused), and a keyed read of the row and the memory per
//! item the last plan was given — no path. They are read once per plan, not per call of
//! the bus. The summaries: the same without the sum, and the paths of the files listed.
//!
//! **Tested without the rest**, a test per state and per word (`konedrivectl`'s
//! `text/uploads/tests.rs` hold the words too):
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
// test: every_entry_that_is_not_blocked_is_paused_while_the_account_is
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use konedrive_dbus::rows::{Change, KeptBack, KeptBackFiles, KeptBackReason, NotInFolder, Transfer};
use konedrive_tree::{ContentVersion, Group, Item, ItemKind, ItemMemory, ItemNotShown, ItemReader, LocalItemId, Reason, TreeError, UnsentTotal, WaitReason, WaitsFor, READ_ONLY_FOLDER};

use crate::sync_parts::published_plan::{decide_is_shown_blocked, ItemAnswer};
use crate::sync_parts::running_steps::{RunningStep, StepClass};
use crate::status::snapshot::SyncStateHandle;
use crate::sync_parts::onedrive_space::OneDriveSpace;
use crate::sync_parts::sending_holds::{SendingHold, SendingHolds};

/// What of an item is unsent, in the five words of `Changes()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnsentKind {
    Mkdir,
    Create,
    Update,
    Move,
    Delete,
}

impl UnsentKind {
    /// The word on the bus: `mkdir`, `create`, `update`, `move`, `delete`.
    pub fn as_str(self) -> &'static str {
        match self {
            UnsentKind::Mkdir => "mkdir",
            UnsentKind::Create => "create",
            UnsentKind::Update => "update",
            UnsentKind::Move => "move",
            UnsentKind::Delete => "delete",
        }
    }

    /// Whether sending it sends a file's content: `Create` and `Update`.
    pub(crate) fn sends_content(self) -> bool {
        matches!(self, UnsentKind::Create | UnsentKind::Update)
    }
}

/// What of `item` is unsent; the first that holds: no local version, `Delete`; no synced
/// place (nothing was agreed on yet, with a binding or without) and a directory, `Mkdir`;
/// no synced place and a file, `Create`; a file whose local content is not the synced one,
/// `Update` (also when the place differs too: one entry); a local place that is
/// not the synced one, `Move`. `None` for an item with no stamp `unsent_since`, and for
/// one none of these holds for. Pure.
///
/// An item with a stamp for which this answers `None` is no entry, for every function
/// below: it is in no list, no summary and none of the counts made from the answers, and
/// it stays in `pending`, where the store's sum counted it.
pub(crate) fn decide_unsent_kind(item: &Item) -> Option<UnsentKind> {
    item.unsent_since?;
    let Some(local) = &item.local else { return Some(UnsentKind::Delete) };
    let Some(synced_place) = &item.synced_place else {
        return Some(match item.kind {
            ItemKind::Directory => UnsentKind::Mkdir,
            ItemKind::File => UnsentKind::Create,
        });
    };
    if item.kind == ItemKind::File && local.content != item.synced_content {
        return Some(UnsentKind::Update);
    }
    (local.place.as_ref() != Some(synced_place)).then_some(UnsentKind::Move)
}

/// An entry's state, in the six words of `Changes()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueState {
    Ready,
    Running,
    Retry,
    Waiting,
    Blocked,
    Paused,
}

impl QueueState {
    /// The word on the bus: `ready`, `running`, `retry`, `waiting`, `blocked`, `paused`.
    pub fn as_str(self) -> &'static str {
        match self {
            QueueState::Ready => "ready",
            QueueState::Running => "running",
            QueueState::Retry => "retry",
            QueueState::Waiting => "waiting",
            QueueState::Blocked => "blocked",
            QueueState::Paused => "paused",
        }
    }
}

/// The state of the sending as a whole, as an entry is described under it: read once per
/// answer of the bus ([`read_queue_conditions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConditions {
    /// The hold that is shown (`SendingHolds::find`): the first that holds. The entries
    /// read `paused` while it is `SendingHold::Paused`, and under no other hold: while
    /// the account is signed out, may not write, is throttled or has not ended its first
    /// listing, each entry keeps its own word and the folder's state says the hold.
    pub(crate) hold: Option<SendingHold>,
    pub(crate) is_onedrive_full: bool,
    /// Whether a sync runs. False: `hold` and `is_onedrive_full` are not known (`None`
    /// and `false`), and say nothing ([`describe_stopped_sync`]).
    pub(crate) is_sync_running: bool,
}

/// The conditions of a folder whose sync does not run: no hold known, OneDrive not known
/// to be full, `is_sync_running` false. What the owner gives the readers in place of
/// [`read_queue_conditions`] when it has no `SendingHolds` and no `OneDriveSpace` to ask,
/// with empty answers and no running step. Pure.
pub(crate) fn describe_stopped_sync() -> QueueConditions {
    QueueConditions { hold: None, is_onedrive_full: false, is_sync_running: false }
}

/// An unsent item with the sources read for it. It has no path: the counts need none,
/// and what lists files reads the paths of those it lists ([`read_entry_paths`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsentEntry {
    pub(crate) item: Item,
    /// `None` for an item the last plan was not given.
    pub(crate) answer: Option<ItemAnswer>,
    /// The step that runs for it, when one does: the step the item is of, or one that
    /// carries its row (`RunningStep::other_items`).
    pub(crate) running: Option<RunningStep>,
    /// What the steps remember of it that its row still says
    /// (`ItemReader::find_item_memory`).
    pub(crate) memory: ItemMemory,
}

/// The state of one entry; `None` for an item the plan leaves out
/// (`ItemAnswer::Excluded`), which is no entry at all. Pure. The first that holds:
///
/// - `blocked`, by the one rule the emblem goes by too
///   (`sync_parts::published_plan::decide_is_shown_blocked`), which is: the memory keeps a refusal for good of it (`entry.memory.refusal`), given
///   to the last plan or not, and whatever the group of its reason — this comes before
///   the rule of the group: OneDrive's `NotAllowed` is of the group `waiting`, and the
///   plan answers it with the same `WaitReason::User(NotAllowed(..))` as a read-only
///   folder's wait, which the memory alone tells apart; or the answer is
///   `WaitReason::User(reason)` with a reason that neither waits for space
///   (`Reason::waits_for_space`) nor is of the group `waiting` (`Reason::group`);
/// - `waiting`, never `blocked`: `WaitReason::StartUpCheck` (the
///   reason says "could not be read on disk") and `ItemAnswer::Fault` ("being checked");
/// - `waiting`, with the reason that the folder is read-only: `sending.hold` is
///   `SendingHold::FolderReadOnly` — what was unsent at a forced switch stays here and
///   is neither blocked nor lost; and every entry while `sending.is_sync_running` is
///   false;
/// - `paused`: `sending.hold` is `SendingHold::Paused`, whatever else the entry would say,
///   a running step among it;
/// - `running`: a step runs for it (`entry.running`);
/// - `retry`: the answer is `WaitReason::Time` and the memory keeps failed tries
///   (`ItemMemory::failed_tries`);
/// - `waiting`: any other `WaitReason::Time` (the quiet time, the next try of a step
///   towards the folder); `WaitReason::User(reason)` with a reason of the group
///   `waiting` (a file open for writing, a directory OneDrive does not have, a read-only
///   folder); `AnotherItem`, `ItemNotGiven`, `RunningStep` with no step found, `Listing`,
///   `ItemsInDirectoryNotGiven`;
/// - `ready`: a planned step of any class; a wait for the user whose reason waits for
///   space (`WaitingForSpace`, `TooBig`), which is kept back and stays in its place;
///   and an item the last plan was not given.
pub(crate) fn decide_queue_state(entry: &UnsentEntry, sending: &QueueConditions) -> Option<QueueState> {
    let answer = entry.answer.as_ref();
    if matches!(answer, Some(ItemAnswer::Excluded(_))) {
        return None;
    }
    if decide_is_shown_blocked(answer, entry.memory.refusal.is_some()) {
        return Some(QueueState::Blocked);
    }
    if matches!(answer, Some(ItemAnswer::Waiting(WaitReason::StartUpCheck) | ItemAnswer::Fault(_))) {
        return Some(QueueState::Waiting);
    }
    if !sending.is_sync_running || sending.hold == Some(SendingHold::FolderReadOnly) {
        return Some(QueueState::Waiting);
    }
    if matches!(sending.hold, Some(SendingHold::Paused { .. })) {
        return Some(QueueState::Paused);
    }
    if entry.running.is_some() {
        return Some(QueueState::Running);
    }
    Some(match answer {
        Some(ItemAnswer::Waiting(WaitReason::Time { .. })) if entry.memory.failed_tries.is_some() => QueueState::Retry,
        Some(ItemAnswer::Waiting(WaitReason::User(reason))) if reason.waits_for_space() => QueueState::Ready,
        Some(ItemAnswer::Waiting(_)) => QueueState::Waiting,
        // An excluded item and a fault were answered above.
        Some(ItemAnswer::Planned(_) | ItemAnswer::Excluded(_) | ItemAnswer::Fault(_)) | None => QueueState::Ready,
    })
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
/// - `WaitReason::StartUpCheck`: `Reason::NotReadOnDisk`, 0; `ItemAnswer::Fault`:
///   `Reason::BeingChecked`, 0;
/// - an item the last plan was not given, of which the memory keeps a refusal for space
///   (`ItemMemory::space_refusal`): `Reason::TooBig` with the size of the content refused
///   and the free space, 0;
/// - any other wait: nothing, 0;
/// - and where none of these gave a reason, while OneDrive is full
///   (`sending.is_onedrive_full`) and `kind` sends content: `Reason::WaitingForSpace`.
///
/// What the window says of two of them, neither called blocked:
/// an item the check at start left reads "could not be read on disk", an item the plan is
/// at fault about "being checked" (the daemon asks a full listing by itself): the two
/// reasons above, whose state reads `waiting`, not `blocked`.
pub(crate) fn decide_queue_reason(kind: UnsentKind, state: QueueState, entry: &UnsentEntry, sending: &QueueConditions) -> (Option<Reason>, i64) {
    if matches!(state, QueueState::Paused | QueueState::Running) {
        return (None, 0);
    }
    let said = if let Some(refusal) = &entry.memory.refusal {
        (Some(refusal.reason.clone()), 0)
    } else {
        match &entry.answer {
            Some(ItemAnswer::Waiting(WaitReason::Time { until, reason })) => (Some(reason.clone()), *until),
            Some(ItemAnswer::Waiting(WaitReason::User(reason))) => (Some(reason.clone()), 0),
            Some(ItemAnswer::Waiting(WaitReason::StartUpCheck)) => (Some(Reason::NotReadOnDisk), 0),
            Some(ItemAnswer::Fault(_)) => (Some(Reason::BeingChecked), 0),
            None => (entry.memory.space_refusal.as_ref().map(|refusal| Reason::TooBig(Some((refusal.content.size(), refusal.free_space)))), 0),
            Some(ItemAnswer::Waiting(_) | ItemAnswer::Planned(_) | ItemAnswer::Excluded(_)) => (None, 0),
        }
    };
    if said.0.is_some() {
        return said;
    }
    if sending.hold == Some(SendingHold::FolderReadOnly) && state == QueueState::Waiting {
        return (Some(Reason::NotAllowed(Some(READ_ONLY_FOLDER.to_owned()))), 0);
    }
    if sending.is_onedrive_full && kind.sends_content() {
        return (Some(Reason::WaitingForSpace), 0);
    }
    (None, 0)
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
pub fn describe_queue_change(entry: &UnsentEntry, path: &Path, sending: &QueueConditions, folder: &Path) -> Option<Change> {
    let kind = decide_unsent_kind(&entry.item)?;
    let state = decide_queue_state(entry, sending)?;
    let (reason, next_try) = decide_queue_reason(kind, state, entry, sending);
    let own_step = entry.running.as_ref().filter(|step| step.item == entry.item.local_item_id);
    let (sent, total) = match own_step {
        Some(step) => (step.sent, step.total),
        _ if kind.sends_content() => (entry.memory.upload_session.as_ref().map_or(0, |session| session.next_offset), find_content_size(kind, &entry.item)),
        _ => (0, 0),
    };
    Some(Change {
        seq: entry.item.unsent_since.unwrap_or(0),
        kind: kind.as_str().to_owned(),
        path: describe_full_path(folder, path),
        state: state.as_str().to_owned(),
        sent,
        total,
        reason: reason.map(|reason| reason.to_string()).unwrap_or_default(),
        next_try,
    })
}

/// The counts of the queue's properties: `PendingCount`/`Bytes`, `BlockedCount`,
/// `QuotaWaitingCount`/`Bytes`, `TooBigCount`; `too_big_bytes` is on no
/// member of the bus and is taken off what is left to upload (`status::totals`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCounts {
    pub(crate) pending: u32,
    pub(crate) pending_bytes: u64,
    pub(crate) blocked: u32,
    pub(crate) space_waiting: u32,
    pub(crate) space_waiting_bytes: u64,
    pub(crate) too_big: u32,
    pub(crate) too_big_bytes: u64,
}

/// What the counts are made of, each read where the module's doc says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueTotals {
    /// `ItemReader::count_unsent`.
    pub(crate) unsent: UnsentTotal,
    /// The items the memory lists as refused for good
    /// (`ItemReader::list_refusal_reasons(None, ..)`, whole), by item.
    pub(crate) refused_for_good: Vec<LocalItemId>,
    /// The files the memory lists as refused for lack of space
    /// (`ItemReader::list_space_refusal_reasons`, whole), each with the size of the
    /// content refused (`Reason::TooBig`'s first figure).
    pub(crate) refused_for_space: Vec<(LocalItemId, u64)>,
    /// The unsent items the last plan was given, with their answers.
    pub(crate) answered: Vec<UnsentEntry>,
}

/// The counts. Pure. With the state of each answered entry ([`decide_queue_state`]) and
/// the size of its local content where its kind sends content:
///
/// - `blocked`: the items refused for good by the memory, and the answered entries that
///   are `blocked` and not among them;
/// - `too_big`, `too_big_bytes`: the files refused for space by the memory, and their
///   sizes;
/// - `space_waiting`, `space_waiting_bytes`: the answered entries whose reason is
///   `WaitingForSpace` ([`decide_queue_reason`]) and that are not among the files refused
///   for space;
/// - `pending`: the store's count less `blocked` and the answered entries the
///   plan leaves out; `pending_bytes`: the store's sum less the sizes of the answered
///   entries that are blocked or left out. An item the memory refuses for good that
///   the last plan was not given is off `pending` and its bytes stay in `pending_bytes`
///   until the plan is given it: the memory's list carries no size. What waits for space
///   or is too big stays in both (`status::totals` takes it off what is left to
///   upload).
///
/// Each saturates at 0 and at the type's largest. A pause changes none of them: the
/// states and the reasons are decided under `sending` with a pause taken off, so what
/// waits for space is counted through a pause as before it.
pub fn count_queue(totals: &QueueTotals, sending: &QueueConditions) -> QueueCounts {
    let sending = &describe_conditions_without_pause(sending);
    let refused_for_good: BTreeSet<LocalItemId> = totals.refused_for_good.iter().copied().collect();
    let refused_for_space: BTreeSet<LocalItemId> = totals.refused_for_space.iter().map(|(item, _)| *item).collect();
    let (mut blocked, mut space_waiting, mut space_waiting_bytes) = (refused_for_good.len() as u64, 0u64, 0u64);
    // What is in the store's sum and is not pending: the items, and the bytes of their contents.
    let (mut not_pending, mut not_pending_bytes) = (refused_for_good.len() as u64, 0u64);
    for entry in &totals.answered {
        let item = entry.item.local_item_id;
        let Some(kind) = decide_unsent_kind(&entry.item) else { continue };
        let size = find_content_size(kind, &entry.item);
        let Some(state) = decide_queue_state(entry, sending) else {
            // The plan leaves it out.
            not_pending = not_pending.saturating_add(1);
            not_pending_bytes = not_pending_bytes.saturating_add(size);
            continue;
        };
        match state {
            QueueState::Blocked => {
                if !refused_for_good.contains(&item) {
                    blocked = blocked.saturating_add(1);
                    not_pending = not_pending.saturating_add(1);
                }
                not_pending_bytes = not_pending_bytes.saturating_add(size);
            }
            QueueState::Ready | QueueState::Running | QueueState::Retry | QueueState::Waiting | QueueState::Paused => {
                let (reason, _) = decide_queue_reason(kind, state, entry, sending);
                if reason == Some(Reason::WaitingForSpace) && !refused_for_space.contains(&item) {
                    space_waiting = space_waiting.saturating_add(1);
                    space_waiting_bytes = space_waiting_bytes.saturating_add(size);
                }
            }
        }
    }
    let too_big_bytes = totals.refused_for_space.iter().fold(0u64, |sum, (_, size)| sum.saturating_add(*size));
    QueueCounts {
        pending: saturate(totals.unsent.count.saturating_sub(not_pending)),
        pending_bytes: totals.unsent.bytes.saturating_sub(not_pending_bytes),
        blocked: saturate(blocked),
        space_waiting: saturate(space_waiting),
        space_waiting_bytes,
        too_big: saturate(totals.refused_for_space.len() as u64),
        too_big_bytes,
    }
}

/// The reason an entry is kept back for, when it is: the reason of
/// [`decide_queue_reason`] for an entry that is `blocked`, `waiting` or `retry` with a
/// reason, and for a `ready` one whose reason waits for space; `Reason::Blocked` for a
/// `blocked` one with none. `None` for one that goes up by itself or runs. A pause changes nothing of it: the summary and
/// the two lists give this the state and the conditions as they are with no pause
/// (`find_kept_back`), so `state` is never `paused` there. Pure.
pub(crate) fn decide_kept_back_reason(kind: UnsentKind, state: QueueState, entry: &UnsentEntry, sending: &QueueConditions) -> Option<Reason> {
    let (reason, _) = decide_queue_reason(kind, state, entry, sending);
    match state {
        QueueState::Blocked => Some(reason.unwrap_or(Reason::Blocked)),
        QueueState::Waiting | QueueState::Retry => reason,
        QueueState::Ready => reason.filter(Reason::waits_for_space),
        QueueState::Running | QueueState::Paused => None,
    }
}

/// `NotUploadedSummary()`: one row per key of a reason ([`decide_kept_back_reason`],
/// `Reason::key`), with how many entries are kept back for it and the sum of their local
/// contents' sizes, in the groups' order, then by key. The group is the reason's
/// (`Reason::group`), but `per-file` for a `blocked` entry whose reason is of the group
/// `waiting` or of none. Pure. The groups and the keys are `konedrive-reason`'s,
/// unchanged.
pub(crate) fn summarize_kept_back(entries: &[UnsentEntry], sending: &QueueConditions) -> Vec<KeptBackReason> {
    // By the group, in the groups' order (`Group`'s own), then by key: how many, and their bytes.
    let mut sums: BTreeMap<(Group, String), (u64, u64)> = BTreeMap::new();
    for entry in entries {
        let Some((kind, state, reason)) = find_kept_back(entry, sending) else { continue };
        let sum = sums.entry((decide_group(&reason, state == QueueState::Blocked), reason.key().to_owned())).or_default();
        sum.0 = sum.0.saturating_add(1);
        sum.1 = sum.1.saturating_add(find_content_size(kind, &entry.item));
    }
    sums.into_iter().map(|((group, reason), (count, bytes))| KeptBackReason { group: group.as_str().to_owned(), reason, count: saturate(count), bytes }).collect()
}

/// The entries of `entries` that `NotUploaded()` lists: those whose state is `blocked`.
/// Pure. What the thin part reads the paths of.
pub(crate) fn list_blocked_entries<'a>(entries: &'a [UnsentEntry], sending: &QueueConditions) -> Vec<&'a UnsentEntry> {
    entries.iter().filter(|entry| decide_unsent_kind(&entry.item).is_some() && decide_queue_state(entry, sending) == Some(QueueState::Blocked)).collect()
}

/// The entries of `entries` kept back for the reason of key `reason`
/// ([`decide_kept_back_reason`]), by local item id. Pure. What `NotUploadedFiles` counts;
/// the thin part reads the paths of the first `limit` of them (of all for 0), and of no
/// other.
pub(crate) fn list_entries_kept_back_for<'a>(entries: &'a [UnsentEntry], sending: &QueueConditions, reason: &str) -> Vec<&'a UnsentEntry> {
    let mut kept_back: Vec<&UnsentEntry> = entries.iter().filter(|entry| find_kept_back(entry, sending).is_some_and(|(_, _, why)| why.key() == reason)).collect();
    kept_back.sort_by_key(|entry| entry.item.local_item_id);
    kept_back
}

/// `NotUploaded()`: the files that need the user ([`list_blocked_entries`]), each with
/// its full path (`paths`, under `folder`) and its reason as stored, by path. An entry
/// `paths` has no path for is left out. Pure. What is left here (an ignored name) is not
/// among them.
pub fn list_not_uploaded(entries: &[UnsentEntry], paths: &BTreeMap<LocalItemId, PathBuf>, sending: &QueueConditions, folder: &Path) -> Vec<KeptBack> {
    let mut files: Vec<KeptBack> = list_blocked_entries(entries, sending).into_iter().filter_map(|entry| describe_kept_back(entry, paths, sending, folder)).collect();
    files.sort_by(|a, b| (&a.path, &a.reason).cmp(&(&b.path, &b.reason)));
    files
}

/// `NotUploadedFiles(reason, limit)`: the files kept back for the reason of key `reason`
/// ([`list_entries_kept_back_for`]): the first `limit` of them in that function's order,
/// by local item id (all for 0) — those `paths` was read for, as a read with a
/// `LIMIT` — each with its full path and its reason as stored, sorted by path among
/// themselves; one `paths` has no path for is left out. `total` is how many entries are
/// kept back for the reason, whatever `limit` and whatever `paths` holds. Pure.
pub fn list_kept_back_files(entries: &[UnsentEntry], paths: &BTreeMap<LocalItemId, PathBuf>, sending: &QueueConditions, reason: &str, limit: u32, folder: &Path) -> KeptBackFiles {
    let kept_back = list_entries_kept_back_for(entries, sending, reason);
    let total = saturate(kept_back.len() as u64);
    let listed = if limit == 0 { kept_back.len() } else { limit as usize };
    let mut items: Vec<KeptBack> = kept_back.into_iter().take(listed).filter_map(|entry| describe_kept_back(entry, paths, sending, folder)).collect();
    items.sort_by(|a, b| (&a.path, &a.reason).cmp(&(&b.path, &b.reason)));
    KeptBackFiles { items, total }
}

/// `Transfers.Uploads`: one row per running step of the class `StepClass::Content`,
/// under the full path of its item (`paths`, under `folder`), with `sent` and `total`. A
/// step `paths` has no path for is left out. Pure.
pub fn describe_running_uploads(running: &[RunningStep], paths: &BTreeMap<LocalItemId, PathBuf>, folder: &Path) -> Vec<Transfer> {
    running
        .iter()
        .filter(|step| step.class == StepClass::Content)
        .filter_map(|step| paths.get(&step.item).map(|path| Transfer { path: describe_full_path(folder, path), done: step.sent, total: step.total }))
        .collect()
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
///
/// An item the folder cannot show is listed at the level of the item
/// itself, one row, with no count of what waits below it (`WaitsFor::Uploads(1)` for a
/// directory too). This list is also where the user finds why a directory cannot be
/// removed here: the rows of what it holds that cannot be shown
/// (`mount`'s `rmdir` answers `ENOTEMPTY` for it and says so once in the activity log).
pub(crate) fn describe_item_not_shown(not_shown: &ItemNotShown, local_path: Option<&Path>, answer: Option<&ItemAnswer>, folder: &Path) -> NotInFolder {
    let path = describe_full_path(folder, &not_shown.onedrive_path);
    let reason = not_shown.reason.as_str().to_owned();
    let Some(local_path) = local_path else { return NotInFolder { path, reason, waits: String::new(), here: String::new() } };
    let here = describe_full_path(folder, local_path);
    let waits = match answer {
        Some(ItemAnswer::Waiting(WaitReason::User(Reason::OpenForWriting))) => WaitsFor::OpenForWriting(here.clone()),
        None | Some(ItemAnswer::Waiting(WaitReason::Listing) | ItemAnswer::Planned(_) | ItemAnswer::Excluded(_) | ItemAnswer::Fault(_)) => WaitsFor::Cycle,
        Some(ItemAnswer::Waiting(_)) => WaitsFor::Uploads(1),
    };
    NotInFolder { path, reason, waits: waits.to_string(), here }
}

/// How many rows a reader below asks the store for at a time.
const READ_PAGE: usize = 500;

/// The group what is kept back for `reason` is shown in: the reason's (`Reason::group`),
/// `waiting` for a reason of no group, and `per-file` for a `blocked` entry whose reason
/// is of the group `waiting` or of none. Pure.
fn decide_group(reason: &Reason, is_blocked: bool) -> Group {
    match reason.group() {
        Some(Group::Waiting) | None if is_blocked => Group::PerFile,
        Some(group) => group,
        None => Group::Waiting,
    }
}

/// A count as the bus carries it: the type's largest for one beyond it.
fn saturate(count: u64) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// `path`, relative to the root, as the full path under `folder` that the bus carries.
fn describe_full_path(folder: &Path, path: &Path) -> String {
    folder.join(path).display().to_string()
}

/// The size of the local content of `item` where `kind` sends content; 0 otherwise.
fn find_content_size(kind: UnsentKind, item: &Item) -> u64 {
    if !kind.sends_content() {
        return 0;
    }
    item.local.as_ref().and_then(|local| local.content.as_ref()).map_or(0, ContentVersion::size)
}

/// `sending` with a pause taken off: the conditions what is kept back and the counts are
/// decided under. A pause is a word of a row of `Changes()` and nothing else: what was
/// kept back before it is kept back through it. Pure.
fn describe_conditions_without_pause(sending: &QueueConditions) -> QueueConditions {
    let hold = sending.hold.clone().filter(|hold| !matches!(hold, SendingHold::Paused { .. }));
    QueueConditions { hold, ..sending.clone() }
}

/// The kind, the state and the reason `entry` is kept back for, as they are with no pause
/// ([`describe_conditions_without_pause`]); `None` for one that is no entry or is not
/// kept back ([`decide_kept_back_reason`]). Pure.
fn find_kept_back(entry: &UnsentEntry, sending: &QueueConditions) -> Option<(UnsentKind, QueueState, Reason)> {
    let sending = &describe_conditions_without_pause(sending);
    let kind = decide_unsent_kind(&entry.item)?;
    let state = decide_queue_state(entry, sending)?;
    let reason = decide_kept_back_reason(kind, state, entry, sending)?;
    Some((kind, state, reason))
}

/// The row of a file kept back: its full path and its reason as stored; `None` for an
/// entry `paths` has no path for, and for one that is not kept back. Pure.
fn describe_kept_back(entry: &UnsentEntry, paths: &BTreeMap<LocalItemId, PathBuf>, sending: &QueueConditions, folder: &Path) -> Option<KeptBack> {
    let (_, _, reason) = find_kept_back(entry, sending)?;
    let path = paths.get(&entry.item.local_item_id)?;
    Some(KeptBack { path: describe_full_path(folder, path), reason: reason.to_string() })
}

// ---- thin: the sources read ----

/// The state of the sending as a whole, now: `SendingHolds::find(now)` and
/// `OneDriveSpace::is_full`. The one reader of it for the bus. Called before
/// `Store::read_items`, never inside it.
pub(crate) fn read_queue_conditions(sending_holds: &SendingHolds, onedrive_space: &OneDriveSpace, now: i64) -> QueueConditions {
    QueueConditions { hold: sending_holds.find(now), is_onedrive_full: onedrive_space.is_full(), is_sync_running: true }
}

/// The entry of `item`: its memory (`ItemReader::find_item_memory`), its answer of
/// `answers`, and of `running` the step that is of it or carries its row. Called inside
/// `Store::read_items` by the readers below.
fn read_unsent_entry(items: &ItemReader<'_>, item: Item, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep]) -> Result<UnsentEntry, TreeError> {
    let id = item.local_item_id;
    Ok(UnsentEntry {
        memory: items.find_item_memory(id)?,
        answer: answers.get(&id).cloned(),
        running: running.iter().find(|step| step.item == id || step.other_items.contains(&id)).cloned(),
        item,
    })
}

/// `Changes(limit)`'s entries: the first `limit` unsent items in the order they came to
/// be unsent (`ItemReader::list_unsent(None, limit)`; `limit` 0 for all, read in pages by
/// the stamp), each as [`read_unsent_entry`] reads it. `answers` and `running` are
/// `PublishedPlan::list_answers` and `RunningSteps::list_steps`, taken by the caller
/// before the read. Called inside `Store::read_items`: the rows and the memory
/// are of one moment, and the bus waits for no writer.
pub(crate) fn read_unsent_entries(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep], limit: usize) -> Result<Vec<UnsentEntry>, TreeError> {
    let mut entries = Vec::new();
    let mut after = None;
    loop {
        let wanted = if limit == 0 { READ_PAGE } else { (limit - entries.len()).min(READ_PAGE) };
        let page = items.list_unsent(after, wanted)?;
        let Some(last_stamp) = page.last().and_then(|item| item.unsent_since) else { break };
        after = Some(last_stamp);
        for item in page {
            if limit != 0 && entries.len() >= limit {
                break;
            }
            entries.push(read_unsent_entry(items, item, answers, running)?);
        }
        if limit != 0 && entries.len() >= limit {
            break;
        }
    }
    Ok(entries)
}

/// The path of each of `entries`, relative to the root, by item: its local path
/// (`ItemReader::find_path`), or for an item with no local version the path of its synced
/// place (`ItemReader::find_synced_path`). An entry that has neither has no
/// path here, and neither has one whose synced path the store refuses to tell: any other
/// error of the store fails the read. Called inside the `Store::read_items` that read the entries, for those of
/// them a list of files names.
pub(crate) fn read_entry_paths(items: &ItemReader<'_>, entries: &[&UnsentEntry]) -> Result<BTreeMap<LocalItemId, PathBuf>, TreeError> {
    let mut paths = BTreeMap::new();
    for entry in entries {
        let id = entry.item.local_item_id;
        let path = if entry.item.local.is_some() {
            items.find_path(id)?
        } else {
            match items.find_synced_path(id) {
                // One row whose synced places end at no directory the folder shows: that
                // entry has no path and is left out; the answer of the bus stands.
                Err(TreeError::Refused(refusal)) => {
                    tracing::debug!("an unsent removal is left out of what the queue lists: {refusal}");
                    None
                }
                found => found?,
            }
        };
        if let Some(path) = path {
            paths.insert(id, path);
        }
    }
    Ok(paths)
}

/// What the counts are made of: `ItemReader::count_unsent`; the memory's two lists of
/// refusals, whole; and for each item of `answers` its row (`ItemReader::find_item`) —
/// left out when the row is gone, has no stamp `unsent_since` or is left here
/// (`ItemReader::is_ignored`) — as [`read_unsent_entry`] reads it. Called inside
/// `Store::read_items`, with `answers` and `running` taken before it.
pub fn read_queue_totals(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep]) -> Result<QueueTotals, TreeError> {
    let unsent = items.count_unsent()?;
    let refused_for_good = items.list_refusal_reasons(None, None, usize::MAX)?.into_iter().map(|refused| refused.item).collect();
    let refused_for_space = items.list_space_refusal_reasons(None, usize::MAX)?.into_iter().map(|refused| (refused.item, refused.reason.sizes().map_or(0, |(size, _)| size))).collect();
    let answered = read_answered_entries(items, answers, running)?.into_values().collect();
    Ok(QueueTotals { unsent, refused_for_good, refused_for_space, answered })
}

/// The entries the summaries and the lists of what is kept back are made of, each item
/// once, by local item id: the unsent items of `answers`, as [`read_queue_totals`] reads
/// them, and the items the memory lists as refused for good or for space
/// (`list_refusal_reasons(None, ..)`, `list_space_refusal_reasons`, both whole) that are
/// not among them, read the same way. As many rows as the last plan was given and the
/// memory refuses; no path. Called inside `Store::read_items`.
pub(crate) fn read_kept_back_entries(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep]) -> Result<Vec<UnsentEntry>, TreeError> {
    let mut entries = read_answered_entries(items, answers, running)?;
    let refused_for_good = items.list_refusal_reasons(None, None, usize::MAX)?;
    let refused_for_space = items.list_space_refusal_reasons(None, usize::MAX)?;
    for refused in refused_for_good.iter().chain(&refused_for_space) {
        if entries.contains_key(&refused.item) {
            continue;
        }
        if let Some(entry) = read_entry_of(items, refused.item, answers, running)? {
            entries.insert(refused.item, entry);
        }
    }
    Ok(entries.into_values().collect())
}

/// The path of the item of each step of `running` that sends content
/// (`StepClass::Content`), relative to the root (`ItemReader::find_path`), by item: what
/// [`describe_running_uploads`] is given. As many reads as uploads run. Called inside
/// `Store::read_items`.
pub(crate) fn read_running_upload_paths(items: &ItemReader<'_>, running: &[RunningStep]) -> Result<BTreeMap<LocalItemId, PathBuf>, TreeError> {
    let mut paths = BTreeMap::new();
    for step in running.iter().filter(|step| step.class == StepClass::Content) {
        if let Some(path) = items.find_path(step.item)? {
            paths.insert(step.item, path);
        }
    }
    Ok(paths)
}

/// `Folder.Skipped()`: a row ([`describe_item_not_shown`]) per item OneDrive has that the
/// folder cannot show (`ItemReader::list_items_not_shown`, whole, in pages by item), each
/// with its local path when the folder still shows it (`ItemReader::find_path`) and its
/// answer of `answers`. Called inside `Store::read_items`, with `answers` taken before
/// it.
pub(crate) fn read_items_not_shown(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, folder: &Path) -> Result<Vec<NotInFolder>, TreeError> {
    let mut rows = Vec::new();
    let mut after = None;
    loop {
        let page = items.list_items_not_shown(after, READ_PAGE)?;
        let Some(last) = page.last() else { break };
        after = Some(last.item.local_item_id);
        for not_shown in &page {
            let id = not_shown.item.local_item_id;
            let local_path = if not_shown.item.local.is_some() { items.find_path(id)? } else { None };
            rows.push(describe_item_not_shown(not_shown, local_path.as_deref(), answers.get(&id), folder));
        }
    }
    Ok(rows)
}

/// Writes what the properties of the bus are read from, when it changed
/// (`SyncStateHandle::update_if_changed`): `counts` into `SyncSnapshot::upload_queue`'s seven
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
    let uploads: Vec<(String, u64, u64)> = uploads.into_iter().map(|upload| (upload.path, upload.done, upload.total)).collect();
    state.update_if_changed(|snapshot| {
        let upload_queue = &mut snapshot.upload_queue;
        upload_queue.pending_count = counts.pending;
        upload_queue.pending_bytes = counts.pending_bytes;
        upload_queue.blocked_count = counts.blocked;
        upload_queue.space_waiting_count = counts.space_waiting;
        upload_queue.space_waiting_bytes = counts.space_waiting_bytes;
        upload_queue.too_big_count = counts.too_big;
        upload_queue.too_big_bytes = counts.too_big_bytes;
        upload_queue.uploads = uploads;
        upload_queue.quota_full = is_onedrive_full;
    });
}

/// Publishes the queue of a sync that stopped: every count 0, no upload, OneDrive not
/// full, and the note none — the note is the plan's task's too, and a stopped task says
/// nothing. Called by the plan's loop as the last thing before it returns at a stop
/// (`SyncPlanTask::run`), so by the one writer of these fields; and by nobody else: a
/// Forget finds them empty.
pub fn publish_stopped_queue(state: &SyncStateHandle) {
    state.update_if_changed(|snapshot| {
        let upload_queue = &mut snapshot.upload_queue;
        upload_queue.note = None;
        upload_queue.pending_count = 0;
        upload_queue.pending_bytes = 0;
        upload_queue.blocked_count = 0;
        upload_queue.space_waiting_count = 0;
        upload_queue.space_waiting_bytes = 0;
        upload_queue.too_big_count = 0;
        upload_queue.too_big_bytes = 0;
        upload_queue.uploads.clear();
        upload_queue.quota_full = false;
    });
}

/// The entry of the item `id`, as [`read_unsent_entry`] reads it; `None` when its row is
/// gone, has no stamp `unsent_since` or is left here (`ItemReader::is_ignored`). Called
/// inside `Store::read_items`.
fn read_entry_of(items: &ItemReader<'_>, id: LocalItemId, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep]) -> Result<Option<UnsentEntry>, TreeError> {
    let Some(item) = items.find_item(id)? else { return Ok(None) };
    if item.unsent_since.is_none() || items.is_ignored(id)? {
        return Ok(None);
    }
    read_unsent_entry(items, item, answers, running).map(Some)
}

/// The unsent items the last plan was given, by item: an entry ([`read_entry_of`]) per
/// item of `answers` that has one. Called inside `Store::read_items`.
fn read_answered_entries(items: &ItemReader<'_>, answers: &BTreeMap<LocalItemId, ItemAnswer>, running: &[RunningStep]) -> Result<BTreeMap<LocalItemId, UnsentEntry>, TreeError> {
    let mut entries = BTreeMap::new();
    for id in answers.keys() {
        if let Some(entry) = read_entry_of(items, *id, answers, running)? {
            entries.insert(*id, entry);
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests;

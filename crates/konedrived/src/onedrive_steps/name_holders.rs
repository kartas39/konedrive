//! Who holds a name OneDrive answered is taken, for a new file: the first of the two
//! causes no listing shows (`docs/design/sync.md` §7.3, `docs/design/writes.md` §4.4) —
//! the empty file of an upload session that holds the name — and the adoption
//! of the step's own earlier upload.
//!
//! **State owned:** when a session of another item that names no end was first seen
//! holding a name, for this run of the folder's sync ([`NameHolders`], in memory, shared
//! by its copies): such a session counts as ended a day after. All else is read: the
//! holder from Graph and, from the step memory, the upload sessions whose empty file
//! holds the place and the windows in which this folder was about to open one there.
//!
//! **Answers for:** a 409 never making a conflict copy of the folder's own empty file,
//! and never a loop: this folder's own is removed, an unknown one's is waited for, and
//! any other holder is the plan's (`StepRefusal::NameTaken`, kept as a taken name).
//!
//! **Tested without the rest**, a test per case of [`NameHolder`], then against the
//! fixture's fake OneDrive:
// test: no_holder_by_now_is_a_name_that_is_free
// test: the_empty_file_of_this_items_own_session_is_gone_on_with
// test: the_empty_file_of_another_items_live_session_is_waited_for
// test: the_empty_file_of_another_items_session_past_its_end_is_forgotten_and_the_name_asked_again
// test: the_holder_of_a_name_a_move_found_taken_is_answered_as_onedrive_spells_it
// test: another_items_session_with_no_end_named_counts_as_ended_a_day_after_it_was_first_seen
// test: the_empty_file_of_a_session_nothing_goes_on_with_is_cancelled
// test: an_empty_unlisted_file_made_within_an_opening_window_is_this_folders_and_is_removed
// test: an_empty_unlisted_file_made_outside_every_window_is_waited_for
// test: an_empty_unlisted_file_made_outside_every_window_is_adopted_by_a_step_that_sends_an_empty_file
// test: a_file_with_the_hash_sent_that_no_row_is_bound_to_is_adopted
// test: any_other_holder_is_another_item_with_its_name_as_onedrive_spells_it

use std::ffi::{OsStr, OsString};
use std::sync::{Arc, Mutex};

use konedrive_graph::drive::item::parse_graph_time;
use konedrive_graph::drive::{DriveError, DriveItem, WriteError};
use konedrive_graph::pool::Class;
use konedrive_graph::quickxor::QuickXor;
use konedrive_tree::{ConfirmedItem, EmptyFileSession, ItemPlace, ItemVersions, LocalItemId, OneDriveItemId, OneDriveItemReference, OpeningWindow, UploadSessionAddress};

use super::confirmed_item::{to_confirmed_item, to_graph_name, to_guard_of_answer};
use super::graph_refusal::{decide_read_refusal, decide_refusal, RefusedRequest};
use super::session_cancellations::SessionCancellations;
use super::GraphAccess;
use crate::sync_parts::step_outcome::{StepFailure, StepOutcome, StepRefusal};

/// How far OneDrive's clock may be behind or ahead of this computer's when the time an
/// empty file was made is compared with an opening's window, in seconds.
const CLOCK_SLACK: i64 = 5 * 60;

/// How long after it was first seen holding a name a session of another item that names
/// no end counts as ended, in seconds: a day, the longest an abandoned empty file of an
/// upload session was seen to live in OneDrive.
const SESSION_WITH_NO_END_KEPT: i64 = 24 * 60 * 60;

/// What is known of a taken name, gathered before the decision.
#[derive(Debug, Clone)]
pub(crate) struct NameHolderFacts {
    /// What OneDrive has under the name now; `None` when nothing is there any more.
    pub(crate) holder: Option<DriveItem>,
    /// The holder as a commit takes it, when it can be read as one
    /// (`confirmed_item::to_confirmed_item`).
    pub(crate) holder_item: Option<ConfirmedItem>,
    /// Whether a row is bound to the holder: a listing told of it already.
    pub(crate) is_holder_bound: bool,
    /// The hash of the content this step sends, when it was computed.
    pub(crate) sent_hash: Option<String>,
    /// The upload sessions whose empty file holds the place
    /// (`ItemReader::list_empty_file_sessions`).
    pub(crate) empty_file_sessions: Vec<EmptyFileSession>,
    /// The times between which this folder was about to open a session there
    /// (`ItemReader::list_opening_windows`).
    pub(crate) opening_windows: Vec<OpeningWindow>,
    /// When each session of another item that names no end was first seen holding a
    /// name, Unix seconds: what [`NameHolders`] keeps.
    pub(crate) sessions_first_seen: Vec<(UploadSessionAddress, i64)>,
}

/// Who holds the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NameHolder {
    /// Nobody by now: the request is sent again.
    Nobody,
    /// The empty file of this item's own session (`EmptyFileSession::Continued` of this
    /// item): the upload goes on in it.
    OwnSession,
    /// The empty file of another item's live session (`EmptyFileSession::Continued` of
    /// another item, whose `expires_at` is not past at `now`; one that names no end is
    /// live for a day after it was first seen holding the name): the step
    /// waits for it, as `StepRefusal::NameHeldByUploadSession`.
    AnotherItemsSession,
    /// The empty file of another item's session whose `expires_at` is past at `now`, or
    /// which names no end and was first seen holding the name a day ago or longer: the
    /// session ended by itself, whether its item has a step or not (it may be left here,
    /// or wait for the user). This step says so
    /// (`StepMemory::forget_ended_upload_session` of that item and address), and the
    /// request is sent again.
    AnotherItemsEndedSession { item: LocalItemId, address: UploadSessionAddress },
    /// The empty file of a session nothing goes on with: cancelled, then the request sent
    /// again.
    AbandonedSession(UploadSessionAddress),
    /// An empty file no listing told of, made within a window in which this folder was
    /// about to open a session there (the clock's slack: 5 minutes, [`CLOCK_SLACK`]):
    /// provably this folder's own, removed, then the request sent again.
    OwnEmptyFile(OneDriveItemReference),
    /// A file no row is bound to that holds the content sent: this step's own earlier
    /// upload, adopted. Before `UnknownEmptyFile` on purpose: an empty file is sent by
    /// one request with no opening kept, so the replay of one whose answer was lost finds
    /// an empty file no listing told of, and adopts it; after the cases above, so that an empty file a
    /// session or an opening of this folder explains is never adopted — a cancel of that
    /// session would take the item.
    OwnEarlierUpload(ConfirmedItem),
    /// An empty file no listing told of, of nobody known, where the content sent is not
    /// empty: the step waits for it. Never copied, never removed.
    UnknownEmptyFile,
    /// Another item, spelt so by OneDrive.
    AnotherItem { holder_name: OsString },
}

/// Who holds the name, from `facts`. Pure; the first case that holds, in the order of
/// [`NameHolder`]'s cases. `now` is the clock's, for the windows and for the end of
/// another item's session. A session is "this item's own" by `item.local_item_id`.
pub(crate) fn decide_name_holder(facts: &NameHolderFacts, item: &ItemVersions, now: i64) -> NameHolder {
    let Some(holder) = &facts.holder else { return NameHolder::Nobody };
    let sessions = &facts.empty_file_sessions;
    if sessions.iter().any(|session| matches!(session, EmptyFileSession::Continued { item: owner, .. } if *owner == item.local_item_id)) {
        return NameHolder::OwnSession;
    }
    let is_ended = |address: &UploadSessionAddress, expires_at: Option<i64>| match expires_at {
        Some(expires_at) => expires_at < now,
        None => facts.sessions_first_seen.iter().any(|(seen, first_seen_at)| seen == address && now.saturating_sub(*first_seen_at) >= SESSION_WITH_NO_END_KEPT),
    };
    let mut ended_session = None;
    for session in sessions {
        if let EmptyFileSession::Continued { item: owner, address, expires_at } = session {
            if !is_ended(address, *expires_at) {
                return NameHolder::AnotherItemsSession;
            }
            ended_session.get_or_insert(NameHolder::AnotherItemsEndedSession { item: *owner, address: address.clone() });
        }
    }
    if let Some(ended_session) = ended_session {
        return ended_session;
    }
    if let Some(address) = sessions.iter().find_map(|session| if let EmptyFileSession::Abandoned { address } = session { Some(address.clone()) } else { None }) {
        return NameHolder::AbandonedSession(address);
    }
    let holder_name = || NameHolder::AnotherItem { holder_name: holder.name.clone().unwrap_or_default().into() };
    // A holder that cannot be read as an item is nobody's to adopt or to remove.
    let Some(holder_item) = &facts.holder_item else { return holder_name() };
    let is_unlisted_file = holder.file.is_some() && !facts.is_holder_bound;
    let is_empty = holder.size == Some(0);
    let is_made_within_window = holder
        .created_date_time
        .as_deref()
        .and_then(parse_graph_time)
        .is_some_and(|made_at| facts.opening_windows.iter().any(|window| window.first_at - CLOCK_SLACK <= made_at && made_at <= window.last_unknown_at + CLOCK_SLACK));
    if is_unlisted_file && is_empty && is_made_within_window {
        return NameHolder::OwnEmptyFile(holder_item.item.clone());
    }
    let has_sent_content = facts.sent_hash.as_deref().is_some_and(|sent_hash| holder.quick_xor_hash() == Some(sent_hash) || (holder.quick_xor_hash().is_none() && is_empty && sent_hash == QuickXor::new().finish_base64()));
    if is_unlisted_file && has_sent_content {
        return NameHolder::OwnEarlierUpload(holder_item.clone());
    }
    if is_unlisted_file && is_empty {
        return NameHolder::UnknownEmptyFile;
    }
    holder_name()
}

/// What finds the holder of a taken name.
#[derive(Clone)]
pub(crate) struct NameHolders {
    graph: GraphAccess,
    session_cancellations: SessionCancellations,
    /// When each session of another item that names no end was first seen holding a name.
    sessions_first_seen: Arc<Mutex<Vec<(UploadSessionAddress, i64)>>>,
}

impl NameHolders {
    pub fn new(graph: GraphAccess, session_cancellations: SessionCancellations) -> Self {
        Self { graph, session_cancellations, sessions_first_seen: Arc::default() }
    }

    /// What OneDrive has under `name` in `parent` now; none when nothing is there. One
    /// read, with a slot of the pool for itself.
    async fn read_holder(&self, parent: &OneDriveItemReference, name: &OsStr) -> Result<Option<DriveItem>, StepOutcome> {
        let name = to_graph_name(name).map_err(StepOutcome::Refused)?;
        let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
        match self.graph.drive.child(parent.item_id.as_str(), name).await {
            Ok(holder) => Ok(Some(holder)),
            Err(DriveError::NotFound) => Ok(None),
            Err(error) => Err(StepOutcome::Refused(decide_read_refusal(error))),
        }
    }

    /// Notes the sessions of `sessions` that name no end as first seen at `now`, those
    /// that were not seen before; all that were seen, each with its time. One that ended
    /// goes from here when this part says so, and the rest with the run.
    fn note_sessions_seen(&self, sessions: &[EmptyFileSession], now: i64) -> Vec<(UploadSessionAddress, i64)> {
        let mut seen = crate::panic::lock(&self.sessions_first_seen);
        for session in sessions {
            if let EmptyFileSession::Continued { address, expires_at: None, .. } = session {
                if !seen.iter().any(|(known, _)| known == address) {
                    seen.push((address.clone(), now));
                }
            }
        }
        seen.clone()
    }

    /// After a 409 on `SendNewFile`: reads the holder (`child`), whether a
    /// row is bound to it (`ItemReader::find_bound_item`), and the step memory's sessions
    /// and windows at the place (`ItemReader::list_empty_file_sessions`,
    /// `list_opening_windows`; the place as the step memory names it: the directory of
    /// the place of `item.local`, under `name`), and
    /// decides ([`decide_name_holder`]). Then acts on what is the folder's own: cancels
    /// an abandoned session (`SessionCancellations::cancel_session`), removes its own
    /// empty file (`delete_item`, guarded by the eTag just read),
    /// forgets another item's session that ended
    /// (`StepMemory::forget_ended_upload_session`), and once the place is resolved — the
    /// empty file removed, the name found free, or the holder not this folder's — calls
    /// `StepMemory::forget_upload_session_openings_at`. Answers what the step does next:
    /// for `Nobody`, `AbandonedSession`, `OwnEmptyFile` and `AnotherItemsEndedSession`
    /// the step sends its request once more, and a second 409 in the same step is
    /// `StepRefusal::NameHeldByUploadSession`: no step asks a third time. A cancel or a
    /// removal OneDrive does not carry out answers `Err` with
    /// `StepRefusal::NameHeldByUploadSession` too; a read that fails, its refusal.
    /// `sent_hash` is the hash of the whole file (`LocalContentFile::hash`), which the
    /// runner reads for this call.
    pub(crate) async fn find_name_holder(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, sent_hash: Option<&str>) -> Result<NameHolder, StepOutcome> {
        let store_failure = |error: konedrive_tree::TreeError| StepOutcome::Failed(StepFailure::Store { detail: error.to_string() });
        let Some(local_place) = item.local.as_ref().and_then(|local| local.place.as_ref()) else {
            return Err(StepOutcome::Failed(StepFailure::Store { detail: format!("item {} was planned to be sent with no local place", item.local_item_id) }));
        };
        let place = ItemPlace { parent_local_item_id: local_place.parent_local_item_id, name: name.to_owned() };
        let holder = self.read_holder(parent, name).await?;
        let holder_item = holder.as_ref().and_then(|holder| to_confirmed_item(holder, &self.graph.drive_id).ok());
        let holder_reference = holder.as_ref().map(|holder| OneDriveItemReference { drive_id: self.graph.drive_id.clone(), item_id: OneDriveItemId::new(holder.id.clone()) });
        let read_place = place.clone();
        let (is_holder_bound, empty_file_sessions, opening_windows) = self
            .graph
            .store
            .read_items(move |items| {
                let is_holder_bound = match &holder_reference {
                    Some(reference) => items.find_bound_item(reference)?.is_some(),
                    None => false,
                };
                Ok((is_holder_bound, items.list_empty_file_sessions(&read_place)?, items.list_opening_windows(&read_place)?))
            })
            .await
            .map_err(store_failure)?;
        let now = (self.graph.clock)();
        let sessions_first_seen = self.note_sessions_seen(&empty_file_sessions, now);
        let facts = NameHolderFacts { holder, holder_item, is_holder_bound, sent_hash: sent_hash.map(str::to_owned), empty_file_sessions, opening_windows, sessions_first_seen };
        let name_holder = decide_name_holder(&facts, item, now);
        let is_place_resolved = match &name_holder {
            NameHolder::Nobody | NameHolder::OwnEarlierUpload(_) | NameHolder::UnknownEmptyFile | NameHolder::AnotherItem { .. } => true,
            NameHolder::OwnSession | NameHolder::AnotherItemsSession => false,
            NameHolder::AnotherItemsEndedSession { item: owner, address } => {
                let (owner, address) = (*owner, address.clone());
                crate::panic::lock(&self.sessions_first_seen).retain(|(seen, _)| *seen != address);
                self.graph.store.call(move |store| store.step_memory().forget_ended_upload_session(owner, &address)).await.map_err(store_failure)?;
                false
            }
            NameHolder::AbandonedSession(address) => {
                if !self.session_cancellations.cancel_session(address).await {
                    return Err(StepOutcome::Refused(StepRefusal::NameHeldByUploadSession));
                }
                false
            }
            NameHolder::OwnEmptyFile(empty_file) => {
                let guard = facts.holder.as_ref().map(to_guard_of_answer);
                let removed = {
                    let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
                    self.graph.drive.delete_item(empty_file.item_id.as_str(), guard.as_ref().map_or("", |guard| guard.as_str())).await
                };
                match removed {
                    Ok(()) | Err(WriteError::NotFound) => {
                        tracing::info!("the name of item {} was held in OneDrive by the empty file of an upload session this folder opened: removed", item.local_item_id);
                        true
                    }
                    Err(error) => {
                        return Err(StepOutcome::Refused(match decide_refusal(&error) {
                            // Refused for now: said as it is, and the openings stay.
                            RefusedRequest::Settled(refusal @ (StepRefusal::Throttled { .. } | StepRefusal::SignedOut)) => refusal,
                            RefusedRequest::AnswerLost { detail } => StepRefusal::Transient { detail },
                            // Not carried out: the step waits for the session to end by itself.
                            _ => StepRefusal::NameHeldByUploadSession,
                        }));
                    }
                }
            }
        };
        if is_place_resolved {
            self.graph.store.call(move |store| store.step_memory().forget_upload_session_openings_at(&place)).await.map_err(store_failure)?;
        }
        Ok(name_holder)
    }

    /// After a 409 on `SendMove`: the name of the item that holds `name` in `parent`, as
    /// OneDrive spells it (`child`), for `StepRefusal::NameTaken`.
    /// `None` when nothing holds it by now: the step answers `StepRefusal::Transient`,
    /// and its next try sends the move again. A read that fails is the step's outcome.
    /// Nothing is adopted here: a move that went through before is answered 412, not
    /// 409, and is `LostAnswers`'.
    pub(crate) async fn find_holder_name(&self, parent: &OneDriveItemReference, name: &OsStr) -> Result<Option<OsString>, StepOutcome> {
        Ok(self.read_holder(parent, name).await?.map(|holder| holder.name.map_or_else(|| name.to_owned(), OsString::from)))
    }
}

#[cfg(test)]
mod tests;

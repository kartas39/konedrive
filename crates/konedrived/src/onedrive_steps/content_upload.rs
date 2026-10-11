//! A file's content sent to OneDrive: an empty file by one request, any other through an
//! upload session, in fragments, gone on with after a stop or a crash. There is no size
//! below which a file with content goes another way: one up to `Limits::chunk` is the
//! session's one fragment.
//!
//! **State owned:** none in memory beyond the one upload it is sending; the session is
//! the step memory's (`UploadSession`: its address, how far it got, the content being
//! sent, and for a new file the place its empty file holds).
//!
//! **The place of a new file** is said twice, as its two readers have it: to Graph the
//! directory by its reference and the name ([`NewFilePlace`]); to the step memory an
//! `ItemPlace`, the directory by its local item id — that of the place of `item.local`,
//! whose binding the plan read `parent` off — and the name that is sent, a temporary one
//! too.
//!
//! **The order with the step memory** (its notes for the steps):
//! `remember_upload_session_opening(item, place, now)` BEFORE the request that opens a
//! new file's session — the first call of a try; on a certain answer with no session
//! `settle_answered_opening(item, opening_try)` — certain is every refusal but
//! `RefusedRequest::AnswerLost`, after which the opening stays and tells the empty file
//! that request may have left; with the address
//! `remember_upload_session(item, item.unsent_since, session)` BEFORE the first byte —
//! `Abandoned` means the row went or
//! the bytes changed, the cancel is queued already, nothing is sent, and the step
//! answers `StepOutcome::NotRun(StepObstacle::ContentChanged)`; after each
//! fragment `advance_upload_session` — `Abandoned` stops with the same outcome, and
//! `OfAnotherItem` (the address is another item's session: nothing was queued, the step
//! named the wrong item, which is a fault said in the journal) stops as
//! `StepOutcome::Failed(StepFailure::Store)`; a session given up (the content
//! changed between fragments, a fragment refused for good): `abandon_upload_session`; a
//! session found gone:
//! `forget_ended_upload_session(item, &address)`, which forgets the session the item's
//! memory keeps and no other. A completed upload calls nothing: the step's commit forgets
//! the session it is told ended ([`SentContent::ended_session`]).
//!
//! **An empty file is one request, and is on record** as every request of one answer is
//! (`konedrive_tree::RequestRecord`, written before `upload_empty` leaves): it goes when
//! OneDrive refuses for certain, and with the step's commit; an answer that says nothing
//! certain is read once (`LostAnswers::read_request_outcome`), and when that read says
//! nothing certain either the step ends `Transient` with the record standing. An upload
//! through a session writes none: the opening and the session are its record.
//!
//! **The pool's slot** is taken here, not by the plan's task, which only counts
//! (`step_choice::StepLimits`): `DriveClient::pool`, `Class::Upload`, by the content's size
//! (`Size::of`), before the first request of [`ContentUpload::send_content`] and held to
//! its end; an empty file's slot is let go with its one request. A read of `LostAnswers`
//! takes a `Class::Metadata` slot of its own.
//!
//! **Tested without the rest**, against the fixture's fake OneDrive (reference:
//! `upload/tests/sessions.rs`, `interrupted_sessions.rs`, `candidates.rs:65-156`):
// test: an_empty_file_is_sent_by_one_request_and_opens_no_session
// test: an_opening_is_kept_before_the_request_and_the_session_before_its_first_byte
// test: an_opening_whose_request_was_answered_for_certain_is_settled_and_one_whose_answer_was_lost_stays
// test: a_session_of_the_same_content_is_gone_on_with_from_where_onedrive_says_it_got
// test: a_session_whose_empty_file_is_elsewhere_is_gone_on_with_and_answers_that_place
// test: a_session_whose_empty_files_directory_has_no_binding_is_abandoned_and_a_new_one_opened
// test: a_hold_between_two_fragments_keeps_the_session
// test: a_full_onedrive_between_two_fragments_keeps_the_session
// test: a_file_changed_between_two_fragments_abandons_the_session
// test: a_fragment_refused_for_good_abandons_the_session
// test: a_session_found_gone_whose_content_is_in_onedrive_is_the_upload_that_went_through
// test: a_session_found_gone_otherwise_is_forgotten_and_said
// test: an_answer_with_another_hash_for_a_new_file_is_remembered_and_its_removal_asked
// test: an_answer_with_another_hash_for_an_existing_item_keeps_the_fact_of_the_own_upload
// test: the_guard_is_asked_again_before_the_last_fragment_of_an_existing_item
// test: a_changed_item_that_holds_the_hash_sent_is_the_upload_that_went_through
// test: the_hash_sent_is_that_of_the_bytes_read_for_the_fragments
// test: the_upload_slot_is_held_from_the_first_request_to_the_end

use std::ffi::OsString;
use std::sync::Arc;

use konedrive_graph::drive::{ChunkOutcome, DriveError, UploadTarget, WriteError};
use konedrive_graph::pool::{Class, Size, Slot};
use konedrive_graph::quickxor::QuickXor;
use konedrive_tree::{
    ConfirmedItem, ContentVersion, ItemMemory, ItemPlace, ItemVersions, MismatchedItem, MismatchedItemStanding, OneDriveItemReference, OwnUploadContent, RecordedRequest, RequestRecord, TreeError, UploadSession,
    UploadSessionAddress, UploadSessionStanding,
};

use super::confirmed_item::{to_confirmed_item, to_graph_name, to_guard_of_answer, RequestGuard};
use super::graph_refusal::{decide_plain_refusal, decide_read_refusal, decide_refusal, RefusedRequest};
use super::lost_answers::{FailedRead, LostAnswerFate, LostAnswers, SentRequest};
use super::GraphAccess;
use crate::mount::LocalContentFile;
use crate::sync_parts::onedrive_space::OneDriveSpace;
use crate::sync_parts::running_steps::{RunningSteps, StepClass};
use crate::sync_parts::sending_holds::SendingHolds;
use crate::sync_parts::step_outcome::{StepFailure, StepObstacle, StepOutcome, StepRefusal, StepTrouble};

/// Where in OneDrive a new file is sent: the directory, by its reference, and the name
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFilePlace {
    pub parent: OneDriveItemReference,
    pub name: OsString,
}

/// What a content is sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentTarget {
    /// A new file at this place, refused with a 409 when the name is taken there
    /// (`UploadTarget::New`).
    NewFile(NewFilePlace),
    /// The item OneDrive has (`item.onedrive_item`), guarded (`UploadTarget::Existing`).
    ExistingItem { guard: RequestGuard },
}

/// A content OneDrive has by now, as the step's commit takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentContent {
    /// The item as Graph answered the last request, or as it was read when the upload
    /// was found to have gone through before.
    pub item: ConfirmedItem,
    /// The QuickXorHash, base64, of the bytes that were read here for it.
    pub sent_hash: String,
    /// The upload session that ended with the upload: what the commit forgets. None for
    /// an empty file, and for an upload found in OneDrive with no session of the item
    /// kept.
    pub ended_session: Option<UploadSessionAddress>,
    /// For a new file: the place the request that made it named — the target's, or, for
    /// a session that was gone on with, the place of its empty file, whatever the file is
    /// called here by now. What `commit_send_new_file` takes as `parent` and `name`.
    /// None for an existing item.
    pub new_file_place: Option<NewFilePlace>,
}

/// How an upload of a content begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadStart {
    /// The content is empty: one request, no session.
    Empty,
    /// The step memory holds a session of this very content: it is asked how far it got
    /// and gone on with.
    Resume(UploadSession),
    /// No session of this content: one is opened.
    Open,
}

/// How the upload of a content of `size` bytes begins, from what the step memory still
/// says of the item. Pure. A session is resumed only for the full content version, change
/// count included (the map's F15): the step memory answers no other. Wherever the file
/// is by now: a session of a new file is gone on with at the place of its empty file.
pub fn decide_upload_start(memory: &ItemMemory, size: u64) -> UploadStart {
    match (&memory.upload_session, size) {
        (_, 0) => UploadStart::Empty,
        (Some(session), _) => UploadStart::Resume(session.clone()),
        (None, _) => UploadStart::Open,
    }
}

/// Whether an upload stops before its next fragment, and as what. Pure. While content is
/// held (`SendingHolds::is_held(StepClass::Content, now)`: a pause, a stop, the sign-out,
/// the write gate, a throttle): `StepOutcome::NotRun(StepObstacle::SendingStopped)`. For a
/// full OneDrive (`OneDriveSpace::is_full`), when nothing holds:
/// `StepOutcome::Refused(StepRefusal::NoSpace { free_space: None })`, which the settlement
/// writes nothing for. Either way the session is kept. Today's `stop_between_fragments`
/// (`upload/content/session.rs:57`).
pub fn decide_stop_between_fragments(is_content_held: bool, is_onedrive_full: bool) -> Option<StepOutcome> {
    match (is_content_held, is_onedrive_full) {
        (true, _) => Some(StepOutcome::NotRun(StepObstacle::SendingStopped)),
        (false, true) => Some(StepOutcome::Refused(StepRefusal::NoSpace { free_space: None })),
        (false, false) => None,
    }
}

/// What is done about an upload into an existing item that ended with other content in
/// OneDrive than was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnUploadFollowUp {
    /// The local content is sent again into the item, guarded by what the upload left:
    /// what today's worker does (`upload/content.rs:503-511`).
    SendAgain,
    /// OneDrive's content is taken as OneDrive's own change: the plan then reads "changed
    /// on both sides" and keeps both.
    TakeAsOneDrivesChange,
}

/// What is done about `upload`: it is sent again, as today's worker does (the owner's
/// decision), for a new file and for an existing item alike. The fact is kept
/// (`ItemStore::remember_own_upload_content`) and a listing cycle asked for, so that the
/// row has OneDrive's version and eTag, and the content is sent again guarded by them.
pub fn decide_own_upload_follow_up(_upload: &OwnUploadContent) -> OwnUploadFollowUp {
    OwnUploadFollowUp::SendAgain
}

/// Why a content was not sent to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendContentError {
    /// 409 on the request that makes a new file or opens its session, and on a fragment
    /// of a new file's session, which was given up then (`abandon_upload_session`): the
    /// runner asks who holds the name (`NameHolders::find_name_holder`) and, for the
    /// answers that say so, calls `send_content` once more — for `NameHolder::OwnSession`
    /// with the session the step memory holds, which it goes on in.
    NameExists,
    /// Anything else: the step's outcome.
    Outcome(StepOutcome),
}

impl From<StepOutcome> for SendContentError {
    fn from(outcome: StepOutcome) -> Self {
        SendContentError::Outcome(outcome)
    }
}

fn fail_store(error: TreeError) -> StepOutcome {
    StepOutcome::Failed(StepFailure::Store { detail: error.to_string() })
}

fn to_outcome(trouble: StepTrouble) -> StepOutcome {
    match trouble {
        StepTrouble::Obstacle(obstacle) => StepOutcome::NotRun(obstacle),
        StepTrouble::Failure(failure) => StepOutcome::Failed(failure),
    }
}

/// `len` bytes of `file` from `offset`, read off the runtime.
async fn read_fragment(file: &Arc<LocalContentFile>, offset: u64, len: usize) -> Result<Vec<u8>, StepTrouble> {
    let file = Arc::clone(file);
    tokio::task::spawn_blocking(move || file.read_fragment(offset, len)).await.unwrap_or_else(|error| Err(StepTrouble::Failure(StepFailure::Store { detail: format!("the read of a fragment ended: {error}") })))
}

/// The QuickXorHash of the whole of `file`, read off the runtime.
pub(super) async fn hash_content(file: &Arc<LocalContentFile>) -> Result<String, StepOutcome> {
    let file = Arc::clone(file);
    let hashed = tokio::task::spawn_blocking(move || file.hash()).await.unwrap_or_else(|error| Err(StepTrouble::Failure(StepFailure::Store { detail: format!("the hash of a content ended: {error}") })));
    hashed.map_err(to_outcome)
}

/// The place of a new file, when `target` is one.
fn find_new_file_place(target: &ContentTarget) -> Option<&NewFilePlace> {
    match target {
        ContentTarget::NewFile(place) => Some(place),
        ContentTarget::ExistingItem { .. } => None,
    }
}

/// What sends a file's content.
#[derive(Clone)]
pub struct ContentUpload {
    graph: GraphAccess,
    lost_answers: LostAnswers,
    /// Asked between two fragments and before a session is opened.
    sending_holds: Arc<SendingHolds>,
    onedrive_space: Arc<OneDriveSpace>,
    /// Told how far each upload got.
    running_steps: Arc<RunningSteps>,
    limits: Limits,
}

impl ContentUpload {
    pub fn new(graph: GraphAccess, lost_answers: LostAnswers, sending_holds: Arc<SendingHolds>, onedrive_space: Arc<OneDriveSpace>, running_steps: Arc<RunningSteps>, limits: Limits) -> Self {
        Self { graph, lost_answers, sending_holds, onedrive_space, running_steps, limits }
    }

    /// Whether the upload stops now ([`decide_stop_between_fragments`]).
    fn find_stop(&self) -> Option<StepOutcome> {
        decide_stop_between_fragments(self.sending_holds.is_held(StepClass::Content, (self.graph.clock)()), self.onedrive_space.is_full())
    }

    /// One job of the store's writer about the step memory, a failure of it the step's.
    async fn write_memory<T: Send + 'static>(&self, write: impl FnOnce(&mut konedrive_tree::StepMemory<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, StepOutcome> {
        self.graph.store.call(move |store| write(&mut store.step_memory())).await.map_err(fail_store)
    }

    /// The session of `item` is given up before it ended.
    async fn abandon_session(&self, item: &ItemVersions) -> Result<(), StepOutcome> {
        let id = item.local_item_id;
        self.write_memory(move |memory| memory.abandon_upload_session(id)).await
    }

    /// The record of the one request of `item` goes: OneDrive answered for certain.
    async fn forget_request_record(&self, item: &ItemVersions) -> Result<(), StepOutcome> {
        let id = item.local_item_id;
        self.write_memory(move |memory| memory.forget_request_record(id)).await
    }

    /// The id of the item of OneDrive `item` is sent into, in the folder's drive.
    fn find_existing_item_id(&self, item: &ItemVersions) -> Result<String, StepOutcome> {
        match &item.onedrive_item {
            Some(onedrive_item) if onedrive_item.drive_id == self.graph.drive_id => Ok(onedrive_item.item_id.as_str().to_owned()),
            Some(_) => Err(StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("item {} is in another drive than the folder's", item.local_item_id) })),
            None => Err(StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("item {} has no item of OneDrive to send its content into", item.local_item_id) })),
        }
    }

    /// What the step answers to a 404: the item for an existing one, the directory for a
    /// new file.
    fn decide_not_found(target: &ContentTarget) -> StepOutcome {
        StepOutcome::Refused(match target {
            ContentTarget::NewFile(_) => StepRefusal::DirectoryGone,
            ContentTarget::ExistingItem { .. } => StepRefusal::ItemGone,
        })
    }

    /// What the step does about a 409: the runner's for a new file, a changed item for an
    /// existing one, which names no place.
    fn decide_name_exists(target: &ContentTarget) -> SendContentError {
        match target {
            ContentTarget::NewFile(_) => SendContentError::NameExists,
            ContentTarget::ExistingItem { .. } => SendContentError::Outcome(StepOutcome::Refused(StepRefusal::ItemChanged)),
        }
    }

    /// The content of `sent_hash` found in OneDrive with no session to forget.
    fn describe_found_content(target: &ContentTarget, found: ConfirmedItem, sent_hash: String) -> SentContent {
        SentContent { item: found, sent_hash, ended_session: None, new_file_place: find_new_file_place(target).cloned() }
    }

    /// A 412 of an existing item: one read of the item. The hash of the whole file, there
    /// already, is this step's own earlier upload; anything else is `ItemChanged`.
    async fn follow_changed_item(&self, item: &ItemVersions, target: &ContentTarget, file: &Arc<LocalContentFile>) -> Result<SentContent, StepOutcome> {
        let sent_hash = hash_content(file).await?;
        match self.lost_answers.read_request_outcome(item, SentRequest::SendContent { sent_hash: &sent_hash }).await.map_err(FailedRead::into_outcome)? {
            LostAnswerFate::WentThrough(Some(found)) => Ok(Self::describe_found_content(target, found, sent_hash)),
            LostAnswerFate::WentThrough(None) | LostAnswerFate::DidNotGoThrough | LostAnswerFate::ItemChanged | LostAnswerFate::NameHeld { .. } => Err(StepOutcome::Refused(StepRefusal::ItemChanged)),
        }
    }

    /// Sends the content of `file` to `target` and answers what OneDrive has of it, as
    /// the step's commit takes it; or why not: a taken name, which the runner decides, or
    /// the step's outcome.
    ///
    /// Takes the pool's upload slot (the module's doc). Nothing is sent while OneDrive is
    /// full ([`decide_stop_between_fragments`], asked before the first request too).
    /// Reads the item's memory (`ItemReader::find_item_memory`), then
    /// [`decide_upload_start`]: `upload_empty` (`G/upload.rs:287`; today
    /// `Job::send_empty`, `upload/content/session.rs:178`), with the content's time; or
    /// `upload_status` (`G/upload.rs:252`; `Job::resume`, `:216`); or
    /// `create_upload_session` (`G/upload.rs:131`; `Job::open`, `:230`) — and then the
    /// fragments ([`send_fragments`](Self::send_fragments)). The step memory is written
    /// in the order the module says.
    ///
    /// A session of a new file that is gone on with names its own place
    /// (`UploadSession::empty_file_place`), not the target's: the directory's reference
    /// is read off the row of that place (`ItemReader::find_item`); a directory with no
    /// binding any more gives the session up (`abandon_upload_session`) and a new one is
    /// opened at the target.
    ///
    /// A refusal is read by `graph_refusal::decide_refusal`, and decided HERE, so that
    /// the runner is answered an outcome: a 507 by `OneDriveSpace::refuse_for_space`
    /// (with `file.size()`); a 412 of an existing item by one read of the item
    /// (`LostAnswers::read_request_outcome` with `SentRequest::SendContent`): the hash
    /// sent, there already, is this step's own earlier upload and is answered as sent
    /// (`upload/content.rs:400-403`), anything else is `StepRefusal::ItemChanged`; a 404
    /// is `ItemGone` for an existing item and `DirectoryGone` for a new file; a 409 of
    /// an existing item, which names no place, is `ItemChanged`; a gone
    /// session by [`follow_gone_session`](Self::follow_gone_session); an answer lost to
    /// `upload_empty` by one read of what became of it
    /// (`LostAnswers::read_request_outcome`), its record standing when that read says
    /// nothing certain either; an answer lost to the opening
    /// or to a fragment is `StepRefusal::Transient`, with the opening or the session
    /// kept, which the next try goes on from.
    ///
    /// Before it answers, the hash of the answer is asked
    /// ([`check_sent_content`](Self::check_sent_content)) — but for an upload found in
    /// OneDrive by its hash.
    ///
    /// Called by the runner's `run_send_new_file` and `run_send_content`.
    pub async fn send_content(&self, item: &ItemVersions, target: &ContentTarget, file: &Arc<LocalContentFile>) -> Result<SentContent, SendContentError> {
        let id = item.local_item_id;
        let size = file.size();
        if let ContentTarget::NewFile(place) = target {
            if place.parent.drive_id != self.graph.drive_id {
                return Err(StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("item {id} is sent into another drive than the folder's") }).into());
            }
        }
        let mut slot = self.graph.drive.pool().acquire_sized(Class::Upload, Size::of(size)).await;
        if let Some(outcome) = self.find_stop() {
            return Err(outcome.into());
        }
        let memory = self.graph.store.read_items(move |items| items.find_item_memory(id)).await.map_err(fail_store)?;
        self.running_steps.tell_progress(id, 0, size);
        let sent = match decide_upload_start(&memory, size) {
            UploadStart::Empty => return self.send_empty_content(item, target, file, slot).await,
            UploadStart::Resume(session) => match self.find_session_target(target, &session).await? {
                Some(session_target) => self.go_on_with_session(item, &session_target, file, session).await?,
                None => {
                    self.abandon_session(item).await?;
                    self.open_session(item, target, file).await?
                }
            },
            UploadStart::Open => self.open_session(item, target, file).await?,
        };
        // An upload found in OneDrive by its hash holds the hash sent: nothing to ask.
        if sent.ended_session.is_some() {
            self.check_sent_content(item, target, &sent).await?;
        }
        slot.succeeded();
        Ok(sent)
    }

    /// What the session `session` sends to: `target` for an existing item; for
    /// a new file the place of the session's empty file, the directory by the binding its
    /// row has now. None when that directory has no binding any more, or the session
    /// names no place: the session cannot be gone on with.
    async fn find_session_target(&self, target: &ContentTarget, session: &UploadSession) -> Result<Option<ContentTarget>, StepOutcome> {
        let ContentTarget::NewFile(_) = target else { return Ok(Some(target.clone())) };
        let Some(empty_file_place) = session.empty_file_place.clone() else { return Ok(None) };
        let directory = empty_file_place.parent_local_item_id;
        let parent = self.graph.store.read_items(move |items| Ok(items.find_item(directory)?.and_then(|row| row.onedrive_item))).await.map_err(fail_store)?;
        Ok(parent.filter(|parent| parent.drive_id == self.graph.drive_id).map(|parent| ContentTarget::NewFile(NewFilePlace { parent, name: empty_file_place.name })))
    }

    /// The target as Graph's client takes it.
    fn to_upload_target<'a>(target: &'a ContentTarget, existing_item_id: &'a str) -> Result<UploadTarget<'a>, StepOutcome> {
        match target {
            ContentTarget::NewFile(place) => Ok(UploadTarget::New { parent_id: place.parent.item_id.as_str(), name: to_graph_name(&place.name).map_err(StepOutcome::Refused)? }),
            ContentTarget::ExistingItem { guard } => Ok(UploadTarget::Existing { id: existing_item_id, if_match: guard.as_str() }),
        }
    }

    /// The id of the existing item `target` names, empty for a new file.
    fn find_target_item_id(&self, item: &ItemVersions, target: &ContentTarget) -> Result<String, StepOutcome> {
        match target {
            ContentTarget::NewFile(_) => Ok(String::new()),
            ContentTarget::ExistingItem { .. } => self.find_existing_item_id(item),
        }
    }

    /// The whole-seconds time of the content `item` sends.
    fn find_content_time(item: &ItemVersions) -> i64 {
        item.local.as_ref().and_then(|local| local.content.as_ref()).map_or(0, |content| content.mtime().0)
    }

    /// An empty content: one request, on record before it leaves.
    async fn send_empty_content(&self, item: &ItemVersions, target: &ContentTarget, file: &Arc<LocalContentFile>, mut slot: Slot) -> Result<SentContent, SendContentError> {
        let id = item.local_item_id;
        // Still empty, still the planned content, and no writer: as for any read.
        read_fragment(file, 0, 0).await.map_err(to_outcome)?;
        let sent_hash = QuickXor::new().finish_base64();
        let recorded = match target {
            ContentTarget::NewFile(place) => RecordedRequest::SendNewFile { parent: place.parent.clone(), name: place.name.clone(), sent_hash: sent_hash.clone() },
            ContentTarget::ExistingItem { .. } => RecordedRequest::SendContent { sent_hash: sent_hash.clone() },
        };
        let existing_item_id = self.find_target_item_id(item, target)?;
        let upload_target = Self::to_upload_target(target, &existing_item_id)?;
        let record = RequestRecord { item: id, onedrive_item: item.onedrive_item.clone(), request: recorded, sent_at: (self.graph.clock)() };
        self.write_memory(move |memory| memory.remember_request_record(&record)).await?;
        let request = match target {
            ContentTarget::NewFile(place) => SentRequest::SendNewFile { parent: &place.parent, name: &place.name, sent_hash: &sent_hash },
            ContentTarget::ExistingItem { .. } => SentRequest::SendContent { sent_hash: &sent_hash },
        };
        let lost_detail = match self.graph.drive.upload_empty(upload_target, Self::find_content_time(item)).await {
            Ok(answer) => match to_confirmed_item(&answer, &self.graph.drive_id) {
                Ok(made) => {
                    slot.succeeded();
                    return Ok(SentContent { item: made, sent_hash: sent_hash.clone(), ended_session: None, new_file_place: find_new_file_place(target).cloned() });
                }
                // The request went through and nothing readable says as what: read as a lost answer.
                Err(failure) => format!("{failure:?}"),
            },
            Err(error) => match decide_refusal(&error) {
                RefusedRequest::AnswerLost { detail } => detail,
                refused => {
                    drop(slot);
                    // A 412 may be this step's own earlier request: the record stays until the item was read.
                    if refused == RefusedRequest::Changed {
                        let followed = self.follow_changed_item(item, target, file).await;
                        if followed.is_err() {
                            self.forget_request_record(item).await?;
                        }
                        return Ok(followed?);
                    }
                    self.forget_request_record(item).await?;
                    return Err(match refused {
                        RefusedRequest::NameExists => Self::decide_name_exists(target),
                        RefusedRequest::NotFound => Self::decide_not_found(target).into(),
                        RefusedRequest::NoSpace => StepOutcome::Refused(self.onedrive_space.refuse_for_space(&self.graph.drive, file.size(), (self.graph.clock)()).await).into(),
                        RefusedRequest::Settled(refusal) => StepOutcome::Refused(refusal).into(),
                        RefusedRequest::SessionGone | RefusedRequest::Changed | RefusedRequest::AnswerLost { .. } => StepOutcome::Refused(decide_plain_refusal(&error)).into(),
                    });
                }
            },
        };
        drop(slot);
        // Nothing certain was answered: what became of the request is read once.
        let transient = || StepOutcome::Refused(StepRefusal::Transient { detail: lost_detail.clone() });
        let fate = match self.lost_answers.read_request_outcome(item, request).await {
            Ok(fate) => fate,
            // The record stands: its item stays busy until it is settled.
            Err(FailedRead::NothingCertain(_)) => return Err(transient().into()),
            // Reading will never say what became of the request: the record goes, and a listing brings what OneDrive has.
            Err(FailedRead::RefusedForCertain(outcome)) => {
                tracing::warn!("what became of the empty file sent for item {id} cannot be read from OneDrive ({outcome:?}): a listing is what is left");
                self.forget_request_record(item).await?;
                return Err(StepOutcome::Refused(StepRefusal::ItemChanged).into());
            }
        };
        match fate {
            LostAnswerFate::WentThrough(Some(found)) => Ok(Self::describe_found_content(target, found, sent_hash.clone())),
            LostAnswerFate::WentThrough(None) | LostAnswerFate::DidNotGoThrough => {
                self.forget_request_record(item).await?;
                Err(transient().into())
            }
            LostAnswerFate::ItemChanged => {
                self.forget_request_record(item).await?;
                Err(StepOutcome::Refused(StepRefusal::ItemChanged).into())
            }
            LostAnswerFate::NameHeld { .. } => {
                self.forget_request_record(item).await?;
                Err(Self::decide_name_exists(target))
            }
        }
    }

    /// Asks OneDrive how far `session` got and goes on with it.
    async fn go_on_with_session(&self, item: &ItemVersions, target: &ContentTarget, file: &Arc<LocalContentFile>, mut session: UploadSession) -> Result<SentContent, SendContentError> {
        match self.graph.drive.upload_status(session.address.as_str()).await {
            Ok(progress) => {
                session.next_offset = progress.next;
                session.expires_at = progress.expires.or(session.expires_at);
                self.send_fragments(item, target, file, session, false).await
            }
            Err(WriteError::SessionGone) => Ok(self.follow_gone_session(item, target, file, &session).await?),
            Err(error) => {
                let refusal = decide_plain_refusal(&error);
                // Refused for good: nothing goes on with this session. Any other refusal keeps it.
                if matches!(refusal, StepRefusal::RefusedForGood { .. }) {
                    self.abandon_session(item).await?;
                }
                Err(StepOutcome::Refused(refusal).into())
            }
        }
    }

    /// Opens a session for the content at `target`, keeps it before its first byte, and
    /// sends the fragments.
    async fn open_session(&self, item: &ItemVersions, target: &ContentTarget, file: &Arc<LocalContentFile>) -> Result<SentContent, SendContentError> {
        let id = item.local_item_id;
        let Some(content) = item.local.as_ref().and_then(|local| local.content.clone()) else { return Err(StepOutcome::NotRun(StepObstacle::ContentChanged).into()) };
        let existing_item_id = self.find_target_item_id(item, target)?;
        let upload_target = Self::to_upload_target(target, &existing_item_id)?;
        // The place as the step memory names it: the directory of the item's local place, under the name sent.
        let empty_file_place = match target {
            ContentTarget::NewFile(place) => {
                let Some(local_place) = item.local.as_ref().and_then(|local| local.place.as_ref()) else {
                    return Err(StepOutcome::Failed(StepFailure::Store { detail: format!("item {id} was planned to be sent with no local place") }).into());
                };
                Some(ItemPlace { parent_local_item_id: local_place.parent_local_item_id, name: place.name.clone() })
            }
            ContentTarget::ExistingItem { .. } => None,
        };
        let opening_try = match empty_file_place.clone() {
            Some(place) => {
                let now = (self.graph.clock)();
                Some(self.write_memory(move |memory| memory.remember_upload_session_opening(id, &place, now)).await?)
            }
            None => None,
        };
        let opened = match self.graph.drive.create_upload_session(upload_target, content.mtime().0).await {
            Ok(opened) => opened,
            Err(error) => {
                let refused = decide_refusal(&error);
                // A certain answer: this request left no empty file.
                if let (Some(opening_try), false) = (opening_try, matches!(refused, RefusedRequest::AnswerLost { .. })) {
                    self.write_memory(move |memory| memory.settle_answered_opening(id, opening_try)).await?;
                }
                return match refused {
                    RefusedRequest::NameExists => Err(Self::decide_name_exists(target)),
                    RefusedRequest::Changed => Ok(self.follow_changed_item(item, target, file).await?),
                    RefusedRequest::NotFound => Err(Self::decide_not_found(target).into()),
                    RefusedRequest::NoSpace => Err(StepOutcome::Refused(self.onedrive_space.refuse_for_space(&self.graph.drive, file.size(), (self.graph.clock)()).await).into()),
                    RefusedRequest::Settled(refusal) => Err(StepOutcome::Refused(refusal).into()),
                    RefusedRequest::SessionGone | RefusedRequest::AnswerLost { .. } => Err(StepOutcome::Refused(decide_plain_refusal(&error)).into()),
                };
            }
        };
        let session = UploadSession { address: UploadSessionAddress::new(opened.url), expires_at: opened.expires, next_offset: 0, content, empty_file_place };
        let (kept, unsent_since) = (session.clone(), item.unsent_since);
        match self.write_memory(move |memory| memory.remember_upload_session(id, unsent_since, &kept)).await? {
            UploadSessionStanding::Continued => self.send_fragments(item, target, file, session, true).await,
            // The row went or the bytes changed: the cancel is queued already.
            UploadSessionStanding::Abandoned => Err(StepOutcome::NotRun(StepObstacle::ContentChanged).into()),
            UploadSessionStanding::OfAnotherItem => {
                tracing::error!("the upload session opened for item {id} is kept as another item's");
                Err(StepOutcome::Failed(StepFailure::Store { detail: format!("the upload session opened for item {id} is another item's") }).into())
            }
        }
    }

    /// Whether the item an existing-item upload goes into is still what the guard names:
    /// the refusal its opening would get now, when it is not.
    async fn find_broken_guard(&self, existing_item_id: &str, guard: &RequestGuard) -> Result<Option<RefusedRequest>, StepOutcome> {
        match self.graph.drive.item(existing_item_id).await {
            Ok(found) if found.e_tag.as_deref() != Some(guard.as_str()) && found.c_tag.as_deref() != Some(guard.as_str()) => Ok(Some(RefusedRequest::Changed)),
            Ok(_) => Ok(None),
            Err(DriveError::NotFound) => Ok(Some(RefusedRequest::NotFound)),
            Err(error) => Err(StepOutcome::Refused(decide_read_refusal(error))),
        }
    }

    /// Sends the fragments of `file` from `session.next_offset` on: `upload_chunk`
    /// (`G/upload.rs:190`; `Job::fragment`, `upload/content/session.rs:285`), each of at
    /// most `Limits::chunk` bytes, read by `LocalContentFile::read_fragment` off the
    /// runtime, each followed by `StepMemory::advance_upload_session` and the progress
    /// told (`RunningSteps::tell_progress`). The hash sent is that of the bytes read
    /// here: for a session gone on with, the bytes before `next_offset` are read again
    /// for it, and it starts over wherever OneDrive does not stand at the end of what was
    /// hashed. Between two fragments, the first too:
    /// [`decide_stop_between_fragments`] with `SendingHolds::is_held` of content and
    /// `OneDriveSpace::is_full`. Before the last fragment of an existing item the guard is
    /// asked again (`item`, `G/mod.rs:217`; `Job::guard_broken`, `:381`) — but when the
    /// opening was the request just before (`is_guard_asked`): another eTag and
    /// cTag than the guard, or no item, gives the session up and is the refusal the
    /// opening would get now.
    ///
    /// What ends it otherwise: a read that says the file changed or is open for writing
    /// (`StepTrouble`) — the session is given up for a changed file, kept for a writer,
    /// and the trouble is the outcome; a fragment refused 409, 412, 404 or for good (a
    /// 400) gives the session up, and is `Err(NameExists)` for a new file's 409, `ItemChanged` for an
    /// existing item's 409, and the refusal's outcome otherwise; a session that expects bytes at or past the file's end is given up and
    /// is `StepFailure::AnswerUnreadable`; an answer that takes nothing (OneDrive still
    /// expects the same offset) is `StepRefusal::Transient`, the session kept; a session
    /// answered gone: [`follow_gone_session`](Self::follow_gone_session); any other
    /// refusal keeps the session for the next try.
    async fn send_fragments(&self, item: &ItemVersions, target: &ContentTarget, file: &Arc<LocalContentFile>, session: UploadSession, mut is_guard_asked: bool) -> Result<SentContent, SendContentError> {
        let id = item.local_item_id;
        let (size, fragment_size) = (file.size(), self.limits.chunk.max(1));
        let existing_item_id = self.find_target_item_id(item, target)?;
        let address = session.address.clone();
        let (mut hasher, mut hashed, mut next) = (QuickXor::new(), 0u64, session.next_offset);
        // What a read that failed makes of the session and answers.
        let follow_trouble = |trouble: StepTrouble| async move {
            if matches!(trouble, StepTrouble::Obstacle(StepObstacle::ContentChanged)) {
                self.abandon_session(item).await?;
            }
            Err::<Vec<u8>, StepOutcome>(to_outcome(trouble))
        };
        loop {
            self.running_steps.tell_progress(id, next, size);
            if let Some(outcome) = self.find_stop() {
                tracing::info!("the upload of item {id} stops at {next} of {size} bytes; its session is kept");
                return Err(outcome.into());
            }
            if next >= size {
                self.abandon_session(item).await?;
                return Err(StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("the upload session expects bytes from {next} of {size}") }).into());
            }
            let len = (size - next).min(fragment_size);
            if next + len >= size && !is_guard_asked {
                if let ContentTarget::ExistingItem { guard } = target {
                    match self.find_broken_guard(&existing_item_id, guard).await? {
                        None => {}
                        Some(RefusedRequest::NotFound) => {
                            self.abandon_session(item).await?;
                            return Err(Self::decide_not_found(target).into());
                        }
                        Some(_) => {
                            self.abandon_session(item).await?;
                            return Ok(self.follow_changed_item(item, target, file).await?);
                        }
                    }
                }
            }
            // The hash follows OneDrive: it starts over wherever the session does not stand at the end of what was read for it.
            if hashed != next {
                (hasher, hashed) = (QuickXor::new(), 0);
                while hashed < next {
                    let hashed_len = (next - hashed).min(fragment_size);
                    let bytes = match read_fragment(file, hashed, hashed_len as usize).await {
                        Ok(bytes) => bytes,
                        Err(trouble) => follow_trouble(trouble).await?,
                    };
                    hasher.update(&bytes);
                    hashed += hashed_len;
                }
            }
            let bytes = match read_fragment(file, next, len as usize).await {
                Ok(bytes) => bytes,
                Err(trouble) => follow_trouble(trouble).await?,
            };
            hasher.update(&bytes);
            hashed = next + len;
            match self.graph.drive.upload_chunk(address.as_str(), next, size, bytes).await {
                // An answer that takes nothing: the session stays, to be gone on with.
                Ok(ChunkOutcome::More(progress)) if progress.next <= next => {
                    return Err(StepOutcome::Refused(StepRefusal::Transient { detail: format!("the upload session still expects bytes from {} of {size}", progress.next) }).into());
                }
                Ok(ChunkOutcome::More(progress)) => {
                    let (advanced, next_offset, expires_at) = (address.clone(), progress.next, progress.expires);
                    match self.write_memory(move |memory| memory.advance_upload_session(id, &advanced, next_offset, expires_at)).await? {
                        UploadSessionStanding::Continued => {}
                        UploadSessionStanding::Abandoned => return Err(StepOutcome::NotRun(StepObstacle::ContentChanged).into()),
                        UploadSessionStanding::OfAnotherItem => {
                            tracing::error!("the upload session item {id} sends to is kept as another item's");
                            return Err(StepOutcome::Failed(StepFailure::Store { detail: format!("the upload session of item {id} is another item's") }).into());
                        }
                    }
                    (next, is_guard_asked) = (progress.next, false);
                }
                Ok(ChunkOutcome::Done(answer)) => {
                    self.running_steps.tell_progress(id, size, size);
                    return match to_confirmed_item(&answer, &self.graph.drive_id) {
                        Ok(made) => Ok(SentContent { item: made, sent_hash: hasher.finish_base64(), ended_session: Some(address), new_file_place: find_new_file_place(target).cloned() }),
                        // The session ended and nothing readable says as what: the item is read.
                        Err(_) => Ok(self.follow_gone_session(item, target, file, &session).await?),
                    };
                }
                Err(WriteError::SessionGone) => return Ok(self.follow_gone_session(item, target, file, &session).await?),
                Err(error) => {
                    return match decide_refusal(&error) {
                        RefusedRequest::NameExists => {
                            self.abandon_session(item).await?;
                            Err(Self::decide_name_exists(target))
                        }
                        RefusedRequest::Changed => {
                            self.abandon_session(item).await?;
                            Ok(self.follow_changed_item(item, target, file).await?)
                        }
                        RefusedRequest::NotFound => {
                            self.abandon_session(item).await?;
                            Err(Self::decide_not_found(target).into())
                        }
                        // Refused for good: the session is given up with the fragment, so that its empty file holds no name.
                        RefusedRequest::Settled(refusal @ StepRefusal::RefusedForGood { .. }) => {
                            self.abandon_session(item).await?;
                            Err(StepOutcome::Refused(refusal).into())
                        }
                        // The session stays, to be gone on with.
                        RefusedRequest::NoSpace => Err(StepOutcome::Refused(self.onedrive_space.refuse_for_space(&self.graph.drive, size, (self.graph.clock)()).await).into()),
                        RefusedRequest::Settled(refusal) => Err(StepOutcome::Refused(refusal).into()),
                        RefusedRequest::SessionGone | RefusedRequest::AnswerLost { .. } => Err(StepOutcome::Refused(decide_plain_refusal(&error)).into()),
                    };
                }
            }
        }
    }

    /// The session is gone in OneDrive: it is forgotten
    /// (`forget_ended_upload_session`); when the content sent is there already — the
    /// item for an existing one, the file at the session's own place for a new one,
    /// compared by the hash of the whole file (`LocalContentFile::hash`) —, it is the
    /// upload that went through (`LostAnswers::read_request_outcome`; `Job::ended`,
    /// `upload/content/session.rs:397`), answered with no session to forget; otherwise
    /// the step answers `StepRefusal::UploadSessionGone`, and its next try opens a new
    /// session (today's worker opened one in the same run, once).
    async fn follow_gone_session(&self, item: &ItemVersions, target: &ContentTarget, file: &Arc<LocalContentFile>, session: &UploadSession) -> Result<SentContent, StepOutcome> {
        let (id, ended) = (item.local_item_id, session.address.clone());
        self.write_memory(move |memory| memory.forget_ended_upload_session(id, &ended)).await?;
        let sent_hash = hash_content(file).await?;
        let request = match target {
            ContentTarget::NewFile(place) => SentRequest::SendNewFile { parent: &place.parent, name: &place.name, sent_hash: &sent_hash },
            ContentTarget::ExistingItem { .. } => SentRequest::SendContent { sent_hash: &sent_hash },
        };
        match self.lost_answers.read_request_outcome(item, request).await.map_err(FailedRead::into_outcome)? {
            LostAnswerFate::WentThrough(Some(found)) => Ok(Self::describe_found_content(target, found, sent_hash)),
            LostAnswerFate::WentThrough(None) | LostAnswerFate::DidNotGoThrough | LostAnswerFate::ItemChanged | LostAnswerFate::NameHeld { .. } => {
                tracing::info!("the upload session of item {id} ended with its content not in OneDrive: the upload starts over");
                Err(StepOutcome::Refused(StepRefusal::UploadSessionGone))
            }
        }
    }

    /// Asks that `sent.item` holds the hash of the content sent (`Job::finish`,
    /// `upload/content.rs:465`); an answer that carries no hash is taken as sent. When it
    /// holds another, nothing is committed and the step answers
    /// `StepRefusal::ContentMismatch`, after: for a new file the made item
    /// remembered (`StepMemory::remember_mismatched_item`, with
    /// `MismatchedItem::of_answer` of the answer's cTag and eTag) BEFORE its removal is
    /// asked (`delete_item`, `G/write.rs:140`, guarded by the answer's eTag) — and its
    /// removal is asked only when the store
    /// answers `MismatchedItemStanding::LeftToRemove`: `OfFolder` says the folder has
    /// that item by now, and nothing is removed; removed or found gone, it is forgotten
    /// with the OneDrive version of a row a listing made of it
    /// (`StepMemory::forget_removed_mismatched_item`); a removal that fails leaves it
    /// remembered, and the plan is told of it (`SyncPlanInput::items_left_by_uploads`).
    /// For an existing item the fact is kept
    /// (`ItemStore::remember_own_upload_content`: OneDrive's content of the answer, the
    /// local content sent). What follows is
    /// [`decide_own_upload_follow_up`]'s: the content is sent again. The session that
    /// ended is forgotten here (`forget_ended_upload_session`): no commit will.
    async fn check_sent_content(&self, item: &ItemVersions, target: &ContentTarget, sent: &SentContent) -> Result<(), StepOutcome> {
        let id = item.local_item_id;
        let (answered_hash, answered_ctag) = match &sent.item.content {
            Some(ContentVersion::Hashed { hash, ctag, .. }) => (hash.as_deref(), ctag.as_deref()),
            Some(ContentVersion::ChangedHere { .. }) | None => (None, None),
        };
        if answered_hash.is_none_or(|answered| answered == sent.sent_hash) {
            return Ok(());
        }
        tracing::warn!("OneDrive holds other content than was sent for item {id}: it is sent again");
        if let Some(ended) = sent.ended_session.clone() {
            self.write_memory(move |memory| memory.forget_ended_upload_session(id, &ended)).await?;
        }
        match target {
            ContentTarget::NewFile(_) => {
                let left = MismatchedItem::of_answer(sent.item.item.clone(), answered_ctag, Some(sent.item.etag.as_str()));
                // The removal is asked also when the store fails: what cannot be remembered must not stay there.
                let standing = self.write_memory(move |memory| memory.remember_mismatched_item(id, &left)).await;
                if let Err(outcome) = &standing {
                    tracing::warn!("what OneDrive holds for item {id} with other content is not remembered: {outcome:?}");
                }
                if !matches!(standing, Ok(MismatchedItemStanding::OfFolder)) {
                    match self.graph.drive.delete_item(sent.item.item.item_id.as_str(), &sent.item.etag).await {
                        Ok(()) | Err(WriteError::NotFound) => {
                            let removed = sent.item.item.clone();
                            self.write_memory(move |memory| memory.forget_removed_mismatched_item(&removed)).await?;
                        }
                        Err(error) => {
                            tracing::warn!("what OneDrive holds for item {id} with other content is not removed ({error}): it is removed before the file is sent again");
                            // Neither removed nor remembered: the store's failure is the step's.
                            standing.map(|_| ())?;
                        }
                    }
                }
            }
            ContentTarget::ExistingItem { .. } => {
                let contents = (sent.item.content.clone(), item.local.as_ref().and_then(|local| local.content.clone()));
                if let (Some(onedrive_content), Some(local_content)) = contents {
                    let upload = OwnUploadContent { onedrive_content, local_content };
                    match decide_own_upload_follow_up(&upload) {
                        // The fact is the plan's to read (`SyncPlanInput::own_upload_contents`, filled from `ItemReader::find_own_upload_content`): with it the listing that brings what this upload left is no change of OneDrive's, and the content is sent again with no conflict copy.
                        OwnUploadFollowUp::SendAgain | OwnUploadFollowUp::TakeAsOneDrivesChange => {
                            self.graph.store.call(move |store| store.item_store().remember_own_upload_content(id, &upload)).await.map_err(fail_store)?;
                        }
                    }
                }
            }
        }
        Err(StepOutcome::Refused(StepRefusal::ContentMismatch))
    }

    /// Before a new file is sent: the item an earlier upload of this item left with other
    /// content (`memory.mismatched_item`; nothing to do with none) is read (`item`,
    /// `G/mod.rs:217`) and: adopted — answered as sent, with no session and at the place
    /// OneDrive has it — when it holds the hash of `file` after all and no row is bound
    /// to it (`ItemReader::find_bound_item`); removed (`delete_item`, guarded by the
    /// eTag just read) when it is unchanged since that upload
    /// (`MismatchedItem::is_unchanged_since_upload`); left where it is when another
    /// changed it since, or the removal is answered 412. Forgotten in each of these cases
    /// and when it is found gone — removed or gone by
    /// `StepMemory::forget_removed_mismatched_item`, which takes the OneDrive version of a
    /// row a listing made of it; adopted or another's by `forget_mismatched_item`, by the
    /// item's reference, drive and id —; a read or a removal that fails otherwise leaves it
    /// remembered and is the step's outcome. Today `Job::clear_bad_item`
    /// (`upload/content.rs:165`).
    pub async fn remove_mismatched_item(&self, item: &ItemVersions, memory: &ItemMemory, file: &Arc<LocalContentFile>) -> Result<Option<SentContent>, StepOutcome> {
        let Some(left) = memory.mismatched_item.clone() else { return Ok(None) };
        // Removed or gone: the row a listing made of it loses its OneDrive version with it. Another's by now, or adopted: OneDrive has it still.
        let forget = |is_gone: bool| {
            let forgotten = left.item.clone();
            self.write_memory(move |memory| if is_gone { memory.forget_removed_mismatched_item(&forgotten) } else { memory.forget_mismatched_item(&forgotten) })
        };
        if left.item.drive_id != self.graph.drive_id {
            return Err(StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("what an upload of item {} left is in another drive than the folder's", item.local_item_id) }));
        }
        let left_id = left.item.item_id.as_str();
        // A slot around each request only: the hash between them reads the whole file.
        let read = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            self.graph.drive.item(left_id).await
        };
        let found = match read {
            Ok(found) => found,
            Err(DriveError::NotFound) => {
                forget(true).await?;
                return Ok(None);
            }
            Err(error) => return Err(StepOutcome::Refused(decide_read_refusal(error))),
        };
        let sent_hash = hash_content(file).await?;
        if found.quick_xor_hash() == Some(sent_hash.as_str()) {
            let bound = left.item.clone();
            let is_bound = self.graph.store.read_items(move |items| Ok(items.find_bound_item(&bound)?.is_some())).await.map_err(fail_store)?;
            if !is_bound {
                let adopted = to_confirmed_item(&found, &self.graph.drive_id).map_err(StepOutcome::Failed)?;
                forget(false).await?;
                let new_file_place = Some(NewFilePlace { parent: adopted.parent.clone(), name: adopted.name.clone() });
                return Ok(Some(SentContent { item: adopted, sent_hash, ended_session: None, new_file_place }));
            }
        }
        if !left.is_unchanged_since_upload(found.c_tag.as_deref(), found.e_tag.as_deref()) {
            tracing::info!("what OneDrive holds for item {} with other content was changed there since: it is left", item.local_item_id);
            forget(false).await?;
            return Ok(None);
        }
        let removed = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            self.graph.drive.delete_item(left_id, to_guard_of_answer(&found).as_str()).await
        };
        match removed {
            Ok(()) | Err(WriteError::NotFound) => {
                forget(true).await?;
                Ok(None)
            }
            // A 412: another changed it between the read and the removal, and it is left.
            Err(WriteError::Changed) => {
                forget(false).await?;
                Ok(None)
            }
            Err(error) => Err(StepOutcome::Refused(decide_plain_refusal(&error))),
        }
    }
}

/// How files are cut (provisional numbers; the tests make them small). How many
/// run at once is the account's transfer pool's (`konedrive_graph::pool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The fragment a file goes up in, a multiple of 320 KiB: a file up to
    /// this size is one fragment.
    pub chunk: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { chunk: konedrive_graph::drive::CHUNK_SIZE }
    }
}

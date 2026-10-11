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
//! **The pool's slot** is taken here, not by the plan's task, which only counts
//! (`step_choice::StepLimits`): `DriveClient::pool`, `Class::Upload`, by the content's size
//! (`Size::of`), before the first request of [`ContentUpload::send_content`] and held to
//! its end — but let go before `LostAnswers::settle_lost_answer` is called, which waits
//! with no slot and takes one for each of its reads.
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
#![allow(dead_code, unused_variables)]

use std::ffi::OsString;
use std::sync::Arc;

use konedrive_tree::{ConfirmedItem, ItemMemory, ItemVersions, OneDriveItemReference, OwnUploadContent, UploadSession, UploadSessionAddress};

use super::confirmed_item::RequestGuard;
use super::lost_answers::LostAnswers;
use super::GraphAccess;
use crate::folder::running_steps::RunningSteps;
use crate::folder::step_outcome::StepOutcome;
use crate::fuse::LocalContentFile;
use crate::upload::onedrive_space::OneDriveSpace;
use crate::upload::sending_holds::SendingHolds;
use crate::upload::Limits;

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
    todo!()
}

/// Whether an upload stops before its next fragment, and as what. Pure. While content is
/// held (`SendingHolds::is_held(StepClass::Content, now)`: a pause, a stop, the sign-out,
/// the write gate, a throttle): `StepOutcome::NotRun(StepObstacle::SendingStopped)`. For a
/// full OneDrive (`OneDriveSpace::is_full`), when nothing holds:
/// `StepOutcome::Refused(StepRefusal::NoSpace { free_space: None })`, which the settlement
/// writes nothing for. Either way the session is kept. Today's `stop_between_fragments`
/// (`upload/content/session.rs:57`).
pub fn decide_stop_between_fragments(is_content_held: bool, is_onedrive_full: bool) -> Option<StepOutcome> {
    todo!()
}

/// What is done about an upload into an existing item that ended with other content in
/// OneDrive than was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnUploadFollowUp {
    /// The local content is sent again into the item, guarded by what the upload left:
    /// what today's worker does (`upload/content.rs:503-511`).
    SendAgain,
    /// OneDrive's content is taken as OneDrive's own change: the plan then reads "changed
    /// on both sides" and keeps both — right if OneDrive rewrites what it is sent
    /// (design §13, not measured for OneDrive for Business), and then once per upload.
    TakeAsOneDrivesChange,
}

/// What is done about `upload`. Written as today's worker does it: `SendAgain`.
// TODO: the owner's, after design §13's measurement on OneDrive for Business: `SendAgain` or `TakeAsOneDrivesChange`. Either way the fact is kept (`ItemStore::remember_own_upload_content`) and a listing cycle asked for, so that the row has OneDrive's version and eTag; and with `SendAgain` the plan must be told the fact to not make a conflict copy against this folder's own upload — an input of the plan that does not exist yet. The same measurement decides a NEW file: where OneDrive rewrites what it is sent, a new file is uploaded, found with another hash, removed and refused at every try, for ever (`check_sent_content`); say what a new file's mismatch is then.
pub fn decide_own_upload_follow_up(upload: &OwnUploadContent) -> OwnUploadFollowUp {
    todo!()
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
    /// `upload_empty` by `LostAnswers::settle_lost_answer`; an answer lost to the opening
    /// or to a fragment is `StepRefusal::Transient`, with the opening or the session
    /// kept, which the next try goes on from.
    ///
    /// Before it answers, the hash of the answer is asked
    /// ([`check_sent_content`](Self::check_sent_content)) — but for an upload found in
    /// OneDrive by its hash.
    ///
    /// Called by the runner's `run_send_new_file` and `run_send_content`.
    pub async fn send_content(&self, item: &ItemVersions, target: &ContentTarget, file: &LocalContentFile) -> Result<SentContent, SendContentError> {
        todo!()
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
    /// asked again (`item`, `G/mod.rs:217`; `Job::guard_broken`, `:381`): another eTag and
    /// cTag than the guard, or no item, gives the session up and is the refusal the
    /// opening would get now.
    ///
    /// What ends it otherwise: a read that says the file changed or is open for writing
    /// (`StepTrouble`) — the session is given up for a changed file, kept for a writer,
    /// and the trouble is the outcome; a fragment refused 409, 412 or 404 gives the
    /// session up, and is `Err(NameExists)` for a new file's 409, `ItemChanged` for an
    /// existing item's 409, and the refusal's outcome otherwise; a session that expects bytes at or past the file's end is given up and
    /// is `StepFailure::AnswerUnreadable`; an answer that takes nothing (OneDrive still
    /// expects the same offset) is `StepRefusal::Transient`, the session kept; a session
    /// answered gone: [`follow_gone_session`](Self::follow_gone_session); any other
    /// refusal keeps the session for the next try.
    async fn send_fragments(&self, item: &ItemVersions, target: &ContentTarget, file: &LocalContentFile, session: UploadSession) -> Result<SentContent, SendContentError> {
        todo!()
    }

    /// The session is gone in OneDrive: it is forgotten
    /// (`forget_ended_upload_session`); when the content sent is there already — the
    /// item for an existing one, the file at the session's own place for a new one,
    /// compared by the hash of the whole file (`LocalContentFile::hash`) —, it is the
    /// upload that went through (`LostAnswers::read_request_outcome`; `Job::ended`,
    /// `upload/content/session.rs:397`), answered with no session to forget; otherwise
    /// the step answers `StepRefusal::UploadSessionGone`, and its next try opens a new
    /// session (today's worker opened one in the same run, once).
    async fn follow_gone_session(&self, item: &ItemVersions, target: &ContentTarget, file: &LocalContentFile, session: &UploadSession) -> Result<SentContent, StepOutcome> {
        todo!()
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
    /// (`StepMemory::forget_mismatched_item`); a removal that fails leaves it
    /// remembered, and the plan is told of it (`SyncPlanInput::items_left_by_uploads`).
    /// For an existing item the fact is kept
    /// (`ItemStore::remember_own_upload_content`: OneDrive's content of the answer, the
    /// local content sent). What follows for an existing item is
    /// [`decide_own_upload_follow_up`]'s. The session that ended is forgotten here
    /// (`forget_ended_upload_session`): no commit will.
    async fn check_sent_content(&self, item: &ItemVersions, sent: &SentContent) -> Result<(), StepOutcome> {
        todo!()
    }

    /// Before a new file is sent: the item an earlier upload of this item left with other
    /// content (`memory.mismatched_item`; nothing to do with none) is read (`item`,
    /// `G/mod.rs:217`) and: adopted — answered as sent, with no session and at the place
    /// OneDrive has it — when it holds the hash of `file` after all and no row is bound
    /// to it (`ItemReader::find_bound_item`); removed (`delete_item`, guarded by the
    /// eTag just read) when it is unchanged since that upload
    /// (`MismatchedItem::is_unchanged_since_upload`); left where it is when another
    /// changed it since, or the removal is answered 412. Forgotten in each of these cases
    /// and when it is found gone (`StepMemory::forget_mismatched_item`, by the item's
    /// reference, drive and id); a read or a removal that fails otherwise leaves it
    /// remembered and is the step's outcome. Today `Job::clear_bad_item`
    /// (`upload/content.rs:165`).
    pub async fn remove_mismatched_item(&self, item: &ItemVersions, memory: &ItemMemory, file: &LocalContentFile) -> Result<Option<SentContent>, StepOutcome> {
        todo!()
    }
}

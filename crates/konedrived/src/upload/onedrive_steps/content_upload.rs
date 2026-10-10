//! A file's content sent to OneDrive: an empty file by one request, any other through an
//! upload session, in fragments, gone on with after a stop or a crash.
//!
//! **State owned:** none in memory beyond the one upload it is sending; the session is
//! the step memory's (`UploadSession`: its address, how far it got, the content being
//! sent, and for a new file the place its empty file holds).
//!
//! **The order with the step memory** (its notes for the steps):
//! `remember_upload_session_opening(item, place, now)` BEFORE the request that opens a
//! new file's session — the first call of a try; on a certain answer with no session
//! `settle_answered_opening(item, opening_try)`; with the address
//! `remember_upload_session` BEFORE the first byte — `Abandoned` means the row went or
//! the bytes changed, the cancel is queued already, and nothing is sent; after each
//! fragment `advance_upload_session` — `Abandoned` stops; a session given up (the content
//! changed between fragments): `abandon_upload_session`; a session found gone:
//! `forget_ended_upload_session(item, &address)`. A completed upload calls nothing: the
//! step's commit forgets the session.
//!
//! **Tested without the rest**, against the fixture's fake OneDrive (reference:
//! `upload/tests/sessions.rs`, `interrupted_sessions.rs`, `candidates.rs:65-156`):
// test: an_empty_file_is_sent_by_one_request_and_opens_no_session
// test: an_opening_is_kept_before_the_request_and_the_session_before_its_first_byte
// test: a_session_of_the_same_content_is_gone_on_with_from_where_onedrive_says_it_got
// test: a_session_of_another_content_is_abandoned_and_a_new_one_opened
// test: a_hold_between_two_fragments_keeps_the_session
// test: a_full_onedrive_between_two_fragments_keeps_the_session
// test: a_file_changed_between_two_fragments_abandons_the_session
// test: a_session_found_gone_whose_content_is_in_onedrive_is_the_upload_that_went_through
// test: a_session_found_gone_otherwise_is_forgotten_and_said
// test: an_answer_with_another_hash_for_a_new_file_is_remembered_and_its_removal_asked
// test: an_answer_with_another_hash_for_an_existing_item_keeps_the_fact_of_the_own_upload
// test: the_guard_is_asked_again_before_the_last_fragment_of_an_existing_item
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use konedrive_graph::drive::UploadTarget;
use konedrive_tree::{ConfirmedItem, ItemMemory, ItemVersions, OwnUploadContent, UploadSession};

use super::lost_answers::LostAnswers;
use super::GraphAccess;
use crate::folder::running_steps::RunningSteps;
use crate::folder::step_outcome::{StepObstacle, StepOutcome};
use crate::fuse::LocalContentFile;
use crate::upload::onedrive_space::OneDriveSpace;
use crate::upload::sending_holds::SendingHolds;
use crate::upload::Limits;

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
/// count included (the map's F15): the step memory answers no other.
pub fn decide_upload_start(memory: &ItemMemory, size: u64) -> UploadStart {
    todo!()
}

/// Whether an upload stops before its next fragment, and as what: while content is held
/// (`SendingHolds::is_held(StepClass::Content, now)`), and for a full OneDrive. Pure.
/// Today's `stop_between_fragments` (`upload/content/session.rs:57`).
pub fn decide_stop_between_fragments(is_content_held: bool, is_onedrive_full: bool) -> Option<StepObstacle> {
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

/// What is done about `upload`.
// OPEN: the owner's (the review's verdict on OPEN 3, after design §13's measurement on
// OneDrive for Business). Both cases are typed and neither is chosen. Either way the
// fact is kept (`ItemStore::remember_own_upload_content`) and a listing cycle asked for,
// so that the row has OneDrive's version and eTag; and the plan needs to be told the
// fact to not make a conflict copy against this folder's own upload — an input of the
// plan that does not exist yet.
pub fn decide_own_upload_follow_up(upload: &OwnUploadContent) -> OwnUploadFollowUp {
    todo!()
}

/// Why a content was not sent to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendContentError {
    /// 409 on the request that makes a new file or opens its session: the runner asks who
    /// holds the name (`NameHolders::find_name_holder`) and, for the answers that say so,
    /// calls `send_content` once more — for `NameHolder::OwnSession` with the session the
    /// step memory holds, which it goes on in.
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
        todo!()
    }

    /// Sends the content of `file` to `target` and answers the item OneDrive made of it,
    /// as its commit takes it; or why not: a taken name, which the runner decides, or the
    /// step's outcome.
    ///
    /// [`decide_upload_start`], then: `upload_empty` (`G/upload.rs:287`; today
    /// `Job::send_empty`, `upload/content/session.rs:178`); or `upload_status`
    /// (`G/upload.rs:252`; `Job::resume`, `:216`); or `create_upload_session`
    /// (`G/upload.rs:131`; `Job::open`, `:230`) — and then the fragments
    /// ([`send_fragments`](Self::send_fragments)). The step memory is written in the
    /// order the module says. A refusal is read by `graph_refusal::decide_refusal`; a
    /// gone session by [`follow_gone_session`](Self::follow_gone_session).
    ///
    /// Called by the runner's `run_send_new_file` and `run_send_content`, with the pool's
    /// upload slot held by the plan's task.
    pub async fn send_content(&self, item: &ItemVersions, target: UploadTarget<'_>, file: &LocalContentFile) -> Result<ConfirmedItem, SendContentError> {
        todo!()
    }

    /// Sends the fragments of `file` from `session.next_offset` on: `upload_chunk`
    /// (`G/upload.rs:190`; `Job::fragment`, `upload/content/session.rs:285`), each read by
    /// `LocalContentFile::read_fragment` off the runtime, each followed by
    /// `StepMemory::advance_upload_session` and the progress told
    /// (`RunningSteps::tell_progress`). Between two fragments:
    /// [`decide_stop_between_fragments`] with `SendingHolds::is_held` of content and
    /// `OneDriveSpace::is_full`. Before the last fragment of an existing item the guard is
    /// asked again (`item`, `G/mod.rs:217`; `Job::guard_broken`, `:381`).
    async fn send_fragments(&self, item: &ItemVersions, target: &UploadTarget<'_>, file: &LocalContentFile, session: UploadSession) -> Result<ConfirmedItem, StepOutcome> {
        todo!()
    }

    /// The session is gone in OneDrive: it is forgotten
    /// (`forget_ended_upload_session`); when the content sent is there already, it is the
    /// upload that went through (`LostAnswers::find_sent_content`; `Job::ended`,
    /// `upload/content/session.rs:397`); otherwise the step answers
    /// `StepRefusal::UploadSessionGone`.
    async fn follow_gone_session(&self, item: &ItemVersions, target: &UploadTarget<'_>, file: &LocalContentFile, session: &UploadSession) -> Result<ConfirmedItem, StepOutcome> {
        todo!()
    }

    /// Asks that `answer` holds the hash of the content sent (`Job::finish`,
    /// `upload/content.rs:465`). When it does not, nothing is committed and the step
    /// answers `StepRefusal::ContentMismatch`, after: for a new file the made item
    /// remembered (`StepMemory::remember_mismatched_item`) BEFORE its removal is asked
    /// (`delete_item`, `G/write.rs:140`); for an existing item the fact kept
    /// (`ItemStore::remember_own_upload_content`: OneDrive's content of the answer, the
    /// local content sent). What follows for an existing item is
    /// [`decide_own_upload_follow_up`]'s.
    pub async fn check_sent_content(&self, item: &ItemVersions, answer: &ConfirmedItem, sent_hash: &str, is_new_file: bool) -> Result<(), StepOutcome> {
        todo!()
    }

    /// Before a new file is sent: the item an earlier upload of this item left with other
    /// content is removed when it is unchanged since that upload
    /// (`MismatchedItem::is_unchanged_since_upload`, read by `item`, `G/mod.rs:217`),
    /// adopted when it holds the content after all, and forgotten either way
    /// (`StepMemory::forget_mismatched_item`, by the item's reference, drive and id). Today `Job::clear_bad_item`
    /// (`upload/content.rs:165`).
    pub async fn remove_mismatched_item(&self, item: &ItemVersions, memory: &ItemMemory) -> Result<Option<ConfirmedItem>, StepOutcome> {
        todo!()
    }
}

//! Who holds a name OneDrive answered is taken, for a new file: design §7's first
//! exception — the empty file of an upload session that holds the name — and the adoption
//! of the step's own earlier upload.
//!
//! **State owned:** none; it reads the holder from Graph and, from the step memory, the
//! upload sessions whose empty file holds the place and the windows in which this folder
//! was about to open one there.
//!
//! **Answers for:** a 409 never making a conflict copy of the folder's own empty file,
//! and never a loop: this folder's own is removed, an unknown one's is waited for, and
//! any other holder is the plan's (`StepRefusal::NameTaken`, kept as a taken name).
//! Today: `Job::own_placeholder` (`upload/content.rs:239`), `opened_placeholder` (`:279`),
//! `taken` (`upload/steps/shared.rs:272`).
//!
//! **Tested without the rest**, a test per case of [`NameHolder`], then against the
//! fixture's fake OneDrive (reference: `upload/tests/sessions.rs:293-784`):
// test: no_holder_by_now_is_a_name_that_is_free
// test: the_empty_file_of_this_items_own_session_is_gone_on_with
// test: the_empty_file_of_another_items_live_session_is_waited_for
// test: the_empty_file_of_another_items_session_past_its_end_is_forgotten_and_the_name_asked_again
// test: the_holder_of_a_name_a_move_found_taken_is_answered_as_onedrive_spells_it
// test: the_empty_file_of_a_session_nothing_goes_on_with_is_cancelled
// test: an_empty_unlisted_file_made_within_an_opening_window_is_this_folders_and_is_removed
// test: an_empty_unlisted_file_made_outside_every_window_is_waited_for
// test: an_empty_unlisted_file_made_outside_every_window_is_adopted_by_a_step_that_sends_an_empty_file
// test: a_file_with_the_hash_sent_that_no_row_is_bound_to_is_adopted
// test: any_other_holder_is_another_item_with_its_name_as_onedrive_spells_it
#![allow(dead_code, unused_variables)]

use std::ffi::{OsStr, OsString};

use konedrive_graph::drive::DriveItem;
use konedrive_tree::{ConfirmedItem, EmptyFileSession, ItemVersions, LocalItemId, OneDriveItemReference, OpeningWindow, UploadSessionAddress};

use super::session_cancellations::SessionCancellations;
use super::GraphAccess;
use crate::folder::step_outcome::StepOutcome;

/// What is known of a taken name, gathered before the decision.
#[derive(Debug, Clone)]
pub struct NameHolderFacts {
    /// What OneDrive has under the name now; `None` when nothing is there any more.
    pub holder: Option<DriveItem>,
    /// Whether a row is bound to the holder: a listing told of it already.
    pub is_holder_bound: bool,
    /// The hash of the content this step sends, when it was computed.
    pub sent_hash: Option<String>,
    /// The upload sessions whose empty file holds the place
    /// (`ItemReader::list_empty_file_sessions`).
    pub empty_file_sessions: Vec<EmptyFileSession>,
    /// The times between which this folder was about to open a session there
    /// (`ItemReader::list_opening_windows`).
    pub opening_windows: Vec<OpeningWindow>,
}

/// Who holds the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameHolder {
    /// Nobody by now: the request is sent again.
    Nobody,
    /// The empty file of this item's own session (`EmptyFileSession::Continued` of this
    /// item): the upload goes on in it.
    OwnSession,
    /// The empty file of another item's live session (`EmptyFileSession::Continued` of
    /// another item, whose `expires_at` is not past at `now`, or is not known): the step
    /// waits for it, as `StepRefusal::NameHeldByUploadSession`.
    // TODO: a session of another item with no `expires_at` (OneDrive named none) has no end this step can act on: it waits with the usual tries for as long as that item's memory keeps the session. Decide whether such a session is given a longest life here.
    AnotherItemsSession,
    /// The empty file of another item's session whose `expires_at` is past at `now`: the
    /// session ended by itself, whether its item has a step or not (it may be left here,
    /// or wait for the user). This step says so
    /// (`StepMemory::forget_ended_upload_session` of that item and address), and the
    /// request is sent again.
    AnotherItemsEndedSession { item: LocalItemId, address: UploadSessionAddress },
    /// The empty file of a session nothing goes on with: cancelled, then the request sent
    /// again.
    AbandonedSession(UploadSessionAddress),
    /// An empty file no listing told of, made within a window in which this folder was
    /// about to open a session there (the clock's slack: 5 minutes, `content.rs:30`):
    /// provably this folder's own, removed, then the request sent again.
    OwnEmptyFile(OneDriveItemReference),
    /// A file no row is bound to that holds the content sent: this step's own earlier
    /// upload, adopted. Before `UnknownEmptyFile` on purpose: an empty file is sent by
    /// one request with no opening kept, so the replay of one whose answer was lost finds
    /// an empty file no listing told of, and adopts it as today's worker does
    /// (`upload/steps/shared.rs:300`); after the cases above, so that an empty file a
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
pub fn decide_name_holder(facts: &NameHolderFacts, item: &ItemVersions, now: i64) -> NameHolder {
    todo!()
}

/// What finds the holder of a taken name.
#[derive(Clone)]
pub struct NameHolders {
    graph: GraphAccess,
    session_cancellations: SessionCancellations,
}

impl NameHolders {
    pub fn new(graph: GraphAccess, session_cancellations: SessionCancellations) -> Self {
        Self { graph, session_cancellations }
    }

    /// After a 409 on `SendNewFile`: reads the holder (`child`, `G/mod.rs:228`), whether a
    /// row is bound to it (`ItemReader::find_bound_item`), and the step memory's sessions
    /// and windows at the place (`ItemReader::list_empty_file_sessions`,
    /// `list_opening_windows`; the place as the step memory names it: the directory of
    /// the place of `item.local`, under `name`), and
    /// decides ([`decide_name_holder`]). Then acts on what is the folder's own: cancels
    /// an abandoned session (`SessionCancellations::cancel_session`), removes its own
    /// empty file (`delete_item`, `G/write.rs:140`, guarded by the eTag just read),
    /// forgets another item's session that ended
    /// (`StepMemory::forget_ended_upload_session`), and once the place is resolved — the
    /// empty file removed, the name found free, or the holder not this folder's — calls
    /// `StepMemory::forget_upload_session_openings_at`. Answers what the step does next:
    /// for `Nobody`, `AbandonedSession`, `OwnEmptyFile` and `AnotherItemsEndedSession`
    /// the step sends its request once more, and a second 409 in the same step is
    /// `StepRefusal::NameHeldByUploadSession`: no step asks a third time. A cancel or a
    /// removal OneDrive does not carry out answers `Err` with
    /// `StepRefusal::NameHeldByUploadSession` too (today `Reason::SessionOpen`,
    /// `upload/content.rs:306`); a read that fails, its refusal.
    /// `sent_hash` is the hash of the whole file (`LocalContentFile::hash`), which the
    /// runner reads for this call.
    pub async fn find_name_holder(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, sent_hash: Option<&str>) -> Result<NameHolder, StepOutcome> {
        todo!()
    }

    /// After a 409 on `SendMove`: the name of the item that holds `name` in `parent`, as
    /// OneDrive spells it (`child`, `G/mod.rs:228`), for `StepRefusal::NameTaken`.
    /// `None` when nothing holds it by now: the step answers `StepRefusal::Transient`,
    /// and its next try sends the move again. A read that fails is the step's outcome.
    /// Nothing is adopted here: a move that went through before is answered 412, not
    /// 409, and is `LostAnswers`'.
    pub async fn find_holder_name(&self, parent: &OneDriveItemReference, name: &OsStr) -> Result<Option<OsString>, StepOutcome> {
        todo!()
    }
}

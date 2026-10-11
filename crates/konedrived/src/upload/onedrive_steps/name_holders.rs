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
// test: the_empty_file_of_a_session_nothing_goes_on_with_is_cancelled
// test: an_empty_unlisted_file_made_within_an_opening_window_is_this_folders_and_is_removed
// test: an_empty_unlisted_file_made_outside_every_window_is_waited_for
// test: a_file_with_the_hash_sent_that_no_row_is_bound_to_is_adopted
// test: any_other_holder_is_another_item_with_its_name_as_onedrive_spells_it
#![allow(dead_code, unused_variables)]

use std::ffi::{OsStr, OsString};

use konedrive_graph::drive::DriveItem;
use konedrive_tree::{ConfirmedItem, EmptyFileSession, ItemVersions, OneDriveItemReference, OpeningWindow, UploadSessionAddress};

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
    /// The empty file of another item's live session: the step waits for it.
    AnotherItemsSession,
    /// The empty file of a session nothing goes on with: cancelled, then the request sent
    /// again.
    AbandonedSession(UploadSessionAddress),
    /// An empty file no listing told of, made within a window in which this folder was
    /// about to open a session there (the clock's slack: 5 minutes, `content.rs:30`):
    /// provably this folder's own, removed, then the request sent again.
    OwnEmptyFile(OneDriveItemReference),
    /// An empty file no listing told of, of nobody known: the step waits for it. Never
    /// copied, never removed.
    UnknownEmptyFile,
    /// A file no row is bound to that holds the content sent: this step's own earlier
    /// upload, adopted.
    OwnEarlierUpload(ConfirmedItem),
    /// Another item, spelt so by OneDrive.
    AnotherItem { holder_name: OsString },
}

/// Who holds the name, from `facts`. Pure; the first case that holds, in the order of
/// [`NameHolder`]'s cases. `now` is the clock's, for the windows.
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
    /// row is bound to it, and the step memory's sessions and windows at the place, and
    /// decides ([`decide_name_holder`]). Then acts on what is the folder's own: cancels
    /// an abandoned session (`SessionCancellations::cancel_session`), removes its own
    /// empty file (`delete_item`, `G/write.rs:140`), and once the place is resolved calls
    /// `StepMemory::forget_upload_session_openings_at`. Answers what the step does next:
    /// for `Nobody`, `AbandonedSession` and `OwnEmptyFile` the step sends its request
    /// once more, and a second 409 in the same step is answered as it then is.
    pub async fn find_name_holder(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, sent_hash: Option<&str>) -> Result<NameHolder, StepOutcome> {
        todo!()
    }
}

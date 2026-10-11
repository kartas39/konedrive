//! What became of a request whose answer was lost, or refused in a way its own earlier
//! try explains: design §7's second exception. The step adopts what it finds.
//!
//! **State owned:** none. The record of a request that left is its running step
//! (`RunningSteps`: the item, and the directory it is sent into): a step whose request
//! may have gone through does not end before OneDrive was read
//! ([`LostAnswers::settle_lost_answer`]), so the plan is told the item and the
//! destination as busy for as long as nothing certain is known, by the readers it has
//! (`RunningSteps::list_items`, `list_destinations`), and `Refused(Transient)` is
//! answered only for a request that certainly changed nothing — with one exception,
//! whose record is the step memory's: an answer lost to the opening of an upload session
//! or to one of its fragments, the last too, is `Transient` at once
//! (`content_upload`), and the opening or the session kept tells the next try what the
//! request left. The file and the directory it is sent into are then not busy to the
//! plan, and nothing is lost by that: a file removed here meanwhile may come back once
//! from OneDrive, and a removal of the directory takes only the upload's own file.
//! Across a stop of the
//! folder's sync nothing is kept: the next start sends nothing before a listing of its
//! own ended (`SendingHold::FirstListingNotEnded`), and that listing names what the
//! request changed. Nothing is remembered here but what the step memory holds anyway
//! (the opening, the session, the temporary name).
//!
//! **Answers for:** a daemon that died, or was stopped, between a request and its commit,
//! and an answer that never came while it runs: nothing in the rows says a request went
//! out, so the replay rests on these (the map's F9, E2). Today:
//! `upload/steps/meta.rs:33-36`, `:100-104`,
//! `upload/content.rs:211-215`, `:374`, `:400-403`, `upload/content/session.rs:397`.
//!
//! **Tested without the rest**, a test per case of [`SentRequest`] and of
//! [`LostAnswerFate`], then against the fixture's fake OneDrive:
// test: a_directory_of_the_name_that_no_row_is_bound_to_is_the_one_this_step_made
// test: a_directory_of_the_name_that_a_row_is_bound_to_is_another_item_with_its_name
// test: a_directory_of_the_name_that_is_not_empty_is_another_item
// test: a_read_refused_for_certain_ends_the_step_as_unreadable
// test: an_item_guarded_by_its_ctag_is_as_planned_while_it_has_that_ctag
// test: a_file_of_the_name_with_the_hash_sent_that_no_row_is_bound_to_is_adopted
// test: an_item_already_at_the_place_sent_is_the_move_that_went_through
// test: an_item_that_holds_the_hash_sent_is_the_upload_that_went_through
// test: an_item_that_is_gone_is_the_removal_that_went_through
// test: an_item_with_the_etag_planned_from_is_a_request_that_did_not_go_through
// test: an_item_that_was_moved_or_changed_by_another_is_not_adopted
// test: a_lost_answer_is_read_again_until_onedrive_answers_and_the_step_ends_only_then
// test: a_read_that_is_throttled_or_signed_out_tells_the_sending_holds_and_goes_on
// TODO: the owner's: design §7 says "every request that changes OneDrive is on record before it leaves", a record in the store that is settled at start by reading its item. Written here as: the running step is the record while the daemon runs, and the first listing of a start settles what a stop left. Decide whether that stands, or the store is to keep the record. The case the running step does not cover, to decide with: the first listing of a start ending is not that listing naming what the lost request changed (a delta may lag a request by seconds). A move of X into D went through and the daemon was killed before the commit; after the start the first listing ends without naming X; the user moves X back out (the rows then say nothing is unsent) and removes D; the unguarded removal of D takes X in OneDrive, and the next listing removes X here. A record in the store, read at start, closes it; nothing in this layer does.
#![allow(dead_code, unused_variables)]

use std::ffi::{OsStr, OsString};
use std::sync::Arc;

use konedrive_graph::drive::DriveItem;
use konedrive_tree::{ConfirmedItem, DriveId, ItemVersions, OneDriveItemReference};

use super::GraphAccess;
use crate::sync_parts::step_outcome::{StepFailure, StepOutcome};
use crate::sync_parts::sending_holds::SendingHolds;

/// A request that changes OneDrive, by what it asked: what is compared with OneDrive to
/// tell whether it went through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentRequest<'a> {
    /// A directory made in `parent` under `name`. Read: `child` of that place.
    MakeDirectory { parent: &'a OneDriveItemReference, name: &'a OsStr },
    /// A file with the content of `sent_hash` made in `parent` under `name`. Read:
    /// `child` of that place.
    SendNewFile { parent: &'a OneDriveItemReference, name: &'a OsStr, sent_hash: &'a str },
    /// The content of `sent_hash` sent into the item. Read: `item`.
    SendContent { sent_hash: &'a str },
    /// The item sent into `parent` under `name`. Read: `item`.
    SendMove { parent: &'a OneDriveItemReference, name: &'a OsStr },
    /// The removal of the item, a file or a directory. Read: `item`.
    SendRemoval,
}

/// What became of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostAnswerFate {
    /// OneDrive has what the request asked: the item as it is now, for the step's commit;
    /// none for a removal, whose item is gone.
    WentThrough(Option<ConfirmedItem>),
    /// OneDrive has the item as the step was planned from it — what the request was
    /// guarded with (`confirmed_item::decide_guard`): the eTag planned from, or, for an
    /// item guarded by the cTag of its synced content, that cTag —, or
    /// nothing at the place a new item was sent to: the request changed nothing.
    DidNotGoThrough,
    /// OneDrive has the item neither as planned nor as asked, or does not have an item
    /// that was to be moved or filled: another change, which a listing brings.
    ItemChanged,
    /// Another item holds the place a new item was sent to — one a row is bound to, of
    /// another kind or content, or a directory that is not empty —, spelt so by OneDrive.
    NameHeld { holder_name: OsString },
}

/// What became of `request` about `item`, from what OneDrive has now at the place the
/// request names (`found`: the item read, none when OneDrive has none there) and whether
/// a row is bound to what was found. Pure. A new item is the step's own only when no row
/// is bound to it, and a directory only when it is empty besides (`DriveItem::folder`'s
/// `childCount` is 0; a count that is not given counts as not empty): the step's own
/// request made an empty directory, and one with something in it is another client's of
/// the same name that no listing brought yet, whose content no row knows — adopted, an
/// unguarded removal would take it. Such a directory is `NameHeld`, and the plan merges
/// the two rows once a listing brought it with what is in it. A move went through when the item is in `parent` under `name` as
/// Graph spells both; a content when the item's hash is `sent_hash`. An answer that
/// cannot be read as an item (`confirmed_item::to_confirmed_item`) is the failure.
pub fn decide_lost_answer_fate(request: &SentRequest<'_>, item: &ItemVersions, found: Option<&DriveItem>, is_found_bound: bool, drive_id: &DriveId) -> Result<LostAnswerFate, StepFailure> {
    todo!()
}

/// What reads OneDrive for what became of a step's request.
#[derive(Clone)]
pub struct LostAnswers {
    graph: GraphAccess,
    /// Told when a read is throttled or answered that the account is signed out, so that
    /// all sending waits though the step does not end.
    sending_holds: Arc<SendingHolds>,
}

impl LostAnswers {
    pub fn new(graph: GraphAccess, sending_holds: Arc<SendingHolds>) -> Self {
        Self { graph, sending_holds }
    }

    /// What became of `request`, by ONE read of OneDrive (`item`, `G/mod.rs:217`, or
    /// `child`, `G/mod.rs:228`, as the case of [`SentRequest`] says), whether a row is
    /// bound to what was found (`ItemReader::find_bound_item`), and
    /// [`decide_lost_answer_fate`]. `DriveError::NotFound` is no failed read: it is
    /// `found: None`. Any other `DriveError` is the step's outcome, read through
    /// `WriteError::from` by `graph_refusal::decide_refusal` (`AnswerLost` of a read is
    /// `StepRefusal::Transient`). Each read takes a `Class::Metadata` slot of the pool
    /// for itself and lets it go with its answer.
    ///
    /// For a request that was certainly refused and whose refusal an earlier try of the
    /// same step may explain: a 409 of `MakeDirectory` (`upload/steps/meta.rs:33-36`), a
    /// 412 of `SendMove` (`meta.rs:100-104`) and of `SendContent`
    /// (`upload/content.rs:400-403`), an upload session found gone.
    pub async fn read_request_outcome(&self, item: &ItemVersions, request: SentRequest<'_>) -> Result<LostAnswerFate, StepOutcome> {
        todo!()
    }

    /// What became of `request`, whose answer said nothing certain
    /// (`RefusedRequest::AnswerLost`, or an answer of success that cannot be read): reads
    /// as [`read_request_outcome`](Self::read_request_outcome) does (`DriveError::NotFound`
    /// is `found: None`; any other `DriveError` is read through `WriteError::from`) and,
    /// while the read itself says nothing certain — no status (the network), a 408, a
    /// 429 or a 5xx (`DriveError::status`) —, waits and reads again: a second after the
    /// first failed read, doubling to a minute. The client waited a throttle's own time
    /// already and names none: a 429 or a 503 is told as `SendingHolds::throttle(None,
    /// now)`; a sign-out as `SendingHolds::set_signed_out(true)`, and the next read is a
    /// minute later. A read that is refused for certain and is neither the item nor "not
    /// there" — any other 4xx: a 403, a 400, a token Graph rejects — ends the step as
    /// `StepFailure::AnswerUnreadable { detail }`: nothing says what became of the
    /// request, the step counts a try, and the one step at a time of the metadata is not
    /// held by it. It answers otherwise only what is certain, and
    /// ends else only with the step's task, aborted at a stop of the folder's sync:
    /// until then the step runs, and its item and destination are busy to the plan. No
    /// slot of the pool is held while it waits: its caller let its own go before the
    /// call, and each read takes a `Class::Metadata` slot for itself. The failure is
    /// also an answer that cannot be read as an item.
    pub async fn settle_lost_answer(&self, item: &ItemVersions, request: SentRequest<'_>) -> Result<LostAnswerFate, StepFailure> {
        todo!()
    }
}

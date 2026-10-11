//! A refusal of Graph, by what a step does about it.
//!
//! **State owned:** none: one pure function.
//!
//! **Answers for:** design §7, "a step that OneDrive refuses decides nothing". Today's
//! `outcome_of` (`upload/engine/outcome.rs:133`) and `Sent::refused`
//! (`upload/content.rs:110`), with the outcome typed and nothing written.
//!
//! **Tested without the rest**, a test per case of `WriteError`:
// test: a_409_a_412_a_404_and_a_gone_session_are_the_steps_to_look_at
// test: a_507_is_no_space
// test: a_429_and_a_503_are_throttled_with_the_time_onedrive_named
// test: a_423_is_locked_and_tried_again
// test: a_403_is_forbidden_and_a_400_refused_for_good_with_its_message
// test: a_signed_out_account_is_said_so
// test: the_network_a_timeout_and_a_5xx_are_an_answer_lost_and_carry_no_address
// test: any_other_4xx_is_settled_as_transient_at_once
#![allow(dead_code, unused_variables)]

use konedrive_graph::drive::WriteError;

use crate::sync_parts::step_outcome::StepRefusal;

/// What a refused request leaves the step to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusedRequest {
    /// 409: the step asks who holds the name (`name_holders`).
    NameExists,
    /// 412: the step asks once whether its own earlier request went through
    /// (`lost_answers`), and otherwise answers `ItemChanged`.
    Changed,
    /// 404: the item or the directory, by which the request named.
    NotFound,
    /// The upload session is gone (`content_upload` asks whether the content is there).
    SessionGone,
    /// 507: the quota is read (`OneDriveSpace::refuse_for_space`, by the content upload
    /// for a content and by the runner for any other request).
    NoSpace,
    /// `WriteError::Transient` and `Failed` with no status (the network, an answer that
    /// could not be read), a 408 or a 5xx other than 503 (`Detail::status`). Nothing says
    /// whether the request reached OneDrive: a step whose request changes OneDrive reads what became of it before it
    /// ends (`LostAnswers::settle_lost_answer`), and answers
    /// `StepRefusal::Transient { detail }` only for a request that changed nothing; a
    /// request that only reads answers it at once.
    // TODO: another layer's: a request that never left (no connection, DNS, a locked secret store: `konedrive-graph/src/drive/send.rs:95`, `:124-126`) is `Transient` with no status, as a dropped answer is. So while offline every step that sends no content, and every empty new file, stays running in `settle_lost_answer`, no try is counted for `Reason::Network`, and the queue shows an item being sent where today it says there is no network. Needed: the Graph client telling "not sent" from "sent, no answer" in `Detail` (`reqwest::Error::is_connect`, the token's errors), and `decide_refusal` settling the first at once as `StepRefusal::Transient`.
    AnswerLost { detail: String },
    /// Nothing more to look at: the answer of the step.
    Settled(StepRefusal),
}

/// What `error` leaves a step to do. Pure. Settled at once: 429 and 503 `Throttled`, 423
/// `Locked`, 403 `Forbidden`, 400 `RefusedForGood` with OneDrive's message, a signed-out
/// account `SignedOut`, and a `Transient` or `Failed` whose status is any other 4xx but
/// 408 (a token Graph rejects, a 405, a 413…) `Transient { detail }` — each a certain
/// answer that the request changed nothing. The
/// text of a lost answer is kept without any address in it (`engine/outcome.rs:158`,
/// `without_urls`).
pub fn decide_refusal(error: &WriteError) -> RefusedRequest {
    todo!()
}

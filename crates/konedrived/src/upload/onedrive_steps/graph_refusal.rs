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
// test: the_network_and_everything_else_is_an_answer_lost_and_carries_no_address
#![allow(dead_code, unused_variables)]

use konedrive_graph::drive::WriteError;

use crate::folder::step_outcome::StepRefusal;

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
    /// `WriteError::Transient` and `Failed`: the network, a timeout, a 5xx other than
    /// 503, an answer that could not be read. Nothing says whether the request reached
    /// OneDrive: a step whose request changes OneDrive reads what became of it before it
    /// ends (`LostAnswers::settle_lost_answer`), and answers
    /// `StepRefusal::Transient { detail }` only for a request that changed nothing; a
    /// request that only reads answers it at once.
    AnswerLost { detail: String },
    /// Nothing more to look at: the answer of the step.
    Settled(StepRefusal),
}

/// What `error` leaves a step to do. Pure. Settled at once: 429 and 503 `Throttled`, 423
/// `Locked`, 403 `Forbidden`, 400 `RefusedForGood` with OneDrive's message, a signed-out
/// account `SignedOut` — each a certain answer that the request changed nothing. The
/// text of a lost answer is kept without any address in it (`engine/outcome.rs:158`,
/// `without_urls`).
pub fn decide_refusal(error: &WriteError) -> RefusedRequest {
    todo!()
}

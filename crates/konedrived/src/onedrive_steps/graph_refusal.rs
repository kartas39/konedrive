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

use konedrive_graph::drive::{DriveError, WriteError};

use crate::sync_parts::step_outcome::StepRefusal;

/// What a refused request leaves the step to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefusedRequest {
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
    /// whether the request reached OneDrive: a step whose request changes OneDrive reads
    /// once what became of it (`LostAnswers::read_request_outcome`); when that read says
    /// nothing certain either — the network is down — the step ends
    /// `StepRefusal::Transient { detail }` with its record standing
    /// (`konedrive_tree::RequestRecord`), which keeps the item and its destination busy
    /// until the record is settled (`LostAnswers::settle_lost_answer`). A request that
    /// only reads answers `Transient` at once.
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
pub(crate) fn decide_refusal(error: &WriteError) -> RefusedRequest {
    match error {
        WriteError::Changed => RefusedRequest::Changed,
        WriteError::NameExists => RefusedRequest::NameExists,
        WriteError::NotFound => RefusedRequest::NotFound,
        WriteError::SessionGone => RefusedRequest::SessionGone,
        WriteError::QuotaExceeded => RefusedRequest::NoSpace,
        WriteError::Throttled { retry_after } => RefusedRequest::Settled(StepRefusal::Throttled { retry_after: *retry_after }),
        WriteError::Locked => RefusedRequest::Settled(StepRefusal::Locked),
        WriteError::Forbidden => RefusedRequest::Settled(StepRefusal::Forbidden),
        WriteError::Refused(detail) => RefusedRequest::Settled(StepRefusal::RefusedForGood { message: detail.message.clone() }),
        WriteError::SignedOut => RefusedRequest::Settled(StepRefusal::SignedOut),
        WriteError::Transient(detail) | WriteError::Failed(detail) => {
            let text = remove_addresses(&detail.message);
            match detail.status {
                // A certain answer that the request changed nothing.
                Some(status) if status.is_client_error() && status.code() != 408 => RefusedRequest::Settled(StepRefusal::Transient { detail: text }),
                _ => RefusedRequest::AnswerLost { detail: text },
            }
        }
    }
}

/// What a refusal leaves a step to answer where a taken name, a changed item, a missing
/// item, a gone session and a lack of space have no meaning of their own for the request:
/// a read, a cancel, a removal of a leftover. A lost answer is `StepRefusal::Transient`
/// at once: such a request is asked again whatever became of it. Pure.
pub(crate) fn decide_plain_refusal(error: &WriteError) -> StepRefusal {
    match decide_refusal(error) {
        RefusedRequest::Settled(refusal) => refusal,
        RefusedRequest::AnswerLost { detail } => StepRefusal::Transient { detail },
        // Said as it is, and tried again.
        refused @ (RefusedRequest::NameExists | RefusedRequest::Changed | RefusedRequest::NotFound | RefusedRequest::SessionGone | RefusedRequest::NoSpace) => {
            StepRefusal::Transient { detail: format!("OneDrive answered {refused:?}") }
        }
    }
}

/// What a READ of OneDrive that failed leaves a step to answer: [`decide_plain_refusal`]
/// of the error through `WriteError::from`. `DriveError::NotFound` is not asked here: it
/// is the read's answer "nothing is there". Pure.
pub(crate) fn decide_read_refusal(error: DriveError) -> StepRefusal {
    decide_plain_refusal(&WriteError::from(error))
}

/// `text` with every `http://…` and `https://…` cut out, up to the next whitespace: the
/// journal and the bus get no address.
fn remove_addresses(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = [rest.find("http://"), rest.find("https://")].into_iter().flatten().min() {
        cleaned.push_str(&rest[..at]);
        cleaned.push_str("<url>");
        rest = &rest[at..];
        rest = &rest[rest.find(char::is_whitespace).unwrap_or(rest.len())..];
    }
    cleaned.push_str(rest);
    cleaned
}

#[cfg(test)]
mod tests;

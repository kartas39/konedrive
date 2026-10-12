use std::time::Duration;

use konedrive_graph::drive::{Detail, DriveError, Status, WriteError};

use super::*;

fn detail(message: &str, status: Option<u16>) -> Detail {
    Detail { message: message.to_owned(), status: status.map(Status::new), code: None }
}

fn transient(detail: &str) -> RefusedRequest {
    RefusedRequest::Settled(StepRefusal::Transient { detail: detail.to_owned() })
}

fn lost(detail: &str) -> RefusedRequest {
    RefusedRequest::AnswerLost { detail: detail.to_owned() }
}

#[test]
fn a_409_a_412_a_404_and_a_gone_session_are_the_steps_to_look_at() {
    let cases = [
        (WriteError::NameExists, RefusedRequest::NameExists),
        (WriteError::Changed, RefusedRequest::Changed),
        (WriteError::NotFound, RefusedRequest::NotFound),
        (WriteError::SessionGone, RefusedRequest::SessionGone),
        (WriteError::QuotaExceeded, RefusedRequest::NoSpace),
    ];
    for (error, expected) in cases {
        assert_eq!(decide_refusal(&error), expected, "{error:?}");
    }
}

#[test]
fn a_refusal_that_is_certain_is_settled_at_once_as_its_case() {
    let cases = [
        (WriteError::Throttled { retry_after: Some(Duration::from_secs(7)) }, StepRefusal::Throttled { retry_after: Some(Duration::from_secs(7)) }),
        (WriteError::Throttled { retry_after: None }, StepRefusal::Throttled { retry_after: None }),
        (WriteError::Locked, StepRefusal::Locked),
        (WriteError::Forbidden, StepRefusal::Forbidden),
        (WriteError::TooLarge, StepRefusal::TooLarge),
        (WriteError::Refused(detail("the name is not allowed", Some(400))), StepRefusal::RefusedForGood { message: "the name is not allowed".to_owned() }),
        (WriteError::SignedOut, StepRefusal::SignedOut),
    ];
    for (error, expected) in cases {
        assert_eq!(decide_refusal(&error), RefusedRequest::Settled(expected), "{error:?}");
    }
}

/// `Transient` and `Failed` by their status: a 4xx other than a 408 is a certain answer
/// that nothing changed; no status, a 408 and a 5xx say nothing certain.
#[test]
fn the_network_a_timeout_and_a_5xx_are_an_answer_lost_and_any_other_4xx_is_settled_as_transient() {
    let cases = [
        (None, lost("x")),
        (Some(408), lost("x")),
        (Some(500), lost("x")),
        (Some(502), lost("x")),
        (Some(504), lost("x")),
        (Some(401), transient("x")),
        (Some(405), transient("x")),
        (Some(413), transient("x")),
        (Some(429), transient("x")),
    ];
    for (status, expected) in cases {
        assert_eq!(decide_refusal(&WriteError::Transient(detail("x", status))), expected, "Transient {status:?}");
        assert_eq!(decide_refusal(&WriteError::Failed(detail("x", status))), expected, "Failed {status:?}");
    }
}

#[test]
fn the_text_of_a_lost_answer_carries_no_address() {
    let error = WriteError::Transient(detail("cannot reach https://graph.microsoft.com/v1.0/me/drive?x=1 now, nor http://a.b/c", None));
    assert_eq!(decide_refusal(&error), lost("cannot reach <url> now, nor <url>"));
    assert_eq!(decide_refusal(&WriteError::Failed(detail("no address here", Some(404)))), transient("no address here"));
}

/// Where a taken name, a changed item, a missing item, a gone session and a lack of space
/// mean nothing of their own, each is said as it is and tried again; a lost answer is
/// `Transient` at once, and what is settled stays what it is.
#[test]
fn a_plain_refusal_is_the_settled_one_and_transient_for_all_else() {
    for error in [WriteError::NameExists, WriteError::Changed, WriteError::NotFound, WriteError::SessionGone, WriteError::QuotaExceeded] {
        assert!(matches!(decide_plain_refusal(&error), StepRefusal::Transient { .. }), "{error:?}");
    }
    assert_eq!(decide_plain_refusal(&WriteError::Transient(detail("the network", None))), StepRefusal::Transient { detail: "the network".to_owned() });
    assert_eq!(decide_plain_refusal(&WriteError::Locked), StepRefusal::Locked);
    assert_eq!(decide_plain_refusal(&WriteError::Forbidden), StepRefusal::Forbidden);
    assert_eq!(decide_plain_refusal(&WriteError::SignedOut), StepRefusal::SignedOut);
    assert_eq!(decide_plain_refusal(&WriteError::Throttled { retry_after: None }), StepRefusal::Throttled { retry_after: None });
}

/// A read that failed, through `WriteError::from`.
#[test]
fn a_failed_read_is_read_as_the_refusal_of_its_error() {
    assert_eq!(decide_read_refusal(DriveError::SignedOut), StepRefusal::SignedOut);
    assert_eq!(decide_read_refusal(DriveError::Transient(detail("the network", None))), StepRefusal::Transient { detail: "the network".to_owned() });
    assert_eq!(decide_read_refusal(DriveError::Failed(detail("rejected", Some(401)))), StepRefusal::Transient { detail: "rejected".to_owned() });
    assert!(matches!(decide_read_refusal(DriveError::ResyncRequired), StepRefusal::Transient { .. }));
}

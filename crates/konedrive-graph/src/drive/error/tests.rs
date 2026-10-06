use super::*;

fn kind(status: u16, code: &str) -> Kind {
    classify(Status::new(status), code)
}

/// The statuses the reads' loop, the writes' loop and a fragment's loop tell apart.
#[test]
fn a_status_alone_decides_when_graph_names_no_code() {
    // What every loop looks at before anything else.
    assert_eq!(kind(401, ""), Kind::Unauthorized);
    assert_eq!(kind(429, ""), Kind::Throttled);
    assert_eq!(kind(503, ""), Kind::Throttled);
    // What a read tells apart.
    assert_eq!(kind(404, ""), Kind::NotFound);
    assert_eq!(kind(410, ""), Kind::Gone);
    assert_eq!(kind(408, ""), Kind::Timeout);
    assert_eq!(kind(416, ""), Kind::RangeNotSatisfiable);
    // What a write tells apart (`docs/design/writes.md` §6.2).
    assert_eq!(kind(412, ""), Kind::Changed);
    assert_eq!(kind(409, ""), Kind::NameExists);
    assert_eq!(kind(507, ""), Kind::QuotaExceeded);
    assert_eq!(kind(423, ""), Kind::Locked);
    assert_eq!(kind(403, ""), Kind::Forbidden);
    assert_eq!(kind(400, ""), Kind::BadRequest);
    // The service failing, but not asking to wait and not full.
    for status in [500, 502, 504, 509, 599] {
        assert_eq!(kind(status, ""), Kind::Server, "{status}");
    }
    // Nothing a call has a meaning of its own for: successes, redirects, other refusals.
    for status in [200, 201, 202, 204, 206, 302, 402, 405, 406, 411, 415, 422] {
        assert_eq!(kind(status, ""), Kind::Other, "{status}");
    }
}

#[test]
fn graphs_code_decides_where_it_is_specific_whatever_the_status() {
    for status in [400, 403, 409, 429, 500, 507] {
        assert_eq!(kind(status, "nameAlreadyExists"), Kind::NameExists, "{status}");
        assert_eq!(kind(status, "quotaLimitReached"), Kind::QuotaExceeded, "{status}");
    }
    // Any other code leaves the status to decide.
    assert_eq!(kind(400, "invalidRequest"), Kind::BadRequest);
    assert_eq!(kind(403, "accessDenied"), Kind::Forbidden);
    assert_eq!(kind(404, "itemNotFound"), Kind::NotFound);
    assert_eq!(kind(429, "activityLimitReached"), Kind::Throttled);
    assert_eq!(kind(503, "serviceNotAvailable"), Kind::Throttled);
    assert_eq!(kind(500, "generalException"), Kind::Server);
}

/// A status prints as the status line has it: the texts of the errors and of the log lines
/// that hold one stay what they were.
#[test]
fn a_status_prints_with_its_reason() {
    assert_eq!(Status::NOT_ACCEPTABLE.to_string(), "406 Not Acceptable");
    assert_eq!(Status::new(503).to_string(), "503 Service Unavailable");
    assert_eq!(Status::new(599).to_string(), "599 <unknown status code>");
}

/// An error made of an answer keeps the status and the code, and prints as its message
/// alone.
#[test]
fn an_error_carries_the_status_and_the_code_beside_its_text() {
    let error = DriveError::Failed(Detail::answered("Graph returned 400 Bad Request", Status::new(400), "invalidRequest"));
    assert_eq!(error.to_string(), "Graph returned 400 Bad Request");
    assert_eq!(format!("{error:?}"), r#"Failed("Graph returned 400 Bad Request")"#);
    assert_eq!((error.status(), error.code()), (Some(Status::new(400)), Some("invalidRequest")));
    let unsent = DriveError::Transient("cannot reach Microsoft Graph".into());
    assert_eq!((unsent.status(), unsent.code()), (None, None));
    assert_eq!(DriveError::NotFound.status(), None);
}

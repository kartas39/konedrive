use std::io;
use std::time::Duration;

use konedrive_graph::drive::{DriveError, WriteError};
use konedrive_tree::outbox::{OutboxState, Reason};
use konedrive_tree::TreeError;

use super::now;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// `mkdir`, `move`, `delete`: one at a time (§3.5).
    Meta,
    /// `create`, `update`: as many as the account's transfer pool gives, small or large.
    Content,
    /// `move-out`: a download, then a delete. One at a time, beside
    /// the others: its dependencies order it (a folder's removal waits for
    /// what left it first).
    Out,
}

/// How one run of a row ended.
#[derive(Debug)]
pub(in crate::upload) enum Outcome {
    /// Committed, dropped, or turned into another row by its own
    /// transaction: nothing more to write.
    Done,
    /// Back in line.
    /// `detail` is a failure's own text, for the journal only (issue #87):
    /// never the row's reason.
    Again { state: OutboxState, reason: Option<Reason>, next_try: Option<i64>, backoff: bool, detail: Option<String> },
    /// OneDrive asked the whole account to wait (§4.10).
    Throttled(Option<Duration>),
    SignedOut,
    /// `403`: OneDrive does not allow this change. The row is blocked; the others go on.
    Forbidden,
    /// A fault point fired: the row stays `running`, as after a crash.
    Crashed,
    /// OneDrive refused the content for lack of space: the step reads the
    /// quota and turns this into [`Outcome::Space`] (`space`).
    NoSpace,
    /// Ready, in its place, but not taken until a quota read lets it go:
    /// `waiting-for-space` or `too-big:…` (`space`).
    Space(Reason),
}

impl Outcome {
    /// Ready again at once: the row was rewritten (a fresh guard, a
    /// temporary name, a copy) and runs as it now is.
    pub fn again() -> Self {
        Outcome::Again { state: OutboxState::Ready, reason: None, next_try: None, backoff: false, detail: None }
    }

    /// Waiting (not quiet): looked at again after `after`.
    pub fn wait(reason: Reason, after: Duration) -> Self {
        Outcome::Again { state: OutboxState::Waiting, reason: Some(reason), next_try: Some(now() + after.as_secs() as i64), backoff: false, detail: None }
    }

    pub fn later(reason: Reason, after: Duration) -> Self {
        Outcome::Again { state: OutboxState::Retry, reason: Some(reason), next_try: Some(now() + after.as_secs() as i64), backoff: false, detail: None }
    }

    /// In backoff: 1 s doubling to an hour with each attempt.
    pub fn backoff(reason: Reason) -> Self {
        Outcome::Again { state: OutboxState::Retry, reason: Some(reason), next_try: None, backoff: true, detail: None }
    }

    /// In backoff for a failure: `key` is one of the reasons for a
    /// failure ([`Reason::Network`] and the others), `detail` the error's
    /// own text, which only the journal gets (issue #87).
    pub fn failed(key: Reason, detail: impl ToString) -> Self {
        Outcome::Again { state: OutboxState::Retry, reason: Some(key), next_try: None, backoff: true, detail: Some(detail.to_string()) }
    }

    pub fn blocked(reason: Reason) -> Self {
        Outcome::Again { state: OutboxState::Blocked, reason: Some(reason), next_try: None, backoff: false, detail: None }
    }
}

/// Why a step stopped before it could decide on an [`Outcome`] itself.
#[derive(Debug)]
pub(in crate::upload) enum Fail {
    Write(WriteError),
    Store(TreeError),
    Io(io::Error),
    /// Stop now with this outcome.
    Now(Outcome),
    Crashed,
}

/// How the worker's `last_error` begins while the write gate is closed.
pub(super) const GATE_CLOSED: &str = "nothing is uploaded: ";

impl From<WriteError> for Fail {
    fn from(e: WriteError) -> Self {
        Fail::Write(e)
    }
}

impl From<DriveError> for Fail {
    fn from(e: DriveError) -> Self {
        Fail::Write(e.into())
    }
}

impl From<TreeError> for Fail {
    fn from(e: TreeError) -> Self {
        Fail::Store(e)
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Io(e)
    }
}

impl From<nix::errno::Errno> for Fail {
    fn from(e: nix::errno::Errno) -> Self {
        Fail::Io(e.into())
    }
}

/// What §3.6's table does with an answer no step settled itself.
pub(in crate::upload) fn outcome_of(fail: Fail) -> Outcome {
    match fail {
        Fail::Now(outcome) => outcome,
        Fail::Crashed => Outcome::Crashed,
        Fail::Store(e) => Outcome::failed(Reason::Store, e),
        Fail::Io(e) => Outcome::failed(Reason::LocalIo, e),
        Fail::Write(e) => match e {
            WriteError::QuotaExceeded => Outcome::NoSpace,
            WriteError::Throttled { retry_after } => Outcome::Throttled(retry_after),
            WriteError::Locked => Outcome::backoff(Reason::Locked),
            WriteError::Forbidden => Outcome::Forbidden,
            WriteError::SignedOut => Outcome::SignedOut,
            WriteError::Refused(message) => Outcome::blocked(Reason::Refused(Some(message.to_string()))),
            e @ WriteError::Transient(_) => Outcome::failed(Reason::Network, e),
            // `Failed`, and a `412`, `409`, `404` or ended session no step settled.
            other => Outcome::failed(Reason::Failed, other),
        },
    }
}

/// `text` with every `http://…` and `https://…` cut out, up to the next
/// whitespace: the journal gets no address (issue #87).
pub(in crate::upload) fn without_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = [rest.find("http://"), rest.find("https://")].into_iter().flatten().min() {
        out.push_str(&rest[..at]);
        out.push_str("<url>");
        rest = &rest[at..];
        rest = &rest[rest.find(char::is_whitespace).unwrap_or(rest.len())..];
    }
    out.push_str(rest);
    out
}

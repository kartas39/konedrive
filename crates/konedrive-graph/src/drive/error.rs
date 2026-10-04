//! What an answer from Graph or OneDrive is: its HTTP status ([`Status`]), what the status
//! and Graph's error code mean together ([`classify`]), and the error a read comes back
//! with ([`DriveError`]). A write's error is [`super::WriteError`].

use std::fmt;

use serde::Deserialize;

/// An HTTP status, as a number. Printed as the status line has it (`406 Not Acceptable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Status(u16);

impl Status {
    pub const UNAUTHORIZED: Status = Status(401);
    pub const NOT_ACCEPTABLE: Status = Status(406);

    pub const fn new(code: u16) -> Self {
        Self(code)
    }

    pub const fn code(self) -> u16 {
        self.0
    }

    pub const fn is_success(self) -> bool {
        self.0 >= 200 && self.0 < 300
    }

    pub const fn is_redirection(self) -> bool {
        self.0 >= 300 && self.0 < 400
    }

    pub const fn is_client_error(self) -> bool {
        self.0 >= 400 && self.0 < 500
    }

    pub const fn is_server_error(self) -> bool {
        self.0 >= 500 && self.0 < 600
    }

    pub(crate) fn of(response: &reqwest::Response) -> Self {
        Self(response.status().as_u16())
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match reqwest::StatusCode::from_u16(self.0) {
            Ok(status) => status.fmt(f),
            Err(_) => self.0.fmt(f),
        }
    }
}

/// What an error says, and the answer behind it when there was one: the HTTP status, and
/// Graph's error code (`error.code` of the body) when the body was read and had one.
///
/// It prints, with `{}` and with `{:?}`, as the message alone.
#[derive(Clone, PartialEq, Eq)]
pub struct Detail {
    pub message: String,
    pub status: Option<Status>,
    pub code: Option<String>,
}

impl Detail {
    /// An error made of an answer: its status, and Graph's code if there was one.
    pub(crate) fn answered(message: impl Into<String>, status: Status, code: &str) -> Self {
        Self { message: message.into(), status: Some(status), code: (!code.is_empty()).then(|| code.to_owned()) }
    }
}

impl fmt::Display for Detail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for Detail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.message, f)
    }
}

impl From<String> for Detail {
    fn from(message: String) -> Self {
        Self { message, status: None, code: None }
    }
}

impl From<&str> for Detail {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl PartialEq<str> for Detail {
    fn eq(&self, other: &str) -> bool {
        self.message == other
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("signed out: sign in again")]
    SignedOut,
    #[error("the item is not in OneDrive any more")]
    NotFound,
    #[error("the change feed has expired and the drive must be listed again")]
    ResyncRequired,
    /// `410` with `resyncChangesUploadDifferences` (`docs/design/writes.md` §9): listed
    /// again, and what the new listing leaves out is uploaded rather than
    /// removed. Only a read-write folder tells it from [`Self::ResyncRequired`].
    #[error("the change feed has expired and the drive must be listed again, keeping what it no longer has")]
    ResyncUpload,
    #[error("the download link has expired")]
    UrlExpired,
    #[error("{0}")]
    Transient(Detail),
    #[error("{0}")]
    Failed(Detail),
}

impl DriveError {
    /// The HTTP status of the answer this error was made of. `None` when there was no
    /// answer (no connection, no token, a URL that cannot be parsed) and for the variants
    /// that say what the answer was by themselves.
    pub fn status(&self) -> Option<Status> {
        match self {
            Self::Transient(detail) | Self::Failed(detail) => detail.status,
            _ => None,
        }
    }

    /// Graph's error code, when the answer's body was read and had one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Transient(detail) | Self::Failed(detail) => detail.code.as_deref(),
            _ => None,
        }
    }
}

/// What an answer is, by its status and Graph's error code: the one place the numbers and
/// the codes are named. Each call decides what a kind means for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `401`.
    Unauthorized,
    /// `429` or `503`: wait before the next request.
    Throttled,
    /// `404`.
    NotFound,
    /// `410`.
    Gone,
    /// `412`: an `If-Match` that no longer holds.
    Changed,
    /// `409`, or the code `nameAlreadyExists`.
    NameExists,
    /// `507`, or the code `quotaLimitReached`.
    QuotaExceeded,
    /// `423`.
    Locked,
    /// `403`.
    Forbidden,
    /// `400`.
    BadRequest,
    /// `408`.
    Timeout,
    /// `416`.
    RangeNotSatisfiable,
    /// Any other `5xx`.
    Server,
    /// Anything else: a success, a redirect, another `4xx`.
    Other,
}

/// The kind of an answer. Graph's code decides where it is specific, whatever the status;
/// the status otherwise. A call that did not read the body passes an empty code.
pub(crate) fn classify(status: Status, code: &str) -> Kind {
    match (status.code(), code) {
        (_, "nameAlreadyExists") => Kind::NameExists,
        (_, "quotaLimitReached") => Kind::QuotaExceeded,
        (401, _) => Kind::Unauthorized,
        (429 | 503, _) => Kind::Throttled,
        (404, _) => Kind::NotFound,
        (410, _) => Kind::Gone,
        (412, _) => Kind::Changed,
        (409, _) => Kind::NameExists,
        (507, _) => Kind::QuotaExceeded,
        (423, _) => Kind::Locked,
        (403, _) => Kind::Forbidden,
        (400, _) => Kind::BadRequest,
        (408, _) => Kind::Timeout,
        (416, _) => Kind::RangeNotSatisfiable,
        (500..=599, _) => Kind::Server,
        _ => Kind::Other,
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    error: GraphError,
}

/// Graph's own account of an error: `error.code` and `error.message` of the body.
#[derive(Default, Deserialize)]
pub(crate) struct GraphError {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

/// What the body of an unsuccessful answer says; empty when it cannot be read or is not
/// Graph's error object.
pub(crate) async fn graph_error(response: reqwest::Response) -> GraphError {
    response
        .bytes()
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<ErrorBody>(&body).ok())
        .map(|body| body.error)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;

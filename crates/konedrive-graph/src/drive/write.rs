//! Microsoft Graph's drive API, writing: a new folder (`POST …/children`),
//! rename and move (`PATCH`), delete, and the typed answers the outbox worker
//! acts on. Uploads are in [`super::upload`].
//!
//! Every change is guarded (the write design's WR2): `If-Match` on anything
//! that exists, `conflictBehavior=fail` on anything new, so a guard that fails
//! comes back as [`WriteError::Changed`] or [`WriteError::NameExists`] and is
//! never retried unguarded here — except a folder's delete
//! ([`DriveClient::delete_folder`]), sent with no guard at all: the whole
//! folder goes, as on Windows, and the recycle bin is the safety net.
//! Throttling (`429`, `503`) comes back as [`WriteError::Throttled`] with the
//! wait Graph asked for: the worker pauses the whole account (`docs/design/writes.md` §6.2), so
//! nothing here sleeps ([`Throttle::Return`]).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header;
use serde_json::{json, Map, Value};

use super::error::{classify, graph_error, Detail, Kind, Status};
use super::item::{days_from_civil, format_graph_time};
use super::send::{Auth, Throttle};
use super::{DriveClient, DriveError, DriveItem};

/// The longest wait a `Retry-After` is taken at: a sanity bound against a
/// garbled or hostile header, not a policy (§6.2).
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// What a write can come back with, typed by what the worker does next
/// (§6.2's table of answers).
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// `412`: the item changed in OneDrive since the eTag or cTag the change
    /// was made against. Re-read the item and decide; never retry unguarded.
    #[error("the item changed in OneDrive since it was last read")]
    Changed,
    /// `409`: an item of that name is already there — on a create, at an
    /// upload session's last fragment, or on a rename or move to a taken name.
    #[error("an item of that name is already in OneDrive")]
    NameExists,
    /// `404`: the item, or the folder a create names, is not in OneDrive.
    #[error("the item is not in OneDrive any more")]
    NotFound,
    /// The upload URL takes no more fragments (`404`, or refused): the
    /// session expired, was cancelled, or already completed. Fetch the item,
    /// adopt it if it holds this content, else start a new session.
    #[error("the upload session has ended")]
    SessionGone,
    /// `507`, or `quotaLimitReached`: OneDrive is full.
    #[error("OneDrive is full")]
    QuotaExceeded,
    /// `429` or `503`: wait, for the whole account, at least `retry_after`
    /// when the answer said how long (seconds or an HTTP date, bounded by
    /// [`MAX_RETRY_AFTER`]), else by the worker's own backoff.
    #[error("OneDrive asked to wait before the next request")]
    Throttled { retry_after: Option<Duration> },
    /// `423`: locked, most likely open for co-authoring. Retry later.
    #[error("the item is locked in OneDrive")]
    Locked,
    /// `403`: this account may not change the item, most likely because the
    /// token's scope does not allow writes.
    #[error("OneDrive does not allow this change: sign in again")]
    Forbidden,
    /// `400`: OneDrive refuses the request, most often the name. Carries the
    /// service's own message.
    #[error("OneDrive refused it: {0}")]
    Refused(Detail),
    #[error("signed out: sign in again")]
    SignedOut,
    /// Another `5xx`, a network error, a locked secret store, an answer that
    /// could not be read: try again later.
    #[error("{0}")]
    Transient(Detail),
    #[error("{0}")]
    Failed(Detail),
}

impl From<DriveError> for WriteError {
    fn from(error: DriveError) -> Self {
        match error {
            DriveError::SignedOut => WriteError::SignedOut,
            DriveError::NotFound => WriteError::NotFound,
            DriveError::Transient(detail) => WriteError::Transient(detail),
            DriveError::Failed(detail) => WriteError::Failed(detail),
            other => WriteError::Failed(other.to_string().into()),
        }
    }
}

/// A metadata change of one item: any of a new name, a new parent folder (by
/// its real id, the root included), and the time the file was last changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemChange<'a> {
    pub name: Option<&'a str>,
    pub parent_id: Option<&'a str>,
    /// Unix seconds, sent as `fileSystemInfo.lastModifiedDateTime`.
    pub modified: Option<i64>,
}

impl ItemChange<'_> {
    fn body(&self) -> Value {
        let mut body = Map::new();
        if let Some(name) = self.name {
            body.insert("name".into(), json!(name));
        }
        if let Some(parent) = self.parent_id {
            body.insert("parentReference".into(), json!({ "id": parent }));
        }
        if let Some(modified) = self.modified {
            body.insert("fileSystemInfo".into(), file_system_info(modified));
        }
        Value::Object(body)
    }
}

impl DriveClient {
    /// A new folder `name` in `parent_id`. A name already taken is
    /// [`WriteError::NameExists`] (`conflictBehavior: fail`): the caller
    /// adopts the folder that is there.
    pub async fn create_folder(&self, parent_id: &str, name: &str) -> Result<DriveItem, WriteError> {
        let url = self.item_url(parent_id, Some("children"))?;
        let body = json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        let response = self.send(Auth::Account, Throttle::Return, || self.api.post(url.clone()).json(&body)).await?;
        item_from(response).await
    }

    /// Renames, moves, or sets the time of `id`, guarded by `if_match` (its
    /// eTag, or its cTag). Answers the item as it now is.
    pub async fn update_item(&self, id: &str, if_match: &str, change: &ItemChange<'_>) -> Result<DriveItem, WriteError> {
        let url = self.item_url(id, None)?;
        let body = change.body();
        let response = self
            .send(Auth::Account, Throttle::Return, || self.api.patch(url.clone()).header(header::IF_MATCH, if_match).json(&body))
            .await?;
        item_from(response).await
    }

    /// Deletes a file into OneDrive's recycle bin, guarded by `if_match`: its
    /// eTag. [`WriteError::NotFound`] means it is already gone.
    pub async fn delete_item(&self, id: &str, if_match: &str) -> Result<(), WriteError> {
        let url = self.item_url(id, None)?;
        let response = self
            .send(Auth::Account, Throttle::Return, || self.api.delete(url.clone()).header(header::IF_MATCH, if_match))
            .await?;
        done(response).await
    }

    /// Deletes folder `id`, whole, into OneDrive's recycle bin: no
    /// `If-Match`, whatever changed inside it in OneDrive since — as Windows
    /// deletes a folder ([decisions.md](../../../../docs/design/decisions.md),
    /// "A folder delete is the whole folder, as on Windows"). The recycle bin
    /// is the safety net. [`WriteError::NotFound`] means it is already gone.
    pub async fn delete_folder(&self, id: &str) -> Result<(), WriteError> {
        let url = self.item_url(id, None)?;
        let response = self.send(Auth::Account, Throttle::Return, || self.api.delete(url.clone())).await?;
        done(response).await
    }
}

/// A successful answer with nothing to read, or the error an unsuccessful one means.
async fn done(response: reqwest::Response) -> Result<(), WriteError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(error_from(response).await)
    }
}

/// `{"lastModifiedDateTime": …}` for `modified`, in Unix seconds.
pub(super) fn file_system_info(modified: i64) -> Value {
    json!({ "lastModifiedDateTime": format_graph_time(modified) })
}

/// The item a successful answer carries, or the error an unsuccessful one
/// means.
pub(super) async fn item_from(response: reqwest::Response) -> Result<DriveItem, WriteError> {
    if !response.status().is_success() {
        return Err(error_from(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| WriteError::Transient(format!("an unreadable answer from Graph: {}", e.without_url()).into()))
}

/// What an unsuccessful answer means (§6.2). Graph's error code decides where
/// it is specific; the status otherwise.
pub(super) async fn error_from(response: reqwest::Response) -> WriteError {
    let status = Status::of(&response);
    let wait = retry_after(response.headers(), SystemTime::now());
    let graph = graph_error(response).await;
    let code = graph.code.as_str();
    let answered = |message: String| Detail::answered(message, status, code);
    match classify(status, code) {
        Kind::NameExists => WriteError::NameExists,
        Kind::QuotaExceeded => WriteError::QuotaExceeded,
        Kind::Changed => WriteError::Changed,
        Kind::NotFound => WriteError::NotFound,
        Kind::Throttled => WriteError::Throttled { retry_after: wait },
        Kind::Locked => WriteError::Locked,
        Kind::Forbidden => WriteError::Forbidden,
        Kind::BadRequest if graph.message.is_empty() => WriteError::Refused(answered(format!("Graph returned {status} {code}"))),
        Kind::BadRequest => WriteError::Refused(answered(graph.message.clone())),
        Kind::Unauthorized => WriteError::Failed(answered("Microsoft Graph rejected the access token".into())),
        _ if status.is_server_error() => WriteError::Transient(answered(format!("Graph returned {status} {code}"))),
        _ => WriteError::Failed(answered(format!("Graph returned {status} {code}"))),
    }
}

/// How long `Retry-After` asks to wait, given as seconds or as an HTTP date
/// (RFC 9110 §10.2.3), from `now`, at most [`MAX_RETRY_AFTER`]. A date already
/// past is no wait. `None` when the header is missing or unreadable.
pub(super) fn retry_after(headers: &header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    let wait = match value.parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        Err(_) => parse_http_date(value)?.duration_since(now).unwrap_or(Duration::ZERO),
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// An HTTP date in any of the three forms RFC 9110 has recipients accept:
/// `Sun, 06 Nov 1994 08:49:37 GMT`, `Sunday, 06-Nov-94 08:49:37 GMT` and
/// `Sun Nov  6 08:49:37 1994`. In all three the day comes before the year and
/// the words other than the month (the weekday, `GMT`) say nothing.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let (mut month, mut time, mut numbers) = (None, None, Vec::new());
    for token in value.split([' ', ',', '-']).filter(|token| !token.is_empty()) {
        if let Some(index) = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(token)) {
            month = Some(index as i64 + 1);
        } else if token.contains(':') {
            time = Some(token);
        } else if token.bytes().all(|b| b.is_ascii_digit()) {
            numbers.push(token.parse::<i64>().ok()?);
        } else if !token.bytes().all(|b| b.is_ascii_alphabetic()) {
            return None;
        }
    }
    let [day, year] = numbers[..] else { return None };
    // RFC 850's two-digit year.
    let year = match year {
        0..=69 => year + 2000,
        70..=99 => year + 1900,
        _ => year,
    };
    let mut clock = time?.splitn(3, ':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let seconds = days_from_civil(year, month?, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).ok()?))
}

#[cfg(test)]
mod tests;

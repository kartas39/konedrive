//! Microsoft Graph's drive API, writing: a new folder (`POST …/children`),
//! rename and move (`PATCH`), delete, and the typed answers the outbox worker
//! acts on. Uploads are in [`super::upload`].
//!
//! Every change is guarded (the write design's WR2): `If-Match` on anything
//! that exists, `conflictBehavior=fail` on anything new, so a guard that fails
//! comes back as [`WriteError::Changed`] or [`WriteError::NameExists`] and is
//! never retried unguarded here. Throttling (`429`, `503`) comes back as
//! [`WriteError::Throttled`] with the wait Graph asked for: the worker pauses
//! the whole account (§4.10), so nothing here sleeps.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{header, StatusCode};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::item::{days_from_civil, format_graph_time};
use super::{DriveClient, DriveError, DriveItem};

/// The longest wait a `Retry-After` is taken at: a sanity bound against a
/// garbled or hostile header, not a policy (§4.10).
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// What a write can come back with, typed by what the worker does next
/// (§3.6's table of answers).
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
    Refused(String),
    #[error("signed out: sign in again")]
    SignedOut,
    /// Another `5xx`, a network error, a locked secret store, an answer that
    /// could not be read: try again later.
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Failed(String),
}

impl From<DriveError> for WriteError {
    fn from(error: DriveError) -> Self {
        match error {
            DriveError::SignedOut => WriteError::SignedOut,
            DriveError::NotFound => WriteError::NotFound,
            DriveError::Transient(message) => WriteError::Transient(message),
            other => WriteError::Failed(other.to_string()),
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
        let response = self.send_write(|token| self.api.post(url.clone()).bearer_auth(token).json(&body)).await?;
        item_from(response).await
    }

    /// Renames, moves, or sets the time of `id`, guarded by `if_match` (its
    /// eTag, or its cTag). Answers the item as it now is.
    pub async fn update_item(&self, id: &str, if_match: &str, change: &ItemChange<'_>) -> Result<DriveItem, WriteError> {
        let url = self.item_url(id, None)?;
        let body = change.body();
        let response = self
            .send_write(|token| {
                self.api.patch(url.clone()).bearer_auth(token).header(header::IF_MATCH, if_match).json(&body)
            })
            .await?;
        item_from(response).await
    }

    /// Deletes `id` into OneDrive's recycle bin, guarded by `if_match`: a
    /// file's eTag, or a folder's cTag, which changes with any descendant.
    /// [`WriteError::NotFound`] means it is already gone.
    pub async fn delete_item(&self, id: &str, if_match: &str) -> Result<(), WriteError> {
        let url = self.item_url(id, None)?;
        let response = self
            .send_write(|token| self.api.delete(url.clone()).bearer_auth(token).header(header::IF_MATCH, if_match))
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(error_from(response).await)
        }
    }

    /// Sends a write with the account's token. A `401` is answered once by
    /// dropping the cached token and asking again, as reads do; every other
    /// answer, throttling included, goes back to the caller as it is.
    pub(super) async fn send_write(
        &self,
        request: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, WriteError> {
        let mut renewed = false;
        loop {
            let token = self.token().await?;
            let response = request(&token)
                .send()
                .await
                .map_err(|e| WriteError::Transient(format!("cannot reach Microsoft Graph: {e}")))?;
            if response.status() == StatusCode::UNAUTHORIZED && !renewed {
                renewed = true;
                self.tokens.invalidate().await;
                continue;
            }
            return Ok(response);
        }
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
        .map_err(|e| WriteError::Transient(format!("an unreadable answer from Graph: {e}")))
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// What an unsuccessful answer means (§3.6). Graph's error code decides where
/// it is specific; the status otherwise.
pub(super) async fn error_from(response: reqwest::Response) -> WriteError {
    let status = response.status();
    let wait = retry_after(response.headers(), SystemTime::now());
    let detail = response
        .bytes()
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<ErrorBody>(&body).ok())
        .map(|body| body.error)
        .unwrap_or(ErrorDetail { code: String::new(), message: String::new() });
    match (status, detail.code.as_str()) {
        (_, "nameAlreadyExists") => WriteError::NameExists,
        (_, "quotaLimitReached") => WriteError::QuotaExceeded,
        (StatusCode::PRECONDITION_FAILED, _) => WriteError::Changed,
        (StatusCode::CONFLICT, _) => WriteError::NameExists,
        (StatusCode::NOT_FOUND, _) => WriteError::NotFound,
        (StatusCode::INSUFFICIENT_STORAGE, _) => WriteError::QuotaExceeded,
        (StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE, _) => {
            WriteError::Throttled { retry_after: wait }
        }
        (StatusCode::LOCKED, _) => WriteError::Locked,
        (StatusCode::FORBIDDEN, _) => WriteError::Forbidden,
        (StatusCode::BAD_REQUEST, code) => WriteError::Refused(if detail.message.is_empty() {
            format!("Graph returned {status} {code}")
        } else {
            detail.message
        }),
        (StatusCode::UNAUTHORIZED, _) => WriteError::Failed("Microsoft Graph rejected the access token".into()),
        (status, code) if status.is_server_error() => WriteError::Transient(format!("Graph returned {status} {code}")),
        (status, code) => WriteError::Failed(format!("Graph returned {status} {code}")),
    }
}

/// How long `Retry-After` asks to wait, given as seconds or as an HTTP date
/// (RFC 9110 §10.2.3), from `now`, at most [`MAX_RETRY_AFTER`]. A date already
/// past is no wait. `None` when the header is missing or unreadable.
pub fn retry_after(headers: &header::HeaderMap, now: SystemTime) -> Option<Duration> {
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
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use url::Url;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::token::StaticToken;

    fn client(server: &MockServer) -> DriveClient {
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        DriveClient::new(base, Arc::new(StaticToken::new("T"))).unwrap()
    }

    #[tokio::test]
    async fn a_new_folder_is_posted_with_conflict_behaviour_fail() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/me/drive/items/P!1/children")).and(header("authorization", "Bearer T"))
            .and(body_json(json!({"name": "Фото", "folder": {}, "@microsoft.graph.conflictBehavior": "fail"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "F", "name": "Фото", "eTag": "e1", "folder": {}})))
            .mount(&server).await;
        let folder = client(&server).create_folder("P!1", "Фото").await.unwrap();
        assert_eq!((folder.id.as_str(), folder.e_tag.as_deref()), ("F", Some("e1")));
    }

    #[tokio::test]
    async fn a_rename_and_move_is_one_patch_guarded_by_the_etag() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH")).and(path("/me/drive/items/I")).and(header("if-match", "e1"))
            .and(body_json(json!({
                "name": "b.txt", "parentReference": {"id": "Q"},
                "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "I", "name": "b.txt", "eTag": "e2"})))
            .mount(&server).await;
        let change = ItemChange { name: Some("b.txt"), parent_id: Some("Q"), modified: Some(1_714_557_600) };
        let item = client(&server).update_item("I", "e1", &change).await.unwrap();
        assert_eq!(item.e_tag.as_deref(), Some("e2"));
    }

    #[tokio::test]
    async fn a_delete_carries_its_guard() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE")).and(path("/me/drive/items/D")).and(header("if-match", "c7"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server).await;
        client(&server).delete_item("D", "c7").await.unwrap();
    }

    /// The error a `DELETE` answered with `answer` comes back as, checking
    /// that it was sent exactly once: nothing here retries or waits.
    async fn refusal(answer: ResponseTemplate) -> WriteError {
        let server = MockServer::start().await;
        Mock::given(method("DELETE")).respond_with(answer).mount(&server).await;
        let err = client(&server).delete_item("X", "e").await.unwrap_err();
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "{err:?}: sent once");
        err
    }

    /// §3.6's answers, each to the error the worker acts on. A throttle is
    /// handed back with its wait rather than waited out here.
    #[tokio::test]
    async fn every_refusal_comes_back_as_its_typed_error() {
        let error = |code: &str, message: &str| json!({"error": {"code": code, "message": message}});
        let answer = ResponseTemplate::new;
        assert!(matches!(refusal(answer(412)).await, WriteError::Changed));
        let taken = answer(409).set_body_json(error("nameAlreadyExists", "taken"));
        assert!(matches!(refusal(taken).await, WriteError::NameExists));
        assert!(matches!(refusal(answer(404)).await, WriteError::NotFound));
        assert!(matches!(refusal(answer(507)).await, WriteError::QuotaExceeded));
        let full = answer(403).set_body_json(error("quotaLimitReached", "full"));
        assert!(matches!(refusal(full).await, WriteError::QuotaExceeded));
        assert!(matches!(refusal(answer(423)).await, WriteError::Locked));
        assert!(matches!(refusal(answer(403)).await, WriteError::Forbidden));
        let refused = refusal(answer(400).set_body_json(error("invalidRequest", "The name is not valid"))).await;
        assert!(matches!(&refused, WriteError::Refused(m) if m == "The name is not valid"), "{refused:?}");
        assert!(matches!(refusal(answer(502)).await, WriteError::Transient(_)));
        let throttled = refusal(answer(429).insert_header("retry-after", "7")).await;
        assert!(
            matches!(throttled, WriteError::Throttled { retry_after: Some(d) } if d == Duration::from_secs(7)),
            "{throttled:?}"
        );
        assert!(matches!(refusal(answer(503)).await, WriteError::Throttled { retry_after: None }));
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777); // Sun, 06 Nov 1994 08:49:37 GMT
        let wait = |value: &str| {
            let mut headers = header::HeaderMap::new();
            headers.insert(header::RETRY_AFTER, value.parse().unwrap());
            retry_after(&headers, now)
        };
        assert_eq!(wait("120"), Some(Duration::from_secs(120)));
        assert_eq!(wait("Sun, 06 Nov 1994 08:50:07 GMT"), Some(Duration::from_secs(30)));
        assert_eq!(wait("Sunday, 06-Nov-94 08:50:07 GMT"), Some(Duration::from_secs(30)), "RFC 850");
        assert_eq!(wait("Sun Nov  6 08:50:07 1994"), Some(Duration::from_secs(30)), "asctime");
        assert_eq!(wait("Sun, 06 Nov 1994 08:00:00 GMT"), Some(Duration::ZERO), "already past");
        assert_eq!(wait("999999"), Some(MAX_RETRY_AFTER), "bounded");
        assert_eq!(wait("soon"), None);
        assert_eq!(retry_after(&header::HeaderMap::new(), now), None);
    }
}

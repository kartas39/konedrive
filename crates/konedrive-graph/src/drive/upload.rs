//! Microsoft Graph's upload sessions (the write design's §3.6 and §4.8): every
//! non-empty file goes through one, so that a new file carries
//! `conflictBehavior: fail`, a changed one `If-Match`, and both their time as
//! `fileSystemInfo`. The caller opens the session
//! ([`DriveClient::create_upload_session`]), persists it before its first
//! byte, and sends the file as fragments of at most [`CHUNK_SIZE`] — one
//! fragment up to [`SMALL_UPLOAD_MAX`] — resuming from
//! [`DriveClient::upload_status`] after an interruption. An empty file, which
//! a session cannot carry, is a `PUT` followed by a `PATCH` for its time
//! ([`DriveClient::upload_empty`]).
//!
//! An open session holds its name in OneDrive with an empty placeholder until
//! it completes or is cancelled (issue #47), so a session is never simply
//! dropped: a fragment OneDrive refuses for now (`429`, `503`, a dropped
//! connection, a timeout) is sent again to the same session
//! ([`DriveClient::upload_chunk`]), and a session given up is cancelled
//! ([`DriveClient::cancel_upload`]).
//!
//! The upload URL is pre-authenticated: it never gets the account's token
//! (Microsoft: that can cause a `401`), and, being a credential for that one
//! file until it expires, it is never logged ([`UploadSession`]'s `Debug`
//! leaves it out, and so do the errors).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{header, StatusCode};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::item::parse_graph_time;
use super::write::{error_from, file_system_info, item_from, ItemChange, WriteError};
use super::{DriveClient, DriveItem};

/// Microsoft's unit for fragments: every one but the last is a multiple of it.
pub const FRAGMENT_UNIT: u64 = 320 * 1024;

/// The fragment the caller sends: 32 × 320 KiB = 10 MiB, the top of the 5–10
/// MiB Microsoft recommends.
pub const CHUNK_SIZE: u64 = 32 * FRAGMENT_UNIT;

/// Up to this size a file goes up in one request; above it, in fragments.
/// Microsoft's own boundary for resumable transfers.
pub const SMALL_UPLOAD_MAX: u64 = CHUNK_SIZE;

/// Microsoft's bound on one request's body: under 60 MiB.
const MAX_FRAGMENT: u64 = 60 * 1024 * 1024;

/// A session's bookkeeping requests (status, cancel) are small: they get the
/// metadata calls' bound rather than a fragment's.
const SESSION_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// What an upload goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadTarget<'a> {
    /// A new file `name` in the folder `parent_id`, refused with
    /// [`WriteError::NameExists`] if the name is taken.
    New { parent_id: &'a str, name: &'a str },
    /// New content for the item `id`, refused with [`WriteError::Changed`]
    /// unless its eTag is still `if_match`.
    Existing { id: &'a str, if_match: &'a str },
}

/// An open upload session: where its fragments go, and when it expires if
/// nothing more is sent (Unix seconds).
#[derive(Clone, PartialEq, Eq)]
pub struct UploadSession {
    pub url: String,
    pub expires: Option<i64>,
}

impl fmt::Debug for UploadSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadSession").field("url", &"<not shown>").field("expires", &self.expires).finish()
    }
}

/// Where a session stands: the first byte it still expects, and its expiry,
/// which every fragment extends (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionProgress {
    pub next: u64,
    pub expires: Option<i64>,
}

/// What a fragment's answer says.
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkOutcome {
    /// More is expected, from `next` on.
    More(SessionProgress),
    /// The last fragment landed: the item as it now is.
    Done(Box<DriveItem>),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionBody {
    upload_url: String,
    #[serde(default)]
    expiration_date_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressBody {
    #[serde(default)]
    expiration_date_time: Option<String>,
    #[serde(default)]
    next_expected_ranges: Vec<String>,
}

impl ProgressBody {
    fn expires(&self) -> Option<i64> {
        self.expiration_date_time.as_deref().and_then(parse_graph_time)
    }

    /// The start of the first missing range (`"26-"`, `"12345-55232"`):
    /// fragments go in order, so the rest follows from it.
    fn next(&self) -> Option<u64> {
        self.next_expected_ranges.first()?.split('-').next()?.trim().parse().ok()
    }
}

impl DriveClient {
    /// Opens a session for `size` bytes. The session request carries the
    /// guard and the time, but not the size: a personal drive answers
    /// `fileSize` with `400 invalidRequest` (measured on a test account,
    /// although Microsoft's documentation lists it), so a full drive shows
    /// itself only when a fragment is refused.
    pub async fn create_upload_session(
        &self,
        target: UploadTarget<'_>,
        _size: u64,
        modified: i64,
    ) -> Result<UploadSession, WriteError> {
        let (url, item, if_match) = match target {
            UploadTarget::New { parent_id, name } => (
                self.child_url(parent_id, name, Some("createUploadSession"))?,
                json!({
                    "@microsoft.graph.conflictBehavior": "fail",
                    "name": name,
                    "fileSystemInfo": file_system_info(modified),
                }),
                None,
            ),
            // By id, the only item there is to replace is this one; `If-Match`
            // is the guard. Said explicitly because `fail` is the default.
            UploadTarget::Existing { id, if_match } => (
                self.item_url(id, Some("createUploadSession"))?,
                json!({
                    "@microsoft.graph.conflictBehavior": "replace",
                    "fileSystemInfo": file_system_info(modified),
                }),
                Some(if_match),
            ),
        };
        let body = json!({ "item": item });
        let response = self
            .send_write(|token| {
                let request = self.api.post(url.clone()).bearer_auth(token).json(&body);
                match if_match {
                    Some(tag) => request.header(header::IF_MATCH, tag),
                    None => request,
                }
            })
            .await?;
        if !response.status().is_success() {
            return Err(error_from(response).await);
        }
        let session: SessionBody = response
            .json()
            .await
            .map_err(|e| WriteError::Transient(format!("an unreadable upload session from Graph: {}", e.without_url())))?;
        Ok(UploadSession {
            url: session.upload_url,
            expires: session.expiration_date_time.as_deref().and_then(parse_graph_time),
        })
    }

    /// Sends the bytes of the file of `total` bytes that start at `offset`.
    /// Every fragment but the last must be a multiple of [`FRAGMENT_UNIT`];
    /// one that is not is refused before anything is sent. A fragment the
    /// session already has (`416`) is answered with where the session stands.
    ///
    /// A fragment refused for now — `429` or `503`, a dropped connection, a
    /// timeout — goes again to the same session (issue #47): after
    /// `Retry-After` (or the policy's wait) the session is asked where it
    /// stands, and the fragment is sent again if it still expects it, up to
    /// the policy's `attempts` sends in all. A session that moved on
    /// meanwhile is answered with where it stands; one that ended (the last
    /// fragment went in, the answer lost) is [`WriteError::SessionGone`]. If
    /// the fragment still does not go through, that refusal comes back and
    /// the session stays open: the caller keeps it for its next run.
    pub async fn upload_chunk(
        &self,
        session_url: &str,
        offset: u64,
        total: u64,
        chunk: Vec<u8>,
    ) -> Result<ChunkOutcome, WriteError> {
        let len = chunk.len() as u64;
        let end = offset
            .checked_add(len)
            .filter(|&end| len > 0 && end <= total)
            .ok_or_else(|| WriteError::Failed(format!("{len} bytes at {offset} do not fit a file of {total}")))?;
        if len >= MAX_FRAGMENT || (end < total && !len.is_multiple_of(FRAGMENT_UNIT)) {
            return Err(WriteError::Failed(format!(
                "a fragment of {len} bytes: each must be under 60 MiB, and all but the last a multiple of 320 KiB"
            )));
        }
        let chunk = Arc::new(chunk);
        let mut sent = 0;
        loop {
            sent += 1;
            // The body goes out in pieces, each counted into the pool's speed as it is taken.
            let request = self
                .upload
                .put(session_url_of(session_url)?)
                .header(header::CONTENT_RANGE, format!("bytes {offset}-{}/{total}", end - 1))
                .header(header::CONTENT_LENGTH, len.to_string())
                .body(self.metered(Arc::clone(&chunk)));
            let (refusal, wait) = match send_to_session(request).await {
                Ok(response) => {
                    self.answered(&response);
                    match response.status() {
                        StatusCode::ACCEPTED => {
                            let body = progress_from(response).await?;
                            return Ok(ChunkOutcome::More(SessionProgress { next: body.next().unwrap_or(end), expires: body.expires() }));
                        }
                        StatusCode::OK | StatusCode::CREATED => {
                            return item_from(response).await.map(|item| ChunkOutcome::Done(Box::new(item)));
                        }
                        StatusCode::RANGE_NOT_SATISFIABLE => return self.upload_status(session_url).await.map(ChunkOutcome::More),
                        status if session_ended(status) => return Err(WriteError::SessionGone),
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                            let wait = self.wait_for(&response);
                            (error_from(response).await, wait)
                        }
                        _ => return Err(error_from(response).await),
                    }
                }
                // A dropped connection, a timeout: the fragment may or may not have gone in.
                Err(lost) => (lost, self.retry.default_wait.min(self.retry.max_wait)),
            };
            if sent >= self.retry.attempts {
                return Err(refusal);
            }
            tracing::debug!("a fragment at {offset} was not taken ({refusal}); it goes again to the same session");
            tokio::time::sleep(wait).await;
            let progress = self.upload_status(session_url).await?;
            if progress.next != offset {
                return Ok(ChunkOutcome::More(progress));
            }
        }
    }

    /// Where a session stands: what to resume from after an interruption.
    pub async fn upload_status(&self, session_url: &str) -> Result<SessionProgress, WriteError> {
        let request = self.upload.get(session_url_of(session_url)?).timeout(SESSION_CALL_TIMEOUT);
        let response = send_to_session(request).await?;
        self.answered(&response);
        match response.status() {
            StatusCode::OK => {
                let body = progress_from(response).await?;
                // Nothing missing, yet not completed: it takes no more fragments.
                let next = body.next().ok_or(WriteError::SessionGone)?;
                Ok(SessionProgress { next, expires: body.expires() })
            }
            status if session_ended(status) => Err(WriteError::SessionGone),
            _ => Err(error_from(response).await),
        }
    }

    /// Abandons a session, dropping what it holds. One already gone is done.
    pub async fn cancel_upload(&self, session_url: &str) -> Result<(), WriteError> {
        let request = self.upload.delete(session_url_of(session_url)?).timeout(SESSION_CALL_TIMEOUT);
        let response = send_to_session(request).await?;
        self.answered(&response);
        match response.status() {
            status if status.is_success() || session_ended(status) => Ok(()),
            _ => Err(error_from(response).await),
        }
    }

    /// An empty file: `PUT` its (lack of) content — guarded by
    /// `conflictBehavior=fail` in the URL for a new one, where `PUT`'s default
    /// is `replace`, or by `If-Match` — then `PATCH` its time. The content is
    /// what matters: a failed `PATCH` is logged and the `PUT`'s answer
    /// returned, leaving OneDrive's own time on the item.
    pub async fn upload_empty(&self, target: UploadTarget<'_>, modified: i64) -> Result<DriveItem, WriteError> {
        let (url, if_match) = match target {
            UploadTarget::New { parent_id, name } => {
                let mut url = self.child_url(parent_id, name, Some("content"))?;
                url.set_query(Some("@microsoft.graph.conflictBehavior=fail"));
                (url, None)
            }
            UploadTarget::Existing { id, if_match } => (self.item_url(id, Some("content"))?, Some(if_match)),
        };
        let response = self
            .send_write(|token| {
                let request = self
                    .api
                    .put(url.clone())
                    .bearer_auth(token)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, "0")
                    .body(Vec::new());
                match if_match {
                    Some(tag) => request.header(header::IF_MATCH, tag),
                    None => request,
                }
            })
            .await?;
        let item = item_from(response).await?;
        let Some(etag) = item.e_tag.clone() else { return Ok(item) };
        let change = ItemChange { modified: Some(modified), ..ItemChange::default() };
        match self.update_item(&item.id, &etag, &change).await {
            Ok(dated) => Ok(dated),
            Err(e) => {
                tracing::warn!(item = %item.id, "an empty file went up, but its time did not: {e}");
                Ok(item)
            }
        }
    }
}

impl DriveClient {
    /// `chunk` as a request body that goes out in pieces of [`METER_PIECE`], each counted
    /// into the pool's upload speed as it is taken.
    fn metered(&self, chunk: Arc<Vec<u8>>) -> reqwest::Body {
        let pool = Arc::clone(self.pool());
        let len = chunk.len();
        let pieces = futures_util::stream::iter((0..len).step_by(METER_PIECE).map(move |at| {
            let end = (at + METER_PIECE).min(len);
            let piece = chunk[at..end].to_vec();
            pool.moved(crate::pool::Direction::Up, piece.len() as u64);
            Ok::<_, std::io::Error>(piece)
        }));
        reqwest::Body::wrap_stream(pieces)
    }
}

/// The pieces an upload's body goes out in, for its speed.
const METER_PIECE: usize = 256 * 1024;

/// A session URL that answers these no longer takes fragments: gone (`404`,
/// `410`) or refused (`401`, `403`: its own authorisation has lapsed).
fn session_ended(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE)
}

fn session_url_of(url: &str) -> Result<Url, WriteError> {
    Url::parse(url).map_err(|_| WriteError::Failed("an upload URL from Graph cannot be parsed".into()))
}

/// Sends a request to a session URL: no token, and no URL in the error.
async fn send_to_session(request: reqwest::RequestBuilder) -> Result<reqwest::Response, WriteError> {
    request
        .send()
        .await
        .map_err(|e| WriteError::Transient(format!("cannot reach OneDrive's upload service: {}", e.without_url())))
}

async fn progress_from(response: reqwest::Response) -> Result<ProgressBody, WriteError> {
    response
        .json()
        .await
        .map_err(|e| WriteError::Transient(format!("an unreadable upload status: {}", e.without_url())))
}

#[cfg(test)]
mod tests;

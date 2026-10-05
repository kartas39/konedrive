//! Microsoft Graph's upload sessions (the write design's §3.6 and §4.8): every
//! non-empty file goes through one, so that a new file carries
//! `conflictBehavior: fail`, a changed one `If-Match`, and both their time as
//! `fileSystemInfo`. The caller opens the session
//! ([`DriveClient::create_upload_session`]), persists it before its first
//! byte, and sends the file as fragments of at most [`CHUNK_SIZE`] — one
//! fragment up to that size — resuming from
//! [`DriveClient::upload_status`] after an interruption. An empty file, which
//! a session cannot carry, is a `PUT` followed by a `PATCH` for its time
//! ([`DriveClient::upload_empty`]).
//!
//! An open session holds its name in OneDrive with an empty placeholder until
//! it completes or is cancelled, so a session is never simply
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
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::error::{classify, Kind, Status};
use super::item::parse_graph_time;
use super::send::{Auth, Throttle};
use super::write::{error_from, file_system_info, item_from, ItemChange, WriteError};
use super::{DriveClient, DriveItem};

/// Microsoft's unit for fragments: every one but the last is a multiple of it.
pub const FRAGMENT_UNIT: u64 = 320 * 1024;

/// The fragment the caller sends: 32 × 320 KiB = 10 MiB, the top of the 5–10
/// MiB Microsoft recommends. A file up to this size goes up in one request.
pub const CHUNK_SIZE: u64 = 32 * FRAGMENT_UNIT;

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
    /// Opens a session. The session request carries the guard and the time,
    /// but not the file's size: a personal drive answers `fileSize` with
    /// `400 invalidRequest` (measured on a test account, although Microsoft's
    /// documentation lists it), so a full drive shows itself only when a
    /// fragment is refused.
    pub async fn create_upload_session(&self, target: UploadTarget<'_>, modified: i64) -> Result<UploadSession, WriteError> {
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
            .send(Auth::Account, Throttle::Return, || {
                let request = self.api.post(url.clone()).json(&body);
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
            .map_err(|e| WriteError::Transient(format!("an unreadable upload session from Graph: {}", e.without_url()).into()))?;
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
    /// timeout — goes again to the same session: after
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
        let end = fragment_end(offset, chunk.len() as u64, total)?;
        let url = session_url_of(session_url)?;
        let chunk = Arc::new(chunk);
        let mut sent = 1;
        loop {
            match self.send_fragment(&url, session_url, offset..end, total, &chunk).await {
                Fragment::Settled(outcome) => break outcome,
                Fragment::NotTaken { refusal, .. } if sent >= self.retry.attempts => break Err(refusal),
                Fragment::NotTaken { refusal, wait } => {
                    tracing::debug!("a fragment at {offset} was not taken ({refusal}); it goes again to the same session");
                    tokio::time::sleep(wait).await;
                }
            }
            match self.upload_status(session_url).await {
                Ok(progress) if progress.next == offset => sent += 1,
                moved_on => break moved_on.map(ChunkOutcome::More),
            }
        }
    }

    /// One send of the fragment `range` of a file of `total` bytes: what its answer
    /// settles, or the refusal a later send may get past.
    async fn send_fragment(&self, url: &Url, session_url: &str, range: Range<u64>, total: u64, chunk: &Arc<Vec<u8>>) -> Fragment {
        // The body goes out in pieces, each counted into the pool's speed as it is taken.
        let sent = self
            .send(Auth::Session, Throttle::Return, || {
                self.upload
                    .put(url.clone())
                    .header(header::CONTENT_RANGE, format!("bytes {}-{}/{total}", range.start, range.end - 1))
                    .header(header::CONTENT_LENGTH, chunk.len().to_string())
                    .body(self.metered(Arc::clone(chunk)))
            })
            .await;
        let response = match sent {
            Ok(response) => response,
            // A dropped connection, a timeout: the fragment may or may not have gone in.
            Err(lost) => return Fragment::NotTaken { refusal: lost.into(), wait: self.retry.default_wait.min(self.retry.max_wait) },
        };
        let status = Status::of(&response);
        Fragment::Settled(match (status.code(), classify(status, "")) {
            (202, _) => progress_from(response)
                .await
                .map(|body| ChunkOutcome::More(SessionProgress { next: body.next().unwrap_or(range.end), expires: body.expires() })),
            (200 | 201, _) => item_from(response).await.map(|item| ChunkOutcome::Done(Box::new(item))),
            (_, Kind::RangeNotSatisfiable) => self.upload_status(session_url).await.map(ChunkOutcome::More),
            (_, kind) if session_ended(kind) => Err(WriteError::SessionGone),
            (_, Kind::Throttled) => {
                let wait = self.wait_for(&response);
                return Fragment::NotTaken { refusal: error_from(response).await, wait };
            }
            _ => Err(error_from(response).await),
        })
    }

    /// Where a session stands: what to resume from after an interruption.
    pub async fn upload_status(&self, session_url: &str) -> Result<SessionProgress, WriteError> {
        let url = session_url_of(session_url)?;
        let response =
            self.send(Auth::Session, Throttle::Return, || self.upload.get(url.clone()).timeout(SESSION_CALL_TIMEOUT)).await?;
        let status = Status::of(&response);
        match (status.code(), classify(status, "")) {
            (200, _) => {
                let body = progress_from(response).await?;
                // Nothing missing, yet not completed: it takes no more fragments.
                let next = body.next().ok_or(WriteError::SessionGone)?;
                Ok(SessionProgress { next, expires: body.expires() })
            }
            (_, kind) if session_ended(kind) => Err(WriteError::SessionGone),
            _ => Err(error_from(response).await),
        }
    }

    /// Abandons a session, dropping what it holds. One already gone is done.
    pub async fn cancel_upload(&self, session_url: &str) -> Result<(), WriteError> {
        let url = session_url_of(session_url)?;
        let response =
            self.send(Auth::Session, Throttle::Return, || self.upload.delete(url.clone()).timeout(SESSION_CALL_TIMEOUT)).await?;
        let status = Status::of(&response);
        if status.is_success() || session_ended(classify(status, "")) {
            Ok(())
        } else {
            Err(error_from(response).await)
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
            .send(Auth::Account, Throttle::Return, || {
                let request = self
                    .api
                    .put(url.clone())
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

/// What one send of a fragment came to.
enum Fragment {
    /// The answer settles the call: the session moved on or completed, or refused the
    /// fragment for good.
    Settled(Result<ChunkOutcome, WriteError>),
    /// Refused for now (`429`, `503`), or no answer: after `wait` the session is asked
    /// where it stands.
    NotTaken { refusal: WriteError, wait: Duration },
}

/// Where the fragment of `len` bytes at `offset` ends in a file of `total`; refused, before
/// anything is sent, if it does not fit the file or Microsoft's bounds on a fragment.
fn fragment_end(offset: u64, len: u64, total: u64) -> Result<u64, WriteError> {
    let end = offset
        .checked_add(len)
        .filter(|&end| len > 0 && end <= total)
        .ok_or_else(|| WriteError::Failed(format!("{len} bytes at {offset} do not fit a file of {total}").into()))?;
    if len >= MAX_FRAGMENT || (end < total && !len.is_multiple_of(FRAGMENT_UNIT)) {
        return Err(WriteError::Failed(
            format!("a fragment of {len} bytes: each must be under 60 MiB, and all but the last a multiple of 320 KiB").into(),
        ));
    }
    Ok(end)
}

/// A session URL that answers these no longer takes fragments: gone (`404`,
/// `410`) or refused (`401`, `403`: its own authorisation has lapsed).
fn session_ended(kind: Kind) -> bool {
    matches!(kind, Kind::Unauthorized | Kind::Forbidden | Kind::NotFound | Kind::Gone)
}

fn session_url_of(url: &str) -> Result<Url, WriteError> {
    Url::parse(url).map_err(|_| WriteError::Failed("an upload URL from Graph cannot be parsed".into()))
}

async fn progress_from(response: reqwest::Response) -> Result<ProgressBody, WriteError> {
    response
        .json()
        .await
        .map_err(|e| WriteError::Transient(format!("an unreadable upload status: {}", e.without_url()).into()))
}

#[cfg(test)]
mod tests;

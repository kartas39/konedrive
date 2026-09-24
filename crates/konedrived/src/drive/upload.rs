//! Microsoft Graph's upload sessions (the write design's §3.6 and §4.8): every
//! non-empty file goes through one, so that a new file carries
//! `conflictBehavior: fail`, a changed one `If-Match`, and both their time as
//! `fileSystemInfo`. Up to [`SMALL_UPLOAD_MAX`] the whole body is one request
//! ([`DriveClient::upload_small`]); above it the caller sends [`CHUNK_SIZE`]
//! fragments one at a time, persisting where the session is after each, and
//! resumes from [`DriveClient::upload_status`] after an interruption. An empty
//! file, which a session cannot carry, is a `PUT` followed by a `PATCH` for
//! its time.
//!
//! The upload URL is pre-authenticated: it never gets the account's token
//! (Microsoft: that can cause a `401`), and, being a credential for that one
//! file until it expires, it is never logged ([`UploadSession`]'s `Debug`
//! leaves it out, and so do the errors).

use std::fmt;
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
    /// Uploads a whole file of at most [`SMALL_UPLOAD_MAX`] bytes with its
    /// time (`modified`, Unix seconds): an empty one by `PUT`, any other as a
    /// one-request session.
    pub async fn upload_small(
        &self,
        target: UploadTarget<'_>,
        content: Vec<u8>,
        modified: i64,
    ) -> Result<DriveItem, WriteError> {
        let size = content.len() as u64;
        if size == 0 {
            return self.upload_empty(target, modified).await;
        }
        if size > SMALL_UPLOAD_MAX {
            return Err(WriteError::Failed(format!("{size} bytes do not go in one request: send them in fragments")));
        }
        let session = self.create_upload_session(target, size, modified).await?;
        match self.upload_chunk(&session.url, 0, size, content).await? {
            ChunkOutcome::Done(item) => Ok(*item),
            ChunkOutcome::More(progress) => Err(WriteError::Transient(format!(
                "the upload session still expects bytes from {} after the whole file",
                progress.next
            ))),
        }
    }

    /// Opens a session for `size` bytes. The session request carries the
    /// guard, the time and the size, so a full personal drive answers `507`
    /// before a byte is sent.
    pub async fn create_upload_session(
        &self,
        target: UploadTarget<'_>,
        size: u64,
        modified: i64,
    ) -> Result<UploadSession, WriteError> {
        let (url, item, if_match) = match target {
            UploadTarget::New { parent_id, name } => (
                self.child_url(parent_id, name, Some("createUploadSession"))?,
                json!({
                    "@microsoft.graph.conflictBehavior": "fail",
                    "name": name,
                    "fileSystemInfo": file_system_info(modified),
                    "fileSize": size,
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
                    "fileSize": size,
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
            .map_err(|e| WriteError::Transient(format!("an unreadable upload session from Graph: {e}")))?;
        Ok(UploadSession {
            url: session.upload_url,
            expires: session.expiration_date_time.as_deref().and_then(parse_graph_time),
        })
    }

    /// Sends the bytes of the file of `total` bytes that start at `offset`.
    /// Every fragment but the last must be a multiple of [`FRAGMENT_UNIT`];
    /// one that is not is refused before anything is sent. A fragment the
    /// session already has (`416`) is answered with where the session stands.
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
        let request = self
            .upload
            .put(session_url_of(session_url)?)
            .header(header::CONTENT_RANGE, format!("bytes {offset}-{}/{total}", end - 1))
            .body(chunk);
        let response = send_to_session(request).await?;
        match response.status() {
            StatusCode::ACCEPTED => {
                let body = progress_from(response).await?;
                Ok(ChunkOutcome::More(SessionProgress { next: body.next().unwrap_or(end), expires: body.expires() }))
            }
            StatusCode::OK | StatusCode::CREATED => item_from(response).await.map(|item| ChunkOutcome::Done(Box::new(item))),
            StatusCode::RANGE_NOT_SATISFIABLE => self.upload_status(session_url).await.map(ChunkOutcome::More),
            status if session_ended(status) => Err(WriteError::SessionGone),
            _ => Err(error_from(response).await),
        }
    }

    /// Where a session stands: what to resume from after an interruption.
    pub async fn upload_status(&self, session_url: &str) -> Result<SessionProgress, WriteError> {
        let request = self.upload.get(session_url_of(session_url)?).timeout(SESSION_CALL_TIMEOUT);
        let response = send_to_session(request).await?;
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
    async fn upload_empty(&self, target: UploadTarget<'_>, modified: i64) -> Result<DriveItem, WriteError> {
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
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::token::StaticToken;

    /// 2024-05-01T10:00:00Z, and a day later.
    const MAY_1: i64 = 1_714_557_600;
    const MAY_2_TEXT: &str = "2024-05-02T10:00:00Z";

    fn client(server: &MockServer) -> DriveClient {
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        DriveClient::new(base, Arc::new(StaticToken::new("T"))).unwrap()
    }

    #[tokio::test]
    async fn a_small_new_file_is_one_session_and_one_put_without_the_token() {
        let graph = MockServer::start().await;
        let up = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/me/drive/items/P!1:/a%20b%23.txt:/createUploadSession"))
            .and(header("authorization", "Bearer T"))
            .and(body_json(json!({"item": {
                "@microsoft.graph.conflictBehavior": "fail", "name": "a b#.txt",
                "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}, "fileSize": 5
            }})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uploadUrl": format!("{}/up/s1", up.uri()), "expirationDateTime": MAY_2_TEXT
            })))
            .mount(&graph).await;
        Mock::given(method("PUT")).and(path("/up/s1")).and(header("content-range", "bytes 0-4/5"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "N", "name": "a b#.txt", "size": 5, "eTag": "e1"})))
            .mount(&up).await;
        let target = UploadTarget::New { parent_id: "P!1", name: "a b#.txt" };
        let item = client(&graph).upload_small(target, b"hello".to_vec(), MAY_1).await.unwrap();
        assert_eq!(item.id, "N");
        let sent = up.received_requests().await.unwrap();
        assert_eq!(sent[0].body, b"hello");
        assert!(sent[0].headers.get("authorization").is_none(), "the upload URL never gets the token");
    }

    #[tokio::test]
    async fn a_changed_file_opens_its_session_by_id_guarded_by_the_etag() {
        let graph = MockServer::start().await;
        Mock::given(method("POST")).and(path("/me/drive/items/I/createUploadSession")).and(header("if-match", "e1"))
            .and(body_json(json!({"item": {
                "@microsoft.graph.conflictBehavior": "replace",
                "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}, "fileSize": 20_000_000
            }})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uploadUrl": format!("{}/up/s2", graph.uri()), "expirationDateTime": MAY_2_TEXT
            })))
            .mount(&graph).await;
        let target = UploadTarget::Existing { id: "I", if_match: "e1" };
        let session = client(&graph).create_upload_session(target, 20_000_000, MAY_1).await.unwrap();
        assert_eq!(session.expires, Some(MAY_1 + 86_400));
        assert!(session.url.ends_with("/up/s2"));
        assert!(!format!("{session:?}").contains("/up/"), "an upload URL is a credential: never in a log");
    }

    #[tokio::test]
    async fn an_empty_new_file_is_a_guarded_put_then_a_patch_for_its_time() {
        let graph = MockServer::start().await;
        Mock::given(method("PUT")).and(path("/me/drive/items/P:/empty.txt:/content"))
            .and(query_param("@microsoft.graph.conflictBehavior", "fail"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "E", "size": 0, "eTag": "e1"})))
            .mount(&graph).await;
        Mock::given(method("PATCH")).and(path("/me/drive/items/E")).and(header("if-match", "e1"))
            .and(body_json(json!({"fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "E", "size": 0, "eTag": "e2"})))
            .mount(&graph).await;
        let target = UploadTarget::New { parent_id: "P", name: "empty.txt" };
        let item = client(&graph).upload_small(target, Vec::new(), MAY_1).await.unwrap();
        assert_eq!(item.e_tag.as_deref(), Some("e2"), "the answer after the time was set");
        let put = &graph.received_requests().await.unwrap()[0];
        let lengths: Vec<_> = put.headers.get_all("content-length").iter().collect();
        assert_eq!(lengths, ["0"], "Graph needs the length of an empty body, once");
    }

    #[tokio::test]
    async fn a_big_file_goes_in_fragments_and_the_last_answers_the_item() {
        let up = MockServer::start().await;
        let total = FRAGMENT_UNIT + 10;
        Mock::given(method("PUT")).and(path("/up/s3"))
            .and(header("content-range", format!("bytes 0-{}/{total}", FRAGMENT_UNIT - 1).as_str()))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "expirationDateTime": MAY_2_TEXT, "nextExpectedRanges": [format!("{FRAGMENT_UNIT}-")]
            })))
            .mount(&up).await;
        Mock::given(method("PUT")).and(path("/up/s3"))
            .and(header("content-range", format!("bytes {FRAGMENT_UNIT}-{}/{total}", total - 1).as_str()))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "B", "size": total})))
            .mount(&up).await;
        let drive = client(&up);
        let url = format!("{}/up/s3", up.uri());
        let first = drive.upload_chunk(&url, 0, total, vec![1; FRAGMENT_UNIT as usize]).await.unwrap();
        assert_eq!(first, ChunkOutcome::More(SessionProgress { next: FRAGMENT_UNIT, expires: Some(MAY_1 + 86_400) }));
        let ChunkOutcome::Done(item) = drive.upload_chunk(&url, FRAGMENT_UNIT, total, vec![2; 10]).await.unwrap() else {
            panic!("the last fragment answers the item")
        };
        assert_eq!(item.size, Some(total));
        let misaligned = drive.upload_chunk(&url, 0, total, vec![0; 1000]).await.unwrap_err();
        assert!(matches!(misaligned, WriteError::Failed(_)), "{misaligned:?}");
        assert_eq!(up.received_requests().await.unwrap().len(), 2, "a misaligned fragment is never sent");
    }

    /// After an interruption the session's status says where to go on; a
    /// fragment it already has (`416`) is answered the same way.
    #[tokio::test]
    async fn an_interrupted_upload_resumes_from_the_next_expected_range() {
        let up = MockServer::start().await;
        Mock::given(method("GET")).and(path("/up/s4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "expirationDateTime": MAY_2_TEXT, "nextExpectedRanges": ["655360-"]
            })))
            .mount(&up).await;
        Mock::given(method("PUT")).and(path("/up/s4")).respond_with(ResponseTemplate::new(416)).mount(&up).await;
        let drive = client(&up);
        let url = format!("{}/up/s4", up.uri());
        let resumed = SessionProgress { next: 655_360, expires: Some(MAY_1 + 86_400) };
        assert_eq!(drive.upload_status(&url).await.unwrap(), resumed);
        let again = drive.upload_chunk(&url, 0, 1_000_000, vec![0; FRAGMENT_UNIT as usize]).await.unwrap();
        assert_eq!(again, ChunkOutcome::More(resumed));
    }

    #[tokio::test]
    async fn an_ended_session_is_session_gone_and_cancelling_it_is_done() {
        let up = MockServer::start().await;
        Mock::given(method("PUT")).and(path("/up/s5")).respond_with(ResponseTemplate::new(404)).mount(&up).await;
        Mock::given(method("DELETE")).and(path("/up/s5")).respond_with(ResponseTemplate::new(404)).mount(&up).await;
        let drive = client(&up);
        let url = format!("{}/up/s5", up.uri());
        let err = drive.upload_chunk(&url, 0, 10, vec![0; 10]).await.unwrap_err();
        assert!(matches!(err, WriteError::SessionGone), "{err:?}");
        drive.cancel_upload(&url).await.unwrap();
    }
}

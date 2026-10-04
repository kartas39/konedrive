//! Microsoft Graph's drive API. Reads live here: the delta feed, one item's
//! metadata, a file's bytes from an offset, and the `/content` redirect.
//! Writes live in [`write`] (folders, rename, move, delete) and [`upload`]
//! (upload sessions); nothing calls them until the write phase's outbox
//! worker does, and the scope stays `Files.Read` until an account is switched
//! to read-write.

mod account;
mod error;
pub mod item;
mod send;
pub mod socket;
mod upload;
mod write;

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use reqwest::header;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use url::Url;

pub use account::{Drive, Profile};
pub use error::{Detail, DriveError, Status};
pub use item::DriveItem;
pub use send::RetryPolicy;
pub use upload::{ChunkOutcome, SessionProgress, UploadSession, UploadTarget, CHUNK_SIZE, FRAGMENT_UNIT, SMALL_UPLOAD_MAX};
pub use write::{ItemChange, WriteError, MAX_RETRY_AFTER};

use error::{classify, graph_error, Kind};
use send::{Auth, Throttle};

use crate::pool::{Direction, TransferPool};
use crate::token::TokenSource;

/// Where a delta request starts: the beginning (a full listing), or a link
/// Graph handed out earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaFrom {
    Start,
    Link(String),
}

#[derive(Debug)]
pub struct DeltaPage {
    pub items: Vec<DriveItem>,
    pub next: DeltaNext,
}

/// What follows a page: another page, or the end of the feed with the link to
/// ask from next time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaNext {
    Page(String),
    Done(String),
}

/// A file's bytes, and the offset the first of them belongs at (part 1's
/// `served_from` contract).
pub struct Download {
    pub served_from: u64,
    pub stream: Box<dyn AsyncRead + Send + Unpin>,
}

/// What Graph answered for a thumbnail, once the answer is final for this
/// version of the item ([`DriveClient::thumbnail`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thumbnail {
    /// The thumbnail's bytes, not yet decoded.
    Image(Vec<u8>),
    /// Graph has none (`404`), or its body is over [`MAX_THUMBNAIL_BYTES`].
    None,
    /// Any other `4xx` but `401`, `408` and `429`: Graph will not make one
    /// at this size (`406` for some items at `c512x512`).
    Refused(Status),
}

/// The cap on `thumbnail`'s body: far more than any
/// real `c512x512` JPEG, small enough to bound memory against an oversized
/// or malicious answer.
const MAX_THUMBNAIL_BYTES: u64 = 8 * 1024 * 1024;

/// The bound on one upload request: a 10 MiB fragment in 10 minutes needs
/// about 140 kbit/s.
const UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct DriveClient {
    /// Metadata calls: a total timeout, because an answer is small.
    api: reqwest::Client,
    /// Content: no total timeout — a large file takes minutes — but a read
    /// timeout, so a stalled connection is noticed. Never follows redirects,
    /// so `/content`'s `302` can be read rather than followed with a token.
    content: reqwest::Client,
    /// Upload fragments, up to 10 MiB each, to a session's own URL: never
    /// the token, never a redirect. A bound on the whole request rather than
    /// a read timeout, since nothing comes back while the body goes out.
    upload: reqwest::Client,
    base: Url,
    tokens: Arc<dyn TokenSource>,
    retry: RetryPolicy,
    /// The account's transfer pool (`crate::pool`): told of every `429`/`503` and of the bytes
    /// that move. A pool of its own until the account's is set ([`with_pool`](Self::with_pool)).
    pool: Arc<TransferPool>,
}

#[derive(Deserialize)]
struct DeltaBody {
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

#[derive(Deserialize)]
struct DriveBody {
    id: String,
}

/// The drive's quota as Graph gives it (`GET /me/drive`): what the outbox decides a full
/// OneDrive by (issue #2). `remaining` is Graph's own figure, never `total - used`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct DriveQuota {
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub used: u64,
    /// `None` when Graph left it out.
    #[serde(default)]
    pub remaining: Option<u64>,
    /// `normal`, `nearing`, `critical` or `exceeded`; empty when Graph left it out.
    #[serde(default)]
    pub state: String,
}

#[derive(Deserialize)]
struct QuotaBody {
    #[serde(default)]
    quota: DriveQuota,
}

impl DriveClient {
    /// `base` is Graph's root, ending in `/` (`https://graph.microsoft.com/v1.0/`).
    pub fn new(base: Url, tokens: Arc<dyn TokenSource>) -> anyhow::Result<Self> {
        let agent = concat!("konedrive/", env!("CARGO_PKG_VERSION"));
        let api = reqwest::Client::builder()
            .user_agent(agent)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()?;
        let content = reqwest::Client::builder()
            .user_agent(agent)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let upload = reqwest::Client::builder()
            .user_agent(agent)
            .connect_timeout(Duration::from_secs(15))
            .timeout(UPLOAD_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let pool = TransferPool::new(crate::pool::DEFAULT_CEILING);
        Ok(Self { api, content, upload, base, tokens, retry: RetryPolicy::default(), pool })
    }

    /// This client reports into `pool`, the account's.
    pub fn with_pool(mut self, pool: Arc<TransferPool>) -> Self {
        self.pool = pool;
        self
    }

    /// The account's transfer pool.
    pub fn pool(&self) -> &Arc<TransferPool> {
        &self.pool
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The signed-in account's drive.
    pub async fn drive_id(&self) -> Result<String, DriveError> {
        let body: DriveBody = self.get_json(self.route("me/drive")?).await?;
        Ok(body.id)
    }

    /// The signed-in account's quota: one request.
    pub async fn quota(&self) -> Result<DriveQuota, DriveError> {
        let body: QuotaBody = self.get_json(self.route("me/drive")?).await?;
        Ok(body.quota)
    }

    pub async fn delta(&self, from: &DeltaFrom) -> Result<DeltaPage, DriveError> {
        let url = match from {
            DeltaFrom::Start => self.route("me/drive/root/delta")?,
            DeltaFrom::Link(link) => self.same_host(link)?,
        };
        let body: DeltaBody = self.get_json(url).await?;
        let next = match (body.next_link, body.delta_link) {
            (Some(next), _) => DeltaNext::Page(next),
            (None, Some(done)) => DeltaNext::Done(done),
            (None, None) => {
                return Err(DriveError::Failed(
                    "a delta page carried neither a next link nor a delta link".into(),
                ))
            }
        };
        Ok(DeltaPage { items: body.value, next })
    }

    pub async fn item(&self, id: &str) -> Result<DriveItem, DriveError> {
        self.get_json(self.item_url(id, None)?).await
    }

    /// The drive's root folder: what the account's folder itself shows.
    pub async fn root_item(&self) -> Result<DriveItem, DriveError> {
        self.get_json(self.route("me/drive/root")?).await
    }

    /// The item called `name` in the folder `parent_id`: what a create that
    /// found the name taken looks at.
    pub async fn child(&self, parent_id: &str, name: &str) -> Result<DriveItem, DriveError> {
        self.get_json(self.child_url(parent_id, name, None)?).await
    }

    /// Where `/items/{id}/content` redirects to — for an item whose metadata
    /// carried no download URL.
    pub async fn content_url(&self, id: &str) -> Result<String, DriveError> {
        let url = self.item_url(id, Some("content"))?;
        let response = self.send(Auth::Account, Throttle::Wait, || self.content.get(url.clone())).await?;
        let status = Status::of(&response);
        if status.is_redirection() {
            return response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| DriveError::Failed("a redirect without a Location".into()));
        }
        match classify(status, "") {
            Kind::NotFound => Err(DriveError::NotFound),
            _ => Err(refused(format!("content returned {status}"), status, "")),
        }
    }

    /// The bytes behind a pre-authenticated download URL, from `from` on — to
    /// the end of the file, or up to `end` (the first byte not wanted: a piece
    /// of a download in parts, issue #28). The URL carries its own
    /// authorisation; the account's token is never sent to it.
    pub async fn download(&self, url: &str, from: u64, end: Option<u64>) -> Result<Download, DriveError> {
        let url = Url::parse(url)
            .map_err(|e| DriveError::Failed(format!("a download URL from Graph cannot be parsed: {e}").into()))?;
        if end.is_some_and(|end| end <= from) {
            return Ok(Download { served_from: from, stream: Box::new(tokio::io::empty()) });
        }
        // `Range` names the last byte wanted, not the first one past it.
        let range = match end {
            Some(end) => Some(format!("bytes={from}-{}", end - 1)),
            None if from > 0 => Some(format!("bytes={from}-")),
            None => None,
        };
        let response = self
            .send(Auth::Link, Throttle::Wait, || {
                let request = self.content.get(url.clone());
                match &range {
                    Some(range) => request.header(header::RANGE, range.as_str()),
                    None => request,
                }
            })
            .await?;
        // No more than was asked for, whatever the server sends.
        let bounded = |stream: Box<dyn AsyncRead + Send + Unpin>, start: u64| -> Box<dyn AsyncRead + Send + Unpin> {
            match end {
                Some(end) => Box::new(stream.take(end.saturating_sub(start))),
                None => stream,
            }
        };
        let status = Status::of(&response);
        match status.code() {
            206 => {
                let start = content_range_start(response.headers())
                    .ok_or_else(|| DriveError::Failed("a partial answer without a readable Content-Range".into()))?;
                Ok(Download { served_from: start, stream: bounded(self.body(response), start) })
            }
            200 => {
                let mut stream = self.body(response);
                if from > 0 {
                    // The server ignored the range, as it may.
                    // Skip what is already on disk, so that the stream starts
                    // where this says it does.
                    let skipped = tokio::io::copy(&mut (&mut stream).take(from), &mut tokio::io::sink())
                        .await
                        .map_err(|e| DriveError::Transient(e.to_string().into()))?;
                    if skipped != from {
                        return Err(DriveError::Transient(
                            format!("the body ended after {skipped} of the {from} bytes to skip").into(),
                        ));
                    }
                }
                Ok(Download { served_from: from, stream: bounded(stream, from) })
            }
            _ => match classify(status, "") {
                // Asked from the end of the file or past it: nothing to serve.
                Kind::RangeNotSatisfiable => Ok(Download { served_from: from, stream: Box::new(tokio::io::empty()) }),
                Kind::Unauthorized | Kind::Forbidden | Kind::NotFound | Kind::Gone => Err(DriveError::UrlExpired),
                _ => Err(refused(format!("the download returned {status}"), status, "")),
            },
        }
    }

    /// Graph's own thumbnail of an item at `size` (`c512x512`: fits in 512
    /// by 512, aspect kept; `large`: Graph's named size, up to 800 px).
    /// Answers that settle the question for this version of the item come
    /// back as `Ok` — the bytes, [`Thumbnail::None`] (a 404, or a body too
    /// large to be worth decoding) or [`Thumbnail::Refused`] (any other 4xx
    /// but `401`, `408` and `429`); a passing trouble (no answer, `401`,
    /// `408`, `429`, 5xx) is an `Err`, worth asking again later. Graph may
    /// redirect to the bytes on another host (a CDN, not Graph): followed by
    /// hand, on the `content` client — which never redirects on its own,
    /// same as `content_url`/`download` — so the bearer token is never
    /// sent past Graph itself.
    pub async fn thumbnail(&self, id: &str, size: &str) -> Result<Thumbnail, DriveError> {
        let mut url = self.item_url(id, Some("thumbnails"))?;
        url.path_segments_mut().map_err(|()| DriveError::Failed("the Graph base URL cannot take a path".into()))?.push("0").push(size).push("content");
        let response = self.send(Auth::Account, Throttle::Wait, || self.content.get(url.clone())).await?;
        let response = if response.status().is_redirection() {
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| DriveError::Failed("a thumbnail redirect without a Location".into()))?
                .to_owned();
            let redirected = Url::parse(&location)
                .map_err(|e| DriveError::Failed(format!("a thumbnail redirect cannot be parsed: {e}").into()))?;
            self.send(Auth::Link, Throttle::Wait, || self.content.get(redirected.clone())).await?
        } else {
            response
        };
        let status = Status::of(&response);
        if status.is_success() {
            return self.bounded_thumbnail_body(response).await;
        }
        let passing = || DriveError::Transient(Detail::answered(format!("a thumbnail returned {status}"), status, ""));
        match classify(status, "") {
            Kind::NotFound => Ok(Thumbnail::None),
            Kind::Unauthorized | Kind::Timeout | Kind::Throttled => Err(passing()),
            _ if status.is_client_error() => Ok(Thumbnail::Refused(status)),
            _ => Err(refused(format!("a thumbnail returned {status}"), status, "")),
        }
    }

    /// [`thumbnail`](Self::thumbnail)'s body, capped at
    /// [`MAX_THUMBNAIL_BYTES`]: too large by
    /// `Content-Length`, or while streaming (a missing or dishonest
    /// `Content-Length`), comes back as `None` rather than an error — a
    /// body this size for a `c512x512` request is not a transient condition
    /// worth retrying, so the caller treats it exactly like a 404.
    async fn bounded_thumbnail_body(&self, response: reqwest::Response) -> Result<Thumbnail, DriveError> {
        if response.content_length().is_some_and(|len| len > MAX_THUMBNAIL_BYTES) {
            return Ok(Thumbnail::None);
        }
        let mut stream = response.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.try_next().await.map_err(|e| DriveError::Transient(e.without_url().to_string().into()))? {
            self.pool.moved(Direction::Down, chunk.len() as u64);
            buf.extend_from_slice(&chunk);
            if buf.len() as u64 > MAX_THUMBNAIL_BYTES {
                return Ok(Thumbnail::None);
            }
        }
        Ok(Thumbnail::Image(buf))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T, DriveError> {
        let response = self.send(Auth::Account, Throttle::Wait, || self.api.get(url.clone())).await?;
        let status = Status::of(&response);
        if status.is_success() {
            return response
                .json()
                .await
                .map_err(|e| DriveError::Transient(format!("an unreadable answer from Graph: {}", e.without_url()).into()));
        }
        match classify(status, "") {
            Kind::NotFound => Err(DriveError::NotFound),
            Kind::Gone => Err(resync(response).await),
            _ => {
                let code = graph_error(response).await.code;
                Err(refused(format!("Graph returned {status}"), status, &code))
            }
        }
    }

    /// A download's body, its bytes counted into the pool's speed as they are read.
    fn body(&self, response: reqwest::Response) -> Box<dyn AsyncRead + Send + Unpin> {
        let pool = Arc::clone(&self.pool);
        let stream = response
            .bytes_stream()
            .inspect_ok(move |chunk| pool.moved(Direction::Down, chunk.len() as u64))
            .map_err(|e| std::io::Error::other(e.without_url()));
        Box::new(tokio_util::io::StreamReader::new(Box::pin(stream)))
    }

    fn route(&self, route: &str) -> Result<Url, DriveError> {
        self.base.join(route).map_err(|e| DriveError::Failed(format!("{route}: {e}").into()))
    }

    /// `me/drive/items/<id>[/<tail>]`, the id as one path segment whatever it
    /// holds (personal ids contain `!`).
    fn item_url(&self, id: &str, tail: Option<&str>) -> Result<Url, DriveError> {
        let mut url = self.route("me/drive/items/")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| DriveError::Failed("the Graph base URL cannot take a path".into()))?;
            segments.pop_if_empty().push(id);
            if let Some(tail) = tail {
                segments.push(tail);
            }
        }
        Ok(url)
    }

    /// `me/drive/items/<parent>:/<name>`, an item by its name in a folder, or
    /// with a tail `me/drive/items/<parent>:/<name>:/<tail>`; the id and the
    /// name each one path segment whatever they hold.
    fn child_url(&self, parent_id: &str, name: &str, tail: Option<&str>) -> Result<Url, DriveError> {
        let mut url = self.route("me/drive/items/")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| DriveError::Failed("the Graph base URL cannot take a path".into()))?;
            segments.pop_if_empty().push(&format!("{parent_id}:"));
            match tail {
                Some(tail) => segments.push(&format!("{name}:")).push(tail),
                None => segments.push(name),
            };
        }
        Ok(url)
    }

    /// A link Graph handed out, followed only to Graph itself: the token goes
    /// with it.
    fn same_host(&self, link: &str) -> Result<Url, DriveError> {
        let url = Url::parse(link).map_err(|e| DriveError::Failed(format!("a link from Graph cannot be parsed: {e}").into()))?;
        if url.scheme() != self.base.scheme() || url.host_str() != self.base.host_str() || url.port_or_known_default() != self.base.port_or_known_default() {
            return Err(DriveError::Failed(format!("refusing to follow a link to another host: {}", url.host_str().unwrap_or("none")).into()));
        }
        Ok(url)
    }
}

/// An answer no call has a meaning for: passing if the service failed (`5xx`), final
/// otherwise.
fn refused(message: String, status: Status, code: &str) -> DriveError {
    let detail = Detail::answered(message, status, code);
    if status.is_server_error() {
        DriveError::Transient(detail)
    } else {
        DriveError::Failed(detail)
    }
}

/// The first byte of `Content-Range: bytes <first>-<last>/<size>`.
fn content_range_start(headers: &header::HeaderMap) -> Option<u64> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?;
    range.split('-').next()?.trim().parse().ok()
}

/// Which `410` Graph answered (`docs/design/writes.md` §9): the two resync codes
/// Microsoft names are told apart by the body; anything else is the plain
/// resync.
async fn resync(response: reqwest::Response) -> DriveError {
    let body = response.text().await.unwrap_or_default();
    if body.contains("resyncChangesUploadDifferences") {
        DriveError::ResyncUpload
    } else {
        DriveError::ResyncRequired
    }
}

#[cfg(test)]
mod tests;

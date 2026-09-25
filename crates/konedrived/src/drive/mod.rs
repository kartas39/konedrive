//! Microsoft Graph's drive API. Reads live here: the delta feed, one item's
//! metadata, a file's bytes from an offset, and the `/content` redirect.
//! Writes live in [`write`] (folders, rename, move, delete) and [`upload`]
//! (upload sessions); nothing calls them until the write phase's outbox
//! worker does, and the scope stays `Files.Read` until an account is switched
//! to read-write.

mod children;
pub mod item;
pub mod upload;
pub mod write;

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use reqwest::{header, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use url::Url;

pub use item::DriveItem;
pub use upload::{ChunkOutcome, SessionProgress, UploadSession, UploadTarget, CHUNK_SIZE, FRAGMENT_UNIT, SMALL_UPLOAD_MAX};
pub use write::{ItemChange, WriteError};

use crate::token::{AuthError, TokenSource};

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
    Transient(String),
    #[error("{0}")]
    Failed(String),
}

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

/// The cap on `thumbnail`'s body: far more than any
/// real `c512x512` JPEG, small enough to bound memory against an oversized
/// or malicious answer.
const MAX_THUMBNAIL_BYTES: u64 = 8 * 1024 * 1024;

/// The bound on one upload request: a 10 MiB fragment in 10 minutes needs
/// about 140 kbit/s.
const UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to wait out throttling (`429`, `503`): `Retry-After` when given,
/// capped, and how many answers of that kind to take before giving up.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub default_wait: Duration,
    pub max_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { attempts: 5, default_wait: Duration::from_secs(10), max_wait: Duration::from_secs(300) }
    }
}

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
        Ok(Self { api, content, upload, base, tokens, retry: RetryPolicy::default() })
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

    /// The item called `name` in the folder `parent_id`: what a create that
    /// found the name taken looks at.
    pub async fn child(&self, parent_id: &str, name: &str) -> Result<DriveItem, DriveError> {
        self.get_json(self.child_url(parent_id, name, None)?).await
    }

    /// Where `/items/{id}/content` redirects to — for an item whose metadata
    /// carried no download URL.
    pub async fn content_url(&self, id: &str) -> Result<String, DriveError> {
        let url = self.item_url(id, Some("content"))?;
        let response = self.send(|token| self.content.get(url.clone()).bearer_auth(token)).await?;
        match response.status() {
            status if status.is_redirection() => response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| DriveError::Failed("a redirect without a Location".into())),
            StatusCode::NOT_FOUND => Err(DriveError::NotFound),
            status if status.is_server_error() => Err(DriveError::Transient(format!("content returned {status}"))),
            status => Err(DriveError::Failed(format!("content returned {status}"))),
        }
    }

    /// The bytes behind a pre-authenticated download URL, from `from` on. The
    /// URL carries its own authorisation; the account's token is never sent
    /// to it.
    pub async fn download(&self, url: &str, from: u64) -> Result<Download, DriveError> {
        let url = Url::parse(url)
            .map_err(|e| DriveError::Failed(format!("a download URL from Graph cannot be parsed: {e}")))?;
        let response = self
            .send_anonymous(|| {
                let request = self.content.get(url.clone());
                if from > 0 {
                    request.header(header::RANGE, format!("bytes={from}-"))
                } else {
                    request
                }
            })
            .await?;
        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let start = content_range_start(response.headers())
                    .ok_or_else(|| DriveError::Failed("a partial answer without a readable Content-Range".into()))?;
                Ok(Download { served_from: start, stream: body(response) })
            }
            StatusCode::OK => {
                let mut stream = body(response);
                if from > 0 {
                    // The server ignored the range, as it may.
                    // Skip what is already on disk, so that the stream starts
                    // where this says it does.
                    let skipped = tokio::io::copy(&mut (&mut stream).take(from), &mut tokio::io::sink())
                        .await
                        .map_err(|e| DriveError::Transient(e.to_string()))?;
                    if skipped != from {
                        return Err(DriveError::Transient(format!(
                            "the body ended after {skipped} of the {from} bytes to skip"
                        )));
                    }
                }
                Ok(Download { served_from: from, stream })
            }
            // Asked from the end of the file or past it: nothing to serve.
            StatusCode::RANGE_NOT_SATISFIABLE => {
                Ok(Download { served_from: from, stream: Box::new(tokio::io::empty()) })
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => {
                Err(DriveError::UrlExpired)
            }
            status if status.is_server_error() => Err(DriveError::Transient(format!("the download returned {status}"))),
            status => Err(DriveError::Failed(format!("the download returned {status}"))),
        }
    }

    /// Graph's own thumbnail of an item at `size` (`c512x512`: fits in 512
    /// by 512, aspect kept) — `None` when Graph has none for it, or when
    /// its answer is too large to be worth decoding (recorded by the
    /// caller the same as a 404, never retried). Graph may
    /// redirect to the bytes on another host (a CDN, not Graph): followed by
    /// hand, on the `content` client — which never redirects on its own,
    /// same as `content_url`/`download` — so the bearer token is never
    /// sent past Graph itself.
    pub async fn thumbnail(&self, id: &str, size: &str) -> Result<Option<Vec<u8>>, DriveError> {
        let mut url = self.item_url(id, Some("thumbnails"))?;
        url.path_segments_mut().map_err(|()| DriveError::Failed("the Graph base URL cannot take a path".into()))?.push("0").push(size).push("content");
        let response = self.send(|token| self.content.get(url.clone()).bearer_auth(token)).await?;
        let response = if response.status().is_redirection() {
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| DriveError::Failed("a thumbnail redirect without a Location".into()))?
                .to_owned();
            let redirected = Url::parse(&location)
                .map_err(|e| DriveError::Failed(format!("a thumbnail redirect cannot be parsed: {e}")))?;
            self.send_anonymous(|| self.content.get(redirected.clone())).await?
        } else {
            response
        };
        match response.status() {
            status if status.is_success() => Self::bounded_thumbnail_body(response).await,
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_server_error() => Err(DriveError::Transient(format!("a thumbnail returned {status}"))),
            status => Err(DriveError::Failed(format!("a thumbnail returned {status}"))),
        }
    }

    /// [`thumbnail`](Self::thumbnail)'s body, capped at
    /// [`MAX_THUMBNAIL_BYTES`]: too large by
    /// `Content-Length`, or while streaming (a missing or dishonest
    /// `Content-Length`), comes back as `None` rather than an error — a
    /// body this size for a `c512x512` request is not a transient condition
    /// worth retrying, so the caller treats it exactly like a 404.
    async fn bounded_thumbnail_body(response: reqwest::Response) -> Result<Option<Vec<u8>>, DriveError> {
        if response.content_length().is_some_and(|len| len > MAX_THUMBNAIL_BYTES) {
            return Ok(None);
        }
        let mut stream = response.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.try_next().await.map_err(|e| DriveError::Transient(e.to_string()))? {
            buf.extend_from_slice(&chunk);
            if buf.len() as u64 > MAX_THUMBNAIL_BYTES {
                return Ok(None);
            }
        }
        Ok(Some(buf))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T, DriveError> {
        let response = self.send(|token| self.api.get(url.clone()).bearer_auth(token)).await?;
        match response.status() {
            status if status.is_success() => response
                .json()
                .await
                .map_err(|e| DriveError::Transient(format!("an unreadable answer from Graph: {e}"))),
            StatusCode::NOT_FOUND => Err(DriveError::NotFound),
            StatusCode::GONE => Err(resync(response).await),
            status if status.is_server_error() => Err(DriveError::Transient(format!("Graph returned {status}"))),
            status => Err(DriveError::Failed(format!("Graph returned {status}"))),
        }
    }

    /// Sends a request with the account's token. A `401` is answered once by
    /// dropping the cached token and asking again; `429` and `503` wait as
    /// told (Ruling of).
    async fn send(&self, request: impl Fn(&str) -> reqwest::RequestBuilder) -> Result<reqwest::Response, DriveError> {
        let mut renewed = false;
        let mut throttled = 0;
        loop {
            let token = self.token().await?;
            let response = request(&token)
                .send()
                .await
                .map_err(|e| DriveError::Transient(format!("cannot reach Microsoft Graph: {e}")))?;
            match response.status() {
                StatusCode::UNAUTHORIZED if !renewed => {
                    renewed = true;
                    self.tokens.invalidate().await;
                }
                StatusCode::UNAUTHORIZED => {
                    return Err(DriveError::Failed("Microsoft Graph rejected the access token".into()))
                }
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    throttled += 1;
                    if throttled >= self.retry.attempts {
                        return Err(DriveError::Transient(format!(
                            "Microsoft Graph kept answering {}",
                            response.status()
                        )));
                    }
                    tokio::time::sleep(self.wait_for(&response)).await;
                }
                _ => return Ok(response),
            }
        }
    }

    /// [`send`](Self::send) for a pre-authenticated URL: no token at all.
    async fn send_anonymous(&self, request: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response, DriveError> {
        let mut throttled = 0;
        loop {
            let response = request()
                .send()
                .await
                .map_err(|e| DriveError::Transient(format!("cannot reach OneDrive: {e}")))?;
            match response.status() {
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    throttled += 1;
                    if throttled >= self.retry.attempts {
                        return Err(DriveError::Transient(format!("OneDrive kept answering {}", response.status())));
                    }
                    tokio::time::sleep(self.wait_for(&response)).await;
                }
                _ => return Ok(response),
            }
        }
    }

    async fn token(&self) -> Result<String, DriveError> {
        self.tokens.access_token().await.map_err(|e| match e {
            AuthError::SignedOut => DriveError::SignedOut,
            AuthError::Locked => DriveError::Transient("the secret storage is locked".into()),
            AuthError::Transient(message) => DriveError::Transient(message),
        })
    }

    /// `Retry-After` in seconds or as an HTTP date, else the default; capped.
    fn wait_for(&self, response: &reqwest::Response) -> Duration {
        write::retry_after(response.headers(), std::time::SystemTime::now())
            .unwrap_or(self.retry.default_wait)
            .min(self.retry.max_wait)
    }

    fn route(&self, route: &str) -> Result<Url, DriveError> {
        self.base.join(route).map_err(|e| DriveError::Failed(format!("{route}: {e}")))
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
        let url = Url::parse(link).map_err(|e| DriveError::Failed(format!("a link from Graph cannot be parsed: {e}")))?;
        if url.scheme() != self.base.scheme() || url.host_str() != self.base.host_str() || url.port_or_known_default() != self.base.port_or_known_default() {
            return Err(DriveError::Failed(format!("refusing to follow a link to another host: {link}")));
        }
        Ok(url)
    }
}

fn body(response: reqwest::Response) -> Box<dyn AsyncRead + Send + Unpin> {
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    Box::new(tokio_util::io::StreamReader::new(Box::pin(stream)))
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
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::AsyncReadExt;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::token::StaticToken;

    fn client(server: &MockServer) -> DriveClient {
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        DriveClient::new(base, Arc::new(StaticToken::new("T")))
            .unwrap()
            .with_retry(RetryPolicy { attempts: 3, default_wait: Duration::from_millis(10), max_wait: Duration::from_millis(50) })
    }

    async fn read_all(download: Download) -> Vec<u8> {
        let mut out = Vec::new();
        let mut stream = download.stream;
        stream.read_to_end(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn delta_follows_next_links_to_the_delta_link() {
        let server = MockServer::start().await;
        let next = format!("{}/me/drive/root/delta?token=p2", server.uri());
        let done = format!("{}/me/drive/root/delta?token=d1", server.uri());
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "B", "name": "b.txt", "file": {}, "parentReference": {"id": "R"}}],
                "@odata.deltaLink": done
            })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(header("authorization", "Bearer T"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "R", "root": {}, "folder": {}}, {"id": "A", "name": "a", "folder": {}, "parentReference": {"id": "R"}}],
                "@odata.nextLink": next
            })))
            .mount(&server).await;
        let drive = client(&server);
        let first = drive.delta(&DeltaFrom::Start).await.unwrap();
        assert_eq!(first.items.len(), 2);
        let DeltaNext::Page(link) = first.next else { panic!("expected a next link") };
        let second = drive.delta(&DeltaFrom::Link(link)).await.unwrap();
        assert_eq!(second.items[0].id, "B");
        assert_eq!(second.next, DeltaNext::Done(done));
    }

    #[tokio::test]
    async fn a_link_to_another_host_is_refused_without_sending_the_token() {
        let server = MockServer::start().await;
        let drive = client(&server);
        let err = drive.delta(&DeltaFrom::Link("https://example.com/steal".into())).await.unwrap_err();
        assert!(matches!(err, DriveError::Failed(_)), "{err:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gone_means_the_feed_must_be_listed_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta"))
            .respond_with(ResponseTemplate::new(410).set_body_json(json!({"error": {"code": "resyncRequired"}})))
            .mount(&server).await;
        assert!(matches!(client(&server).delta(&DeltaFrom::Start).await, Err(DriveError::ResyncRequired)));
    }

    #[tokio::test]
    async fn throttling_waits_as_told_and_tries_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .up_to_n_times(1).with_priority(1)
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
            .with_priority(2)
            .mount(&server).await;
        assert_eq!(client(&server).drive_id().await.unwrap(), "D1");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn throttling_that_never_ends_is_a_transient_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server).await;
        assert!(matches!(client(&server).drive_id().await, Err(DriveError::Transient(_))));
        assert_eq!(server.received_requests().await.unwrap().len(), 3, "RetryPolicy.attempts");
    }

    /// Hands out "T1" until invalidated, then "T2".
    struct Rotating(AtomicBool);

    #[async_trait::async_trait]
    impl TokenSource for Rotating {
        async fn access_token(&self) -> Result<String, AuthError> {
            Ok(if self.0.load(Ordering::SeqCst) { "T2".into() } else { "T1".into() })
        }
        async fn invalidate(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_refused_token_is_dropped_and_the_call_made_once_more() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive")).and(header("authorization", "Bearer T1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/me/drive")).and(header("authorization", "Bearer T2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
            .mount(&server).await;
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        let drive = DriveClient::new(base, Arc::new(Rotating(AtomicBool::new(false)))).unwrap();
        assert_eq!(drive.drive_id().await.unwrap(), "D1");
    }

    #[tokio::test]
    async fn signed_out_is_reported_as_such() {
        struct Out;
        #[async_trait::async_trait]
        impl TokenSource for Out {
            async fn access_token(&self) -> Result<String, AuthError> {
                Err(AuthError::SignedOut)
            }
            async fn invalidate(&self) {}
        }
        let server = MockServer::start().await;
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        let drive = DriveClient::new(base, Arc::new(Out)).unwrap();
        assert!(matches!(drive.drive_id().await, Err(DriveError::SignedOut)));
    }

    #[tokio::test]
    async fn an_item_id_is_one_path_segment_whatever_it_contains() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive/items/ABC!123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "ABC!123", "name": "x.bin", "size": 5, "cTag": "c1", "file": {"hashes": {"quickXorHash": "AAAAAAAAAAAAAAAAAAAAAAAAAAA="}},
                "@microsoft.graph.downloadUrl": "https://dl.example/x"
            })))
            .mount(&server).await;
        let item = client(&server).item("ABC!123").await.unwrap();
        assert_eq!(item.download_url.as_deref(), Some("https://dl.example/x"));
        assert_eq!(item.c_tag.as_deref(), Some("c1"));
    }

    #[tokio::test]
    async fn a_child_is_found_by_its_name_in_its_folder() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive/items/P!1:/100%25%20%D1%84.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "C", "name": "100% ф.txt"})))
            .mount(&server).await;
        assert_eq!(client(&server).child("P!1", "100% ф.txt").await.unwrap().id, "C");
    }

    #[tokio::test]
    async fn a_missing_item_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive/items/X"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server).await;
        assert!(matches!(client(&server).item("X").await, Err(DriveError::NotFound)));
    }

    #[tokio::test]
    async fn a_partial_answer_starts_where_its_content_range_says() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/dl")).and(header("range", "bytes=4-"))
            .respond_with(ResponseTemplate::new(206).insert_header("content-range", "bytes 4-9/10").set_body_bytes(b"456789".to_vec()))
            .mount(&server).await;
        let download = client(&server).download(&format!("{}/dl", server.uri()), 4).await.unwrap();
        assert_eq!(download.served_from, 4);
        assert_eq!(read_all(download).await, b"456789");
        let sent = &server.received_requests().await.unwrap()[0];
        assert!(sent.headers.get("authorization").is_none(), "a pre-authenticated URL gets no token");
    }

    #[tokio::test]
    async fn a_whole_body_answering_a_range_is_skipped_to_the_offset() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/dl"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"0123456789".to_vec()))
            .mount(&server).await;
        let download = client(&server).download(&format!("{}/dl", server.uri()), 4).await.unwrap();
        assert_eq!(download.served_from, 4);
        assert_eq!(read_all(download).await, b"456789");
    }

    #[tokio::test]
    async fn a_range_past_the_end_is_an_empty_stream_at_that_offset() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/dl"))
            .respond_with(ResponseTemplate::new(416))
            .mount(&server).await;
        let download = client(&server).download(&format!("{}/dl", server.uri()), 10).await.unwrap();
        assert_eq!(download.served_from, 10);
        assert!(read_all(download).await.is_empty());
    }

    #[tokio::test]
    async fn a_refused_download_url_has_expired() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/dl"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server).await;
        assert!(matches!(client(&server).download(&format!("{}/dl", server.uri()), 0).await, Err(DriveError::UrlExpired)));
    }

    #[tokio::test]
    async fn content_answers_with_where_the_bytes_are() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive/items/X/content"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "https://dl.example/x"))
            .mount(&server).await;
        assert_eq!(client(&server).content_url("X").await.unwrap(), "https://dl.example/x");
    }

    /// Graph's thumbnail redirects to a CDN host; the bearer
    /// token must never reach it, the same guarantee `download` gives a
    /// pre-authenticated URL.
    #[tokio::test]
    async fn a_thumbnail_redirect_is_followed_without_the_bearer_token() {
        let server = MockServer::start().await;
        let cdn = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/blob", cdn.uri())))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/blob"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"jpeg-bytes".to_vec()))
            .mount(&cdn).await;
        let bytes = client(&server).thumbnail("P", "c512x512").await.unwrap();
        assert_eq!(bytes, Some(b"jpeg-bytes".to_vec()));
        let received = cdn.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert!(received[0].headers.get("authorization").is_none(), "the token must not reach the CDN");
    }

    /// A body over the cap is treated like a 404 — recorded,
    /// never retried — rather than downloaded in full or returned as an
    /// error.
    #[tokio::test]
    async fn a_thumbnail_over_the_size_cap_comes_back_as_none() {
        let server = MockServer::start().await;
        let oversized = vec![0u8; MAX_THUMBNAIL_BYTES as usize + 1];
        Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
            .mount(&server).await;
        assert!(client(&server).thumbnail("P", "c512x512").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_drive_id_comes_from_me_drive() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&calls);
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(move |_: &Request| {
                seen.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({"id": "D9", "quota": {"used": 1, "total": 2}}))
            })
            .mount(&server).await;
        assert_eq!(client(&server).drive_id().await.unwrap(), "D9");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

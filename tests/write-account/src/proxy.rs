//! The one way out to OneDrive: a forwarder on `127.0.0.1` that konedrive's own Graph client
//! (`DriveClient`) and the harness's few requests of its own are pointed at. It hands each
//! request to the [`Guard`] before anything of it is sent, and forwards only what the guard
//! admits. So the guard sees every request at the wire, whatever code made it.
//!
//! Answers come back unchanged but for the links in them, which would otherwise lead past the
//! guard: an upload session's URL is kept here and replaced by one of the proxy's own,
//! `@odata.nextLink` and `@odata.deltaLink` are pointed at the proxy, and download URLs are
//! removed (the harness downloads nothing).

use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Response, StatusCode};
use serde_json::Value;
use tokio::net::TcpListener;
use url::Url;

use crate::guard::{declared_size, Forward, Guard, Request, Target};

/// The status a refused request is answered with: never one OneDrive sends, so a refusal cannot
/// be taken for an answer of OneDrive's.
pub const REFUSED: u16 = 599;

pub struct Proxy {
    /// Graph's base, as the harness's clients see it: `http://127.0.0.1:<port>/graph/`.
    pub base: Url,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Shared {
    /// Graph's own base, ending in `/`.
    upstream: Url,
    /// `http://127.0.0.1:<port>`.
    origin: String,
    guard: Arc<Guard>,
    http: reqwest::Client,
}

/// Starts the proxy in front of `upstream` (Graph's base, ending in `/`).
pub async fn start(upstream: Url, guard: Arc<Guard>) -> io::Result<Proxy> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let base = Url::parse(&format!("{origin}/graph/")).map_err(io::Error::other)?;
    let shared = Arc::new(Shared { upstream, origin, guard, http });
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let shared = Arc::clone(&shared);
                    async move { Ok::<_, Infallible>(handle(&shared, request).await) }
                });
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    Ok(Proxy { base, task })
}

async fn handle(shared: &Shared, request: hyper::Request<Incoming>) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    let body = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return plain(StatusCode::BAD_REQUEST, format!("the proxy could not read the request: {e}")),
    };
    let path = parts.uri.path();
    let query = parts.uri.query();
    let content_range = parts.headers.get(header::CONTENT_RANGE).and_then(|value| value.to_str().ok());
    let target = if let Some(rel) = path.strip_prefix("/graph/") {
        Target::Graph { rel, query }
    } else if let Some(key) = path.strip_prefix("/upload/") {
        Target::Upload { key }
    } else {
        return refuse(&shared.guard, format!("{} {path} is not a path this proxy serves", parts.method));
    };
    let request = Request { method: &parts.method, target, body: &body, content_range };
    let forward = match shared.guard.admit(&request) {
        Ok(forward) => forward,
        Err(why) => return refuse(&shared.guard, why),
    };
    let (url, to_upload) = match (&forward, target) {
        (Forward::Graph, Target::Graph { rel, query }) => {
            let mut url = format!("{}{rel}", shared.upstream);
            if let Some(query) = query {
                url.push('?');
                url.push_str(query);
            }
            (url, false)
        }
        (Forward::Upload(url), _) => (url.clone(), true),
        (Forward::Graph, Target::Upload { .. }) => unreachable!("an upload key is never forwarded to Graph"),
    };
    let Ok(url) = Url::parse(&url) else {
        return plain(StatusCode::BAD_GATEWAY, "the proxy could not form the address to forward to".into());
    };
    let mut headers = parts.headers.clone();
    strip_hop_by_hop(&mut headers);
    headers.remove(header::HOST);
    if to_upload {
        // An upload URL carries its own authorisation: the account's token never goes there.
        headers.remove(header::AUTHORIZATION);
    }
    let sent = shared.http.request(parts.method.clone(), url).headers(headers).body(body.clone()).send().await;
    let answer = match sent {
        Ok(answer) => answer,
        Err(e) => return plain(StatusCode::BAD_GATEWAY, format!("the proxy could not reach OneDrive: {}", e.without_url())),
    };
    let status = answer.status();
    let mut headers = answer.headers().clone();
    strip_hop_by_hop(&mut headers);
    headers.remove(header::CONTENT_LENGTH);
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    let answer = match answer.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => return plain(StatusCode::BAD_GATEWAY, format!("the proxy could not read OneDrive's answer: {}", e.without_url())),
    };
    let answer = match serde_json::from_slice::<Value>(&answer) {
        Ok(mut value) if json => {
            let rel = match target {
                Target::Graph { rel, .. } => Some(rel),
                Target::Upload { .. } => None,
            };
            shared.guard.learn(&parts.method, rel, status.as_u16(), &value);
            rewrite(shared, &mut value, &body);
            Bytes::from(serde_json::to_vec(&value).unwrap_or_default())
        }
        _ => answer,
    };
    let mut response = Response::new(Full::new(answer));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Points the links in an answer at the proxy, and keeps an upload session's URL here.
fn rewrite(shared: &Shared, value: &mut Value, request_body: &[u8]) {
    let Some(object) = value.as_object_mut() else { return };
    if let Some(Value::String(url)) = object.get("uploadUrl") {
        let size = declared_size(request_body).unwrap_or(0);
        let key = shared.guard.open_session(url.clone(), size);
        object.insert("uploadUrl".into(), Value::String(format!("{}/upload/{key}", shared.origin)));
    }
    for link in ["@odata.nextLink", "@odata.deltaLink"] {
        if let Some(Value::String(url)) = object.get(link) {
            let pointed = match url.strip_prefix(shared.upstream.as_str()) {
                Some(rest) => format!("{}/graph/{rest}", shared.origin),
                // Somewhere else: DriveClient refuses to follow it, and so the link is left.
                None => url.clone(),
            };
            object.insert(link.into(), Value::String(pointed));
        }
    }
    object.remove("@microsoft.graph.downloadUrl");
    if let Some(Value::Array(items)) = object.get_mut("value") {
        for item in items.iter_mut().filter_map(Value::as_object_mut) {
            item.remove("@microsoft.graph.downloadUrl");
        }
    }
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in [
        header::CONNECTION,
        header::TRANSFER_ENCODING,
        header::TE,
        header::TRAILER,
        header::UPGRADE,
        header::PROXY_AUTHORIZATION,
        header::PROXY_AUTHENTICATE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
}

fn refuse(guard: &Guard, why: String) -> Response<Full<Bytes>> {
    guard.record_refusal(&why);
    eprintln!("  GUARD  refused: {why}");
    let body = serde_json::json!({ "error": { "code": "konedriveGuardRefused", "message": why } });
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = StatusCode::from_u16(REFUSED).expect("a valid status");
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn plain(status: StatusCode, message: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(message)));
    *response.status_mut() = status;
    response
}

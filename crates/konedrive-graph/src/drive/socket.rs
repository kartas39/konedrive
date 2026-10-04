//! Graph's change notifications over Socket.IO (issue #54): the endpoint a drive hands
//! out (`GET /me/drive/root/subscriptions/socketIo`), and a small client of Engine.IO v4
//! and Socket.IO over a websocket that says when something in the drive changed.
//!
//! Only the websocket transport: Microsoft does not support Engine.IO's long polling.
//! An event carries nothing that is used: it only says "something changed", and the
//! caller runs a delta. The notification URL's query carries a token: it is never
//! logged, and neither is the URL built from it; only the host is.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use super::item::parse_graph_time;
use super::{DriveClient, DriveError};

/// How long an endpoint is taken to live when Graph leaves `expirationDateTime` out. A
/// guess: the documentation names no lifetime (limitations log).
pub const DEFAULT_LIFETIME: Duration = Duration::from_secs(3600);

/// How long before its expiry an endpoint is replaced by a new one.
pub const RENEW_EARLY: Duration = Duration::from_secs(120);

/// The bound on opening a connection: TCP, TLS, the websocket upgrade and Engine.IO's
/// open packet.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest message taken from the server. An event is a few hundred bytes.
const MAX_MESSAGE: usize = 1024 * 1024;

/// How much of an unexpected packet goes into a reason or the log.
const SHOWN: usize = 120;

/// Where a drive's notifications are served, and until when.
#[derive(Clone)]
pub struct SocketEndpoint {
    /// Graph's `notificationUrl`: `https://host/<namespace>?<query>`. Its query carries a
    /// token; never log it ([`host`](Self::host) is what may be logged).
    pub notification_url: Url,
    /// When the endpoint stops working: Graph's `expirationDateTime`, or
    /// [`DEFAULT_LIFETIME`] from when it was fetched.
    pub expires_at: SystemTime,
    /// Whether [`expires_at`](Self::expires_at) came from Graph (rather than the guess).
    pub expiry_from_service: bool,
}

impl SocketEndpoint {
    /// The notification URL's host, the only part of it that may be logged.
    pub fn host(&self) -> &str {
        self.notification_url.host_str().unwrap_or("none")
    }

    /// How long from `now` until a new endpoint should be fetched: [`RENEW_EARLY`] before
    /// it expires, and zero when that is already past.
    pub fn renew_after(&self, now: SystemTime) -> Duration {
        self.expires_at.checked_sub(RENEW_EARLY).and_then(|at| at.duration_since(now).ok()).unwrap_or(Duration::ZERO)
    }
}

impl fmt::Debug for SocketEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SocketEndpoint")
            .field("host", &self.host())
            .field("expires_at", &self.expires_at)
            .field("expiry_from_service", &self.expiry_from_service)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndpointBody {
    notification_url: String,
    #[serde(default)]
    expiration_date_time: Option<String>,
}

impl DriveClient {
    /// The drive's Socket.IO endpoint (`GET /me/drive/root/subscriptions/socketIo`). The
    /// documentation shows only `id` and `notificationUrl`; the service also sends
    /// `expirationDateTime`, and without it (or when it cannot be read) the endpoint is
    /// taken to live [`DEFAULT_LIFETIME`].
    pub async fn socket_endpoint(&self) -> Result<SocketEndpoint, DriveError> {
        let body: EndpointBody = self.get_json(self.route("me/drive/root/subscriptions/socketIo")?).await?;
        let notification_url = Url::parse(&body.notification_url)
            .map_err(|e| DriveError::Failed(format!("the notification URL from Graph cannot be parsed: {e}").into()))?;
        let given = body
            .expiration_date_time
            .as_deref()
            .and_then(parse_graph_time)
            .and_then(|seconds| u64::try_from(seconds).ok())
            .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds));
        Ok(SocketEndpoint {
            notification_url,
            expires_at: given.unwrap_or_else(|| SystemTime::now() + DEFAULT_LIFETIME),
            expiry_from_service: given.is_some(),
        })
    }
}

/// Why a connection ended, or never opened. Its text never holds the notification URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SocketEnd {
    /// The notification URL cannot be turned into a websocket URL.
    #[error("the notification URL cannot be used: {0}")]
    BadUrl(String),
    /// TCP, TLS or the websocket upgrade failed, or took too long.
    #[error("cannot open the notification socket: {0}")]
    Connect(String),
    /// The server closed the websocket, the Engine.IO session or the namespace.
    #[error("the notification socket was closed: {0}")]
    Closed(String),
    /// The server refused the namespace (Socket.IO `CONNECT_ERROR`).
    #[error("the notification service refused the connection: {0}")]
    Refused(String),
    /// A frame that is not Engine.IO / Socket.IO as this client knows it.
    #[error("a malformed frame on the notification socket: {0}")]
    Malformed(String),
    /// No ping from the server for `pingInterval + pingTimeout`.
    #[error("the notification socket went quiet: no ping for {0:?}")]
    Dead(Duration),
    /// The websocket failed while open.
    #[error("the notification socket failed: {0}")]
    Transport(String),
}

/// Where to open the websocket for a notification URL, and the Socket.IO namespace to
/// join: `https://host/<path>?<query>` is served at
/// `wss://host/socket.io/?EIO=4&transport=websocket&<query>`, in the namespace `/<path>`.
/// `http` becomes `ws` (the tests' local server).
pub fn websocket_target(notification_url: &Url) -> Result<(String, String), SocketEnd> {
    let scheme = match notification_url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => return Err(SocketEnd::BadUrl(format!("the scheme {other} is not HTTP"))),
    };
    let host = notification_url.host_str().ok_or_else(|| SocketEnd::BadUrl("no host".into()))?;
    let host = match notification_url.host() {
        Some(url::Host::Ipv6(_)) => format!("[{host}]"),
        _ => host.to_owned(),
    };
    let port = notification_url.port().map(|port| format!(":{port}")).unwrap_or_default();
    let mut target = format!("{scheme}://{host}{port}/socket.io/?EIO=4&transport=websocket");
    if let Some(query) = notification_url.query().filter(|query| !query.is_empty()) {
        target.push('&');
        target.push_str(query);
    }
    let path = notification_url.path().trim_end_matches('/');
    let namespace = if path.is_empty() { "/".to_owned() } else { path.to_owned() };
    Ok((target, namespace))
}

/// One open connection to the notification service.
pub struct NotificationSocket {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    namespace: String,
    ping_interval: Duration,
    ping_timeout: Duration,
    /// The last ping from the server, or the open packet before the first one.
    last_ping: Instant,
    host: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenPacket {
    ping_interval: u64,
    ping_timeout: u64,
}

impl NotificationSocket {
    /// Opens the websocket, reads Engine.IO's open packet and joins the default
    /// namespace and the notification URL's own.
    pub async fn connect(notification_url: &Url) -> Result<Self, SocketEnd> {
        let (target, namespace) = websocket_target(notification_url)?;
        let host = notification_url.host_str().unwrap_or("none").to_owned();
        tokio::time::timeout(CONNECT_TIMEOUT, Self::open(target, namespace, host))
            .await
            .map_err(|_| SocketEnd::Connect(format!("no answer in {CONNECT_TIMEOUT:?}")))?
    }

    async fn open(target: String, namespace: String, host: String) -> Result<Self, SocketEnd> {
        let mut request = target.into_client_request().map_err(|e| SocketEnd::BadUrl(redacted(&e)))?;
        request.headers_mut().insert(
            "user-agent",
            tungstenite::http::HeaderValue::from_static(concat!("konedrive/", env!("CARGO_PKG_VERSION"))),
        );
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
        let (mut ws, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), true)
            .await
            .map_err(|e| SocketEnd::Connect(redacted(&e)))?;
        let open = loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => match text.as_str().strip_prefix('0') {
                    Some(json) => break json.to_owned(),
                    None => return Err(SocketEnd::Malformed(format!("expected the open packet, got {}", shown(&text)))),
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(frame))) => return Err(SocketEnd::Closed(close_reason(frame.as_ref()))),
                Some(Ok(other)) => return Err(SocketEnd::Malformed(format!("expected the open packet, got {}", kind(&other)))),
                Some(Err(e)) => return Err(SocketEnd::Connect(redacted(&e))),
                None => return Err(SocketEnd::Closed("the connection ended before the open packet".into())),
            }
        };
        let open: OpenPacket = serde_json::from_str(&open)
            .map_err(|e| SocketEnd::Malformed(format!("an unreadable open packet: {e}")))?;
        ws.send(Message::text("40")).await.map_err(|e| SocketEnd::Transport(redacted(&e)))?;
        if namespace != "/" {
            ws.send(Message::text(format!("40{namespace}"))).await.map_err(|e| SocketEnd::Transport(redacted(&e)))?;
        }
        tracing::debug!(
            host = %host, namespace = %namespace,
            ping_interval_ms = open.ping_interval, ping_timeout_ms = open.ping_timeout,
            "the notification socket is open"
        );
        Ok(Self {
            ws,
            namespace,
            ping_interval: Duration::from_millis(open.ping_interval),
            ping_timeout: Duration::from_millis(open.ping_timeout),
            last_ping: Instant::now(),
            host,
        })
    }

    /// Engine.IO's `pingInterval`, from the open packet.
    pub fn ping_interval(&self) -> Duration {
        self.ping_interval
    }

    /// Engine.IO's `pingTimeout`, from the open packet.
    pub fn ping_timeout(&self) -> Duration {
        self.ping_timeout
    }

    /// Waits for the next `notification` event, answering the server's pings meanwhile.
    /// Any other packet is logged at `debug` and skipped. `Err` ends the connection:
    /// drop the socket after it.
    ///
    /// Safe to cancel between events (in a `select!`): no event is lost that has not been
    /// read. A pong is not safe: when this future is dropped while a pong is being sent,
    /// that pong may be lost, and the server then ends the connection when its
    /// `pingTimeout` runs out (limitations log F182).
    pub async fn notification(&mut self) -> Result<(), SocketEnd> {
        loop {
            if let Heard::Notification = self.heard().await? {
                return Ok(());
            }
        }
    }

    /// As [`notification`](Self::notification), but also returns after each server ping
    /// (answered already): the caller learns the connection is alive. The same caveat on a
    /// pong cut short applies.
    pub async fn heard(&mut self) -> Result<Heard, SocketEnd> {
        loop {
            let quiet = self.ping_interval + self.ping_timeout;
            let message = match tokio::time::timeout_at(self.last_ping + quiet, self.ws.next()).await {
                Err(_) => return Err(SocketEnd::Dead(quiet)),
                Ok(None) => return Err(SocketEnd::Closed("the connection ended".into())),
                Ok(Some(Err(e))) => return Err(SocketEnd::Transport(redacted(&e))),
                Ok(Some(Ok(message))) => message,
            };
            let text = match message {
                Message::Text(text) => text,
                // Websocket-level pings are answered by tungstenite itself.
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(frame) => return Err(SocketEnd::Closed(close_reason(frame.as_ref()))),
                other => return Err(SocketEnd::Malformed(format!("an unexpected {} frame", kind(&other)))),
            };
            match self.engine_packet(text.as_str()).await? {
                Packet::Notification => return Ok(Heard::Notification),
                Packet::Ping => return Ok(Heard::Ping),
                Packet::Ignored(what) => {
                    tracing::debug!(host = %self.host, packet = %shown(text.as_str()), "{what} on the notification socket, ignored")
                }
            }
        }
    }

    /// Closes the websocket politely; errors are of no interest any more.
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    async fn engine_packet(&mut self, text: &str) -> Result<Packet, SocketEnd> {
        let mut chars = text.chars();
        match chars.next() {
            // A second open packet: nothing to do with it.
            Some('0') => Ok(Packet::Ignored("an open packet")),
            Some('1') => Err(SocketEnd::Closed("the server closed the Engine.IO session".into())),
            Some('2') => {
                // The server pings, the client answers with the same payload. When the
                // caller drops `heard()` / `notification()` while this send is under way,
                // the pong may be lost; the server then ends the connection by its
                // `pingTimeout` (limitations log F182).
                self.last_ping = Instant::now();
                let pong = format!("3{}", chars.as_str());
                self.ws.send(Message::text(pong)).await.map_err(|e| SocketEnd::Transport(redacted(&e)))?;
                Ok(Packet::Ping)
            }
            Some('3') => Ok(Packet::Ignored("a pong")),
            Some('4') => self.socket_packet(chars.as_str()),
            Some('5') => Ok(Packet::Ignored("an upgrade")),
            Some('6') => Ok(Packet::Ignored("a noop")),
            _ => Err(SocketEnd::Malformed(format!("not an Engine.IO packet: {}", shown(text)))),
        }
    }

    /// A Socket.IO packet: `<type>[/<namespace>,][<ack id>][<json>]`.
    fn socket_packet(&self, text: &str) -> Result<Packet, SocketEnd> {
        let mut chars = text.chars();
        let kind = chars.next();
        let rest = chars.as_str();
        let (namespace, rest) = match rest.strip_prefix('/') {
            Some(_) => match rest.split_once(',') {
                Some((namespace, rest)) => (namespace, rest),
                None => (rest, ""),
            },
            None => ("/", rest),
        };
        let ours = namespace == "/" || namespace == self.namespace;
        match kind {
            Some('0') => Ok(Packet::Ignored("a namespace joined")),
            Some('1') if ours => Err(SocketEnd::Closed(format!("the server left the namespace {namespace}"))),
            Some('4') if ours => Err(SocketEnd::Refused(shown(rest))),
            Some('2') => {
                let json = rest.trim_start_matches(|c: char| c.is_ascii_digit());
                let event: Vec<serde_json::Value> = serde_json::from_str(json)
                    .map_err(|e| SocketEnd::Malformed(format!("an unreadable event: {e}")))?;
                match event.first().and_then(|name| name.as_str()) {
                    Some("notification") if ours => Ok(Packet::Notification),
                    Some(_) => Ok(Packet::Ignored("another event")),
                    None => Err(SocketEnd::Malformed("an event without a name".into())),
                }
            }
            Some(_) => Ok(Packet::Ignored("a Socket.IO packet")),
            None => Err(SocketEnd::Malformed("an empty Socket.IO message".into())),
        }
    }
}

enum Packet {
    Notification,
    Ping,
    Ignored(&'static str),
}

/// What [`NotificationSocket::heard`] heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// A `notification` event: something in the drive changed.
    Notification,
    /// A server ping, answered: the connection is alive.
    Ping,
}

/// A websocket error without anything that could carry the URL: tungstenite's own
/// texts name no URL, but a failed HTTP answer is cut to its status.
fn redacted(e: &tungstenite::Error) -> String {
    match e {
        tungstenite::Error::Http(response) => format!("the server answered {}", response.status()),
        tungstenite::Error::Url(_) => "the websocket URL is not usable".into(),
        other => other.to_string(),
    }
}

fn close_reason(frame: Option<&tungstenite::protocol::CloseFrame>) -> String {
    match frame {
        Some(frame) if !frame.reason.is_empty() => format!("{} ({})", shown(&frame.reason), u16::from(frame.code)),
        Some(frame) => format!("code {}", u16::from(frame.code)),
        None => "no reason given".into(),
    }
}

fn kind(message: &Message) -> &'static str {
    match message {
        Message::Text(_) => "text",
        Message::Binary(_) => "binary",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Close(_) => "close",
        Message::Frame(_) => "raw",
    }
}

fn shown(text: &str) -> String {
    match text.char_indices().nth(SHOWN) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests;

//! One-shot HTTP listener on the loopback interface for the OAuth redirect.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use url::Url;

/// What the browser reads when nobody said what came of the sign-in.
const UNTOLD: &str = "KOneDrive has the browser's answer. Look at KOneDrive for what came of the sign-in.";

/// How long one connection may take to send its request head.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the browser is kept waiting to be told what came of the sign-in.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    Code(String),
    /// The authorization server reported an error, e.g. `access_denied`.
    Error { error: String, description: String },
}

#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    #[error("timed out waiting for the browser")]
    TimedOut,
    #[error("loopback listener failed: {0}")]
    Io(#[from] std::io::Error),
}

/// The browser's request for the redirect, not answered yet: the page it gets says what
/// came of the sign-in, which is known only once the code is exchanged and the account
/// checked. Whoever has the callback says it with [`say`](Self::say). Dropped untold, or
/// not told in time, the browser is answered all the same, with a page that points to
/// KOneDrive: its connection is never left open.
#[derive(Debug)]
pub struct Answer(oneshot::Sender<String>);

impl Answer {
    /// Holds `stream` until the answer is said, for at most `patience`.
    fn hold(mut stream: TcpStream, patience: Duration) -> Self {
        let (say, said) = oneshot::channel();
        tokio::spawn(async move {
            let sentence = match tokio::time::timeout(patience, said).await {
                Ok(Ok(sentence)) => sentence,
                _ => UNTOLD.to_owned(),
            };
            let _ = respond(&mut stream, "200 OK", &page(&sentence)).await;
        });
        Self(say)
    }

    /// Answers the browser with a page that says `sentence`: that the sign-in went
    /// through, or why it did not. Never a token, a code or a URL.
    pub fn say(self, sentence: &str) {
        let _ = self.0.send(sentence.to_owned());
    }
}

/// The page that says `sentence`, which is text: nothing in it is markup.
fn page(sentence: &str) -> String {
    let mut text = String::with_capacity(sentence.len());
    for c in sentence.chars() {
        match c {
            '&' => text.push_str("&amp;"),
            '<' => text.push_str("&lt;"),
            '>' => text.push_str("&gt;"),
            '"' => text.push_str("&quot;"),
            '\'' => text.push_str("&#39;"),
            c => text.push(c),
        }
    }
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>KOneDrive</title></head>\
         <body><p>{text}</p><p>You can close this tab and return to KOneDrive.</p></body></html>"
    )
}

pub struct LoopbackListener {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl LoopbackListener {
    /// Binds 127.0.0.1 on an ephemeral port, and ::1 on the same port when possible
    /// (browsers may resolve `localhost` to either).
    pub async fn bind() -> std::io::Result<Self> {
        let v4 = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = v4.local_addr()?.port();
        let v6 = TcpListener::bind(("::1", port)).await.ok();
        Ok(Self { v4, v6, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Answers requests until one to `/` carries `state == expected_state` together with
    /// `code` or `error`, or until `timeout` elapses. Any other `GET` gets 404. A request
    /// that is not a `GET`, or whose head does not come whole and in time, is dropped with
    /// no answer. The request that carried the callback is answered through the [`Answer`].
    pub async fn wait(self, expected_state: &str, timeout: Duration) -> Result<(Callback, Answer), LoopbackError> {
        tokio::time::timeout(timeout, self.serve(expected_state))
            .await
            .map_err(|_| LoopbackError::TimedOut)?
    }

    async fn serve(&self, expected_state: &str) -> Result<(Callback, Answer), LoopbackError> {
        loop {
            let (mut stream, _) = match &self.v6 {
                Some(v6) => tokio::select! {
                    accepted = self.v4.accept() => accepted?,
                    accepted = v6.accept() => accepted?,
                },
                None => self.v4.accept().await?,
            };
            let Ok(Ok(target)) = tokio::time::timeout(REQUEST_TIMEOUT, read_target(&mut stream)).await else {
                continue;
            };
            if let Some(callback) = parse_callback(&target, expected_state) {
                return Ok((callback, Answer::hold(stream, ANSWER_TIMEOUT)));
            }
            let _ = respond(&mut stream, "404 Not Found", "").await;
        }
    }
}

/// Reads the request head and returns the target of a `GET` request.
async fn read_target(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 || head.len() > 16 * 1024 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        head.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut request_line = head.lines().next().unwrap_or_default().split(' ');
    match (request_line.next(), request_line.next()) {
        (Some("GET"), Some(target)) => Ok(target.to_owned()),
        _ => Err(std::io::ErrorKind::InvalidData.into()),
    }
}

/// `target` is the request target, e.g. `/?code=...&state=...`.
fn parse_callback(target: &str, expected_state: &str) -> Option<Callback> {
    let url = Url::parse(&format!("http://localhost{target}")).ok()?;
    if url.path() != "/" {
        return None;
    }
    let (mut code, mut state, mut error, mut description) = (None, None, None, String::new());
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = value.into_owned(),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return None;
    }
    match (code, error) {
        (_, Some(error)) => Some(Callback::Error { error, description }),
        (Some(code), None) => Some(Callback::Code(code)),
        (None, None) => None,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests;

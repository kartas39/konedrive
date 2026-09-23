//! One-shot HTTP listener on the loopback interface for the OAuth redirect.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

const DONE_PAGE: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>KOneDrive</title></head>\
<body><p>Sign-in finished. You can close this tab and return to KOneDrive.</p></body></html>";

/// How long one connection may take to send its request head.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// `code` or `error`, or until `timeout` elapses. Every other request gets 404.
    pub async fn wait(self, expected_state: &str, timeout: Duration) -> Result<Callback, LoopbackError> {
        tokio::time::timeout(timeout, self.serve(expected_state))
            .await
            .map_err(|_| LoopbackError::TimedOut)?
    }

    async fn serve(&self, expected_state: &str) -> Result<Callback, LoopbackError> {
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
                let _ = respond(&mut stream, "200 OK", DONE_PAGE).await;
                return Ok(callback);
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
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> (u16, String) {
        let response = reqwest::get(format!("http://127.0.0.1:{port}{target}")).await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    #[tokio::test]
    async fn returns_code_for_matching_state() {
        let listener = LoopbackListener::bind().await.unwrap();
        let port = listener.port();
        assert_eq!(listener.redirect_uri(), format!("http://localhost:{port}"));
        let waiter = tokio::spawn(async move { listener.wait("S1", Duration::from_secs(5)).await });
        let (status, body) = get(port, "/?code=C1&state=S1").await;
        assert_eq!(status, 200);
        assert!(body.contains("close this tab"));
        assert_eq!(waiter.await.unwrap().unwrap(), Callback::Code("C1".into()));
    }

    #[tokio::test]
    async fn ignores_other_paths_and_wrong_state() {
        let listener = LoopbackListener::bind().await.unwrap();
        let port = listener.port();
        let waiter = tokio::spawn(async move { listener.wait("S2", Duration::from_secs(5)).await });
        assert_eq!(get(port, "/favicon.ico").await.0, 404);
        assert_eq!(get(port, "/?code=EVIL&state=WRONG").await.0, 404);
        assert_eq!(get(port, "/?code=C2&state=S2").await.0, 200);
        assert_eq!(waiter.await.unwrap().unwrap(), Callback::Code("C2".into()));
    }

    #[tokio::test]
    async fn reports_authorization_errors() {
        let listener = LoopbackListener::bind().await.unwrap();
        let port = listener.port();
        let waiter = tokio::spawn(async move { listener.wait("S3", Duration::from_secs(5)).await });
        let target = "/?error=access_denied&error_description=user%20cancelled&state=S3";
        assert_eq!(get(port, target).await.0, 200);
        assert_eq!(
            waiter.await.unwrap().unwrap(),
            Callback::Error { error: "access_denied".into(), description: "user cancelled".into() }
        );
    }

    #[tokio::test]
    async fn times_out() {
        let listener = LoopbackListener::bind().await.unwrap();
        let result = listener.wait("S", Duration::from_millis(100)).await;
        assert!(matches!(result, Err(LoopbackError::TimedOut)));
    }
}

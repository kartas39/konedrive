//! The one way a request leaves [`DriveClient`]: who it is authorised as ([`Auth`]), and
//! what happens when the answer is `429` or `503` ([`Throttle`]).
//!
//! Reads wait here and ask again ([`Throttle::Wait`]). Writes get the answer back
//! ([`Throttle::Return`]) and come back as
//! [`WriteError::Throttled`](super::WriteError::Throttled): the worker pauses the whole
//! account. A fragment of an upload session is sent with [`Throttle::Return`] too, and
//! [`DriveClient::upload_chunk`] does the rest: it waits, asks the session where it stands,
//! and sends the fragment again only if the session still expects it. The account page's
//! two calls get the answer back and tell nobody ([`Throttle::Pass`]).

use std::time::{Duration, SystemTime};

use super::error::{classify, Detail, Kind, Status};
use super::write::retry_after;
use super::{DriveClient, DriveError};
use crate::token::AuthError;

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

/// What a request is authorised by.
#[derive(Debug, Clone, Copy)]
pub(super) enum Auth<'a> {
    /// The account's token, from the client's token source. A `401` is answered once by
    /// dropping the cached token and asking again; a second one is an error.
    Account,
    /// A token the caller holds: sent as it is, and a `401` goes back to the caller.
    Bearer(&'a str),
    /// A pre-authenticated download URL: it carries its own authorisation, and never gets
    /// a token.
    Link,
    /// An upload session's URL: pre-authenticated as well.
    Session,
}

impl Auth<'_> {
    /// Who did not answer, for the error's text.
    fn service(self) -> &'static str {
        match self {
            Auth::Account | Auth::Bearer(_) => "Microsoft Graph",
            Auth::Link => "OneDrive",
            Auth::Session => "OneDrive's upload service",
        }
    }
}

/// What a `429` or a `503` does to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Throttle {
    /// Wait as told (`Retry-After`, else the policy's wait; capped) and ask again, up to
    /// the policy's `attempts` answers of that kind. The pool is told of each wait.
    Wait,
    /// The answer goes back to the caller as it is, the pool told of it first.
    Return,
    /// The answer goes back to the caller as it is, and the pool is not told: the call is
    /// none of the account's transfers.
    Pass,
}

impl DriveClient {
    /// Sends a request, built anew by `request` for each try. The answer comes back
    /// whatever its status, except for what `auth` and `throttle` take care of here.
    pub(super) async fn send(
        &self,
        auth: Auth<'_>,
        throttle: Throttle,
        request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, DriveError> {
        let service = auth.service();
        let mut renewed = false;
        let mut throttled = 0;
        loop {
            let request = match auth {
                Auth::Account => {
                    let token = self.token().await?;
                    request().bearer_auth(token)
                }
                Auth::Bearer(token) => request().bearer_auth(token),
                Auth::Link | Auth::Session => request(),
            };
            let response = request
                .send()
                .await
                .map_err(|e| DriveError::Transient(format!("cannot reach {service}: {}", e.without_url()).into()))?;
            let status = Status::of(&response);
            match (classify(status, ""), auth, throttle) {
                (Kind::Unauthorized, Auth::Account, _) if !renewed => {
                    renewed = true;
                    self.tokens.invalidate().await;
                }
                (Kind::Unauthorized, Auth::Account, _) => {
                    return Err(DriveError::Failed(Detail::answered("Microsoft Graph rejected the access token", status, "")))
                }
                (Kind::Throttled, _, Throttle::Wait) => {
                    throttled += 1;
                    if throttled >= self.retry.attempts {
                        return Err(DriveError::Transient(Detail::answered(
                            format!("{service} kept answering {status}"),
                            status,
                            "",
                        )));
                    }
                    let wait = self.wait_for(&response);
                    self.pool.throttled(Some(wait));
                    tokio::time::sleep(wait).await;
                }
                (Kind::Throttled, _, Throttle::Return) => {
                    self.pool.throttled(retry_after(response.headers(), SystemTime::now()));
                    return Ok(response);
                }
                _ => return Ok(response),
            }
        }
    }

    async fn token(&self) -> Result<String, DriveError> {
        self.tokens.access_token().await.map_err(|e| match e {
            AuthError::SignedOut => DriveError::SignedOut,
            AuthError::Locked => DriveError::Transient("the secret storage is locked".into()),
            AuthError::Transient(message) => DriveError::Transient(message.into()),
        })
    }

    /// `Retry-After` in seconds or as an HTTP date, else the default; capped.
    pub(super) fn wait_for(&self, response: &reqwest::Response) -> Duration {
        retry_after(response.headers(), SystemTime::now()).unwrap_or(self.retry.default_wait).min(self.retry.max_wait)
    }
}

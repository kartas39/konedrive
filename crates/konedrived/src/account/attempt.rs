//! One sign-in attempt in the browser, whoever it is for: the loopback listener, the wait
//! for the browser, the exchange of the code, and the question of which drive and whose
//! email the tokens are for. An account runs it to sign in again and to switch to
//! read-write; the accounts manager runs it for a new account, which is made only once
//! the attempt has succeeded (`Accounts.SignIn`). What a result leads to is its caller's
//! business: nothing here touches an account, `config.toml` or the wallet.

use std::time::Duration;

use konedrive_graph::drive::DriveClient;
use konedrive_graph::loopback::{Callback, LoopbackError, LoopbackListener};
use konedrive_graph::oauth::{scopes_for, Endpoints, OAuthClient, TokenResponse};
use konedrive_graph::pkce::{random_token, Pkce};
use konedrive_graph::token::StaticToken;
use tokio::sync::oneshot;

use crate::config::{DriveId, Mode};

/// What a sign-in is refused with when Graph names no drive.
pub(crate) const NO_DRIVE: &str = "Microsoft Graph did not say which drive this is";

/// The HTTP client a sign-in's token requests go through.
pub(crate) fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("konedrive/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
}

/// A client asking for `mode`'s scope, for the authorization, the code exchange or a
/// refresh.
pub(crate) fn oauth_client(http: reqwest::Client, endpoints: Endpoints, client_id: &str, mode: Mode) -> OAuthClient {
    OAuthClient::new(http, endpoints, client_id.to_owned()).with_scope(scopes_for(mode == Mode::ReadWrite))
}

/// An attempt that waits for the browser: its listener is bound, and its authorization URL
/// can be made.
pub(crate) struct Attempt {
    pub(crate) oauth: OAuthClient,
    listener: LoopbackListener,
    pub(crate) redirect_uri: String,
    pub(crate) pkce: Pkce,
    pub(crate) csrf: String,
}

impl Attempt {
    /// Binds the loopback listener the browser answers on. `Err` says why it cannot be.
    pub(crate) async fn bind(oauth: OAuthClient) -> Result<Self, String> {
        let listener = LoopbackListener::bind().await.map_err(|e| format!("cannot listen on localhost: {e}"))?;
        let redirect_uri = listener.redirect_uri();
        Ok(Self { oauth, listener, redirect_uri, pkce: Pkce::new(), csrf: random_token() })
    }

    /// The scope the attempt asks for.
    pub(crate) fn scope(&self) -> &'static str {
        self.oauth.scope()
    }

    /// The authorization URL with Microsoft's account picker, so that any account can be
    /// chosen.
    pub(crate) fn picker_url(&self) -> String {
        self.oauth.picker_authorize_url(&self.redirect_uri, &self.pkce, &self.csrf).to_string()
    }

    /// Waits for the browser, for at most `timeout`, and exchanges the code. `Err` says
    /// why there are no tokens; empty when `cancel` ended the wait.
    pub(crate) async fn tokens(self, timeout: Duration, cancel: oneshot::Receiver<()>) -> Result<TokenResponse, String> {
        let Attempt { oauth, listener, redirect_uri, pkce, csrf } = self;
        let callback = tokio::select! {
            result = listener.wait(&csrf, timeout) => result,
            _ = cancel => return Err(String::new()),
        };
        let code = match callback {
            Ok(Callback::Code(code)) => code,
            Ok(Callback::Error { error, .. }) if error == "access_denied" => {
                return Err("Access was denied in the browser.".into())
            }
            Ok(Callback::Error { error, description }) => {
                return Err(format!("Microsoft reported an error: {error}: {description}"))
            }
            Err(LoopbackError::TimedOut) => {
                return Err("Timed out waiting for the browser. If Microsoft showed an error page, \
                            check the app registration: client ID and redirect URI http://localhost."
                    .into())
            }
            Err(e) => return Err(e.to_string()),
        };
        oauth
            .exchange_code(&code, &pkce.verifier, &redirect_uri)
            .await
            .map_err(|e| format!("Sign-in failed: {e}"))
    }
}

/// Who a sign-in turned out to be (`docs/design/accounts.md` §6.2): the drive, and the email when `/me`
/// answered.
pub(crate) struct Identity {
    pub(crate) drive: DriveId,
    pub(crate) email: Option<String>,
}

/// What an attempt that succeeded has: these tokens, this drive, this email.
pub(crate) struct SignedIn {
    pub(crate) tokens: TokenResponse,
    pub(crate) refresh_token: String,
    pub(crate) identity: Identity,
}

/// Why a token response is no sign-in.
pub(crate) enum Unconfirmed {
    /// It carries no refresh token.
    NoRefreshToken,
    /// Graph did not say which drive the token is for: why.
    Unidentified(String),
}

/// A token response as a sign-in: its refresh token, and whose it is, asked of `graph`
/// with the new access token. Only the drive is needed: it is the check (§6.2). The email
/// names the account and its wallet item.
pub(crate) async fn confirm(tokens: TokenResponse, graph: &DriveClient) -> Result<SignedIn, Unconfirmed> {
    let Some(refresh_token) = tokens.refresh_token.clone() else { return Err(Unconfirmed::NoRefreshToken) };
    let (drive, profile) = tokio::join!(graph.drive(&tokens.access_token), graph.profile(&tokens.access_token));
    let drive = drive.map_err(|e| e.to_string()).and_then(|drive| DriveId::new(drive.id).ok_or_else(|| NO_DRIVE.to_owned()));
    let drive = drive.map_err(Unconfirmed::Unidentified)?;
    let identity = Identity { drive, email: profile.ok().map(|p| p.email).filter(|e| !e.is_empty()) };
    Ok(SignedIn { tokens, refresh_token, identity })
}

impl Unconfirmed {
    /// As a sign-in says it.
    pub(crate) fn sentence(&self) -> String {
        match self {
            Unconfirmed::NoRefreshToken => "Microsoft did not return a refresh token.".into(),
            Unconfirmed::Unidentified(why) => format!("Could not check which account this is ({why}); try again."),
        }
    }
}

/// A Graph client for an attempt that belongs to no account: every question
/// [`confirm`] asks carries the token it is asked with.
pub(crate) fn graph(endpoints: &Endpoints) -> anyhow::Result<DriveClient> {
    DriveClient::new(endpoints.graph.clone(), std::sync::Arc::new(StaticToken::new(String::new())))
}

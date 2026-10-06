//! Microsoft identity platform: authorization URL and token endpoint.

use serde::Deserialize;
use url::Url;

use crate::pkce::Pkce;

/// What a read-only account asks for: reading its files, its profile, and a refresh token.
pub const SCOPES: &str = "Files.Read User.Read offline_access";

/// What a read-write account asks for: changing its files too (`docs/design/writes.md` §2).
pub const READ_WRITE_SCOPES: &str = "Files.ReadWrite User.Read offline_access";

/// The scope an account signs in and refreshes with: `read_write` says whether its mode is
/// read-write. A read-only account keeps
/// asking for `Files.Read` at every refresh, so its access tokens cannot write even when
/// its grant is wider (§2.1): Microsoft allows a refresh to ask for "equivalent to or
/// a subset of" what was granted, and so keeps enforcing that nothing is written.
pub fn scopes_for(read_write: bool) -> &'static str {
    if read_write {
        READ_WRITE_SCOPES
    } else {
        SCOPES
    }
}

/// The permissions a token response's `scope` names, without the resource: Microsoft may
/// spell one `Files.Read` or `https://graph.microsoft.com/Files.Read`.
fn permissions(scope: &str) -> impl Iterator<Item = &str> {
    scope.split_whitespace().map(|one| one.rsplit('/').next().unwrap_or(one))
}

/// Whether a token valid for `scope` may change files: it carries `Files.ReadWrite` (or
/// `Files.ReadWrite.All`), in any case.
pub fn grants_writes(scope: &str) -> bool {
    permissions(scope).any(|p| p.eq_ignore_ascii_case("Files.ReadWrite") || p.eq_ignore_ascii_case("Files.ReadWrite.All"))
}

/// Whether a token valid for `scope` can change nothing: none of its permissions writes,
/// manages or fully controls anything. Wider than [`grants_writes`] on purpose — the read-only
/// token `TokenExport.ReadOnly` hands out must not carry `Sites.ReadWrite.All` either.
pub fn is_read_only(scope: &str) -> bool {
    !permissions(scope).any(|p| {
        let p = p.to_ascii_lowercase();
        p.contains("write") || p.contains("manage") || p.contains("fullcontrol")
    })
}

/// Base URLs, both ending in `/`. Tests point them at a mock server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// `https://login.microsoftonline.com/consumers/oauth2/v2.0/`
    pub authority: Url,
    /// `https://graph.microsoft.com/v1.0/`
    pub graph: Url,
}

impl Endpoints {
    pub fn microsoft() -> Self {
        Self {
            authority: Url::parse("https://login.microsoftonline.com/consumers/oauth2/v2.0/").unwrap(),
            graph: Url::parse("https://graph.microsoft.com/v1.0/").unwrap(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// "The scopes that the access token is valid for." Absent means what was asked for
    /// (RFC 6749 §5.1): [`TokenResponse::granted`].
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenResponse {
    /// What the token is valid for: its `scope`, or `asked` when the response has none.
    pub fn granted(&self, asked: &str) -> String {
        self.scope.clone().unwrap_or_else(|| asked.to_owned())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The code or refresh token is no longer valid; the user must sign in again.
    #[error("invalid_grant: {0}")]
    InvalidGrant(String),
    /// Rejected for another reason (unknown client ID, bad redirect URI, …).
    #[error("{error}: {description}")]
    Rejected { error: String, description: String },
    /// Network problem or server error; worth retrying.
    #[error("{0}")]
    Transient(String),
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(default)]
    error_description: String,
}

#[derive(Clone)]
pub struct OAuthClient {
    http: reqwest::Client,
    endpoints: Endpoints,
    client_id: String,
    /// For the authorization, the code exchange and every refresh alike.
    scope: &'static str,
}

impl OAuthClient {
    /// A client asking for the read-only scope ([`SCOPES`]).
    pub fn new(http: reqwest::Client, endpoints: Endpoints, client_id: String) -> Self {
        Self { http, endpoints, client_id, scope: SCOPES }
    }

    /// The same client asking for `scope` instead ([`scopes_for`]).
    pub fn with_scope(mut self, scope: &'static str) -> Self {
        self.scope = scope;
        self
    }

    /// What this client asks for.
    pub fn scope(&self) -> &'static str {
        self.scope
    }

    /// [`authorize_url`](Self::authorize_url) with Microsoft's account picker
    /// (`prompt=select_account`): the sign-in page lists the accounts the browser knows and
    /// "Use another account", rather than going on silently as whoever the browser is signed in
    /// as. Every sign-in that is not pinned uses it, so a second account can be added from a
    /// browser already signed in to the first.
    pub fn picker_authorize_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str) -> Url {
        let mut url = self.authorize_url(redirect_uri, pkce, state);
        url.query_pairs_mut().append_pair("prompt", "select_account");
        url
    }

    /// [`authorize_url`](Self::authorize_url), pinned to one Microsoft account: the sign-in
    /// page asks for that account's password again (`prompt=login`) with its name filled in
    /// (`login_hint`, when it is known), rather than taking whoever the browser is signed in
    /// as. Every sign-in that asks for `Files.ReadWrite` is pinned (`docs/design/writes.md` §2): a browser
    /// signed in to another account — the user's real one — cannot consent to write access for
    /// it with one stray click.
    pub fn pinned_authorize_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str, login_hint: Option<&str>) -> Url {
        let mut url = self.authorize_url(redirect_uri, pkce, state);
        url.query_pairs_mut().append_pair("prompt", "login");
        if let Some(hint) = login_hint.filter(|hint| !hint.is_empty()) {
            url.query_pairs_mut().append_pair("login_hint", hint);
        }
        url
    }

    pub fn authorize_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str) -> Url {
        let mut url = self.endpoints.authority.join("authorize").unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", self.scope)
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        url
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<TokenResponse, OAuthError> {
        self.token_request(&[
            ("client_id", self.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("scope", self.scope),
        ])
        .await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, OAuthError> {
        self.token_request(&[
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", self.scope),
        ])
        .await
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse, OAuthError> {
        let url = self.endpoints.authority.join("token").unwrap();
        let response = self
            .http
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(|e| OAuthError::Transient(format!("cannot reach Microsoft: {}", e.without_url())))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json::<TokenResponse>()
                .await
                .map_err(|e| OAuthError::Transient(format!("unreadable token response: {}", e.without_url())));
        }
        if status.is_server_error() {
            return Err(OAuthError::Transient(format!("token endpoint returned {status}")));
        }
        let body: ErrorBody = response
            .json()
            .await
            .map_err(|e| OAuthError::Transient(format!("unreadable error response ({status}): {}", e.without_url())))?;
        if body.error == "invalid_grant" {
            Err(OAuthError::InvalidGrant(body.error_description))
        } else {
            Err(OAuthError::Rejected { error: body.error, description: body.error_description })
        }
    }
}

#[cfg(test)]
mod tests;

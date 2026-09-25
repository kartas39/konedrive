//! Microsoft identity platform: authorization URL and token endpoint.

use serde::Deserialize;
use url::Url;

use crate::config::Mode;
use crate::pkce::Pkce;

/// What a read-only account asks for: reading its files, its profile, and a refresh token.
pub const SCOPES: &str = "Files.Read User.Read offline_access";

/// What a read-write account asks for: changing its files too (`docs/design/writes.md` §2).
pub const READ_WRITE_SCOPES: &str = "Files.ReadWrite User.Read offline_access";

/// The scope an account in `mode` signs in and refreshes with. A read-only account keeps
/// asking for `Files.Read` at every refresh, so its access tokens cannot write even when
/// its grant is wider (design §9): Microsoft allows a refresh to ask for "equivalent to or
/// a subset of" what was granted, and so keeps enforcing that nothing is written.
pub fn scopes_for(mode: Mode) -> &'static str {
    match mode {
        Mode::ReadOnly => SCOPES,
        Mode::ReadWrite => READ_WRITE_SCOPES,
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
/// token `Dev1.AccessToken` hands out must not carry `Sites.ReadWrite.All` either.
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
            .map_err(|e| OAuthError::Transient(format!("cannot reach Microsoft: {e}")))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json::<TokenResponse>()
                .await
                .map_err(|e| OAuthError::Transient(format!("unreadable token response: {e}")));
        }
        if status.is_server_error() {
            return Err(OAuthError::Transient(format!("token endpoint returned {status}")));
        }
        let body: ErrorBody = response
            .json()
            .await
            .map_err(|e| OAuthError::Transient(format!("unreadable error response ({status}): {e}")))?;
        if body.error == "invalid_grant" {
            Err(OAuthError::InvalidGrant(body.error_description))
        } else {
            Err(OAuthError::Rejected { error: body.error, description: body.error_description })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn client() -> OAuthClient {
        OAuthClient::new(
            reqwest::Client::new(),
            Endpoints::microsoft(),
            "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
        )
    }

    #[test]
    fn authorize_url_carries_all_parameters() {
        let pkce = Pkce::from_verifier("verifier".into());
        let url = client().authorize_url("http://localhost:4321", &pkce, "xyz");
        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize"
        );
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "0f8fad5b-d9cb-469f-a165-70867728950e");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:4321");
        assert_eq!(q["response_mode"], "query");
        assert_eq!(q["scope"], "Files.Read User.Read offline_access");
        assert_eq!(q["state"], "xyz");
        assert_eq!(q["code_challenge"], pkce.challenge);
        assert_eq!(q["code_challenge_method"], "S256");
    }

    /// Each mode asks for its own scope; only read-write asks to change files.
    #[test]
    fn each_mode_asks_for_its_own_scope() {
        let pkce = Pkce::from_verifier("verifier".into());
        let url = client().with_scope(scopes_for(Mode::ReadWrite)).authorize_url("http://localhost:1", &pkce, "s");
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], "Files.ReadWrite User.Read offline_access");
        assert_eq!(scopes_for(Mode::ReadOnly), "Files.Read User.Read offline_access");
        assert!(grants_writes(scopes_for(Mode::ReadWrite)) && !grants_writes(scopes_for(Mode::ReadOnly)));
        assert!(is_read_only(scopes_for(Mode::ReadOnly)) && !is_read_only(scopes_for(Mode::ReadWrite)));

        let pinned = client().pinned_authorize_url("http://localhost:1", &pkce, "s", Some("test@outlook.com"));
        let q: HashMap<String, String> = pinned.query_pairs().into_owned().collect();
        assert_eq!((q["prompt"].as_str(), q["login_hint"].as_str()), ("login", "test@outlook.com"));
        let unknown = client().pinned_authorize_url("http://localhost:1", &pkce, "s", Some(""));
        let q: HashMap<String, String> = unknown.query_pairs().into_owned().collect();
        assert_eq!(q["prompt"], "login");
        assert!(!q.contains_key("login_hint"));
    }

    /// A granted scope is read however Microsoft spells it: with the resource or without, in
    /// any case. Anything that writes is not read-only, whether or not it writes files.
    #[test]
    fn granted_scopes_are_read_by_permission() {
        assert!(grants_writes("https://graph.microsoft.com/Files.ReadWrite https://graph.microsoft.com/User.Read"));
        assert!(grants_writes("files.readwrite.all openid"));
        assert!(!grants_writes("Files.Read Files.ReadWrite.AppFolder User.Read"));
        assert!(!grants_writes(""));
        assert!(is_read_only("https://graph.microsoft.com/Files.Read User.Read offline_access openid profile"));
        assert!(!is_read_only("Files.Read Sites.ReadWrite.All"));
        assert!(!is_read_only("Files.Read Sites.Manage.All"));
        let response = TokenResponse { access_token: "AT".into(), expires_in: 1, refresh_token: None, scope: None };
        assert_eq!(response.granted(SCOPES), SCOPES, "no scope in the response is what was asked for");
    }

    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mock_client(server: &MockServer) -> OAuthClient {
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        OAuthClient::new(
            reqwest::Client::new(),
            Endpoints { authority: base.clone(), graph: base },
            "cid".into(),
        )
    }

    async fn mock_token(server: &MockServer, status: u16, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn exchange_code_posts_pkce_verifier_and_parses_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=the-code"))
            .and(body_string_contains("code_verifier=the-verifier"))
            .and(body_string_contains("client_id=cid"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token_type": "Bearer", "access_token": "AT", "expires_in": 3600, "refresh_token": "RT"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let tokens = mock_client(&server)
            .exchange_code("the-code", "the-verifier", "http://localhost:1")
            .await
            .unwrap();
        assert_eq!(tokens.access_token, "AT");
        assert_eq!(tokens.expires_in, 3600);
        assert_eq!(tokens.refresh_token.as_deref(), Some("RT"));
    }

    #[tokio::test]
    async fn refresh_maps_invalid_grant() {
        let server = MockServer::start().await;
        mock_token(&server, 400, json!({"error": "invalid_grant", "error_description": "expired"})).await;
        let err = mock_client(&server).refresh("RT").await.unwrap_err();
        assert!(matches!(err, OAuthError::InvalidGrant(ref d) if d == "expired"), "{err:?}");
    }

    #[tokio::test]
    async fn other_client_errors_are_rejected() {
        let server = MockServer::start().await;
        mock_token(&server, 400, json!({"error": "invalid_client", "error_description": "bad app"})).await;
        let err = mock_client(&server).refresh("RT").await.unwrap_err();
        assert!(matches!(err, OAuthError::Rejected { ref error, .. } if error == "invalid_client"), "{err:?}");
    }

    #[tokio::test]
    async fn server_errors_are_transient() {
        let server = MockServer::start().await;
        mock_token(&server, 503, json!({})).await;
        let err = mock_client(&server).refresh("RT").await.unwrap_err();
        assert!(matches!(err, OAuthError::Transient(_)), "{err:?}");
    }

    #[tokio::test]
    async fn connection_failures_are_transient() {
        let base = Url::parse("http://127.0.0.1:9/").unwrap();
        let client = OAuthClient::new(
            reqwest::Client::new(),
            Endpoints { authority: base.clone(), graph: base },
            "cid".into(),
        );
        assert!(matches!(client.refresh("RT").await, Err(OAuthError::Transient(_))));
    }
}

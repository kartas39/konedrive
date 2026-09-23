//! Microsoft identity platform: authorization URL and token endpoint.

use serde::Deserialize;
use url::Url;

use crate::pkce::Pkce;

pub const SCOPES: &str = "Files.Read User.Read offline_access";

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
}

impl OAuthClient {
    pub fn new(http: reqwest::Client, endpoints: Endpoints, client_id: String) -> Self {
        Self { http, endpoints, client_id }
    }

    pub fn authorize_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str) -> Url {
        let mut url = self.endpoints.authority.join("authorize").unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", SCOPES)
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
            ("scope", SCOPES),
        ])
        .await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, OAuthError> {
        self.token_request(&[
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", SCOPES),
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

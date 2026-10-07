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
    let url = client().with_scope(scopes_for(true)).authorize_url("http://localhost:1", &pkce, "s");
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(q["scope"], "Files.ReadWrite User.Read offline_access");
    assert_eq!(scopes_for(false), "Files.Read User.Read offline_access");
    assert!(grants_writes(scopes_for(true)) && !grants_writes(scopes_for(false)));
    assert!(is_read_only(scopes_for(false)) && !is_read_only(scopes_for(true)));

    let pinned = client().pinned_authorize_url("http://localhost:1", &pkce, "s", Some("test@outlook.com"));
    let q: HashMap<String, String> = pinned.query_pairs().into_owned().collect();
    assert_eq!((q["prompt"].as_str(), q["login_hint"].as_str()), ("login", "test@outlook.com"));
    let unknown = client().pinned_authorize_url("http://localhost:1", &pkce, "s", Some(""));
    let q: HashMap<String, String> = unknown.query_pairs().into_owned().collect();
    assert_eq!(q["prompt"], "login");
    assert!(!q.contains_key("login_hint"));

    let picker = client().picker_authorize_url("http://localhost:1", &pkce, "s");
    let q: HashMap<String, String> = picker.query_pairs().into_owned().collect();
    assert_eq!(q["prompt"], "select_account");
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

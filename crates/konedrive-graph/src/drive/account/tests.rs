use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use std::sync::Arc;

use url::Url;

use super::*;
use crate::token::StaticToken;

/// The client's own token is not the one these calls send.
fn graph(server: &MockServer) -> DriveClient {
    DriveClient::new(Url::parse(&format!("{}/", server.uri())).unwrap(), Arc::new(StaticToken::new("the client's own"))).unwrap()
}

async fn mock_get(server: &MockServer, route: &str, status: u16, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .and(header("authorization", "Bearer T"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn profile_prefers_mail() {
    let server = MockServer::start().await;
    mock_get(&server, "/me", 200, json!({"displayName": "Ann", "mail": "ann@example.com", "userPrincipalName": "upn@example.com"})).await;
    let profile = graph(&server).profile("T").await.unwrap();
    assert_eq!(profile, Profile { display_name: "Ann".into(), email: "ann@example.com".into() });
}

#[tokio::test]
async fn profile_falls_back_to_user_principal_name() {
    let server = MockServer::start().await;
    mock_get(&server, "/me", 200, json!({"displayName": "Ann", "mail": null, "userPrincipalName": "ann@outlook.com"})).await;
    assert_eq!(graph(&server).profile("T").await.unwrap().email, "ann@outlook.com");
}

#[tokio::test]
async fn drive_reads_its_id_and_quota() {
    let server = MockServer::start().await;
    mock_get(&server, "/me/drive", 200, json!({"id": "d", "quota": {"used": 10, "total": 100, "remaining": 90}})).await;
    assert_eq!(
        graph(&server).drive("T").await.unwrap(),
        Drive { id: "d".into(), quota: DriveQuota { used: 10, total: 100, remaining: Some(90), state: String::new() } }
    );
}

#[tokio::test]
async fn distinguishes_unauthorized_from_other_failures() {
    let server = MockServer::start().await;
    mock_get(&server, "/me", 401, json!({})).await;
    mock_get(&server, "/me/drive", 500, json!({})).await;
    assert!(matches!(graph(&server).profile("T").await, Err(e @ DriveError::Failed(_)) if e.status() == Some(Status::UNAUTHORIZED)));
    assert!(matches!(graph(&server).drive("T").await, Err(e @ DriveError::Failed(_)) if e.status() == Some(Status::new(500))));
}

/// The token is the caller's: a rejected one is not renewed, and throttling is not waited
/// out. Each answer goes back after the one request.
#[tokio::test]
async fn neither_renews_the_token_nor_waits_out_throttling() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me")).respond_with(ResponseTemplate::new(401)).expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "3600"))
        .expect(1)
        .mount(&server)
        .await;
    let graph = graph(&server);
    let rejected = graph.profile("T").await.unwrap_err();
    assert_eq!(rejected.to_string(), "Microsoft Graph rejected the access token");
    let width = graph.pool().size();
    let throttled = graph.drive("T").await.unwrap_err();
    assert_eq!(throttled.to_string(), "me/drive returned 429 Too Many Requests");
    assert_eq!(throttled.status(), Some(Status::new(429)));
    assert_eq!(graph.pool().size(), width, "the transfer pool is not told");
}

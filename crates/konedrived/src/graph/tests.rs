use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn graph(server: &MockServer) -> GraphClient {
    GraphClient::new(reqwest::Client::new(), Url::parse(&format!("{}/", server.uri())).unwrap())
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
        Drive { id: "d".into(), quota: Quota { used: 10, total: 100, remaining: Some(90), state: String::new() } }
    );
}

#[tokio::test]
async fn distinguishes_unauthorized_from_other_failures() {
    let server = MockServer::start().await;
    mock_get(&server, "/me", 401, json!({})).await;
    mock_get(&server, "/me/drive", 500, json!({})).await;
    assert!(matches!(graph(&server).profile("T").await, Err(GraphError::Unauthorized)));
    assert!(matches!(graph(&server).drive("T").await, Err(GraphError::Failed(_))));
}

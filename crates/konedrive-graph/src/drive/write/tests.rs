use std::sync::Arc;

use serde_json::json;
use url::Url;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::token::StaticToken;

fn client(server: &MockServer) -> DriveClient {
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    DriveClient::new(base, Arc::new(StaticToken::new("T"))).unwrap()
}

#[tokio::test]
async fn a_new_folder_is_posted_with_conflict_behaviour_fail() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/me/drive/items/P!1/children")).and(header("authorization", "Bearer T"))
        .and(body_json(json!({"name": "Фото", "folder": {}, "@microsoft.graph.conflictBehavior": "fail"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "F", "name": "Фото", "eTag": "e1", "folder": {}})))
        .mount(&server).await;
    let folder = client(&server).create_folder("P!1", "Фото").await.unwrap();
    assert_eq!((folder.id.as_str(), folder.e_tag.as_deref()), ("F", Some("e1")));
}

#[tokio::test]
async fn a_rename_and_move_is_one_patch_guarded_by_the_etag() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH")).and(path("/me/drive/items/I")).and(header("if-match", "e1"))
        .and(body_json(json!({
            "name": "b.txt", "parentReference": {"id": "Q"},
            "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "I", "name": "b.txt", "eTag": "e2"})))
        .mount(&server).await;
    let change = ItemChange { name: Some("b.txt"), parent_id: Some("Q"), modified: Some(1_714_557_600) };
    let item = client(&server).update_item("I", "e1", &change).await.unwrap();
    assert_eq!(item.e_tag.as_deref(), Some("e2"));
}

#[tokio::test]
async fn a_delete_carries_its_guard() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE")).and(path("/me/drive/items/D")).and(header("if-match", "c7"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server).await;
    client(&server).delete_item("D", "c7").await.unwrap();
}

/// The error a `DELETE` answered with `answer` comes back as, checking
/// that it was sent exactly once: nothing here retries or waits.
async fn refusal(answer: ResponseTemplate) -> WriteError {
    let server = MockServer::start().await;
    Mock::given(method("DELETE")).respond_with(answer).mount(&server).await;
    let err = client(&server).delete_item("X", "e").await.unwrap_err();
    assert_eq!(server.received_requests().await.unwrap().len(), 1, "{err:?}: sent once");
    err
}

/// §3.6's answers, each to the error the worker acts on. A throttle is
/// handed back with its wait rather than waited out here.
#[tokio::test]
async fn every_refusal_comes_back_as_its_typed_error() {
    let error = |code: &str, message: &str| json!({"error": {"code": code, "message": message}});
    let answer = ResponseTemplate::new;
    assert!(matches!(refusal(answer(412)).await, WriteError::Changed));
    let taken = answer(409).set_body_json(error("nameAlreadyExists", "taken"));
    assert!(matches!(refusal(taken).await, WriteError::NameExists));
    assert!(matches!(refusal(answer(404)).await, WriteError::NotFound));
    assert!(matches!(refusal(answer(507)).await, WriteError::QuotaExceeded));
    let full = answer(403).set_body_json(error("quotaLimitReached", "full"));
    assert!(matches!(refusal(full).await, WriteError::QuotaExceeded));
    assert!(matches!(refusal(answer(423)).await, WriteError::Locked));
    assert!(matches!(refusal(answer(403)).await, WriteError::Forbidden));
    let refused = refusal(answer(400).set_body_json(error("invalidRequest", "The name is not valid"))).await;
    assert!(matches!(&refused, WriteError::Refused(m) if m == "The name is not valid"), "{refused:?}");
    assert!(matches!(refusal(answer(502)).await, WriteError::Transient(_)));
    let throttled = refusal(answer(429).insert_header("retry-after", "7")).await;
    assert!(
        matches!(throttled, WriteError::Throttled { retry_after: Some(d) } if d == Duration::from_secs(7)),
        "{throttled:?}"
    );
    assert!(matches!(refusal(answer(503)).await, WriteError::Throttled { retry_after: None }));
}

#[test]
fn retry_after_reads_seconds_and_http_dates() {
    let now = UNIX_EPOCH + Duration::from_secs(784_111_777); // Sun, 06 Nov 1994 08:49:37 GMT
    let wait = |value: &str| {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::RETRY_AFTER, value.parse().unwrap());
        retry_after(&headers, now)
    };
    assert_eq!(wait("120"), Some(Duration::from_secs(120)));
    assert_eq!(wait("Sun, 06 Nov 1994 08:50:07 GMT"), Some(Duration::from_secs(30)));
    assert_eq!(wait("Sunday, 06-Nov-94 08:50:07 GMT"), Some(Duration::from_secs(30)), "RFC 850");
    assert_eq!(wait("Sun Nov  6 08:50:07 1994"), Some(Duration::from_secs(30)), "asctime");
    assert_eq!(wait("Sun, 06 Nov 1994 08:00:00 GMT"), Some(Duration::ZERO), "already past");
    assert_eq!(wait("999999"), Some(MAX_RETRY_AFTER), "bounded");
    assert_eq!(wait("soon"), None);
    assert_eq!(retry_after(&header::HeaderMap::new(), now), None);
}

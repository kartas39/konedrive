use std::sync::Arc;

use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::drive::RetryPolicy;
use crate::token::StaticToken;

/// 2024-05-01T10:00:00Z, and a day later.
const MAY_1: i64 = 1_714_557_600;
const MAY_2_TEXT: &str = "2024-05-02T10:00:00Z";

fn client(server: &MockServer) -> DriveClient {
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    DriveClient::new(base, Arc::new(StaticToken::new("T"))).unwrap()
}

#[tokio::test]
async fn a_new_file_is_one_session_and_its_put_goes_without_the_token() {
    let graph = MockServer::start().await;
    let up = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/me/drive/items/P!1:/a%20b%23.txt:/createUploadSession"))
        .and(header("authorization", "Bearer T"))
        .and(body_json(json!({"item": {
            "@microsoft.graph.conflictBehavior": "fail", "name": "a b#.txt",
            "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadUrl": format!("{}/up/s1", up.uri()), "expirationDateTime": MAY_2_TEXT
        })))
        .mount(&graph).await;
    Mock::given(method("PUT")).and(path("/up/s1")).and(header("content-range", "bytes 0-4/5"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "N", "name": "a b#.txt", "size": 5, "eTag": "e1"})))
        .mount(&up).await;
    let target = UploadTarget::New { parent_id: "P!1", name: "a b#.txt" };
    let drive = client(&graph);
    let session = drive.create_upload_session(target, MAY_1).await.unwrap();
    let ChunkOutcome::Done(item) = drive.upload_chunk(&session.url, 0, 5, b"hello".to_vec()).await.unwrap() else {
        panic!("one fragment is the whole file")
    };
    assert_eq!(item.id, "N");
    let sent = up.received_requests().await.unwrap();
    assert_eq!(sent[0].body, b"hello");
    assert!(sent[0].headers.get("authorization").is_none(), "the upload URL never gets the token");
}

#[tokio::test]
async fn a_changed_file_opens_its_session_by_id_guarded_by_the_etag() {
    let graph = MockServer::start().await;
    Mock::given(method("POST")).and(path("/me/drive/items/I/createUploadSession")).and(header("if-match", "e1"))
        .and(body_json(json!({"item": {
            "@microsoft.graph.conflictBehavior": "replace",
            "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadUrl": format!("{}/up/s2", graph.uri()), "expirationDateTime": MAY_2_TEXT
        })))
        .mount(&graph).await;
    let target = UploadTarget::Existing { id: "I", if_match: "e1" };
    let session = client(&graph).create_upload_session(target, MAY_1).await.unwrap();
    assert_eq!(session.expires, Some(MAY_1 + 86_400));
    assert!(session.url.ends_with("/up/s2"));
    assert!(!format!("{session:?}").contains("/up/"), "an upload URL is a credential: never in a log");
}

#[tokio::test]
async fn an_empty_new_file_is_a_guarded_put_then_a_patch_for_its_time() {
    let graph = MockServer::start().await;
    Mock::given(method("PUT")).and(path("/me/drive/items/P:/empty.txt:/content"))
        .and(query_param("@microsoft.graph.conflictBehavior", "fail"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "E", "size": 0, "eTag": "e1"})))
        .mount(&graph).await;
    Mock::given(method("PATCH")).and(path("/me/drive/items/E")).and(header("if-match", "e1"))
        .and(body_json(json!({"fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "E", "size": 0, "eTag": "e2"})))
        .mount(&graph).await;
    let target = UploadTarget::New { parent_id: "P", name: "empty.txt" };
    let item = client(&graph).upload_empty(target, MAY_1).await.unwrap();
    assert_eq!(item.e_tag.as_deref(), Some("e2"), "the answer after the time was set");
    let put = &graph.received_requests().await.unwrap()[0];
    let lengths: Vec<_> = put.headers.get_all("content-length").iter().collect();
    assert_eq!(lengths, ["0"], "Graph needs the length of an empty body, once");
}

#[tokio::test]
async fn a_big_file_goes_in_fragments_and_the_last_answers_the_item() {
    let up = MockServer::start().await;
    let total = FRAGMENT_UNIT + 10;
    Mock::given(method("PUT")).and(path("/up/s3"))
        .and(header("content-range", format!("bytes 0-{}/{total}", FRAGMENT_UNIT - 1).as_str()))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "expirationDateTime": MAY_2_TEXT, "nextExpectedRanges": [format!("{FRAGMENT_UNIT}-")]
        })))
        .mount(&up).await;
    Mock::given(method("PUT")).and(path("/up/s3"))
        .and(header("content-range", format!("bytes {FRAGMENT_UNIT}-{}/{total}", total - 1).as_str()))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "B", "size": total})))
        .mount(&up).await;
    let drive = client(&up);
    let url = format!("{}/up/s3", up.uri());
    let first = drive.upload_chunk(&url, 0, total, vec![1; FRAGMENT_UNIT as usize]).await.unwrap();
    assert_eq!(first, ChunkOutcome::More(SessionProgress { next: FRAGMENT_UNIT, expires: Some(MAY_1 + 86_400) }));
    let ChunkOutcome::Done(item) = drive.upload_chunk(&url, FRAGMENT_UNIT, total, vec![2; 10]).await.unwrap() else {
        panic!("the last fragment answers the item")
    };
    assert_eq!(item.size, Some(total));
    let misaligned = drive.upload_chunk(&url, 0, total, vec![0; 1000]).await.unwrap_err();
    assert!(matches!(misaligned, WriteError::Failed(_)), "{misaligned:?}");
    assert_eq!(up.received_requests().await.unwrap().len(), 2, "a misaligned fragment is never sent");
}

/// After an interruption the session's status says where to go on; a
/// fragment it already has (`416`) is answered the same way.
#[tokio::test]
async fn an_interrupted_upload_resumes_from_the_next_expected_range() {
    let up = MockServer::start().await;
    Mock::given(method("GET")).and(path("/up/s4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "expirationDateTime": MAY_2_TEXT, "nextExpectedRanges": ["655360-"]
        })))
        .mount(&up).await;
    Mock::given(method("PUT")).and(path("/up/s4")).respond_with(ResponseTemplate::new(416)).mount(&up).await;
    let drive = client(&up);
    let url = format!("{}/up/s4", up.uri());
    let resumed = SessionProgress { next: 655_360, expires: Some(MAY_1 + 86_400) };
    assert_eq!(drive.upload_status(&url).await.unwrap(), resumed);
    let again = drive.upload_chunk(&url, 0, 1_000_000, vec![0; FRAGMENT_UNIT as usize]).await.unwrap();
    assert_eq!(again, ChunkOutcome::More(resumed));
}

#[tokio::test]
async fn an_ended_session_is_session_gone_and_cancelling_it_is_done() {
    let up = MockServer::start().await;
    Mock::given(method("PUT")).and(path("/up/s5")).respond_with(ResponseTemplate::new(404)).mount(&up).await;
    Mock::given(method("DELETE")).and(path("/up/s5")).respond_with(ResponseTemplate::new(404)).mount(&up).await;
    let drive = client(&up);
    let url = format!("{}/up/s5", up.uri());
    let err = drive.upload_chunk(&url, 0, 10, vec![0; 10]).await.unwrap_err();
    assert!(matches!(err, WriteError::SessionGone), "{err:?}");
    drive.cancel_upload(&url).await.unwrap();
}

/// A fragment refused `429` goes again to the same session once
/// the session says it still expects it; refused every time, the refusal
/// comes back after the policy's attempts and the session is left open.
#[tokio::test]
async fn a_throttled_fragment_goes_again_to_the_same_session() {
    let up = MockServer::start().await;
    Mock::given(method("PUT")).and(path("/up/s6"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&up).await;
    Mock::given(method("PUT")).and(path("/up/s6"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "T", "size": 5})))
        .mount(&up).await;
    Mock::given(method("GET")).and(path("/up/s6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"nextExpectedRanges": ["0-"]})))
        .mount(&up).await;
    let drive = client(&up).with_retry(RetryPolicy { attempts: 3, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(20) });
    let url = format!("{}/up/s6", up.uri());
    let ChunkOutcome::Done(item) = drive.upload_chunk(&url, 0, 5, b"hello".to_vec()).await.unwrap() else {
        panic!("the fragment went in the second time")
    };
    assert_eq!(item.id, "T");
    let sent: Vec<String> = up.received_requests().await.unwrap().iter().map(|r| r.method.to_string()).collect();
    assert_eq!(sent, ["PUT", "GET", "PUT"], "asked where the session stands, then sent again");

    let refusing = MockServer::start().await;
    Mock::given(method("PUT")).respond_with(ResponseTemplate::new(503)).mount(&refusing).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"nextExpectedRanges": ["0-"]})))
        .mount(&refusing).await;
    let drive = client(&refusing).with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(20) });
    let err = drive.upload_chunk(&format!("{}/up/s7", refusing.uri()), 0, 5, b"hello".to_vec()).await.unwrap_err();
    assert!(matches!(err, WriteError::Throttled { .. }), "{err:?}");
    let methods: Vec<String> = refusing.received_requests().await.unwrap().iter().map(|r| r.method.to_string()).collect();
    assert_eq!(methods, ["PUT", "GET", "PUT"], "two sends in all, and no cancel");
}

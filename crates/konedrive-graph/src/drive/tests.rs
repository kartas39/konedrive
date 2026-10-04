use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncReadExt;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::token::{AuthError, StaticToken};

fn client(server: &MockServer) -> DriveClient {
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    DriveClient::new(base, Arc::new(StaticToken::new("T")))
        .unwrap()
        .with_retry(RetryPolicy { attempts: 3, default_wait: Duration::from_millis(10), max_wait: Duration::from_millis(50) })
}

async fn read_all(download: Download) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stream = download.stream;
    stream.read_to_end(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn delta_follows_next_links_to_the_delta_link() {
    let server = MockServer::start().await;
    let next = format!("{}/me/drive/root/delta?token=p2", server.uri());
    let done = format!("{}/me/drive/root/delta?token=d1", server.uri());
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{"id": "B", "name": "b.txt", "file": {}, "parentReference": {"id": "R"}}],
            "@odata.deltaLink": done
        })))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(header("authorization", "Bearer T"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{"id": "R", "root": {}, "folder": {}}, {"id": "A", "name": "a", "folder": {}, "parentReference": {"id": "R"}}],
            "@odata.nextLink": next
        })))
        .mount(&server).await;
    let drive = client(&server);
    let first = drive.delta(&DeltaFrom::Start).await.unwrap();
    assert_eq!(first.items.len(), 2);
    let DeltaNext::Page(link) = first.next else { panic!("expected a next link") };
    let second = drive.delta(&DeltaFrom::Link(link)).await.unwrap();
    assert_eq!(second.items[0].id, "B");
    assert_eq!(second.next, DeltaNext::Done(done));
}

#[tokio::test]
async fn a_link_to_another_host_is_refused_without_sending_the_token() {
    let server = MockServer::start().await;
    let drive = client(&server);
    let err = drive.delta(&DeltaFrom::Link("https://example.com/steal".into())).await.unwrap_err();
    assert!(matches!(err, DriveError::Failed(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn gone_means_the_feed_must_be_listed_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({"error": {"code": "resyncRequired"}})))
        .mount(&server).await;
    assert!(matches!(client(&server).delta(&DeltaFrom::Start).await, Err(DriveError::ResyncRequired)));
}

#[tokio::test]
async fn throttling_waits_as_told_and_tries_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1).with_priority(1)
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
        .with_priority(2)
        .mount(&server).await;
    let drive = client(&server);
    let before = drive.pool().size();
    assert_eq!(drive.drive_id().await.unwrap(), "D1");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(drive.pool().size(), before / 2, "a throttle on any request halves the account's pool");
}

#[tokio::test]
async fn throttling_that_never_ends_is_a_transient_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server).await;
    assert!(matches!(client(&server).drive_id().await, Err(DriveError::Transient(_))));
    assert_eq!(server.received_requests().await.unwrap().len(), 3, "RetryPolicy.attempts");
}

/// Hands out "T1" until invalidated, then "T2".
struct Rotating(AtomicBool);

#[async_trait::async_trait]
impl TokenSource for Rotating {
    async fn access_token(&self) -> Result<String, AuthError> {
        Ok(if self.0.load(Ordering::SeqCst) { "T2".into() } else { "T1".into() })
    }
    async fn invalidate(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_refused_token_is_dropped_and_the_call_made_once_more() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive")).and(header("authorization", "Bearer T1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/me/drive")).and(header("authorization", "Bearer T2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
        .mount(&server).await;
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    let drive = DriveClient::new(base, Arc::new(Rotating(AtomicBool::new(false)))).unwrap();
    assert_eq!(drive.drive_id().await.unwrap(), "D1");
}

#[tokio::test]
async fn signed_out_is_reported_as_such() {
    struct Out;
    #[async_trait::async_trait]
    impl TokenSource for Out {
        async fn access_token(&self) -> Result<String, AuthError> {
            Err(AuthError::SignedOut)
        }
        async fn invalidate(&self) {}
    }
    let server = MockServer::start().await;
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    let drive = DriveClient::new(base, Arc::new(Out)).unwrap();
    assert!(matches!(drive.drive_id().await, Err(DriveError::SignedOut)));
}

#[tokio::test]
async fn an_item_id_is_one_path_segment_whatever_it_contains() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/items/ABC!123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "ABC!123", "name": "x.bin", "size": 5, "cTag": "c1", "file": {"hashes": {"quickXorHash": "AAAAAAAAAAAAAAAAAAAAAAAAAAA="}},
            "@microsoft.graph.downloadUrl": "https://dl.example/x"
        })))
        .mount(&server).await;
    let item = client(&server).item("ABC!123").await.unwrap();
    assert_eq!(item.download_url.as_deref(), Some("https://dl.example/x"));
    assert_eq!(item.c_tag.as_deref(), Some("c1"));
}

#[tokio::test]
async fn a_child_is_found_by_its_name_in_its_folder() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/items/P!1:/100%25%20%D1%84.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "C", "name": "100% ф.txt"})))
        .mount(&server).await;
    assert_eq!(client(&server).child("P!1", "100% ф.txt").await.unwrap().id, "C");
}

#[tokio::test]
async fn a_missing_item_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/items/X"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server).await;
    assert!(matches!(client(&server).item("X").await, Err(DriveError::NotFound)));
}

#[tokio::test]
async fn a_partial_answer_starts_where_its_content_range_says() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/dl")).and(header("range", "bytes=4-"))
        .respond_with(ResponseTemplate::new(206).insert_header("content-range", "bytes 4-9/10").set_body_bytes(b"456789".to_vec()))
        .mount(&server).await;
    let download = client(&server).download(&format!("{}/dl", server.uri()), 4, None).await.unwrap();
    assert_eq!(download.served_from, 4);
    assert_eq!(read_all(download).await, b"456789");
    let sent = &server.received_requests().await.unwrap()[0];
    assert!(sent.headers.get("authorization").is_none(), "a pre-authenticated URL gets no token");
}

/// A piece of a download in parts asks for its own range, and gets no
/// more than that even from a server that ignores it.
#[tokio::test]
async fn a_bounded_range_names_its_last_byte_and_reads_no_further() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/dl")).and(header("range", "bytes=4-6"))
        .respond_with(ResponseTemplate::new(206).insert_header("content-range", "bytes 4-6/10").set_body_bytes(b"456".to_vec()))
        .mount(&server).await;
    let download = client(&server).download(&format!("{}/dl", server.uri()), 4, Some(7)).await.unwrap();
    assert_eq!((download.served_from, read_all(download).await), (4, b"456".to_vec()));
    server.reset().await;
    Mock::given(method("GET")).and(path("/dl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"0123456789".to_vec()))
        .mount(&server).await;
    let download = client(&server).download(&format!("{}/dl", server.uri()), 4, Some(7)).await.unwrap();
    assert_eq!(read_all(download).await, b"456", "a whole body is cut to the range asked for");
}

#[tokio::test]
async fn a_whole_body_answering_a_range_is_skipped_to_the_offset() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/dl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"0123456789".to_vec()))
        .mount(&server).await;
    let download = client(&server).download(&format!("{}/dl", server.uri()), 4, None).await.unwrap();
    assert_eq!(download.served_from, 4);
    assert_eq!(read_all(download).await, b"456789");
}

#[tokio::test]
async fn a_range_past_the_end_is_an_empty_stream_at_that_offset() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/dl"))
        .respond_with(ResponseTemplate::new(416))
        .mount(&server).await;
    let download = client(&server).download(&format!("{}/dl", server.uri()), 10, None).await.unwrap();
    assert_eq!(download.served_from, 10);
    assert!(read_all(download).await.is_empty());
}

#[tokio::test]
async fn a_refused_download_url_has_expired() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/dl"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server).await;
    assert!(matches!(client(&server).download(&format!("{}/dl", server.uri()), 0, None).await, Err(DriveError::UrlExpired)));
}

#[tokio::test]
async fn content_answers_with_where_the_bytes_are() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/items/X/content"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "https://dl.example/x"))
        .mount(&server).await;
    assert_eq!(client(&server).content_url("X").await.unwrap(), "https://dl.example/x");
}

/// Graph's thumbnail redirects to a CDN host; the bearer
/// token must never reach it, the same guarantee `download` gives a
/// pre-authenticated URL.
#[tokio::test]
async fn a_thumbnail_redirect_is_followed_without_the_bearer_token() {
    let server = MockServer::start().await;
    let cdn = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/blob", cdn.uri())))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/blob"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"jpeg-bytes".to_vec()))
        .mount(&cdn).await;
    let bytes = client(&server).thumbnail("P", "c512x512").await.unwrap();
    assert_eq!(bytes, Thumbnail::Image(b"jpeg-bytes".to_vec()));
    let received = cdn.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].headers.get("authorization").is_none(), "the token must not reach the CDN");
}

/// A body over the cap is treated like a 404 — recorded,
/// never retried — rather than downloaded in full or returned as an
/// error.
#[tokio::test]
async fn a_thumbnail_over_the_size_cap_comes_back_as_none() {
    let server = MockServer::start().await;
    let oversized = vec![0u8; MAX_THUMBNAIL_BYTES as usize + 1];
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
        .mount(&server).await;
    assert_eq!(client(&server).thumbnail("P", "c512x512").await.unwrap(), Thumbnail::None);
}

/// A pre-authenticated URL on a listener that takes each connection
/// and drops it at once: a network error, with a secret in the query.
async fn dropping_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/blob?tempauth=SECRET", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    url
}

fn assert_no_url(message: &str, url: &str) {
    let host = Url::parse(url).unwrap().host_str().unwrap().to_owned();
    for part in ["tempauth", "SECRET", "/blob", host.as_str()] {
        assert!(!message.contains(part), "{part:?} in {message:?}");
    }
}

/// Issue #80: a network error on the thumbnail's redirect names no URL,
/// so a pre-authenticated one never reaches the journal.
#[tokio::test]
async fn a_network_error_on_a_thumbnail_redirect_carries_no_url() {
    let server = MockServer::start().await;
    let url = dropping_url().await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", url.as_str()))
        .mount(&server).await;
    let err = client(&server).thumbnail("P", "c512x512").await.unwrap_err();
    assert!(matches!(err, DriveError::Transient(_)), "{err:?}");
    assert_no_url(&err.to_string(), &url);
    assert_no_url(&format!("{err:?}"), &url);
}

/// Issue #80: the same for a download's pre-authenticated URL.
#[tokio::test]
async fn a_network_error_on_a_download_carries_no_url() {
    let server = MockServer::start().await;
    let url = dropping_url().await;
    let Err(err) = client(&server).download(&url, 0, None).await else { panic!("a dropped connection downloaded") };
    assert!(matches!(err, DriveError::Transient(_)), "{err:?}");
    assert_no_url(&err.to_string(), &url);
}

#[tokio::test]
async fn the_drive_id_comes_from_me_drive() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&calls);
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(move |_: &Request| {
            seen.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({"id": "D9", "quota": {"used": 1, "total": 2}}))
        })
        .mount(&server).await;
    assert_eq!(client(&server).drive_id().await.unwrap(), "D9");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_quota_is_graphs_remaining_and_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D9", "quota": {"used": 90, "total": 100, "remaining": 4, "state": "critical", "deleted": 6}})))
        .mount(&server).await;
    let quota = client(&server).quota().await.unwrap();
    assert_eq!(quota, DriveQuota { total: 100, used: 90, remaining: Some(4), state: "critical".into() });
}

/// What a read is refused with says which answer it was: the status, and Graph's code.
#[tokio::test]
async fn a_refused_read_carries_the_status_and_graphs_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/items/X"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {"code": "invalidRequest", "message": "no"}})))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/me/drive/items/Y")).respond_with(ResponseTemplate::new(502)).mount(&server).await;
    let refused = client(&server).item("X").await.unwrap_err();
    assert!(matches!(refused, DriveError::Failed(_)), "{refused:?}");
    assert_eq!(refused.to_string(), "Graph returned 400 Bad Request");
    assert_eq!((refused.status(), refused.code()), (Some(Status::new(400)), Some("invalidRequest")));
    let failing = client(&server).item("Y").await.unwrap_err();
    assert!(matches!(failing, DriveError::Transient(_)), "{failing:?}");
    assert_eq!((failing.status(), failing.code()), (Some(Status::new(502)), None));
}

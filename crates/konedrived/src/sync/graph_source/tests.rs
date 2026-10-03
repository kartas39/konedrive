use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::Duration;

use konedrive_fs::placeholder::{read_ctag, read_state, State};
use serde_json::json;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use konedrive_graph::drive::RetryPolicy;
use konedrive_graph::quickxor::QuickXor;
use crate::sync::source::hydrate;
use konedrive_graph::token::StaticToken;

fn data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

fn quickxor(data: &[u8]) -> String {
    let mut h = QuickXor::new();
    h.update(data);
    h.finish_base64()
}

fn source(server: &MockServer) -> GraphSource {
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    let drive = konedrive_graph::drive::DriveClient::new(base, Arc::new(StaticToken::new("T")))
        .unwrap()
        .with_retry(RetryPolicy { attempts: 3, default_wait: Duration::from_millis(10), max_wait: Duration::from_millis(20) });
    GraphSource::new(drive)
}

/// Metadata for item `I`: `size`, cTag `c1`, the hash of `content`, and a
/// download URL on the mock server unless `url` is `None`.
async fn mock_item(server: &MockServer, content: &[u8], url: Option<&str>) {
    let mut body = json!({
        "id": "I", "name": "f.bin", "size": content.len(), "cTag": "c1",
        "file": {"hashes": {"quickXorHash": quickxor(content)}},
        "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
    });
    if let Some(url) = url {
        body["@microsoft.graph.downloadUrl"] = json!(format!("{}{url}", server.uri()));
    }
    Mock::given(method("GET")).and(path("/me/drive/items/I"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server).await;
}

fn placeholder(dir: &std::path::Path, size: u64) -> std::fs::File {
    let handle = std::fs::File::open(dir).unwrap();
    konedrive_fs::placeholder::create_placeholder(&handle, "f.bin", "I", size, std::time::SystemTime::UNIX_EPOCH).unwrap();
    std::fs::File::options().read(true).write(true).open(dir.join("f.bin")).unwrap()
}

async fn fill(file: &std::fs::File, source: &GraphSource) -> i32 {
    hydrate(file.as_fd().try_clone_to_owned().unwrap(), source).await
}

#[tokio::test]
async fn a_placeholder_is_filled_from_graph_and_verified() {
    let server = MockServer::start().await;
    let content = data(300_000);
    mock_item(&server, &content, Some("/dl/1")).await;
    Mock::given(method("GET")).and(path("/dl/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let file = placeholder(dir.path(), content.len() as u64);
    assert_eq!(fill(&file, &source(&server)).await, 0);
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), content);
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
    let mtime = std::fs::metadata(dir.path().join("f.bin")).unwrap().modified().unwrap();
    assert_eq!(mtime, std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_714_557_600));
}

#[tokio::test]
async fn a_short_answer_is_resumed_with_a_range() {
    let server = MockServer::start().await;
    let content = data(300_000);
    mock_item(&server, &content, Some("/dl/1")).await;
    Mock::given(method("GET")).and(path("/dl/1")).and(header("range", "bytes=100000-"))
        .respond_with(ResponseTemplate::new(206)
            .insert_header("content-range", "bytes 100000-299999/300000")
            .set_body_bytes(content[100_000..].to_vec()))
        .with_priority(1)
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/dl/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content[..100_000].to_vec()))
        .with_priority(2)
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let file = placeholder(dir.path(), content.len() as u64);
    assert_eq!(fill(&file, &source(&server)).await, 0);
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), content);
}

#[tokio::test]
async fn an_expired_download_link_succeeds_with_retry_in_fetch() {
    let server = MockServer::start().await;
    let content = data(50_000);
    mock_item(&server, &content, Some("/dl/1")).await;
    Mock::given(method("GET")).and(path("/dl/1"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1).with_priority(1)
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/dl/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .with_priority(2)
        .mount(&server).await;
    let src = source(&server);
    // A single fetch() call must succeed: only the internal retry (not the
    // outer hydrate loop) can recover from an expired URL in one go.
    let fetched = src.fetch("I", 0, None).await.unwrap();
    assert_eq!(fetched.size, content.len() as u64);
    let items = server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/items/I").count();
    assert_eq!(items, 2, "metadata was asked again for a fresh link");
}

#[tokio::test]
async fn an_expired_link_recovered_through_the_outer_retry_loop() {
    let server = MockServer::start().await;
    let content = data(50_000);
    mock_item(&server, &content, Some("/dl/1")).await;
    Mock::given(method("GET")).and(path("/dl/1"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1).with_priority(1)
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/dl/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .with_priority(2)
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let file = placeholder(dir.path(), content.len() as u64);
    assert_eq!(fill(&file, &source(&server)).await, 0);
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), content);
}

#[tokio::test]
async fn without_a_download_url_the_content_redirect_is_used() {
    let server = MockServer::start().await;
    let content = data(50_000);
    mock_item(&server, &content, None).await;
    Mock::given(method("GET")).and(path("/me/drive/items/I/content"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/dl/2", server.uri()).as_str()))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/dl/2"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let file = placeholder(dir.path(), content.len() as u64);
    assert_eq!(fill(&file, &source(&server)).await, 0);
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), content);
}

#[tokio::test]
async fn an_item_gone_from_onedrive_fails_at_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive/items/I"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let file = placeholder(dir.path(), 10);
    assert_eq!(fill(&file, &source(&server)).await, libc::EIO);
    assert_eq!(server.received_requests().await.unwrap().len(), 1, "no retries for a missing item");
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
}

#[tokio::test]
async fn content_that_does_not_match_its_hash_never_becomes_hydrated() {
    let server = MockServer::start().await;
    let content = data(50_000);
    mock_item(&server, &content, Some("/dl/1")).await;
    let mut damaged = content.clone();
    damaged[123] ^= 1;
    Mock::given(method("GET")).and(path("/dl/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(damaged))
        .mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let file = placeholder(dir.path(), content.len() as u64);
    assert_eq!(fill(&file, &source(&server)).await, libc::EIO);
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
}

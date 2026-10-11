use std::sync::Arc;

use konedrive_graph::token::StaticToken;
use serde_json::json;
use tokio::io::AsyncReadExt;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// A Graph that answers the metadata of the item `A1` with `item` and its bytes with
/// `bytes`, and the source over it.
async fn serve(mut item: serde_json::Value, bytes: &[u8]) -> (MockServer, GraphSource) {
    let server = MockServer::start().await;
    item["id"] = json!("A1");
    item["@microsoft.graph.downloadUrl"] = json!(format!("{}/bytes-of-a1", server.uri()));
    Mock::given(method("GET")).and(path_regex("/items/A1$")).respond_with(ResponseTemplate::new(200).set_body_json(item)).mount(&server).await;
    Mock::given(method("GET")).and(path("/bytes-of-a1")).respond_with(ResponseTemplate::new(200).set_body_bytes(bytes)).mount(&server).await;
    let base = url::Url::parse(&format!("{}/", server.uri())).unwrap();
    let drive = DriveClient::new(base, Arc::new(StaticToken::new("T"))).unwrap();
    (server, GraphSource::new(drive))
}

async fn read_whole(fetched: Fetched) -> Vec<u8> {
    let (mut stream, mut bytes) = (fetched.stream, Vec::new());
    stream.read_to_end(&mut bytes).await.unwrap();
    bytes
}

#[tokio::test]
async fn the_size_of_an_answer_is_the_one_onedrive_said_and_none_when_it_said_none() {
    // The metadata says a size: that one.
    let (_server, source) = serve(json!({"file": {}, "size": 5, "cTag": "c1"}), b"bytes").await;
    let fetched = source.fetch("A1", 0, None).await.ok().expect("the item is answered");
    assert_eq!(fetched.size, Some(5));
    assert_eq!(read_whole(fetched).await, b"bytes");

    // An empty file: OneDrive says 0, which is a size.
    let (_server, source) = serve(json!({"file": {}, "size": 0, "cTag": "c2"}), b"").await;
    assert_eq!(source.fetch("A1", 0, None).await.ok().expect("the item is answered").size, Some(0));

    // The metadata says none: not said, which is not an empty file.
    let (_server, source) = serve(json!({"file": {}, "cTag": "c1"}), b"bytes").await;
    assert_eq!(source.fetch("A1", 0, None).await.ok().expect("the item is answered").size, None, "a size OneDrive did not say");

    // A range a program reads tells nothing of the item, whatever the metadata said.
    let (_server, source) = serve(json!({"file": {}, "size": 5, "cTag": "c1"}), b"bytes").await;
    assert_eq!(source.read_range("A1", 0, 5).await.ok().expect("the range is answered").size, None);
}

use std::sync::Arc;

use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::token::StaticToken;

const OPEN: &str = r#"0{"sid":"s1","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#;

/// A local Engine.IO v4 server for one connection: it records the request's path and
/// query, sends `script` in order, and forwards everything the client sends.
struct Server {
    url: Url,
    uri: tokio::sync::oneshot::Receiver<String>,
    received: mpsc::UnboundedReceiver<String>,
    /// Frames to send after the script, while the connection lasts.
    send: mpsc::UnboundedSender<Message>,
}

// tungstenite's handshake callback returns its large error type by design.
#[allow(clippy::result_large_err)]
async fn server(namespace_and_query: &str, script: Vec<Message>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}{namespace_and_query}", listener.local_addr().unwrap())).unwrap();
    let (uri_tx, uri) = tokio::sync::oneshot::channel();
    let (received_tx, received) = mpsc::unbounded_channel();
    let (send, mut to_send) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let callback = move |request: &Request, response: Response| {
            let _ = uri_tx.send(request.uri().to_string());
            Ok(response)
        };
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback).await.unwrap();
        for message in script {
            if ws.send(message).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                message = ws.next() => match message {
                    Some(Ok(Message::Text(text))) => { let _ = received_tx.send(text.to_string()); }
                    Some(Ok(_)) => {}
                    _ => return,
                },
                message = to_send.recv() => match message {
                    Some(message) => { if ws.send(message).await.is_err() { return; } }
                    None => return,
                },
            }
        }
    });
    Server { url, uri, received, send }
}

async fn next_sent(server: &mut Server) -> String {
    tokio::time::timeout(Duration::from_secs(5), server.received.recv()).await.unwrap().unwrap()
}

#[test]
fn the_notification_url_becomes_the_socket_io_websocket_url() {
    let url = Url::parse("https://f3hb0mpua.svc.ms/notifications?token=abc&applicationId=x").unwrap();
    let (target, namespace) = websocket_target(&url).unwrap();
    assert_eq!(target, "wss://f3hb0mpua.svc.ms/socket.io/?EIO=4&transport=websocket&token=abc&applicationId=x");
    assert_eq!(namespace, "/notifications");

    let url = Url::parse("http://127.0.0.1:8080/?t=1").unwrap();
    assert_eq!(websocket_target(&url).unwrap(), ("ws://127.0.0.1:8080/socket.io/?EIO=4&transport=websocket&t=1".into(), "/".into()));
    let url = Url::parse("https://host.example/a/b/").unwrap();
    assert_eq!(websocket_target(&url).unwrap(), ("wss://host.example/socket.io/?EIO=4&transport=websocket".into(), "/a/b".into()));
    assert!(matches!(websocket_target(&Url::parse("ftp://host/x").unwrap()), Err(SocketEnd::BadUrl(_))));
}

#[tokio::test]
async fn it_joins_both_namespaces_and_reports_a_notification() {
    let mut server = server(
        "/notifications?token=secret",
        vec![
            Message::text(OPEN),
            Message::text(r#"40{"sid":"a"}"#),
            Message::text(r#"40/notifications,{"sid":"b"}"#),
            Message::text("6"),
            Message::text(r#"42/notifications,["hello",{}]"#),
            Message::text(r#"42/other,["notification","{}"]"#),
            Message::text(r#"43/notifications,1[]"#),
            Message::text(r#"42/notifications,["notification","{\"clientState\":null}"]"#),
        ],
    )
    .await;
    let mut socket = NotificationSocket::connect(&server.url).await.unwrap();
    let uri = (&mut server.uri).await.unwrap();
    assert_eq!(uri, "/socket.io/?EIO=4&transport=websocket&token=secret");
    assert_eq!(socket.ping_interval(), Duration::from_millis(25_000));
    assert_eq!(socket.ping_timeout(), Duration::from_millis(20_000));
    assert_eq!(next_sent(&mut server).await, "40");
    assert_eq!(next_sent(&mut server).await, "40/notifications");

    tokio::time::timeout(Duration::from_secs(5), socket.notification()).await.unwrap().unwrap();
    // Nothing else was waiting: the ignored packets gave no notification.
    assert!(tokio::time::timeout(Duration::from_millis(200), socket.notification()).await.is_err());

    // An event in the default namespace counts too, with an ack id.
    server.send.send(Message::text(r#"427["notification",{}]"#)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), socket.notification()).await.unwrap().unwrap();
    socket.close().await;
}

#[tokio::test]
async fn pings_are_answered_and_silence_ends_the_connection() {
    let mut server = server(
        "/notifications",
        vec![Message::text(r#"0{"sid":"s","upgrades":[],"pingInterval":300,"pingTimeout":200}"#), Message::text("2")],
    )
    .await;
    let mut socket = NotificationSocket::connect(&server.url).await.unwrap();
    let started = Instant::now();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        let pinger = server.send.clone();
        let (end, ()) = tokio::join!(socket.notification(), async {
            // Pings keep it alive past one quiet window.
            tokio::time::sleep(Duration::from_millis(300)).await;
            pinger.send(Message::text("2")).unwrap();
        });
        end
    })
    .await
    .unwrap();
    assert_eq!(end, Err(SocketEnd::Dead(Duration::from_millis(500))));
    assert!(started.elapsed() >= Duration::from_millis(750), "{:?}", started.elapsed());
    let mut sent = Vec::new();
    while let Ok(text) = server.received.try_recv() {
        sent.push(text);
    }
    assert_eq!(sent, ["40", "40/notifications", "3", "3"]);
}

#[tokio::test]
async fn a_close_ends_the_connection_with_its_reason() {
    let server = server("/notifications", vec![Message::text(OPEN)]).await;
    let mut socket = NotificationSocket::connect(&server.url).await.unwrap();
    server
        .send
        .send(Message::Close(Some(tungstenite::protocol::CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::Away,
            reason: "going away".into(),
        })))
        .unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), socket.notification()).await.unwrap();
    assert_eq!(end, Err(SocketEnd::Closed("going away (1001)".into())));

    let server2 = self::server("/notifications", vec![Message::text(OPEN), Message::text("1")]).await;
    let mut socket = NotificationSocket::connect(&server2.url).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), socket.notification()).await.unwrap();
    assert!(matches!(end, Err(SocketEnd::Closed(_))), "{end:?}");
}

#[tokio::test]
async fn a_malformed_frame_ends_the_connection() {
    for bad in [Message::text("x"), Message::text(r#"42/notifications,[notification"#), Message::binary(vec![1, 2])] {
        let server = server("/notifications", vec![Message::text(OPEN), bad]).await;
        let mut socket = NotificationSocket::connect(&server.url).await.unwrap();
        let end = tokio::time::timeout(Duration::from_secs(5), socket.notification()).await.unwrap();
        assert!(matches!(end, Err(SocketEnd::Malformed(_))), "{end:?}");
    }
    // A server that does not open the Engine.IO session is refused at once.
    let server = server("/notifications", vec![Message::text("40")]).await;
    let err = NotificationSocket::connect(&server.url).await.err().unwrap();
    assert!(matches!(err, SocketEnd::Malformed(_)), "{err:?}");
}

#[tokio::test]
async fn a_refused_namespace_ends_the_connection() {
    let server = server("/notifications", vec![Message::text(OPEN), Message::text(r#"44/notifications,{"message":"no"}"#)]).await;
    let mut socket = NotificationSocket::connect(&server.url).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), socket.notification()).await.unwrap();
    assert!(matches!(end, Err(SocketEnd::Refused(_))), "{end:?}");
}

#[tokio::test]
async fn no_server_is_a_connect_failure_without_the_query() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = Url::parse(&format!("http://{addr}/notifications?token=secret")).unwrap();
    let err = NotificationSocket::connect(&url).await.err().unwrap();
    assert!(matches!(err, SocketEnd::Connect(_)), "{err:?}");
    assert!(!err.to_string().contains("secret"), "{err}");
}

fn client(server: &MockServer) -> DriveClient {
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    DriveClient::new(base, Arc::new(StaticToken::new("T"))).unwrap()
}

#[tokio::test]
async fn the_endpoint_carries_its_expiry_when_graph_gives_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/subscriptions/socketIo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "opaque",
            "notificationUrl": "https://f3hb0mpua.svc.ms/notifications?token=secret",
            "expirationDateTime": "2026-10-01T12:00:00.1234567Z"
        })))
        .mount(&server)
        .await;
    let endpoint = client(&server).socket_endpoint().await.unwrap();
    assert_eq!(endpoint.host(), "f3hb0mpua.svc.ms");
    assert!(endpoint.expiry_from_service);
    let expires = UNIX_EPOCH + Duration::from_secs(parse_graph_time("2026-10-01T12:00:00Z").unwrap() as u64);
    assert_eq!(endpoint.expires_at, expires);
    assert_eq!(endpoint.renew_after(expires - Duration::from_secs(600)), Duration::from_secs(480));
    assert_eq!(endpoint.renew_after(expires), Duration::ZERO);
    assert!(!format!("{endpoint:?}").contains("secret"));
}

#[tokio::test]
async fn an_endpoint_without_expiry_is_taken_to_live_an_hour() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/subscriptions/socketIo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "opaque",
            "notificationUrl": "https://f3hb0mpua.svc.ms/notifications?token=secret"
        })))
        .mount(&server)
        .await;
    let before = SystemTime::now();
    let endpoint = client(&server).socket_endpoint().await.unwrap();
    assert!(!endpoint.expiry_from_service);
    assert!(endpoint.expires_at >= before + DEFAULT_LIFETIME);
    assert!(endpoint.expires_at <= SystemTime::now() + DEFAULT_LIFETIME);
}

mod common;

use std::future::Future;
use std::time::Duration;

use common::*;
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::{Account1Proxy, INTERFACE_NAME, OBJECT_PATH, SERVICE_NAME};

const XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Account1.xml"));

struct Setup {
    proxy: Account1Proxy<'static>,
    client: zbus::Connection,
    _server: zbus::Connection,
    fixture: Fixture,
    _bus: TestBus,
}

async fn setup(sign_in_timeout: Duration) -> Setup {
    let bus = TestBus::start();
    let fixture = Fixture::new(sign_in_timeout).await;
    let server = konedrived::dbus::serve(bus.builder(), fixture.svc.clone(), None).await.unwrap();
    fixture.svc.startup().await;
    let client = bus.connect().await;
    let proxy = Account1Proxy::new(&client).await.unwrap();
    Setup { proxy, client, _server: server, fixture, _bus: bus }
}

/// Polls `check` (proxy properties are cached and updated by PropertiesChanged).
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..500 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exposes_initial_properties() {
    let s = setup(Duration::from_secs(5)).await;
    assert_eq!(s.proxy.state().await.unwrap(), "signed-out");
    assert_eq!(s.proxy.client_id().await.unwrap(), "");
    assert_eq!(s.proxy.last_error().await.unwrap(), "");
    assert_eq!(s.proxy.quota_total().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_client_id_validates_and_notifies() {
    let s = setup(Duration::from_secs(5)).await;
    let err = s.proxy.set_client_id("not-a-guid").await.unwrap_err();
    assert!(
        matches!(&err, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs"),
        "{err:?}"
    );
    assert_eq!(s.proxy.client_id().await.unwrap(), "");
    s.proxy.set_client_id(CLIENT_ID).await.unwrap();
    let proxy = &s.proxy;
    eventually("ClientId", || async move { proxy.client_id().await.unwrap() == CLIENT_ID }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sign_in_and_out_over_dbus() {
    let s = setup(Duration::from_secs(10)).await;
    let proxy = &s.proxy;
    proxy.set_client_id(CLIENT_ID).await.unwrap();
    let url = proxy.begin_sign_in().await.unwrap();
    eventually("signing-in", || async move { proxy.state().await.unwrap() == "signing-in" }).await;

    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    eventually("quota", || async move { proxy.quota_total().await.unwrap() == 5368709120 }).await;
    assert_eq!(proxy.state().await.unwrap(), "signed-in");
    assert_eq!(proxy.display_name().await.unwrap(), "Test User");
    assert_eq!(proxy.email().await.unwrap(), "test@outlook.com");
    assert_eq!(proxy.quota_used().await.unwrap(), 1073741824);

    proxy.sign_out().await.unwrap();
    eventually("signed-out", || async move { proxy.state().await.unwrap() == "signed-out" }).await;
    assert_eq!(proxy.display_name().await.unwrap(), "");
    assert_eq!(s.fixture.store.current(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn introspection_matches_the_checked_in_xml() {
    let s = setup(Duration::from_secs(5)).await;
    let introspectable = zbus::fdo::IntrospectableProxy::builder(&s.client)
        .destination(SERVICE_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let live = introspectable.introspect().await.unwrap();
    assert_eq!(signature_lines(&live, INTERFACE_NAME), signature_lines(XML, INTERFACE_NAME));
}

/// Normalizes one interface to sorted lines such as `method SetClientId in=s out=`
/// and `property State s read`. Argument names are ignored.
fn signature_lines(xml: &str, interface: &str) -> Vec<String> {
    let start = xml
        .find(&format!("<interface name=\"{interface}\""))
        .unwrap_or_else(|| panic!("interface {interface} missing in:\n{xml}"));
    let end = start + xml[start..].find("</interface>").expect("unterminated interface");
    let mut lines = Vec::new();
    let mut method: Option<(String, String, String)> = None;
    let flush = |method: &mut Option<(String, String, String)>, lines: &mut Vec<String>| {
        if let Some((name, input, output)) = method.take() {
            lines.push(format!("method {name} in={input} out={output}"));
        }
    };
    for raw in xml[start..end].split('<').skip(1) {
        let tag = raw.split('>').next().unwrap_or_default().trim();
        let attr = |key: &str| -> String {
            let pattern = format!("{key}=\"");
            tag.find(&pattern)
                .map(|i| {
                    let rest = &tag[i + pattern.len()..];
                    rest[..rest.find('"').unwrap()].to_owned()
                })
                .unwrap_or_default()
        };
        if tag.starts_with("method ") {
            flush(&mut method, &mut lines);
            method = Some((attr("name"), String::new(), String::new()));
        } else if tag.starts_with("arg ") {
            if let Some((_, input, output)) = method.as_mut() {
                if attr("direction") == "out" {
                    output.push_str(&attr("type"));
                } else {
                    input.push_str(&attr("type"));
                }
            }
        } else if tag.starts_with("/method") {
            flush(&mut method, &mut lines);
        } else if tag.starts_with("property ") {
            flush(&mut method, &mut lines);
            lines.push(format!("property {} {} {}", attr("name"), attr("type"), attr("access")));
        }
    }
    flush(&mut method, &mut lines);
    lines.sort();
    lines
}

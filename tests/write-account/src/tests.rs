//! The guards, each shown to refuse (`docs/design/writes.md` §12): against wiremock standing in for Graph,
//! and on the guard itself. Nothing here reaches Microsoft.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use konedrived::config::Config;
use konedrived::drive::{DriveClient, UploadTarget};
use konedrived::token::StaticToken;
use reqwest::Method;
use serde_json::{json, Value};
use url::Url;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::args::{read_config, read_token, Args};
use crate::guard::{Caps, Forward, Guard, Request, Target, CLEANUP_RESERVE, MIB, TOP};
use crate::harness::{self, Ended, Options};
use crate::{checks, proxy};

const DRIVE: &str = "TEST-DRIVE";

fn options(server: &MockServer, allowed: &[&str]) -> Options {
    let config = Config { write_test_drive_ids: allowed.iter().map(|id| id.to_string()).collect(), ..Config::default() };
    Options {
        upstream: Url::parse(&format!("{}/", server.uri())).unwrap(),
        token: "RW".into(),
        read_only_token: "RO".into(),
        test_drive: DRIVE.into(),
        config,
        caps: Caps::default(),
        run_id: "run-1".into(),
        delta_wait: Duration::from_millis(10),
    }
}

/// `GET /me/drive` for `token`: drive `id` with `used` bytes in use.
async fn me(server: &MockServer, token: &str, id: &str, used: Option<u64>) {
    let quota = used.map_or(json!({}), |used| json!({ "used": used, "total": 5u64 << 30 }));
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": id, "quota": quota })))
        .mount(server)
        .await;
}

/// The drive listed in two pages, `first` and `second` items long.
async fn listing(server: &MockServer, first: usize, second: usize) {
    let items = |from: usize, n: usize| -> Vec<Value> {
        (from..from + n).map(|i| json!({ "id": format!("I{i}"), "name": format!("f{i}"), "file": {}, "parentReference": { "id": "ROOT" } })).collect()
    };
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": items(0, first),
            "@odata.nextLink": format!("{}/me/drive/root/delta?page=2", server.uri()),
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": items(first, second),
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=end", server.uri()),
        })))
        .mount(server)
        .await;
}

async fn root(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/me/drive/items/root"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "ROOT", "root": {}, "folder": {} })))
        .mount(server)
        .await;
}

async fn refused(options: Options) -> String {
    match harness::run(options).await {
        Ended::Refused(why) => why,
        Ended::Ran(report) => panic!("the run was not refused: {report:?}"),
    }
}

/// How many requests reached the server, each checked to be a read.
async fn reads_only(server: &MockServer) -> usize {
    let requests = server.received_requests().await.unwrap();
    for request in &requests {
        assert_eq!(request.method.as_str(), "GET", "{} {} reached the server", request.method, request.url);
    }
    requests.len()
}

// Guard 1.

#[tokio::test]
async fn a_drive_not_on_the_allow_list_is_refused_before_any_request() {
    let server = MockServer::start().await;
    let why = refused(options(&server, &["ANOTHER-DRIVE"])).await;
    assert!(why.contains("write_test_drive_ids"), "{why}");
    let why = refused(options(&server, &[])).await;
    assert!(why.contains("write_test_drive_ids"), "the default, an empty list: {why}");
    assert_eq!(reads_only(&server).await, 0);
}

#[tokio::test]
async fn a_token_that_reaches_another_drive_is_refused() {
    let server = MockServer::start().await;
    me(&server, "RW", "THE-REAL-ONE", Some(1000)).await;
    me(&server, "RO", DRIVE, Some(1000)).await;
    let why = refused(options(&server, &[DRIVE])).await;
    assert!(why.contains("THE-REAL-ONE"), "{why}");
    assert_eq!(reads_only(&server).await, 1);
}

#[tokio::test]
async fn a_read_only_token_that_reaches_another_drive_is_refused() {
    let server = MockServer::start().await;
    me(&server, "RW", DRIVE, Some(1000)).await;
    me(&server, "RO", "THE-REAL-ONE", Some(1000)).await;
    let why = refused(options(&server, &[DRIVE])).await;
    assert!(why.contains("read-only token") && why.contains("THE-REAL-ONE"), "{why}");
    assert_eq!(reads_only(&server).await, 2);
}

#[tokio::test]
async fn the_same_token_twice_is_refused_before_any_request() {
    let server = MockServer::start().await;
    let mut options = options(&server, &[DRIVE]);
    options.read_only_token = options.token.clone();
    let why = refused(options).await;
    assert!(why.contains("read-only token is the read-write one"), "{why}");
    assert_eq!(reads_only(&server).await, 0);
}

// Guard 2.

#[tokio::test]
async fn a_drive_with_a_gigabyte_in_use_is_refused() {
    let server = MockServer::start().await;
    me(&server, "RW", DRIVE, Some(1 << 30)).await;
    let why = refused(options(&server, &[DRIVE])).await;
    assert!(why.contains("in use"), "{why}");
    assert_eq!(reads_only(&server).await, 1);
}

#[tokio::test]
async fn a_drive_whose_use_cannot_be_read_is_refused() {
    let server = MockServer::start().await;
    me(&server, "RW", DRIVE, None).await;
    let why = refused(options(&server, &[DRIVE])).await;
    assert!(why.contains("quota.used"), "{why}");
    assert_eq!(reads_only(&server).await, 1);
}

#[tokio::test]
async fn a_drive_with_a_thousand_items_is_refused() {
    let server = MockServer::start().await;
    me(&server, "RW", DRIVE, Some(1000)).await;
    me(&server, "RO", DRIVE, Some(1000)).await;
    listing(&server, 600, 400).await;
    let why = refused(options(&server, &[DRIVE])).await;
    assert!(why.contains("1000"), "{why}");
    // The second page was asked for through the proxy, the link pointed at it.
    assert_eq!(reads_only(&server).await, 4);
}

/// The positive control for guards 1 and 2: a test account passes, and the first writes are
/// the ones that make the run folder.
#[tokio::test]
async fn a_test_account_passes_the_preflight_and_first_makes_its_folder() {
    let server = MockServer::start().await;
    me(&server, "RW", DRIVE, Some(1000)).await;
    me(&server, "RO", DRIVE, Some(1000)).await;
    listing(&server, 600, 399).await;
    root(&server).await;
    // No answer for the folder: OneDrive's 404 ends the run there.
    let Ended::Ran(report) = harness::run(options(&server, &[DRIVE])).await else { panic!("refused") };
    assert_eq!(report.checks.len(), 1, "{report:?}");
    assert!(report.failed() && report.refused.is_none(), "{report:?}");
    let writes: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() != "GET")
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect();
    assert_eq!(writes, ["POST /me/drive/items/ROOT/children"]);
}

// Guards 3 and 4, on the guard itself.

fn graph<'a>(method: &'a Method, rel: &'a str, query: Option<&'a str>, body: &'a [u8]) -> Request<'a> {
    Request { method, target: Target::Graph { rel, query }, body, content_range: None }
}

fn fragment<'a>(key: &'a str, range: &'a str, body: &'a [u8]) -> Request<'a> {
    Request { method: &Method::PUT, target: Target::Upload { key }, body, content_range: Some(range) }
}

fn folder_body(name: &str, behaviour: &str) -> String {
    json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": behaviour }).to_string()
}

/// A guard past the preflight, whose run folder `RUN` holds the file `IN`; `OUT` is at the top
/// of the drive.
fn armed(caps: Caps) -> Guard {
    let guard = Guard::new(caps, "run-1".into());
    guard.learn(&Method::GET, Some("me/drive/items/root"), 200, &json!({ "id": "ROOT", "root": {}, "folder": {} }));
    guard.learn(&Method::GET, Some("me/drive/items/OUT"), 200, &json!({ "id": "OUT", "file": {}, "parentReference": { "id": "ROOT" } }));
    guard.arm();
    let top = folder_body(TOP, "fail");
    assert_eq!(guard.admit(&graph(&Method::POST, "me/drive/items/ROOT/children", None, top.as_bytes())), Ok(Forward::Graph));
    let answer = json!({ "id": "TOP", "name": TOP, "folder": {}, "parentReference": { "id": "ROOT" } });
    guard.learn(&Method::POST, Some("me/drive/items/ROOT/children"), 201, &answer);
    let run = folder_body("run-1", "fail");
    assert_eq!(guard.admit(&graph(&Method::POST, "me/drive/items/TOP/children", None, run.as_bytes())), Ok(Forward::Graph));
    let answer = json!({ "id": "RUN", "name": "run-1", "folder": {}, "parentReference": { "id": "TOP" } });
    guard.learn(&Method::POST, Some("me/drive/items/TOP/children"), 201, &answer);
    guard.learn(&Method::PUT, None, 201, &json!({ "id": "IN", "name": "a.txt", "file": {}, "parentReference": { "id": "RUN" } }));
    assert_eq!(guard.run_folder().as_deref(), Some("RUN"));
    assert!(guard.is_inside("IN") && !guard.is_inside("OUT") && !guard.is_inside("TOP"));
    guard
}

#[test]
fn every_write_outside_the_run_folder_is_refused() {
    let copy = Method::from_bytes(b"COPY").unwrap();
    let session = |size: u64| json!({ "item": { "@microsoft.graph.conflictBehavior": "fail", "fileSize": size } }).to_string();
    let fail = Some("@microsoft.graph.conflictBehavior=fail");
    let cases: Vec<(&Method, &str, Option<&str>, String)> = vec![
        (&Method::PATCH, "me/drive/items/IN", None, json!({ "parentReference": { "id": "ROOT" } }).to_string()),
        (&Method::PATCH, "me/drive/items/IN", None, json!({ "parentReference": { "id": "TOP" } }).to_string()),
        (&Method::PATCH, "me/drive/items/IN", None, json!({ "parentReference": { "path": "/drive/root:" } }).to_string()),
        (&Method::PATCH, "me/drive/items/IN", None, json!({ "name": "../escape" }).to_string()),
        (&Method::PATCH, "me/drive/items/OUT", None, json!({ "name": "y" }).to_string()),
        (&Method::PATCH, "me/drive/items/RUN", None, json!({ "name": "renamed" }).to_string()),
        (&Method::DELETE, "me/drive/items/OUT", None, String::new()),
        (&Method::DELETE, "me/drive/items/TOP", None, String::new()),
        (&Method::DELETE, "me/drive/items/RUN", None, String::new()),
        (&Method::POST, "me/drive/items/OUT/children", None, folder_body("x", "fail")),
        (&Method::POST, "me/drive/items/ROOT/children", None, folder_body(TOP, "fail")),
        (&Method::POST, "me/drive/items/TOP/children", None, folder_body("run-2", "fail")),
        (&Method::POST, "me/drive/items/RUN/children", None, folder_body("sub", "replace")),
        (&Method::POST, "me/drive/items/OUT:/x.bin:/createUploadSession", None, session(1)),
        (&Method::POST, "me/drive/items/OUT/createUploadSession", None, json!({ "item": { "fileSize": 1 } }).to_string()),
        (&Method::POST, "me/drive/items/RUN:/..:/createUploadSession", None, session(1)),
        (&Method::POST, "me/drive/items/RUN:/a%2Fb:/createUploadSession", None, session(1)),
        (&Method::POST, "me/drive/items/RUN:/x.bin:/createUploadSession", None, json!({ "item": { "fileSize": 1 } }).to_string()),
        (&Method::PUT, "me/drive/items/OUT:/x.txt:/content", fail, String::new()),
        (&Method::PUT, "me/drive/items/RUN:/x.txt:/content", None, String::new()),
        (&Method::PUT, "me/drive/items/OUT/content", None, String::new()),
        (&Method::POST, "me/drive/items/OUT/restore", None, "{}".into()),
        (&Method::POST, "me/drive/items/IN/restore", None, json!({ "parentReference": { "id": "ROOT" } }).to_string()),
        (&Method::POST, "me/drive/root:/konedrive-write-test/run-1:/children", None, folder_body("x", "fail")),
        (&Method::POST, "me/drive/items/IN/copy", None, json!({ "parentReference": { "id": "RUN" } }).to_string()),
        (&copy, "me/drive/items/IN", None, String::new()),
    ];
    for (method, rel, query, body) in cases {
        let guard = armed(Caps::default());
        let what = format!("{method} {rel} {body}");
        assert!(guard.admit(&graph(method, rel, query, body.as_bytes())).is_err(), "admitted: {what}");
        assert!(guard.refused().is_some(), "{what}");
        // A refusal is final: nothing else goes out, not even a write inside the run folder.
        let inside = folder_body("sub", "fail");
        assert!(guard.admit(&graph(&Method::POST, "me/drive/items/RUN/children", None, inside.as_bytes())).is_err(), "{what}");
    }
}

/// The positive control: what the checks send inside the run folder goes through.
#[test]
fn what_stays_inside_the_run_folder_is_admitted() {
    let guard = armed(Caps::default());
    let session = json!({ "item": {
        "@microsoft.graph.conflictBehavior": "fail", "name": "new.bin",
        "fileSystemInfo": { "lastModifiedDateTime": "2023-11-14T22:13:20Z" }, "fileSize": 64 * MIB,
    } })
    .to_string();
    let replace = json!({ "item": { "@microsoft.graph.conflictBehavior": "replace", "fileSize": 10 } }).to_string();
    let change = json!({ "name": "b.txt", "parentReference": { "id": "RUN" }, "fileSystemInfo": {} }).to_string();
    let cases: Vec<(&Method, &str, Option<&str>, String)> = vec![
        (&Method::GET, "me/drive/items/OUT", None, String::new()),
        (&Method::POST, "me/drive/items/RUN/children", None, folder_body("sub", "fail")),
        (&Method::POST, "me/drive/items/RUN:/new.bin:/createUploadSession", None, session),
        (&Method::POST, "me/drive/items/IN/createUploadSession", None, replace),
        (&Method::PUT, "me/drive/items/RUN:/empty.txt:/content", Some("@microsoft.graph.conflictBehavior=fail"), String::new()),
        (&Method::PUT, "me/drive/items/IN/content", None, String::new()),
        (&Method::PATCH, "me/drive/items/IN", None, change),
        (&Method::POST, "me/drive/items/IN/restore", None, "{}".into()),
        (&Method::DELETE, "me/drive/items/IN", None, String::new()),
    ];
    for (method, rel, query, body) in cases {
        assert_eq!(guard.admit(&graph(method, rel, query, body.as_bytes())), Ok(Forward::Graph), "{method} {rel}");
    }
    assert_eq!(guard.refused(), None);
}

#[test]
fn nothing_but_reads_before_the_preflight_passes() {
    let guard = Guard::new(Caps::default(), "run-1".into());
    assert_eq!(guard.admit(&graph(&Method::GET, "me/drive", None, b"")), Ok(Forward::Graph));
    let top = folder_body(TOP, "fail");
    assert!(guard.admit(&graph(&Method::POST, "me/drive/items/root/children", None, top.as_bytes())).is_err());
}

#[test]
fn the_caps_are_the_designs() {
    assert_eq!(Caps::default(), Caps { per_file: 64 * MIB, per_run: 200 * MIB, requests: 500 });
}

#[test]
fn a_file_over_the_cap_is_refused() {
    let guard = armed(Caps::default());
    let body = json!({ "item": { "@microsoft.graph.conflictBehavior": "fail", "fileSize": 64 * MIB + 1 } }).to_string();
    let request = graph(&Method::POST, "me/drive/items/RUN:/big.bin:/createUploadSession", None, body.as_bytes());
    assert!(guard.admit(&request).unwrap_err().contains("per file"));
}

#[test]
fn the_run_stops_at_its_byte_cap() {
    let guard = armed(Caps { per_file: 10, per_run: 25, requests: 500 });
    let content = [7u8; 10];
    for (n, url) in ["https://up.example/1", "https://up.example/2"].into_iter().enumerate() {
        let key = guard.open_session(url.into(), 10);
        assert_eq!(guard.admit(&fragment(&key, "bytes 0-9/10", &content)), Ok(Forward::Upload(url.into())), "session {n}");
    }
    let key = guard.open_session("https://up.example/3".into(), 10);
    assert!(guard.admit(&fragment(&key, "bytes 0-9/10", &content)).unwrap_err().contains("cap of 25"));
    assert_eq!(guard.usage().1, 20);
    // A fragment that does not fit its session, and a URL the run was not given.
    let guard = armed(Caps::default());
    let key = guard.open_session("https://up.example/1".into(), 10);
    assert!(guard.admit(&fragment(&key, "bytes 0-9/11", &content)).is_err());
    let guard = armed(Caps::default());
    assert!(guard.admit(&fragment("s9", "bytes 0-9/10", &content)).is_err());
}

#[test]
fn the_run_stops_at_its_request_cap_and_keeps_some_for_the_cleanup() {
    let guard = Guard::new(Caps { requests: CLEANUP_RESERVE + 3, ..Caps::default() }, "run-1".into());
    for _ in 0..3 {
        assert_eq!(guard.admit(&graph(&Method::GET, "me/drive", None, b"")), Ok(Forward::Graph));
    }
    assert!(guard.admit(&graph(&Method::GET, "me/drive", None, b"")).unwrap_err().contains("used up"));
    guard.begin_cleanup();
    assert_eq!(guard.admit(&graph(&Method::GET, "me/drive", None, b"")), Ok(Forward::Graph));
}

#[test]
fn the_cleanup_deletes_the_run_folder_and_nothing_else() {
    let guard = armed(Caps::default());
    guard.begin_cleanup();
    assert!(guard.admit(&graph(&Method::DELETE, "me/drive/items/IN", None, b"")).is_err());
    assert!(guard.admit(&graph(&Method::PATCH, "me/drive/items/IN", None, br#"{"name":"x"}"#)).is_err());
    assert_eq!(guard.admit(&graph(&Method::DELETE, "me/drive/items/RUN", None, b"")), Ok(Forward::Graph));
}

// Guards 3 and 4 at the wire: konedrive's own client, through the proxy, against wiremock.

async fn bootstrapped(server: &MockServer) -> (Arc<Guard>, proxy::Proxy, DriveClient) {
    root(server).await;
    let folder = |id: &str, name: &str, parent: &str| {
        ResponseTemplate::new(201).set_body_json(json!({ "id": id, "name": name, "folder": {}, "parentReference": { "id": parent } }))
    };
    Mock::given(method("POST")).and(path("/me/drive/items/ROOT/children")).respond_with(folder("TOP", TOP, "ROOT")).mount(server).await;
    Mock::given(method("GET"))
        .and(path(format!("/me/drive/items/ROOT:/{TOP}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "TOP", "name": TOP, "folder": {}, "parentReference": { "id": "ROOT" } })))
        .mount(server)
        .await;
    Mock::given(method("POST")).and(path("/me/drive/items/TOP/children")).respond_with(folder("RUN", "run-1", "TOP")).mount(server).await;
    let guard = Arc::new(Guard::new(Caps::default(), "run-1".into()));
    let upstream = Url::parse(&format!("{}/", server.uri())).unwrap();
    let proxy = proxy::start(upstream, Arc::clone(&guard)).await.unwrap();
    let drive = DriveClient::new(proxy.base.clone(), Arc::new(StaticToken::new("RW"))).unwrap();
    assert_eq!(drive.item("root").await.unwrap().id, "ROOT");
    guard.arm();
    assert_eq!(checks::bootstrap(&drive, &guard, "ROOT", "run-1").await, Ok("RUN".to_owned()));
    (guard, proxy, drive)
}

fn writes(requests: &[wiremock::Request]) -> Vec<String> {
    requests.iter().filter(|r| r.method.as_str() != "GET").map(|r| format!("{} {}", r.method, r.url.path())).collect()
}

#[tokio::test]
async fn nothing_is_written_through_the_proxy_before_the_preflight_passes() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    let guard = Arc::new(Guard::new(Caps::default(), "run-1".into()));
    let proxy = proxy::start(Url::parse(&format!("{}/", server.uri())).unwrap(), Arc::clone(&guard)).await.unwrap();
    let drive = DriveClient::new(proxy.base.clone(), Arc::new(StaticToken::new("RW"))).unwrap();
    assert!(drive.create_folder("root", TOP).await.is_err());
    assert!(guard.refused().is_some());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_write_outside_the_run_folder_never_reaches_onedrive() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    let (guard, _proxy, drive) = bootstrapped(&server).await;
    assert!(drive.delete_item("OUTSIDE", "e1").await.is_err());
    assert!(guard.refused().unwrap().contains("OUTSIDE"));
    assert!(drive.create_folder("RUN", "fine").await.is_err(), "nothing more goes out after a refusal");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(writes(&requests), ["POST /me/drive/items/ROOT/children", "POST /me/drive/items/TOP/children"]);
}

#[tokio::test]
async fn an_upload_goes_through_the_guard_and_never_carries_the_token() {
    let server = MockServer::start().await;
    let session = json!({ "uploadUrl": format!("{}/session/1", server.uri()), "expirationDateTime": "2030-01-01T00:00:00Z" });
    Mock::given(method("POST"))
        .and(path("/me/drive/items/RUN:/small.bin:/createUploadSession"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
        .mount(&server)
        .await;
    let item = json!({ "id": "S", "name": "small.bin", "size": 100, "file": {}, "parentReference": { "id": "RUN" } });
    Mock::given(method("PUT"))
        .and(path("/session/1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(item))
        .mount(&server)
        .await;
    let (guard, _proxy, drive) = bootstrapped(&server).await;
    let target = UploadTarget::New { parent_id: "RUN", name: "small.bin" };
    assert_eq!(drive.upload_small(target, vec![7; 100], 1_700_000_000).await.unwrap().id, "S");
    let requests = server.received_requests().await.unwrap();
    let put = requests.iter().find(|r| r.url.path() == "/session/1").expect("the fragment reached the session");
    assert!(put.headers.get("authorization").is_none(), "the token went to the upload URL");
    assert_eq!(put.headers.get("content-range").unwrap(), "bytes 0-99/100");
    assert_eq!(guard.usage().1, 100);
    assert!(guard.is_inside("S"), "learnt from the session's answer");
    // A file over the cap is refused before its session is asked for.
    let big = UploadTarget::New { parent_id: "RUN", name: "big.bin" };
    assert!(drive.create_upload_session(big, 64 * MIB + 1, 1_700_000_000).await.is_err());
    let requests = server.received_requests().await.unwrap();
    assert!(!writes(&requests).iter().any(|w| w.contains("big.bin")), "{:?}", writes(&requests));
}

// The command line and the files it names.

#[test]
fn every_flag_is_required() {
    let all = [
        ("--graph-test-drive", "D"),
        ("--graph-token", "rw.token"),
        ("--graph-read-only-token", "ro.token"),
        ("--daemon-config", "config.toml"),
    ];
    let argv = |skip: Option<&str>| {
        let mut argv = vec!["konedrive-write-test"];
        for (flag, value) in all.iter().filter(|(flag, _)| Some(*flag) != skip) {
            argv.extend([*flag, *value]);
        }
        argv
    };
    assert!(Args::try_parse_from(argv(None)).is_ok());
    for (flag, _) in all {
        assert!(Args::try_parse_from(argv(Some(flag))).is_err(), "runs without {flag}");
    }
}

#[test]
fn a_token_file_must_be_private_and_hold_one_token() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("rw.token");
    std::fs::write(&file, "TOKEN\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_token(&file).unwrap_err().contains("0600"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_token(&file).unwrap(), "TOKEN");
    let link = dir.path().join("link.token");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert!(read_token(&link).is_err(), "a symlink");
    std::fs::write(&file, "TWO TOKENS").unwrap();
    assert!(read_token(&file).is_err());
    std::fs::write(&file, "").unwrap();
    assert!(read_token(&file).is_err());
}

#[test]
fn the_allow_list_is_the_daemons() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "config_version = 2\nwrite_test_drive_ids = [\"D1\"]\n").unwrap();
    let config = read_config(&file).unwrap();
    assert!(config.writes_allowed("D1") && !config.writes_allowed("D2"));
    std::fs::write(&file, "not toml at all [").unwrap();
    assert!(read_config(&file).is_err());
}

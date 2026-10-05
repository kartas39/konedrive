//! What the tests of `konedrivectl` share: the daemon, started as `konedrived` starts it
//! (`konedrived::daemon::startup::start`), on a private bus, and the binary run against it.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::testing::TestBus;

/// The daemon on `bus`, with `config.toml` and the accounts' files in `dir`, and no account
/// yet. Its folders are local (no drive), its wallet is in memory, it runs no Baloo
/// and fills no thumbnails, and Microsoft is an address where nothing answers: no test
/// signs in for real. Its helper hub looks for the helper at a socket in `dir`, where
/// nothing is bound, from before the daemon starts: a test that wants a helper connects a
/// fake one itself.
pub async fn start_daemon(bus: &TestBus, dir: &Path) -> konedrived::daemon::startup::Daemon {
    start_daemon_showing(bus, dir, konedrived::daemon::manager::no_drive()).await
}

/// [`start_daemon`], whose accounts' folders show what `drive` gives: a mocked Graph.
pub async fn start_daemon_showing(bus: &TestBus, dir: &Path, drive: konedrived::daemon::manager::DriveOf) -> konedrived::daemon::startup::Daemon {
    let nowhere = |path: &str| url::Url::parse(&format!("http://127.0.0.1:9/{path}/")).unwrap();
    let endpoints = konedrive_graph::oauth::Endpoints { authority: nowhere("authority"), graph: nowhere("graph") };
    start_daemon_with(bus, dir, drive, endpoints).await
}

/// [`start_daemon`], with Microsoft played by `server` ([`mock_identity`]): a sign-in can
/// succeed.
pub async fn start_daemon_signing_in(bus: &TestBus, dir: &Path, server: &wiremock::MockServer) -> konedrived::daemon::startup::Daemon {
    let base = url::Url::parse(&format!("{}/", server.uri())).unwrap();
    let endpoints = konedrive_graph::oauth::Endpoints { authority: base.clone(), graph: base };
    start_daemon_with(bus, dir, konedrived::daemon::manager::no_drive(), endpoints).await
}

async fn start_daemon_with(
    bus: &TestBus,
    dir: &Path,
    drive: konedrived::daemon::manager::DriveOf,
    endpoints: konedrive_graph::oauth::Endpoints,
) -> konedrived::daemon::startup::Daemon {
    let options = konedrived::daemon::manager::Options {
        endpoints,
        wallet: Arc::new(konedrived::account::testing::MemoryWallet::default()),
        sign_in_timeout: Duration::from_secs(5),
        baloo: konedrived::desktop::baloo::Baloo::disabled,
        thumbnails: None,
        drive,
        bus: Arc::new(konedrived::dbus::export::OnBus),
    };
    let registry = konedrived::sync::registry::Registry::new();
    registry.hub().set_socket(dir.join("no-helper.sock"));
    let paths = konedrived::config::Paths::in_dir(dir);
    konedrived::daemon::startup::start_on(bus.builder(), paths, options, registry).await.unwrap()
}

/// The `konedrivectl` binary on the bus at `bus_addr`, as a user's shell runs it, with
/// `env` added — and never the `KONEDRIVE_ACCOUNT` of the shell running the tests. It never
/// opens a browser (`KONEDRIVE_NO_BROWSER`).
pub fn run_env(bus_addr: &str, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    command(bus_addr, args, env).output().expect("failed to run the konedrivectl binary")
}

/// [`run_env`]'s command, not started yet.
pub fn command(bus_addr: &str, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_konedrivectl"));
    command
        .args(args)
        .env("DBUS_SESSION_BUS_ADDRESS", bus_addr)
        .env_remove(konedrivectl::ACCOUNT_VARIABLE)
        .env(konedrivectl::NO_BROWSER_VARIABLE, "1");
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

pub fn run(bus_addr: &str, args: &[&str]) -> std::process::Output {
    run_env(bus_addr, args, &[])
}

pub fn out_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn err_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A Microsoft account at `server`: the authorization code `code` signs it in, with `email`
/// and the drive `drive`.
pub async fn mock_identity(server: &wiremock::MockServer, code: &str, token: &str, email: &str, drive: &str) {
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains(format!("code={code}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"token_type": "Bearer", "access_token": token, "expires_in": 3600, "refresh_token": format!("R-{token}")}),
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"displayName": "Somebody", "mail": email})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": drive, "quota": {"used": 1u64, "total": 2u64}})))
        .mount(server)
        .await;
}

/// A command this test started: stopped however the test ends.
pub struct Stopped(pub Option<std::process::Child>);

impl Drop for Stopped {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Runs `args` — a command that prints a sign-in address and waits — and plays the browser:
/// the address's redirect is called with `answer` (`code=…`, `error=…`) and its `state`.
/// The command's output once it has ended. Blocking.
pub fn run_signing_in(bus_addr: &str, args: &[&str], answer: &str) -> std::process::Output {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;
    let mut child = command(bus_addr, args, &[]);
    let child = child.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("failed to run the konedrivectl binary");
    let mut child = Stopped(Some(child));
    let mut stdout = BufReader::new(child.0.as_mut().unwrap().stdout.take().unwrap());
    let mut printed = String::new();
    let mut address = None;
    let mut line = String::new();
    while address.is_none() && stdout.read_line(&mut line).unwrap() > 0 {
        address = line.split_whitespace().find(|word| word.starts_with("http")).map(str::to_owned);
        printed.push_str(&line);
        line.clear();
    }
    if let Some(address) = address {
        let address = url::Url::parse(&address).unwrap();
        let query: std::collections::HashMap<String, String> = address.query_pairs().into_owned().collect();
        let redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
        let mut browser = std::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
        write!(browser, "GET /?{answer}&state={} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", query["state"]).unwrap();
        let mut answered = String::new();
        let _ = browser.read_to_string(&mut answered);
    }
    stdout.read_to_string(&mut printed).unwrap();
    let mut out = child.0.take().unwrap().wait_with_output().unwrap();
    out.stdout = printed.into_bytes();
    out
}

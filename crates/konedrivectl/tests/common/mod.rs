//! What the tests of `konedrivectl` share: the daemon, started as `konedrived` starts it
//! (`konedrived::accounts::start`), on a private bus, and the binary run against it.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::testing::TestBus;

/// The daemon on `bus`, with `config.toml` and the accounts' files in `dir`, and no account
/// yet. Its folders are local (`onedrive: false`), its wallet is in memory, it runs no Baloo
/// and fills no thumbnails, and Microsoft is an address where nothing answers: no test
/// signs in for real. Its helper hub looks for the helper at a socket in `dir`, where
/// nothing is bound, from before the daemon starts: a test that wants a helper connects a
/// fake one itself.
pub async fn start_daemon(bus: &TestBus, dir: &Path) -> konedrived::accounts::Daemon {
    let nowhere = |path: &str| url::Url::parse(&format!("http://127.0.0.1:9/{path}/")).unwrap();
    let options = konedrived::accounts::Options {
        endpoints: konedrived::oauth::Endpoints { authority: nowhere("authority"), graph: nowhere("graph") },
        wallet: Arc::new(konedrived::secret::MemoryWallet::default()),
        sign_in_timeout: Duration::from_secs(5),
        baloo: konedrived::sync::baloo::Baloo::disabled,
        thumbnails: None,
        onedrive: false,
    };
    let hub = konedrived::sync::hub::HelperHub::new();
    hub.set_socket(dir.join("no-helper.sock"));
    let paths = konedrived::config::Paths::in_dir(dir);
    konedrived::accounts::start_on(bus.builder(), paths, options, hub).await.unwrap()
}

/// The `konedrivectl` binary on the bus at `bus_addr`, as a user's shell runs it, with
/// `env` added — and never the `KONEDRIVE_ACCOUNT` of the shell running the tests.
pub fn run_env(bus_addr: &str, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_konedrivectl"));
    command.args(args).env("DBUS_SESSION_BUS_ADDRESS", bus_addr).env_remove(konedrivectl::ACCOUNT_VARIABLE);
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("failed to run the konedrivectl binary")
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

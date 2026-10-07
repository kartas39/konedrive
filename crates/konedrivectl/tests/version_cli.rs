//! `konedrivectl --version`: its own build, then the running daemon's, and a word when they
//! differ. The daemon is the real one (the same build as this binary), a fake that says another
//! version, or none at all.

mod common;

use konedrive_dbus::testing::TestBus;
use konedrive_dbus::version::{line, COMMIT, VERSION};

/// Just enough of `org.konedrive.Accounts` to say which build it is.
struct OtherBuild;

#[zbus::interface(name = "org.konedrive.Accounts")]
impl OtherBuild {
    #[zbus(property)]
    async fn version(&self) -> String {
        "0.0.9-dev.3".to_owned()
    }

    #[zbus(property)]
    async fn commit(&self) -> String {
        "1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b".to_owned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_build_says_nothing_more() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, dir.path()).await;

    let out = common::run(bus.address(), &["--version"]);
    assert!(out.status.success(), "{out:?}");
    let text = common::out_text(&out);
    assert_eq!(text, format!("{}\n{}\n", line("konedrivectl", VERSION, COMMIT), line("konedrived", VERSION, COMMIT)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_build_asks_for_a_restart() {
    let bus = TestBus::start();
    let _fake = bus
        .builder()
        .serve_at(konedrive_dbus::ACCOUNTS_PATH, OtherBuild)
        .unwrap()
        .name(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .build()
        .await
        .unwrap();

    let out = common::run(bus.address(), &["-V"]);
    assert!(out.status.success(), "{out:?}");
    let text = common::out_text(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], line("konedrivectl", VERSION, COMMIT), "{text}");
    assert_eq!(lines[1], "konedrived 0.0.9-dev.3 (commit 1a2b3c4)", "{text}");
    assert!(lines[2].contains("restart it (systemctl --user restart konedrived)"), "{text}");
    assert_eq!(lines.len(), 3, "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_daemon_is_no_failure() {
    let bus = TestBus::start();
    let out = common::run(bus.address(), &["--version"]);
    assert!(out.status.success(), "{out:?}");
    let text = common::out_text(&out);
    assert_eq!(text, format!("{}\nkonedrived: not running (not on the session bus)\n", line("konedrivectl", VERSION, COMMIT)));
}

#[test]
fn a_command_is_still_needed_without_version() {
    let out = common::run("unix:path=/nonexistent", &["--account", "Personal"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(common::err_text(&out).contains("a command is needed"), "{}", common::err_text(&out));
}

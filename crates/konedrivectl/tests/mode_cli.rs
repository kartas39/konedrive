//! `konedrivectl account mode` and `dev export-access-token --read-write` against the daemon
//! over a private bus (`docs/design/writes.md` §11): the mode shown, and the development gate refusing
//! read-write — the switch and the token alike — for an account not in
//! `write_test_drive_ids`, which by default is every account.

mod common;

use common::{err_text, out_text, run};
use konedrive_dbus::testing::TestBus;
use konedrived::config::Mode;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_mode_shows_the_mode_and_the_gate_refuses_read_write() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon(&bus, dir.path()).await;
    let account = daemon.manager.add("Personal", &daemon.connection).await.unwrap();
    let configured = || daemon.manager.config().account(&account.id).unwrap().mode;

    let out = run(bus.address(), &["account", "mode"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out), "read-only\n");

    let out = run(bus.address(), &["account", "mode", "read-write"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let told = err_text(&out);
    assert!(told.contains("Personal was not switched to read-write") && told.contains("write_test_drive_ids"), "{told}");
    assert!(told.contains("Nothing was changed"), "{told}");
    assert!(out_text(&out).is_empty(), "no sign-in page is offered: {out:?}");
    assert_eq!(configured(), Mode::ReadOnly);

    let token = dir.path().join("token");
    let out = run(bus.address(), &["dev", "export-access-token", "--read-write", "--out", token.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(err_text(&out).contains("cannot get a read-write access token") && err_text(&out).contains("write_test_drive_ids"), "{out:?}");
    assert!(!token.exists(), "nothing is written");

    let out = run(bus.address(), &["account", "mode", "read-only"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Read-only"), "{out:?}");
    assert_eq!(configured(), Mode::ReadOnly);

    for wrong in [&["account", "mode", "--force"][..], &["account", "mode", "writable"]] {
        let out = run(bus.address(), wrong);
        assert_eq!(out.status.code(), Some(2), "{wrong:?}: {out:?}");
    }
}

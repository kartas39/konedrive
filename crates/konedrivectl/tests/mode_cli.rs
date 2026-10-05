//! `konedrivectl account mode` and `dev export-access-token --read-write` (a development
//! build's, the `dev-tools` feature; a release has no `dev` command) against the daemon
//! over a private bus (`docs/design/writes.md` §11): the mode shown; read-write, which is the
//! user's choice for any signed-in account, starting its sign-in with `write_test_drive_ids`
//! empty; and the read-write token, which that list alone still refuses.

mod common;

use std::io::{BufRead, BufReader};
use std::process::Stdio;

use common::{err_text, out_text, run};
use konedrive_dbus::testing::TestBus;
use konedrived::account::state::SignInState;
use konedrived::config::{DriveId, Mode};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_mode_shows_the_mode_and_read_write_starts_its_sign_in_for_any_account() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon(&bus, dir.path()).await;
    let account = daemon.manager.add("Personal", &daemon.connection).await.unwrap();
    let configured = || daemon.manager.config().account(&account.id).unwrap().mode;
    assert!(daemon.manager.config().snapshot().write_test_drive_ids.is_empty(), "the list is empty");

    let out = run(bus.address(), &["account", "mode"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out), "read-only\n");

    // Not signed in: that, and nothing about a list, is what refuses the switch.
    let out = run(bus.address(), &["account", "mode", "read-write"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let told = err_text(&out);
    assert!(told.contains("Personal was not switched to read-write: it is not signed in"), "{told}");
    assert!(!told.contains("write_test_drive_ids"), "{told}");
    assert!(out_text(&out).is_empty(), "no sign-in page is offered: {out:?}");
    assert_eq!(configured(), Mode::ReadOnly);

    let token = dir.path().join("token");
    let out = run(bus.address(), &["dev", "export-access-token", "--read-write", "--out", token.to_str().unwrap()]);
    if cfg!(feature = "dev-tools") {
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(err_text(&out).contains("cannot get a read-write access token") && err_text(&out).contains("write_test_drive_ids"), "{out:?}");
    } else {
        // A release has no `dev` command: clap refuses it.
        assert_eq!(out.status.code(), Some(2), "{out:?}");
    }
    assert!(!token.exists(), "nothing is written");

    let out = run(bus.address(), &["account", "mode", "read-only"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Read-only"), "{out:?}");
    assert_eq!(configured(), Mode::ReadOnly);

    for wrong in [&["account", "mode", "--force"][..], &["account", "mode", "writable"]] {
        let out = run(bus.address(), wrong);
        assert_eq!(out.status.code(), Some(2), "{wrong:?}: {out:?}");
    }

    // Signed in (said so here: Microsoft is an address where nothing answers), with the list
    // still empty and the account's drive recorded: the switch starts its sign-in, prints the address, which asks for
    // `Files.ReadWrite`, and waits. Nothing is written before the grant. The account signed
    // out from elsewhere ends the wait.
    let service = std::sync::Arc::clone(&daemon.manager.accounts()[0].account);
    service.state().update(|s| s.state = SignInState::SignedIn);

    // No drive is recorded for the account, and none can be asked for: refused at once, with
    // no sign-in page, in words about the drive.
    let out = run(bus.address(), &["account", "mode", "read-write"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(err_text(&out).contains("drive is not known yet"), "{out:?}");
    assert!(out_text(&out).is_empty(), "no sign-in page is offered: {out:?}");
    assert_eq!(configured(), Mode::ReadOnly);

    // With its drive recorded — one the list does not have — the switch goes on.
    daemon.manager.config().record_drive(&account.id, &DriveId::new("D1").unwrap()).unwrap();
    let mut switch = common::command(bus.address(), &["account", "mode", "read-write"], &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = switch.stdout.take().unwrap();
    let address = tokio::task::spawn_blocking(move || {
        BufReader::new(stdout).lines().map_while(Result::ok).find(|line| line.trim_start().starts_with("http"))
    })
    .await
    .unwrap()
    .expect("the sign-in address is printed");
    let asked = url::Url::parse(address.trim()).unwrap().query_pairs().find(|(name, _)| name == "scope").unwrap().1.into_owned();
    assert!(asked.split(' ').any(|scope| scope == "Files.ReadWrite"), "{asked}");
    assert_eq!(configured(), Mode::ReadOnly, "nothing is written before the grant");
    service.sign_out().await.unwrap();
    let out = tokio::task::spawn_blocking(move || switch.wait_with_output()).await.unwrap().unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(err_text(&out).contains("it stays read-only"), "{out:?}");
    assert_eq!(configured(), Mode::ReadOnly);
}

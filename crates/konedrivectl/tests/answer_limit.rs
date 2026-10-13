//! The answer limit (`konedrivectl::ANSWER_LIMIT`): a command does not wait for ever for a
//! daemon that does not answer, and the calls the daemon answers only once the work is done
//! are waited for past the limit. The daemon is a fake that owns the daemon's name.

mod common;

use std::time::Duration;

use konedrive_dbus::testing::TestBus;
use konedrivectl::text::refusals::unanswered_text;
use konedrivectl::ANSWER_LIMIT;
use zbus::zvariant::OwnedObjectPath;

/// `org.konedrive.Accounts` of a daemon that never answers what it is asked.
struct SilentAccounts;

#[zbus::interface(name = "org.konedrive.Accounts")]
impl SilentAccounts {
    #[zbus(property)]
    async fn list(&self) -> Vec<OwnedObjectPath> {
        std::future::pending().await
    }
}

/// `org.konedrive.Accounts` of a daemon with no account.
struct NoAccounts;

#[zbus::interface(name = "org.konedrive.Accounts")]
impl NoAccounts {
    #[zbus(property)]
    async fn list(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }
}

/// `org.konedrive.Files` whose download takes longer than the answer limit.
struct SlowFiles;

#[zbus::interface(name = "org.konedrive.Files")]
impl SlowFiles {
    async fn hydrate(&self, _path: &str) {
        tokio::time::sleep(ANSWER_LIMIT + Duration::from_secs(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_does_not_answer_fails_the_command_after_the_limit() {
    let bus = TestBus::start();
    let _fake = bus
        .builder()
        .serve_at(konedrive_dbus::ACCOUNTS_PATH, SilentAccounts)
        .unwrap()
        .name(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .build()
        .await
        .unwrap();

    let address = bus.address().to_owned();
    let out = tokio::task::spawn_blocking(move || common::run(&address, &["account", "list"])).await.unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(common::err_text(&out), format!("Error: {}\n", unanswered_text()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_is_waited_for_past_the_limit() {
    let bus = TestBus::start();
    let _fake = bus
        .builder()
        .serve_at(konedrive_dbus::ACCOUNTS_PATH, NoAccounts)
        .unwrap()
        .serve_at(konedrive_dbus::ACCOUNTS_PATH, SlowFiles)
        .unwrap()
        .name(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("report.txt");
    std::fs::write(&file, b"").unwrap();

    let (address, path) = (bus.address().to_owned(), file.to_str().unwrap().to_owned());
    let out = tokio::task::spawn_blocking(move || common::run(&address, &["sync", "hydrate", &path])).await.unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(common::out_text(&out), format!("{}\n", konedrivectl::text::files::DOWNLOADED));
}

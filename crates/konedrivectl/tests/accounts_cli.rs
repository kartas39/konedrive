//! Design test 12: `konedrivectl` with several accounts, against the daemon over a private
//! bus — the `account` commands; an account chosen by id, label or email, with `--account` or
//! `KONEDRIVE_ACCOUNT`, and a command that needs one refused with exit status 2 when there are
//! several and none is chosen; the path commands, routed by `Files1` whichever account holds
//! the path, and refusing `--account`; `status` and `sync status` over every account; and
//! `login` with no account at all, which adds `Personal`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{err_text, out_text, run_env};
use konedrive_dbus::accounts::{Account1Proxy, Accounts1Proxy};
use konedrive_dbus::testing::TestBus;

const CLIENT_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..250 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Runs a command that must fail, and returns its exit status and what it said.
fn failed(bus: &TestBus, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let out = run_env(bus.address(), args, env);
    assert!(!out.status.success(), "{args:?} must fail: {out:?}");
    assert!(out_text(&out).is_empty(), "a refusal prints nothing on stdout: {out:?}");
    (out.status.code().unwrap(), err_text(&out))
}

/// Runs a command that must succeed, and returns what it printed.
fn succeeded(bus: &TestBus, args: &[&str], env: &[(&str, &str)]) -> String {
    let out = run_env(bus.address(), args, env);
    assert!(out.status.success(), "{args:?} must succeed: {out:?}");
    out_text(&out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accounts_are_added_chosen_renamed_and_removed() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon(&bus, dir.path()).await;

    assert!(succeeded(&bus, &["account", "list"], &[]).starts_with("No accounts yet."));
    let (status, told) = failed(&bus, &["logout"], &[]);
    assert_eq!(status, 1, "{told}");
    assert!(told.contains("No account yet: `konedrivectl account add <label>`"), "{told}");
    let (status, told) = failed(&bus, &["login"], &[(konedrivectl::ACCOUNT_VARIABLE, "Test")]);
    assert_eq!(status, 1, "{told}");
    assert!(told.contains("KONEDRIVE_ACCOUNT names the account \"Test\", and there are no accounts yet"), "{told}");

    let added = succeeded(&bus, &["account", "add", "Personal"], &[]);
    let personal = daemon.manager.accounts()[0].id.clone();
    assert!(added.contains(&format!("Added the account Personal ({personal})")), "{added}");
    succeeded(&bus, &["account", "add", "Family"], &[]);
    let (_, told) = failed(&bus, &["account", "add", "family"], &[]);
    assert!(told.contains("cannot be an account's label") && told.contains("already used"), "{told}");
    daemon.manager.accounts()[1].account.state().update(|s| s.email = "family.ann@live.com".into());

    let list = succeeded(&bus, &["account", "list"], &[]);
    let lines: Vec<&str> = list.lines().collect();
    assert_eq!(lines.len(), 3, "{list}");
    assert!(lines[0].starts_with("ID ") && lines[0].ends_with("FOLDER"), "{list}");
    assert!(lines[1].starts_with(&format!("{personal}  Personal")) && lines[1].contains("signed-out"), "{list}");
    assert!(lines[1].contains("read-only") && lines[1].ends_with('\u{2014}'), "{list}");
    assert!(lines[2].contains("Family") && lines[2].contains("family.ann@live.com"), "{list}");

    // Several accounts and none chosen: exit status 2, naming them.
    let (status, told) = failed(&bus, &["logout"], &[]);
    assert_eq!(status, 2, "{told}");
    assert!(told.contains("Several accounts: choose one with --account (Personal, Family)"), "{told}");

    // By id, by label or email in any case, and from the environment; --account first.
    let label_of = |text: String| text.lines().next().unwrap_or_default().to_owned();
    assert_eq!(label_of(succeeded(&bus, &["--account", &personal, "status"], &[])), "Label:      Personal");
    assert_eq!(label_of(succeeded(&bus, &["status", "--account", "FAMILY"], &[])), "Label:      Family");
    assert_eq!(label_of(succeeded(&bus, &["--account", "Family.Ann@LIVE.com", "status"], &[])), "Label:      Family");
    let env = [(konedrivectl::ACCOUNT_VARIABLE, "family")];
    assert_eq!(label_of(succeeded(&bus, &["status"], &env)), "Label:      Family");
    assert_eq!(label_of(succeeded(&bus, &["--account", "personal", "status"], &env)), "Label:      Personal");
    let (status, told) = failed(&bus, &["--account", "nobody", "logout"], &[]);
    assert_eq!(status, 2, "{told}");
    assert!(told.contains("there is no account \"nobody\"") && told.contains("Personal, Family"), "{told}");
    assert!(succeeded(&bus, &["--account", "family", "logout"], &[]).contains("Signed out of Family."));

    assert_eq!(succeeded(&bus, &["account", "rename", "family", "Home"], &[]).trim(), "Renamed Family to Home.");
    let (_, told) = failed(&bus, &["account", "rename", "home", "a@b"], &[]);
    assert!(told.contains("\"a@b\" cannot be an account's label"), "{told}");
    let (status, told) = failed(&bus, &["--account", "home", "account", "remove", "home"], &[]);
    assert_eq!(status, 2, "{told}");
    assert!(told.contains("leave out --account"), "{told}");

    let removed = succeeded(&bus, &["account", "remove", "home"], &[]);
    assert!(removed.starts_with("Removed the account Home:") && removed.contains("It had no folder."), "{removed}");
    let list = succeeded(&bus, &["account", "list"], &[]);
    assert!(!list.contains("Home") && list.contains("Personal"), "{list}");
    // One account again: nothing to choose.
    assert!(succeeded(&bus, &["logout"], &[]).contains("Signed out of Personal."));
}

/// `root` registered without interception for `account`, and filled from a directory of
/// its own holding `names`, each with the account's label as its content.
fn local_folder(bus: &TestBus, dir: &Path, account: &str, names: &[&str]) -> PathBuf {
    let root = dir.join(account);
    let source = dir.join(format!("{account}-source"));
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&source).unwrap();
    for name in names {
        std::fs::write(source.join(name), account.as_bytes()).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let args = ["--account", account, "sync", "register-without-interception", root.to_str().unwrap()];
    let said = succeeded(bus, &args, &[]);
    // Several accounts: the success line says whose folder it is.
    assert!(said.starts_with(&format!("{account}: Folder registered without interception: ")), "{said}");
    succeeded(bus, &["--account", account, "sync", "populate-from", source.to_str().unwrap()], &[]);
    root
}

fn state_of(bus: &TestBus, path: &Path) -> String {
    succeeded(bus, &["sync", "state", path.to_str().unwrap()], &[]).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_commands_go_by_the_path_and_status_shows_every_account() {
    let bus = TestBus::start();
    let config = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, config.path()).await;
    let dir = tempfile::tempdir().unwrap();
    succeeded(&bus, &["account", "add", "Personal"], &[]);
    succeeded(&bus, &["account", "add", "Family"], &[]);
    let personal = local_folder(&bus, dir.path(), "Personal", &["a.txt", "a2.txt"]);
    let family = local_folder(&bus, dir.path(), "Family", &["b.txt", "b2.txt"]);

    // No --account: the path decides, whatever KONEDRIVE_ACCOUNT says.
    let env = [(konedrivectl::ACCOUNT_VARIABLE, "Family")];
    for (file, content) in [(personal.join("a.txt"), "Personal"), (family.join("b.txt"), "Family")] {
        assert_eq!(state_of(&bus, &file), "online-only");
        assert!(succeeded(&bus, &["sync", "hydrate", file.to_str().unwrap()], &env).contains("Downloaded."));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), content, "filled from its own account's source");
        assert_eq!(state_of(&bus, &file), "hydrated");
    }
    let (a2, b2) = (personal.join("a2.txt"), family.join("b2.txt"));
    let pinned = succeeded(&bus, &["sync", "pin", a2.to_str().unwrap(), b2.to_str().unwrap()], &[]);
    // Pins in two accounts: `sync transfers` is one account's, so the hint names none.
    let transfers = "`konedrivectl --account <account> sync transfers`";
    assert_eq!(pinned.trim(), format!("Kept on this device. 2 files are downloading ({transfers})."));
    wait_for("both pinned files", || state_of(&bus, &a2) == "hydrated" && state_of(&bus, &b2) == "hydrated").await;

    let (status, told) = failed(&bus, &["--account", "Personal", "sync", "hydrate", a2.to_str().unwrap()], &[]);
    assert_eq!(status, 2, "{told}");
    assert!(told.contains("the path decides the account: leave out --account"), "{told}");
    let outside = dir.path().join("elsewhere.txt");
    std::fs::write(&outside, b"mine").unwrap();
    let (_, told) = failed(&bus, &["sync", "hydrate", outside.to_str().unwrap()], &[]);
    let both = format!("({}, {})", personal.display(), family.display());
    assert!(told.contains("is not inside any account's sync folder") && told.contains(&both), "{told}");

    // A suggested command names the account it is about: with KONEDRIVE_ACCOUNT pointing
    // elsewhere, a bare `sync forget` would forget the other account's folder.
    let other = dir.path().join("Other");
    std::fs::create_dir(&other).unwrap();
    let args = ["--account", "Family", "sync", "register-without-interception", other.to_str().unwrap()];
    let (_, told) = failed(&bus, &args, &[(konedrivectl::ACCOUNT_VARIABLE, "Personal")]);
    assert!(told.contains("run `konedrivectl --account Family sync forget` first"), "{told}");

    // A command on one account's folder needs the account.
    let (status, told) = failed(&bus, &["sync", "transfers"], &[]);
    assert_eq!(status, 2, "{told}");
    assert!(told.contains("(Personal, Family)"), "{told}");

    // `status` and `sync status` show every account, each under its label.
    let text = succeeded(&bus, &["status"], &[]);
    assert!(text.starts_with("Client ID:  (not set)\n"), "{text}");
    assert!(text.contains("\nPersonal\n  State:      signed-out\n"), "{text}");
    assert!(text.contains("\nFamily\n  State:      signed-out\n"), "{text}");
    let text = succeeded(&bus, &["sync", "status"], &[]);
    assert!(text.starts_with("Helper:"), "{text}");
    assert_eq!(text.matches("Helper:").count(), 1, "one helper for every account: {text}");
    assert!(text.contains(&format!("\nPersonal\n  Folder:                 {}\n", personal.display())), "{text}");
    assert!(text.contains(&format!("\nFamily\n  Folder:                 {}\n", family.display())), "{text}");
    let list = succeeded(&bus, &["account", "list"], &[]);
    assert!(list.contains(&format!("{} (no-interception)", family.display())), "{list}");

    // A third account's folder cannot be inside another's.
    succeeded(&bus, &["account", "add", "Work"], &[]);
    let inside = personal.join("work");
    std::fs::create_dir(&inside).unwrap();
    let args = ["--account", "work", "sync", "register-without-interception", inside.to_str().unwrap()];
    let (_, told) = failed(&bus, &args, &[]);
    assert!(told.contains("cannot be this account's folder") && told.contains("Personal"), "{told}");

    // Removing an account keeps its folder's files, and its paths are no account's any more.
    let removed = succeeded(&bus, &["account", "remove", "personal"], &[]);
    assert!(removed.contains(&format!("Kept: the folder {}", personal.display())), "{removed}");
    assert_eq!(std::fs::read_to_string(personal.join("a.txt")).unwrap(), "Personal");
    assert_eq!(state_of(&bus, &personal.join("a.txt")), "not-managed");
}

/// `login` with no account at all adds `Personal` and signs it in; with no client ID it
/// says so first and adds nothing. The browser is a stand-in `xdg-open` that records the
/// address it is given, and the sign-in is cancelled from the bus, as another client would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_with_no_account_adds_personal() {
    let bus = TestBus::start();
    let config = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, config.path()).await;
    let client = bus.connect().await;
    let manager = Accounts1Proxy::builder(&client)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();

    let (_, told) = failed(&bus, &["login"], &[]);
    assert!(told.contains("konedrivectl set-client-id <id>"), "{told}");
    assert!(manager.accounts().await.unwrap().is_empty(), "nothing is added without a client ID");
    manager.set_client_id(CLIENT_ID).await.unwrap();

    let browser = tempfile::tempdir().unwrap();
    let opened = browser.path().join("opened");
    let script = browser.path().join("xdg-open");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", opened.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let login = std::process::Command::new(env!("CARGO_BIN_EXE_konedrivectl"))
        .arg("login")
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("PATH", browser.path())
        .env_remove(konedrivectl::ACCOUNT_VARIABLE)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut account = None;
    for _ in 0..250 {
        if let Some(path) = manager.accounts().await.unwrap().first() {
            let proxy = Account1Proxy::builder(&client)
                .path(path.clone())
                .unwrap()
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await
                .unwrap();
            if proxy.state().await.unwrap() == "signing-in" {
                account = Some(proxy);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let account = account.expect("login never started signing in");
    assert_eq!(account.label().await.unwrap(), "Personal");
    wait_for("the browser", || opened.exists()).await;
    account.cancel_sign_in().await.unwrap();

    let out = login.wait_with_output().unwrap();
    assert!(!out.status.success(), "a cancelled sign-in fails: {out:?}");
    let said = out_text(&out);
    assert!(said.starts_with("Added an account called Personal"), "{said}");
    let url = std::fs::read_to_string(&opened).unwrap();
    assert!(said.contains(&url) && url.contains(CLIENT_ID), "the address is printed and opened: {said}");
    assert!(err_text(&out).contains("cancelled"), "{}", err_text(&out));
    assert_eq!(manager.accounts().await.unwrap().len(), 1);
}

//! Design test 12: `konedrivectl` with several accounts, against the daemon over a private
//! bus — the `account` commands; an account chosen by id, label or email, with `--account` or
//! `KONEDRIVE_ACCOUNT`, and a command that needs one refused with exit status 2 when there are
//! several and none is chosen; the path commands, routed by `Files` whichever account holds
//! the path, and refusing `--account`; `status` and `sync status` over every account; and
//! `account add`, which signs in and adds the account under its email. No client ID is set:
//! the daemon's built-in one is what `status` shows and what a sign-in address carries.
//! An account that never signs in is added through the daemon's manager.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{err_text, out_text, run_env};
use konedrive_dbus::testing::TestBus;

use konedrived::config::DEFAULT_CLIENT_ID;

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
    assert!(told.contains("No account yet: `konedrivectl account add`"), "{told}");
    let (status, told) = failed(&bus, &["login"], &[(konedrivectl::ACCOUNT_VARIABLE, "Test")]);
    assert_eq!(status, 1, "{told}");
    assert!(told.contains("KONEDRIVE_ACCOUNT names the account \"Test\", and there are no accounts yet"), "{told}");

    // `account add` adds a new account: it takes none to act on.
    let (status, told) = failed(&bus, &["--account", "x", "account", "add"], &[]);
    assert_eq!(status, 2, "{told}");
    assert!(told.contains("`account add` adds a new account: leave out --account"), "{told}");
    assert!(daemon.manager.accounts().is_empty());

    let personal = daemon.manager.add("Personal", &daemon.connection).await.unwrap().id.clone();
    daemon.manager.add("Family", &daemon.connection).await.unwrap();
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
    // A label may be an email; it may not hold a "/".
    assert_eq!(succeeded(&bus, &["account", "rename", "home", "a@b"], &[]).trim(), "Renamed Home to a@b.");
    assert_eq!(succeeded(&bus, &["account", "rename", "A@B", "Home"], &[]).trim(), "Renamed a@b to Home.");
    let (_, told) = failed(&bus, &["account", "rename", "home", "a/b"], &[]);
    assert!(told.contains("\"a/b\" cannot be an account's label"), "{told}");
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
    let daemon = common::start_daemon(&bus, config.path()).await;
    let dir = tempfile::tempdir().unwrap();
    daemon.manager.add("Personal", &daemon.connection).await.unwrap();
    daemon.manager.add("Family", &daemon.connection).await.unwrap();
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
    assert!(text.starts_with(&format!("Client ID:  {DEFAULT_CLIENT_ID}\n")), "{text}");
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
    daemon.manager.add("Work", &daemon.connection).await.unwrap();
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

/// `account add` signs in and adds the account under its email, with no account and with one
/// present; a OneDrive account that is added already is not added again; a sign-in that
/// fails adds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_add_signs_in_and_names_the_account_by_its_email() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let server = wiremock::MockServer::start().await;
    common::mock_identity(&server, "ann-code", "AT1", "ann@outlook.com", "D1").await;
    common::mock_identity(&server, "bob-code", "AT2", "bob@live.com", "D2").await;
    let daemon = common::start_daemon_signing_in(&bus, dir.path(), &server).await;
    let add = |answer: &'static str| {
        let address = bus.address().to_owned();
        async move { tokio::task::spawn_blocking(move || common::run_signing_in(&address, &["account", "add"], answer)).await.unwrap() }
    };
    let labels = || daemon.manager.accounts().iter().map(|a| a.account.state().get().label).collect::<Vec<_>>();

    // A sign-in the browser refuses: no account.
    let out = add("error=access_denied").await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(err_text(&out).contains("the account was not added: Access was denied in the browser."), "{out:?}");
    assert!(labels().is_empty());
    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap_or_default();
    assert!(!config.contains("[[accounts]]"), "a failed sign-in leaves no account: {config}");
    assert!(succeeded(&bus, &["account", "list"], &[]).starts_with("No accounts yet."));

    // With no account: the address is printed, with the built-in client ID, and the
    // account is named by its email.
    let out = add("code=ann-code").await;
    assert!(out.status.success(), "{out:?}");
    let said = out_text(&out);
    assert!(said.contains("Open this address in a browser:"), "{said}");
    let url = said.split_whitespace().find(|w| w.starts_with("http")).unwrap_or_default();
    assert!(url.contains(DEFAULT_CLIENT_ID), "{said}");
    assert!(said.contains("Signed in. The account is called ann@outlook.com"), "{said}");
    assert_eq!(labels(), ["ann@outlook.com"]);

    // With one present: another one, by its own email.
    let out = add("code=bob-code").await;
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("The account is called bob@live.com"), "{out:?}");
    assert_eq!(labels(), ["ann@outlook.com", "bob@live.com"]);
    let list = succeeded(&bus, &["account", "list"], &[]);
    assert!(list.contains("ann@outlook.com") && list.contains("bob@live.com") && list.contains("signed-in"), "{list}");

    // The same OneDrive account again: not added, and told under which name it is.
    let out = add("code=ann-code").await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(err_text(&out).contains("this OneDrive account is already added, as ann@outlook.com"), "{out:?}");
    assert_eq!(labels().len(), 2);
}

/// A daemon that stops while `account add` waits for the browser sends no `SignInFinished`:
/// the command ends at once, and says the daemon stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_add_ends_when_the_daemon_stops() {
    use std::io::Read;
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon(&bus, dir.path()).await;
    let address = bus.address().to_owned();
    let mut waiting = tokio::task::spawn_blocking(move || common::start_signing_in(&address, &["account", "add"])).await.unwrap();
    assert!(waiting.address.is_some(), "no sign-in address: {}", waiting.printed);

    // The daemon leaves the bus, as one that exits does.
    daemon.connection.clone().close().await.unwrap();
    let child = waiting.child.0.as_mut().unwrap();
    let mut status = None;
    wait_for("`account add` to end", || {
        status = child.try_wait().unwrap();
        status.is_some()
    })
    .await;
    assert_eq!(status.unwrap().code(), Some(1));
    let mut said = String::new();
    child.stderr.take().unwrap().read_to_string(&mut said).unwrap();
    assert!(said.contains("the daemon stopped before the sign-in ended. Nothing was added"), "{said}");
}

/// A development build's `dev add-account <label>` adds a signed-out account under the
/// label, as `account add <label>` did; a release build has no such command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_add_account_adds_a_signed_out_account() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon(&bus, dir.path()).await;
    if !cfg!(feature = "dev-tools") {
        let (status, told) = failed(&bus, &["dev", "add-account", "Local"], &[]);
        assert_eq!(status, 2, "{told}");
        assert!(daemon.manager.accounts().is_empty());
        return;
    }
    let added = succeeded(&bus, &["dev", "add-account", "Local"], &[]);
    let account = daemon.manager.accounts()[0].clone();
    assert!(added.contains(&format!("Added the account Local ({})", account.id)), "{added}");
    assert_eq!(account.account.state().get().state, konedrived::account::state::SignInState::SignedOut);
    let list = succeeded(&bus, &["account", "list"], &[]);
    assert!(list.contains("Local") && list.contains("signed-out"), "{list}");
    let (_, told) = failed(&bus, &["dev", "add-account", "local"], &[]);
    assert!(told.contains("cannot be an account's label") && told.contains("already used"), "{told}");
    let (status, told) = failed(&bus, &["--account", "Local", "dev", "add-account", "Other"], &[]);
    assert_eq!(status, 2, "{told}");
    assert!(told.contains("leave out --account"), "{told}");
}

/// The daemon takes a label with an "@" (`config::check_label`: an account is commonly named
/// by its email), and the CLI says nothing else in what it adds to a refused label,
/// whatever it was refused for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_the_cli_says_of_a_label_is_what_the_daemon_takes() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon(&bus, dir.path()).await;

    // The rule itself: an email is a label.
    daemon.manager.add("ann@outlook.com", &daemon.connection).await.unwrap();
    assert!(succeeded(&bus, &["account", "rename", "ann@outlook.com", "a@b"], &[]).contains("to a@b"));
    assert_eq!(daemon.manager.accounts()[0].account.state().get().label, "a@b");

    // Refused for its length, and told no rule about "@" on the way.
    let (_, told) = failed(&bus, &["account", "rename", "a@b", &"x".repeat(41)], &[]);
    assert!(told.contains("at most 40"), "{told}");
    assert!(!told.contains("no \"@\""), "the CLI says a label has no \"@\", and the daemon takes one: {told}");
}

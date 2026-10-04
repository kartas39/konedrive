use std::path::PathBuf;

use super::*;
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::helper::status::HelperState;
use crate::status::snapshot::{published_error, published_state};
use crate::sync::folder::{Record, Up};

fn record(interception: Interception, source: RootSource) -> Record {
    Record {
        root: SyncRoot { path: PathBuf::from(PATH), root_id: "9d8c7b6a-5f4e-4d3c-8b2a-1f0e9d8c7b6a".into() },
        interception,
        source,
        baloo: false,
        dev: Some(7),
    }
}

fn folder(is: Is) -> Folder {
    Folder { standing: Standing::Active, wanted: Mode::ReadOnly, is }
}

fn up(interception: Interception, source: RootSource, recovery: Recovery) -> Is {
    Is::Up(Up { record: record(interception, source), recovery, switch_failed: None })
}

/// What the bus shows of `folder` with a link to the helper (`link`) or none, the helper
/// in the state `helper`: `Path`, `State`, `LastError`, `Source`.
fn shown(folder: &Folder, link: bool, helper: HelperState) -> (String, &'static str, String, &'static str) {
    let published = publish(folder);
    let snapshot = SyncSnapshot {
        root_path: published.path,
        root_state: published.state,
        last_error: published.error,
        switch_note: published.switch_note,
        waits_for_helper: published.helper.waits(link),
        helper_state: helper,
        ..SyncSnapshot::default()
    };
    let source = published.view.record.map_or("", |record| record.source.as_str());
    (snapshot.root_path.clone(), published_state(&snapshot), published_error(&snapshot), source)
}

const PATH: &str = "/home/u/OneDrive";
const WITHOUT: Interception = Interception::Without { switch_when_helper: false };
const UNRESET: &str = "startup recovery could not reset 1 of 3 managed file(s)";
const FAILED: &str = "cannot bring up the sync folder /home/u/OneDrive: errno 5";
const HELD: &str = "this account is held back: its label repeats";

/// What a case is, the folder, whether there is a link, the helper's state, and what is
/// shown: `Path`, `State`, `LastError`, `Source`.
type Case = (&'static str, Folder, bool, HelperState, (&'static str, &'static str, String, &'static str));

/// Every state of the folder, and what the bus shows of it.
#[test]
fn every_state_is_published_as_its_path_state_error_and_source() {
    use HelperState::{Connected, Stopped, Unknown};
    use Interception::Intercepted;
    use RootSource::{Local, OneDrive};
    let advice = Stopped.advice().unwrap();
    let switching = Interception::Without { switch_when_helper: true };
    let unread = format!(
        "cannot bring up the sync folder {PATH}: config.toml has source = \"OneDrive\" for it, which is neither \
         \"onedrive\" nor \"local\"; correct it and start konedrive again, or forget the folder and add it again"
    );
    let down = |interception, down| folder(Is::Down(record(interception, OneDrive), down));
    let failed = || Down::Failed { why: FAILED.into() };
    let cases: Vec<Case> = vec![
        ("no folder", folder(Is::Absent), true, Connected, ("", "none", String::new(), "")),
        ("held for the helper, nothing known of it", down(Intercepted, Down::WaitsForHelper), false, Unknown, (PATH, "waiting", String::new(), "onedrive")),
        ("held for the helper, which is stopped", down(Intercepted, Down::WaitsForHelper), false, Stopped, (PATH, "error", advice.into(), "onedrive")),
        (
            "recorded without interception, about to come up",
            folder(Is::Down(record(WITHOUT, Local), Down::NotYetUp)),
            false,
            Stopped,
            (PATH, "waiting", String::new(), "local"),
        ),
        (
            "kept after a registration the helper may still hold",
            down(Intercepted, Down::Kept { why: "registering failed; it is kept".into() }),
            true,
            Connected,
            (PATH, "error", "registering failed; it is kept".into(), "onedrive"),
        ),
        ("a source config.toml misspells", down(Intercepted, Down::UnreadSource { written: "OneDrive".into() }), false, Stopped, (PATH, "error", unread, "onedrive")),
        ("a bring-up that failed", down(Intercepted, failed()), true, Connected, (PATH, "error", FAILED.into(), "onedrive")),
        ("a bring-up that failed, the helper gone since", down(Intercepted, failed()), false, Stopped, (PATH, "error", format!("{advice}. {FAILED}"), "onedrive")),
        ("up, intercepted", folder(up(Intercepted, OneDrive, Recovery::Clean)), true, Connected, (PATH, "ready", String::new(), "onedrive")),
        ("up, intercepted, the helper gone", folder(up(Intercepted, OneDrive, Recovery::Clean)), false, Stopped, (PATH, "error", advice.into(), "onedrive")),
        ("up, a file recovery could not reset", folder(up(Intercepted, Local, Recovery::Unreset(UNRESET.into()))), true, Connected, (PATH, "error", UNRESET.into(), "local")),
        (
            "up, part of the folder not inspected",
            folder(up(Intercepted, Local, Recovery::Uninspected("could not inspect 2 item(s)".into()))),
            true,
            Connected,
            (PATH, "ready", "could not inspect 2 item(s)".into(), "local"),
        ),
        (
            "up without interception, a local folder",
            folder(up(WITHOUT, Local, Recovery::Clean)),
            false,
            Stopped,
            (PATH, "no-interception", NO_INTERCEPTION_WARNING.into(), "local"),
        ),
        (
            "up without interception, recovery deferred",
            folder(up(WITHOUT, Local, Recovery::Deferred("left 1 interrupted file(s)".into()))),
            false,
            Unknown,
            (PATH, "no-interception", format!("{NO_INTERCEPTION_WARNING}. left 1 interrupted file(s)"), "local"),
        ),
        (
            "up without interception, showing OneDrive: it waits for the helper",
            folder(up(switching, OneDrive, Recovery::Clean)),
            false,
            Stopped,
            (PATH, "error", format!("{advice}. {NO_INTERCEPTION_WARNING}"), "onedrive"),
        ),
        (
            "up without interception, the switch to interception failed",
            folder(Is::Up(Up { record: record(switching, Local), recovery: Recovery::Clean, switch_failed: Some("errno 5".into()) })),
            true,
            Connected,
            (PATH, "no-interception", format!("{NO_INTERCEPTION_WARNING}. {}", SwitchNote { why: "errno 5".into() }.text()), "local"),
        ),
        (
            "an account held back, with a folder recorded",
            Folder { standing: Standing::HeldBack(HELD.into()), ..down(Intercepted, Down::WaitsForHelper) },
            false,
            Stopped,
            (PATH, "error", HELD.into(), ""),
        ),
        ("an account held back, with no folder", Folder { standing: Standing::HeldBack(HELD.into()), ..folder(Is::Absent) }, true, Connected, ("", "error", HELD.into(), "")),
        (
            "an account being removed, its folder forgotten",
            Folder { standing: Standing::Retiring { held: None }, ..folder(Is::Absent) },
            true,
            Connected,
            ("", "none", String::new(), ""),
        ),
    ];
    for (what, folder, link, helper, (path, state, error, source)) in cases {
        assert_eq!(shown(&folder, link, helper), (path.to_owned(), state, error, source), "{what}");
    }
}

/// What the readers get: the folder to act on, why it is not up, and the mode. An account
/// held back gives its readers no folder.
#[test]
fn the_view_says_what_the_readers_act_on() {
    let held_for_helper = Is::Down(record(Interception::Intercepted, RootSource::OneDrive), Down::WaitsForHelper);
    let waiting = publish(&Folder { wanted: Mode::ReadWrite, ..folder(held_for_helper) }).view;
    assert_eq!(
        (waiting.record.map(|r| r.root.path), waiting.down, waiting.wanted),
        (Some(PathBuf::from(PATH)), Some("it waits for the konedrive helper to connect".to_owned()), Mode::ReadWrite)
    );
    let up = publish(&folder(up(Interception::Intercepted, RootSource::OneDrive, Recovery::Clean))).view;
    assert!(up.record.is_some() && up.down.is_none());
    let held = publish(&Folder { standing: Standing::HeldBack("why".into()), ..folder(Is::Down(record(WITHOUT, RootSource::Local), Down::NotYetUp)) }).view;
    assert!(held.record.is_none());
}

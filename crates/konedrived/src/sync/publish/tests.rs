use std::path::PathBuf;

use super::*;
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::helper::status::HelperState;
use crate::status::snapshot::{published_error, published_state, FolderStatus};
use crate::sync::folder::{Content, OneDriveFolder, Record, Up};

fn record(interception: Interception, source: RootSource) -> Record {
    Record {
        root: SyncRoot { path: PathBuf::from(PATH), root_id: "9d8c7b6a-5f4e-4d3c-8b2a-1f0e9d8c7b6a".into() },
        interception,
        source,
        baloo: false,
        dev: Some(7),
        kept: Default::default(),
    }
}

fn folder(is: Is) -> Folder {
    Folder { standing: Standing::Active, wanted: Mode::ReadOnly, is }
}

fn up(interception: Interception, source: RootSource, recovery: Recovery) -> Is {
    Is::Up(Up { record: record(interception, source), recovery, switch_failed: None, content: content(source, Why::NotStarted) })
}

/// What a folder that is up and shows `source` holds, its sync not running for `why`.
fn content(source: RootSource, why: Why) -> Content {
    match source {
        RootSource::Local => Content::Local,
        RootSource::OneDrive => Content::OneDrive(OneDriveFolder { sync: Sync::Stopped(why), watcher_ended: None }),
    }
}

/// What the bus shows of `folder` with a link to the helper (`link`) or none, the helper
/// in the state `helper`: `Path`, `State`, `LastError`, `Source`.
fn shown(folder: &Folder, link: bool, helper: HelperState) -> (String, &'static str, String, &'static str) {
    let published = publish(folder);
    let cycle = crate::status::snapshot::CycleStatus {
        sync_trouble: published.cannot_start.map(|text| SyncTrouble { text, blocking: true }),
        ..Default::default()
    };
    let folder = FolderStatus {
        root_path: published.path,
        root_state: published.state,
        last_error: published.error,
        switch_note: published.switch_note,
        waits_for_helper: published.helper.waits(link),
        locked_note: published.locked_note,
        writable: published.writable,
        helper_state: helper,
    };
    let snapshot = SyncSnapshot { folder, cycle, ..SyncSnapshot::default() };
    let source = published.view.record.map_or("", |record| record.source.as_str());
    (snapshot.folder.root_path.clone(), published_state(&snapshot), published_error(&snapshot), source)
}

const PATH: &str = "/home/u/OneDrive";
const WITHOUT: Interception = Interception::Without { switch_when_helper: false };
const UNRESET: &str = "startup recovery could not reset 1 of 3 managed file(s)";
const FAILED: &str = "cannot bring up the sync folder /home/u/OneDrive: errno 5";
const HELD: &str = "this account is held back: its label repeats";

/// What a case is, the folder, whether there is a link, the helper's state, and what is
/// shown: `Path`, `State`, `LastError`, `Source`.
type Case = (&'static str, Folder, bool, HelperState, (&'static str, &'static str, String, &'static str));

/// `Writable`, and the sentence of a folder that is locked though its account is read-write,
/// from what the running sync does about local changes: writable only for a read-write
/// folder whose lock is off, not while its watcher still walks it.
#[test]
fn a_folder_is_writable_only_once_its_lock_is_off() {
    use crate::sync::running_sync::{Lock, Uploading};
    let why = "the folder stays read-only and nothing is uploaded";
    let cases = [
        ("no sync runs", Mode::ReadWrite, None, (false, "")),
        ("read-only, locked", Mode::ReadOnly, Some(Uploading::Locked(None)), (false, "")),
        ("read-write, its watcher could not start", Mode::ReadWrite, Some(Uploading::Locked(Some(why.into()))), (false, why)),
        ("read-write, its watcher still walks", Mode::ReadWrite, Some(Uploading::Open(Lock::Walking)), (false, "")),
        ("read-write, the walk was cut", Mode::ReadWrite, Some(Uploading::Open(Lock::Stays(why.into()))), (false, why)),
        ("read-write, the lock off", Mode::ReadWrite, Some(Uploading::Open(Lock::Off)), (true, "")),
    ];
    for (what, wanted, uploading, (writable, note)) in cases {
        assert_eq!(uploads(wanted, uploading), (writable, note.to_owned()), "{what}");
    }
}

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
            "up and intercepted, showing OneDrive, its sync could not start",
            folder(Is::Up(Up {
                record: record(Intercepted, OneDrive),
                recovery: Recovery::Clean,
                switch_failed: None,
                content: content(OneDrive, Why::CannotStart("the tree store cannot be opened: errno 5".into())),
            })),
            true,
            Connected,
            (PATH, "error", "the tree store cannot be opened: errno 5".into(), "onedrive"),
        ),
        (
            "up without interception, the switch to interception failed",
            folder(Is::Up(Up {
                record: record(switching, Local),
                recovery: Recovery::Clean,
                switch_failed: Some("errno 5".into()),
                content: Content::Local,
            })),
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

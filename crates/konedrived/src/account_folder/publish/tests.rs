use super::*;
use crate::account_folder::folder::{Down, Up};
use crate::account_folder::staged_folder::make_record;
use crate::config::{AccountHold, Mode};
use crate::status::folder_trouble::{FolderTrouble, TakeDownCause};

fn folder_that(is: Is) -> Folder {
    Folder { is, wanted: Mode::ReadWrite, ..Folder::new() }
}

/// What the folders of these tests ran into.
fn store_failed() -> FolderTrouble {
    FolderTrouble::StoreFailed { path: "/home/u/OneDrive".into(), detail: "it cannot".into() }
}

fn taken_down() -> FolderTrouble {
    FolderTrouble::TakenDown { path: "/home/u/OneDrive".into(), cause: TakeDownCause::UndoFailed { path: "/home/u/OneDrive/a".into() } }
}

fn up_with(sync: Sync) -> Is {
    Is::Up(Up { record: make_record(std::path::Path::new("/home/u/OneDrive"), None), sync, take_down_try: None })
}

/// What a recorded folder's sync comes to: the folder is ready, and a sync that could not
/// start, waits for the account's drive or was taken down says its sentence as trouble
/// that blocks; one that only has not started, or was interrupted by a change, says none.
#[test]
fn a_stopped_sync_says_its_trouble_where_it_has_one() {
    let cases = [
        (Why::NotStarted, None),
        (Why::Interrupted, None),
        (Why::CannotStart(store_failed()), Some(store_failed())),
        (Why::DriveNotKnown(FolderTrouble::DriveNotKnown { path: "/home/u/OneDrive".into() }), Some(FolderTrouble::DriveNotKnown { path: "/home/u/OneDrive".into() })),
        (Why::TakenDown { cause: taken_down(), attempt: 3, next_try_at: Some(7) }, Some(taken_down())),
        (Why::TakenDown { cause: taken_down(), attempt: 0, next_try_at: None }, Some(taken_down())),
    ];
    for (why, trouble) in cases {
        let said = format!("{why:?}");
        let published = publish(&folder_that(up_with(Sync::Stopped(why))));
        assert_eq!(published.state, RootState::Ready, "{said}");
        assert_eq!(published.path, "/home/u/OneDrive");
        assert_eq!(published.error, None, "{said}: the registration ran into nothing");
        assert_eq!(published.cannot_start, trouble, "{said}");
        assert!(matches!(&published.view.sync, SyncView::Stopped(shown) if *shown == trouble), "{said}");
        assert!(!published.writable, "{said}: no mount takes changes");
        assert!(published.view.record.is_some() && published.view.down.is_none(), "{said}");
        assert_eq!(published.view.wanted, Mode::ReadWrite);
        assert!(published.view.parts.is_none() && published.view.poll.is_none() && published.view.sending_holds.is_none() && published.view.onedrive_space.is_none(), "{said}: nothing of a running sync");
    }
}

#[test]
fn a_folder_that_is_absent_or_down_says_so() {
    let record = || make_record(std::path::Path::new("/home/u/OneDrive"), None);
    let absent = publish(&folder_that(Is::Absent));
    assert_eq!((absent.state, absent.path.as_str(), absent.error), (RootState::None, "", None));
    assert!(absent.view.record.is_none() && matches!(absent.view.sync, SyncView::None));

    let waiting = publish(&folder_that(Is::Down(record(), Down::NotYetUp)));
    assert_eq!((waiting.state, waiting.error), (RootState::Waiting, None), "nothing is known to be wrong");
    assert_eq!(waiting.view.down.as_deref(), Some("it is being brought up"));

    let moved = FolderTrouble::Moved { path: "/home/u/OneDrive".into() };
    let failed = publish(&folder_that(Is::Down(record(), Down::Failed { why: moved.clone() })));
    assert_eq!((failed.state, failed.error.as_ref()), (RootState::Error, Some(&moved)));
    assert_eq!(failed.view.down, Some(moved.to_string()));
    assert!(failed.view.record.is_some(), "a folder that is down is still acted on");

    let unread = publish(&folder_that(Is::Down(record(), Down::UnreadSource { written: "nextcloud".into() })));
    assert_eq!(unread.state, RootState::Error);
    assert_eq!(unread.error, Some(FolderTrouble::UnreadSource { path: "/home/u/OneDrive".into(), written: "nextcloud".into() }));
}

/// An account held back says so over whatever its folder is, and nothing acts on the
/// folder: the view has no record, though the path is shown.
#[test]
fn an_account_held_back_says_why_over_whatever_its_folder_is() {
    let hold = || AccountHold::SameDrive { other: "Home".into() };
    for is in [Is::Absent, up_with(Sync::Stopped(Why::CannotStart(store_failed())))] {
        let has_folder = !matches!(is, Is::Absent);
        let folder = Folder { standing: Standing::HeldBack(hold()), ..folder_that(is) };
        let published = publish(&folder);
        assert_eq!((published.state, published.error), (RootState::Error, Some(FolderTrouble::HeldBack { hold: hold() })));
        assert_eq!(published.path.is_empty(), !has_folder);
        assert!(published.view.record.is_none() && published.cannot_start.is_none());
        assert_eq!(published.view.wanted, Mode::ReadWrite);
    }
}

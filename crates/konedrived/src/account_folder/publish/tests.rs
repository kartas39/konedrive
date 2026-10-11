use super::*;
use crate::account_folder::folder::{Down, Up};
use crate::account_folder::staged_folder::make_record;
use crate::config::Mode;

fn folder_that(is: Is) -> Folder {
    Folder { is, wanted: Mode::ReadWrite, ..Folder::new() }
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
        (Why::CannotStart("it cannot".into()), Some("it cannot")),
        (Why::DriveNotKnown("no drive yet".into()), Some("no drive yet")),
        (Why::TakenDown { cause: "a fault".into(), attempt: 3, next_try_at: Some(7) }, Some("a fault")),
        (Why::TakenDown { cause: "a fault".into(), attempt: 0, next_try_at: None }, Some("a fault")),
    ];
    for (why, trouble) in cases {
        let said = format!("{why:?}");
        let published = publish(&folder_that(up_with(Sync::Stopped(why))));
        assert_eq!(published.state, RootState::Ready, "{said}");
        assert_eq!(published.path, "/home/u/OneDrive");
        assert_eq!(published.error, "", "{said}: the registration ran into nothing");
        assert_eq!(published.cannot_start.as_deref(), trouble, "{said}");
        assert!(matches!(&published.view.sync, SyncView::Stopped(shown) if shown.as_deref() == trouble), "{said}");
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
    assert_eq!((absent.state, absent.path.as_str(), absent.error.as_str()), (RootState::None, "", ""));
    assert!(absent.view.record.is_none() && matches!(absent.view.sync, SyncView::None));

    let waiting = publish(&folder_that(Is::Down(record(), Down::NotYetUp)));
    assert_eq!((waiting.state, waiting.error.as_str()), (RootState::Waiting, ""), "nothing is known to be wrong");
    assert_eq!(waiting.view.down.as_deref(), Some("it is being brought up"));

    let failed = publish(&folder_that(Is::Down(record(), Down::Failed { why: "the folder moved".into() })));
    assert_eq!((failed.state, failed.error.as_str()), (RootState::Error, "the folder moved"));
    assert_eq!(failed.view.down.as_deref(), Some("the folder moved"));
    assert!(failed.view.record.is_some(), "a folder that is down is still acted on");

    let unread = publish(&folder_that(Is::Down(record(), Down::UnreadSource { written: "nextcloud".into() })));
    assert_eq!(unread.state, RootState::Error);
    assert!(unread.error.contains("nextcloud") && unread.error.contains("/home/u/OneDrive"), "{}", unread.error);
}

/// An account held back says so over whatever its folder is, and nothing acts on the
/// folder: the view has no record, though the path is shown.
#[test]
fn an_account_held_back_says_why_over_whatever_its_folder_is() {
    for is in [Is::Absent, up_with(Sync::Stopped(Why::CannotStart("it cannot".into())))] {
        let has_folder = !matches!(is, Is::Absent);
        let folder = Folder { standing: Standing::HeldBack("held back: a drive twice".into()), ..folder_that(is) };
        let published = publish(&folder);
        assert_eq!((published.state, published.error.as_str()), (RootState::Error, "held back: a drive twice"));
        assert_eq!(published.path.is_empty(), !has_folder);
        assert!(published.view.record.is_none() && published.cannot_start.is_none());
        assert_eq!(published.view.wanted, Mode::ReadWrite);
    }
}

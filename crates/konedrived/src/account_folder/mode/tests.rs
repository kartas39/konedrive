use std::sync::Arc;

use konedrive_graph::drive::DriveQuota;

use super::*;
use crate::account::state::ModeNote;
use crate::account::AccountFolder;
use crate::account_folder::staged_folder::{make_kept_parts, make_service, publish_running_view, read_sync, stage_folder};
use crate::account_folder::testing::parts;
use crate::config::DriveId;
use crate::mount::item_fixture::ItemFixture;
use crate::sync_parts::running_steps::RunningStep;

const READ_WRITE: &str = "Files.ReadWrite User.Read offline_access";
const READ: &str = "Files.Read User.Read offline_access";

fn snapshot(state: SignInState, granted: &str, live_drive: &str) -> AccountSnapshot {
    AccountSnapshot { state, granted_scopes: granted.to_owned(), live_drive: live_drive.to_owned(), ..AccountSnapshot::default() }
}

fn standing(mode: Mode, recorded_drive: Option<&str>) -> WriteStanding {
    WriteStanding { mode, recorded_drive: recorded_drive.and_then(DriveId::new) }
}

/// What of the account holds the folder's sending: signed out for any state but signed in;
/// and for an account `config.toml` says is read-write the first refusal that holds — the
/// file unreadable, no drive recorded, the drive not seen yet, another drive seen, a
/// sign-in that grants no writes. None for a read-only account: its folder is held as
/// read-only, which is the folder's own hold.
#[test]
fn what_of_the_account_holds_the_sending_is_the_first_cause_that_holds() {
    let read_write_d1 = standing(Mode::ReadWrite, Some("D1"));
    // The recorded drive is said as a drive id is kept: one spelling whatever the case of its letters.
    let mismatch = || Some(ModeNote::DriveMismatch { live: "D9".into(), recorded: "d1".into() });
    // (the snapshot, what config.toml says, signed out, the refusal)
    let cases: [(AccountSnapshot, Option<WriteStanding>, bool, Option<ModeNote>); 11] = [
        (snapshot(SignInState::SignedIn, READ_WRITE, "D1"), Some(read_write_d1.clone()), false, None),
        (snapshot(SignInState::SignedOut, "", ""), Some(standing(Mode::ReadOnly, None)), true, None),
        (snapshot(SignInState::SigningIn, READ_WRITE, "D1"), Some(read_write_d1.clone()), true, None),
        (snapshot(SignInState::SignedIn, READ_WRITE, "D1"), None, false, Some(ModeNote::ConfigUnreadable)),
        // A read-only account: nothing of it refuses, whatever else is amiss.
        (snapshot(SignInState::SignedIn, READ, ""), Some(standing(Mode::ReadOnly, None)), false, None),
        (snapshot(SignInState::SignedIn, READ, "D9"), Some(standing(Mode::ReadOnly, Some("D1"))), false, None),
        (snapshot(SignInState::SignedIn, READ_WRITE, "D1"), Some(standing(Mode::ReadWrite, None)), false, Some(ModeNote::NoDriveRecorded)),
        // The drive not seen yet is said as that, not as a drive "" that differs.
        (snapshot(SignInState::SignedIn, READ_WRITE, ""), Some(read_write_d1.clone()), false, Some(ModeNote::DriveNotSeen)),
        (snapshot(SignInState::SignedIn, READ_WRITE, "D9"), Some(read_write_d1.clone()), false, mismatch()),
        (snapshot(SignInState::SignedIn, READ, "D1"), Some(read_write_d1.clone()), false, Some(ModeNote::SignInToWrite)),
        // Another drive comes before a grant that cannot write: signing in again cannot cure it.
        (snapshot(SignInState::SignedIn, READ, "D9"), Some(read_write_d1.clone()), false, mismatch()),
    ];
    for (snapshot, standing, is_signed_out, refusal) in cases {
        let holds = decide_account_holds(&snapshot, standing.as_ref());
        let said = format!("{:?} {:?} {:?} {standing:?}: {holds:?}", snapshot.state, snapshot.granted_scopes, snapshot.live_drive);
        assert_eq!(holds.is_signed_out, is_signed_out, "{said}");
        assert_eq!(holds.writes_refusal, refusal, "{said}");
    }
}

/// The account switches to read-only while the folder's sync does not run but its mount is
/// still served: the folder takes the mode, the kept mount takes no change from then on,
/// and what is unsent is counted and stays.
#[test]
fn a_folder_whose_sync_does_not_run_holds_what_is_unsent_by_switching_its_kept_mount() {
    let fixture = ItemFixture::new();
    fixture.make_own_file("a.txt", b"one");
    fixture.make_own_file("b.txt", b"two");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        mount.switch_to_read_write().expect("the mount takes changes");
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;
        service.hold_folder().await.set_wanted(Mode::ReadWrite);
        assert!(service.writable());

        let held = service.hold_unsent_changes().await;
        assert_eq!((held.unsent, held.is_request_in_flight), (2, false), "nothing is dropped, and no sync runs to send");
        assert_eq!(service.mode(), Mode::ReadOnly);
        assert!(!mount.access_mode_switch().is_read_write(), "the kept mount refuses new writes");
        assert!(!service.writable(), "and the bus says so");
        assert_eq!(service.count_unsent_to_send().await, 2);
        // What the last plan answered will not be sent by itself is left out, as in the count the plan reports.
        service.view().parts.expect("the kept parts").published_plan.publish_unsent_not_to_send_count(1);
        assert_eq!(service.count_unsent_to_send().await, 1);
        service.view().parts.expect("the kept parts").published_plan.publish_unsent_not_to_send_count(5);
        assert_eq!(service.count_unsent_to_send().await, 0, "never below nothing");
        end_scene(&service, mount).await;
    });
}

/// The folder follows the mode the ACCOUNT says when the folder's lock is held, not the one
/// its caller woke with: an older snapshot never turns a switch back.
#[test]
fn the_mode_followed_is_the_accounts_own_whatever_the_caller_woke_with() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;
        parts(&service).account.state().update(|s| s.mode = Mode::ReadWrite);

        // Woken with read-only, an older snapshot: the account says read-write by now.
        service.follow_mode(Mode::ReadOnly).await;
        assert_eq!(service.mode(), Mode::ReadWrite);
        assert!(mount.access_mode_switch().is_read_write(), "the kept mount takes changes");

        parts(&service).account.state().update(|s| s.mode = Mode::ReadOnly);
        service.follow_mode(Mode::ReadWrite).await;
        assert_eq!(service.mode(), Mode::ReadOnly);
        assert!(!mount.access_mode_switch().is_read_write());
        end_scene(&service, mount).await;
    });
}

/// A folder that is not up only takes the mode, which its next start applies.
#[tokio::test]
async fn a_folder_that_is_not_up_only_takes_the_mode() {
    let (service, _clock) = make_service();
    parts(&service).account.state().update(|s| s.mode = Mode::ReadWrite);
    service.follow_mode(Mode::ReadWrite).await;
    assert_eq!(service.mode(), Mode::ReadWrite);
    let held = service.hold_unsent_changes().await;
    assert_eq!((held.unsent, held.is_request_in_flight), (0, false));
    assert_eq!(service.mode(), Mode::ReadOnly);
    assert_eq!(service.count_unsent_to_send().await, 0);
    assert!(!service.has_request_in_flight());
}

/// Whether a request that changes OneDrive is in flight is read from the running steps of
/// the view: a step that sends content or metadata is one, a download or a step towards the
/// folder is none. It takes no lock of the folder: asked here while the folder is held.
#[test]
fn a_request_is_in_flight_while_a_step_towards_onedrive_runs() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"content");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        publish_running_view(&service, &fixture, kept);
        let _door = service.folder.write().await;
        let step = |class| RunningStep { class, item: file.local_item_id, other_items: Vec::new(), destination: None, sent: 0, total: 7 };
        for (class, is_in_flight) in [(StepClass::Content, true), (StepClass::Metadata, true), (StepClass::Download, false), (StepClass::Folder, false), (StepClass::Row, false)] {
            assert!(fixture.running_steps.begin(step(class)));
            assert_eq!(service.has_request_in_flight(), is_in_flight, "{class:?}");
            fixture.running_steps.end(file.local_item_id);
            assert!(!service.has_request_in_flight(), "after {class:?} ended");
        }
        drop(_door);
        end_scene(&service, mount).await;
    });
}

/// `RefreshInfo` read the quota: it is applied to the running sync's space, and the plan's
/// task is woken for it.
#[test]
fn a_quota_the_account_read_is_applied_to_the_running_sync_and_wakes_the_plan() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        publish_running_view(&service, &fixture, kept);
        assert!(!fixture.onedrive_space.is_full());
        service.quota_read(&DriveQuota { total: 100, used: 100, remaining: Some(0), state: "exceeded".into(), ..DriveQuota::default() });
        assert!(fixture.onedrive_space.is_full(), "who reads the quota applies it");
        assert!(fixture.sync_plan_wake.wait().await.contains(&WakeCause::QuotaRead));
        end_scene(&service, mount).await;
    });
}

/// The scene's end: the view and the folder let go of the mount, which is dropped where a
/// thread may be joined.
async fn end_scene(service: &SyncService, mount: Arc<crate::mount::ItemMount>) {
    let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
    service.view.send_replace(crate::account_folder::folder::View::default());
    assert!(read_sync(service, |folder| folder.parts().is_none()).await);
    tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
}

use std::sync::Arc;

use crate::account::state::{AccountSnapshot, ModeNote, SignInState};
use crate::account::AccountFolder;
use crate::account_folder::folder::{Sync, SyncView, Why};
use crate::account_folder::staged_folder::{end_running_sync, make_service_with_drive, read_sync, stage_running_sync};
use crate::account_folder::testing::parts;
use crate::conditions::running::Clock;
use crate::config::{ConfigError, Mode};
use crate::mount::item_fixture::ItemFixture;
use crate::sync_parts::running_steps::{RunningStep, StepClass};
use crate::sync_parts::sending_holds::SendingHold;
use crate::sync_tasks::tests::wait_until;

/// A change of the folder stops the sync that runs — its token cancelled, its tasks waited
/// for — and keeps what it left with the folder; the same change starts it again over
/// those parts, with the mount it had.
#[test]
fn a_change_stops_the_running_sync_keeps_its_parts_and_starts_it_again_over_them() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let (service, _clock) = make_service_with_drive(&fixture, graph.client());
        stage_running_sync(&service, &fixture, graph.client()).await;
        let (mount, first_stop) = (service.mount().expect("the folder is mounted"), service.running().expect("its sync runs"));
        assert!(service.view().poll.is_some() && service.view().sending_holds.is_some());
        {
            let mut stopped = service.change().await;
            assert!(stopped.ran());
            assert!(first_stop.is_cancelled(), "the sync was told to stop");
            assert!(stopped.folder().running().is_none());
            assert!(matches!(stopped.folder().up().map(|up| &up.sync), Some(Sync::Stopped(Why::Interrupted))));
            assert!(stopped.folder().parts().is_some_and(|kept| Arc::ptr_eq(&kept.mount, &mount)), "what the stop left is kept, whole");
            // The readers find the folder's store and mount, and nothing of a running sync.
            let view = service.view();
            assert!(view.parts.is_some() && view.poll.is_none() && view.sending_holds.is_none() && view.onedrive_space.is_none());
            assert!(matches!(view.sync, SyncView::Stopped(None)));
            assert!(fixture.running_steps.list_steps().is_empty(), "no step is left running");

            service.start_again(&mut stopped).await;
            assert!(stopped.folder().running().is_some_and(|started| Arc::ptr_eq(&started.mount, &mount)), "started over the parts it kept");
        }
        let second_stop = service.running().expect("the sync runs again");
        assert!(!second_stop.is_cancelled());
        assert!(service.view().poll.is_some() && service.view().sending_holds.is_some());
        assert_eq!(service.last_error(), "");
        end_running_sync(&service).await;
        assert!(second_stop.is_cancelled());
    });
}

/// `Refresh()` of a folder whose sync runs asks that sync, and stops nothing: the listing
/// is asked for a cycle and the quota is read once, by the plan's task.
#[test]
fn a_refresh_asks_the_running_sync_and_stops_nothing() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let (service, _clock) = make_service_with_drive(&fixture, graph.client());
        stage_running_sync(&service, &fixture, graph.client()).await;
        let stop = service.running().expect("the sync runs");
        // The first cycle of the start has ended.
        wait_until("the first listing ended", || graph.with(|cloud| cloud.count("GET", "delta")) >= 1).await;
        let (cycles, quota_reads) = (graph.with(|cloud| cloud.count("GET", "delta")), graph.with(|cloud| cloud.quota_reads()));

        service.refresh().await.expect("a sync runs");
        assert!(!stop.is_cancelled(), "the sync was not stopped for it");
        assert!(service.running().is_some());
        // By the plan's task, which the refresh woke: the one reader of a running sync's quota.
        wait_until("the quota read for the refresh", || graph.with(|cloud| cloud.quota_reads()) > quota_reads).await;
        wait_until("the cycle the refresh asked for", || graph.with(|cloud| cloud.count("GET", "delta")) > cycles).await;
        assert_eq!(graph.with(|cloud| cloud.quota_reads()), quota_reads + 1, "the quota is read once for one refresh, and by none but the task");
        end_running_sync(&service).await;
    });
}

/// The owner's one watcher of the account sets and ends what the account holds of a running
/// sync: a sign-out holds all sending and a sign-in ends that hold; why the account may not
/// write is set when it changes, and taken off when it may again.
#[test]
fn the_accounts_state_sets_and_ends_the_holds_of_a_running_sync() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let (service, clock) = make_service_with_drive(&fixture, graph.client());
        stage_running_sync(&service, &fixture, graph.client()).await;
        let made = parts(&service);
        let holds = service.view().sending_holds.expect("the sync runs");
        let shown = || holds.find(clock.now());
        // A read-write account of drive d1, as `config.toml` and its state say.
        made.persist.store.record_drive(&made.persist.account, &crate::config::DriveId::new("d1").unwrap()).unwrap();
        made.persist
            .store
            .update_account(&made.persist.account, |account| {
                account.mode = Mode::ReadWrite;
                Ok::<_, ConfigError>(())
            })
            .unwrap();
        let read_write = AccountSnapshot { state: SignInState::SignedIn, mode: Mode::ReadWrite, granted_scopes: "Files.ReadWrite".into(), live_drive: "d1".into(), ..AccountSnapshot::default() };
        made.account.state().update(|s| *s = read_write.clone());
        service.follow_account(&read_write).await;
        assert!(!matches!(shown(), Some(SendingHold::SignedOut | SendingHold::WritesNotAllowed { .. })), "{:?}", shown());

        let signed_out = AccountSnapshot { state: SignInState::SignedOut, ..read_write.clone() };
        made.account.state().update(|s| *s = signed_out.clone());
        service.follow_account(&signed_out).await;
        assert_eq!(shown(), Some(SendingHold::SignedOut));
        assert!(service.running().is_some(), "no task is stopped for a sign-out");

        made.account.state().update(|s| *s = read_write.clone());
        service.follow_account(&read_write).await;
        assert!(!matches!(shown(), Some(SendingHold::SignedOut)), "the sign-in ended the hold: {:?}", shown());

        // The token was seen to reach another drive: the account may not write, and the folder says why.
        let other_drive = AccountSnapshot { live_drive: "d9".into(), ..read_write.clone() };
        made.account.state().update(|s| *s = other_drive.clone());
        service.follow_account(&other_drive).await;
        assert!(matches!(shown(), Some(SendingHold::WritesNotAllowed { reason: ModeNote::DriveMismatch { live, .. } }) if live == "d9"), "{:?}", shown());

        made.account.state().update(|s| *s = read_write.clone());
        service.follow_account(&read_write).await;
        assert!(!matches!(shown(), Some(SendingHold::WritesNotAllowed { .. })), "{:?}", shown());
        end_running_sync(&service).await;
    });
}

/// The folder follows its account's mode while its sync runs, and no task is stopped for
/// it: read-only, the mount refuses changes and the sending is held as a read-only
/// folder's; read-write again, both are taken back.
#[test]
fn a_running_sync_follows_the_accounts_mode_without_being_stopped() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let (service, clock) = make_service_with_drive(&fixture, graph.client());
        stage_running_sync(&service, &fixture, graph.client()).await;
        let (made, mount, stop) = (parts(&service), service.mount().unwrap(), service.running().unwrap());
        let holds = service.view().sending_holds.unwrap();
        wait_until("the first listing ended", || !matches!(holds.find(clock.now()), Some(SendingHold::FirstListingNotEnded))).await;
        assert!(mount.access_mode_switch().is_read_write() && service.writable());

        // The account says read-only (its state's default mode).
        service.follow_mode(Mode::ReadOnly).await;
        assert_eq!(service.mode(), Mode::ReadOnly);
        assert!(!mount.access_mode_switch().is_read_write() && !service.writable());
        assert_eq!(holds.find(clock.now()), Some(SendingHold::FolderReadOnly));

        made.account.state().update(|s| s.mode = Mode::ReadWrite);
        service.follow_mode(Mode::ReadWrite).await;
        assert_eq!(service.mode(), Mode::ReadWrite);
        assert!(mount.access_mode_switch().is_read_write() && service.writable());
        assert_eq!(holds.find(clock.now()), None);
        assert!(!stop.is_cancelled(), "the same sync ran through both switches");
        end_running_sync(&service).await;
    });
}

/// The account switches to read-only over a running sync with nothing in flight: the folder
/// holds at once, what is unsent is counted and stays, and no request is in flight.
#[test]
fn a_running_sync_holds_what_is_unsent_and_says_that_nothing_is_in_flight() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        // A drive that answers nothing: the first listing never ends, so nothing is sent and what is unsent stays.
        let nowhere = konedrive_graph::drive::DriveClient::new(url::Url::parse("http://127.0.0.1:9/").unwrap(), Arc::new(konedrive_graph::token::StaticToken::new("T")))
            .unwrap()
            .with_retry(konedrive_graph::drive::RetryPolicy { attempts: 1, default_wait: std::time::Duration::from_millis(5), max_wait: std::time::Duration::from_millis(5) });
        let (service, clock) = make_service_with_drive(&fixture, nowhere.clone());
        stage_running_sync(&service, &fixture, nowhere).await;
        let mount = service.mount().unwrap();
        fixture.make_own_file("a.txt", b"one");
        fixture.make_own_file("b.txt", b"two");

        let held = service.hold_unsent_changes().await;
        assert_eq!((held.unsent, held.is_request_in_flight), (2, false));
        assert_eq!(service.mode(), Mode::ReadOnly);
        assert!(!mount.access_mode_switch().is_read_write());
        assert_eq!(service.view().sending_holds.unwrap().find(clock.now()), Some(SendingHold::FolderReadOnly));
        assert_eq!(service.count_unsent_to_send().await, 2, "nothing is dropped");
        assert!(service.running().is_some(), "the sync runs on");
        end_running_sync(&service).await;
    });
}

/// A step towards OneDrive that still runs when the switch's wait for it runs out: the
/// folder holds all the same and says that a request is in flight, so that the account
/// keeps its token; asked again it only reads the running steps, and says none once the
/// step ended. On a clock that is paused: the wait costs no time.
#[test]
fn a_step_that_outlasts_the_switchs_wait_is_said_as_a_request_in_flight() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"content");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
    runtime.block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let (service, _clock) = make_service_with_drive(&fixture, graph.client());
        stage_running_sync(&service, &fixture, graph.client()).await;
        // An upload that hangs: a step towards OneDrive that does not end.
        assert!(fixture.running_steps.begin(RunningStep { class: StepClass::Content, item: file.local_item_id, other_items: Vec::new(), destination: None, sent: 1, total: 7 }));

        let held = service.hold_unsent_changes().await;
        assert!(held.is_request_in_flight, "the wait ran out with the step still running");
        assert_eq!(service.mode(), Mode::ReadOnly, "the folder holds all the same");
        assert!(service.has_request_in_flight());
        fixture.running_steps.end(file.local_item_id);
        assert!(!service.has_request_in_flight());
        end_running_sync(&service).await;
    });
}

/// The daemon's end for a folder whose sync runs: the sync is stopped, the mount of the
/// parts it kept is stopped, the folder's path is answered for the desktop to be told, and
/// nothing of the folder is left with the service or its readers.
#[test]
fn the_daemons_end_stops_the_running_sync_and_the_mount_it_kept() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let (service, _clock) = make_service_with_drive(&fixture, graph.client());
        stage_running_sync(&service, &fixture, graph.client()).await;
        let stop = service.running().unwrap();

        assert_eq!(service.end_at_daemon_stop().await, Some(fixture.folder()));
        assert!(stop.is_cancelled());
        assert!(read_sync(&service, |folder| folder.running().is_none() && folder.parts().is_none()).await, "the parts went with the mount's stop");
        assert!(service.mount().is_none() && service.view().poll.is_none());
        assert!(fixture.running_steps.list_steps().is_empty());
        // Nothing starts it afterwards.
        assert!(service.refresh().await.is_err());
        assert!(read_sync(&service, |folder| folder.running().is_none()).await);
    });
}

/// A sync that was told to stop is published as stopped, and with none of its handles: a
/// reader that asks the view for the poll, the holds or the space of a running sync gets
/// none, as the sync itself is not shown as running.
#[test]
fn a_sync_told_to_stop_is_published_with_none_of_its_handles() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let (service, _clock) = make_service_with_drive(&fixture, graph.client());
        stage_running_sync(&service, &fixture, graph.client()).await;
        {
            let folder = service.folder.read().await;
            let running = crate::account_folder::publish::publish(&folder).view;
            assert!(matches!(running.sync, SyncView::Running(_)) && running.poll.is_some() && running.sending_holds.is_some() && running.onedrive_space.is_some());
            folder.running().expect("the sync runs").tasks.stop_handle().cancel();
            let told_to_stop = crate::account_folder::publish::publish(&folder).view;
            assert!(matches!(told_to_stop.sync, SyncView::Stopped(None)));
            assert!(told_to_stop.poll.is_none() && told_to_stop.sending_holds.is_none() && told_to_stop.onedrive_space.is_none(), "the handles of a sync that is not shown as running");
            assert!(told_to_stop.parts.is_some(), "the parts stay: the store and the mount are the folder's");
        }
        end_running_sync(&service).await;
    });
}

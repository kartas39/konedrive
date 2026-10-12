//! A folder's stop and what ends the parts it keeps, over the fixture's folder and a mount
//! that is built and not served.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use konedrive_tree::{Reason, Store, TreeStore};

use super::*;
use crate::listing::listing_poller::ListingPollHandle;
use crate::mount::item_fixture::ItemFixture;
use crate::sync_parts::lease::Lease;
use crate::sync_parts::running_steps::StepClass;
use crate::sync_parts::sending_holds::SendingHold;
use crate::sync_parts::sync_plan_wake::SyncPlanWake;
use crate::sync_tasks::tests::{block_on_paused_clock, make_kept_parts, make_start_input, start_over_kept_parts, stop, wait_until};

/// The permission bits of `path`.
fn mode_of(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The parts of the fixture's folder with a store of their own, which nobody else holds:
/// what a take-down and a Forget close.
fn make_kept_parts_with_own_store(fixture: &ItemFixture, directory: &tempfile::TempDir) -> KeptFolderParts {
    let store = Store::new(TreeStore::open(&directory.path().join("own.db")).expect("a store"));
    KeptFolderParts { store, ..make_kept_parts(fixture) }
}

#[test]
fn a_step_that_waits_for_the_lease_under_the_stoppers_write_lock_does_not_hold_the_stop() {
    let fixture = ItemFixture::new();
    // OneDrive has a file the folder does not show yet: the first plan shows it, a step towards the folder.
    let listed = fixture.list_onedrive_file("new.txt", b"new");
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let folder_lock = Arc::new(tokio::sync::RwLock::new(()));
        let mut input = make_start_input(&fixture, graph.client());
        input.lease = Lease::on(&folder_lock);
        // The owner holds the folder for writing: the step cannot get its lease.
        let held_for_writing = folder_lock.write().await;
        let started = start_over_kept_parts(&fixture, input).await;
        wait_until("the step towards the folder was begun", || fixture.running_steps.list_steps().iter().any(|step| step.class == StepClass::Folder)).await;

        // The owner's order: the token, then the stop under the lock it holds.
        started.tasks.stop_handle().cancel();
        let kept = tokio::time::timeout(Duration::from_secs(10), stop_folder_sync(started)).await.expect("the stop does not wait for the step that waits for the lock");

        assert!(fixture.running_steps.list_steps().is_empty(), "the step was settled as not run");
        assert!(fixture.find_item(listed.local_item_id).unwrap().local.is_none(), "and touched nothing");
        assert!(!fixture.disk_path("new.txt").exists());
        drop((held_for_writing, kept));
    });
}

#[test]
fn a_sign_out_holds_all_sending_and_stops_no_task() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let started = start_over_kept_parts(&fixture, make_start_input(&fixture, graph.client())).await;

        started.tasks.hold_for_sign_out();

        let now = (fixture.clock())();
        assert_eq!(started.tasks.sending_holds().find(now), Some(SendingHold::SignedOut));
        assert!(started.tasks.sending_holds().is_held(StepClass::Download, now), "a download waits for the sign-in too");
        assert!(!started.tasks.plan_loop.is_finished() && !started.tasks.stop_handle().is_cancelled());
        stop(started).await;
    });
}

#[test]
fn a_sign_in_makes_every_next_try_due_ends_the_hold_wakes_the_plan_and_asks_for_a_cycle() {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("own.txt", b"own");
    let far = (fixture.clock())() + 3600;
    fixture.write_rows(move |store| store.step_memory().remember_failed_try(file.local_item_id, file.unsent_since, &Reason::Forbidden, Some(far)).map(drop));
    assert_eq!(fixture.write_rows(|store| store.step_memory().list_timed_waits(10)).len(), 1);
    let sync_plan_wake = Arc::new(SyncPlanWake::new());
    let sending_holds = SendingHolds::new(Arc::clone(&sync_plan_wake));
    sending_holds.set_signed_out(true);
    let (poll, asked_cycle_count) = ListingPollHandle::for_tests();
    fixture.runtime().block_on(async {
        sync_plan_wake.wait().await;

        release_after_sign_in(&fixture.store, &sending_holds, &poll).await;

        assert_ne!(sending_holds.find((fixture.clock())()), Some(SendingHold::SignedOut));
        assert_eq!(sync_plan_wake.wait().await, BTreeSet::from([WakeCause::SendingHoldChanged]), "the end of the hold wakes the plan");
        assert_eq!(asked_cycle_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    });
    assert!(fixture.write_rows(|store| store.step_memory().list_timed_waits(10)).is_empty(), "nothing waits for a time any more");
}

#[test]
fn a_refresh_wakes_the_plan_and_asks_for_a_cycle_and_the_network_back_wakes_the_live_socket_too() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut started = start_over_kept_parts(&fixture, make_start_input(&fixture, graph.client())).await;
        // A wake and a poll of the test's own in place of the tasks': what the two calls give them is read here, and by no task.
        let sync_plan_wake = Arc::new(SyncPlanWake::new());
        let (poll, asked_cycle_count) = ListingPollHandle::for_tests();
        started.tasks.sync_plan_wake = Arc::clone(&sync_plan_wake);
        started.tasks.poll = poll;

        started.tasks.refresh();

        assert_eq!(sync_plan_wake.wait().await, BTreeSet::from([WakeCause::RefreshAsked]), "the task makes the next tries due on this cause");
        assert_eq!(asked_cycle_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        started.tasks.wake_for_network();

        assert_eq!(sync_plan_wake.wait().await, BTreeSet::from([WakeCause::SendingHoldChanged]), "no next try is made due for the network");
        assert_eq!(asked_cycle_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        stop(started).await;
    });
}

#[test]
fn a_folder_taken_down_has_no_mount_and_a_closed_store() {
    let fixture = ItemFixture::new();
    fixture.make_own_file("own.txt", b"own");
    let directory = tempfile::tempdir().unwrap();
    let kept = make_kept_parts_with_own_store(&fixture, &directory);
    let (mount, store_file) = (Arc::downgrade(&kept.mount), directory.path().join("own.db"));
    assert_eq!(mode_of(&fixture.folder()), 0o755, "a built mount gave the write bits back");

    let outcome = fixture.runtime().block_on(take_folder_down(kept));

    assert_eq!(outcome, TakeDownOutcome::StoreClosed);
    assert!(mount.upgrade().is_none(), "nothing holds the mount");
    assert_eq!((mode_of(&fixture.folder()), mode_of(&fixture.disk_path("own.txt"))), (0o555, 0o444), "nothing is put into the folder while no mount lies over it");
    // Closed: the next start opens it as the store that was kept.
    assert_eq!(TreeStore::open_with_origin(&store_file).unwrap().1, konedrive_tree::StoreOrigin::Kept);
}

#[test]
fn a_folder_forgotten_has_its_write_bits_and_a_closed_store() {
    let fixture = ItemFixture::new();
    fixture.make_own_file("own.txt", b"own");
    let directory = tempfile::tempdir().unwrap();
    let kept = make_kept_parts_with_own_store(&fixture, &directory);
    let mount = Arc::downgrade(&kept.mount);

    let outcome = fixture.runtime().block_on(forget_folder(kept));

    assert_eq!(outcome, TakeDownOutcome::StoreClosed);
    assert!(mount.upgrade().is_none());
    assert_eq!((mode_of(&fixture.folder()), mode_of(&fixture.disk_path("own.txt"))), (0o755, 0o644), "the folder is the user's own again");
}

#[test]
fn a_store_somebody_still_holds_after_the_wait_is_answered() {
    let fixture = ItemFixture::new();
    // The fixture's own store: the fixture holds a clone of it past the take-down.
    let kept = make_kept_parts(&fixture);

    let (outcome, waited) = block_on_paused_clock(async {
        let began = tokio::time::Instant::now();
        (take_folder_down(kept).await, began.elapsed())
    });

    assert_eq!(outcome, TakeDownOutcome::StoreStillOpen);
    assert_eq!(waited, STORE_CLOSES_WITHIN, "the wait is bounded");
    assert_eq!(mode_of(&fixture.folder()), 0o555, "the mount was stopped all the same");
    assert!(fixture.read_rows(|items| items.count_unsent()).count == 0, "and the store still answers its holder");
}

#[test]
fn the_daemons_end_stops_the_mount_and_closes_nothing() {
    let fixture = ItemFixture::new();
    let kept = make_kept_parts(&fixture);
    let mount = Arc::downgrade(&kept.mount);

    fixture.runtime().block_on(end_kept_folder_at_daemon_stop(kept));

    assert!(mount.upgrade().is_none(), "the parts are dropped");
    assert_eq!(mode_of(&fixture.folder()), 0o555, "the folder is left without its write bits");
    assert!(fixture.take_downs.lock().unwrap().is_empty(), "and no trouble is said");
}

#[test]
fn a_folder_forgotten_with_no_parts_kept_has_its_write_bits() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    fs::create_dir_all(folder.join("docs")).unwrap();
    fs::write(folder.join("docs/a.txt"), b"a").unwrap();
    // As a take-down or the daemon's end left it.
    for (path, mode) in [("docs/a.txt", 0o444), ("docs", 0o555), ("", 0o555)] {
        fs::set_permissions(folder.join(path), fs::Permissions::from_mode(mode)).unwrap();
    }
    let root = SyncRoot { path: folder.clone(), root_id: "R".into() };

    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(forget_folder_without_mount(&root));

    assert_eq!((mode_of(&folder), mode_of(&folder.join("docs")), mode_of(&folder.join("docs/a.txt"))), (0o755, 0o755, 0o644));
}

#[test]
fn a_closed_store_that_is_not_there_or_not_of_this_version_counts_as_nothing_unsent() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    // No file.
    assert_eq!(count_unsent_of_closed_store(&directory.path().join("none.db")).unwrap(), UnsentTotal::default());
    assert!(!directory.path().join("none.db").exists(), "and none is made by the question");

    // A store of this version says what it holds: one file made here, never sent.
    fixture.make_own_file("own.txt", b"own");
    assert_eq!(count_unsent_of_closed_store(&fixture.store_path).unwrap(), UnsentTotal { count: 1, bytes: 3 });

    // The same store as an older version's: a sync would rebuild it empty.
    let old = directory.path().join("old.db");
    fs::copy(&fixture.store_path, &old).unwrap();
    for journal in ["-wal", "-shm"] {
        let beside = std::path::PathBuf::from(format!("{}{journal}", fixture.store_path.display()));
        if beside.exists() {
            fs::copy(&beside, format!("{}{journal}", old.display())).unwrap();
        }
    }
    rusqlite::Connection::open(&old).unwrap().execute("UPDATE meta SET value = '19' WHERE key = 'schema_version'", []).unwrap();
    assert_eq!(count_unsent_of_closed_store(&old).unwrap(), UnsentTotal::default());

    // No database at all.
    let notes = directory.path().join("notes.txt");
    fs::write(&notes, vec![b'x'; 4096]).unwrap();
    assert_eq!(count_unsent_of_closed_store(&notes).unwrap(), UnsentTotal::default());
}

#[test]
fn a_closed_store_that_cannot_be_read_is_an_error_and_not_nothing_unsent() {
    let directory = tempfile::tempdir().unwrap();
    // A directory where the store's file should be.
    assert!(count_unsent_of_closed_store(directory.path()).is_err());
}

//! The stages of a folder's start: the pure decisions as tables, each stage's own function
//! over the fixture's store and the fake OneDrive, stages 5 to 7 over parts whose mount is
//! built and not served, and `start_folder_sync` with kept parts and up to its refusals of
//! stages 1 and 2. Past stage 3 with nothing kept the start mounts: the VM suite's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use konedrive_tree::{RebuildLoss, TreeStore};

use super::*;
use crate::mount::item_fixture::{ItemFixture, DRIVE_ID};
use crate::status::snapshot::ScanState;
use crate::sync_parts::running_steps::StepClass;
use crate::sync_parts::sending_holds::SendingHold;
use crate::sync_tasks::tests::{block_on_paused_clock, make_kept_parts, make_start_input, stop, wait_until};

fn drive(id: &str) -> DriveId {
    DriveId::new(id).expect("a drive id")
}

fn findings(bound_by_mark_count: usize) -> StartCheckFindings {
    StartCheckFindings { held_items: Default::default(), has_unread_part: false, unresolved_path_count: 0, bound_by_mark_count }
}

/// A store of the older version `version` at `path`, whose old table of upload sessions
/// holds `urls`.
fn make_old_store_with_sessions(path: &Path, version: &str, urls: &[String]) {
    drop(TreeStore::open(path).expect("a store"));
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute("UPDATE meta SET value = ?1 WHERE key = 'schema_version'", [version]).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS upload_sessions (url TEXT PRIMARY KEY, parent TEXT, name TEXT, opened INTEGER NOT NULL);").unwrap();
    for url in urls {
        conn.execute("INSERT INTO upload_sessions (url, opened) VALUES (?1, 0)", [url]).unwrap();
    }
}

#[test]
fn the_drive_for_the_check_is_the_stores_else_the_recorded_else_the_accounts() {
    let (store, recorded, account) = (Some(drive("store")), Some(drive("recorded")), Some(drive("account")));
    let cases = [
        (store.clone(), recorded.clone(), account.clone(), CheckDrive::Known(drive("store"))),
        (store.clone(), None, None, CheckDrive::Known(drive("store"))),
        (None, recorded.clone(), account.clone(), CheckDrive::Known(drive("recorded"))),
        (None, recorded.clone(), None, CheckDrive::Known(drive("recorded"))),
        (None, None, account.clone(), CheckDrive::Known(drive("account"))),
        (None, None, None, CheckDrive::NotKnownYet),
    ];
    for (store_drive, recorded_drive, account_drive, decided) in cases {
        assert_eq!(decide_check_drive(store_drive.clone(), recorded_drive.clone(), account_drive.clone()), decided, "{store_drive:?} {recorded_drive:?} {account_drive:?}");
    }
}

#[test]
fn a_full_listing_is_asked_for_only_where_the_check_bound_entries_of_a_kept_store_by_their_marks() {
    let made_empty = StoreOrigin::MadeEmpty { loss: None };
    let rebuilt = StoreOrigin::MadeEmpty { loss: Some(RebuildLoss::default()) };
    let cases = [
        // A store that was kept asks for no full listing.
        (StoreOrigin::Kept, 0, None),
        // Entries bound by their marks ask for one.
        (StoreOrigin::Kept, 1, Some(FullListingCause::BoundByMarkAtStart)),
        (StoreOrigin::Kept, 40, Some(FullListingCause::BoundByMarkAtStart)),
        // A store made empty lists in full by itself, and is asked nothing.
        (made_empty.clone(), 0, None),
        (made_empty, 3, None),
        (rebuilt, 3, None),
    ];
    for (origin, bound_by_mark_count, decided) in cases {
        assert_eq!(decide_first_listing(&origin, &findings(bound_by_mark_count)), decided, "{origin:?} with {bound_by_mark_count} bound by marks");
    }
}

#[test]
fn the_check_at_start_is_said_as_a_scan_that_runs_and_then_as_one_that_finished() {
    let last = LocalScan { state: ScanState::Idle, reason: "start".into(), started: 10, finished: 14, took: 4 };

    let running = describe_running_check(&last, 100);

    assert_eq!(running, LocalScan { state: ScanState::Running, reason: "start".into(), started: 100, finished: 14, took: 4 }, "the last one's end stays while this one runs");
    assert_eq!(describe_finished_check(&running, 107), LocalScan { state: ScanState::Idle, finished: 107, took: 7, ..running.clone() });
    // A clock that went back gives no length, and no panic.
    assert_eq!(describe_finished_check(&running, 90).took, 0);
    // The first check of a run.
    assert_eq!(describe_running_check(&LocalScan::default(), 5).state, ScanState::Running);
}

#[test]
fn the_folders_store_is_made_where_there_is_none_kept_where_there_is_one_and_refused_where_it_cannot_be_opened() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tree.db");
    fixture.runtime().block_on(async {
        let (store, origin) = open_folder_store(&path).await.expect("a store is made");
        assert_eq!(origin, StoreOrigin::MadeEmpty { loss: None });
        store.close().await;

        let (store, origin) = open_folder_store(&path).await.expect("the store is opened");
        assert_eq!(origin, StoreOrigin::Kept);
        store.close().await;

        // A directory where the file should be: a failure that is no finding of a version.
        let refused = open_folder_store(directory.path()).await.err().expect("no store is opened");
        assert!(matches!(refused, StartRefusal::Store(_)), "{refused:?}");
    });
}

#[test]
fn entries_bound_by_their_marks_ask_the_store_for_a_full_listing_and_nothing_else_does() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        request_first_listing(&fixture.store, &StoreOrigin::Kept, &findings(0)).await.unwrap();
        request_first_listing(&fixture.store, &StoreOrigin::MadeEmpty { loss: None }, &findings(2)).await.unwrap();
        assert!(!fixture.read_rows(|items| items.is_full_listing_requested()), "nothing was asked for");

        request_first_listing(&fixture.store, &StoreOrigin::Kept, &findings(2)).await.unwrap();

        assert!(fixture.read_rows(|items| items.is_full_listing_requested()));
    });
}

#[test]
fn a_quota_read_at_the_start_says_whether_onedrive_is_full_before_the_first_plan() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let space = OneDriveSpace::new(Quota::detached());

        graph.with(|cloud| cloud.free = Some(64 << 20));
        read_quota_before_first_plan(&graph.client(), &space, &fixture.store, 100).await;
        assert!(!space.is_full());

        graph.with(|cloud| cloud.free = Some(0));
        read_quota_before_first_plan(&graph.client(), &space, &fixture.store, 200).await;
        assert!(space.is_full(), "the first plan sends no content");
        assert_eq!(graph.with(|cloud| cloud.quota_reads()), 2, "one request for each read");
    });
}

#[test]
fn the_upload_sessions_of_a_store_of_an_older_version_are_cancelled_before_it_is_removed() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tree.db");
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let sessions: Vec<String> = ["one.bin", "two.bin"].iter().map(|name| format!("{}/upload/{}", graph.server.uri(), graph.with(|cloud| cloud.open_elsewhere("R", name, 0)))).collect();
        assert_eq!(graph.with(|cloud| cloud.open_sessions()), 2);
        make_old_store_with_sessions(&path, "19", &sessions);

        cancel_upload_sessions_of_old_store(&path, &graph.client()).await;

        assert_eq!(graph.with(|cloud| cloud.open_sessions()), 0, "their empty files hold no name any more");
        assert!(path.exists(), "the store is removed by the next stage, not by this one");
        // The next stage rebuilds it, and a second start finds nothing to cancel.
        let (store, origin) = open_folder_store(&path).await.unwrap();
        assert!(matches!(origin, StoreOrigin::MadeEmpty { loss: Some(_) }), "{origin:?}");
        store.close().await;
        let asked = graph.with(|cloud| cloud.count("DELETE", "upload"));
        cancel_upload_sessions_of_old_store(&path, &graph.client()).await;
        assert_eq!(graph.with(|cloud| cloud.count("DELETE", "upload")), asked);
    });
}

#[test]
fn the_sessions_of_a_store_of_todays_version_and_of_no_store_are_not_cancelled() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let session = format!("{}/upload/{}", graph.server.uri(), graph.with(|cloud| cloud.open_elsewhere("R", "kept.bin", 0)));
        let todays = directory.path().join("todays.db");
        make_old_store_with_sessions(&todays, konedrive_tree::SCHEMA_VERSION, &[session]);

        cancel_upload_sessions_of_old_store(&todays, &graph.client()).await;
        cancel_upload_sessions_of_old_store(&directory.path().join("none.db"), &graph.client()).await;

        assert_eq!(graph.with(|cloud| cloud.open_sessions()), 1, "a store that is kept goes on with its sessions");
        assert_eq!(graph.with(|cloud| cloud.count("DELETE", "upload")), 0);
    });
}

#[test]
fn a_onedrive_that_does_not_answer_holds_the_start_for_the_one_bound_and_no_longer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tree.db");
    block_on_paused_clock(async {
        let graph = crate::fake_onedrive::FakeGraph::start().await;
        let sessions: Vec<String> = ["one.bin", "two.bin", "three.bin"].iter().map(|name| format!("{}/upload/{}", graph.server.uri(), graph.with(|cloud| cloud.open_elsewhere("R", name, 0)))).collect();
        make_old_store_with_sessions(&path, "19", &sessions);
        // Each cancel stands far longer than the bound.
        graph.with(|cloud| cloud.delay("DELETE", "upload", OLD_SESSIONS_CANCELLED_WITHIN * 100, 3));
        let began = tokio::time::Instant::now();

        cancel_upload_sessions_of_old_store(&path, &graph.client()).await;

        assert_eq!(began.elapsed(), OLD_SESSIONS_CANCELLED_WITHIN, "all three were waited for together, once");
    });
}

#[test]
fn the_holds_that_stand_at_the_start_are_set_before_the_first_plan() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        input.standing_holds = StandingSendingHolds { paused_until: Some(None), is_signed_out: true, writes_refusal: Some("the sign-in grants no writes".into()) };

        let started = start_sync_over_parts(&input, make_kept_parts(&fixture), StoreOrigin::Kept).await.ok().expect("the start goes through");

        let (holds, now) = (started.tasks.sending_holds(), (fixture.clock())());
        assert_eq!(holds.find(now), Some(SendingHold::SignedOut), "the first that holds is shown");
        assert!(holds.is_held(StepClass::Download, now) && holds.is_held(StepClass::Content, now));
        // Each stands by itself: with the sign-out ended, the refusal is the one shown; then the pause.
        holds.set_signed_out(false);
        assert_eq!(holds.find(now), Some(SendingHold::WritesNotAllowed { reason: "the sign-in grants no writes".into() }));
        holds.set_writes_refusal(None);
        assert_eq!(holds.find(now), Some(SendingHold::Paused { until: None }));
        stop(started).await;
    });
}

#[test]
fn the_plans_task_is_started_in_a_read_only_folder_too_and_the_access_mode_is_applied_before_its_first_plan() {
    let fixture = ItemFixture::new();
    fixture.make_own_file("unsent.txt", b"unsent");
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        input.wanted = Mode::ReadOnly;
        // The plan's task says how much is unsent after every plan it published.
        let (plans, unsent) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (counted_plans, counted_unsent) = (Arc::clone(&plans), Arc::clone(&unsent));
        input.unsent_count_report = Arc::new(move |count| {
            counted_unsent.store(count, Ordering::SeqCst);
            counted_plans.fetch_add(1, Ordering::SeqCst);
        });

        let started = start_sync_over_parts(&input, make_kept_parts(&fixture), StoreOrigin::Kept).await.ok().expect("the start goes through");

        assert!(!started.mount.access_mode_switch().is_read_write());
        assert!(started.tasks.sending_holds().is_held(StepClass::Content, (fixture.clock())()), "a read-only folder sends nothing");
        wait_until("the plan's task published a plan", || plans.load(Ordering::SeqCst) > 0).await;
        assert_eq!(unsent.load(Ordering::SeqCst), 1, "what is unsent stays here, counted");
        wait_until("the first listing ended", || graph.with(|cloud| cloud.count("GET", "root/delta")) >= 1).await;
        stop(started).await;
        assert_eq!(graph.with(|cloud| cloud.at("unsent.txt").is_some()), false, "and nothing was sent");
    });
}

#[test]
fn a_start_over_parts_answers_them_with_its_tasks_and_the_origin_it_was_given() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let input = make_start_input(&fixture, graph.client());
        let parts = make_kept_parts(&fixture);
        let mount = Arc::clone(&parts.mount);
        let origin = StoreOrigin::MadeEmpty { loss: Some(RebuildLoss::default()) };

        let started = start_sync_over_parts(&input, parts, origin.clone()).await.ok().expect("the start goes through");

        assert!(Arc::ptr_eq(&started.mount, &mount) && Arc::ptr_eq(&started.sync_plan_wake, &fixture.sync_plan_wake));
        assert_eq!((started.store_origin.clone(), started.drive_id.clone()), (origin, drive(DRIVE_ID)));
        assert!(started.mount.access_mode_switch().is_read_write(), "the wanted access mode is the mount's");
        assert_eq!(graph.with(|cloud| cloud.quota_reads()), 1, "the quota was read once, before the tasks");
        stop(started).await;
    });
}

#[test]
fn the_parts_are_given_back_by_a_start_over_them_that_is_refused() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        input.wanted = Mode::ReadOnly;
        let parts = make_kept_parts(&fixture);
        let mount = Arc::clone(&parts.mount);
        // Its write-operation thread is gone: the switch of stage 5 says so.
        mount.stop();

        let (refusal, parts) = start_sync_over_parts(&input, parts, StoreOrigin::Kept).await.err().expect("the start is refused");

        assert!(matches!(refusal, StartRefusal::Mount(_)), "{refusal:?}");
        assert!(Arc::ptr_eq(&parts.mount, &mount) && Arc::ptr_eq(&parts.running_steps, &fixture.running_steps));
        assert!(parts.store.call(|store| store.drive_id()).await.is_ok(), "the store is open");
        assert_eq!(graph.with(|cloud| cloud.quota_reads()), 0, "no later stage ran");
    });
}

#[test]
fn a_folder_that_keeps_its_parts_opens_no_store_runs_no_check_builds_no_mount_and_asks_for_no_full_listing() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        // Where no store can be opened, and with no drive known but the parts' own.
        input.tree_db = directory.path().to_path_buf();
        let parts = make_kept_parts(&fixture);
        let mount = Arc::clone(&parts.mount);
        input.kept = Some(parts);
        let state = input.state.clone();

        let started = start_folder_sync(input).await.ok().expect("the start goes through");

        assert!(Arc::ptr_eq(&started.mount, &mount), "the kept mount");
        assert_eq!(started.store_origin, StoreOrigin::Kept);
        assert_eq!(started.drive_id, drive(DRIVE_ID));
        assert_eq!(state.get().local.scan.state, ScanState::None, "no check ran");
        assert!(!fixture.read_rows(|items| items.is_full_listing_requested()));
        stop(started).await;
    });
}

#[test]
fn a_refused_start_gives_back_the_parts_it_was_given_as_kept() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        input.wanted = Mode::ReadOnly;
        let parts = make_kept_parts(&fixture);
        let mount = Arc::clone(&parts.mount);
        mount.stop();
        input.kept = Some(parts);

        let refused = start_folder_sync(input).await.err().expect("the start is refused");

        assert!(matches!(refused.refusal, StartRefusal::Mount(_)), "{:?}", refused.refusal);
        assert_eq!(refused.rebuild_loss, None);
        let kept = refused.kept.expect("the kept parts come back");
        assert!(Arc::ptr_eq(&kept.mount, &mount));
        assert!(kept.store.call(|store| store.drive_id()).await.is_ok(), "the store was not dropped");
    });
}

#[test]
fn a_start_whose_store_cannot_be_opened_is_refused_with_nothing_left() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        input.tree_db = directory.path().to_path_buf();
        let state = input.state.clone();

        let refused = start_folder_sync(input).await.err().expect("the start is refused");

        assert!(matches!(refused.refusal, StartRefusal::Store(_)), "{:?}", refused.refusal);
        assert!(refused.kept.is_none() && refused.rebuild_loss.is_none());
        assert_eq!(state.get().local.scan.state, ScanState::None, "no check ran: no mount was built");
    });
}

#[test]
fn a_folder_whose_drive_is_not_known_is_not_mounted() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tree.db");
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        // A store that is made now knows no drive, and neither does anybody else.
        input.tree_db = path.clone();
        let state = input.state.clone();

        let refused = start_folder_sync(input).await.err().expect("the start is refused");

        assert!(matches!(refused.refusal, StartRefusal::DriveNotKnown), "{:?}", refused.refusal);
        assert!(refused.kept.is_none() && refused.rebuild_loss.is_none(), "a store that was made lost nothing");
        assert_eq!(state.get().local.scan.state, ScanState::None, "no check ran: no mount was built");
        // The store of the refused start was let go of: it closes for whoever opens it next.
        let (store, origin) = open_folder_store(&path).await.unwrap();
        assert_eq!(origin, StoreOrigin::Kept, "it was made by the refused start");
        tokio::time::timeout(std::time::Duration::from_secs(10), store.close()).await.expect("nobody else holds it");
    });
}

#[test]
fn a_refused_start_says_what_the_rebuild_of_its_store_lost() {
    let fixture = ItemFixture::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tree.db");
    make_old_store_with_sessions(&path, "19", &["https://sessions.invalid/up/1".to_owned()]);
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let mut input = make_start_input(&fixture, graph.client());
        input.tree_db = path.clone();

        let refused = start_folder_sync(input).await.err().expect("the start is refused");

        // Rebuilt empty, the store knows no drive: the refusal is stage 2's, and carries stage 1's loss.
        assert!(matches!(refused.refusal, StartRefusal::DriveNotKnown), "{:?}", refused.refusal);
        assert_eq!(refused.rebuild_loss, Some(RebuildLoss { open_upload_sessions: 1, ..RebuildLoss::default() }));
        // The next start finds the new store as one that was kept, and could not say it.
        let (store, origin) = open_folder_store(&path).await.unwrap();
        assert_eq!(origin, StoreOrigin::Kept);
        store.close().await;
    });
}

#[test]
fn the_runner_of_the_steps_towards_the_folder_is_made_of_the_mounts_own_parts() {
    let fixture = ItemFixture::new();
    let listed = fixture.list_onedrive_directory("docs");
    let mount = fixture.build_mount();

    let folder_steps = make_folder_steps(mount.folder_step_parts());

    // It runs a step over the mount's rows: the directory OneDrive has is shown.
    assert!(fixture.find_item(listed.local_item_id).unwrap().local.is_none());
    let outcomes = fixture.runtime().block_on(folder_steps.run_show_items(vec![fixture.versions_of(listed.local_item_id)]));
    assert_eq!(outcomes.len(), 1);
    assert!(fixture.find_item(listed.local_item_id).unwrap().local.is_some(), "{outcomes:?}");
}

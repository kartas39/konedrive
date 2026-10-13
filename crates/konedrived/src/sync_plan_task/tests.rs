//! What the plan's task publishes and announces, over the mount's fixture with its
//! stand-in for the runners, its poll handle and its clock: the queue's status after a
//! plan, after a settled round and at the tick, and what a settlement wrote and changed.

use std::sync::Mutex;

use konedrive_tree::{ActivityKind, ActivityRow, ConflictRow, PinnedDownloadTotal};

use super::*;
use crate::mount::item_fixture::ItemFixture;
use crate::onedrive_steps::lost_answers::LostAnswers;
use crate::status::snapshot::{DiskSpaceShortage, UploadQueueStatus, SyncSnapshot};

/// A call of the task's announcer of marks: the items, and whether a step ran for any of
/// them when it was called.
type MarksCall = (Vec<LocalItemId>, bool);

/// A plan's task over the fixture, and what it tells that a test reads.
struct TaskFixture {
    fixture: ItemFixture,
    task: Arc<SyncPlanTask>,
    state: SyncStateHandle,
    marks_calls: Arc<Mutex<Vec<MarksCall>>>,
    /// The unsent counts the task told, in order; the last is watched.
    unsent_counts: Arc<Mutex<Vec<u64>>>,
    last_unsent_count: tokio::sync::watch::Sender<Option<u64>>,
    after_folder_steps: Arc<tokio::sync::Notify>,
    /// How often the task asked its reader of the quota.
    quota_read_count: Arc<AtomicUsize>,
    /// What the task's reader of the free space answers, in bytes: plenty, until a test
    /// says otherwise.
    free_space: Arc<Mutex<Option<u64>>>,
    /// Kept for as long as the task: the fake OneDrive the cancellations of sessions ask.
    _fake_onedrive: crate::fake_onedrive::FakeGraph,
}

impl TaskFixture {
    /// The task, not running: a test calls what it publishes by, or runs its loop.
    fn new() -> Self {
        let fixture = ItemFixture::new();
        let fake_onedrive = fixture.runtime().block_on(fixture.start_fake_onedrive());
        let graph = fixture.graph_access(&fake_onedrive);
        let mut snapshot = SyncSnapshot::default();
        snapshot.folder.root_path = fixture.folder().display().to_string();
        let state = SyncStateHandle::new(snapshot);
        let (marks_calls, unsent_counts, after_folder_steps) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())), Arc::new(tokio::sync::Notify::new()));
        let marks_announcer = {
            let (marks_calls, running_steps) = (Arc::clone(&marks_calls), Arc::clone(&fixture.running_steps));
            Arc::new(move |items: Vec<LocalItemId>| {
                let is_step_running = items.iter().any(|item| running_steps.has_running_step(*item));
                marks_calls.lock().unwrap().push((items, is_step_running));
            })
        };
        let last_unsent_count = tokio::sync::watch::Sender::new(None);
        let quota_read_count = Arc::new(AtomicUsize::new(0));
        let unsent_count_report = {
            let (unsent_counts, last_unsent_count) = (Arc::clone(&unsent_counts), last_unsent_count.clone());
            Arc::new(move |unsent| {
                unsent_counts.lock().unwrap().push(unsent);
                last_unsent_count.send_replace(Some(unsent));
            })
        };
        let free_space = Arc::new(Mutex::new(Some(u64::MAX)));
        let task = SyncPlanTask::new(SyncPlanTaskParts {
            store: fixture.store.clone(),
            sync_plan_wake: Arc::clone(&fixture.sync_plan_wake),
            step_runners: fixture.step_runners_stand_in(),
            session_cancellations: SessionCancellations::new(graph.clone(), LostAnswers::new(graph, Arc::clone(&fixture.tree_lock)), Arc::clone(&fixture.running_steps)),
            running_steps: Arc::clone(&fixture.running_steps),
            sending_holds: Arc::clone(&fixture.sending_holds),
            onedrive_space: Arc::clone(&fixture.onedrive_space),
            published_plan: Arc::clone(&fixture.published_plan),
            poll: fixture.poll_handle().0,
            pool: Arc::clone(&fixture.pool),
            state: state.clone(),
            settings: PlanSettings { computer_name: "here".into(), quiet_time: 0, items_read_at_once: 100 },
            start_check: StartCheckFindings { held_items: BTreeSet::new(), has_unread_part: false, unresolved_path_count: 0, bound_by_mark_count: 0 },
            folder: fixture.folder(),
            clock: fixture.clock(),
            quota_reader: {
                let quota_read_count = Arc::clone(&quota_read_count);
                Arc::new(move || {
                    quota_read_count.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { None })
                })
            },
            activity: Arc::new(Activity::new(state.clone())),
            marks_announcer,
            after_folder_steps: Some(Arc::clone(&after_folder_steps)),
            unsent_count_report,
            free_space_reader: {
                let free_space = Arc::clone(&free_space);
                Arc::new(move || *free_space.lock().unwrap())
            },
            free_space_reserve_reader: Arc::new(|| disk_space::count_reserve_bytes(10)),
        });
        TaskFixture { fixture, task, state, marks_calls, unsent_counts, last_unsent_count, after_folder_steps, quota_read_count, free_space, _fake_onedrive: fake_onedrive }
    }

    fn upload_queue(&self) -> UploadQueueStatus {
        self.state.get().upload_queue
    }

    /// Waits until what the task published says `is_reached`: the state is watched, no
    /// time is waited for. A minute bounds a test that would hang.
    async fn wait_for_state(&self, what: &str, is_reached: impl Fn(&SyncSnapshot) -> bool) {
        let mut published = self.state.subscribe();
        let reached = tokio::time::timeout(Duration::from_secs(60), published.wait_for(|snapshot| is_reached(snapshot))).await;
        assert!(reached.is_ok_and(|reached| reached.is_ok()), "the task never published {what}: {:?}", self.state.get());
    }

    fn running_step(item: LocalItemId, class: StepClass, sent: u64, total: u64) -> RunningStep {
        RunningStep { class, item, other_items: Vec::new(), destination: None, sent, total }
    }
}

#[test]
fn the_progress_tick_comes_a_second_on_while_a_step_sends_content_and_at_no_other_time() {
    let step = |class| TaskFixture::running_step(LocalItemId::new(5), class, 0, 0);
    assert_eq!(decide_progress_tick(&[], 100), None, "nothing runs");
    assert_eq!(decide_progress_tick(&[step(StepClass::Content)], 100), Some(101));
    for class in [StepClass::Metadata, StepClass::Download, StepClass::Folder, StepClass::Row] {
        assert_eq!(decide_progress_tick(&[step(class)], 100), None, "{class:?} has no bytes to publish");
        assert_eq!(decide_progress_tick(&[step(class), step(StepClass::Content)], 100), Some(101), "{class:?} beside an upload");
    }
    assert_eq!(decide_progress_tick(&[step(StepClass::Content)], i64::MAX), Some(i64::MAX), "the last second there is");
}

#[test]
fn the_queues_status_is_published_from_the_rows_the_answers_and_the_running_uploads() {
    let task_fixture = TaskFixture::new();
    let (fixture, task) = (&task_fixture.fixture, &task_fixture.task);
    let run = fixture.runtime();
    let first = fixture.make_own_file("first.txt", b"12345").local_item_id;
    assert_eq!(task_fixture.upload_queue(), UploadQueueStatus::default(), "nothing is published before the task publishes");

    run.block_on(task.publish_queue_status(true));
    let published = task_fixture.upload_queue();
    assert_eq!((published.pending_count, published.pending_bytes, published.uploads.clone()), (1, 5, vec![]), "the store's count, with no plan made yet");

    // An upload runs: its bytes are published under the file's full path.
    assert!(fixture.running_steps.begin(TaskFixture::running_step(first, StepClass::Content, 2, 5)));
    run.block_on(task.publish_queue_status(true));
    assert_eq!(task_fixture.upload_queue().uploads, [(fixture.disk_path("first.txt").display().to_string(), 2, 5)]);

    // The tick: the bytes of the uploads alone, the counts as they were last read.
    fixture.make_own_file("second.txt", b"123");
    fixture.running_steps.tell_progress(first, 4, 5);
    run.block_on(task.publish_queue_status(false));
    let published = task_fixture.upload_queue();
    assert_eq!((published.pending_count, published.pending_bytes), (1, 5), "no count is read at the tick");
    assert_eq!(published.uploads, [(fixture.disk_path("first.txt").display().to_string(), 4, 5)]);
    // The next round that reads the counts says the file made since.
    fixture.running_steps.end(first);
    run.block_on(task.publish_queue_status(true));
    let published = task_fixture.upload_queue();
    assert_eq!((published.pending_count, published.pending_bytes, published.uploads), (2, 8, vec![]));

    // Whether OneDrive is full is published with them.
    assert!(!task_fixture.upload_queue().quota_full);
    fixture.onedrive_space.apply_quota(&DriveQuota { total: 10, used: 10, remaining: Some(0), state: "exceeded".into() }, (task.parts.clock)());
    run.block_on(task.publish_queue_status(false));
    assert!(task_fixture.upload_queue().quota_full);
}

#[test]
fn the_status_is_published_after_a_plan_and_after_a_settled_round_and_the_marks_once_the_step_is_ended() {
    let task_fixture = TaskFixture::new();
    let (fixture, task) = (&task_fixture.fixture, &task_fixture.task);
    let own = fixture.make_own_file("own.txt", b"12345").local_item_id;
    let listed = fixture.list_onedrive_file("listed.txt", b"in OneDrive").local_item_id;
    let stop = CancellationToken::new();
    fixture.runtime().block_on(async {
        let running = tokio::spawn(Arc::clone(task).run(stop.clone()));
        // The first listing of this run has not ended: the plan is made and published, and nothing is sent.
        task_fixture.wait_for_state("the unsent file after the first plan", |snapshot| snapshot.upload_queue.pending_count == 1 && snapshot.upload_queue.pending_bytes == 5).await;
        assert_eq!(fixture.find_item(own).unwrap().onedrive_item, None, "sending is held: the file is not sent");
        assert_eq!(task_fixture.unsent_counts.lock().unwrap().first(), Some(&1), "the unsent count of the plan's reading is told with every plan");
        // The listing ended: the file is sent, its step settled, and the round's status says nothing is unsent.
        fixture.sending_holds.set_listing_ended();
        task_fixture.wait_for_state("an empty queue after the settled round", |snapshot| snapshot.upload_queue.pending_count == 0 && snapshot.cycle.items_placed == 2).await;
        // And the plan made after that round tells that nothing is unsent.
        let mut told = task_fixture.last_unsent_count.subscribe();
        let told_nothing_unsent = tokio::time::timeout(Duration::from_secs(60), told.wait_for(|unsent| *unsent == Some(0))).await;
        assert!(told_nothing_unsent.is_ok_and(|told| told.is_ok()), "the task never told that nothing is unsent: {:?}", task_fixture.unsent_counts.lock().unwrap());
        stop.cancel();
        running.await.expect("the loop returns at the stop");
    });
    assert!(fixture.find_item(own).unwrap().onedrive_item.is_some(), "the file was sent");
    assert!(fixture.find_item(listed).unwrap().local.is_some(), "the listed file is shown");
    assert_eq!(task_fixture.upload_queue(), UploadQueueStatus::default(), "a stopped sync shows no count of its own");
    assert_eq!(task_fixture.unsent_counts.lock().unwrap().last(), Some(&0));

    // The marks of the sent file were announced when its step towards OneDrive began, while it ran, and when it was settled, once it ran no more.
    let marks_calls: Vec<MarksCall> = task_fixture.marks_calls.lock().unwrap().iter().filter(|(items, _)| items.contains(&own)).cloned().collect();
    assert_eq!(marks_calls.first(), Some(&(vec![own], true)), "at its start the step runs: {marks_calls:?}");
    assert_eq!(marks_calls.last(), Some(&(vec![own], false)), "at its settlement the step is ended: {marks_calls:?}");
    assert!(task_fixture.marks_calls.lock().unwrap().iter().all(|(items, _)| !items.contains(&listed)), "a step towards the folder changes no sync mark");
    // The step towards the folder that showed the listed file nudged the thumbnail filler.
    assert!(futures_util::FutureExt::now_or_never(task_fixture.after_folder_steps.notified()).is_some());
}

/// What is left to download of the pinned files is published with the plan whose reading
/// counted them: all of them as soon as they are pinned, before any step ran, and none
/// once the content landed. A stopped sync shows none.
#[test]
fn the_pinned_files_to_download_are_published_with_the_plan_that_counted_them() {
    let task_fixture = TaskFixture::new();
    let (fixture, task) = (&task_fixture.fixture, &task_fixture.task);
    let pinned_download = || task_fixture.state.get().transfers.pinned_download;
    let plan_and_publish = || {
        fixture.runtime().block_on(async {
            let round = task.make_plan(task.gather_daemon_facts()).await.expect("the store is read");
            task.publish_round(&round, None).await;
        })
    };
    let directory = fixture.show_onedrive_directory("kept");
    let first = fixture.show_onedrive_file("kept/a.txt", b"theirs").local_item_id;
    let second = fixture.show_onedrive_file("kept/b.txt", b"theirs too").local_item_id;
    plan_and_publish();
    assert_eq!(pinned_download(), PinnedDownloadTotal::default(), "nothing is pinned");

    let onedrive_id = directory.onedrive_item.as_ref().expect("bound").item_id.as_str().to_owned();
    fixture.write_rows(move |store| store.set_pin(&onedrive_id, true));
    task.missing_pinned_files.note_wake_causes(&[WakeCause::UserAnswered].into());
    plan_and_publish();
    assert_eq!(pinned_download(), PinnedDownloadTotal { count: 2, bytes: 16 }, "both, though no step ran");

    fixture.write_rows(move |store| store.item_store().set_content_presence(first, konedrive_tree::ContentPresence::OnDisk));
    plan_and_publish();
    assert_eq!(pinned_download(), PinnedDownloadTotal { count: 1, bytes: 10 }, "one landed");
    fixture.write_rows(move |store| store.item_store().set_content_presence(second, konedrive_tree::ContentPresence::OnDisk));
    plan_and_publish();
    assert_eq!(pinned_download(), PinnedDownloadTotal::default());
    assert!(!task.missing_pinned_files.is_reader_asked(), "and the reader is asked no more");
}

fn event(fixture: &ItemFixture, kind: ActivityKind, path: &str) -> ActivityRow {
    ActivityRow { at: 1_700_000_000, kind, path: fixture.disk_path(path).display().to_string(), detail: String::new() }
}

#[test]
fn what_a_settlement_wrote_is_announced_and_the_conflicts_counted_again_unless_the_loop_is_stopping() {
    let task_fixture = TaskFixture::new();
    let (fixture, task) = (&task_fixture.fixture, &task_fixture.task);
    let run = fixture.runtime();
    let activity = Arc::clone(&task.parts.activity);
    activity.attach(fixture.store.clone(), &fixture.folder());
    let mut announced = activity.subscribe();
    // A conflict on record whose copy is there; the count on the bus is as it was at the attach.
    let add_conflict = |name: &str| {
        fixture.write_disk_file(name, b"the copy");
        let row = ConflictRow { at: 1_700_000_000, original: fixture.disk_path("report.txt").display().to_string(), kept_copy: fixture.disk_path(name).display().to_string() };
        fixture.write_rows(move |store| store.add_conflicts(&[row]));
    };
    add_conflict("report (copy 1).txt");
    let conflict_count = || task_fixture.state.get().local.conflict_count;
    assert_eq!(conflict_count(), 0);

    // No conflict among what was written: every event is announced, in the order written, and nothing is counted.
    let (uploaded, moved) = (event(fixture, ActivityKind::Uploaded, "a.txt"), event(fixture, ActivityKind::CloudMoved, "b.txt"));
    run.block_on(task.announce_settled_steps(&[], &[uploaded.clone(), moved.clone()]));
    assert_eq!([announced.try_recv().ok(), announced.try_recv().ok(), announced.try_recv().ok()], [Some(uploaded), Some(moved), None]);
    assert_eq!(conflict_count(), 0);
    assert!(futures_util::FutureExt::now_or_never(task_fixture.after_folder_steps.notified()).is_none() && task_fixture.marks_calls.lock().unwrap().is_empty(), "no step was settled: no mark, no nudge");

    // A conflict was written: `Conflicts.Count` says it when the call returns.
    let conflict = event(fixture, ActivityKind::Conflict, "report (copy 1).txt");
    run.block_on(task.announce_settled_steps(&[], &[conflict.clone()]));
    assert_eq!((announced.try_recv().ok(), conflict_count()), (Some(conflict.clone()), 1));

    // Once the loop is stopping the event is still announced, and the count is not asked: nothing waits for the disk at a stop.
    add_conflict("report (copy 2).txt");
    fixture.sending_holds.set_stopping();
    run.block_on(task.announce_settled_steps(&[], &[conflict.clone()]));
    assert_eq!((announced.try_recv().ok(), conflict_count()), (Some(conflict), 1));
}

/// `Folder.Refresh()` on a running sync: the task it wakes is the one that reads the quota,
/// once; a quota somebody else read and applied is not read again.
#[test]
fn a_refresh_reads_the_quota_once_and_a_quota_read_by_another_is_not_read_again() {
    let task_fixture = TaskFixture::new();
    let (task, stop) = (Arc::clone(&task_fixture.task), CancellationToken::new());
    task_fixture.fixture.runtime().block_on(async {
        task.apply_wake_causes(&[WakeCause::QuotaRead, WakeCause::StepFinished].into(), &stop).await;
        assert_eq!(task_fixture.quota_read_count.load(Ordering::SeqCst), 0);
        task.apply_wake_causes(&[WakeCause::RefreshAsked].into(), &stop).await;
        assert_eq!(task_fixture.quota_read_count.load(Ordering::SeqCst), 1);
    });
}

/// The download of a pinned file of `size` bytes, of the item `number`.
fn download_step(number: i64, size: u64) -> SyncStep {
    use konedrive_tree::{ContentPresence, ContentVersion, DriveId, ItemPlace, ItemVersion, ItemVersions, OneDriveItemId, OneDriveItemReference};
    let place = ItemPlace { parent_local_item_id: LocalItemId::new(1), name: format!("file{number}").into() };
    let content = ContentVersion::Hashed { size, mtime: 0, mtime_nsec: 0, hash: Some("h".into()), ctag: None };
    let version = ItemVersion { place: Some(place.clone()), content: Some(content.clone()) };
    SyncStep::Folder(FolderStep::DownloadContent {
        item: ItemVersions {
            local_item_id: LocalItemId::new(number),
            onedrive_item: Some(OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(format!("I{number}")) }),
            local: Some(version.clone()),
            onedrive: Some(version),
            onedrive_etag: Some("e1".to_owned()),
            synced_place: Some(place),
            synced_content: Some(content),
            content_presence: ContentPresence::NotOnDisk,
            unsent_since: None,
        },
    })
}

/// 12 GiB free and 10 kept: of the downloads of a plan the ones that fit in the two
/// beyond are left to start, the others are held and said as a trouble of the folder;
/// a download that runs takes its size of the two; with space again nothing is held
/// and nothing said.
#[test]
fn a_download_that_would_not_leave_the_free_space_kept_is_held_and_said() {
    const GB: u64 = 1 << 30;
    let task_fixture = TaskFixture::new();
    let task = &task_fixture.task;
    let planned = || vec![download_step(5, GB), download_step(6, 5 * GB), download_step(7, GB)];
    let ids = |steps: &[SyncStep]| steps.iter().map(SyncStep::local_item_id).collect::<Vec<_>>();
    let id = LocalItemId::new;

    let (to_start, held) = task.hold_downloads_for_space(planned(), &[]);
    assert_eq!((ids(&to_start), ids(&held)), (vec![id(5), id(6), id(7)], vec![]), "plenty is free");
    assert_eq!(task_fixture.state.get().transfers.disk_space_shortage, None);

    *task_fixture.free_space.lock().unwrap() = Some(12 * GB);
    let (to_start, held) = task.hold_downloads_for_space(planned(), &[]);
    assert_eq!((ids(&to_start), ids(&held)), (vec![id(5), id(7)], vec![id(6)]));
    let snapshot = task_fixture.state.get();
    assert_eq!(snapshot.transfers.disk_space_shortage, Some(DiskSpaceShortage { count: 1, bytes: 5 * GB, free_space: 12 * GB, reserve: 10 * GB }));
    let (trouble, _) = crate::status::snapshot::running_trouble(&snapshot).expect("a trouble of the folder");
    assert_eq!(trouble, "1 file(s) kept on this device wait for disk space: 5.0 GiB to download, 12.0 GiB free, 10 GiB stay free");

    let running = [TaskFixture::running_step(LocalItemId::new(5), StepClass::Download, 0, GB)];
    let (to_start, held) = task.hold_downloads_for_space(planned(), &running);
    assert_eq!((ids(&to_start), ids(&held)), (vec![id(5), id(7)], vec![id(6)]), "the one that runs is not asked, and the last still fits beside it");
    let running = [TaskFixture::running_step(LocalItemId::new(9), StepClass::Download, 0, 2 * GB)];
    let (_, held) = task.hold_downloads_for_space(planned(), &running);
    assert_eq!(ids(&held), vec![id(5), id(6), id(7)], "a download that runs takes the two");

    *task_fixture.free_space.lock().unwrap() = Some(40 * GB);
    let (_, held) = task.hold_downloads_for_space(planned(), &[]);
    assert_eq!(ids(&held), Vec::new());
    assert_eq!(task_fixture.state.get().transfers.disk_space_shortage, None);
}

#[test]
fn a_few_more_uploads_and_downloads_are_started_than_the_pool_has_slots() {
    let limits = step_choice::decide_step_limits(16);
    assert_eq!(limits, StepLimits { content: 16 + step_choice::STEPS_BEYOND_SLOTS, metadata: 1, download: 16 + step_choice::STEPS_BEYOND_SLOTS });
}

/// Moves `file_count` files through `pool` as the plan's task has its downloads run: as
/// many at once as `decide_step_limits` allows for the pool's size at the time, each
/// waiting for its slot, taking its time (each file another, by the runtime's clock, as
/// files of many sizes do), succeeding and letting the slot go.
async fn download_files(pool: &Arc<TransferPool>, file_count: usize) {
    let mut left = file_count;
    let mut running = tokio::task::JoinSet::new();
    while left > 0 || !running.is_empty() {
        while left > 0 && running.len() < step_choice::decide_step_limits(pool.size()).download {
            left -= 1;
            let pool = Arc::clone(pool);
            running.spawn(async move {
                let mut slot = pool.acquire(konedrive_graph::pool::Class::Download).await;
                tokio::time::sleep(Duration::from_millis(50 + (left as u64 * 37) % 100)).await;
                slot.succeeded();
            });
        }
        // A round of the plan: every download that ended is taken off before the next
        // ones are started.
        running.join_next().await;
        while running.try_join_next().is_some() {}
    }
}

/// With more files to download than the pool has slots, and no throttle, the pool grows
/// to its ceiling: the steps started beyond the slots wait at the pool, which is how it
/// learns of them. With no more files than slots it stays as it started.
#[tokio::test(start_paused = true)]
async fn the_pool_reaches_its_ceiling_while_there_is_work_beyond_its_slots_and_not_without() {
    use konedrive_graph::pool::{DEFAULT_CEILING, START};
    let pool = TransferPool::new(DEFAULT_CEILING);
    download_files(&pool, 1329).await;
    assert_eq!(pool.size(), DEFAULT_CEILING);

    let pool = TransferPool::new(DEFAULT_CEILING);
    for _ in 0..10 {
        download_files(&pool, START).await;
    }
    assert_eq!(pool.size(), START, "nothing ever waited for a slot");
}

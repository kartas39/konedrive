use std::os::unix::fs::MetadataExt;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use konedrive_tree::{ContentPresence, Item, LocalVersionChange};

use super::download_source::decide_refusal;
use super::*;
use crate::folder::running_steps::{RunningStep, StepClass};
use crate::fuse::item_fixture::ItemFixture;
use crate::hydration::source::{Fetched, SourceError};

/// A content source the test holds: it answers through the fixture's, when the test lets
/// it, or what the test says it refuses.
struct HeldSource {
    inner: Arc<dyn ContentSource>,
    /// A fetch takes one permit and gives none back: the test hands them out.
    gate: tokio::sync::Semaphore,
    refusal: Mutex<Option<fn() -> SourceError>>,
}

impl HeldSource {
    fn held(fixture: &ItemFixture) -> Arc<Self> {
        Arc::new(HeldSource { inner: fixture.content_source(), gate: tokio::sync::Semaphore::new(0), refusal: Mutex::new(None) })
    }

    fn open(fixture: &ItemFixture) -> Arc<Self> {
        let source = Self::held(fixture);
        source.let_go();
        source
    }

    /// Every fetch is answered from now on.
    fn let_go(&self) {
        self.gate.add_permits(1 << 20);
    }

    fn refuse(&self, refusal: Option<fn() -> SourceError>) {
        *self.refusal.lock().unwrap() = refusal;
    }
}

#[async_trait::async_trait]
impl ContentSource for HeldSource {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        self.gate.acquire().await.expect("the gate is never closed").forget();
        let refusal = *self.refusal.lock().unwrap();
        match refusal {
            Some(refusal) => Err(refusal()),
            None => self.inner.fetch(item_id, from, end).await,
        }
    }
}

fn make_downloader(fixture: &ItemFixture, source: &Arc<HeldSource>) -> Arc<ItemDownloader> {
    let landing = Arc::new(ItemDownloadLanding::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.running_steps)));
    Arc::new(ItemDownloader::new(
        Arc::clone(&fixture.backing_directory),
        Arc::clone(&fixture.download_states),
        landing,
        Arc::clone(&fixture.inotify_bridge),
        Arc::clone(&fixture.write_operation_thread),
        Arc::clone(source) as Arc<dyn ContentSource>,
        Arc::clone(&fixture.pool),
        Arc::clone(&fixture.split_download_share),
        Report::nowhere(),
        fixture.folder(),
        fixture.runtime(),
    ))
}

fn to_download(row: &Item) -> ItemToDownload {
    ItemToDownload { item: row.local_item_id, onedrive_item: row.onedrive_item.clone().expect("an item of OneDrive"), content: row.local.as_ref().and_then(|local| local.content.clone()).expect("a file the folder shows") }
}

fn key_of(row: &Item) -> DownloadKey {
    let file = to_download(row);
    DownloadKey { item: file.item, content: file.content }
}

/// Asks for a download of `row` and gives what its waiter will hear.
fn ask(downloader: &Arc<ItemDownloader>, row: &Item, asker: DownloadAsker) -> oneshot::Receiver<Result<(), i32>> {
    let (waiter, heard) = oneshot::channel();
    downloader.download(to_download(row), asker, Some(waiter));
    heard
}

fn hear(heard: oneshot::Receiver<Result<(), i32>>) -> Result<(), i32> {
    heard.blocking_recv().expect("the waiter is answered")
}

/// Waits until `is_so` answers yes: for what a task of the runtime does by itself.
fn wait_until(what: &str, is_so: impl Fn() -> bool) {
    let began = Instant::now();
    while !is_so() {
        assert!(began.elapsed() < Duration::from_secs(20), "waited too long for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Waits until the download of `row` holds its file: it waits for the source from then on.
fn wait_until_it_writes(fixture: &ItemFixture, row: &Item) -> u64 {
    let key = key_of(row);
    wait_until("the download has its file", || fixture.download_states.find_running_download(&key).is_some());
    fixture.download_states.find_running_download(&key).expect("the running download").file.metadata().expect("its file").ino()
}

fn begin_download_step(fixture: &ItemFixture, row: &Item) {
    assert!(fixture.running_steps.begin(RunningStep { class: StepClass::Download, item: row.local_item_id, other_items: Vec::new(), destination: None, sent: 0, total: 0 }));
}

fn presence_of(fixture: &ItemFixture, row: &Item) -> ContentPresence {
    fixture.find_item(row.local_item_id).expect("the row").content_presence
}

fn plan_pin(row: &Item) -> DownloadAsker {
    DownloadAsker::PlanPin { planned_from: ItemVersions::of(row) }
}

#[test]
fn the_pools_class_is_open_for_every_asker_but_the_plans_pin() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"a");
    assert_eq!(decide_pool_class(&DownloadAsker::Read), Class::Open);
    assert_eq!(decide_pool_class(&DownloadAsker::OpenForWriting), Class::Open);
    assert_eq!(decide_pool_class(&DownloadAsker::FilesInterface), Class::Open);
    assert_eq!(decide_pool_class(&plan_pin(&shown)), Class::Download);
}

#[test]
fn the_landing_asks_every_version_for_the_plans_pin_and_the_content_alone_for_anyone_else() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"a");
    let file = to_download(&shown);
    let content_alone = LandingCondition::ContentUnchanged { item: shown.local_item_id, downloaded: file.content.clone() };
    assert_eq!(decide_landing_condition(&file, &DownloadAsker::Read), content_alone);
    assert_eq!(decide_landing_condition(&file, &DownloadAsker::OpenForWriting), content_alone);
    assert_eq!(decide_landing_condition(&file, &DownloadAsker::FilesInterface), content_alone);
    assert_eq!(decide_landing_condition(&file, &plan_pin(&shown)), LandingCondition::VersionsUnchanged { item: ItemVersions::of(&shown) });
}

#[test]
fn a_refusal_of_the_source_is_typed_by_what_the_plans_task_does_about_it() {
    let wait = Duration::from_secs(30);
    assert_eq!(decide_refusal(&SourceError::SignedOut("401".into())), DownloadRefusal::SignedOut);
    assert_eq!(decide_refusal(&SourceError::Throttled { detail: "429".into(), retry_after: Some(wait) }), DownloadRefusal::Throttled { retry_after: Some(wait) });
    assert_eq!(decide_refusal(&SourceError::NotFound("404".into())), DownloadRefusal::ItemGone);
    assert_eq!(decide_refusal(&SourceError::Transient("no network".into())), DownloadRefusal::Transient { detail: "no network".into() });

    // A download that failed is answered by what the source last refused, and else by its errno.
    assert_eq!(decide_download_failure(libc::EIO, Some(DownloadRefusal::SignedOut)), Err(DownloadRefusal::SignedOut));
    assert_eq!(decide_download_failure(libc::ENOSPC, None), Ok(ItemLandingOutcome::Failed { errno: libc::ENOSPC }));

    // What a waiter hears.
    let landed = ItemLandingOutcome::Landed { place: konedrive_tree::ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "a".into() }, path: "a".into() };
    assert_eq!(decide_waiters_answer(&Ok(landed)), Ok(()));
    assert_eq!(decide_waiters_answer(&Ok(ItemLandingOutcome::RowsChanged)), Err(libc::ESTALE));
    assert_eq!(decide_waiters_answer(&Err(DownloadRefusal::ItemGone)), Err(libc::ESTALE));
    assert_eq!(decide_waiters_answer(&Ok(ItemLandingOutcome::PlaceHeld { path: "a".into() })), Err(libc::EEXIST));
    assert_eq!(decide_waiters_answer(&Ok(ItemLandingOutcome::Failed { errno: libc::ENOSPC })), Err(libc::ENOSPC));
    for end in [Ok(ItemLandingOutcome::StepStopped), Err(DownloadRefusal::SignedOut), Err(DownloadRefusal::Throttled { retry_after: None }), Err(DownloadRefusal::Transient { detail: String::new() })] {
        assert_eq!(decide_waiters_answer(&end), Err(libc::EIO), "{end:?}");
    }

    // What the plan's step is answered when it joined somebody else's download.
    assert_eq!(decide_joined_end(Ok(())), Ok(ItemLandingOutcome::RowsChanged));
    assert_eq!(decide_joined_end(Err(libc::ESTALE)), Ok(ItemLandingOutcome::RowsChanged));
    assert_eq!(decide_joined_end(Err(libc::EEXIST)), Ok(ItemLandingOutcome::PlaceHeld { path: PathBuf::new() }));
    assert!(matches!(decide_joined_end(Err(libc::EIO)), Err(DownloadRefusal::Transient { .. })));
}

#[test]
fn content_of_another_size_or_version_than_was_asked_for_is_not_the_asked_content() {
    let asked = ItemFixture::onedrive_content_of(b"the content");
    let (hash, other_hash) = (decode_base64(&crate::fake_onedrive::qx(b"the content")).unwrap(), decode_base64(&crate::fake_onedrive::qx(b"THE CONTENT")).unwrap());
    let ctag = format!("c-{}", crate::fake_onedrive::qx(b"the content"));
    let version = |ctag: &str, quick_xor| Version { ctag: ctag.to_owned(), quick_xor };

    assert!(decide_is_asked_content(&asked, 11, Some(&version(&ctag, Some(hash)))));
    assert!(decide_is_asked_content(&asked, 11, Some(&version("another cTag", Some(hash)))), "the hash says it where both have one");
    assert!(!decide_is_asked_content(&asked, 11, Some(&version(&ctag, Some(other_hash)))), "another hash");
    assert!(!decide_is_asked_content(&asked, 12, Some(&version(&ctag, Some(hash)))), "another size");
    assert!(decide_is_asked_content(&asked, 11, Some(&version(&ctag, None))), "no hash of the source's: the cTag");
    assert!(!decide_is_asked_content(&asked, 11, Some(&version("another cTag", None))));
    assert!(decide_is_asked_content(&asked, 11, None), "a source that says no version: the size alone");
    let without_marks = ContentVersion::Hashed { size: 11, mtime: 0, mtime_nsec: 0, hash: None, ctag: None };
    assert!(decide_is_asked_content(&without_marks, 11, Some(&version("any", Some(hash)))), "a content told by neither is any content of its size");
}

#[test]
fn a_download_lands_at_the_place_the_row_has_when_it_ends() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);

    let heard = ask(&downloader, &shown, DownloadAsker::FilesInterface);
    wait_until_it_writes(&fixture, &shown);
    assert!(fixture.download_states.is_downloading(shown.local_item_id));
    fixture.move_here(shown.local_item_id, "docs/moved.txt");
    source.let_go();

    assert_eq!(hear(heard), Ok(()));
    assert_eq!(fixture.read_disk_file("docs/moved.txt").as_deref(), Some(&b"the report"[..]));
    assert_eq!(fixture.read_disk_file("report.txt"), None);
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    assert!(!fixture.download_states.is_downloading(shown.local_item_id));
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_download_whose_content_the_row_no_longer_has_lands_nothing_and_answers_stale() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);

    let heard = ask(&downloader, &shown, DownloadAsker::Read);
    wait_until_it_writes(&fixture, &shown);
    // A program opens the file for writing meanwhile: its content is another from here on.
    let id = shown.local_item_id;
    fixture.write_rows(move |store| store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: id }));
    source.let_go();

    assert_eq!(hear(heard), Err(libc::ESTALE));
    assert_eq!(fixture.read_disk_file("report.txt"), None);
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::NotOnDisk);
}

#[test]
fn a_download_of_content_onedrive_no_longer_has_lands_nothing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    // OneDrive has another content by now, which no listing has told the rows yet.
    fixture.serve_content(shown.local_item_id, b"THE REPORT");
    let source = HeldSource::open(&fixture);
    let downloader = make_downloader(&fixture, &source);

    assert_eq!(hear(ask(&downloader, &shown, DownloadAsker::Read)), Err(libc::ESTALE));
    assert_eq!(fixture.read_disk_file("report.txt"), None, "the new content does not land as the old");

    begin_download_step(&fixture, &shown);
    let planned = fixture.runtime().block_on(downloader.download_and_land(to_download(&shown), ItemVersions::of(&shown)));
    assert_eq!(planned, Err(DownloadRefusal::ItemGone));
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::NotOnDisk);
}

#[test]
fn a_download_asked_twice_runs_once_and_answers_both() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);

    let first = ask(&downloader, &shown, DownloadAsker::Read);
    let second = ask(&downloader, &shown, DownloadAsker::FilesInterface);
    source.let_go();

    assert_eq!((hear(first), hear(second)), (Ok(()), Ok(())));
    assert_eq!(fixture.fixture_content_source().list_fetches().len(), 1, "one download fetched the content");
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
}

#[test]
fn a_download_that_fails_keeps_its_unfinished_file_and_goes_on_from_it() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);

    let heard = ask(&downloader, &shown, DownloadAsker::Read);
    let file_of_first = wait_until_it_writes(&fixture, &shown);
    source.refuse(Some(|| SourceError::NotFound("the test says so".into())));
    source.gate.add_permits(1);
    assert_eq!(hear(heard), Err(libc::ESTALE), "OneDrive does not have the item");

    source.refuse(None);
    let heard = ask(&downloader, &shown, DownloadAsker::Read);
    let file_of_second = wait_until_it_writes(&fixture, &shown);
    assert_eq!(file_of_second, file_of_first, "the next download of the content writes into the file the failed one left");
    source.let_go();
    assert_eq!(hear(heard), Ok(()));
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
}

#[test]
fn the_file_is_synced_before_the_landing_job_is_queued() {
    // What can be seen of it: while the write-operation thread is taken, the download
    // gets as far as its landing's job — the whole content is in its file, nothing of it
    // waited for the thread — and the row says nothing yet.
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let source = HeldSource::open(&fixture);
    let downloader = make_downloader(&fixture, &source);
    let (let_thread_go, thread_held) = mpsc::channel::<()>();
    fixture.write_operation_thread.run(move || {
        let _ = thread_held.recv();
    });

    let heard = ask(&downloader, &shown, DownloadAsker::Read);
    wait_until("the landing's job is queued", || fixture.write_operation_thread.count_queued_jobs() == 1);
    let running = fixture.download_states.find_running_download(&key_of(&shown)).expect("the download runs until its landing answered");
    assert_eq!(running.file.metadata().expect("its file").len(), 10, "the whole content is in the file");
    assert_eq!(fixture.read_disk_file("report.txt"), None, "the file has no name yet");
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::NotOnDisk);

    let_thread_go.send(()).expect("the thread is let go");
    assert_eq!(hear(heard), Ok(()));
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
}

#[test]
fn a_download_for_the_plan_commits_as_the_plans_step() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let source = HeldSource::open(&fixture);
    let downloader = make_downloader(&fixture, &source);
    begin_download_step(&fixture, &shown);

    let landed = fixture.runtime().block_on(downloader.download_and_land(to_download(&shown), ItemVersions::of(&shown)));

    assert_eq!(landed, Ok(ItemLandingOutcome::Landed { place: shown.local.clone().unwrap().place.unwrap(), path: "pinned.txt".into() }));
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    assert_eq!(fixture.running_steps.abort_requests(), Vec::new(), "the step's commit has begun: a stop leaves it");

    // Planned from versions the row has not any more: the step's commit refuses.
    let other = fixture.show_onedrive_file("other.txt", b"other");
    let planned_from = ItemVersions::of(&other);
    begin_download_step(&fixture, &other);
    fixture.list_onedrive_file("other.txt", b"another content");
    // The source still gives the content that was asked for: the row is what changed.
    fixture.serve_content(other.local_item_id, b"other");
    let landed = fixture.runtime().block_on(downloader.download_and_land(to_download(&other), planned_from));
    assert_eq!(landed, Ok(ItemLandingOutcome::RowsChanged));
    assert_eq!(fixture.read_disk_file("other.txt"), None);
}

#[test]
fn a_sign_out_a_throttle_and_a_missing_item_of_the_source_are_answered_as_such() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let source = HeldSource::open(&fixture);
    let downloader = make_downloader(&fixture, &source);
    let download = |refusal: fn() -> SourceError| {
        source.refuse(Some(refusal));
        fixture.runtime().block_on(downloader.download_and_land(to_download(&shown), ItemVersions::of(&shown)))
    };

    assert_eq!(download(|| SourceError::SignedOut("401".into())), Err(DownloadRefusal::SignedOut));
    assert_eq!(download(|| SourceError::Throttled { detail: "429".into(), retry_after: Some(Duration::from_secs(30)) }), Err(DownloadRefusal::Throttled { retry_after: Some(Duration::from_secs(30)) }));
    assert_eq!(download(|| SourceError::NotFound("404".into())), Err(DownloadRefusal::ItemGone));
    assert_eq!(fixture.read_disk_file("pinned.txt"), None);

    // Whoever waits for such a download is answered an errno.
    source.refuse(Some(|| SourceError::SignedOut("401".into())));
    assert_eq!(hear(ask(&downloader, &shown, DownloadAsker::Read)), Err(libc::EIO));
}

#[test]
fn the_plans_download_is_cancelled_when_the_plan_alone_asked_for_it() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);
    begin_download_step(&fixture, &shown);
    let (plans, file) = (Arc::clone(&downloader), to_download(&shown));
    let planned_from = ItemVersions::of(&shown);
    let step = fixture.runtime().spawn(async move { plans.download_and_land(file, planned_from).await });
    wait_until_it_writes(&fixture, &shown);

    downloader.cancel(shown.local_item_id, &to_download(&shown).content);

    assert!(!fixture.download_states.is_downloading(shown.local_item_id), "the download is aborted at once");
    assert!(matches!(fixture.runtime().block_on(step).expect("the step's task"), Err(DownloadRefusal::Transient { .. })));
    source.let_go();
    assert_eq!(fixture.read_disk_file("pinned.txt"), None);
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::NotOnDisk);
}

#[test]
fn a_download_a_read_joined_goes_on_after_the_plans_cancel() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);
    begin_download_step(&fixture, &shown);
    let (plans, file) = (Arc::clone(&downloader), to_download(&shown));
    let planned_from = ItemVersions::of(&shown);
    let step = fixture.runtime().spawn(async move { plans.download_and_land(file, planned_from).await });
    wait_until_it_writes(&fixture, &shown);
    let read = ask(&downloader, &shown, DownloadAsker::Read);

    // The stop of the sync: the step's task is aborted, and its guard gives the download up.
    assert_eq!(fixture.running_steps.abort_requests(), [shown.local_item_id]);
    step.abort();
    downloader.cancel(shown.local_item_id, &to_download(&shown).content);

    assert!(fixture.download_states.is_downloading(shown.local_item_id), "a read asked for it too: it goes on");
    source.let_go();
    assert_eq!(hear(read), Ok(()), "the read's file lands, by the content alone");
    assert_eq!(fixture.read_disk_file("pinned.txt").as_deref(), Some(&b"pinned"[..]));
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_plan_that_joins_a_reads_download_is_answered_that_the_rows_changed() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);
    let read = ask(&downloader, &shown, DownloadAsker::Read);
    wait_until_it_writes(&fixture, &shown);
    begin_download_step(&fixture, &shown);
    let (plans, file) = (Arc::clone(&downloader), to_download(&shown));
    let planned_from = ItemVersions::of(&shown);
    let step = fixture.runtime().spawn(async move { plans.download_and_land(file, planned_from).await });
    // The plan's asking joined: one download, and no second fetch.
    std::thread::sleep(Duration::from_millis(20));
    source.let_go();

    assert_eq!(hear(read), Ok(()));
    assert_eq!(fixture.runtime().block_on(step).expect("the step's task"), Ok(ItemLandingOutcome::RowsChanged), "the landing committed as the read's, not as the step");
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    assert_eq!(fixture.fixture_content_source().list_fetches().len(), 1);
}

#[test]
fn a_download_aborted_and_asked_again_is_not_ended_by_the_aborted_tasks_guard() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);
    let plans = ask(&downloader, &shown, plan_pin(&shown));
    wait_until_it_writes(&fixture, &shown);

    downloader.cancel(shown.local_item_id, &to_download(&shown).content);
    assert_eq!(hear(plans), Err(libc::EIO), "the plan's waiter is told");
    let read = ask(&downloader, &shown, DownloadAsker::Read);
    // The aborted task is dropped by the runtime in its own time, and its guard with it.
    std::thread::sleep(Duration::from_millis(50));
    assert!(fixture.download_states.is_downloading(shown.local_item_id), "the newer download of the key still runs");
    source.let_go();

    assert_eq!(hear(read), Ok(()));
    assert_eq!(fixture.read_disk_file("pinned.txt").as_deref(), Some(&b"pinned"[..]));
}

#[test]
fn a_stop_aborts_every_download() {
    let fixture = ItemFixture::new();
    let (first, second) = (fixture.show_onedrive_file("a.txt", b"a"), fixture.show_onedrive_file("b.txt", b"b"));
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);
    let (heard_first, heard_second) = (ask(&downloader, &first, DownloadAsker::Read), ask(&downloader, &second, plan_pin(&second)));
    wait_until_it_writes(&fixture, &first);

    downloader.stop();

    assert_eq!((hear(heard_first), hear(heard_second)), (Err(libc::EIO), Err(libc::EIO)));
    assert!(!fixture.download_states.is_downloading(first.local_item_id) && !fixture.download_states.is_downloading(second.local_item_id));
    source.let_go();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!((fixture.read_disk_file("a.txt"), fixture.read_disk_file("b.txt")), (None, None), "nothing lands after the stop");
}

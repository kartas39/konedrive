use std::collections::VecDeque;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use konedrive_tree::{ContentPresence, Item, LocalVersionChange};

use super::download_source::{decide_answer_use, decide_refusal, find_gravest_refusal, find_told_size, AnswerUse};
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
    make_downloader_and_landing(fixture, source).0
}

fn make_downloader_and_landing(fixture: &ItemFixture, source: &Arc<HeldSource>) -> (Arc<ItemDownloader>, Arc<ItemDownloadLanding>) {
    make_downloader_from(fixture, Arc::clone(source) as Arc<dyn ContentSource>)
}

fn make_downloader_from(fixture: &ItemFixture, source: Arc<dyn ContentSource>) -> (Arc<ItemDownloader>, Arc<ItemDownloadLanding>) {
    make_downloader_reporting_to(fixture, source, Report::nowhere())
}

/// A report whose activity keeps the events of the fixture's folder.
fn make_report(fixture: &ItemFixture) -> Report {
    let mut snapshot = crate::status::snapshot::SyncSnapshot::default();
    snapshot.folder.root_path = fixture.folder().display().to_string();
    Report { activity: Arc::new(activity::Activity::new(crate::status::snapshot::SyncStateHandle::new(snapshot))), ..Report::nowhere() }
}

fn make_downloader_reporting_to(fixture: &ItemFixture, source: Arc<dyn ContentSource>, report: Report) -> (Arc<ItemDownloader>, Arc<ItemDownloadLanding>) {
    let landing = Arc::new(ItemDownloadLanding::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.running_steps), Arc::clone(&fixture.mount_take_down)));
    let downloader = Arc::new(ItemDownloader::new(
        Arc::clone(&fixture.backing_directory),
        Arc::clone(&fixture.download_states),
        Arc::clone(&landing),
        Arc::clone(&fixture.inotify_bridge),
        Arc::clone(&fixture.write_operation_thread),
        source,
        Arc::clone(&fixture.pool),
        Arc::clone(&fixture.split_download_share),
        report,
        fixture.folder(),
        fixture.runtime(),
    ));
    (downloader, landing)
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

/// What the waiter hears within five seconds; `None` when nothing answers it by then.
fn hear_soon(fixture: &ItemFixture, heard: oneshot::Receiver<Result<(), i32>>) -> Option<Result<(), i32>> {
    fixture.runtime().block_on(async { tokio::time::timeout(Duration::from_secs(5), heard).await.ok().map(|heard| heard.expect("the waiter is answered")) })
}

/// A content source that answers what the test queued, an answer per fetch, and keeps
/// what it was asked.
#[derive(Default)]
struct QueuedSource {
    answers: Mutex<VecDeque<Result<QueuedAnswer, SourceError>>>,
    fetches: Mutex<Vec<(u64, Option<u64>)>>,
}

/// One answer: the item's size and version, where the stream starts, and its bytes.
struct QueuedAnswer {
    size: u64,
    version: Option<Version>,
    served_from: u64,
    bytes: Vec<u8>,
}

impl QueuedSource {
    fn queue(&self, answer: Result<QueuedAnswer, SourceError>) {
        self.answers.lock().unwrap().push_back(answer);
    }

    /// The whole of `content` from `served_from` on, as the content OneDrive would tell
    /// for these bytes.
    fn queue_content(&self, content: &[u8], served_from: u64) {
        self.queue(Ok(QueuedAnswer { size: content.len() as u64, version: Some(version_of(content)), served_from, bytes: content[served_from as usize..].to_vec() }));
    }
}

#[async_trait::async_trait]
impl ContentSource for QueuedSource {
    async fn fetch(&self, _item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        self.fetches.lock().unwrap().push((from, end));
        let answer = self.answers.lock().unwrap().pop_front().expect("an answer the test queued")?;
        Ok(Fetched { served_from: answer.served_from, size: answer.size, mtime: SystemTime::UNIX_EPOCH, version: answer.version, stream: Box::new(std::io::Cursor::new(answer.bytes)) })
    }
}

/// Download states of its own with one download of `asked` that runs, for a source that
/// is tested without a downloader.
fn start_download_alone(asked: &ContentVersion) -> (ItemDownloadStates, DownloadRun) {
    let download_states = ItemDownloadStates::new(0);
    match download_states.start_or_join(&DownloadKey { item: LocalItemId::new(5), content: asked.clone() }, &DownloadAsker::Read, None) {
        ItemDownloadStart::Started { run, .. } => (download_states, run),
        ItemDownloadStart::Joined | ItemDownloadStart::Stopped => panic!("no download of it runs"),
    }
}

/// The version OneDrive tells for `content`, as the fixture's source says it.
fn version_of(content: &[u8]) -> Version {
    Version { ctag: format!("c-{}", crate::fake_onedrive::qx(content)), quick_xor: decode_base64(&crate::fake_onedrive::qx(content)) }
}

/// Runs a future that waits for nothing but its own lines.
fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().build().expect("a runtime").block_on(future)
}

async fn read_whole(fetched: Fetched) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let (mut stream, mut bytes) = (fetched.stream, Vec::new());
    stream.read_to_end(&mut bytes).await.expect("the stream is read");
    bytes
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
    assert_eq!(decide_refusal(&SourceError::NotFound("404".into())), DownloadRefusal::ItemGone, "\"no such item\" states no other content");
    assert_eq!(decide_refusal(&SourceError::Transient("no network".into())), DownloadRefusal::Transient { detail: "no network".into() });

    // A download that failed is answered by what the source last refused, and else by its errno.
    assert_eq!(decide_download_failure(libc::EIO, Some(DownloadRefusal::SignedOut)), Err(DownloadRefusal::SignedOut));
    assert_eq!(decide_download_failure(libc::ENOSPC, None), Ok(ItemLandingOutcome::Failed { errno: libc::ENOSPC }));

    // What a waiter hears.
    let landed = ItemLandingOutcome::Landed { place: konedrive_tree::ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "a".into() }, path: "a".into() };
    assert_eq!(decide_waiters_answer(&Ok(landed)), Ok(()));
    assert_eq!(decide_waiters_answer(&Ok(ItemLandingOutcome::RowsChanged)), Err(libc::ESTALE));
    assert_eq!(decide_waiters_answer(&Err(DownloadRefusal::ItemGone)), Err(libc::ESTALE));
    assert_eq!(decide_waiters_answer(&Err(DownloadRefusal::AnotherContent)), Err(libc::ESTALE));
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

    assert!(decide_is_asked_content(&asked, Some(11), Some(&version(&ctag, Some(hash)))));
    assert!(decide_is_asked_content(&asked, Some(11), Some(&version("another cTag", Some(hash)))), "the hash says it where both have one");
    assert!(!decide_is_asked_content(&asked, Some(11), Some(&version(&ctag, Some(other_hash)))), "another hash");
    assert!(!decide_is_asked_content(&asked, Some(12), Some(&version(&ctag, Some(hash)))), "another size");
    assert!(decide_is_asked_content(&asked, Some(11), Some(&version(&ctag, None))), "no hash of the source's: the cTag");
    assert!(!decide_is_asked_content(&asked, Some(11), Some(&version("another cTag", None))));
    assert!(decide_is_asked_content(&asked, Some(11), None), "a source that says no version: the size alone");
    let without_marks = ContentVersion::Hashed { size: 11, mtime: 0, mtime_nsec: 0, hash: None, ctag: None };
    assert!(decide_is_asked_content(&without_marks, Some(11), Some(&version("any", Some(hash)))), "a content told by neither is any content of its size");
    // A size that was not told disagrees with nothing: the version tells, where one was told.
    assert!(decide_is_asked_content(&asked, None, Some(&version(&ctag, Some(hash)))));
    assert!(!decide_is_asked_content(&asked, None, Some(&version(&ctag, Some(other_hash)))));
    assert!(!decide_is_asked_content(&asked, None, Some(&version("another cTag", None))));
    assert!(decide_is_asked_content(&asked, None, None), "nothing was told: nothing disagrees");
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
    assert!(fixture.download_states.is_another_content(&key_of(&shown)), "and whoever reads through a handle of the old content is told");

    begin_download_step(&fixture, &shown);
    let planned = fixture.runtime().block_on(downloader.download_and_land(to_download(&shown), ItemVersions::of(&shown)));
    assert_eq!(planned, Err(DownloadRefusal::AnotherContent));
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
    // The network fails, at the fetch and at each time it is tried again.
    source.refuse(Some(|| SourceError::Transient("the test says so".into())));
    source.gate.add_permits(3);
    assert_eq!(hear(heard), Err(libc::EIO));

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
    let (tell_taken, thread_taken) = mpsc::channel::<()>();
    fixture.write_operation_thread.run(move || {
        let _ = tell_taken.send(());
        let _ = thread_held.recv();
    });
    // The thread runs the job that holds it: nothing is queued, and the one job that
    // is queued from here on is the landing's. (Counted before the thread took the
    // holding job, that job itself is "one job queued", with no download begun.)
    thread_taken.recv().expect("the thread took the job that holds it");
    assert_eq!(fixture.write_operation_thread.count_queued_jobs(), 0);

    let heard = ask(&downloader, &shown, DownloadAsker::Read);
    wait_until("the landing's job is queued", || fixture.write_operation_thread.count_queued_jobs() == 1);
    let running = fixture.download_states.find_running_download(&key_of(&shown)).expect("the download runs until its landing answered");
    assert_eq!(running.file.metadata().expect("its file").len(), 10, "the whole content is in the file");
    // The time and the marks are on it before its sync, which covers them: a copy that has
    // its name after a power loss has them too.
    let mark_of = |mark: &str| xattr::FileExt::get_xattr(&*running.file, mark).expect("the marks are read").map(|value| String::from_utf8_lossy(&value).into_owned());
    assert_eq!(mark_of(konedrive_fs::placeholder::XATTR_ITEM_ID).as_deref(), Some("report.txt"), "marked before the landing's job runs");
    assert_eq!(mark_of(konedrive_fs::placeholder::XATTR_CTAG), Some(format!("c-{}", crate::fake_onedrive::qx(b"the report"))));
    assert!(mark_of(konedrive_fs::placeholder::XATTR_STATE).is_some(), "the whole-copy state");
    assert_eq!(running.file.metadata().expect("its file").mtime(), crate::fuse::item_fixture::ONEDRIVE_MTIME, "the content's time");
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
    // The plan's asking joined — the read's waiter and the plan's wait for the one
    // download — before the source is let go: one download, and no second fetch.
    wait_until("the plan's asking joined the read's download", || fixture.download_states.count_waiters(&key_of(&shown)) == 2);
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

#[test]
fn an_answer_of_the_source_is_used_by_its_content_and_its_place_case_by_case() {
    let asked = ItemFixture::onedrive_content_of(b"the content");
    let (same, other) = (version_of(b"the content"), version_of(b"THE CONTENT"));
    // The one stream of the file: what is on disk counts from where it starts.
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&same), 0, 0, None), AnswerUse::WrittenInOrder { from: 0 });
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&same), 4, 4, None), AnswerUse::WrittenInOrder { from: 4 }, "gone on from a checkpoint, or after a break");
    // A stream that starts elsewhere than asked is refused by the download: nothing is counted.
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&same), 0, 4, None), AnswerUse::NotCounted);
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&same), 8, 4, None), AnswerUse::NotCounted);
    // A part of a download in parts tells nothing of how far the file is written.
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&same), 4, 4, Some(8)), AnswerUse::NotCounted);
    // Another content, of the same size: whole, gone on, or a part.
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&other), 0, 0, None), AnswerUse::AnotherContent);
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&other), 4, 4, None), AnswerUse::AnotherContent);
    assert_eq!(decide_answer_use(&asked, Some(11), Some(&other), 4, 4, Some(8)), AnswerUse::AnotherContent);
    // Another size, whatever the version says.
    assert_eq!(decide_answer_use(&asked, Some(12), Some(&same), 0, 0, None), AnswerUse::AnotherContent);
    assert_eq!(decide_answer_use(&asked, Some(12), None, 0, 0, None), AnswerUse::AnotherContent);
    // A source that says no version: the size alone tells.
    assert_eq!(decide_answer_use(&asked, Some(11), None, 0, 0, None), AnswerUse::WrittenInOrder { from: 0 });
    // No size told: the version alone tells, and with neither nothing disagrees.
    assert_eq!(decide_answer_use(&asked, None, Some(&same), 0, 0, None), AnswerUse::WrittenInOrder { from: 0 });
    assert_eq!(decide_answer_use(&asked, None, Some(&other), 0, 0, None), AnswerUse::AnotherContent);
    assert_eq!(decide_answer_use(&asked, None, None, 0, 0, None), AnswerUse::WrittenInOrder { from: 0 });
}

#[test]
fn an_answer_of_another_content_ends_the_download_before_the_mark_is_raised_or_a_byte_read() {
    let asked = ItemFixture::onedrive_content_of(b"the content");
    let alone = start_download_alone(&asked);
    // The first answer is of another content of the same size.
    let (inner, written_up_to) = (QueuedSource::default(), Arc::new(AtomicU64::new(0)));
    inner.queue_content(b"THE CONTENT", 0);
    let source = DownloadSource::new(&inner, asked.clone(), Arc::clone(&written_up_to), &alone.0, &alone.1);
    let fetched = run(source.fetch("id", 0, None));
    assert!(matches!(fetched, Err(SourceError::NotFound(_))), "answered as a file OneDrive does not have: the download ends at once");
    assert_eq!(written_up_to.load(Ordering::SeqCst), 0, "nothing is said to be written");
    assert_eq!(source.find_refusal(), Some(DownloadRefusal::AnotherContent));
    assert_eq!(*inner.fetches.lock().unwrap(), [(0, None)]);

    // The asked content first, and another content after a break: the mark stays where
    // the bytes of the asked content got to, and nothing of the other is read.
    let (inner, written_up_to) = (QueuedSource::default(), Arc::new(AtomicU64::new(0)));
    inner.queue(Ok(QueuedAnswer { size: 11, version: Some(version_of(b"the content")), served_from: 0, bytes: b"the c".to_vec() }));
    inner.queue_content(b"THE CONTENT", 5);
    let source = DownloadSource::new(&inner, asked.clone(), Arc::clone(&written_up_to), &alone.0, &alone.1);
    let read = run(async { read_whole(source.fetch("id", 0, None).await.ok().expect("the asked content")).await });
    assert_eq!((read.as_slice(), written_up_to.load(Ordering::SeqCst)), (&b"the c"[..], 5), "counted as it is read");
    assert!(matches!(run(source.fetch("id", 5, None)), Err(SourceError::NotFound(_))));
    assert_eq!(written_up_to.load(Ordering::SeqCst), 5);
    assert_eq!(source.find_refusal(), Some(DownloadRefusal::AnotherContent));

    // A part of another content is refused as the one stream is; a part of the asked
    // content is handed on and tells nothing of how far the file is written.
    let (inner, written_up_to) = (QueuedSource::default(), Arc::new(AtomicU64::new(0)));
    inner.queue(Ok(QueuedAnswer { size: 11, version: Some(version_of(b"the content")), served_from: 4, bytes: b"cont".to_vec() }));
    inner.queue(Ok(QueuedAnswer { size: 11, version: Some(version_of(b"THE CONTENT")), served_from: 0, bytes: b"THE ".to_vec() }));
    let source = DownloadSource::new(&inner, asked, Arc::clone(&written_up_to), &alone.0, &alone.1);
    assert_eq!(run(async { read_whole(source.fetch("id", 4, Some(8)).await.ok().expect("a part of the asked content")).await }), b"cont");
    assert!(matches!(run(source.fetch("id", 0, Some(4))), Err(SourceError::NotFound(_))));
    assert_eq!(written_up_to.load(Ordering::SeqCst), 0);
    assert_eq!(source.find_refusal(), Some(DownloadRefusal::AnotherContent));
}

#[test]
fn a_refusal_is_kept_until_the_same_range_is_answered_and_the_gravest_is_told() {
    let (transient, throttled) = (DownloadRefusal::Transient { detail: "no network".into() }, DownloadRefusal::Throttled { retry_after: None });
    assert_eq!(find_gravest_refusal([]), None);
    assert_eq!(find_gravest_refusal([transient.clone(), throttled.clone()]), Some(throttled.clone()));
    assert_eq!(find_gravest_refusal([throttled.clone(), DownloadRefusal::SignedOut, transient.clone()]), Some(DownloadRefusal::SignedOut));
    assert_eq!(find_gravest_refusal([DownloadRefusal::SignedOut, DownloadRefusal::ItemGone]), Some(DownloadRefusal::ItemGone));
    assert_eq!(find_gravest_refusal([DownloadRefusal::ItemGone, DownloadRefusal::AnotherContent, DownloadRefusal::SignedOut]), Some(DownloadRefusal::AnotherContent), "what was stated of the content before a part that was not found");

    let asked = ItemFixture::onedrive_content_of(b"the content");
    let part = |served_from: u64, bytes: &[u8]| Ok(QueuedAnswer { size: 11, version: Some(version_of(b"the content")), served_from, bytes: bytes.to_vec() });
    let inner = QueuedSource::default();
    let alone = start_download_alone(&asked);
    let source = DownloadSource::new(&inner, asked, Arc::default(), &alone.0, &alone.1);
    assert_eq!(source.find_refusal(), None, "nothing was fetched");
    // One part is throttled; another part, answered after it, takes nothing back.
    inner.queue(Err(SourceError::Throttled { detail: "429".into(), retry_after: None }));
    inner.queue(part(4, b"cont"));
    assert!(run(source.fetch("id", 0, Some(4))).is_err());
    assert!(run(source.fetch("id", 4, Some(8))).is_ok());
    assert_eq!(source.find_refusal(), Some(throttled));
    // The same part answered: its refusal is taken back.
    inner.queue(part(0, b"the "));
    assert!(run(source.fetch("id", 0, Some(4))).is_ok());
    assert_eq!(source.find_refusal(), None);
    // The one stream: its last answer counts.
    inner.queue(Err(SourceError::Transient("no network".into())));
    assert!(run(source.fetch("id", 0, None)).is_err());
    assert_eq!(source.find_refusal(), Some(transient));
    inner.queue(part(0, b"the content"));
    assert!(run(source.fetch("id", 0, None)).is_ok());
    assert_eq!(source.find_refusal(), None);
}

#[test]
fn a_download_asked_for_after_the_stop_answers_eio_and_fetches_nothing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"a");
    let source = HeldSource::open(&fixture);
    let downloader = make_downloader(&fixture, &source);

    downloader.stop();

    assert_eq!(hear_soon(&fixture, ask(&downloader, &shown, DownloadAsker::FilesInterface)), Some(Err(libc::EIO)));
    downloader.download(to_download(&shown), DownloadAsker::Read, None);
    begin_download_step(&fixture, &shown);
    let planned = fixture.runtime().block_on(downloader.download_and_land(to_download(&shown), ItemVersions::of(&shown)));
    assert!(matches!(planned, Err(DownloadRefusal::Transient { .. })), "{planned:?}");
    assert!(!fixture.download_states.is_downloading(shown.local_item_id));
    assert_eq!(fixture.fixture_content_source().list_fetches().len(), 0, "nothing is fetched after the stop");
    assert_eq!(fixture.read_disk_file("a.txt"), None);
}

#[test]
fn an_open_for_writing_that_joins_the_plans_download_does_not_wait_for_the_pause() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"the content");
    let source = HeldSource::open(&fixture);
    let downloader = make_downloader(&fixture, &source);
    // "Pause syncing": the pool hands out no slot of the plan's class.
    fixture.pool.set_paused(true);
    begin_download_step(&fixture, &shown);
    let (plans, file) = (Arc::clone(&downloader), to_download(&shown));
    let planned_from = ItemVersions::of(&shown);
    let step = fixture.runtime().spawn(async move { plans.download_and_land(file, planned_from).await });
    wait_until("the plan's download is asked for", || fixture.download_states.count_waiters(&key_of(&shown)) == 1);
    assert_eq!(fixture.fixture_content_source().list_fetches().len(), 0, "held by the pause");

    // A program opens the file for writing: its open joins the plan's download.
    let writer = ask(&downloader, &shown, DownloadAsker::OpenForWriting);

    assert_eq!(hear_soon(&fixture, writer), Some(Ok(())), "the open is answered while syncing is paused");
    assert_eq!(fixture.read_disk_file("a.txt").as_deref(), Some(&b"the content"[..]));
    assert!(matches!(fixture.runtime().block_on(step).expect("the step's task"), Ok(ItemLandingOutcome::Landed { .. })), "the landing committed as the plan's step");
    assert!(fixture.pool.set_paused(false), "it was paused all along");
}

#[test]
fn a_download_whose_landing_removed_what_it_named_starts_the_next_with_a_new_file() {
    let fixture = Arc::new(ItemFixture::new());
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let source = HeldSource::open(&fixture);
    let (downloader, landing) = make_downloader_and_landing(&fixture, &source);
    // A listing's page comes between the link and the commit of the plan's landing:
    // another eTag, the same content. The step's versions are the row's no more.
    let listing = Arc::clone(&fixture);
    landing.run_before_commit(move || {
        listing.list_onedrive_file("report.txt", b"the report");
    });
    begin_download_step(&fixture, &shown);
    let planned = fixture.runtime().block_on(downloader.download_and_land(to_download(&shown), ItemVersions::of(&shown)));
    assert_eq!(planned, Ok(ItemLandingOutcome::RowsChanged));
    assert_eq!(fixture.read_disk_file("report.txt"), None, "what was named is removed again");

    // The file that had a name can never be named again: the next download of the same
    // item and content must not go on from it.
    let row = fixture.find_item(shown.local_item_id).expect("the row");
    assert_eq!(key_of(&row), key_of(&shown), "the same item and content");
    assert_eq!(hear(ask(&downloader, &row, DownloadAsker::Read)), Ok(()));
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    fixture.assert_rows_and_disk_agree();
}

/// The second review's "premise 1": a "no such item" after a part of the file is down
/// throws nothing away, and is not remembered as another content.
#[test]
fn a_download_of_an_item_onedrive_does_not_have_fails_and_keeps_its_unfinished_file() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let source = HeldSource::held(&fixture);
    let downloader = make_downloader(&fixture, &source);

    let heard = ask(&downloader, &shown, DownloadAsker::Read);
    let file_of_first = wait_until_it_writes(&fixture, &shown);
    source.refuse(Some(|| SourceError::NotFound("404".into())));
    source.gate.add_permits(1);
    assert_eq!(hear(heard), Err(libc::ESTALE), "the errno of an item that is gone");
    assert!(!fixture.download_states.is_another_content(&key_of(&shown)), "no other content was stated");

    // OneDrive has the item again: the next download goes on from the file that was kept.
    source.refuse(None);
    let heard = ask(&downloader, &shown, DownloadAsker::Read);
    assert_eq!(wait_until_it_writes(&fixture, &shown), file_of_first, "the unfinished file is kept, as after any failure");
    source.let_go();
    assert_eq!(hear(heard), Ok(()));
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
}

#[test]
fn a_size_the_source_does_not_tell_is_no_other_content() {
    let asked = ItemFixture::onedrive_content_of(b"the content");
    let empty = ItemFixture::onedrive_content_of(b"");
    assert_eq!(find_told_size(11, &asked), Some(11));
    assert_eq!(find_told_size(12, &asked), Some(12));
    assert_eq!(find_told_size(0, &asked), None, "0 for a content that has bytes: not said");
    assert_eq!(find_told_size(0, &empty), Some(0), "an empty file is 0 bytes long");
    assert_eq!(find_told_size(5, &empty), Some(5));

    // Through the download's source: no size and the asked version is let through, and
    // nothing is kept as a refusal; no size and another version is another content.
    let inner = QueuedSource::default();
    inner.queue(Ok(QueuedAnswer { size: 0, version: Some(version_of(b"the content")), served_from: 0, bytes: b"the content".to_vec() }));
    inner.queue(Ok(QueuedAnswer { size: 0, version: None, served_from: 0, bytes: b"the content".to_vec() }));
    inner.queue(Ok(QueuedAnswer { size: 0, version: Some(version_of(b"")), served_from: 0, bytes: Vec::new() }));
    let alone = start_download_alone(&asked);
    let source = DownloadSource::new(&inner, asked.clone(), Arc::default(), &alone.0, &alone.1);
    assert!(run(source.fetch("id", 0, None)).is_ok());
    assert!(run(source.fetch("id", 0, None)).is_ok(), "neither told: nothing disagrees");
    assert_eq!(source.find_refusal(), None);
    assert!(matches!(run(source.fetch("id", 0, None)), Err(SourceError::NotFound(_))), "emptied in OneDrive: its version says so");
    assert_eq!(source.find_refusal(), Some(DownloadRefusal::AnotherContent));
    // A real empty file against a row of 0 bytes.
    let inner = QueuedSource::default();
    inner.queue_content(b"", 0);
    inner.queue_content(b"bytes", 0);
    let alone = start_download_alone(&empty);
    let source = DownloadSource::new(&inner, empty, Arc::default(), &alone.0, &alone.1);
    assert!(run(source.fetch("id", 0, None)).is_ok());
    assert!(matches!(run(source.fetch("id", 0, None)), Err(SourceError::NotFound(_))));

    // A whole download: the answer that tells no size fails that one asking, and nothing
    // is remembered of it.
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the content");
    let inner = Arc::new(QueuedSource::default());
    inner.queue(Ok(QueuedAnswer { size: 0, version: Some(version_of(b"the content")), served_from: 0, bytes: b"the content".to_vec() }));
    let (downloader, _) = make_downloader_from(&fixture, Arc::clone(&inner) as Arc<dyn ContentSource>);
    assert_eq!(hear(ask(&downloader, &shown, DownloadAsker::Read)), Err(libc::EIO));
    assert!(!fixture.download_states.is_another_content(&key_of(&shown)));
    assert_eq!(fixture.read_disk_file("report.txt"), None);
}

#[test]
fn the_mark_is_taken_back_the_moment_the_download_does_not_trust_what_is_on_disk() {
    let asked = ItemFixture::onedrive_content_of(b"the content");
    let alone = start_download_alone(&asked);
    let (inner, written_up_to) = (QueuedSource::default(), Arc::new(AtomicU64::new(0)));
    inner.queue_content(b"the content", 0);
    inner.queue_content(b"the content", 0);
    let source = DownloadSource::new(&inner, asked, Arc::clone(&written_up_to), &alone.0, &alone.1);
    run(async { read_whole(source.fetch("id", 0, None).await.ok().expect("the asked content")).await });
    assert_eq!(written_up_to.load(Ordering::SeqCst), 11);

    // The download found the hash of what it wrote wrong: at once, with no fetch between.
    source.reject_written();
    assert_eq!(written_up_to.load(Ordering::SeqCst), 0);
    assert_eq!(inner.fetches.lock().unwrap().len(), 1);
    // What it writes when it starts over counts again.
    run(async { read_whole(source.fetch("id", 0, None).await.ok().expect("the asked content")).await });
    assert_eq!(written_up_to.load(Ordering::SeqCst), 11);
}

#[test]
fn what_was_remembered_of_a_key_is_void_at_the_first_answer_its_next_download_accepts() {
    let asked = ItemFixture::onedrive_content_of(b"the content");
    let (download_states, first) = start_download_alone(&asked);
    let key = first.key().clone();
    let start = || match download_states.start_or_join(&key, &DownloadAsker::Read, None) {
        ItemDownloadStart::Started { run, .. } => run,
        ItemDownloadStart::Joined | ItemDownloadStart::Stopped => panic!("no download of it runs"),
    };
    download_states.finish_as_another_content(&first);

    // The one stream: neither a refusal nor an answer of another content voids it.
    let (run_again, inner) = (start(), QueuedSource::default());
    let source = DownloadSource::new(&inner, asked.clone(), Arc::default(), &download_states, &run_again);
    assert!(download_states.is_another_content(&key), "asked for, and OneDrive has not answered");
    inner.queue(Err(SourceError::Transient("no network".into())));
    assert!(run(source.fetch("id", 0, None)).is_err());
    assert!(download_states.is_another_content(&key), "a refusal is no answer about the content");
    inner.queue_content(b"THE CONTENT", 0);
    assert!(run(source.fetch("id", 0, None)).is_err());
    assert!(download_states.is_another_content(&key), "another content again");
    inner.queue_content(b"the content", 0);
    let fetched = run(source.fetch("id", 0, None)).ok().expect("the asked content");
    assert!(!download_states.is_another_content(&key), "void at the answer, before a byte of it is read");
    drop(fetched);
    download_states.finish_as_another_content(&run_again);

    // A part of a download in parts is an answer as well.
    let (run_again, inner) = (start(), QueuedSource::default());
    let source = DownloadSource::new(&inner, asked, Arc::default(), &download_states, &run_again);
    inner.queue(Ok(QueuedAnswer { size: 11, version: Some(version_of(b"the content")), served_from: 4, bytes: b"cont".to_vec() }));
    assert!(download_states.is_another_content(&key));
    assert!(run(source.fetch("id", 4, Some(8))).is_ok());
    assert!(!download_states.is_another_content(&key));
}

#[test]
fn the_end_of_a_download_is_news_per_case() {
    let another_content: DownloadEnd = Err(DownloadRefusal::AnotherContent);
    assert_eq!(decide_end_news(&another_content, false), DownloadEndNews::Changed, "the first time OneDrive states another content");
    assert_eq!(decide_end_news(&another_content, true), DownloadEndNews::AnotherContentAgain, "OneDrive states what is remembered of the key");
    // Whatever else it ended as is told, also where OneDrive did not answer about the content.
    for end in [Ok(ItemLandingOutcome::RowsChanged), Ok(ItemLandingOutcome::Failed { errno: libc::EIO }), Err(DownloadRefusal::ItemGone), Err(DownloadRefusal::SignedOut), Err(DownloadRefusal::Transient { detail: "no network".into() })] {
        assert_eq!(decide_end_news(&end, true), DownloadEndNews::Changed, "{end:?}");
        assert_eq!(decide_end_news(&end, false), DownloadEndNews::Changed, "{end:?}");
    }
}

#[test]
fn a_content_onedrive_keeps_stating_another_one_for_is_told_once() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let item = shown.local_item_id;
    // OneDrive has another content by now, which no listing has told the rows yet.
    fixture.serve_content(item, b"THE REPORT");
    let report = make_report(&fixture);
    let (downloader, _) = make_downloader_reporting_to(&fixture, fixture.content_source(), report.clone());
    let count_events = |kind: EventKind| report.activity.recent(100).expect("the activity kept in memory").iter().filter(|event| event.kind == kind).count();
    let count_announcements = || fixture.inotify_bridge.list_queued().iter().flatten().filter(|announcement| matches!(announcement, ItemAnnouncement::ContentChanged { item: announced, .. } if *announced == item)).count();
    let announced_before = count_announcements();

    // The first time the content is found to be another one: everything is told.
    assert_eq!(hear(ask(&downloader, &shown, DownloadAsker::Read)), Err(libc::ESTALE));
    assert_eq!((count_events(EventKind::Failed), count_announcements() - announced_before), (1, 2));

    // Each read asks OneDrive anew, which states the same: nothing changed to be told.
    for _read in 0..3 {
        assert_eq!(hear(ask(&downloader, &shown, DownloadAsker::Read)), Err(libc::ESTALE));
    }
    assert!(fixture.download_states.is_another_content(&key_of(&shown)));
    assert_eq!((count_events(EventKind::Failed), count_announcements() - announced_before), (1, 2), "the failure and the announcements of the first download only");

    // OneDrive states the asked content again: the download is told as any is, from its
    // first answer on.
    fixture.serve_content(item, b"the report");
    assert_eq!(hear(ask(&downloader, &shown, DownloadAsker::Read)), Ok(()));
    assert_eq!((count_events(EventKind::Failed), count_events(EventKind::Downloaded), count_announcements() - announced_before), (1, 1, 3));
}

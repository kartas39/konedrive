use tokio::sync::oneshot;

use super::*;

/// Once 100 bytes of a file were read as ranges, it is downloaded whole.
fn download_states() -> ItemDownloadStates {
    ItemDownloadStates::new(100)
}

fn key(item: i64, ctag: &str) -> DownloadKey {
    DownloadKey { item: LocalItemId::new(item), content: ContentVersion::Hashed { size: 10, mtime: 1_700_000_000, mtime_nsec: 0, hash: None, ctag: Some(ctag.into()) } }
}

/// The plan's pin, as its step asks: with the versions the step was planned from.
fn plan_pin() -> DownloadAsker {
    let planned_from = ItemVersions {
        local_item_id: LocalItemId::new(5),
        onedrive_item: None,
        local: None,
        onedrive: None,
        onedrive_etag: None,
        synced_place: None,
        synced_content: None,
        content_presence: konedrive_tree::ContentPresence::NotOnDisk,
        unsent_since: None,
    };
    DownloadAsker::PlanPin { planned_from }
}

fn unnamed_file() -> Arc<File> {
    Arc::new(tempfile::tempfile().unwrap())
}

/// A download of `key` is asked for by a read that does not wait. The download it
/// started, with the unfinished file it is to go on from; `None` when one runs already.
fn start(download_states: &ItemDownloadStates, key: &DownloadKey) -> Option<(DownloadRun, Option<Arc<File>>)> {
    start_as(download_states, key, DownloadAsker::Read, None)
}

fn start_as(download_states: &ItemDownloadStates, key: &DownloadKey, asker: DownloadAsker, waiter: Option<Waiter>) -> Option<(DownloadRun, Option<Arc<File>>)> {
    match download_states.start_or_join(key, &asker, waiter) {
        ItemDownloadStart::Started { run, unfinished_file } => Some((run, unfinished_file)),
        ItemDownloadStart::Joined | ItemDownloadStart::Stopped => None,
    }
}

/// A download of `key` is started, with no unfinished file to go on from.
fn start_anew(download_states: &ItemDownloadStates, key: &DownloadKey) -> DownloadRun {
    let (run, unfinished_file) = start(download_states, key).expect("no download of it runs");
    assert!(unfinished_file.is_none());
    run
}

/// A download of `key` that got its file and failed: the file it left.
fn fail_download(download_states: &ItemDownloadStates, key: &DownloadKey) -> Arc<File> {
    let (run, unfinished_file) = start(download_states, key).expect("no download of it runs");
    let file = unfinished_file.unwrap_or_else(unnamed_file);
    download_states.begin_writing(&run, Arc::clone(&file));
    download_states.finish(&run, Err(libc::EIO));
    file
}

/// A task that never ends by itself, as a download that waits for its bytes.
fn spawn_download() -> tokio::task::JoinHandle<()> {
    tokio::spawn(std::future::pending::<()>())
}

#[test]
fn a_second_asking_for_the_same_item_and_content_joins_the_running_download() {
    let download_states = download_states();
    let item = LocalItemId::new(5);
    assert!(!download_states.is_downloading(item));
    let run = start_anew(&download_states, &key(5, "c1"));
    assert_eq!(run.key(), &key(5, "c1"));
    assert!(download_states.is_downloading(item));
    assert!(start(&download_states, &key(5, "c1")).is_none(), "joined");
    download_states.finish(&run, Ok(()));
    assert!(!download_states.is_downloading(item));
    start_anew(&download_states, &key(5, "c1"));
}

#[test]
fn an_asking_for_another_content_of_the_item_starts_another_download() {
    let download_states = download_states();
    let first = start_anew(&download_states, &key(5, "c1"));
    let second = start_anew(&download_states, &key(5, "c2"));
    let other = start_anew(&download_states, &key(6, "c1"));
    // The item is downloading while a download of any content of it runs.
    download_states.finish(&first, Ok(()));
    assert!(download_states.is_downloading(LocalItemId::new(5)));
    download_states.finish(&second, Err(libc::ESTALE));
    assert!(!download_states.is_downloading(LocalItemId::new(5)) && download_states.is_downloading(LocalItemId::new(6)));
    // The file and the watermark a read is answered from are of the content asked.
    let file = unnamed_file();
    assert!(download_states.find_running_download(&key(6, "c1")).is_none(), "no file yet");
    let written_up_to = download_states.begin_writing(&other, Arc::clone(&file));
    written_up_to.store(7, std::sync::atomic::Ordering::SeqCst);
    let running = download_states.find_running_download(&key(6, "c1")).unwrap();
    assert!(Arc::ptr_eq(&running.file, &file) && running.written_up_to == 7);
    assert!(download_states.find_running_download(&key(6, "c2")).is_none());
}

#[test]
fn every_waiter_is_answered_when_the_download_ends_however_it_ends() {
    let download_states = download_states();
    let ((first, first_told), (second, second_told)) = (oneshot::channel(), oneshot::channel());
    let (run, _) = start_as(&download_states, &key(5, "c1"), DownloadAsker::Read, Some(first)).unwrap();
    assert!(start_as(&download_states, &key(5, "c1"), plan_pin(), Some(second)).is_none());
    download_states.finish(&run, Ok(()));
    assert_eq!((first_told.blocking_recv(), second_told.blocking_recv()), (Ok(Ok(())), Ok(Ok(()))));

    let (waiter, told) = oneshot::channel();
    let (run, _) = start_as(&download_states, &key(5, "c1"), DownloadAsker::OpenForWriting, Some(waiter)).unwrap();
    download_states.finish(&run, Err(libc::ENOSPC));
    assert_eq!(told.blocking_recv(), Ok(Err(libc::ENOSPC)));

    // Dropped without an end.
    let (waiter, told) = oneshot::channel();
    let (run, _) = start_as(&download_states, &key(5, "c1"), DownloadAsker::FilesInterface, Some(waiter)).unwrap();
    download_states.abandon(&run);
    assert_eq!(told.blocking_recv(), Ok(Err(libc::EIO)));
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
}

/// What a download says after it was aborted, or twice, is of a download that runs no
/// more: the next one of the same item and content is another.
#[test]
fn an_end_told_by_an_older_download_of_the_key_changes_nothing_of_the_newer() {
    let download_states = download_states();
    let (older, _) = start_as(&download_states, &key(5, "c1"), plan_pin(), None).unwrap();
    assert!(download_states.abort_if_plan_alone_asked(&key(5, "c1")));
    let (waiter, mut told) = oneshot::channel();
    let (newer, _) = start_as(&download_states, &key(5, "c1"), DownloadAsker::Read, Some(waiter)).unwrap();
    assert_ne!(older, newer, "of one key, and not the same download");
    // The older task's guard, its late end, its late file.
    download_states.abandon(&older);
    download_states.finish(&older, Err(libc::EIO));
    download_states.finish(&older, Ok(()));
    let late_file = unnamed_file();
    download_states.begin_writing(&older, Arc::clone(&late_file));
    assert!(download_states.is_downloading(LocalItemId::new(5)), "the newer download still runs");
    assert!(told.try_recv().is_err(), "and its waiter was told nothing");
    assert!(download_states.find_running_download(&key(5, "c1")).is_none(), "nor is it given the older one's file");
    assert_eq!(Arc::strong_count(&late_file), 1);
    download_states.finish(&newer, Ok(()));
    assert_eq!(told.blocking_recv(), Ok(Ok(())));
    // An end told twice changes nothing either.
    download_states.finish(&newer, Err(libc::EIO));
    start_anew(&download_states, &key(5, "c1"));
}

#[test]
fn a_failed_download_keeps_its_unfinished_file_for_the_next_asking_of_that_content() {
    let download_states = download_states();
    let file = fail_download(&download_states, &key(5, "c1"));
    assert!(!download_states.is_downloading(LocalItemId::new(5)), "a failed download does not run");
    let other = start_anew(&download_states, &key(5, "c2"));
    download_states.finish(&other, Ok(()));
    let (run, kept) = start(&download_states, &key(5, "c1")).unwrap();
    assert!(Arc::ptr_eq(&kept.unwrap(), &file));
    // One that failed before it had a file keeps nothing; nor does one that was abandoned.
    download_states.finish(&run, Err(libc::EIO));
    let run = start_anew(&download_states, &key(5, "c1"));
    download_states.begin_writing(&run, unnamed_file());
    download_states.abandon(&run);
    start_anew(&download_states, &key(5, "c1"));
    // The latest files are kept, and the oldest beyond them let go.
    let download_states = self::download_states();
    let files: Vec<Arc<File>> = (0..UNFINISHED_FILES_KEPT as i64 + 1).map(|item| fail_download(&download_states, &key(item, "c"))).collect();
    assert_eq!(Arc::strong_count(&files[0]), 1, "the oldest is let go");
    start_anew(&download_states, &key(0, "c"));
    assert!(files[1..].iter().all(|file| Arc::strong_count(file) == 2));
    assert!(Arc::ptr_eq(&start(&download_states, &key(1, "c")).unwrap().1.unwrap(), &files[1]));
}

#[test]
fn the_bytes_read_as_ranges_reach_the_threshold_once_and_are_forgotten_with_the_item() {
    let download_states = download_states();
    let item = LocalItemId::new(5);
    assert!(!download_states.count_bytes_read(item, 60, 1000));
    assert!(!download_states.count_bytes_read(LocalItemId::new(6), 60, 1000), "each item has a count of its own");
    assert!(download_states.count_bytes_read(item, 40, 1000), "100 bytes of it were read");
    download_states.forget_item(item);
    assert!(!download_states.count_bytes_read(item, 60, 1000), "counted from nothing again");
    // A file smaller than the threshold reaches it once all of it was read.
    assert!(download_states.count_bytes_read(LocalItemId::new(7), 30, 30));
    // A download that ended well ends the count too.
    let run = start_anew(&download_states, &key(5, "c1"));
    download_states.finish(&run, Ok(()));
    assert!(!download_states.count_bytes_read(item, 60, 1000));
    // The unfinished files of the item go with it, whatever content they were of; a
    // download of it that runs is left to its landing.
    let download_states = self::download_states();
    let (first, second) = (fail_download(&download_states, &key(5, "c1")), fail_download(&download_states, &key(5, "c2")));
    let other = fail_download(&download_states, &key(6, "c1"));
    start_anew(&download_states, &key(5, "c3"));
    download_states.forget_item(item);
    assert_eq!((Arc::strong_count(&first), Arc::strong_count(&second), Arc::strong_count(&other)), (1, 1, 2));
    assert!(download_states.is_downloading(item));
}

/// Decided by who asked, not by who still listens: the plan's waiter is heard here.
#[tokio::test]
async fn a_download_only_the_plan_asked_for_is_aborted_by_its_cancel_and_keeps_its_unfinished_file() {
    let download_states = download_states();
    let (plan_waiter, plan_told) = oneshot::channel();
    let (run, unfinished_file) = start_as(&download_states, &key(5, "c1"), plan_pin(), Some(plan_waiter)).unwrap();
    assert!(unfinished_file.is_none());
    let task = spawn_download();
    download_states.keep_abort_handle(&run, task.abort_handle());
    let file = unnamed_file();
    download_states.begin_writing(&run, Arc::clone(&file));
    assert!(download_states.abort_if_plan_alone_asked(&key(5, "c1")));
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(plan_told.await, Ok(Err(libc::EIO)), "its waiter is answered");
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
    assert!(!download_states.abort_if_plan_alone_asked(&key(5, "c1")), "nothing runs: nothing is aborted");
    // The aborted task's guard changes nothing; the unfinished file is kept as after a
    // failure, for whoever asks next.
    download_states.abandon(&run);
    let (_, kept) = start_as(&download_states, &key(5, "c1"), plan_pin(), None).unwrap();
    assert!(Arc::ptr_eq(&kept.unwrap(), &file));
    assert!(download_states.abort_if_plan_alone_asked(&key(5, "c1")), "the plan alone, with no waiter");
}

#[tokio::test]
async fn a_download_anything_but_the_plan_asked_for_is_not_aborted_by_the_plans_cancel() {
    let download_states = download_states();
    // Each of the three joins the plan's download; a read asks with no waiter.
    for (item, asker) in [(5, DownloadAsker::Read), (6, DownloadAsker::OpenForWriting), (7, DownloadAsker::FilesInterface)] {
        let key = key(item, "c1");
        let (plan_waiter, plan_told) = oneshot::channel();
        let (run, _) = start_as(&download_states, &key, plan_pin(), Some(plan_waiter)).unwrap();
        let task = spawn_download();
        download_states.keep_abort_handle(&run, task.abort_handle());
        assert!(start_as(&download_states, &key, asker.clone(), None).is_none());
        // The plan's step was aborted: nobody hears its waiter.
        drop(plan_told);
        assert!(!download_states.abort_if_plan_alone_asked(&key), "{asker:?} wants it");
        tokio::task::yield_now().await;
        assert!(!task.is_finished() && download_states.is_downloading(LocalItemId::new(item)));
        download_states.finish(&run, Ok(()));
        task.abort();
    }
    // The other way round: a read began it, and the plan joined.
    let (run, _) = start_as(&download_states, &key(8, "c1"), DownloadAsker::Read, None).unwrap();
    let task = spawn_download();
    download_states.keep_abort_handle(&run, task.abort_handle());
    assert!(start_as(&download_states, &key(8, "c1"), plan_pin(), None).is_none());
    assert!(!download_states.abort_if_plan_alone_asked(&key(8, "c1")));
    assert!(download_states.is_downloading(LocalItemId::new(8)));
    task.abort();
}

#[tokio::test]
async fn every_download_is_aborted_and_its_waiters_answered_when_the_mount_stops() {
    let download_states = download_states();
    let (waiter, told) = oneshot::channel();
    let (first_run, _) = start_as(&download_states, &key(5, "c1"), DownloadAsker::Read, Some(waiter)).unwrap();
    let first = spawn_download();
    download_states.keep_abort_handle(&first_run, first.abort_handle());
    // One whose task was spawned and whose handle is not kept yet.
    let second_run = start_anew(&download_states, &key(6, "c1"));
    let second = spawn_download();
    let failed = fail_download(&download_states, &key(7, "c1"));
    download_states.stop();
    assert_eq!(told.await, Ok(Err(libc::EIO)));
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(!download_states.is_downloading(LocalItemId::new(5)) && !download_states.is_downloading(LocalItemId::new(6)));
    assert_eq!(Arc::strong_count(&failed), 2, "what a failed download left is kept");
    // The handle that comes after the stop aborts its task at once.
    download_states.keep_abort_handle(&second_run, second.abort_handle());
    assert!(second.await.unwrap_err().is_cancelled());
}

/// A handle that comes after its download was aborted aborts its task at once, also when
/// the next download of the key runs by then, whose own handle is another.
#[tokio::test]
async fn a_handle_kept_after_the_abort_aborts_its_own_task_and_leaves_the_next_download_of_the_key() {
    let download_states = download_states();
    let (aborted_run, _) = start_as(&download_states, &key(6, "c1"), plan_pin(), None).unwrap();
    let aborted = spawn_download();
    assert!(download_states.abort_if_plan_alone_asked(&key(6, "c1")));
    let next_run = start_anew(&download_states, &key(6, "c1"));
    let next = spawn_download();
    download_states.keep_abort_handle(&next_run, next.abort_handle());
    download_states.keep_abort_handle(&aborted_run, aborted.abort_handle());
    assert!(aborted.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;
    assert!(!next.is_finished(), "the newer download keeps its own handle");
    download_states.stop();
    assert!(next.await.unwrap_err().is_cancelled());
}

#[test]
fn a_download_whose_file_is_whole_keeps_no_unfinished_file_however_it_ends() {
    let download_states = download_states();
    // It failed at its landing: the file had a name, perhaps, and lost it.
    let run = start_anew(&download_states, &key(5, "c1"));
    let whole = unnamed_file();
    download_states.begin_writing(&run, Arc::clone(&whole));
    download_states.end_writing(&run);
    assert!(download_states.find_running_download(&key(5, "c1")).is_some(), "a read is answered from it while the download runs");
    download_states.finish(&run, Err(libc::ENOENT));
    assert_eq!(Arc::strong_count(&whole), 1, "the file is let go");
    start_anew(&download_states, &key(5, "c1"));
    // The plan gave it up while its landing ran.
    let (run, _) = start_as(&download_states, &key(6, "c1"), plan_pin(), None).unwrap();
    download_states.begin_writing(&run, Arc::clone(&whole));
    download_states.end_writing(&run);
    assert!(download_states.abort_if_plan_alone_asked(&key(6, "c1")));
    assert_eq!(Arc::strong_count(&whole), 1);
    start_anew(&download_states, &key(6, "c1"));
    // What an older download of the key says of its file changes nothing of the newer one's.
    let older = start_anew(&download_states, &key(7, "c1"));
    download_states.finish(&older, Ok(()));
    let newer = start_anew(&download_states, &key(7, "c1"));
    let unfinished = unnamed_file();
    download_states.begin_writing(&newer, Arc::clone(&unfinished));
    download_states.end_writing(&older);
    download_states.finish(&newer, Err(libc::EIO));
    assert!(Arc::ptr_eq(&start(&download_states, &key(7, "c1")).unwrap().1.unwrap(), &unfinished));
}

#[test]
fn a_download_onedrive_answered_with_another_content_is_remembered_until_the_next_download_of_the_key_is_answered() {
    let download_states = download_states();
    assert!(!download_states.is_another_content(&key(5, "c1")));
    let (waiter, told) = oneshot::channel();
    let (run, _) = start_as(&download_states, &key(5, "c1"), DownloadAsker::Read, Some(waiter)).unwrap();
    let file = unnamed_file();
    download_states.begin_writing(&run, Arc::clone(&file));
    assert!(!download_states.is_another_content(&key(5, "c1")), "nothing is known while it runs");

    download_states.finish_as_another_content(&run);

    assert_eq!(told.blocking_recv(), Ok(Err(libc::ESTALE)));
    assert!(download_states.is_another_content(&key(5, "c1")));
    assert!(!download_states.is_another_content(&key(5, "c2")), "of that content alone");
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
    assert_eq!(Arc::strong_count(&file), 1, "nothing of its file is kept");
    // Said twice, or by a download that runs no more: nothing.
    download_states.finish_as_another_content(&run);
    download_states.finish(&run, Ok(()));
    assert!(download_states.is_another_content(&key(5, "c1")));
    // Asked again: remembered until OneDrive has answered, not until it is asked.
    let older = start_anew(&download_states, &key(5, "c1"));
    assert!(download_states.is_another_content(&key(5, "c1")), "the download that asks anew has no answer yet");
    assert!(download_states.is_downloading(LocalItemId::new(5)));
    // It ends with no answer, however it ends: remembered as before.
    download_states.finish(&older, Err(libc::EIO));
    assert!(download_states.is_another_content(&key(5, "c1")), "a failure with no answer");
    let (run, unfinished_file) = start(&download_states, &key(5, "c1")).unwrap();
    assert!(unfinished_file.is_none(), "and no file is gone on from: no answer was written into one");
    download_states.begin_writing(&run, unnamed_file());
    download_states.abandon(&run);
    assert!(download_states.is_another_content(&key(5, "c1")), "a download that was dropped");
    let run = start_as(&download_states, &key(5, "c1"), plan_pin(), None).unwrap().0;
    assert!(download_states.abort_if_plan_alone_asked(&key(5, "c1")));
    assert!(download_states.is_another_content(&key(5, "c1")), "one the plan gave up");
    download_states.forget_another_content(&run);
    assert!(download_states.is_another_content(&key(5, "c1")), "what a download that runs no more accepted changes nothing");
    // An answer that states another content again: remembered still.
    let run = start_anew(&download_states, &key(5, "c1"));
    download_states.finish_as_another_content(&run);
    assert!(download_states.is_another_content(&key(5, "c1")));
    // An answer of the key's content: void from that answer on, while the download runs.
    let run = start_anew(&download_states, &key(5, "c1"));
    download_states.forget_another_content(&older);
    assert!(download_states.is_another_content(&key(5, "c1")), "an older download of the key says nothing of this one");
    download_states.forget_another_content(&run);
    assert!(!download_states.is_another_content(&key(5, "c1")));
    download_states.finish(&run, Err(libc::EIO));
    assert!(!download_states.is_another_content(&key(5, "c1")), "a failure after an answer of the key's content");
    // Forgotten with the item, also while the next download has no answer.
    download_states.finish_as_another_content(&start_anew(&download_states, &key(5, "c1")));
    download_states.forget_item(LocalItemId::new(5));
    assert!(!download_states.is_another_content(&key(5, "c1")));
    download_states.finish_as_another_content(&start_anew(&download_states, &key(5, "c1")));
    let _run = start_anew(&download_states, &key(5, "c1"));
    download_states.forget_item(LocalItemId::new(5));
    assert!(!download_states.is_another_content(&key(5, "c1")));
}

#[test]
fn a_download_asked_for_after_the_stop_is_not_started() {
    let download_states = download_states();
    let running = start_anew(&download_states, &key(5, "c1"));
    download_states.stop();
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
    for asker in [DownloadAsker::Read, DownloadAsker::OpenForWriting, DownloadAsker::FilesInterface, plan_pin()] {
        let (waiter, told) = oneshot::channel();
        assert!(matches!(download_states.start_or_join(&key(6, "c1"), &asker, Some(waiter)), ItemDownloadStart::Stopped), "{asker:?}");
        assert_eq!(told.blocking_recv(), Ok(Err(libc::EIO)), "{asker:?}");
    }
    assert!(matches!(download_states.start_or_join(&key(5, "c1"), &DownloadAsker::Read, None), ItemDownloadStart::Stopped));
    assert!(!download_states.is_downloading(LocalItemId::new(5)) && !download_states.is_downloading(LocalItemId::new(6)));
    // What the aborted download still says changes nothing.
    download_states.finish(&running, Ok(()));
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
}

#[tokio::test]
async fn a_download_of_the_plans_is_told_when_anything_else_asks_for_it() {
    let download_states = download_states();
    let is_told = |run: &DownloadRun| {
        let mut waited = std::pin::pin!(download_states.wait_for_asker_beyond_plan(run));
        std::future::Future::poll(waited.as_mut(), &mut std::task::Context::from_waker(std::task::Waker::noop())).is_ready()
    };
    // The plan's own, and the plan asking once more: not told.
    let (run, _) = start_as(&download_states, &key(5, "c1"), plan_pin(), None).unwrap();
    assert!(start_as(&download_states, &key(5, "c1"), plan_pin(), None).is_none());
    assert!(!is_told(&run));
    // Told while it waits, by each of the three that are not the plan.
    for (item, asker) in [(6, DownloadAsker::Read), (7, DownloadAsker::OpenForWriting), (8, DownloadAsker::FilesInterface)] {
        let key = key(item, "c1");
        let (run, _) = start_as(&download_states, &key, plan_pin(), None).unwrap();
        let waited = tokio::time::timeout(std::time::Duration::from_secs(20), download_states.wait_for_asker_beyond_plan(&run));
        let (told, joined) = tokio::join!(waited, async { start_as(&download_states, &key, asker.clone(), None).is_none() });
        assert!(joined, "{asker:?} joins");
        assert!(told.is_ok(), "{asker:?} asked for it");
    }
    // One that anything else started is told at once.
    let run = start_anew(&download_states, &key(9, "c1"));
    assert!(is_told(&run));
    // One that runs no more is never told: its task ends, or is aborted.
    download_states.finish(&run, Ok(()));
    assert!(!is_told(&run));
}

#[test]
fn the_count_of_bytes_read_as_ranges_starts_over_once_it_reached_the_threshold() {
    let download_states = download_states();
    let item = LocalItemId::new(5);
    assert!(!download_states.count_bytes_read(item, 99, 1000));
    assert!(download_states.count_bytes_read(item, 1, 1000), "100 bytes of it were read: a download is started");
    // The download ended without landing, or still runs: the next reads start none.
    assert!(!download_states.count_bytes_read(item, 4, 1000));
    assert!(!download_states.count_bytes_read(item, 95, 1000));
    assert!(download_states.count_bytes_read(item, 1, 1000), "as many bytes read again");
}

#[test]
fn no_more_than_the_latest_contents_onedrive_answered_with_another_are_remembered() {
    let download_states = download_states();
    let answer_with_another_content = |item: i64| download_states.finish_as_another_content(&start_anew(&download_states, &key(item, "c1")));
    let count_remembered = |items: std::ops::Range<i64>| items.filter(|item| download_states.is_another_content(&key(*item, "c1"))).count();
    let kept = ANOTHER_CONTENTS_REMEMBERED as i64;
    (0..kept).for_each(answer_with_another_content);
    assert_eq!(count_remembered(0..kept), ANOTHER_CONTENTS_REMEMBERED);

    // One more: the oldest goes.
    answer_with_another_content(kept);
    assert!(!download_states.is_another_content(&key(0, "c1")), "the oldest is forgotten");
    assert_eq!(count_remembered(0..kept + 1), ANOTHER_CONTENTS_REMEMBERED);
    // Answered so again, it is the latest: the next to go is the oldest by then.
    answer_with_another_content(1);
    answer_with_another_content(kept + 1);
    assert!(download_states.is_another_content(&key(1, "c1")) && !download_states.is_another_content(&key(2, "c1")));
    // A failed download's unfinished file is not counted among them, nor they among those.
    let failed = fail_download(&download_states, &key(900, "c1"));
    answer_with_another_content(kept + 2);
    assert!(Arc::ptr_eq(&start(&download_states, &key(900, "c1")).unwrap().1.unwrap(), &failed));
    assert_eq!(count_remembered(0..kept + 3), ANOTHER_CONTENTS_REMEMBERED);
}

#[test]
fn the_failure_of_a_download_onedrive_did_not_answer_is_kept_with_what_is_remembered_of_its_key() {
    let download_states = download_states();
    let key = key(5, "c1");
    let failed_asking = || download_states.find_another_content(&key).map(|remembered| remembered.failed_asking);
    assert_eq!(failed_asking(), None, "nothing is remembered of the key");
    download_states.finish_as_another_content(&start_anew(&download_states, &key));
    assert_eq!(failed_asking(), Some(None), "OneDrive answered its last download");

    // A download that fails with no answer: its errno stands for the key, also while
    // the next one runs and has no answer.
    download_states.finish(&start_anew(&download_states, &key), Err(libc::EIO));
    assert_eq!(failed_asking(), Some(Some(libc::EIO)));
    let run = start_anew(&download_states, &key);
    assert_eq!(failed_asking(), Some(Some(libc::EIO)), "what the running download is held against at its end");
    download_states.finish(&run, Err(libc::ESTALE));
    assert_eq!(failed_asking(), Some(Some(libc::ESTALE)), "the last failure");

    // A download that was given up or dropped failed with nothing: as before it.
    let run = start_anew(&download_states, &key);
    download_states.abandon(&run);
    assert_eq!(failed_asking(), Some(Some(libc::ESTALE)));
    start_as(&download_states, &key, plan_pin(), None).unwrap();
    assert!(download_states.abort_if_plan_alone_asked(&key));
    assert_eq!(failed_asking(), Some(Some(libc::ESTALE)));

    // OneDrive answers with another content again: no failure stands.
    download_states.finish_as_another_content(&start_anew(&download_states, &key));
    assert_eq!(failed_asking(), Some(None));

    // An answer of the key's content voids all of it; a failure after it is of a key
    // nothing is remembered of.
    download_states.finish(&start_anew(&download_states, &key), Err(libc::EIO));
    let run = start_anew(&download_states, &key);
    download_states.forget_another_content(&run);
    assert_eq!(failed_asking(), None);
    download_states.finish(&run, Err(libc::EIO));
    assert_eq!(failed_asking(), None);
}

#[test]
fn the_unfinished_files_are_those_of_the_running_downloads_that_are_not_whole_and_of_the_failed_ones() {
    let download_states = download_states();
    assert!(download_states.list_unfinished_files().is_empty());
    // Which of `files` are listed, each with its item.
    let list = |download_states: &ItemDownloadStates, files: &[(i64, &Arc<File>)]| -> Vec<i64> {
        let listed = download_states.list_unfinished_files();
        let known: Vec<i64> = files.iter().filter(|(item, file)| listed.iter().any(|(listed_item, listed_file)| *listed_item == LocalItemId::new(*item) && Arc::ptr_eq(listed_file, file))).map(|(item, _)| *item).collect();
        assert_eq!(known.len(), listed.len(), "nothing is listed but the files given");
        known
    };

    // A running download: nothing before it writes into a file, its file from then on.
    let running = start_anew(&download_states, &key(5, "c1"));
    assert!(download_states.list_unfinished_files().is_empty(), "no file yet");
    let running_file = unnamed_file();
    download_states.begin_writing(&running, Arc::clone(&running_file));
    // One that failed with a file written in part: kept, and listed.
    let failed_file = fail_download(&download_states, &key(6, "c2"));
    // One whose file is whole and on its way to its landing: not listed.
    let whole = start_anew(&download_states, &key(7, "c3"));
    let whole_file = unnamed_file();
    download_states.begin_writing(&whole, Arc::clone(&whole_file));
    download_states.end_writing(&whole);
    // One OneDrive answered with another content: nothing of its file is kept.
    let another = start_anew(&download_states, &key(8, "c4"));
    let another_file = unnamed_file();
    download_states.begin_writing(&another, Arc::clone(&another_file));
    download_states.finish_as_another_content(&another);
    let files = [(5, &running_file), (6, &failed_file), (7, &whole_file), (8, &another_file)];
    assert_eq!(list(&download_states, &files), [5, 6]);

    // The running one landed: gone. The failed one is gone on from: its file is the running download's.
    download_states.end_writing(&running);
    download_states.finish(&running, Ok(()));
    let (again, unfinished_file) = start(&download_states, &key(6, "c2")).expect("no download of it runs");
    assert!(unfinished_file.is_some_and(|file| Arc::ptr_eq(&file, &failed_file)));
    assert_eq!(list(&download_states, &files), Vec::<i64>::new(), "a download that goes on has no file before it writes into it");
    download_states.begin_writing(&again, Arc::clone(&failed_file));
    assert_eq!(list(&download_states, &files), [6]);
    // What is kept of an item goes with it: the file of a download that failed, not that of one that runs, which is left to its landing.
    download_states.forget_item(LocalItemId::new(6));
    assert_eq!(list(&download_states, &files), [6]);
    download_states.finish(&again, Err(libc::EIO));
    assert_eq!(list(&download_states, &files), [6]);
    download_states.forget_item(LocalItemId::new(6));
    assert_eq!(list(&download_states, &files), Vec::<i64>::new());
}

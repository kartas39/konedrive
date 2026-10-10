use tokio::sync::oneshot;

use super::*;

/// Once 100 bytes of a file were read as ranges, it is downloaded whole.
fn download_states() -> ItemDownloadStates {
    ItemDownloadStates::new(100)
}

fn key(item: i64, ctag: &str) -> DownloadKey {
    DownloadKey { item: LocalItemId::new(item), content: ContentVersion::Hashed { size: 10, mtime: 1_700_000_000, mtime_nsec: 0, hash: None, ctag: Some(ctag.into()) } }
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
    match download_states.start_or_join(key, asker, waiter) {
        ItemDownloadStart::Started { run, unfinished_file } => Some((run, unfinished_file)),
        ItemDownloadStart::Joined => None,
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
    assert!(start_as(&download_states, &key(5, "c1"), DownloadAsker::PlanPin, Some(second)).is_none());
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
    let older = start_anew(&download_states, &key(5, "c1"));
    download_states.abort_all();
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

#[test]
fn the_waiters_are_failed_when_the_mount_stops() {
    let download_states = download_states();
    let ((first, first_told), (second, second_told)) = (oneshot::channel(), oneshot::channel());
    let (run, _) = start_as(&download_states, &key(5, "c1"), DownloadAsker::Read, Some(first)).unwrap();
    start_as(&download_states, &key(6, "c1"), DownloadAsker::PlanPin, Some(second)).unwrap();
    download_states.fail_waiters();
    assert_eq!((first_told.blocking_recv(), second_told.blocking_recv()), (Ok(Err(libc::EIO)), Ok(Err(libc::EIO))));
    assert!(download_states.is_downloading(LocalItemId::new(5)), "a download that still runs ends as any other, with nobody waiting");
    download_states.finish(&run, Ok(()));
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
}

/// Decided by who asked, not by who still listens: the plan's waiter is heard here.
#[tokio::test]
async fn a_download_only_the_plan_asked_for_is_aborted_by_its_cancel_and_keeps_its_unfinished_file() {
    let download_states = download_states();
    let (plan_waiter, plan_told) = oneshot::channel();
    let (run, unfinished_file) = start_as(&download_states, &key(5, "c1"), DownloadAsker::PlanPin, Some(plan_waiter)).unwrap();
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
    let (_, kept) = start_as(&download_states, &key(5, "c1"), DownloadAsker::PlanPin, None).unwrap();
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
        let (run, _) = start_as(&download_states, &key, DownloadAsker::PlanPin, Some(plan_waiter)).unwrap();
        let task = spawn_download();
        download_states.keep_abort_handle(&run, task.abort_handle());
        assert!(start_as(&download_states, &key, asker, None).is_none());
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
    assert!(start_as(&download_states, &key(8, "c1"), DownloadAsker::PlanPin, None).is_none());
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
    download_states.abort_all();
    assert_eq!(told.await, Ok(Err(libc::EIO)));
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(!download_states.is_downloading(LocalItemId::new(5)) && !download_states.is_downloading(LocalItemId::new(6)));
    // The handle that comes after the abort aborts its task at once: also when the next
    // download of the key runs by then, whose own handle is another.
    let next_run = start_anew(&download_states, &key(6, "c1"));
    let next = spawn_download();
    download_states.keep_abort_handle(&next_run, next.abort_handle());
    download_states.keep_abort_handle(&second_run, second.abort_handle());
    assert!(second.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;
    assert!(!next.is_finished(), "the newer download keeps its own handle");
    download_states.abort_all();
    assert!(next.await.unwrap_err().is_cancelled());
    assert_eq!(Arc::strong_count(&failed), 2, "what a failed download left is kept");
}

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

/// A download of `key` is asked for by nobody who waits. The unfinished file it is to go
/// on from, when it was started; `None` when one runs already.
fn start(download_states: &ItemDownloadStates, key: &DownloadKey) -> Option<Option<Arc<File>>> {
    match download_states.start_or_join(key, None) {
        ItemDownloadStart::Started { unfinished_file } => Some(unfinished_file),
        ItemDownloadStart::Joined => None,
    }
}

/// A download of `key` that got its file and failed: the file it left.
fn fail_download(download_states: &ItemDownloadStates, key: &DownloadKey) -> Arc<File> {
    let file = start(download_states, key).expect("no download of it runs").unwrap_or_else(unnamed_file);
    download_states.begin_writing(key, Arc::clone(&file));
    download_states.finish(key, Err(libc::EIO));
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
    assert!(matches!(start(&download_states, &key(5, "c1")), Some(None)));
    assert!(download_states.is_downloading(item));
    assert!(start(&download_states, &key(5, "c1")).is_none(), "joined");
    download_states.finish(&key(5, "c1"), Ok(()));
    assert!(!download_states.is_downloading(item));
    assert!(matches!(start(&download_states, &key(5, "c1")), Some(None)), "it ended: the next asking starts one");
}

#[test]
fn an_asking_for_another_content_of_the_item_starts_another_download() {
    let download_states = download_states();
    assert!(start(&download_states, &key(5, "c1")).is_some());
    assert!(start(&download_states, &key(5, "c2")).is_some(), "another content is another download");
    assert!(start(&download_states, &key(6, "c1")).is_some(), "and so is another item");
    // The item is downloading while a download of any content of it runs.
    download_states.finish(&key(5, "c1"), Ok(()));
    assert!(download_states.is_downloading(LocalItemId::new(5)));
    download_states.finish(&key(5, "c2"), Err(libc::ESTALE));
    assert!(!download_states.is_downloading(LocalItemId::new(5)) && download_states.is_downloading(LocalItemId::new(6)));
    // The file and the watermark a read is answered from are of the content asked.
    let file = unnamed_file();
    assert!(download_states.find_running_download(&key(6, "c1")).is_none(), "no file yet");
    let written_up_to = download_states.begin_writing(&key(6, "c1"), Arc::clone(&file));
    written_up_to.store(7, std::sync::atomic::Ordering::SeqCst);
    let running = download_states.find_running_download(&key(6, "c1")).unwrap();
    assert!(Arc::ptr_eq(&running.file, &file) && running.written_up_to == 7);
    assert!(download_states.find_running_download(&key(6, "c2")).is_none());
}

#[test]
fn every_waiter_is_answered_when_the_download_ends_however_it_ends() {
    let download_states = download_states();
    let ((first, first_told), (second, second_told)) = (oneshot::channel(), oneshot::channel());
    download_states.start_or_join(&key(5, "c1"), Some(first));
    download_states.start_or_join(&key(5, "c1"), Some(second));
    download_states.finish(&key(5, "c1"), Ok(()));
    assert_eq!((first_told.blocking_recv(), second_told.blocking_recv()), (Ok(Ok(())), Ok(Ok(()))));

    let (waiter, told) = oneshot::channel();
    download_states.start_or_join(&key(5, "c1"), Some(waiter));
    download_states.finish(&key(5, "c1"), Err(libc::ENOSPC));
    assert_eq!(told.blocking_recv(), Ok(Err(libc::ENOSPC)));

    // Dropped without an end.
    let (waiter, told) = oneshot::channel();
    download_states.start_or_join(&key(5, "c1"), Some(waiter));
    download_states.abandon(&key(5, "c1"));
    assert_eq!(told.blocking_recv(), Ok(Err(libc::EIO)));
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
    // An end told of what does not run changes nothing.
    download_states.abandon(&key(5, "c1"));
    download_states.finish(&key(5, "c1"), Ok(()));
    assert!(matches!(start(&download_states, &key(5, "c1")), Some(None)));
}

#[test]
fn a_failed_download_keeps_its_unfinished_file_for_the_next_asking_of_that_content() {
    let download_states = download_states();
    let file = fail_download(&download_states, &key(5, "c1"));
    assert!(!download_states.is_downloading(LocalItemId::new(5)), "a failed download does not run");
    assert!(matches!(start(&download_states, &key(5, "c2")), Some(None)), "another content does not go on from it");
    let kept = start(&download_states, &key(5, "c1")).unwrap().unwrap();
    assert!(Arc::ptr_eq(&kept, &file));
    // One that failed before it had a file keeps nothing; nor does one that was abandoned.
    download_states.finish(&key(5, "c1"), Err(libc::EIO));
    assert!(matches!(start(&download_states, &key(5, "c1")), Some(None)));
    download_states.begin_writing(&key(5, "c1"), unnamed_file());
    download_states.abandon(&key(5, "c1"));
    assert!(matches!(start(&download_states, &key(5, "c1")), Some(None)));
    // The latest files are kept, and the oldest beyond them let go.
    let download_states = self::download_states();
    let files: Vec<Arc<File>> = (0..UNFINISHED_FILES_KEPT as i64 + 1).map(|item| fail_download(&download_states, &key(item, "c"))).collect();
    assert_eq!(Arc::strong_count(&files[0]), 1, "the oldest is let go");
    assert!(matches!(start(&download_states, &key(0, "c")), Some(None)));
    assert!(files[1..].iter().all(|file| Arc::strong_count(file) == 2));
    assert!(Arc::ptr_eq(&start(&download_states, &key(1, "c")).unwrap().unwrap(), &files[1]));
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
    start(&download_states, &key(5, "c1"));
    download_states.finish(&key(5, "c1"), Ok(()));
    assert!(!download_states.count_bytes_read(item, 60, 1000));
    // The unfinished files of the item go with it, whatever content they were of; a
    // download of it that runs is left to its landing.
    let download_states = self::download_states();
    let (first, second) = (fail_download(&download_states, &key(5, "c1")), fail_download(&download_states, &key(5, "c2")));
    let other = fail_download(&download_states, &key(6, "c1"));
    start(&download_states, &key(5, "c3"));
    download_states.forget_item(item);
    assert_eq!((Arc::strong_count(&first), Arc::strong_count(&second), Arc::strong_count(&other)), (1, 1, 2));
    assert!(download_states.is_downloading(item));
}

#[test]
fn the_waiters_are_failed_when_the_mount_stops() {
    let download_states = download_states();
    let ((first, first_told), (second, second_told)) = (oneshot::channel(), oneshot::channel());
    download_states.start_or_join(&key(5, "c1"), Some(first));
    download_states.start_or_join(&key(6, "c1"), Some(second));
    download_states.fail_waiters();
    assert_eq!((first_told.blocking_recv(), second_told.blocking_recv()), (Ok(Err(libc::EIO)), Ok(Err(libc::EIO))));
    assert!(download_states.is_downloading(LocalItemId::new(5)), "a download that still runs ends as any other, with nobody waiting");
    download_states.finish(&key(5, "c1"), Ok(()));
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
}

#[tokio::test]
async fn a_download_that_is_cancelled_is_aborted_and_its_waiters_answered() {
    let download_states = download_states();
    // The plan's step waited for it, and its task was aborted: nobody hears its waiter.
    let (plan_waiter, plan_told) = oneshot::channel();
    assert!(matches!(download_states.start_or_join(&key(5, "c1"), Some(plan_waiter)), ItemDownloadStart::Started { unfinished_file: None }));
    let task = spawn_download();
    download_states.keep_abort_handle(&key(5, "c1"), task.abort_handle());
    let file = unnamed_file();
    download_states.begin_writing(&key(5, "c1"), Arc::clone(&file));
    drop(plan_told);
    assert!(download_states.abort_if_unawaited(&key(5, "c1")));
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!download_states.is_downloading(LocalItemId::new(5)));
    assert!(!download_states.abort_if_unawaited(&key(5, "c1")), "nothing runs: nothing is aborted");
    // Its unfinished file is kept as after a failure.
    let kept = start(&download_states, &key(5, "c1")).unwrap().unwrap();
    assert!(Arc::ptr_eq(&kept, &file));
    // A download nobody ever waited for is aborted likewise.
    let task = spawn_download();
    assert!(start(&download_states, &key(6, "c1")).is_some());
    download_states.keep_abort_handle(&key(6, "c1"), task.abort_handle());
    assert!(download_states.abort_if_unawaited(&key(6, "c1")));
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn a_download_with_another_waiter_is_not_cancelled_for_the_plan() {
    let download_states = download_states();
    let (plan_waiter, plan_told) = oneshot::channel();
    let (read_waiter, read_told) = oneshot::channel();
    download_states.start_or_join(&key(5, "c1"), Some(plan_waiter));
    download_states.start_or_join(&key(5, "c1"), Some(read_waiter));
    let task = spawn_download();
    download_states.keep_abort_handle(&key(5, "c1"), task.abort_handle());
    drop(plan_told);
    assert!(!download_states.abort_if_unawaited(&key(5, "c1")), "a read still waits for it");
    tokio::task::yield_now().await;
    assert!(!task.is_finished() && download_states.is_downloading(LocalItemId::new(5)));
    download_states.finish(&key(5, "c1"), Ok(()));
    assert_eq!(read_told.await, Ok(Ok(())));
    task.abort();
}

#[tokio::test]
async fn every_download_is_aborted_and_its_waiters_answered_when_the_mount_stops() {
    let download_states = download_states();
    let (waiter, told) = oneshot::channel();
    download_states.start_or_join(&key(5, "c1"), Some(waiter));
    let first = spawn_download();
    download_states.keep_abort_handle(&key(5, "c1"), first.abort_handle());
    // One whose task was spawned and whose handle is not kept yet.
    assert!(start(&download_states, &key(6, "c1")).is_some());
    let second = spawn_download();
    let failed = fail_download(&download_states, &key(7, "c1"));
    download_states.abort_all();
    assert_eq!(told.await, Ok(Err(libc::EIO)));
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(!download_states.is_downloading(LocalItemId::new(5)) && !download_states.is_downloading(LocalItemId::new(6)));
    // The handle that comes after the abort aborts its task at once.
    download_states.keep_abort_handle(&key(6, "c1"), second.abort_handle());
    assert!(second.await.unwrap_err().is_cancelled());
    assert_eq!(Arc::strong_count(&failed), 2, "what a failed download left is kept");
}

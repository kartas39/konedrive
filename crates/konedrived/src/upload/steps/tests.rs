use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use super::{blocking, blocking_under, Sections};

/// A section started under a lock keeps it until the section is over, though the task that
/// started it is gone (the worker stopping): nobody takes the lock while the section still
/// works on the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_section_keeps_its_lock_after_its_task_is_dropped() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let (begun, has_begun) = mpsc::channel();
    let (end, may_end) = mpsc::channel::<()>();
    let task = {
        let lock = Arc::clone(&lock);
        tokio::spawn(async move {
            let tree = Arc::new(lock.lock_owned().await);
            blocking_under(Arc::clone(&tree), move || {
                begun.send(()).unwrap();
                may_end.recv().unwrap();
                Ok(())
            })
            .await
        })
    };
    tokio::task::spawn_blocking(move || has_begun.recv().unwrap()).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(lock.try_lock().is_err(), "the section is still running: the lock is still held");
    end.send(()).unwrap();
    let free = tokio::time::timeout(Duration::from_secs(10), lock.lock()).await;
    assert!(free.is_ok(), "the lock is free once the section has ended");
}

/// The worker's stop waits for a section of a row whose task it dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_waits_for_the_section_of_a_dropped_row() {
    let sections = Sections::default();
    let (begun, has_begun) = mpsc::channel();
    let (end, may_end) = mpsc::channel::<()>();
    let row = {
        let sections = sections.clone();
        tokio::spawn(async move {
            sections
                .of(blocking(move || {
                    begun.send(()).unwrap();
                    may_end.recv().unwrap();
                    Ok(())
                }))
                .await
        })
    };
    tokio::task::spawn_blocking(move || has_begun.recv().unwrap()).await.unwrap();
    row.abort();
    assert!(row.await.unwrap_err().is_cancelled());
    let stop = tokio::spawn(async move { sections.ended().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!stop.is_finished(), "the section is still running: the stop waits");
    end.send(()).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10), stop).await.is_ok(), "the stop ends once the section has");
}

/// The `If-Match` a request carries: the eTag, the cTag where there is no
/// eTag, and none with neither — except for an item just read, whose delete
/// then goes out with an empty one.
#[test]
fn a_guard_is_the_etag_then_the_ctag_and_empty_only_for_an_item_just_read() {
    use super::Guard;
    use konedrive_tree::outbox::Base;

    let of = |etag: Option<&str>, ctag: Option<&str>| Guard::of(etag, ctag).map(|g| g.as_str().to_owned());
    assert_eq!(of(Some("e"), Some("c")).as_deref(), Some("e"));
    assert_eq!(of(None, Some("c")).as_deref(), Some("c"));
    assert_eq!(of(None, None), None);
    let base = Base { etag: None, ctag: Some("c".into()), parent: None, name: None };
    assert_eq!(Guard::of_base(&base).unwrap().as_str(), "c");
    assert!(Guard::of_base(&Base::default()).is_none());

    let item = |json: &str| serde_json::from_str::<konedrive_graph::drive::DriveItem>(json).unwrap();
    assert_eq!(Guard::of_item(&item(r#"{"id":"I","eTag":"e","cTag":"c"}"#)).as_str(), "e");
    assert_eq!(Guard::of_item(&item(r#"{"id":"I","cTag":"c"}"#)).as_str(), "c");
    assert_eq!(Guard::of_item(&item(r#"{"id":"I"}"#)).as_str(), "");

    let guard = Guard::of(Some("e"), None).unwrap();
    assert_eq!(guard.clone().renewed(None).as_str(), "e");
    assert_eq!(guard.renewed(Some("e2".into())).as_str(), "e2");
}

/// A directory marked with a folder's id is that folder where the base places the folder
/// there, or where a row of the outbox says the folder is that directory: by the
/// directory's file handle when the row has one, by its place when it has none.
#[test]
fn a_marked_directory_is_its_folder_by_the_base_or_by_a_row_of_the_outbox() {
    use super::shared::is_marked_folder;
    use konedrive_fs::handle::FileHandle;
    use konedrive_tree::outbox::{OutboxKind, OutboxRow, OutboxState};
    use std::path::Path;

    let dir = tempfile::tempdir().unwrap();
    let handle_of = |name: &str| {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        FileHandle::at(&std::fs::File::open(dir.path()).unwrap(), std::ffi::OsStr::new(name)).unwrap()
    };
    let (this, other) = (handle_of("this"), handle_of("other"));
    let row = |kind: OutboxKind, rel: &str, object: Option<&FileHandle>| OutboxRow {
        seq: 1,
        kind,
        item_id: Some("N".into()),
        object: object.cloned(),
        rel: rel.into(),
        base: None,
        target_parent: Some("R".into()),
        target_name: Some("d".into()),
        state: OutboxState::Ready,
        reason: None,
        attempts: 0,
        next_try: None,
        snapshot: None,
        session_url: None,
        session_expires: None,
        session_next: None,
        confirmed: false,
        size: None,
    };
    let here = Path::new("d");
    let is = |base_place: Option<&str>, rows: &[OutboxRow]| is_marked_folder(here, Some(&this), base_place.map(Path::new), rows);

    assert!(is(Some("d"), &[]), "the base places the folder here");
    assert!(!is(Some("elsewhere"), &[]), "a copy that kept the mark: the folder is elsewhere, and no row says otherwise");
    assert!(!is(None, &[]), "the base places the folder nowhere");
    assert!(is(Some("elsewhere"), &[row(OutboxKind::Move, "d", Some(&this))]), "a row takes this very directory here");
    assert!(is(Some("elsewhere"), &[row(OutboxKind::Move, "d", None)]), "a row with no handle takes the folder to this place");
    assert!(!is(Some("elsewhere"), &[row(OutboxKind::Move, "d", Some(&other))]), "the row is of another directory");
    assert!(!is(Some("elsewhere"), &[row(OutboxKind::Move, "e", None)]), "the row takes the folder to another place");
    assert!(!is(Some("elsewhere"), &[row(OutboxKind::Delete, "d", Some(&this))]), "a row that removes the folder");
    let no_handle_here = is_marked_folder(here, None, Some(Path::new("elsewhere")), &[row(OutboxKind::Move, "d", Some(&other))]);
    assert!(no_handle_here, "with no handle read here, the row's place decides");
}

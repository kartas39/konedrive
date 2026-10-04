use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use konedrive_fs::placeholder;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};
use xattr::FileExt;

use super::*;
use crate::remote::listing::tests::*;
use crate::remote::testing::feed::{file, folder, root_item};
use crate::remote::testing::*;
use crate::remote::listing::*;
use crate::remote::materialize::{Failure, FailureReason, ReplaceOutcome, Replacement};
use crate::status::snapshot::{published_error, ReplacementNote, SyncStateHandle};

/// A listing of `s`, and the transfer pool its replacements wait for a slot of: paused, it
/// hands none out, and a replacement waits.
fn with_pool(s: &World) -> (Arc<Listing>, Arc<konedrive_graph::pool::TransferPool>) {
    let drive = s.drive();
    let pool = Arc::clone(drive.pool());
    let source = Arc::new(crate::hydration::graph_source::GraphSource::new(drive.clone()));
    (Listing::new(ListingContext { drive, source, ..s.context() }), pool)
}
/// A downloaded file that changed in OneDrive is replaced after the cycle by
/// its new version — another inode — and a pinned one is still pinned.
#[tokio::test]
async fn a_file_changed_in_the_cloud_is_replaced_after_the_cycle_and_keeps_its_pin() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    write_version(&f_txt, b"old conten", "c1");
    pin_by_hand(&s.root.path, "docs/f.txt");
    let before = ino(&f_txt);
    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.version("c2", &new)).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;

    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;

    assert_eq!(std::fs::read(&f_txt).unwrap(), new);
    assert_ne!(ino(&f_txt), before);
    assert_eq!(File::open(&f_txt).unwrap().get_xattr(placeholder::XATTR_PIN).unwrap(), Some(b"1".to_vec()));
}

/// When the new version cannot be had, the old one stays, the status says
/// why, the activity log has an `update-failed` event — not `failed`, which
/// is a download's — and it is tried again as it is; once it goes through,
/// the note is gone and the log says `updated`.
#[tokio::test]
async fn a_replacement_that_fails_is_said_and_tried_again() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    write_version(&f_txt, b"old conten", "c1");
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.graph.server).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "the old version stays");
    let note = s.state.get().replacement_note.expect("the status says it");
    assert_eq!(note.files, 1);
    assert!(published_error(&s.state.get()).contains("could not be updated"), "{note:?}");
    let (kind, at, why) = s.activity().pop().unwrap();
    assert_eq!((kind.as_str(), at.as_str()), ("update-failed", s.full("docs/f.txt").as_str()));
    assert!(why.contains("could not be downloaded"), "{why}");

    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.version("c2", &new)).await;
    s.feed(Some("L2"), json!([]), "L3").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(!report.full, "a failed replacement is retried as it is, with no Full reconcile");
    listing.join_replacements().await;
    assert_eq!(std::fs::read(&f_txt).unwrap(), new);
    assert_eq!(s.state.get().replacement_note, None);
    assert_eq!(s.activity().pop().unwrap(), ("updated".into(), s.full("docs/f.txt"), "11 B".into()));
    assert!(s.report.transfers.list().is_empty(), "no download is left showing");
}

/// A replacement the disk has no room for is an
/// `update-failed` event whose detail is exactly "not enough disk space"
/// — the words the window's notifier turns into "disk full".
#[tokio::test]
async fn a_replacement_with_no_room_on_the_disk_says_exactly_that() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    write_version(&f_txt, b"old conten", "c1");
    // A new version no disk here holds beside the old one.
    let huge = json!({"id": "F", "name": "f.txt", "size": 1u64 << 60, "cTag": "c2", "file": {},
                      "parentReference": {"id": "D"}, "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}});
    s.feed(Some("L1"), json!([huge]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    assert_eq!(
        s.activity().pop().unwrap(),
        ("update-failed".to_owned(), s.full("docs/f.txt"), activity::NO_DISK_SPACE.to_owned())
    );
    let note = s.state.get().replacement_note.expect("the status says it");
    assert!(note.why.contains("not enough space"), "{note:?}");
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "the old version stays");
}

/// A replacement retried after every cycle and failing
/// the same way each time is one `update-failed` event, not one a
/// minute — on a full disk, where it fails at once, that flushed the
/// log and notified every minute.
#[tokio::test]
async fn a_replacement_that_keeps_failing_the_same_way_is_recorded_once() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    write_version(&s.root.path.join("docs/f.txt"), b"old conten", "c1");
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(404))
        .with_priority(1)
        .mount(&s.graph.server).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    s.feed(Some("L2"), json!([]), "L3").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;

    let asked = s.graph.server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/items/F").count();
    assert_eq!(asked, 2, "it was tried again");
    let recorded = s.activity().into_iter().filter(|(kind, _, _)| kind == "update-failed").count();
    assert_eq!(recorded, 1, "the same failure again is not news: {:?}", s.activity());
    assert!(s.state.get().replacement_note.is_some(), "the status still says it");
}

/// I1's other half: a failure is news again when its reason changes — not
/// its words — when it is for a newer version, and when the file was replaced
/// since. The note counts the files and quotes the newest failure.
#[test]
fn a_failure_with_a_new_reason_or_version_is_recorded_again() {
    let state = SyncStateHandle::new(Default::default());
    let replacements = Replacements::new(state.clone());
    let r = |ctag: &str| Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: ctag.into(), size: 10 };
    let failed = |reason, text: &str| ReplaceOutcome::Failed(Failure { reason, text: text.into() });
    let news = |ctag: &str, outcome: ReplaceOutcome| replacements.record(&r(ctag), &outcome).news;
    assert!(news("c2", failed(FailureReason::Download(libc::EIO), "a")));
    assert!(!news("c2", failed(FailureReason::Download(libc::EIO), "a")), "the same again");
    assert!(!news("c2", failed(FailureReason::Download(libc::EIO), "other words")), "the same reason in other words");
    assert!(news("c2", failed(FailureReason::NoSpace, "b")), "another reason");
    assert!(news("c3", failed(FailureReason::NoSpace, "b")), "a newer version");
    assert_eq!(state.get().replacement_note, Some(ReplacementNote { files: 1, why: "b".into() }));
    assert!(news("c3", ReplaceOutcome::Replaced));
    assert_eq!(state.get().replacement_note, None);
    assert!(news("c3", failed(FailureReason::NoSpace, "b")), "failing after it went through");
}

/// A replacement that ends with nothing to do (here: the folder above the
/// file moved while it downloaded) makes the next cycle Full, and that
/// cycle finds the file again and issues its replacement anew.
#[tokio::test]
async fn a_replacement_that_finds_its_file_moved_makes_the_next_cycle_full_and_issues_it_again() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    write_version(&s.root.path.join("docs/f.txt"), b"old conten", "c1");
    let new = b"new content".to_vec();
    let (root, moved, answer) = (s.root.path.clone(), AtomicBool::new(false), s.version("c2", &new));
    s.serve_new_version(&new, move |_: &Request| {
        if !moved.swap(true, Ordering::SeqCst) {
            move_docs_away(&root);
        }
        answer.clone()
    })
    .await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert_eq!(report.applied.pending.replacements.len(), 1);
    listing.join_replacements().await;
    assert_eq!(std::fs::read(s.root.path.join("papers/f.txt")).unwrap(), b"old conten", "nothing was swapped in");

    s.feed(Some("L2"), json!([]), "L3").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "only a Full reconcile finds the replacement again");
    assert_eq!(report.applied.pending.replacements.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["F"]);
    listing.join_replacements().await;
    assert_eq!(std::fs::read(s.root.path.join("docs/f.txt")).unwrap(), new);
}

/// A replacement cut short because the poller stops has no outcome:
/// it asks for no Full reconcile and changes no note.
#[tokio::test]
async fn a_replacement_stopped_with_the_poller_asks_for_nothing() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    write_version(&f_txt, b"old conten", "c1");
    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.version("c2", &new).set_delay(Duration::from_secs(30))).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    let poller = Poller::start(Arc::clone(&listing), Schedule::polled(Duration::from_secs(3600), vec![]));
    let mut asked = false;
    for _ in 0..100 {
        asked = s.graph.server.received_requests().await.unwrap().iter().any(|r| r.url.path() == "/me/drive/items/F");
        if asked {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(asked, "the replacement is under way");
    tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("the stop cuts the download short");
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten");
    assert_eq!(s.state.get().replacement_note, None);
    s.feed(Some("L2"), json!([]), "L3").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(!report.full, "a replacement the stop cut short is no reason for a Full reconcile");
}

/// A stop asked while a replacement's swap is under way waits for it: the
/// poller's stop returns when the swap has ended, and the swap is said — in
/// the activity log and as the item's recorded inode. The folder is locked,
/// and the swap is held at its directory's write window, which the test has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_waits_for_a_swap_under_way_and_the_swap_is_said() {
    use konedrive_fs::handle::FileHandle;
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    write_version(&f_txt, b"old conten", "c1");
    let old = crate::folder::locks::InodeKey::of(&File::open(&f_txt).unwrap()).unwrap();

    // The download's answer waits until this thread has the write windows to itself.
    let new = b"new content".to_vec();
    let (fetching, is_fetching) = std::sync::mpsc::channel::<()>();
    let (go_on, may_go_on) = std::sync::mpsc::channel::<()>();
    let (fetching, may_go_on, body) = (std::sync::Mutex::new(fetching), std::sync::Mutex::new(may_go_on), new.clone());
    Mock::given(method("GET")).and(path("/dl/F/c2"))
        .respond_with(move |_: &Request| {
            let _ = fetching.lock().unwrap().send(());
            let _ = may_go_on.lock().unwrap().recv();
            ResponseTemplate::new(200).set_body_bytes(body.clone())
        })
        .with_priority(1)
        .mount(&s.graph.server).await;
    s.serve_new_version(&new, s.version("c2", &new)).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    let poller = Poller::start(Arc::clone(&listing), Schedule::polled(Duration::from_secs(3600), vec![]));
    is_fetching.recv_timeout(PATIENCE).expect("the replacement downloads");
    let windows = crate::folder::disk::dir_modes();
    go_on.send(()).unwrap();

    // The file's lock taken: nothing stands between that and the swap's section.
    for _ in 0..500 {
        if s.locks.try_lock(old).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(s.locks.try_lock(old).is_none(), "the swap is under way");
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "held before its rename");

    let released = Arc::new(AtomicBool::new(false));
    let was_released = Arc::clone(&released);
    let stop = tokio::spawn(async move {
        poller.stop().await;
        was_released.load(Ordering::SeqCst)
    });
    // Time for the stop to be asked: it cannot return while the swap is held.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!stop.is_finished(), "the stop waits for the swap");
    released.store(true, Ordering::SeqCst);
    drop(windows);

    assert!(stop.await.unwrap(), "the stop returned only once the swap could end");
    assert_eq!(std::fs::read(&f_txt).unwrap(), new);
    assert_eq!(s.activity().pop().unwrap(), ("updated".into(), s.full("docs/f.txt"), "11 B".into()));
    let swapped_in = FileHandle::of(&File::open(&f_txt).unwrap()).unwrap();
    let recorded = s.store.call(|store| store.local_handle("F")).await.unwrap();
    assert_eq!(recorded, Some(swapped_in), "the item's recorded object is the inode swapped in");
}

/// A replacement that ends while a cycle reconciles asks for a Full
/// reconcile after it — the cycle that was running must not swallow the
/// request when it succeeds. Made deterministic by holding both
/// replacement slots until the running cycle is stuck marking a folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_that_ends_while_a_cycle_runs_still_makes_the_next_one_full() {
    let s = World::read_only().await;
    let (reached, release) = s.helper.stall_on("/.konedrive-new-N");
    let (listing, pool) = with_pool(&s);
    s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1")]), "L1").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    write_version(&s.root.path.join("docs/f.txt"), b"old conten", "c1");
    let new = b"new content".to_vec();
    let (root, moved, answer) = (s.root.path.clone(), AtomicBool::new(false), s.version("c2", &new));
    s.serve_new_version(&new, move |_: &Request| {
        if !moved.swap(true, Ordering::SeqCst) {
            move_docs_away(&root);
        }
        answer.clone()
    })
    .await;

    // The replacement is issued, and waits for a slot.
    // A paused pool hands no background slot out: the replacement waits.
    pool.set_paused(true);
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    // The next cycle starts, and is stuck marking the folder it makes.
    s.feed(Some("L2"), json!([folder("N", "R", "new")]), "L3").await;
    let running = tokio::spawn({
        let listing = Arc::clone(&listing);
        async move { listing.cycle(&CancellationToken::new()).await }
    });
    tokio::task::spawn_blocking(move || reached.recv().unwrap()).await.unwrap();
    // Meanwhile the replacement runs, and ends with nothing to do.
    pool.set_paused(false);
    listing.join_replacements().await;
    assert!(!running.is_finished());
    release.send(()).unwrap();
    running.await.unwrap().unwrap();

    s.feed(Some("L3"), json!([]), "L4").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the replacement's request outlived the cycle that was running when it came");
    listing.join_replacements().await;
    assert_eq!(std::fs::read(s.root.path.join("docs/f.txt")).unwrap(), new);
}

/// A newer version that arrives while an older one of the same file is
/// still being fetched is fetched after it, not dropped: nothing else
/// would ever look at that file again.
#[tokio::test]
async fn a_newer_version_that_arrives_while_a_replacement_runs_is_fetched_after_it() {
    let s = World::read_only().await;
    let (listing, pool) = with_pool(&s);
    s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1")]), "L1").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    let f_txt = s.root.path.join("docs/f.txt");
    write_version(&f_txt, b"old conten", "c1");
    let (two, three) = (b"version two".to_vec(), b"version three".to_vec());
    // Graph serves version two once, and version three from then on.
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(s.version("c2", &two))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.graph.server).await;
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(s.version("c3", &three))
        .with_priority(2)
        .mount(&s.graph.server).await;
    s.serve_download("c2", &two).await;
    s.serve_download("c3", &three).await;

    // A paused pool hands no background slot out: the replacement waits.
    pool.set_paused(true);
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    s.feed(Some("L2"), json!([file("F", "D", "f.txt", "c3")]), "L3").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    pool.set_paused(false);
    listing.join_replacements().await;
    assert_eq!(std::fs::read(&f_txt).unwrap(), three);
    assert_eq!(placeholder::read_ctag(&File::open(&f_txt).unwrap()).unwrap().as_deref(), Some("c3"));
}

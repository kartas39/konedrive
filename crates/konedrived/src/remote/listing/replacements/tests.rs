use crate::helper::HelperLink;
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
use crate::remote::listing::*;
#[tokio::test]
async fn a_file_changed_in_the_cloud_is_replaced_after_the_cycle() {
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.new_version(&new)).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    assert_eq!(std::fs::read(&f_txt).unwrap(), new);
}

/// A pinned file replaced by its new version — another inode — is still
/// pinned.
#[tokio::test]
async fn a_replacement_keeps_the_files_own_pin() {
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
    pin_by_hand(&s.root.path, "docs/f.txt");
    let before = ino(&f_txt);
    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.new_version(&new)).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;

    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;

    assert_eq!(std::fs::read(&f_txt).unwrap(), new);
    assert_ne!(ino(&f_txt), before);
    assert_eq!(File::open(&f_txt).unwrap().get_xattr(placeholder::XATTR_PIN).unwrap(), Some(b"1".to_vec()));
}

/// When the new version cannot be had, the old one stays, the
/// status says why, and it is tried again.
#[tokio::test]
async fn a_replacement_that_fails_is_said_and_tried_again() {
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.server).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "the old version stays");
    assert!(s.state.get().replacement_note.contains("could not be updated"), "{:?}", s.state.get().replacement_note);

    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.new_version(&new)).await;
    s.feed(Some("L2"), json!([]), "L3").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(!report.full, "a failed replacement is retried as it is, with no Full reconcile");
    listing.join_replacements().await;
    assert_eq!(std::fs::read(&f_txt).unwrap(), new);
    assert_eq!(s.state.get().replacement_note, "");
}

/// A replacement the disk has no room for is an
/// `update-failed` event whose detail is exactly "not enough disk space"
/// — the words the window's notifier turns into "disk full".
#[tokio::test]
async fn a_replacement_with_no_room_on_the_disk_says_exactly_that() {
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
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
    assert!(s.state.get().replacement_note.contains("not enough space"), "{:?}", s.state.get().replacement_note);
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "the old version stays");
}

/// A replacement that goes through is an `updated` event,
/// one that fails an `update-failed` event saying why — not `failed`,
/// which is a download's.
#[tokio::test]
async fn a_replacement_is_recorded_as_updated_or_failed() {
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.server).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    let (kind, at, why) = s.activity().pop().unwrap();
    assert_eq!((kind.as_str(), at.as_str()), ("update-failed", s.full("docs/f.txt").as_str()));
    assert!(why.contains("could not be downloaded"), "{why}");

    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.new_version(&new)).await;
    s.feed(Some("L2"), json!([]), "L3").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    assert_eq!(s.activity().pop().unwrap(), ("updated".into(), s.full("docs/f.txt"), "11 B".into()));
    assert!(s.report.transfers.list().is_empty(), "no download is left showing");
}

/// A replacement retried after every cycle and failing
/// the same way each time is one `update-failed` event, not one a
/// minute — on a full disk, where it fails at once, that flushed the
/// log and notified every minute.
#[tokio::test]
async fn a_replacement_that_keeps_failing_the_same_way_is_recorded_once() {
    let s = setup().await;
    let listing = listed(&s).await;
    hydrate_by_hand(&s.root.path.join("docs/f.txt"), b"old conten");
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(404))
        .with_priority(1)
        .mount(&s.server).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;
    s.feed(Some("L2"), json!([]), "L3").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.join_replacements().await;

    let asked = s.server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/items/F").count();
    assert_eq!(asked, 2, "it was tried again");
    let recorded = s.activity().into_iter().filter(|(kind, _, _)| kind == "update-failed").count();
    assert_eq!(recorded, 1, "the same failure again is not news: {:?}", s.activity());
    assert!(s.state.get().replacement_note.contains("could not be updated"), "the status still says it");
}

/// I1's other half: a failure is news again when its reason changes,
/// when it is for a newer version, and when the file was replaced since.
#[tokio::test]
async fn a_failure_with_a_new_reason_or_version_is_recorded_again() {
    let s = setup().await;
    let listing = s.listing();
    let r = |ctag: &str| Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: ctag.into(), size: 10 };
    assert!(listing.record_replacement(&r("c2"), ReplaceOutcome::Failed("a".into())));
    assert!(!listing.record_replacement(&r("c2"), ReplaceOutcome::Failed("a".into())), "the same again");
    assert!(listing.record_replacement(&r("c2"), ReplaceOutcome::NoSpace("b".into())), "another reason");
    assert!(listing.record_replacement(&r("c3"), ReplaceOutcome::NoSpace("b".into())), "a newer version");
    assert!(listing.record_replacement(&r("c3"), ReplaceOutcome::Replaced));
    assert!(listing.record_replacement(&r("c3"), ReplaceOutcome::NoSpace("b".into())), "failing after it went through");
}

/// A replacement that ends with nothing to do (here: the folder above the
/// file moved while it downloaded) makes the next cycle Full, and that
/// cycle finds the file again and issues its replacement anew.
#[tokio::test]
async fn a_replacement_that_finds_its_file_moved_makes_the_next_cycle_full_and_issues_it_again() {
    let s = setup().await;
    let listing = listed(&s).await;
    hydrate_by_hand(&s.root.path.join("docs/f.txt"), b"old conten");
    let new = b"new content".to_vec();
    let (root, moved, answer) = (s.root.path.clone(), AtomicBool::new(false), s.new_version(&new));
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
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
    let new = b"new content".to_vec();
    s.serve_new_version(&new, s.new_version(&new).set_delay(Duration::from_secs(30))).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    let poller = Poller::start(Arc::clone(&listing), Schedule::polled(Duration::from_secs(3600), vec![]));
    let mut asked = false;
    for _ in 0..100 {
        asked = s.server.received_requests().await.unwrap().iter().any(|r| r.url.path() == "/me/drive/items/F");
        if asked {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(asked, "the replacement is under way");
    tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("the stop cuts the download short");
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten");
    assert_eq!(s.state.get().replacement_note, "");
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
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
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
        .mount(&s.server).await;
    s.serve_new_version(&new, s.new_version(&new)).await;
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    let poller = Poller::start(Arc::clone(&listing), Schedule::polled(Duration::from_secs(3600), vec![]));
    is_fetching.recv_timeout(PATIENCE).expect("the replacement downloads");
    let windows = crate::folder::disk::dir_modes();
    go_on.send(()).unwrap();

    // The file's lock taken: nothing stands between that and the swap's section.
    for _ in 0..500 {
        if listing.ctx.locks.try_lock(old).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(listing.ctx.locks.try_lock(old).is_none(), "the swap is under way");
    assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "held before its rename");

    let released = Arc::new(AtomicBool::new(false));
    let was_released = Arc::clone(&released);
    let stop = tokio::spawn(async move {
        poller.stop().await;
        was_released.load(Ordering::SeqCst)
    });
    for _ in 0..500 {
        if listing.cancel_replacements.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(listing.cancel_replacements.is_cancelled(), "the stop is asked");
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
    let s = setup().await;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-N");
    let link = HelperLink::connect(&socket_path).await.unwrap().0;
    let listing = Listing::new(ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link))), ..s.context() });
    s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1")]), "L1").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    hydrate_by_hand(&s.root.path.join("docs/f.txt"), b"old conten");
    let new = b"new content".to_vec();
    let (root, moved, answer) = (s.root.path.clone(), AtomicBool::new(false), s.new_version(&new));
    s.serve_new_version(&new, move |_: &Request| {
        if !moved.swap(true, Ordering::SeqCst) {
            move_docs_away(&root);
        }
        answer.clone()
    })
    .await;

    // The replacement is issued, and waits for a slot.
    // A paused pool hands no background slot out: the replacement waits.
    listing.ctx.drive.pool().set_paused(true);
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
    listing.ctx.drive.pool().set_paused(false);
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
    let s = setup().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    hydrate_by_hand(&f_txt, b"old conten");
    let (two, three) = (b"version two".to_vec(), b"version three".to_vec());
    // Graph serves version two once, and version three from then on.
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(s.version("c2", &two))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.server).await;
    Mock::given(method("GET")).and(path("/me/drive/items/F"))
        .respond_with(s.version("c3", &three))
        .with_priority(2)
        .mount(&s.server).await;
    s.serve_download("c2", &two).await;
    s.serve_download("c3", &three).await;

    // A paused pool hands no background slot out: the replacement waits.
    listing.ctx.drive.pool().set_paused(true);
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    s.feed(Some("L2"), json!([file("F", "D", "f.txt", "c3")]), "L3").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing.ctx.drive.pool().set_paused(false);
    listing.join_replacements().await;
    assert_eq!(std::fs::read(&f_txt).unwrap(), three);
    assert_eq!(placeholder::read_ctag(&File::open(&f_txt).unwrap()).unwrap().as_deref(), Some("c3"));
}

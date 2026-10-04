//! A read-write folder's cycle (`docs/design/writes.md` §9) against a fake
//! OneDrive on wiremock — the outbox worker's, now serving the delta feed too
//! — in a temporary folder: the tree lock, the stale-delta guard, what
//! waits, echoes of the outbox's own changes, the `410` variants, a
//! replacement under a write lease, the order of the cycle and the outbox,
//! and a folder removed in OneDrive that holds local work. No real network.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{State, XATTR_ITEM_ID};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

use super::super::{Listing, ListingContext};
use super::Writes;
use crate::folder::classify::classify;
use crate::fake_onedrive::{FakeItem, ROOT};
use crate::folder::disk::Disk;
use crate::hydration::graph_source::GraphSource;
use crate::local::{Batch, Examined, Examiner, IgnoreList};
use crate::remote::testing::{id_at, now, state_at, write_version, World};
use crate::upload::{Engine, OutboxWorker};
use konedrive_tree::outbox::{Committed, OutboxKind, OutboxState};
use konedrive_tree::{ActivityKind, Change};

pub(super) fn names(batch: &Batch) -> String {
    format!("{batch:?}")
}

/// the read-write reconcile must, item 2: the cycle holds the outbox worker's tree lock from its
/// staging to its swap, so that no commit lands in between (and is
/// reverted by the swap).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cycle_holds_the_tree_lock_from_staging_to_the_swap() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    w.graph.with(|c| c.edit("F", b"two"));
    let held = Arc::clone(&w.tree_lock).lock_owned().await;
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!cycle.is_finished(), "the cycle staged while the outbox held the tree");
    assert_ne!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    drop(held);
    cycle.await.unwrap().unwrap();
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    assert_eq!(w.cycles.load(Ordering::SeqCst), 2, "each cycle tells the outbox");
}

/// §3.7's stale-delta guard, and the read-write reconcile must, items 1 and 4: a delta fetched
/// before an upload's commit does not take the item back to the version
/// before it; a change OneDrive made after the commit is taken — read again
/// — and the file is replaced, its base kept at the version it holds until
/// the new one is in place, and its new inode recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delta_fetched_before_a_commit_does_not_undo_it() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));

    // The next delta is answered late, as the drive was before the upload.
    let stale = w.graph.with(|c| c.delta_body());
    w.graph.with(|c| c.script("GET", "root/delta", ResponseTemplate::new(200).set_body_json(stale).set_delay(Duration::from_millis(800)), 1));
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.commit_upload("F", "docs/f.txt", b"mine").await;
    let committed = w.base("F").unwrap();
    let report = cycle.await.unwrap().unwrap();
    listing.join_replacements().await;
    assert_eq!(w.base("F").unwrap().etag, committed.etag, "the stale delta did not undo the commit");
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"mine");
    assert!(report.applied.pending.replacements.is_empty(), "{:?}", report.applied.pending.replacements);
    assert!(w.graph.with(|c| c.count("GET", "items/F")) >= 1, "read again from OneDrive");

    // Now OneDrive changes it again right after an upload, within one fetch.
    let stale = w.graph.with(|c| c.delta_body());
    w.graph.with(|c| c.script("GET", "root/delta", ResponseTemplate::new(200).set_body_json(stale).set_delay(Duration::from_millis(800)), 1));
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.commit_upload("F", "docs/f.txt", b"mine again").await;
    w.graph.with(|c| c.edit("F", b"theirs"));
    let report = cycle.await.unwrap().unwrap();
    assert_eq!(report.applied.pending.replacements.len(), 1, "OneDrive's newer version is fetched");
    listing.join_replacements().await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"theirs");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()), "the base took it as it landed");
    assert!(w.deferred("F").is_none());
    let handle = FileHandle::of(&File::open(w.path("docs/f.txt")).unwrap()).unwrap();
    assert_eq!(w.store.call(move |s| s.local_handle("F")).await.unwrap(), Some(handle), "the new version's inode is the item's");
}

/// WR6, §3.7 echo: the outbox's own create, edit and delete come back in the
/// delta and change nothing here — no download, no replacement, no removal,
/// nothing said.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_outbox_own_changes_coming_back_in_the_delta_change_nothing() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    let new = w.path("docs/new.txt");
    std::fs::write(&new, b"hello").unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
    w.upload().await;
    let id = id_at(&new).expect("uploaded");
    let inode = std::fs::metadata(&new).unwrap().ino();

    let report = w.cycle(&listing).await;
    assert_eq!((report.applied.counts.created, report.applied.counts.updated, report.applied.counts.deleted), (0, 0, 0));
    assert!(report.applied.pending.replacements.is_empty() && report.applied.changes.is_empty() && report.applied.pending.unsettled.is_empty());
    assert_eq!(std::fs::metadata(&new).unwrap().ino(), inode);
    assert_eq!(state_at(&new), Some(State::Hydrated));

    // An edit, uploaded, comes back.
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(&new).unwrap().write_all(b" again").unwrap();
    let mut batch = Batch::new();
    batch.written(Path::new("docs"), OsStr::new("new.txt"), None);
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
    w.upload().await;
    let report = w.cycle(&listing).await;
    assert!(report.applied.pending.replacements.is_empty() && report.applied.changes.is_empty());
    assert_eq!(std::fs::read(&new).unwrap(), b"hello again");
    assert_eq!(std::fs::metadata(&new).unwrap().ino(), inode);

    // A delete, done in OneDrive, comes back.
    std::fs::remove_file(&new).unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1, "gone, on the (fake) helper's word");
    w.upload().await;
    assert!(w.graph.with(|c| c.bin.contains_key(&id)));
    let report = w.cycle(&listing).await;
    assert!(report.applied.changes.is_empty() && !new.exists() && w.base(&id).is_none());
    let said: Vec<ActivityKind> = konedrive_tree::off_runtime(|| w.report.activity.recent(100)).unwrap().into_iter().filter(|e| e.path.ends_with("new.txt")).map(|e| e.kind).collect();
    assert!(!said.iter().any(|kind| matches!(kind, ActivityKind::Added | ActivityKind::Updated | ActivityKind::Removed)), "{said:?}");
}

/// §3.7 `410`: `resyncChangesUploadDifferences` keeps what the new listing
/// left out and was downloaded here — its attributes off, for the outbox to
/// upload again — keeps a download whose version differs from the listing's
/// beside it, as a copy, and removes placeholders, which hold nothing;
/// `resyncChangesApplyDifferences` removes what is gone and replaces what
/// changed, keeping only local work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_two_resyncs_differ_in_what_the_listing_left_out() {
    for (code, upload) in [("resyncChangesUploadDifferences", true), ("resyncChangesApplyDifferences", false)] {
        let w = World::read_write().await;
        let listing = w.listed().await;
        w.graph.with(|c| c.add_file("P", "D", "p.txt", b"p"));
        w.cycle(&listing).await;
        assert_eq!(state_at(&w.path("docs/p.txt")), Some(State::OnlineOnly));
        write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
        write_version(&w.path("top.txt"), b"top", &w.cloud_ctag("T"));
        w.graph.with(|c| {
            c.trash("F");
            c.trash("P");
            c.edit("T", b"top, changed");
            c.script("GET", "root/delta", ResponseTemplate::new(410).set_body_json(json!({"error": {"code": "resyncRequired", "innerError": {"code": code}}})), 1);
        });
        let report = w.cycle(&listing).await;
        assert!(report.full, "{code}");
        assert!(!w.path("docs/p.txt").exists(), "{code}: a placeholder holds nothing here");
        assert_eq!(w.path("docs/f.txt").exists(), upload, "{code}");
        assert_eq!(w.path("top-fedora.txt").exists(), upload, "{code}");
        if upload {
            assert_eq!(id_at(&w.path("docs/f.txt")), None, "uploaded again as new");
            assert_eq!(std::fs::read(w.path("top-fedora.txt")).unwrap(), b"top", "the version OneDrive may have lost is kept");
            assert_eq!(id_at(&w.path("top-fedora.txt")), None);
            assert_eq!(state_at(&w.path("top.txt")), Some(State::OnlineOnly), "OneDrive's version takes the name");
            let examined = w.examined.lock().unwrap().iter().map(names).collect::<String>();
            assert!(examined.contains("f.txt") && examined.contains("top-fedora.txt"), "{examined}");
        } else {
            assert_eq!(std::fs::read(w.path("top.txt")).unwrap(), b"top, changed", "a clean download takes the new version");
        }
    }
}

/// §3.7 replacement under a lease, and the read-write reconcile must, item 4: a file open for
/// writing is not replaced — nothing is downloaded — and its base keeps the
/// version it holds; once closed, the next cycle replaces it and the base
/// follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_waits_for_a_file_open_for_writing() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    let old = w.cloud_ctag("F");
    write_version(&w.path("docs/f.txt"), b"one", &old);
    w.graph.with(|c| c.edit("F", b"two"));
    let writer = std::fs::OpenOptions::new().write(true).open(w.path("docs/f.txt")).unwrap();
    let report = w.cycle(&listing).await;
    assert_eq!(report.applied.pending.replacements.len(), 1);
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"one");
    assert_eq!(w.graph.with(|c| c.count("GET", "dl/F")), 0, "nothing downloaded for nothing");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(old.as_str()), "the base keeps the version on disk");
    assert!(w.deferred("F").is_some());

    drop(writer);
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"two");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    assert!(w.deferred("F").is_none());
}

/// F82 (7): an outbox commit that adopted OneDrive's answer with a newer
/// cTag than the file holds (a move whose earlier PATCH landed, or whose
/// place OneDrive won) — the delta then brings nothing new — is looked at
/// again by the next cycle, and the unchanged file is replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_older_than_what_the_outbox_committed_is_replaced() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.cycle(&listing).await;
    w.graph.with(|c| c.edit("F", b"two"));
    let item = w.graph.client().item("F").await.unwrap();
    let Change::Upsert(answer) = classify(&item) else { panic!("an upsert") };
    {
        let _tree = w.tree_lock.lock().await;
        let seq = w.row(OutboxKind::Update, Some("F"), "docs/f.txt");
        w.store.call(move |s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: None }, None)).await.unwrap();
    }
    let report = w.cycle(&listing).await;
    assert_eq!(report.applied.pending.replacements.len(), 1, "{report:?}");
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"two");
}

/// the read-write reconcile must, item 1 (the examination's hunk in `replace_through`, read-only as before): a
/// replacement records the new version's inode as the item's, so that a
/// move of it out of the folder is never taken for a delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_records_its_new_inode_in_a_read_only_folder_too() {
    let w = World::read_write().await;
    let read_only = || Listing::new(ListingContext { locked: true, writes: None, ..w.context() });
    let listing = read_only();
    w.cycle(&listing).await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.graph.with(|c| c.edit("F", b"two"));
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"two");
    let handle = FileHandle::of(&File::open(w.path("docs/f.txt")).unwrap()).unwrap();
    assert_eq!(w.store.call(move |s| s.local_handle("F")).await.unwrap(), Some(handle));
    Disk::open(&w.root, false).unwrap().unlock_tree().unwrap();
}

/// §3.3, §4.9: the folder's first cycle waits for the watcher's Full local
/// scan, and the outbox for a cycle: nothing is sent before one went through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_cycle_waits_for_the_scan_and_the_outbox_for_the_cycle() {
    let w = World::read_write().await;
    let (scanned, first_scan) = tokio::sync::watch::channel(false);
    let listing = Listing::new(ListingContext { writes: Some(w.writes(Some(first_scan))), ..w.context() });
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.graph.with(|c| c.count("GET", "root/delta")), 0, "OneDrive asked before the local scan");
    scanned.send(true).unwrap();
    cycle.await.unwrap().unwrap();
    assert_eq!(w.cycles.load(Ordering::SeqCst), 1);

    let worker = OutboxWorker::new(w.config());
    worker.wait_for_cycle(false);
    std::fs::write(w.path("docs/new.txt"), b"hello").unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    w.examine(batch).await;
    worker.start();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(w.graph.with(|c| c.at("docs/new.txt").is_none()), "sent before a cycle");
    worker.cycle_done();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while w.graph.with(|c| c.at("docs/new.txt").is_none()) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(w.graph.with(|c| c.at("docs/new.txt").is_some()), "sent once the cycle went through");
    worker.stop().await;
}

/// §3.7: an item with a live row keeps its base at the swap, and OneDrive's
/// change waits; once the row is gone, the next cycle applies it — the delta
/// cursor never sends it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_that_waited_for_a_row_is_applied_once_the_row_is_gone() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    let seq = w.row(OutboxKind::Update, Some("F"), "docs/f.txt");
    w.graph.with(|c| c.rename("F", "D", "renamed.txt"));
    w.cycle(&listing).await;
    assert!(w.path("docs/f.txt").exists() && !w.path("docs/renamed.txt").exists());
    assert_eq!(w.base("F").unwrap().name, "f.txt");
    assert!(w.deferred("F").is_some());

    w.store.call(move |s| s.outbox_drop(seq, None, None)).await.unwrap();
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/renamed.txt")).as_deref(), Some("F"));
    assert!(!w.path("docs/f.txt").exists());
    assert_eq!(w.base("F").unwrap().name, "renamed.txt");
    assert!(w.deferred("F").is_none());
}

/// An item OneDrive renames to a name the folder cannot hold, while the
/// folder cannot let it go yet (moved here, not examined): it stays an item
/// of the folder, its change waits, and it is on the skipped list, and
/// counted, from that cycle on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_that_cannot_stay_and_cannot_go_yet_is_on_the_skipped_list_at_once() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    std::fs::rename(w.path("docs/f.txt"), w.path("docs/moved.txt")).unwrap();
    let long = format!("{}.txt", "x".repeat(260));
    w.graph.with(|c| c.rename("F", "D", &long));
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/moved.txt")).as_deref(), Some("F"), "left where it is");
    assert_eq!(w.base("F").unwrap().placement, konedrive_tree::Placement::Placed, "the base still places it");
    assert!(w.deferred("F").is_some(), "its change waits");
    let skipped = w.store.call(|s| s.skipped()).await.unwrap();
    let waits = Some(konedrive_tree::WaitsFor::Changes("docs/f.txt".into()));
    assert_eq!(skipped, vec![konedrive_tree::Skipped { rel: Path::new("docs").join(&long), reason: konedrive_tree::SkipReason::NameTooLong, waits, here: Some("docs/f.txt".into()) }]);
    assert_eq!(w.state.get().cycle.skipped_count, 1);
}

/// A folder removed in OneDrive while a new file waits in it to be uploaded,
/// and a download in it was changed here (the owner's ruling of 2026-10-04):
/// those two stay and reach OneDrive as new files in a new folder; the rest
/// of the folder goes, the Activity says what was kept, and nothing is
/// deleted in OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_removed_in_onedrive_keeps_what_was_made_or_changed_here_and_it_goes_up_as_new() {
    let w = World::read_write().await;
    w.graph.with(|c| c.add_file("G", "D", "g.txt", b"theirs"));
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(w.path("docs/f.txt")).unwrap().write_all(b" and mine").unwrap();
    std::fs::write(w.path("docs/mine.txt"), b"mine").unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("mine.txt"));
    batch.written(Path::new("docs"), OsStr::new("f.txt"), None);
    assert_eq!(w.examine(batch).await.applied.queued.len(), 2);
    assert!(w.path("docs/g.txt").exists());
    w.graph.with(|c| c.trash("D"));
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"one and mine", "the change made here stays");
    assert_eq!(std::fs::read(w.path("docs/mine.txt")).unwrap(), b"mine", "and so does the new file");
    assert!(!w.path("docs/g.txt").exists(), "what OneDrive had went");
    assert_eq!((id_at(&w.path("docs")), id_at(&w.path("docs/f.txt"))), (None, None), "the user's own now");
    let said = konedrive_tree::off_runtime(|| w.report.activity.recent(100)).unwrap();
    assert!(
        said.iter().any(|e| e.kind == ActivityKind::Removed && e.path == w.path("docs").display().to_string() && e.detail.starts_with("2 files")),
        "the Activity says what was kept: {said:?}"
    );

    // What the cycle handed to the watcher is all that is examined.
    w.examine_handed_and_upload().await;
    assert_eq!(w.graph.with(|c| c.paths()), ["docs", "docs/f.txt", "docs/mine.txt", "top.txt"], "made again in OneDrive, as new");
    assert!(w.graph.with(|c| c.item("D").is_none() && c.item("F").is_none()), "new items, not the removed ones");
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "f.txt" && i.content == b"one and mine")));
    assert_eq!(w.deletes(), 0);
}

/// One file changed here and removed in OneDrive: whichever comes first, the
/// cycle or the upload, it is kept and goes up as a new file. Here the cycle
/// is first; the upload first is `upload::tests` (`gone_or_new`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_changed_here_and_removed_in_onedrive_is_kept_and_uploaded_as_new() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(w.path("docs/f.txt")).unwrap().write_all(b" and mine").unwrap();
    let mut batch = Batch::new();
    batch.written(Path::new("docs"), OsStr::new("f.txt"), None);
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
    w.graph.with(|c| c.trash("F"));
    w.cycle(&listing).await;
    let said = konedrive_tree::off_runtime(|| w.report.activity.recent(100)).unwrap();
    assert!(said.iter().any(|e| e.kind == ActivityKind::Removed && e.path == w.path("docs/f.txt").display().to_string() && e.detail.starts_with("1 file changed or new")), "{said:?}");
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"one and mine");
    assert!(w.base("F").is_none(), "the base took the removal");

    w.examine_handed_and_upload().await;
    let new = w.graph.with(|c| c.items.values().find(|i| i.name == "f.txt").cloned()).expect("uploaded");
    assert!(new.id != "F" && new.parent.as_deref() == Some("D") && new.content == b"one and mine", "{new:?}");
    assert_eq!(w.deletes(), 0);
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some(new.id.as_str()), "and it is the new item here");
}

/// What stays under a name nothing uploads — here an ignored one — is said to
/// stay on this computer, never to be uploaded: the Activity is true, and
/// OneDrive gets only the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_stays_under_an_ignored_name_is_said_to_stay_on_this_computer_only() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    std::fs::write(w.path("docs/notes.tmp"), b"mine").unwrap();
    w.graph.with(|c| c.trash("D"));
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/notes.tmp")).unwrap(), b"mine");
    let said = konedrive_tree::off_runtime(|| w.report.activity.recent(100)).unwrap();
    let entry = said.iter().find(|e| e.kind == ActivityKind::Removed && e.path == w.path("docs").display().to_string()).unwrap_or_else(|| panic!("{said:?}"));
    assert_eq!(entry.detail, "1 item with an ignored or refused name was kept on this computer only");

    w.examine_handed_and_upload().await;
    assert!(w.graph.with(|c| c.items.values().all(|i| i.name != "notes.tmp")), "not uploaded: {:?}", w.graph.with(|c| c.paths()));
}

/// A cycle that fails after it took konedrive's attributes off what it keeps
/// (here a changed file, in a removed folder holding an object that will
/// not go) still hands the watcher what to examine and says what it kept:
/// nothing else would, and the file would stay unseen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cycle_that_fails_still_hands_over_what_it_kept() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::read_write().await;
    w.graph.with(|c| {
        c.add(FakeItem { id: "E".into(), parent: Some("D".into()), name: "deep".into(), folder: true, content: Vec::new(), hash: None, size: 0, etag: "e-E".into(), ctag: "c-E".into(), mtime: 0 });
        c.add_file("G", "E", "g.txt", b"one");
    });
    let listing = w.listed().await;
    write_version(&w.path("docs/deep/g.txt"), b"one", &w.cloud_ctag("G"));
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(w.path("docs/deep/g.txt")).unwrap().write_all(b" and mine").unwrap();
    w.graph.with(|c| c.trash("D"));
    // `docs/f.txt` will not go; `docs/deep/g.txt`, deeper, is looked at first.
    std::fs::set_permissions(w.path("docs"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = listing.cycle(&CancellationToken::new()).await;
    std::fs::set_permissions(w.path("docs"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(id_at(&w.path("docs/deep/g.txt")), None, "its attributes are off already");
    let said = konedrive_tree::off_runtime(|| w.report.activity.recent(100)).unwrap();
    assert!(said.iter().any(|e| e.kind == ActivityKind::Removed && e.detail.starts_with("1 file changed or new")), "the Activity says what was kept: {said:?}");

    let handed = std::mem::take(&mut *w.examined.lock().unwrap());
    assert!(!handed.is_empty(), "the watcher was told");
    for batch in handed {
        w.examine(batch).await;
    }
    let rows = w.store.call(|s| s.outbox_rows()).await.unwrap();
    assert!(rows.iter().any(|r| r.rel == Path::new("docs/deep/g.txt") && r.item_id.is_none()), "recorded as new: {rows:?}");
}

/// The same when what the failing cycle kept never had an id — a folder
/// OneDrive removed that holds only an ignored file with data: the folder is
/// the user's own from then on, no later cycle takes it off again, and so
/// the failing cycle is the one that says it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cycle_that_fails_says_what_it_kept_that_never_had_an_id() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::read_write().await;
    let folder = |id: &str, parent: &str, name: &str| FakeItem { id: id.into(), parent: Some(parent.into()), name: name.into(), folder: true, content: Vec::new(), hash: None, size: 0, etag: format!("e-{id}"), ctag: format!("c-{id}"), mtime: 0 };
    w.graph.with(|c| {
        c.add(folder("E", "D", "deep"));
        c.add(folder("H", "E", "only"));
    });
    let listing = w.listed().await;
    std::fs::write(w.path("docs/deep/only/draft.swp"), b"unsaved").unwrap();
    w.graph.with(|c| c.trash("D"));
    // `docs/f.txt` will not go; `docs/deep/only`, deeper, is taken off first.
    std::fs::set_permissions(w.path("docs"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = listing.cycle(&CancellationToken::new()).await;
    std::fs::set_permissions(w.path("docs"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(std::fs::read(w.path("docs/deep/only/draft.swp")).unwrap(), b"unsaved");
    assert_eq!(id_at(&w.path("docs/deep/only")), None, "the user's own folder now");
    let said = konedrive_tree::off_runtime(|| w.report.activity.recent(100)).unwrap();
    assert!(
        said.iter().any(|e| e.kind == ActivityKind::Removed && e.detail == "1 item with an ignored or refused name was kept on this computer only"),
        "the Activity says what was kept: {said:?}"
    );
}

/// Where the object `handle` names is, found by walking `bases` as the
/// helper's `OpenByHandle` would find it: an object renamed anywhere under
/// them, inside the folder or out of it, is still found.
fn find_by_handle(bases: &[PathBuf], handle: &FileHandle) -> Option<PathBuf> {
    let mut stack: Vec<PathBuf> = bases.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let opened = File::open(&dir).ok()?;
        for entry in entries.flatten() {
            if FileHandle::at(&opened, &entry.file_name()).ok().as_ref() == Some(handle) {
                return Some(entry.path());
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    None
}

/// "Is this object alive, and where?", answered as the helper answers it,
/// from where the objects really are.
pub(super) struct Scanning(pub(super) Vec<PathBuf>);

impl crate::local::liveness::Liveness for Scanning {
    fn whereabouts(&self, handle: &FileHandle) -> std::io::Result<crate::local::liveness::Whereabouts> {
        Ok(match find_by_handle(&self.0, handle) {
            Some(path) => crate::local::liveness::Whereabouts::At(path),
            None => crate::local::liveness::Whereabouts::Gone,
        })
    }
}

/// The helper for the worker's `move-out` rows, opening objects where
/// they really are.
pub(super) struct ScanningHelper(Vec<PathBuf>);

#[async_trait::async_trait]
impl crate::helper::linked::Helper for ScanningHelper {
    async fn open_by_handle(&self, _dir: &File, handle: &FileHandle) -> Result<std::os::fd::OwnedFd, crate::helper::HelperError> {
        let stale = || crate::helper::HelperError::Refused(libc::ESTALE);
        let path = find_by_handle(&self.0, handle).ok_or_else(stale)?;
        let file = if path.is_dir() {
            File::open(&path)
        } else {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(&path)
        };
        Ok(file.map_err(|_| stale())?.into())
    }

    async fn mark_file(&self, _file: &File) -> Result<(), crate::helper::HelperError> {
        Ok(())
    }

    async fn mark_dir(&self, _dir: &File) -> Result<(), crate::helper::HelperError> {
        Ok(())
    }

    async fn unmark_dir(&self, _dir: &File) -> Result<(), crate::helper::HelperError> {
        Ok(())
    }

    fn clearance(&self) -> Option<crate::helper::Clearance> {
        Some(crate::helper::Clearance::NoLink(self.0[0].join("no-helper.sock")))
    }
}

impl World {
    /// Everywhere an object of the folder can be: the folder, beside it, and
    /// the rescue directory.
    pub(super) fn everywhere(&self) -> Vec<PathBuf> {
        vec![self.root.path.parent().unwrap().to_path_buf(), self.rescue_dir.canonicalize().unwrap()]
    }

    /// the move-out step in the loop: a Full local scan whose "where is it now?" is
    /// answered from where objects really are, then the worker, with
    /// `move-out` rows downloaded and deleted in OneDrive as the move-out step does.
    pub(super) async fn scan_and_upload(&self) -> Examined {
        let (root, store, locks, bases) = (self.root.clone(), self.store.clone(), self.locks.clone(), self.everywhere());
        let examined = tokio::task::spawn_blocking(move || {
            let disk = Disk::open(&root, false).unwrap();
            let liveness = Scanning(bases);
            Examiner { disk: &disk, store: &store, liveness: &liveness, ignore: &IgnoreList::default(), locks: &locks, now: now() }
                .full_scan()
                .unwrap()
        })
        .await
        .unwrap();
        let root = self.root.path.clone();
        let mut config = self.config();
        config.moved_out = Some(crate::upload::move_out::MoveOuts {
            helper: Arc::new(ScanningHelper(self.everywhere())),
            filler: Arc::new(crate::upload::move_out::SourceFill(Arc::new(GraphSource::new(self.graph.client())))),
            route: None,
            home_trash: None,
            roots: Arc::new(move || vec![root.clone()]),
        });
        Arc::new(Engine::new(config)).drain(&CancellationToken::new()).await;
        examined
    }

    pub(super) fn deletes(&self) -> usize {
        self.graph.with(|c| c.count("DELETE", "items/"))
    }
}

/// C1: one delta renames `top.txt` in OneDrive and adds a file to `docs`,
/// which was renamed here and not examined yet. The Changed pass moves
/// `top.txt` to the holding directory, then hands over to the Full scan (the
/// new file's folder is not where the tree has it), which must take it from
/// there to its new name — never out of the folder, where the move-out step would take it
/// for a move out and delete it in OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_pass_handing_over_with_something_in_holding_keeps_it_in_the_folder() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    std::fs::rename(w.path("docs"), w.path("papers")).unwrap();
    w.graph.with(|c| {
        c.rename("T", ROOT, "top2.txt");
        c.add_file("N", "D", "new.txt", b"new");
    });
    let report = w.cycle(&listing).await;
    assert!(report.full, "handed over to the Full scan");
    assert!(report.applied.on_disk.rescued.is_empty(), "moved out of the folder: {:?}", report.applied.on_disk.rescued);
    assert_eq!(id_at(&w.path("top2.txt")).as_deref(), Some("T"), "placed from the holding directory");
    assert!(!w.path(".konedrive-holding").exists());
    assert_eq!(id_at(&w.path("papers")).as_deref(), Some("D"), "the local rename is left to the examination");

    let examined = w.scan_and_upload().await;
    let rows = w.store.call(move |s| s.outbox_rows()).await.unwrap();
    assert!(!rows.iter().any(|r| r.kind.removes()), "{rows:?}: {:?}", examined.applied);
    assert_eq!(w.deletes(), 0, "nothing deleted in OneDrive");
    assert!(w.graph.with(|c| c.bin.is_empty()));
}

/// A placeholder moved out of the folder, changed in
/// OneDrive before the examination saw the move, is not placed again by the
/// reconcile — placed again, the examination would find the item at its place
/// and never make the `move-out` row, and the object outside would read zeros
/// for good. With the move-out step in the loop the object is downloaded where it went, its
/// delete meets OneDrive's change and is dropped, and only then is the item
/// placed again in the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_moved_out_and_changed_in_onedrive_is_downloaded_where_it_went() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    let outside = w.root.path.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::rename(w.path("top.txt"), outside.join("top.txt")).unwrap();
    w.graph.with(|c| c.edit("T", b"top, changed"));
    let report = w.cycle(&listing).await;
    assert!(!w.path("top.txt").exists(), "not placed while its object is alive outside");
    assert!(report.applied.pending.unsettled.contains("T"));
    assert!(w.examined.lock().unwrap().iter().map(names).collect::<String>().contains("top.txt"), "handed to the examination");

    w.scan_and_upload().await;
    assert_eq!(std::fs::read(outside.join("top.txt")).unwrap(), b"top, changed", "downloaded where it went, never zeros");
    assert!(xattr::get(outside.join("top.txt"), XATTR_ITEM_ID).unwrap().is_none(), "the user's own file now");
    assert_eq!(w.deletes(), 1, "one DELETE, answered 412: OneDrive's change wins");
    assert!(w.graph.with(|c| c.item("T").is_some() && c.bin.is_empty()), "nothing deleted in OneDrive");
    assert!(w.store.call(move |s| s.outbox_rows()).await.unwrap().is_empty());

    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("top.txt")).as_deref(), Some("T"), "placed again once the examination decided");
    assert_eq!(state_at(&w.path("top.txt")), Some(State::OnlineOnly));
}

/// C1: what a stop or a crash left in the holding directory goes back into
/// the folder at the next Full reconcile: where the tree has it, or — held by
/// a local change — where the base has it. Nothing is rescued out of the
/// folder, and nothing is deleted in OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_a_stop_left_in_the_holding_directory_goes_back_into_the_folder() {
    let w = World::read_write().await;
    w.listed().await;
    std::fs::create_dir(w.path(".konedrive-holding")).unwrap();
    std::fs::rename(w.path("docs/f.txt"), w.path(".konedrive-holding/F")).unwrap();
    std::fs::rename(w.path("top.txt"), w.path(".konedrive-holding/T")).unwrap();
    w.row(OutboxKind::Update, Some("T"), "top.txt");
    // A restart: the first cycle is Full.
    let report = w.cycle(&w.listing()).await;
    assert!(report.full);
    assert!(report.applied.on_disk.rescued.is_empty(), "moved out of the folder: {:?}", report.applied.on_disk.rescued);
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"), "placed from the holding directory");
    assert_eq!(id_at(&w.path("top.txt")).as_deref(), Some("T"), "put back where the base has it");
    assert!(!w.path(".konedrive-holding").exists());

    w.store.call(move |s| s.outbox_drop_all()).await.unwrap();
    let examined = w.scan_and_upload().await;
    assert!(examined.applied.queued.is_empty(), "{:?}", w.store.call(move |s| s.outbox_rows()).await.unwrap());
    assert_eq!(w.deletes(), 0, "nothing deleted in OneDrive");
}

/// I1: OneDrive changes a file right after an upload's commit, within one
/// fetch; the guard reads it again, but the file is open, so the replacement
/// waits. The change waits too — the cursor never sends it again — and lands
/// once the file is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_read_again_survives_a_replacement_that_waits() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.cycle(&listing).await;
    let stale = w.graph.with(|c| c.delta_body());
    w.graph.with(|c| c.script("GET", "root/delta", ResponseTemplate::new(200).set_body_json(stale).set_delay(Duration::from_millis(800)), 1));
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.commit_upload("F", "docs/f.txt", b"mine").await;
    w.graph.with(|c| c.edit("F", b"theirs"));
    let writer = std::fs::OpenOptions::new().write(true).open(w.path("docs/f.txt")).unwrap();
    cycle.await.unwrap().unwrap();
    listing.join_replacements().await;
    assert!(w.deferred("F").is_some(), "OneDrive's change waits");
    w.cycle(&listing).await;
    assert!(w.deferred("F").is_some(), "and keeps waiting while the file is open");
    drop(writer);
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"theirs");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    assert!(w.deferred("F").is_none());
}

/// I3: `docs` is deleted here (a live `delete` row), and OneDrive moves
/// `top.txt` into it. The move is not the folder's: the file stays where it
/// is and its move waits, and no row takes OneDrive's item back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_moved_in_onedrive_into_a_folder_deleted_here_is_not_moved_back() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    std::fs::remove_dir_all(w.path("docs")).unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new(""), OsStr::new("docs"));
    w.examine(batch).await;
    assert!(w.store.call(move |s| s.outbox_rows()).await.unwrap().iter().any(|r| r.kind == OutboxKind::Delete && r.item_id.as_deref() == Some("D")));
    w.graph.with(|c| c.rename("T", "D", "top.txt"));
    w.cycle(&listing).await;
    assert!(w.path("top.txt").exists());
    assert_eq!(w.base("T").unwrap().parent_id.as_deref(), Some(ROOT), "the base keeps it where the disk has it");
    assert!(w.deferred("T").is_some(), "OneDrive's move waits");
    let mut batch = Batch::new();
    batch.name(Path::new(""), OsStr::new("top.txt"));
    w.examine(batch).await;
    let rows = w.store.call(move |s| s.outbox_rows()).await.unwrap();
    assert!(!rows.iter().any(|r| r.item_id.as_deref() == Some("T")), "a row that moves OneDrive's item back: {rows:?}");
}

/// A held delete outliving the item's own removal in OneDrive: `docs` is
/// deleted here, the mass-delete guard holds its row, and before it is
/// confirmed or restored, `docs` is deleted in OneDrive too (another
/// device). The next cycle's delta reports it gone: the held row has
/// nothing left to delete, so it is dropped without a request, `HeldCount`
/// goes back to 0, and the worker is woken to say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_delete_of_an_item_already_deleted_in_onedrive_is_dropped() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    std::fs::remove_dir_all(w.path("docs")).unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new(""), OsStr::new("docs"));
    w.examine(batch).await;
    let seq = w.store.call(move |s| s.outbox_rows()).await.unwrap().into_iter().find(|r| r.item_id.as_deref() == Some("D")).unwrap().seq;
    // The mass-delete guard's decision, without tripping its threshold.
    w.store.call(move |s| s.outbox_set_state(seq, OutboxState::Held, Some(&"mass-delete".into()), None)).await.unwrap();
    assert_eq!(w.store.call(move |s| crate::upload::outbox_counts(s, false)).await.unwrap().held, 1);

    // `docs` is deleted in OneDrive too, from another device.
    w.graph.with(|c| c.trash("D"));
    w.cycle(&listing).await;

    let rows = w.store.call(move |s| s.outbox_rows()).await.unwrap();
    assert!(rows.is_empty(), "the held delete has nothing left to delete: {rows:?}");
    assert_eq!(w.store.call(move |s| crate::upload::outbox_counts(s, false)).await.unwrap().held, 0);
    assert_eq!(w.deletes(), 0, "never sent to OneDrive");
    let dropped = w.dropped.lock().unwrap().clone();
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].item_id.as_deref(), Some("D"));
}

/// §5, §6 echo, with the delta ahead of the commit: an upload landed, and the
/// worker stopped before its commit (its row stays `running`). The cycle
/// meanwhile brings the new version: it is not downloaded over the file, nor
/// is the new file taken for a create/create conflict. The replay adopts it,
/// and the next cycle changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delta_that_brings_an_upload_before_its_commit_changes_nothing() {
    let w = World::read_write().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(w.path("docs/f.txt")).unwrap().write_all(b" and mine").unwrap();
    std::fs::write(w.path("docs/new.txt"), b"new").unwrap();
    let mut batch = Batch::new();
    batch.written(Path::new("docs"), OsStr::new("f.txt"), None);
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    assert_eq!(w.examine(batch).await.applied.queued.len(), 2);
    let (f_ino, new_ino) = (std::fs::metadata(w.path("docs/f.txt")).unwrap().ino(), std::fs::metadata(w.path("docs/new.txt")).unwrap().ino());

    // Both rows are sent (at once: they are content rows), and the worker
    // stops before committing either.
    let engine = Arc::new(Engine::new(w.config()));
    engine.arm(crate::upload::Fault::AfterSend);
    engine.arm(crate::upload::Fault::AfterSend);
    engine.drain(&CancellationToken::new()).await;
    assert!(w.graph.with(|c| c.at("docs/new.txt").is_some()) && w.graph.with(|c| c.item("F").unwrap().content == b"one and mine"));
    let rows = w.store.call(move |s| s.outbox_rows()).await.unwrap();
    assert!(rows.iter().all(|r| r.state == OutboxState::Running), "{rows:?}");

    let report = w.cycle(&listing).await;
    assert!(report.applied.pending.replacements.is_empty() && report.applied.on_disk.copies.is_empty(), "{report:?}");
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"one and mine");
    assert_eq!(id_at(&w.path("docs/new.txt")), None, "still the outbox's to commit");

    w.upload().await;
    assert!(w.store.call(move |s| s.outbox_rows()).await.unwrap().is_empty());
    let report = w.cycle(&listing).await;
    assert!(report.applied.pending.replacements.is_empty() && report.applied.on_disk.copies.is_empty() && report.applied.changes.is_empty(), "{report:?}");
    assert_eq!(std::fs::metadata(w.path("docs/f.txt")).unwrap().ino(), f_ino);
    assert_eq!(std::fs::metadata(w.path("docs/new.txt")).unwrap().ino(), new_ino);
    assert!(id_at(&w.path("docs/new.txt")).is_some());
    assert!(w.store.call(move |s| s.deferred_ids()).await.unwrap().is_empty());
    assert_eq!(w.graph.with(|c| c.paths()).len(), 4, "no copy in OneDrive: {:?}", w.graph.with(|c| c.paths()));
}

mod stale;

/// RE6: trouble that stops the folder closes the write gate, and the cycle that clears it
/// says so to the outbox worker — after the trouble is gone, not only with `cycled`, which
/// comes while the gate is still closed. A cycle with no such trouble before it says nothing.
/// A cycle that fails with trouble that is only said clears it too, and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cycle_that_clears_blocking_trouble_wakes_the_outbox() {
    use crate::status::snapshot::SyncTrouble;
    let w = World::read_write().await;
    let reopened = Arc::new(Mutex::new(Vec::new()));
    let writes = Writes {
        reopened: Arc::new({
            let (reopened, state) = (Arc::clone(&reopened), w.state.clone());
            // What the worker would find at its wake.
            move || reopened.lock().unwrap().push(state.get().cycle.sync_trouble)
        }),
        ..w.writes(None)
    };
    let listing = Listing::new(ListingContext { writes: Some(writes), ..w.context() });
    w.cycle(&listing).await;
    assert!(reopened.lock().unwrap().is_empty(), "nothing was stopped");

    w.state.update(|s| s.cycle.sync_trouble = Some(SyncTrouble { text: "said and tried again".into(), blocking: false }));
    w.cycle(&listing).await;
    assert!(reopened.lock().unwrap().is_empty(), "trouble that closes no gate");

    w.state.update(|s| s.cycle.sync_trouble = Some(SyncTrouble { text: "the tree store: disk I/O error".into(), blocking: true }));
    w.cycle(&listing).await;
    assert_eq!(*reopened.lock().unwrap(), vec![None], "woken once, with the trouble already cleared");

    w.state.update(|s| s.cycle.sync_trouble = Some(SyncTrouble { text: "the tree store: disk I/O error".into(), blocking: true }));
    w.graph.with(|c| c.script("GET", "root/delta", ResponseTemplate::new(503), 10));
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(!err.blocking(), "{err:?}");
    let woken = reopened.lock().unwrap().clone();
    assert_eq!(woken.len(), 2, "woken by the cycle that failed, too");
    assert_eq!(woken[1], Some(SyncTrouble { text: err.to_string(), blocking: false }), "the gate reads open by then");
}

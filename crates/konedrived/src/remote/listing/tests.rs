use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use konedrive_fs::placeholder::{self, State, XATTR_ROOT};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::reconcile::{record, Said};
use super::*;
use crate::remote::materialize::{OnDisk, Rescued};
use crate::remote::testing::feed::{file, folder, root_item, vault};
use crate::remote::testing::{move_docs_away, write_version, Options, World};
use crate::status::snapshot::SyncSnapshot;
use std::os::unix::fs::MetadataExt;
use xattr::FileExt;

/// A folder listed once: `docs/f.txt` and the Personal Vault, which is skipped.
pub(super) async fn listed(s: &World) -> Arc<Listing> {
    listed_with(s, s.context()).await
}

/// [`listed`], through a listing made from `context`.
pub(super) async fn listed_with(s: &World, context: ListingContext) -> Arc<Listing> {
    s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()]), "L1").await;
    let listing = Listing::new(context);
    listing.cycle(&CancellationToken::new()).await.unwrap();
    listing
}

#[tokio::test]
async fn an_initial_listing_fills_the_folder_and_stores_the_link() {
    let s = World::read_only().await;
    let mut states = s.state.subscribe();
    let seen_listing = tokio::spawn(async move {
        loop {
            if states.borrow_and_update().cycle.listing {
                return true;
            }
            if states.changed().await.is_err() {
                return false;
            }
        }
    });
    // Slow enough that `listing = true` is still published when the
    // watcher looks.
    Mock::given(method("GET")).and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(200)
            .set_body_json(json!({"value": [root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()],
                                  "@odata.deltaLink": s.link_to("L1")}))
            .set_delay(Duration::from_millis(300)))
        .up_to_n_times(1)
        .mount(&s.graph.server).await;
    s.listing().cycle(&CancellationToken::new()).await.unwrap();
    assert!(s.root.path.join("docs/f.txt").is_file());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link_to("L1")));
    let snapshot = s.state.get();
    assert!(!snapshot.cycle.listing);
    assert_eq!((snapshot.cycle.items_listed, snapshot.cycle.items_placed, snapshot.cycle.skipped_count), (3, 2, 1));
    assert_eq!(snapshot.cycle.sync_trouble, None);
    assert!(tokio::time::timeout(Duration::from_secs(1), seen_listing).await.unwrap().unwrap(), "`listing` was published while it ran");
}

#[tokio::test]
async fn a_later_cycle_applies_only_the_changes() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(!report.full);
    assert!(s.root.path.join("docs/renamed.txt").is_file());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link_to("L2")));
}

#[tokio::test]
async fn an_empty_delta_touches_nothing_but_the_link() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let ctime = |p: PathBuf| {
        let m = std::fs::metadata(p).unwrap();
        (m.ctime(), m.ctime_nsec())
    };
    let before = ctime(s.root.path.join("docs"));
    s.feed(Some("L1"), json!([]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert_eq!((report.full, report.changes), (false, 0));
    assert_eq!(ctime(s.root.path.join("docs")), before);
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link_to("L2")));
}

/// A feed that has expired is listed again, and what the new
/// listing no longer has is deleted here.
#[tokio::test]
async fn an_expired_feed_lists_again_and_deletes_what_is_gone() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(410))
        .with_priority(1)
        .mount(&s.graph.server).await;
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L9").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full);
    assert!(!s.root.path.join("docs/f.txt").exists());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link_to("L9")));
}

#[tokio::test]
async fn a_very_large_delta_is_reconciled_in_full() {
    let s = World::read_only().await;
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
    let listing = Listing::new(ListingContext { full_threshold: 2, ..s.context() });
    listing.cycle(&CancellationToken::new()).await.unwrap();
    s.feed(Some("L1"), json!([file("A", "D", "a", "c"), file("B", "D", "b", "c"), file("C", "D", "c", "c")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full);
    assert!(s.root.path.join("docs/c").is_file());
}

/// Pins the folder at `rel` in the (locked) folder, as `Pin` does.
pub(super) fn pin_by_hand(root: &Path, rel: &str) {
    placeholder::write_pin(&File::open(root.join(rel)).unwrap()).unwrap();
}

/// A file the cloud adds to a pinned folder is queued for download once
/// it is placed; nothing else is.
#[tokio::test]
async fn a_new_file_placed_in_a_pinned_folder_is_queued() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    pin_by_hand(&s.root.path, "docs");
    s.feed(Some("L1"), json!([file("G", "D", "g.txt", "c1"), file("H", "R", "h.txt", "c1")]), "L2").await;

    let report = listing.cycle(&CancellationToken::new()).await.unwrap();

    assert!(!report.full);
    assert_eq!(report.applied.pinned, vec![PathBuf::from("docs/g.txt")]);
    assert_eq!(s.pins.queued(), vec![s.root.path.join("docs/g.txt")]);
}

/// After a restart, the first cycle's Full reconcile is followed by the
/// sweep: a pinned file still online-only — its download lost to the
/// restart — is queued again, and the pins are counted.
#[tokio::test]
async fn the_sweep_after_a_restart_queues_a_pinned_file_not_downloaded_yet() {
    let s = World::read_only().await;
    listed(&s).await;
    pin_by_hand(&s.root.path, "docs");
    assert!(s.pins.queued().is_empty());
    s.feed(Some("L1"), json!([]), "L2").await;

    let restarted = s.listing();
    let report = restarted.cycle(&CancellationToken::new()).await.unwrap();

    assert!(report.full);
    assert_eq!(s.pins.queued(), vec![s.root.path.join("docs/f.txt")]);
    assert_eq!(s.state.get().local.pinned_count, 1);
}

#[tokio::test]
async fn another_account_blocks_the_folder_and_touches_nothing() {
    let s = World::read_only().await;
    s.store.call(|t| t.set_drive_id("D0")).await.unwrap();
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
    // The account hears which drive its token reaches.
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let neighbours = Neighbours {
        claimed: Arc::new(|_| false),
        drive_seen: Arc::new({
            let seen = Arc::clone(&seen);
            move |drive| seen.lock().unwrap().push(drive.to_owned())
        }),
    };
    let listing = Listing::new(ListingContext { neighbours: Some(neighbours), ..s.context() });
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::OtherAccount(_)), "{err:?}");
    assert!(err.blocking());
    assert!(!s.root.path.join("docs").exists());
    assert_eq!(s.state.get().cycle.sync_trouble, Some(SyncTrouble { text: err.to_string(), blocking: true }));
    assert_eq!(*seen.lock().unwrap(), vec!["D".to_owned()]);
}

#[tokio::test]
async fn no_network_is_said_and_is_not_blocking() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&s.graph.server).await;
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::Offline(_)), "{err:?}");
    assert!(!err.blocking());
    assert_eq!(s.state.get().cycle.sync_trouble, Some(SyncTrouble { text: err.to_string(), blocking: false }));
    s.feed(Some("L1"), json!([]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "a cycle after a failed one reconciles in full (Ruling R7)");
    assert_eq!(s.state.get().cycle.sync_trouble, None);
}

#[tokio::test]
async fn a_failed_reconcile_makes_the_next_cycle_full() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let root = File::open(&s.root.path).unwrap();
    placeholder::with_owner_write(&root, || root.remove_xattr(XATTR_ROOT)).unwrap();
    s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::Apply(_)), "{err:?}");
    placeholder::with_owner_write(&root, || root.set_xattr(XATTR_ROOT, s.root.root_id.as_bytes())).unwrap();
    s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the stored link was not advanced, and the folder is reconciled in full");
    assert!(s.root.path.join("docs/renamed.txt").is_file());
}

/// A rescue is one rename, never a copy (ruling): with the
/// preferred rescue directory on another filesystem than the folder, the
/// files go beside the folder instead — and the conflict says so, not
/// where they would have gone. The preferred one here is under `/proc`,
/// which is never the folder's filesystem, and nothing is ever written
/// there.
#[tokio::test]
async fn the_conflict_names_the_directory_the_files_really_went_to() {
    let preferred = PathBuf::from("/proc/konedrive-nonexistent/rescued");
    let s = World::new(Options { locked: true, rescue_dir: Some(preferred.clone()), ..Options::default() }).await;
    let listing = listed(&s).await;
    // A file of the user's own where the cloud now puts one.
    let docs = File::open(s.root.path.join("docs")).unwrap();
    placeholder::with_owner_write(&docs, || std::fs::write(s.root.path.join("docs/new.txt"), b"mine")).unwrap();
    s.feed(Some("L1"), json!([file("N", "D", "new.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();

    let beside = s.root.path.parent().unwrap().join(".konedrive-rescued-OneDrive");
    assert_eq!(report.applied.on_disk.rescued.len(), 1);
    let kept = &report.applied.on_disk.rescued[0].rescued;
    assert!(kept.starts_with(&beside), "{}", kept.display());
    assert_eq!(std::fs::read(kept).unwrap(), b"mine");
    let conflicts = konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].rescued, kept.display().to_string());
    assert!(!conflicts[0].rescued.starts_with(&preferred.display().to_string()));
}

/// `conflict` events are capped like every other
/// kind — 50 and "and N more" — while every conflict is still listed.
#[tokio::test]
async fn conflict_events_are_capped_like_the_other_kinds() {
    let s = World::read_only().await;
    let kept = tempfile::tempdir().unwrap();
    let rescued = (0..53)
        .map(|n| {
            let at = kept.path().join(format!("f{n:02}.txt"));
            std::fs::write(&at, b"mine").unwrap();
            Rescued { original: format!("docs/f{n:02}.txt").into(), rescued: at }
        })
        .collect();
    konedrive_tree::off_runtime(|| record(&s.report, &s.store, &s.root.path, &Applied { on_disk: OnDisk { rescued, ..OnDisk::default() }, ..Applied::default() }, Said::EachChange));

    let folder = s.root.path.display().to_string();
    let events = s.activity();
    assert_eq!(events.iter().filter(|(kind, at, _)| kind == "conflict" && *at != folder).count(), 50);
    assert!(events.contains(&("conflict".to_owned(), folder, "and 3 more".to_owned())), "{events:?}");
    assert_eq!(konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap().len(), 53, "every conflict is still listed");
}

/// A Changed pass that moves a local file out of
/// the way and then hands over to a Full reconcile. The Full pass finds
/// nothing left to rescue, so what the first pass rescued must be the
/// conflict — a row and an event — or it is lost. And what the first pass
/// only moved to the holding directory, which the Full pass then rescues
/// from there, is a conflict under the path it had in the folder.
#[tokio::test]
async fn a_rescue_made_before_a_full_hand_over_is_still_a_conflict() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    s.feed(Some("L1"), json!([file("K", "R", "kept.txt", "c1")]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    // Downloaded and changed here; then removed in OneDrive.
    let kept = s.root.path.join("kept.txt");
    write_version(&kept, b"downloaded", "c1");
    std::thread::sleep(Duration::from_millis(10));
    {
        use std::os::unix::fs::FileExt as _;
        placeholder::reopen_writable(&File::open(&kept).unwrap()).unwrap().write_all_at(b"and mine", 10).unwrap();
    }
    let root = File::open(&s.root.path).unwrap();
    placeholder::with_owner_write(&root, || std::fs::write(s.root.path.join("top.txt"), b"mine")).unwrap();
    // A new file for `docs`, which is not where the tree has it: the
    // Changed pass moves `kept.txt` to the holding directory, rescues
    // `top.txt` (shallower than the new file), then hands over.
    move_docs_away(&s.root.path);
    let delta = json!([{"id": "K", "deleted": {"state": "deleted"}}, file("T", "R", "top.txt", "c1"), file("M", "D", "m.txt", "c1")]);
    s.feed(Some("L2"), delta, "L3").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the Changed pass handed over to a Full one");

    let rows = konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap();
    let mut originals: Vec<_> = rows.iter().map(|c| c.original.clone()).collect();
    originals.sort();
    assert_eq!(originals, vec![s.full("kept.txt"), s.full("top.txt")], "each under the path it had in the folder");
    let rescued = |original: &str| rows.iter().find(|c| c.original == s.full(original)).unwrap().rescued.clone();
    assert_eq!(std::fs::read(rescued("top.txt")).unwrap(), b"mine");
    assert_eq!(std::fs::read(rescued("kept.txt")).unwrap(), b"downloadedand mine");
    assert!(s.activity().contains(&("conflict".to_owned(), s.full("top.txt"), rescued("top.txt"))), "{:?}", s.activity());
    assert_eq!(s.state.get().local.conflict_count, 2);
}

/// A first listing, and any Full reconcile, is ONE summary
/// event — "N items" — for the whole folder, not one event per item.
#[tokio::test]
async fn a_full_reconcile_is_one_listed_event() {
    let s = World::read_only().await;
    let _first = listed(&s).await;
    let folder = s.root.path.display().to_string();
    assert_eq!(s.activity(), vec![("listed".to_owned(), folder.clone(), "3 items".to_owned())]);
    // A new `Listing` reconciles its first cycle in full, whatever the
    // delta holds: two new files are still one event.
    s.feed(Some("L1"), json!([file("N", "D", "n.txt", "c1"), file("M", "D", "m.txt", "c1")]), "L2").await;
    let report = s.listing().cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full);
    assert_eq!(
        s.activity(),
        vec![("listed".to_owned(), folder.clone(), "3 items".to_owned()), ("listed".to_owned(), folder, "5 items".to_owned())]
    );
}

/// An incremental cycle logs what it did item by item, but
/// at most 50 events of a kind, then one "and N more" for the rest.
#[tokio::test]
async fn an_incremental_cycle_logs_at_most_fifty_of_a_kind_and_how_many_more() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let folder = s.root.path.display().to_string();
    let mut items: Vec<Value> = (0..53).map(|n| file(&format!("N{n}"), "D", &format!("n{n:02}.txt"), "c1")).collect();
    items.push(json!({"id": "F", "deleted": {"state": "deleted"}}));
    s.feed(Some("L1"), json!(items), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(!report.full, "the Changed scope is what is capped");
    let events = s.activity().split_off(1);
    let added_each = events.iter().filter(|(kind, at, _)| kind == "added" && *at != folder).count();
    assert_eq!(added_each, 50);
    assert!(events.contains(&("added".to_owned(), folder.clone(), "and 3 more".to_owned())), "{events:?}");
    assert!(events.contains(&("removed".to_owned(), s.full("docs/f.txt"), String::new())), "{events:?}");
    assert_eq!(events.len(), 52, "{events:?}");
}

/// A rescue is a conflict — a row, a `conflict` event saying
/// where the file was and where it is now — and the row drops off by
/// itself once the rescued file is gone.
#[tokio::test]
async fn a_rescue_is_a_conflict_until_its_file_is_gone() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let docs = File::open(s.root.path.join("docs")).unwrap();
    placeholder::with_owner_write(&docs, || std::fs::write(s.root.path.join("docs/new.txt"), b"mine")).unwrap();
    s.feed(Some("L1"), json!([file("N", "D", "new.txt", "c1")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    let rescued = report.applied.on_disk.rescued[0].rescued.display().to_string();
    let original = s.full("docs/new.txt");

    let conflicts = konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap();
    let rows: Vec<_> = conflicts.iter().map(|c| (c.original.clone(), c.rescued.clone())).collect();
    assert_eq!(rows, vec![(original.clone(), rescued.clone())]);
    assert_eq!(s.state.get().local.conflict_count, 1);
    assert_eq!(
        crate::status::snapshot::published_error(&s.state.get()),
        "",
        "a conflict is not a problem: LastError says nothing of it, Conflicts() says it all"
    );
    assert!(s.activity().contains(&("conflict".to_owned(), original, rescued.clone())), "{:?}", s.activity());

    std::fs::remove_file(&rescued).unwrap();
    s.feed(Some("L2"), json!([]), "L3").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    assert_eq!(s.state.get().local.conflict_count, 0, "a conflict whose file is gone drops off by the next cycle");
    assert!(konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap().is_empty());
}

/// `LastChecked` is when a cycle last succeeded — kept in the
/// store for the next start — and a cycle that fails leaves it alone.
#[tokio::test]
async fn last_checked_moves_only_when_a_cycle_succeeds() {
    let s = World::read_only().await;
    let before = activity::unix_now();
    let listing = listed(&s).await;
    let checked = s.state.get().cycle.last_checked;
    assert!(checked >= before, "{checked} < {before}");
    assert_eq!(s.store.call(move |x| x.last_checked()).await.unwrap(), Some(checked));

    // Marked, so that a failed cycle writing the time it ran — the same
    // second, most likely — could not pass for leaving it alone.
    s.state.update(|x| x.cycle.last_checked = 7);
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D2"})))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.graph.server).await;
    assert!(matches!(listing.cycle(&CancellationToken::new()).await, Err(CycleError::OtherAccount(_))));
    assert_eq!(s.state.get().cycle.last_checked, 7, "a failed cycle checked nothing");

    s.feed(Some("L1"), json!([]), "L2").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(s.state.get().cycle.last_checked >= before);
}

/// A file being filled when its change arrives is left for later
/// (`Counts::deferred`); a Changed scope never looks at it again, so the
/// next cycle is a Full one.
#[tokio::test]
async fn a_cycle_that_leaves_a_file_for_later_makes_the_next_one_full() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    let f_txt = s.root.path.join("docs/f.txt");
    placeholder::write_state(&File::open(&f_txt).unwrap(), State::Hydrating).unwrap();
    s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert_eq!(report.applied.counts.deferred, 1);
    // The fill ends without the file: it is online-only again.
    placeholder::write_state(&File::open(&f_txt).unwrap(), State::OnlineOnly).unwrap();
    s.feed(Some("L2"), json!([]), "L3").await;
    let report = listing.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the file left for later is looked at again");
    assert_eq!(placeholder::read_ctag(&File::open(&f_txt).unwrap()).unwrap().as_deref(), Some("c2"));
}

/// at every cycle: signing out and in as another account
/// between two cycles stops the folder before anything is placed.
#[tokio::test]
async fn a_sign_in_as_another_account_between_cycles_blocks_the_folder() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D2"})))
        .with_priority(1)
        .mount(&s.graph.server).await;
    s.feed(Some("L1"), json!([folder("N", "R", "new")]), "L2").await;
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, CycleError::OtherAccount(_)), "{err:?}");
    assert!(err.blocking());
    assert!(!s.root.path.join("new").exists());
}

/// the drive a folder was listed from is kept
/// in `config.toml` as the account's too, so a tree store rebuilt empty —
/// its `meta` has forgotten the drive — still refuses another account.
/// The first cycle writes it there.
#[tokio::test]
async fn the_drive_kept_beside_the_root_outlives_a_rebuilt_store() {
    let s = World::read_only().await;
    let config_dir = tempfile::tempdir().unwrap();
    let config = crate::config::Paths::in_dir(config_dir.path());
    let store = Arc::new(crate::config::ConfigStore::open(&config, async { false }).await);
    let account = store.add_account("Personal").unwrap().id;
    let record = DriveRecord { store: Arc::clone(&store), account: account.clone(), recorded: None };
    listed_with(&s, ListingContext { drive_record: Some(record), ..s.context() }).await;
    assert_eq!(store.account(&account).unwrap().drive_id, crate::config::DriveId::new("D"), "written by the first cycle");

    let rebuilt = Store::new(TreeStore::in_memory().unwrap());
    let record = DriveRecord { store, account, recorded: Some("D0".into()) };
    let listing = Listing::new(ListingContext { store: rebuilt, drive_record: Some(record), ..s.context() });
    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(&err, CycleError::OtherAccount(drive) if drive == "D0"), "{err:?}");
}

/// A drive is one account (design §8.2, review M1): an account with no drive recorded
/// yet, signed in to a drive another account has, does not list it into a second folder
/// — the folder is blocked, naming that account, and nothing is placed.
#[tokio::test]
async fn a_drive_another_account_has_is_not_listed_into_a_second_folder() {
    let s = World::read_only().await;
    let config_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::config::ConfigStore::open(&crate::config::Paths::in_dir(config_dir.path()), async { false }).await);
    let first = store.add_account("Work").unwrap().id;
    store.record_drive(&first, &crate::config::DriveId::new("D").unwrap()).unwrap();
    let account = store.add_account("Personal").unwrap().id;
    let record = DriveRecord { store: Arc::clone(&store), account: account.clone(), recorded: None };
    let listing = Listing::new(ListingContext { drive_record: Some(record), ..s.context() });

    let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();

    assert!(matches!(&err, CycleError::DriveTaken(label) if label == "Work"), "{err:?}");
    assert!(err.blocking());
    assert_eq!(store.account(&account).unwrap().drive_id, None);
    assert!(std::fs::read_dir(&s.root.path).unwrap().next().is_none(), "nothing is placed");
}

/// A cycle whose future is dropped part-way is a failed one: the Full
/// reconcile it had taken is asked for again, and a listing it was
/// running is no longer said to run.
#[tokio::test]
async fn a_dropped_cycle_leaves_a_full_reconcile_and_no_listing_behind() {
    let s = World::read_only().await;
    listed(&s).await;
    let restarted = s.listing();
    // The feed has expired, and the listing that follows is slow.
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(410))
        .up_to_n_times(1).with_priority(1)
        .mount(&s.graph.server).await;
    s.feed_after(None, json!([root_item()]), "L9", Duration::from_secs(30)).await;
    let dropped = tokio::time::timeout(Duration::from_millis(500), restarted.cycle(&CancellationToken::new())).await;
    assert!(dropped.is_err(), "still listing when dropped");
    assert!(!s.state.get().cycle.listing, "a dropped listing is not said to run");
    s.feed(Some("L1"), json!([]), "L2").await;
    let report = restarted.cycle(&CancellationToken::new()).await.unwrap();
    assert!(report.full, "the Full reconcile the dropped cycle had taken is asked for again");
}

/// Cycles of one listing never overlap (a `refresh` and the poller's
/// own, say): the second starts from the link the first left.
#[tokio::test]
async fn two_cycles_at_once_run_one_after_the_other() {
    let s = World::read_only().await;
    let listing = listed(&s).await;
    s.feed_after(Some("L1"), json!([folder("N", "R", "new")]), "L2", Duration::from_millis(300)).await;
    s.feed(Some("L2"), json!([]), "L3").await;
    let token = CancellationToken::new();
    let (first, second) = tokio::join!(listing.cycle(&token), listing.cycle(&token));
    first.unwrap();
    second.unwrap();
    assert!(s.root.path.join("new").is_dir());
    assert_eq!(s.store.call(|t| t.delta_link()).await.unwrap(), Some(s.link_to("L3")));
}

/// A cycle dropped while its reconcile runs keeps the lifecycle lock, and
/// its turn, until the reconcile has stopped: a Forget must not take the
/// lock off a folder something is still changing, and no other cycle may
/// rebuild `staging` under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_cycle_keeps_its_locks_until_its_reconcile_stops() {
    let s = World::read_only().await;
    let (reached, release) = s.helper.stall_on("/.konedrive-new-N");
    let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
    let listing = Listing::new(ListingContext {
        lease: crate::remote::listing::Lease::on(&lifecycle),
        ..s.context()
    });
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
    listing.cycle(&CancellationToken::new()).await.unwrap();
    s.feed(Some("L1"), json!([folder("N", "R", "new")]), "L2").await;
    let reached = tokio::task::spawn_blocking(move || reached.recv().unwrap());
    let token = CancellationToken::new();
    tokio::select! {
        _ = listing.cycle(&token) => panic!("the reconcile cannot end before the helper answers"),
        _ = reached => {}
    }
    s.feed(Some("L2"), json!([]), "L3").await;
    let second = tokio::spawn({
        let listing = Arc::clone(&listing);
        async move { listing.cycle(&CancellationToken::new()).await }
    });
    let early = tokio::time::timeout(Duration::from_millis(300), lifecycle.write()).await;
    assert!(early.is_err(), "the lock is held while the folder is still being changed");
    assert!(!second.is_finished(), "no other cycle runs while the dropped one's reconcile does");
    release.send(()).unwrap();
    let report = second.await.unwrap().unwrap();
    assert!(report.full, "the dropped cycle counts as a failed one");
    let later = tokio::time::timeout(Duration::from_secs(5), lifecycle.write()).await;
    assert!(later.is_ok(), "and the lock is let go once the reconcile has stopped");
    drop(later);
    assert!(s.root.path.join("new").is_dir());
}

/// a download cut off by a restart keeps its
/// checkpoint through startup recovery AND through the Full reconcile
/// that the restarted sync's first cycle is. The fill's writes moved the
/// time to now, and that reconcile took the file for a new version and
/// punched the partial download away — 1.5 GB, seconds after login. Now
/// only the time is put back.
#[tokio::test]
async fn a_partial_download_survives_recovery_and_the_full_reconcile_after_it() {
    use std::os::unix::fs::FileExt as _;
    let s = World::read_only().await;
    listed(&s).await;
    let path = s.root.path.join("docs/f.txt");
    {
        let file = placeholder::reopen_writable(&File::open(&path).unwrap()).unwrap();
        placeholder::write_state(&file, State::Hydrating).unwrap();
        file.write_all_at(b"abcd", 0).unwrap();
        placeholder::write_progress(&file, &placeholder::Progress { ctag: "c1".into(), bytes: 4 }).unwrap();
        file.write_all_at(b"ef", 4).unwrap();
    }
    let nowhere = crate::helper::Clearance::NoLink(s.rescue_dir.join("no-helper.sock"));
    let recovered = crate::hydration::recovery::recover(&nowhere, &s.root, &InodeLocks::new()).await.unwrap();
    assert_eq!(recovered.reset, 1, "{recovered:?}");

    s.feed(Some("L1"), json!([]), "L2").await;
    let restarted = s.listing();
    let report = restarted.cycle(&CancellationToken::new()).await.unwrap();

    assert!(report.full, "a restarted sync reconciles in full first");
    let file = File::open(&path).unwrap();
    assert_eq!(placeholder::read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(placeholder::read_progress(&file).unwrap(), Some(placeholder::Progress { ctag: "c1".into(), bytes: 4 }));
    assert_eq!(&std::fs::read(&path).unwrap()[..6], b"abcd\0\0", "the checkpointed bytes stay, the rest is punched");
    assert_eq!(file.metadata().unwrap().mtime(), 1_714_557_600, "the cloud's time is back");
}

/// RE6: one failure of the tree store is the same trouble wherever a reconcile meets it,
/// and it stops the folder. Reading the root's id and every call the materializer makes go
/// through `applying`; the commit and every `on_store` call go through `From<TreeError>`.
#[tokio::test]
async fn a_store_failure_is_the_same_trouble_wherever_a_reconcile_meets_it() {
    let failure = || konedrive_tree::TreeError::Io(std::io::Error::other("disk I/O error"));
    let reading_the_root = applying(ApplyError::from(failure()));
    let committing = CycleError::from(failure());
    assert!(matches!(reading_the_root, CycleError::Store(_)), "{reading_the_root:?}");
    assert!(reading_the_root.blocking() && committing.blocking(), "{reading_the_root:?}, {committing:?}");
    assert_eq!(reading_the_root.to_string(), "the tree store: disk I/O error");
    assert_eq!(committing.to_string(), reading_the_root.to_string());

    // As published: the folder reads `error`, and `LastError` has the store's words once.
    let s = World::read_only().await;
    let listing = Listing::new(s.context());
    listing.publish_outcome(&Err(reading_the_root));
    let published = s.state.get();
    assert_eq!(published.cycle.sync_trouble, Some(SyncTrouble { text: "the tree store: disk I/O error".into(), blocking: true }));
    assert_eq!(crate::status::snapshot::published_state(&SyncSnapshot { folder: crate::status::snapshot::FolderStatus { root_state: crate::status::snapshot::RootState::Ready, ..published.folder.clone() }, ..published }), "error");
}

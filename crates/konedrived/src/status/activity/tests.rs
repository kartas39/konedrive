use std::fs::File;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;

use super::*;
use crate::status::snapshot::SyncSnapshot;

fn event_at(kind: Kind, path: &str) -> Event {
    Event { at: 1, kind, path: path.into(), detail: String::new() }
}

/// Issue #39: the end of a cycle looks over the conflicts a batch at a
/// time, round the list, dropping those whose file is gone; the list on
/// the bus still looks at every one.
#[test]
fn conflicts_are_looked_over_a_batch_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = konedrive_tree::Store::new(konedrive_tree::TreeStore::open(&dir.path().join("tree.sqlite")).unwrap());
    let rows: Vec<ConflictRow> = (0..450)
        .map(|i| {
            let rescued = dir.path().join(format!("c{i:03}"));
            // Every tenth file is gone already.
            if i % 10 != 0 {
                File::create(&rescued).unwrap();
            }
            ConflictRow { at: i, original: format!("/r/c{i:03}"), rescued: rescued.display().to_string(), kind: konedrive_tree::ConflictKind::Rescued }
        })
        .collect();
    store.call_blocking(move |s| s.add_conflicts(&rows)).unwrap();
    let state = SyncStateHandle::new(SyncSnapshot::default());
    let activity = Activity::new(state.clone());
    activity.attach(store.clone(), Path::new("/r"));
    assert_eq!(state.get().local.conflict_count, 450 - 20, "the first batch of 200 dropped its 20 gone");
    activity.prune();
    assert_eq!(state.get().local.conflict_count, 450 - 40);
    activity.prune();
    assert_eq!(state.get().local.conflict_count, 450 - 45, "the last 50, and round again");
    activity.prune();
    assert_eq!(state.get().local.conflict_count, 405);
    std::fs::remove_file(dir.path().join("c449")).unwrap();
    assert_eq!(activity.conflicts().unwrap().len(), 404, "the bus's list looks at every one");
    assert_eq!(state.get().local.conflict_count, 404);
}

/// The cap on its own: the first `per_kind` of each kind in
/// the order they came, then one "and N more" per kind that had more.
#[test]
fn capped_keeps_the_first_of_each_kind_and_counts_the_rest() {
    let mut events: Vec<Event> = (0..4).map(|n| event_at(Kind::Added, &format!("/r/a{n}"))).collect();
    events.push(event_at(Kind::Removed, "/r/x"));
    events.extend((4..6).map(|n| event_at(Kind::Added, &format!("/r/a{n}"))));
    let out = capped(events, 3, "/r");
    let shown: Vec<_> = out.iter().map(|e| (e.kind.as_str(), e.path.as_str(), e.detail.as_str())).collect();
    assert_eq!(
        shown,
        vec![
            ("added", "/r/a0", ""),
            ("added", "/r/a1", ""),
            ("added", "/r/a2", ""),
            ("removed", "/r/x", ""),
            ("added", "/r", "and 3 more"),
        ]
    );
}

fn in_folder(root: &str) -> SyncStateHandle {
    SyncStateHandle::new(SyncSnapshot { folder: crate::status::snapshot::FolderStatus { root_path: root.into(), ..Default::default() }, ..SyncSnapshot::default() })
}

fn fresh_store() -> Store {
    Store::new(konedrive_tree::TreeStore::in_memory().unwrap())
}

/// An event of the folder registered before is
/// not carried into the store of the one attached now.
#[test]
fn an_event_of_another_folder_is_not_carried_into_the_one_attached() {
    let state = in_folder("/a");
    let activity = Activity::new(state.clone());
    activity.record_blocking(vec![event(Kind::Downloaded, "/a/f.bin", "1 B")]);
    state.update(|s| s.folder.root_path = "/b".into());
    activity.record_blocking(vec![event(Kind::Downloaded, "/b/g.bin", "1 B")]);
    activity.attach(fresh_store(), Path::new("/b"));
    let paths: Vec<String> = activity.recent(10).unwrap().into_iter().map(|e| e.path).collect();
    assert_eq!(paths, vec!["/b/g.bin".to_owned()]);
}

/// Nothing recorded while a store is attached is
/// lost — not what memory held, nor what is recorded while it moves
/// into the store. A writer records the whole time the store is
/// attached; every event it recorded must be in the store after.
#[test]
fn nothing_recorded_while_a_store_is_attached_is_lost() {
    use std::sync::atomic::AtomicBool;
    let activity = Arc::new(Activity::new(in_folder("/r")));
    activity.record_blocking((0..100).map(|n| event(Kind::Downloaded, format!("/r/held{n}"), "")).collect());
    // Made first, so that the attach starts the moment the writer runs.
    let store = fresh_store();
    let (recorded, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
    let writer = {
        let (activity, recorded, stop) = (Arc::clone(&activity), Arc::clone(&recorded), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(SeqCst) && n < 100 {
                activity.record_blocking(vec![event(Kind::Downloaded, format!("/r/live{n}"), "")]);
                n += 1;
                recorded.store(n, SeqCst);
                std::thread::yield_now();
            }
            n
        })
    };
    while recorded.load(SeqCst) < 5 {
        std::thread::yield_now();
    }
    activity.attach(store, Path::new("/r"));
    stop.store(true, SeqCst);
    let written = writer.join().unwrap();
    let kept: std::collections::HashSet<String> = activity.recent(200).unwrap().into_iter().map(|e| e.path).collect();
    let lost: Vec<String> = (0..written)
        .map(|n| format!("/r/live{n}"))
        .chain((0..100).map(|n| format!("/r/held{n}")))
        .filter(|path| !kept.contains(path))
        .collect();
    assert!(lost.is_empty(), "{} of {} lost: {lost:?}", lost.len(), written + 100);
}

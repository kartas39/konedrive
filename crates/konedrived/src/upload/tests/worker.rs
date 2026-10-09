//! The worker's loop: when it waits, and which row goes next.

use super::*;
use crate::folder::locks::InodeKey;

/// A row in backoff goes again when its time comes, though another row is still in
/// flight: it does not wait for an unrelated upload to end. The other row is held at the
/// read of its file, by the file's lock, until the first is in OneDrive.
#[test]
fn a_row_that_falls_due_goes_while_another_is_still_in_flight() {
    let w = World::new(&[]);
    w.make_file("a.txt", b"a");
    w.make_file("b.txt", b"b");
    w.cloud(|c| c.script("POST", "a.txt", ResponseTemplate::new(502), 1));
    let held = w.h.block_on(w.locks.lock(InodeKey::of(&File::open(w.path("b.txt")).unwrap()).unwrap()));
    let engine = w.h.engine();
    let task = w.h.runtime.spawn({
        let engine = Arc::clone(&engine);
        async move { engine.drain(&CancellationToken::new()).await }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while w.content("a.txt").as_deref() != Some(b"a".as_slice()) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(w.content("a.txt").as_deref(), Some(b"a".as_slice()), "a.txt went again while b.txt was held: {:?}", w.summary());
    assert_ne!(w.content("b.txt").as_deref(), Some(b"b".as_slice()), "b.txt is still held");
    drop(held);
    w.h.runtime.block_on(task).unwrap();
    assert_committed(&w, "a.txt", "a.txt");
    assert_committed(&w, "b.txt", "b.txt");
}

/// `docs/design/writes.md` §2.3: while the host's write gate is closed nothing is sent and
/// the rows keep their place; once it is open they go.
#[test]
fn a_closed_write_gate_sends_nothing_until_it_opens() {
    let w = World::new(&[]);
    w.make_file("a.txt", b"a");
    *w.h.host.gate.lock().unwrap() = Some("the account is read-only".into());
    let engine = w.run();
    assert_eq!(w.cloud(|c| c.log.len()), 0);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Ready)]);

    *w.h.host.gate.lock().unwrap() = None;
    w.h.drain(&engine);
    assert_committed(&w, "a.txt", "a.txt");
}
/// Four independent small files run at once, and a child waits for its
/// parent's `mkdir`: the row only sends once the folder it goes into is in
/// OneDrive. What is checked is that all four are in flight together, not how
/// many uploads there are at that moment: nothing caps them at four (the
/// account's transfer pool starts at 16 slots), and the child's own upload,
/// which starts as soon as its `mkdir` is answered, runs while the four are
/// still held — a sample that falls during it counts five.
#[test]
fn four_independent_files_run_at_once_and_a_child_waits_for_its_mkdir() {
    let w = World::new(&[]);
    let names = ["a.txt", "b.txt", "c.txt", "d.txt"];
    for name in names {
        w.make_file(name, name.as_bytes());
    }
    w.make_directory("dir");
    w.make_file("dir/child.txt", b"child");
    assert_eq!(w.rows().len(), 6, "{:?}", w.summary());

    // Each small file opens an upload session first (`POST
    // createUploadSession`), answered late here: each of the four stays in
    // flight for at least that long from its start, a margin far wider than
    // the worker takes to start the other three. It holds up neither the
    // mkdir nor the child behind it.
    w.cloud(|c| {
        for name in names {
            c.delay("POST", name, Duration::from_secs(1), 1);
        }
    });

    let worker = OutboxWorker::new(w.h.config());
    let (together, seen) = w.h.runtime.block_on(async {
        worker.start();
        worker.wake();
        let mut seen = Vec::new();
        let mut together = false;
        let mut waited = 0;
        while waited < 500 {
            seen = worker.handle.engine.status().uploads.into_iter().map(|u| u.rel).collect::<Vec<_>>();
            together = names.iter().all(|name| seen.iter().any(|rel| rel == Path::new(name)));
            if together {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        waited = 0;
        while !w.store.call(|s| s.outbox_rows()).await.unwrap().is_empty() && waited < 500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        worker.stop().await;
        (together, seen)
    });

    assert!(together, "the four independent small files were never in flight together; last seen: {seen:?}");
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    for (rel, path) in [("a.txt", "a.txt"), ("b.txt", "b.txt"), ("c.txt", "c.txt"), ("d.txt", "d.txt"), ("dir", "dir"), ("dir/child.txt", "dir/child.txt")] {
        assert_committed(&w, rel, path);
    }
    let position = |fragment: &str| w.cloud(|c| c.log.iter().position(|(_, p)| p.contains(fragment)));
    let mkdir_at = position("children").expect("the mkdir request");
    let child_at = position("child.txt").expect("the child's content request");
    assert!(mkdir_at < child_at, "the child's row waits for its parent's mkdir: {mkdir_at} vs {child_at}");
}

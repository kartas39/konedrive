//! The worker's loop: when it waits, and which row goes next.

use super::*;
use crate::folder::locks::InodeKey;

/// A row in backoff goes again when its time comes, though another row is still in
/// flight: it does not wait for an unrelated upload to end. The other row is held at the
/// read of its file, by the file's lock, until the first is in OneDrive.
#[test]
fn a_row_that_falls_due_goes_while_another_is_still_in_flight() {
    let w = World::new(&[]);
    w.write("a.txt", b"a");
    w.write("b.txt", b"b");
    w.examine(&[("", "a.txt"), ("", "b.txt")]);
    w.cloud(|c| c.script("POST", "a.txt", ResponseTemplate::new(502), 1));
    let held = w.locks.try_lock(InodeKey::of(&File::open(w.path("b.txt")).unwrap()).unwrap()).expect("nobody holds b.txt");
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
    w.write("a.txt", b"a");
    w.examine(&[("", "a.txt")]);
    *w.h.host.gate.lock().unwrap() = Some("the account is read-only".into());
    let engine = w.run();
    assert_eq!(w.cloud(|c| c.log.len()), 0);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Ready)]);

    *w.h.host.gate.lock().unwrap() = None;
    w.h.drain(&engine);
    assert_committed(&w, "a.txt", "a.txt");
}

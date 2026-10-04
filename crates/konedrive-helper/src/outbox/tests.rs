use std::os::fd::OwnedFd;
use std::time::Instant;

use konedrive_proto::PROTOCOL_VERSION;
use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};

use super::*;

fn pair() -> (UnixStream, UnixStream) {
    let (a, b) =
        socketpair(AddressFamily::Unix, SockType::SeqPacket, None, SockFlag::empty()).unwrap();
    let a: OwnedFd = a;
    let b: OwnedFd = b;
    (UnixStream::from(a), UnixStream::from(b))
}

fn request(req_id: u64) -> Outgoing {
    Outgoing { message: ToDaemon::HydrateRequest { req_id }, fd: None }
}

/// The whole point of: a peer that never reads must not be
/// able to make the helper wait. Before this, the same peer blocked
/// `Channel::send` permanently after 278 datagrams, with a worker thread
/// and the connection's writer mutex held.
#[test]
fn a_peer_that_never_reads_gets_refusals_not_a_stalled_caller() {
    let (ours, theirs) = pair();
    let outbox = Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap();

    let started = Instant::now();
    let mut refused = 0usize;
    // Far more than the queue and the socket buffer together can hold.
    for req_id in 0..4096 {
        if outbox.try_send(request(req_id)).is_err() {
            refused += 1;
        }
    }
    assert!(refused > 0, "a peer that never reads must eventually refuse");
    assert!(
        started.elapsed() < SEND_TIMEOUT,
        "queueing must never wait on the peer: took {:?}",
        started.elapsed()
    );
    drop(theirs);
}

/// A refused message comes back rather than vanishing, because the caller
/// owns an event fd that has to be answered either way.
#[test]
fn a_refused_message_is_handed_back_intact() {
    let (ours, theirs) = pair();
    let outbox = Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap();
    let mut returned = None;
    for req_id in 0..4096 {
        if let Err(back) = outbox.try_send(request(req_id)) {
            returned = Some(back);
            break;
        }
    }
    let back = returned.expect("something must be refused");
    assert!(matches!(back.message, ToDaemon::HydrateRequest { .. }));
    drop(theirs);
}

/// A closed outbox refuses immediately, so a connection that has gone
/// away can never swallow an event fd.
#[test]
fn a_closed_outbox_refuses_everything() {
    let (ours, theirs) = pair();
    let outbox = Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap();
    outbox.close();
    assert!(outbox.try_send(request(1)).is_err());
    drop(theirs);
}

/// Windows short enough for a unit test. The ratio is what matters:
/// several `SO_SNDTIMEO` expiries fit inside one liveness window, as in
/// production (10 s against 60 s).
const FAST: Timing = Timing {
    send_timeout: Duration::from_millis(50),
    liveness_window: Duration::from_secs(1),
};

/// Queues until the outbox stays full, so that the socket buffer is
/// full, the writer thread is blocked in `sendmsg`, and the queue behind
/// it is full too — whatever this host's socket buffer size happens to
/// be. A single refusal is not enough: the queue can fill before the
/// writer thread has taken anything off it at all. Full means "refused,
/// and still refused after the writer has had 100 ms to make room".
/// Returns how many messages were accepted.
fn fill(outbox: &Outbox) -> u64 {
    let mut accepted = 0;
    let mut refused_last_time = false;
    loop {
        if outbox.try_send(request(accepted)).is_ok() {
            accepted += 1;
            refused_last_time = false;
            continue;
        }
        if refused_last_time {
            return accepted;
        }
        refused_last_time = true;
        std::thread::sleep(Duration::from_millis(100));
        assert!(accepted < 65_536, "the outbox never stayed full; the peer must be reading");
    }
}

/// Whether the helper's side has torn the connection down. The writer's
/// teardown is `shutdown(SHUT_RDWR)`, which the peer sees as `POLLHUP`
/// even while it still has unread datagrams queued.
fn hung_up(peer: &UnixStream) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let mut fds = [PollFd::new(peer.as_fd(), PollFlags::empty())];
    poll(&mut fds, PollTimeout::ZERO).unwrap();
    fds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLHUP))
}

/// Reads until `expected` messages arrived or the connection ended, and
/// says how many arrived.
fn drain(peer: UnixStream, expected: u64) -> u64 {
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut channel = Channel::new(peer).unwrap();
    let mut received = 0;
    while received < expected {
        match channel.recv::<ToDaemon>() {
            Ok((ToDaemon::HydrateRequest { req_id }, _)) => {
                assert_eq!(req_id, received, "messages must arrive whole and in order");
                received += 1;
            }
            Ok((other, _)) => panic!("unexpected {other:?}"),
            Err(_) => break,
        }
    }
    received
}

/// The half burst needed. A daemon that is slow to
/// read — its socket full, the helper's writer blocked in `sendmsg`
/// through many `SO_SNDTIMEO` expiries — but that keeps reporting
/// finished fills is busy, not wedged, and keeps its connection. Before,
/// the first expiry ended the connection and every opener enrolled on it
/// was denied `EIO`.
#[test]
fn a_slow_daemon_that_keeps_reporting_is_never_disconnected() {
    let (ours, theirs) = pair();
    let outbox = Outbox::start_with(ours.try_clone().unwrap(), ours, "test", FAST).unwrap();
    let queued = fill(&outbox);

    // Three whole liveness windows, and sixty send timeouts, with the
    // daemon reading nothing but reporting every 50 ms.
    let until = Instant::now() + FAST.liveness_window * 3;
    while Instant::now() < until {
        outbox.heard_from_peer();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!hung_up(&theirs), "a daemon that keeps talking must keep its connection");

    assert_eq!(
        drain(theirs, queued),
        queued,
        "every message queued while the daemon was slow must still be delivered"
    );
}

/// The half needs kept. A daemon that neither
/// reads nor says anything for a whole window is wedged, and its
/// connection ends — which is what gets its enrolled openers answered
/// rather than suspended for as long as the helper runs. Not before the
/// window, though.
#[test]
fn a_silent_daemon_is_disconnected_after_the_window_and_not_before() {
    let (ours, theirs) = pair();
    let outbox = Outbox::start_with(ours.try_clone().unwrap(), ours, "test", FAST).unwrap();
    // Before the first message, so that it is no later than the moment
    // the writer's send got stuck: the connection must outlive this by
    // a whole window.
    let started = Instant::now();
    fill(&outbox);

    let deadline = started + FAST.liveness_window * 10;
    while !hung_up(&theirs) {
        assert!(
            Instant::now() < deadline,
            "a daemon silent for ten liveness windows was never disconnected"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        started.elapsed() >= FAST.liveness_window,
        "disconnected after {:?}, inside the {:?} window",
        started.elapsed(),
        FAST.liveness_window
    );
}

/// Silence is counted from when the send got stuck, not only from the
/// daemon's last message. An idle daemon has said nothing for as long as
/// it has been idle; a burst that then fills its socket must not find an
/// hour of "silence" already on the clock at the first `SO_SNDTIMEO` and
/// end the connection — that would be defect again, only for
/// daemons that had been quiet first.
#[test]
fn a_daemon_idle_before_a_burst_is_not_disconnected_by_its_first_blocked_send() {
    let (ours, theirs) = pair();
    let outbox = Outbox::start_with(ours.try_clone().unwrap(), ours, "test", FAST).unwrap();

    // Idle — nothing sent either way — for two whole windows.
    std::thread::sleep(FAST.liveness_window * 2);

    // `fill` returns at most ~200 ms after the writer got stuck; this
    // takes it to at most ~600 ms — a dozen send timeouts into the
    // burst, and still inside the window.
    let queued = fill(&outbox);
    std::thread::sleep(FAST.liveness_window * 2 / 5);
    assert!(
        !hung_up(&theirs),
        "a daemon that was idle before a burst must not be disconnected by the burst"
    );
    assert_eq!(drain(theirs, queued), queued);
}

/// Runs `f` on a thread of its own and says whether it returned within
/// `limit` — so that a call which blocks forever fails the test instead
/// of hanging it.
fn within<T: Send + 'static>(
    limit: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok()
}

/// Acknowledges from a thread of its own until `count` are queued or one
/// is refused; hands back how many were queued.
fn acknowledge(outbox: &Arc<Outbox>, count: usize) -> std::thread::JoinHandle<usize> {
    let outbox = Arc::clone(outbox);
    std::thread::spawn(move || {
        for sent in 0..count {
            if outbox.send_ack(Errno::from_wire(sent as i32)).is_err() {
                return sent;
            }
        }
        count
    })
}

/// Waits until `sender` has been blocked — still running, having queued
/// nothing for 200 ms — so the tests below start from a reader thread
/// that is really waiting for room, whatever this host's socket buffer
/// holds.
fn until_blocked(sender: &std::thread::JoinHandle<usize>) {
    std::thread::sleep(Duration::from_millis(500));
    assert!(!sender.is_finished(), "a peer that never reads must eventually make it wait");
}

/// The helper's own requests fill their compartment — the
/// peer is slow to read, the writer is blocked in `sendmsg` — and then the
/// daemon makes a call. Its `Ack` must be queued, at once, and the
/// connection must stay. `serve_one` used to end the connection right
/// here, because the `Ack` did not fit in a queue the requests had
/// filled: a daemon torn down for backpressure, a moment after proving it
/// was alive.
#[test]
fn an_ack_fits_when_requests_have_filled_the_outbox() {
    let (ours, theirs) = pair();
    let outbox = Arc::new(Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap());
    let queued = fill(&outbox);

    let acking = Arc::clone(&outbox);
    let acked = within(Duration::from_secs(2), move || acking.send_ack(Ok(())).is_ok());
    assert_eq!(acked, Some(true), "an Ack must be queued at once, not refused or kept waiting");
    assert!(!hung_up(&theirs), "and the connection must stay");

    // Everything arrives, in the order it was queued.
    theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut peer = Channel::new(theirs).unwrap();
    for expected in 0..queued {
        match peer.recv::<ToDaemon>() {
            Ok((ToDaemon::HydrateRequest { req_id }, _)) => assert_eq!(req_id, expected),
            other => panic!("request {expected} did not arrive: {other:?}"),
        }
    }
    assert!(
        matches!(peer.recv::<ToDaemon>(), Ok((ToDaemon::Ack { errno: 0 }, _))),
        "the Ack arrives after the requests queued before it"
    );
}

/// The other half: past [`ACK_RESERVE`] replies unread, an
/// `Ack` waits for room — it is never refused — and when the peer reads,
/// every one of them arrives, whole and in order, on the same connection.
#[test]
fn beyond_its_reserve_an_ack_waits_for_room_and_is_never_refused() {
    const COUNT: usize = 2000;
    let (ours, theirs) = pair();
    let outbox = Arc::new(Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap());
    let sender = acknowledge(&outbox, COUNT);
    until_blocked(&sender);
    assert!(!hung_up(&theirs), "waiting for room must not end the connection");

    theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut peer = Channel::new(theirs).unwrap();
    for expected in 0..COUNT {
        match peer.recv::<ToDaemon>() {
            Ok((ToDaemon::Ack { errno }, _)) => assert_eq!(errno, expected as i32),
            other => panic!("Ack {expected} of {COUNT} did not arrive: {other:?}"),
        }
    }
    assert_eq!(sender.join().unwrap(), COUNT, "every Ack must have been queued");
}

/// A reader thread waiting for room is released the moment the
/// connection is closed: waiting for room must never outlive the
/// connection it is waiting on.
#[test]
fn closing_releases_an_ack_waiting_for_room() {
    let (ours, theirs) = pair();
    let outbox = Arc::new(Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap());
    let sender = acknowledge(&outbox, 100_000);
    until_blocked(&sender);

    outbox.close();
    let released = within(Duration::from_secs(2), move || sender.join().unwrap());
    assert!(
        released.is_some_and(|queued| queued < 100_000),
        "the waiting Ack must be refused once the connection is closed"
    );
    drop(theirs);
}

/// And by the writer giving up — here for a whole liveness window of
/// silence, which is exactly the peer the waiting is for: one that
/// neither reads its replies nor says anything, and whose reader thread,
/// waiting for room, is not reading anything it sends either.
#[test]
fn the_writer_giving_up_releases_an_ack_waiting_for_room() {
    let (ours, theirs) = pair();
    let outbox =
        Arc::new(Outbox::start_with(ours.try_clone().unwrap(), ours, "test", FAST).unwrap());
    let sender = acknowledge(&outbox, 100_000);
    until_blocked(&sender);

    let released =
        within(FAST.liveness_window * 10, move || sender.join().unwrap());
    assert!(
        released.is_some_and(|queued| queued < 100_000),
        "a reader thread waiting for room on a connection the writer has ended must be \
         released, or it is held for the life of the process"
    );
    assert!(hung_up(&theirs), "and the connection is over");
}

/// The ordinary case still works: a peer that reads gets its messages,
/// in order.
#[test]
fn messages_reach_a_peer_that_reads() {
    let (ours, theirs) = pair();
    let outbox = Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap();
    let mut peer = Channel::new(theirs).unwrap();

    outbox
        .try_send(Outgoing {
            message: ToDaemon::Welcome { version: PROTOCOL_VERSION },
            fd: None,
        })
        .map_err(|_| ())
        .unwrap();
    outbox.try_send(request(7)).map_err(|_| ()).unwrap();

    let (first, _) = peer.recv::<ToDaemon>().unwrap();
    assert!(matches!(first, ToDaemon::Welcome { .. }), "{first:?}");
    let (second, _) = peer.recv::<ToDaemon>().unwrap();
    assert!(matches!(second, ToDaemon::HydrateRequest { req_id: 7 }), "{second:?}");
}

/// A writer thread that panics ends its connection as one that stops does:
/// the queue is closed, so nothing more is taken for it, and the socket is
/// shut down, so the thread reading it stops waiting.
#[test]
fn a_writer_that_panics_still_ends_its_connection() {
    let (ours, theirs) = pair();
    let pending = Arc::new(Pending {
        queue: Mutex::new(Queue { items: VecDeque::new(), acks: 0, requests: 0, closed: false }),
        queued: Condvar::new(),
        room: Condvar::new(),
    });
    let end = WriterEnd { pending: Arc::clone(&pending), socket: ours.try_clone().unwrap() };
    let writer = std::thread::spawn(move || {
        let _end = end;
        panic!("deliberate: the writer thread");
    });
    assert!(writer.join().is_err());

    assert!(pending.lock().closed, "a panicked writer left its queue open");
    let mut reader = Channel::new(ours).unwrap();
    let error = reader.recv::<ToDaemon>().expect_err("the reader would have waited for ever");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error:?}");
    drop(theirs);
}

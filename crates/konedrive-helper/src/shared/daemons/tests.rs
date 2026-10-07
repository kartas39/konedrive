use super::*;

use std::os::unix::net::UnixStream;

use nix::sys::socket::{AddressFamily, SockFlag, SockType};

/// A registration for connection `conn` of `uid`, backed by a real
/// outbox on a socket pair nobody reads — the registry only ever
/// looks at `uid` and `conn`.
fn daemon(uid: u32, conn: u64) -> (Daemon, UnixStream) {
    daemon_of_pid(uid, conn, 1)
}

fn daemon_of_pid(uid: u32, conn: u64, pid: i32) -> (Daemon, UnixStream) {
    use nix::sys::socket::socketpair;
    let (ours, theirs) =
        socketpair(AddressFamily::Unix, SockType::SeqPacket, None, SockFlag::empty()).unwrap();
    let ours = UnixStream::from(ours);
    let outbox = Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap();
    (Daemon { conn, uid, pid, outbox: Arc::new(outbox) }, UnixStream::from(theirs))
}

fn registered(daemons: &Registry, uid: u32) -> Option<u64> {
    daemons.top(uid).map(|d| d.conn)
}

/// The interleaving the VM suite hit. Two connections from
/// one uid: connection 1, accepted first, is a throwaway that connects
/// and drops; connection 2, accepted second, is the live daemon. Their
/// threads run in the opposite order, so the live daemon registers first
/// and the throwaway registers *last* — and then goes away.
///
/// Before the fix the late insert replaced the live daemon, the
/// throwaway's cleanup then found itself registered and removed it, and
/// the uid was left with no daemon while its daemon's socket was still
/// open: every placeholder open waited `DAEMON_WAIT` and was denied.
#[test]
fn an_older_connection_that_registers_late_does_not_evict_a_live_daemon() {
    let mut daemons = Registry::default();
    let (live, _live_peer) = daemon(1000, 2);
    let (throwaway, throwaway_peer) = daemon(1000, 1);

    assert!(daemons.register(live), "the first registration always lands");
    let late = daemons.register(throwaway);

    // The throwaway goes away; its `Disconnect` guard runs.
    drop(throwaway_peer);
    daemons.deregister(1000, 1);
    assert_eq!(
        registered(&daemons, 1000),
        Some(2),
        "the live daemon must still be the one registered after the throwaway's cleanup"
    );
    assert!(
        !late,
        "an older connection must not replace a newer one, whatever order they arrive in"
    );
}

/// The other half, which must keep working: a newer connection from the
/// same uid — a daemon that reconnected — does replace the older one, and
/// the older one's cleanup then leaves it alone.
#[test]
fn a_newer_connection_replaces_an_older_one_and_survives_its_cleanup() {
    let mut daemons = Registry::default();
    let (old, _old_peer) = daemon(1000, 1);
    let (new, _new_peer) = daemon(1000, 2);

    assert!(daemons.register(old));
    assert!(daemons.register(new), "a reconnecting daemon must take over");
    daemons.deregister(1000, 1);
    assert_eq!(registered(&daemons, 1000), Some(2));

    daemons.deregister(1000, 2);
    assert_eq!(registered(&daemons, 1000), None, "and its own cleanup removes it");
}

/// The sequential case H120 left open. A process of the
/// daemon's own uid connects after it — newest wins, so it takes over —
/// and then goes away. The live daemon underneath must get the uid back:
/// its socket is still open, so nothing will ever make it reconnect, and
/// a uid left with no registration has every open wait `DAEMON_WAIT` and
/// then be denied `EIO`, until the daemon restarts.
#[test]
fn a_newer_connection_that_goes_away_hands_the_uid_back_to_the_live_one() {
    let mut daemons = Registry::default();
    let (live, _live_peer) = daemon(1000, 1);
    let (transient, _transient_peer) = daemon(1000, 2);

    daemons.register(live);
    daemons.register(transient);
    assert_eq!(registered(&daemons, 1000), Some(2), "the newer connection takes over");

    daemons.deregister(1000, 2);
    assert_eq!(
        registered(&daemons, 1000),
        Some(1),
        "when the newer connection goes, the live daemon underneath must be the one \
         hydrations go to again"
    );
}

/// The exemption follows the top. One pid per
/// uid is exempt at any moment — the one the uid's hydrations go to —
/// and it passes back down when the connection above it goes. A live
/// connection underneath is not exempt while it is not the top, and no
/// connection is ever exempt for another uid.
#[test]
fn only_the_top_connections_pid_is_exempt() {
    let mut daemons = Registry::default();
    let (live, _live_peer) = daemon_of_pid(1000, 1, 10);
    let (transient, _transient_peer) = daemon_of_pid(1000, 2, 20);
    daemons.register(live);
    assert!(daemons.is_top_pid(1000, 10), "a lone daemon is exempt for its uid");

    daemons.register(transient);
    assert!(daemons.is_top_pid(1000, 20), "the newer connection is on top");
    assert!(!daemons.is_top_pid(1000, 10), "and the one underneath is no longer exempt");
    assert!(!daemons.is_top_pid(1001, 20), "nor is anybody exempt for another uid");

    daemons.deregister(1000, 2);
    assert!(daemons.is_top_pid(1000, 10), "the exemption goes back down with the top");
    assert!(!daemons.is_top_pid(1000, 20), "and leaves with the connection that left");
}

/// Removing a connection from the middle of a stack keeps the order of
/// the rest, and a uid whose last connection goes holds no entry at all.
#[test]
fn a_connection_leaves_the_stack_from_wherever_it_sits() {
    let mut daemons = Registry::default();
    let mut peers = Vec::new();
    for conn in 1..=3 {
        let (d, peer) = daemon(1000, conn);
        peers.push(peer);
        assert!(daemons.register(d), "each newer connection lands on top");
    }
    daemons.deregister(1000, 2);
    assert_eq!(registered(&daemons, 1000), Some(3), "the top is untouched");
    daemons.deregister(1000, 3);
    assert_eq!(registered(&daemons, 1000), Some(1), "and the oldest is under it");
    daemons.deregister(1000, 1);
    assert_eq!(registered(&daemons, 1000), None);
    assert!(daemons.by_uid.is_empty(), "no empty stack is kept");
    daemons.deregister(1000, 1);
    assert!(daemons.by_uid.is_empty(), "and removing it twice is harmless");
}

/// Ordering is per uid: another uid's newer connection is not a reason to
/// refuse this one.
#[test]
fn connection_order_is_compared_only_within_one_uid() {
    let mut daemons = Registry::default();
    let (other, _other_peer) = daemon(1001, 5);
    let (ours, _our_peer) = daemon(1000, 3);
    assert!(daemons.register(other));
    assert!(daemons.register(ours));
    assert_eq!(registered(&daemons, 1000), Some(3));
    assert_eq!(registered(&daemons, 1001), Some(5));
}

/// The helper's stop ends a wait for a daemon at once, and no wait begins
/// after it: a worker parked here holds an open that must be answered
/// before the process ends, and `DAEMON_WAIT` is longer than a stop may take.
#[test]
fn a_stop_ends_the_wait_for_a_daemon() {
    let daemons = Arc::new(Daemons::new());
    let waiting = Arc::clone(&daemons);
    let started = Instant::now();
    let waiter = std::thread::spawn(move || waiting.wait_for(1000, || true).map(|d| d.conn));
    // Long enough for the waiter to be parked; the stop is seen either way.
    std::thread::sleep(Duration::from_millis(50));
    daemons.stop();

    assert_eq!(waiter.join().unwrap(), Err(NoDaemon::Stopping));
    assert!(started.elapsed() < DAEMON_WAIT / 2, "it did not wait for a daemon");
    assert_eq!(
        daemons.wait_for(1000, || true).map(|d| d.conn),
        Err(NoDaemon::Stopping),
        "and a later open is not made to wait either"
    );
}

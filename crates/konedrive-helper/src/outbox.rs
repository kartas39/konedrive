//! Everything the helper sends to one daemon, and the thread that sends it.
//!
//! # Why this is not just a mutex around the socket
//!
//! It used to be. `hydrate` took a per-connection `Mutex<Channel>` and called
//! `Channel::send` inside it — and `send` is a **blocking** `sendmsg` on a
//! blocking `SOCK_SEQPACKET` socket. Measured on the host: against a peer
//! that never reads, `send` accepts **278** datagrams and then blocks
//! forever. One worker thread wedged in there holds the mutex; every other
//! worker handling an open for that uid then blocks acquiring it; the
//! connection's own `Ack` blocks too. With a bounded worker pool that ends
//! with all 64 workers consumed, and from that moment **every intercepted
//! open on the machine is denied `EAGAIN`** for as long as the peer stays
//! quiet.
//!
//! Any local user could do it: connect, register a directory they own, never
//! read the socket, open a few hundred of their own placeholders. A
//! privileged process must not be stallable by an unprivileged one, and a
//! lock held across an unbounded blocking call on a descriptor the *peer*
//! controls is exactly that.
//!
//! So sending is moved off the worker threads entirely, mirroring the split
//! client already has:
//!
//! - callers hand a message to a **bounded queue** and return immediately —
//!   [`Outbox::try_send`], never a blocking send, so no worker ever waits;
//! - the queue has two compartments: room for the requests the
//!   helper starts, which the per-connection credit bounds
//!   ([`REQUEST_CAPACITY`]), and room reserved for the `Ack`s that answer the
//!   daemon's own calls ([`ACK_RESERVE`]), so that neither kind can ever take
//!   the other's place;
//! - one writer thread per connection owns the `Channel` and does the
//!   blocking work, where blocking costs one thread that belongs to that
//!   connection rather than a share of a global resource;
//! - and even that thread is bounded: a peer that stops reading *and* stops
//!   talking for [`LIVENESS_WINDOW`] ends the connection instead of holding a
//!   thread — and every opener enrolled on it — for the lifetime of the
//! process (see [`deliver`]).

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use konedrive_proto::{Channel, ToDaemon, MAX_OUTSTANDING_HYDRATIONS};

/// How many messages the helper *starts* — `HydrateRequest`s, and the one
/// `Welcome` — may wait for one daemon at once.
///
/// Exactly the credit plus the greeting. A connection never has more than
/// [`MAX_OUTSTANDING_HYDRATIONS`] requests handed out and unanswered, and a
/// request waiting here is one of those, so a request that finds this full is
/// not backpressure from a slow daemon: either the connection is over, or the
/// peer answered a request it had not been sent yet (request ids are small
/// integers, and only a hostile peer guesses one), which returns a credit
/// early. Either way the refusal lands on that peer's own openers.
///
/// It used to be one queue of 256 shared with `Ack`s, and that sharing is
/// what removes: a burst of requests could fill it, and then the
/// `Ack` for the daemon's next call did not fit and the connection was ended
/// for it.
pub const REQUEST_CAPACITY: usize = MAX_OUTSTANDING_HYDRATIONS + 1;

/// How many `Ack`s may wait for one daemon before the connection's reader
/// thread waits for room.
///
/// An `Ack` answers one of the daemon's own calls, and the daemon awaits each
/// call before it counts as done, so the `Ack`s waiting here are at most its
/// calls in flight: four hydration reports at once (four fill
/// slots), plus whatever registration, marking and dehydration calls its sync
/// service has running, each of which awaits its calls one at a time. Nothing
/// in the daemon caps that second number with a constant, so this is not a
/// bound the daemon promises; it is sized an order of magnitude above
/// anything it does.
///
/// And reaching it costs nothing but time: [`Outbox::send_ack`] **waits** for
/// room instead of failing, so the reader stops taking new requests from that
/// peer until its replies drain — backpressure onto the peer, on the one
/// thread that belongs to its connection. An `Ack` is never refused and never
/// costs a connection, which is the defect this replaces: `serve_one` used to
/// end any connection whose `Ack` did not fit, i.e. end a daemon for being
/// slow to read at the moment it had just proved it was alive.
pub const ACK_RESERVE: usize = 128;

/// How long one blocked `sendmsg` lasts before the writer thread wakes up to
/// ask whether the daemon is still alive (`SO_SNDTIMEO`).
///
/// **Not** a limit on how long a daemon may take to read, and no longer a
/// reason to end a connection by itself. It used to be both,
/// and its doc comment said no healthy daemon could trip it; burst
/// tripped it with an ordinary burst of opens, and 662 enrolled openers were
/// denied `EIO` for it. What decides whether the connection ends is
/// [`LIVENESS_WINDOW`]; this is only how often that is asked.
///
/// (That burst was not a slow daemon but a wedged pipeline — see
/// `konedrive_proto::MAX_OUTSTANDING_HYDRATIONS`, which is what now keeps a
/// healthy daemon's socket from filling at all. A blocked send is left for
/// what remains: a daemon whose reader is merely slow to be scheduled.)
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a daemon may go **without sending the helper anything at all**,
/// while a send to it is blocked, before the connection is ended.
///
/// A blocked send is not evidence of a wedged daemon; silence is. A daemon
/// that is working through a burst still reports each fill as it finishes
/// (`HydrateDone`), so it keeps its connection however long the burst
/// takes. One that has stopped reading *and* stopped talking is wedged or
/// hostile, and ending its connection is what makes its enrolled openers
/// answered (`EIO`, by the disconnect guard) rather than suspended forever —
/// protection, which this keeps.
///
/// **Provisional**, like the pool's numbers: chosen, not measured. Since
/// `MAX_OUTSTANDING_HYDRATIONS` a healthy daemon's reader never stops, so
/// its socket does not fill and this window is not reached however slow its
/// fills are (in the VM suite's 3000-open burst the outbox never once filled
/// and no connection was lost, on any of the three filesystems). What it still
/// catches is a peer whose reader has stopped for another reason and that
/// has said nothing for a whole minute.
pub const LIVENESS_WINDOW: Duration = Duration::from_secs(60);

/// The two times above, as one value, so that the tests can run the same
/// code with windows measured in milliseconds.
#[derive(Debug, Clone, Copy)]
struct Timing {
    send_timeout: Duration,
    liveness_window: Duration,
}

const TIMING: Timing = Timing { send_timeout: SEND_TIMEOUT, liveness_window: LIVENESS_WINDOW };

/// When the daemon last sent the helper anything, shared between the
/// connection's reader thread (which records it) and its writer thread
/// (which asks). An offset from a fixed `Instant` so it can live in an
/// atomic: neither thread ever waits for the other to read or write it.
struct Liveness {
    origin: Instant,
    /// Nanoseconds after `origin`.
    last_heard: AtomicU64,
}

impl Liveness {
    /// A connection that has just been accepted counts as heard from.
    fn new() -> Self {
        Self { origin: Instant::now(), last_heard: AtomicU64::new(0) }
    }

    fn heard(&self) {
        let now = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.last_heard.fetch_max(now, Ordering::AcqRel);
    }

    fn last_heard(&self) -> Instant {
        self.origin + Duration::from_nanos(self.last_heard.load(Ordering::Acquire))
    }
}

/// One message on its way to a daemon, with the descriptor it carries.
///
/// The descriptor is owned rather than borrowed because the message outlives
/// the call that queued it: by the time the writer thread sends it, `hydrate`
/// has long returned.
pub struct Outgoing {
    pub message: ToDaemon,
    pub fd: Option<OwnedFd>,
}

/// What is waiting for the writer thread, and the two compartments' counts.
struct Queue {
    items: VecDeque<Outgoing>,
    /// `Ack`s in `items`, bounded by [`ACK_RESERVE`].
    acks: usize,
    /// Everything else in `items`, bounded by [`REQUEST_CAPACITY`].
    requests: usize,
    /// Set by [`Outbox::close`], by the last handle going away, and by the
    /// writer thread when it stops. From then on nothing is accepted and
    /// whatever is still queued is dropped: the connection is over.
    closed: bool,
}

/// The queue, shared between the handles that fill it and the writer thread
/// that empties it.
struct Pending {
    queue: Mutex<Queue>,
    /// Signalled when something is queued, or the queue is closed.
    queued: Condvar,
    /// Signalled when an `Ack` leaves the queue, or the queue is closed: the
    /// only thing ever waited for on this side is room for an `Ack`.
    room: Condvar,
}

impl Pending {
    /// The queue, whatever a panicking holder left it as: every change to it
    /// is one push or one pop with its count, so there is nothing
    /// half-applied to find.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stops accepting, drops whatever is still queued, and wakes everyone
    /// waiting on either side.
    fn close(&self) {
        {
            let mut queue = self.lock();
            queue.closed = true;
            queue.items.clear();
            queue.acks = 0;
            queue.requests = 0;
        }
        self.queued.notify_all();
        self.room.notify_all();
    }

    /// The next message for the writer thread, waiting for one if need be.
    /// `None` once the queue is closed.
    fn next(&self) -> Option<Outgoing> {
        let mut queue = self.lock();
        loop {
            if queue.closed {
                return None;
            }
            if let Some(outgoing) = queue.items.pop_front() {
                if matches!(outgoing.message, ToDaemon::Ack { .. }) {
                    queue.acks -= 1;
                    drop(queue);
                    self.room.notify_all();
                } else {
                    queue.requests -= 1;
                }
                return Some(outgoing);
            }
            queue = self.queued.wait(queue).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// The connection is over; nothing more can be sent on it.
#[derive(Debug)]
pub struct Closed;

/// The send half of one connection.
pub struct Outbox {
    pending: Arc<Pending>,
    /// A duplicate of the connection's socket, kept only to `shutdown()` it.
    /// The reader thread blocks in `recvmsg` on the same underlying open file
    /// description, so shutting this down is what unblocks it when the writer
    /// decides the connection is over.
    socket: UnixStream,
    liveness: Arc<Liveness>,
}

impl Outbox {
    /// Builds the outbox and starts its writer thread.
    ///
    /// `socket` becomes the writer thread's `Channel`; `shutdown` is a
    /// duplicate of the same socket used only to tear the connection down.
    pub fn start(socket: UnixStream, shutdown: UnixStream, name: &str) -> io::Result<Self> {
        Self::start_with(socket, shutdown, name, TIMING)
    }

    fn start_with(
        socket: UnixStream,
        shutdown: UnixStream,
        name: &str,
        timing: Timing,
    ) -> io::Result<Self> {
        set_send_timeout(&socket, timing.send_timeout)?;
        let mut channel = Channel::new(socket)?;
        let pending = Arc::new(Pending {
            queue: Mutex::new(Queue {
                items: VecDeque::new(),
                acks: 0,
                requests: 0,
                closed: false,
            }),
            queued: Condvar::new(),
            room: Condvar::new(),
        });
        let teardown = shutdown.try_clone()?;
        let liveness = Arc::new(Liveness::new());
        let heard = Arc::clone(&liveness);
        let draining = Arc::clone(&pending);
        std::thread::Builder::new().name(format!("konedrive-tx-{name}")).spawn(move || {
            // Ends when the queue is closed (the connection is over, or every
            // handle to it is gone), when a send fails outright, or when the
            // daemon has been silent for the whole liveness window with a
            // send blocked.
            while let Some(outgoing) = draining.next() {
                if let Err(why) = deliver(&mut channel, &outgoing, &heard, timing) {
                    tracing::warn!("cannot write to a daemon ({why}); ending the connection");
                    break;
                }
            }
            // Whatever ended the loop, the connection is finished: refuse
            // everything from now on — which is also what releases a reader
            // thread waiting for room for an `Ack` — and unblock the reader
            // thread's `recv` so it can run the disconnect cleanup.
            draining.close();
            let _ = teardown.shutdown(std::net::Shutdown::Both);
        })?;
        Ok(Self { pending, socket: shutdown, liveness })
    }

    /// Records that the daemon has just sent the helper something. The
    /// connection's reader thread calls this for every message it receives,
    /// whatever the message is: all that matters to [`LIVENESS_WINDOW`] is
    /// that the peer is still there and still doing things.
    pub fn heard_from_peer(&self) {
        self.liveness.heard();
    }

    /// Queues one message the helper starts — a `HydrateRequest`, or the
    /// `Welcome` — or hands it straight back when there is no room for it.
    ///
    /// Never blocks. `Err` means the connection is over (see
    /// [`is_closed`](Self::is_closed)) or its [`REQUEST_CAPACITY`] is taken,
    /// and the caller must answer whatever it was about to ask for — an event
    /// fd dropped without a response leaves its opener suspended until the
    /// helper exits.
    ///
    /// Not for `Ack`s, which have room of their own: see
    /// [`send_ack`](Self::send_ack).
    pub fn try_send(&self, outgoing: Outgoing) -> Result<(), Outgoing> {
        debug_assert!(
            !matches!(outgoing.message, ToDaemon::Ack { .. }),
            "an Ack goes through send_ack, into the room reserved for it"
        );
        {
            let mut queue = self.pending.lock();
            if queue.closed || queue.requests >= REQUEST_CAPACITY {
                return Err(outgoing);
            }
            queue.requests += 1;
            queue.items.push_back(outgoing);
        }
        self.pending.queued.notify_one();
        Ok(())
    }

    /// Queues the `Ack` for one of the daemon's calls, into the room reserved
    /// for `Ack`s. Nothing the helper starts can take that room,
    /// so for any daemon that awaits its calls this returns at once.
    ///
    /// A peer with more than [`ACK_RESERVE`] replies unread is made to
    /// **wait**: this blocks until the writer has sent one, so the caller —
    /// the connection's own reader thread, never a worker — reads nothing
    /// more from that peer until it catches up. It returns `Err` only when
    /// the connection is over, which is also what releases a caller waiting
    /// here: the writer thread closes the queue when it stops, including when
    /// [`LIVENESS_WINDOW`] ends a peer that neither reads nor talks.
    pub fn send_ack(&self, errno: i32) -> Result<(), Closed> {
        self.send_ack_with(errno, None)
    }

    /// [`send_ack`](Self::send_ack), with a descriptor attached: the answer to
    /// an `OpenByHandle`.
    pub fn send_ack_with(&self, errno: i32, fd: Option<OwnedFd>) -> Result<(), Closed> {
        let mut queue = self.pending.lock();
        loop {
            if queue.closed {
                return Err(Closed);
            }
            if queue.acks < ACK_RESERVE {
                break;
            }
            queue =
                self.pending.room.wait(queue).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        queue.acks += 1;
        queue.items.push_back(Outgoing { message: ToDaemon::Ack { errno }, fd });
        drop(queue);
        self.pending.queued.notify_one();
        Ok(())
    }

    /// Whether the connection is over. A refusal from [`try_send`](Self::try_send)
    /// on an open outbox means its request capacity was taken instead.
    pub fn is_closed(&self) -> bool {
        self.pending.lock().closed
    }

    /// Stops accepting new messages and tears the socket down, which unblocks
    /// both the reader thread and the writer thread.
    pub fn close(&self) {
        self.pending.close();
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Outbox {
    /// The last handle going away ends the writer thread, as the last sender
    /// of a channel used to. Nothing can queue anything after this anyway.
    fn drop(&mut self) {
        self.pending.close();
    }
}

/// Sends one message, however long the daemon takes to make room for it —
/// unless it stops talking for the whole liveness window.
///
/// A `SOCK_SEQPACKET` send either queues the whole datagram, descriptor
/// included, or nothing at all, so trying the same message again after
/// `SO_SNDTIMEO` expires cannot duplicate or tear it.
///
/// Silence is measured from whichever is later: the daemon's last message,
/// or the moment this send began. The second half matters. A daemon that
/// has been idle for an hour has sent nothing for an hour, and a burst of
/// opens then fills its socket as a matter of course; measured from its last
/// message alone, the first `SO_SNDTIMEO` of that burst would already count
/// an hour of silence and end a perfectly healthy connection — the very
/// defect this replaces. What the window asks is narrower and is the one
/// question that tells wedged from busy: *since this send got stuck, has the
/// daemon said anything at all?*
fn deliver(
    channel: &mut Channel,
    outgoing: &Outgoing,
    liveness: &Liveness,
    timing: Timing,
) -> Result<(), String> {
    let fd = outgoing.fd.as_ref().map(AsFd::as_fd);
    let blocked_since = Instant::now();
    loop {
        match channel.send(&outgoing.message, fd) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let quiet_since = liveness.last_heard().max(blocked_since);
                let silent_for = quiet_since.elapsed();
                if silent_for < timing.liveness_window {
                    // Busy, not wedged: it spoke within the window, or the
                    // send has not been blocked for a whole window yet.
                    continue;
                }
                return Err(format!(
                    "a send has been blocked for {:?} and the daemon has said nothing for \
                     {silent_for:?}, longer than the {:?} liveness window; it is wedged or not \
                     reading",
                    blocked_since.elapsed(),
                    timing.liveness_window
                ));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn set_send_timeout(socket: &UnixStream, timeout: Duration) -> io::Result<()> {
    let tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: `socket` is an open socket, and `tv` is a live, correctly sized
    // `timeval` for `SO_SNDTIMEO`.
    let rc = unsafe {
        libc::setsockopt(
            std::os::fd::AsRawFd::as_raw_fd(socket),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            std::ptr::addr_of!(tv).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
                if outbox.send_ack(sent as i32).is_err() {
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
        let acked = within(Duration::from_secs(2), move || acking.send_ack(0).is_ok());
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
}

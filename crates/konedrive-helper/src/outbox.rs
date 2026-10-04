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

/// Finishes a connection when its writer thread stops, however it stops:
/// refuses everything from then on — which is also what releases a reader
/// thread waiting for room for an `Ack` — and unblocks the reader thread's
/// `recv` so it can run the disconnect cleanup.
///
/// A `Drop` guard and not statements after the loop, so that a panic in the
/// thread ends the connection too. Without it the reader would go on taking
/// requests whose `Ack`s nobody sends, and every hydration of the uid would
/// be queued for a daemon that is never asked.
struct WriterEnd {
    pending: Arc<Pending>,
    /// A duplicate of the connection's socket, kept only to `shutdown()` it.
    socket: UnixStream,
}

impl Drop for WriterEnd {
    fn drop(&mut self) {
        self.pending.close();
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
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
        let liveness = Arc::new(Liveness::new());
        let heard = Arc::clone(&liveness);
        let end = WriterEnd { pending: Arc::clone(&pending), socket: shutdown.try_clone()? };
        std::thread::Builder::new().name(format!("konedrive-tx-{name}")).spawn(move || {
            // Ends when the queue is closed (the connection is over, or every
            // handle to it is gone), when a send fails outright, or when the
            // daemon has been silent for the whole liveness window with a
            // send blocked. Whatever ends it, `end` finishes the connection
            // as it is dropped.
            while let Some(outgoing) = end.pending.next() {
                if let Err(why) = deliver(&mut channel, &outgoing, &heard, timing) {
                    tracing::warn!("cannot write to a daemon ({why}); ending the connection");
                    break;
                }
            }
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
mod tests;

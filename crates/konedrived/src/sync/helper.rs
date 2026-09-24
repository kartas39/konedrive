//! The daemon's end of the helper socket. A blocking thread owns the socket
//! (descriptor passing is synchronous); tokio talks to it through channels.
//!
//! Requests and acknowledgements are paired by order, not by any request id
//! carried in the wire messages themselves: the helper answers every
//! `ToHelper` message with exactly one `ToDaemon::Ack` (see
//! `konedrive-helper/src/main.rs::serve_one`, which loops `recv` then
//! `apply` then `send(Ack)` with nothing else interleaved on that
//! connection), so a plain FIFO queue of the callers waiting on a reply is
//! enough: the writer thread pushes a caller's `oneshot::Sender` onto the
//! back of the queue in the same order it writes that caller's message, and
//! the reader thread pops the front of the queue for every `Ack` it reads.
//! `HydrateRequest` messages are not replies to anything and never touch
//! this queue; they are forwarded straight to the tokio `mpsc` channel
//! returned by `connect`.
//!
//! Three things ride on top of that FIFO queue rather than being special
//! cases of it:
//!
//! - **The handshake, delivered off the caller's thread.** The
//!   reader and writer threads start immediately after the socket connects,
//!   before any handshake happens at all. The helper's unprompted `Welcome`
//!   greeting is read by the reader thread — exactly like every other
//!   message — and handed to `connect` through a `oneshot`, rather than
//!   being read inline with a raw blocking `recvmsg` on whatever thread
//!   called `connect`. That used to be the one unbounded blocking call left
//!   in this file: a `connect()` awaited on a current-thread tokio runtime
//!   against a helper that accepted and then said nothing starved every
//!   other task on that runtime, and even a `spawn_blocking` caller would
//!   have parked a blocking-pool thread forever, one per attempt, against a
//!   truly wedged helper. Moving the blocking recv onto the reader thread —
//!   whose entire job is to block, and which `shutdown()` already knows how
//!   to unblock — removes it without needing `SO_RCVTIMEO` or any other
//!   socket-level timeout.
//! - **`Hello`.** The helper greets unprompted with `Welcome`
//!   before reading anything, so the client's `Hello` cannot be sent to
//!   elicit it — sent first it would just be read as the connection's first
//!   ordinary request. So `Hello` is queued through this same
//!   `calls`/`pending` machinery as the very first `Call`, once the reader
//!   and writer threads exist to carry and pair it. It is never
//!   fire-and-forget: a non-zero errno on its `Ack` means the helper
//!   rejected our protocol version, and `connect` fails the connection
//!   rather than proceeding.
//! - **Call timeouts.** A helper that is connected but stuck —
//!   not crashed, not closed — must not hang a caller forever. Every call
//!   is bounded (30 s; 120 s for `register_root` and `unregister_root`, which
//!   perform a full tree walk inside the call; the `Welcome`/`Hello` handshake shares the
//!   30 s bound too, and its 30 s cap is a documented, load-bearing part of
//!   `connect`'s contract on `HelperLink::connect`).
//!   Because pairing is strict FIFO, a timed-out call cannot simply be
//!   dropped from `pending`: the next `Ack` would then pair with the wrong
//!   caller. So a timeout tears the whole connection down instead, which
//!   lets the reader thread's existing disconnect drain fail every other
//!   outstanding call with `NotRunning`; only the call that actually timed
//!   out gets the distinct `HelperError::Timeout`.
//!
//! Tearing the connection down (above) is tied to `Drop`, not to any one
//! code path: `connect_with_timeout` is `async`,
//! which means it can be cancelled mid-handshake — an outer
//! `tokio::time::timeout` shorter than its internal 30 s bound, a
//! `select!`, anything that drops the future instead of letting it run to
//! completion. A plain `UnixStream`'s own `Drop` just closes one of the
//! three duplicated file descriptors, which does not shut down the socket
//! the other two still share, so a cancelled `connect` used to leave the
//! reader thread blocked in `recv()` forever — one leaked thread per
//! cancelled attempt. `ShutdownOnDrop` below wraps the connection-control
//! duplicate so its destructor calls `shutdown(Shutdown::Both)`
//! unconditionally; Rust runs a future's locals' destructors whether it
//! completes or is dropped early, so this closes every cancellation point
//! at once, present and future, without needing `SO_RCVTIMEO` or any other
//! socket-level timeout.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc as blocking_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error("the konedrive helper is not running")]
    NotRunning,
    #[error("the helper refused the request (errno {0})")]
    Refused(i32),
    #[error("the helper did not respond within its call timeout")]
    Timeout,
    #[error("{0}")]
    Io(String),
}

/// One suspended open, waiting for its file to be filled.
pub struct HydrateRequest {
    pub req_id: u64,
    pub fd: OwnedFd,
}

/// One outgoing request, plus where to deliver the eventual answer.
struct Call {
    message: ToHelper,
    fd: Option<OwnedFd>,
    reply: oneshot::Sender<Result<(), HelperError>>,
}

/// Callers waiting on the next `Ack`, oldest first. Shared between the
/// writer thread (which pushes, in send order) and the reader thread (which
/// pops, in arrival order, and drains the rest on disconnect).
type PendingReplies = Arc<Mutex<VecDeque<oneshot::Sender<Result<(), HelperError>>>>>;

/// Bound on every ordinary call, and on the `Welcome`/`Hello`
/// handshake: a helper that is connected but silent this long
/// is broken.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// `register_root` and `unregister_root` perform a full walk of the whole tree
/// inside the call — every directory, and a lookup of every file — so they
/// get a longer bound.
const REGISTER_ROOT_TIMEOUT: Duration = Duration::from_secs(120);

/// A duplicate of the connection's socket that shuts the whole connection
/// down — `shutdown(Shutdown::Both)` — whenever it is dropped, not only when
/// some code path remembers to call `shutdown()` explicitly.
///
/// Every duplicate of the underlying socket shares one open file
/// description, so shutting down any one of them (this one, or the reader's
/// or writer's own) tears the connection down for all of them and unblocks
/// whichever thread is sitting in a blocking call at the time. Tying that to
/// `Drop` is what makes it safe under cancellation: `connect_with_timeout`
/// is `async`, so it can be dropped mid-handshake by an outer timeout or
/// `select!` without ever reaching any of its own `return` statements —
/// Rust still runs the destructors of everything it owns, including this,
/// so the connection is torn down and the reader thread is unblocked either
/// way.
struct ShutdownOnDrop(UnixStream);

impl ShutdownOnDrop {
    fn shutdown(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub struct HelperLink {
    calls: blocking_mpsc::Sender<Call>,
    /// A duplicate of the connection's socket, kept to `shutdown()` it
    /// explicitly (timed-out call) and, via `ShutdownOnDrop`,
    /// whenever the last handle to it goes away for any other reason
    ///. The reader and writer threads hold their own
    /// duplicates of the same underlying open file description, so shutting
    /// this one down unblocks them too.
    socket: Arc<ShutdownOnDrop>,
    /// Becomes `true` when the reader thread stops — the connection is over,
    /// whoever ended it. What `supervise_helper` waits on,
    /// rather than on `serve_hydrations`, which lets the fills already
    /// running finish before it returns.
    closed: watch::Receiver<bool>,
    call_timeout: Duration,
    register_root_timeout: Duration,
}

impl HelperLink {
    /// Connects to the helper's control socket, exchanges the version
    /// handshake, and starts the blocking threads that own the connection
    /// from then on.
    ///
    /// The socket must be `SOCK_SEQPACKET` — `konedrive_proto::Channel`
    /// relies on the kernel's per-`send()` datagram framing, which
    /// `std::os::unix::net::UnixStream::connect` (a `SOCK_STREAM` socket)
    /// does not provide and `Channel::new` now rejects outright. The
    /// connection is built with `nix` instead, exactly as the helper's own
    /// listener builds its side (`konedrive-helper/src/main.rs::listen`).
    ///
    /// `async`: every blocking wait this function does — for
    /// the helper's `Welcome` and for our own `Hello`'s `Ack` — is bounded
    /// and awaited, never a raw blocking call on the caller's own thread.
    ///
    ///: this can legitimately take up to 30 s (`CALL_TIMEOUT`) to
    /// resolve, against a helper that accepts the connection and then never
    /// speaks — the `Welcome`/`Hello` handshake shares the same bound every
    /// other call on `HelperLink` uses. Bounding this call
    /// further from outside — a shorter `tokio::time::timeout`, a
    /// `select!`, dropping it on some other signal — is safe and expected:
    /// ties the connection's teardown to `Drop` rather than to
    /// any code path inside this function, so cancelling it part-way
    /// through the handshake still shuts the connection down and leaves
    /// nothing running in the background.
    pub async fn connect(socket_path: &Path) -> io::Result<(Self, mpsc::Receiver<HydrateRequest>)> {
        Self::connect_with_timeout(socket_path, CALL_TIMEOUT, REGISTER_ROOT_TIMEOUT).await
    }

    /// As [`connect`](Self::connect), but with the two call-timeout bounds
    /// overridable. Production code always goes through
    /// `connect`, which pins them at their real values; the test suite calls
    /// this directly with a much shorter bound so a "helper is stuck" test
    /// does not have to sleep the real 30 s to prove the timeout fires.
    async fn connect_with_timeout(
        socket_path: &Path,
        call_timeout: Duration,
        register_root_timeout: Duration,
    ) -> io::Result<(Self, mpsc::Receiver<HydrateRequest>)> {
        let stream = connect_seqpacket(socket_path)?;
        let channel = Channel::new(stream)?;

        // The reader and writer threads start immediately, right
        // here — before any handshake, and before this function has read a
        // single byte itself. Every duplicate below shares one open file
        // description with every other, so a `shutdown()` on any of them
        // (from a failed handshake, or later from a timed-out call) tears
        // down the whole connection and unblocks whichever of these threads
        // is sitting in a blocking call at the time.
        let writer_stream = channel.get_ref().try_clone()?;
        let reader_stream = channel.get_ref().try_clone()?;
        // Wrapped in `ShutdownOnDrop`: if this function returns
        // early, or is cancelled from outside while awaiting below, this
        // local's destructor shuts the connection down unconditionally —
        // see the module doc comment and `ShutdownOnDrop` itself.
        let control_stream = ShutdownOnDrop(channel.get_ref().try_clone()?);
        drop(channel);
        let mut writer = Channel::new(writer_stream)?;
        let mut reader = Channel::new(reader_stream)?;

        let (calls_tx, calls_rx) = blocking_mpsc::channel::<Call>();
        // Exactly as deep as the helper may have requests outstanding on this
        // connection (`MAX_OUTSTANDING_HYDRATIONS`), so that every request in
        // flight fits and the reader thread below never stops reading: an
        // `Ack` queued behind requests it cannot take would never be read,
        // and the fill waiting for that `Ack` would never free its slot. The
        // worst case needs one less — the request loop holds one while it
        // waits for a fill slot — and `sync::tests::the_reader_reaches_acks_
        // queued_behind_every_request_the_helper_may_send` fails at 62.
        let (requests_tx, requests_rx) =
            mpsc::channel::<HydrateRequest>(konedrive_proto::MAX_OUTSTANDING_HYDRATIONS);
        let pending: PendingReplies = Arc::new(Mutex::new(VecDeque::new()));
        // The reader thread delivers the helper's opening `Welcome` here,
        // the same way it delivers every `Ack`: through a channel, never by
        // a blocking read on the thread that called `connect`.
        let (welcome_tx, welcome_rx) = oneshot::channel::<Result<(), HelperError>>();
        // And says here that it has stopped, whatever stopped it.
        let (closed_tx, closed_rx) = watch::channel(false);

        // Writes every call in order, pushing its reply onto the back of
        // `pending` immediately before the write so the reader thread can
        // never observe an `Ack` for a call that is not yet queued. Only
        // this thread ever pushes, so on a write failure the entry it just
        // pushed is guaranteed to still be at the back: no `Ack` for it can
        // ever arrive (the message never left), so it is resolved here
        // rather than left for the reader to time out on.
        {
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || {
                while let Ok(call) = calls_rx.recv() {
                    pending.lock().unwrap().push_back(call.reply);
                    let sent = writer.send(&call.message, call.fd.as_ref().map(AsFd::as_fd));
                    if let Err(_e) = sent {
                        if let Some(reply) = pending.lock().unwrap().pop_back() {
                            let _ = reply.send(Err(HelperError::NotRunning));
                        }
                        // The connection is dead: shutting it down forces
                        // the reader thread's blocking `recv` to return
                        // promptly (rather than waiting on a peer that will
                        // never send again), so it can drain whatever else
                        // is still in `pending`.
                        let _ = writer.get_ref().shutdown(std::net::Shutdown::Both);
                        break;
                    }
                }
            });
        }

        // Reads whatever the helper sends: the opening `Welcome` resolves
        // the handshake oneshot, an `Ack` resolves the oldest outstanding
        // call, a `HydrateRequest` is forwarded to the tokio side untouched.
        // On disconnect (or a malformed stream), every call still waiting in
        // `pending` — and the handshake itself, if `Welcome` never arrived —
        // is resolved with `NotRunning` rather than left to hang forever.
        {
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || {
                let mut welcome_tx = Some(welcome_tx);
                loop {
                    match reader.recv::<ToDaemon>() {
                        Ok((ToDaemon::Welcome { version }, _)) => {
                            let Some(tx) = welcome_tx.take() else {
                                // A second `Welcome` after the handshake is
                                // already done is a helper protocol
                                // violation; there is nothing to do with it
                                // but ignore it.
                                continue;
                            };
                            let result = if version == PROTOCOL_VERSION {
                                Ok(())
                            } else {
                                Err(HelperError::Io(format!(
                                    "the helper greeted with protocol version {version}, expected \
                                     {PROTOCOL_VERSION}"
                                )))
                            };
                            let _ = tx.send(result);
                        }
                        Ok((ToDaemon::Ack { errno }, _)) => {
                            let Some(reply) = pending.lock().unwrap().pop_front() else {
                                continue;
                            };
                            let result =
                                if errno == 0 { Ok(()) } else { Err(HelperError::Refused(errno)) };
                            let _ = reply.send(result);
                        }
                        Ok((ToDaemon::HydrateRequest { req_id }, Some(fd))) => {
                            if requests_tx.blocking_send(HydrateRequest { req_id, fd }).is_err() {
                                break;
                            }
                        }
                        Ok((ToDaemon::HydrateRequest { .. }, None)) => {
                            // Malformed: a hydrate request with no attached
                            // descriptor cannot be answered. Drop it; there
                            // is nothing else to do with it.
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                // The connection ended (or was shut down) before `Welcome`
                // ever arrived: whoever is waiting on it must not hang.
                if let Some(tx) = welcome_tx.take() {
                    let _ = tx.send(Err(HelperError::NotRunning));
                }
                let _ = reader.get_ref().shutdown(std::net::Shutdown::Both);
                {
                    let mut queue = pending.lock().unwrap();
                    while let Some(reply) = queue.pop_front() {
                        let _ = reply.send(Err(HelperError::NotRunning));
                    }
                }
                let _ = closed_tx.send(true);
            });
        }

        // Both waits below share `call_timeout`: a helper
        // that accepts a connection and then sends nothing is exactly as
        // broken as one that accepts a call and never answers it. Whatever
        // goes wrong (refused, timed out, or the connection simply closing)
        // leaves the two threads just spawned with nothing left to do, but
        // no explicit cleanup is needed here any more:
        // `control_stream` shuts the connection down when it drops, whether
        // that is because this function returns early below or because an
        // outer `tokio::time::timeout`/`select!` drops this whole future
        // while one of these awaits is still pending.
        if let Err(e) = await_welcome(welcome_rx, call_timeout).await {
            return Err(handshake_error(e));
        }

        // `Hello` is queued through the same `calls`/`pending`
        // machinery as every other request, only now that both threads
        // above exist to carry and pair it — never fire-and-forget before
        // them.
        if let Err(e) = send_hello(&calls_tx, call_timeout).await {
            return Err(handshake_error(e));
        }

        Ok((
            Self {
                calls: calls_tx,
                socket: Arc::new(control_stream),
                closed: closed_rx,
                call_timeout,
                register_root_timeout,
            },
            requests_rx,
        ))
    }

    async fn call(&self, message: ToHelper, fd: Option<OwnedFd>, timeout: Duration) -> Result<(), HelperError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.calls
            .send(Call { message, fd, reply: reply_tx })
            .map_err(|_| HelperError::NotRunning)?;
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(HelperError::NotRunning),
            Err(_elapsed) => {
                // Pairing is strict FIFO, so leaving this entry
                // in `pending` while only this call gives up would pair the
                // next `Ack` with the wrong caller. Tearing the connection
                // down is the only safe recovery: it makes the reader
                // thread's existing disconnect drain fail every other
                // outstanding call with `NotRunning`, while this call gets
                // the distinct `Timeout` so its caller can tell "the helper
                // is stuck" apart from "the helper is gone".
                self.shutdown();
                Err(HelperError::Timeout)
            }
        }
    }

    fn shutdown(&self) {
        self.socket.shutdown();
    }

    /// Returns once the connection is over — the helper went away, a call
    /// timed out, or the connection was shut down from this side.
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        // An error means the reader thread is gone without saying so, which
        // is just as over.
        let _ = closed.wait_for(|closed| *closed).await;
    }

    /// Whether the connection is over (see [`closed`](Self::closed)).
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow() || self.closed.has_changed().is_err()
    }

    pub async fn register_root(&self, dir: &File, root_id: &str) -> Result<(), HelperError> {
        self.call(
            ToHelper::RegisterRoot { root_id: root_id.to_owned() },
            Some(dup(dir)?),
            self.register_root_timeout,
        )
        .await
    }

    /// The same long bound as [`register_root`](Self::register_root): the
    /// helper walks the whole tree inside this call too, unmarking every
    /// directory and clearing every file's ignore mark.
    pub async fn unregister_root(&self, root_id: &str) -> Result<(), HelperError> {
        self.call(
            ToHelper::UnregisterRoot { root_id: root_id.to_owned() },
            None,
            self.register_root_timeout,
        )
        .await
    }

    pub async fn mark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.call(ToHelper::MarkDir, Some(dup(dir)?), self.call_timeout).await
    }

    pub async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.call(ToHelper::UnmarkDir, Some(dup(dir)?), self.call_timeout).await
    }

    pub async fn mark_file(&self, file: &File) -> Result<(), HelperError> {
        self.call(ToHelper::MarkFile, Some(dup(file)?), self.call_timeout).await
    }

    pub async fn clear_ignore(&self, file: &File) -> Result<(), HelperError> {
        self.call(ToHelper::ClearIgnore, Some(dup(file)?), self.call_timeout).await
    }

    pub async fn hydrate_done(&self, req_id: u64, errno: i32) -> Result<(), HelperError> {
        self.call(ToHelper::HydrateDone { req_id, errno }, None, self.call_timeout).await
    }
}

fn dup(file: &File) -> Result<OwnedFd, HelperError> {
    file.as_fd().try_clone_to_owned().map_err(|e| HelperError::Io(e.to_string()))
}

/// Awaits the helper's opening `Welcome`, delivered by the reader thread
///, bounded by `timeout`.
async fn await_welcome(
    welcome_rx: oneshot::Receiver<Result<(), HelperError>>,
    timeout: Duration,
) -> Result<(), HelperError> {
    match tokio::time::timeout(timeout, welcome_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(HelperError::NotRunning),
        Err(_elapsed) => Err(HelperError::Timeout),
    }
}

/// Sends `Hello` as an ordinary queued `Call` and awaits its
/// `Ack`, bounded by `timeout`.
async fn send_hello(calls: &blocking_mpsc::Sender<Call>, timeout: Duration) -> Result<(), HelperError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    calls
        .send(Call { message: ToHelper::Hello { version: PROTOCOL_VERSION }, fd: None, reply: reply_tx })
        .map_err(|_| HelperError::NotRunning)?;
    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(HelperError::NotRunning),
        Err(_elapsed) => Err(HelperError::Timeout),
    }
}

/// Turns a failed handshake (`Welcome` or `Hello`) into a clear connection
/// error.
fn handshake_error(e: HelperError) -> io::Error {
    match e {
        HelperError::Refused(errno) => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the helper rejected our protocol version {PROTOCOL_VERSION} during the handshake \
                 (errno {errno})"
            ),
        ),
        HelperError::Timeout => {
            io::Error::new(io::ErrorKind::TimedOut, "the helper did not complete the handshake in time")
        }
        HelperError::NotRunning => io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the helper connection closed during the handshake",
        ),
        HelperError::Io(msg) => io::Error::other(msg),
    }
}

/// Builds a `SOCK_SEQPACKET` client socket and connects it to `path`. This
/// mirrors the socket type the helper's own listener builds
/// (`konedrive-helper/src/main.rs::listen`); `std::os::unix::net::UnixStream::connect`
/// would instead produce a `SOCK_STREAM` socket, which `Channel::new` rejects.
fn connect_seqpacket(path: &Path) -> io::Result<UnixStream> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
    let addr = UnixAddr::new(path)?;
    connect(fd.as_raw_fd(), &addr)?;
    Ok(UnixStream::from(fd))
}

/// Whether anything holds a socket bound at the helper's path, and so
/// whether a fanotify group of the helper's can exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperPresence {
    /// Nothing is bound there: no file, or a file left behind by a helper
    /// that has exited. The helper creates its group only moments before it
    /// binds and places its first mark only after, and the group — marks and
    /// all — goes when the process does; so no ignore mark of ours exists.
    Absent,
    /// A socket is bound there: a helper is running, or is starting and
    /// about to walk.
    Present,
    /// The question could not be answered; treated as [`Present`].
    Unknown(String),
}

/// Looks at the helper's socket **without connecting to it**.
///
/// A connection, even one closed at once, is a connection the helper
/// registers as this uid's daemon: while it lived, the uid's hydrations
/// would go to it and be failed when it went. So the probe is a `connect`
/// with the wrong socket type — `SOCK_STREAM` against the helper's
/// `SOCK_SEQPACKET`. The kernel looks the path up, finds the socket bound to
/// that inode, and refuses the type with `EPROTOTYPE` before anything is
/// queued; a file with no socket bound to it is `ECONNREFUSED`, and no file
/// is `ENOENT`. Bound sockets are found by inode, not by network namespace,
/// so the helper's `PrivateNetwork=yes` changes nothing. Measured in the VM
/// suite, "every punch without interception clears the ignore mark first, or
/// is refused while a helper runs".
pub fn helper_presence(path: &Path) -> HelperPresence {
    let probe = || -> nix::Result<()> {
        let fd = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )?;
        let addr = UnixAddr::new(path)?;
        connect(fd.as_raw_fd(), &addr)
    };
    match probe() {
        Err(nix::errno::Errno::ENOENT | nix::errno::Errno::ECONNREFUSED) => HelperPresence::Absent,
        Err(nix::errno::Errno::EPROTOTYPE) => HelperPresence::Present,
        // A stream socket bound there accepted, or queued, the probe: not
        // the helper's, but somebody's, and nothing to be sure of.
        Ok(()) | Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINPROGRESS) => {
            HelperPresence::Unknown(format!(
                "{} is a stream socket, not the helper's",
                path.display()
            ))
        }
        Err(e) => HelperPresence::Unknown(format!("{}: {e}", path.display())),
    }
}

/// How a punch makes sure it leaves no ignore mark of ours on the file it
/// empties — **the local rule**, decided at the punch and nowhere
/// else.
///
/// An ignore mark on an emptied file lets every later open through to its
/// zeros, silently, for as long as the mark lives (it survives
/// modification). Whether one can be there used to be argued across the
/// whole system — "a folder without interception cannot carry a stale mark
/// that matters" — and that argument was falsified three times, each time by
/// a race nobody had seen (H132, the 's N2).
/// This does not argue at all. Right before a file is emptied:
///
/// - **a link to the helper exists** → the helper is asked to `ClearIgnore`,
///   and any reported failure stops the punch. The helper grants it on
///   ownership of the file alone, since removing a mark can only cost an
///   extra interception, never zeros;
/// - **no helper has its socket bound** → no fanotify group of ours exists,
///   so no mark of ours does ([`HelperPresence::Absent`]); the punch goes
///   ahead;
/// - **a helper is bound and this daemon has no link to it** → its group may
///   hold a mark nobody here can clear; nothing is emptied, and the caller
///   tries again once the link is up.
///
/// No new race can falsify it: it depends on nothing that happened before
/// the punch.
#[derive(Clone)]
pub enum Clearance {
    Link(HelperLink),
    /// No link: the helper's socket path, looked at when the punch is due.
    NoLink(PathBuf),
}

/// Why a [`Clearance`] did not clear the way for a punch.
#[derive(Debug, thiserror::Error)]
pub enum NotCleared {
    #[error("the helper did not clear the ignore mark: {0}")]
    Helper(HelperError),
    #[error(
        "a konedrive helper is running and this daemon is not connected to it yet, so the \
         file's ignore mark cannot be cleared"
    )]
    Unlinked,
    #[error("cannot tell whether a konedrive helper is running: {0}")]
    Unknown(String),
}

impl Clearance {
    /// Clears the way for emptying `file`, by the rule on [`Clearance`].
    /// `Ok` is the only answer after which a punch may follow.
    pub async fn clear(&self, file: &File) -> Result<(), NotCleared> {
        match self {
            Clearance::Link(link) => link.clear_ignore(file).await.map_err(NotCleared::Helper),
            Clearance::NoLink(socket) => {
                let socket = socket.clone();
                let presence = tokio::task::spawn_blocking(move || helper_presence(&socket))
                    .await
                    .unwrap_or_else(|e| HelperPresence::Unknown(e.to_string()));
                match presence {
                    HelperPresence::Absent => Ok(()),
                    HelperPresence::Present => Err(NotCleared::Unlinked),
                    HelperPresence::Unknown(why) => Err(NotCleared::Unknown(why)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicU32, Ordering};

    use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
    use nix::sys::socket::{
        accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType,
        UnixAddr,
    };

    use super::*;

    /// A `SOCK_SEQPACKET` listener bound at `path`, built the same way the
    /// helper builds its own (`konedrive-helper/src/main.rs::listen`).
    /// `std::os::unix::net::UnixListener::bind` produces a `SOCK_STREAM`
    /// listener, which `Channel::new` now rejects — a test harness built on
    /// it would not exercise the same code path production traffic does.
    fn seqpacket_listener(path: &std::path::Path) -> OwnedFd {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        let addr = UnixAddr::new(path).unwrap();
        bind(fd.as_raw_fd(), &addr).unwrap();
        sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
        fd
    }

    fn seqpacket_accept(listener: &OwnedFd) -> UnixStream {
        let fd = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this process now solely owns.
        unsafe { UnixStream::from_raw_fd(fd) }
    }

    /// Reads and acknowledges the daemon's opening `Hello`,
    /// exactly as the real helper's `apply()` does — every fake helper below
    /// must do this before it can see any real request, since `connect()`
    /// now blocks on this exchange before it returns.
    fn ack_hello(channel: &mut Channel) {
        let (hello, _) = channel.recv::<ToHelper>().unwrap();
        assert!(matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION), "{hello:?}");
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
    }

    /// A stand-in helper: accepts one connection, greets, acknowledges the
    /// handshake `Hello`, and answers every request after that with
    /// Ack{0}, recording what it was asked to do.
    ///
    /// The listener is bound here, on the caller's thread, before this
    /// function returns — not inside the spawned thread — so that by the
    /// time the test goes on to call `HelperLink::connect` the socket path
    /// is guaranteed to already exist. Binding it inside the spawned thread
    /// instead would race `connect` against `bind`.
    fn fake_helper(path: std::path::PathBuf) -> std::sync::mpsc::Receiver<String> {
        let listener = seqpacket_listener(&path);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);
            while let Ok((message, fd)) = channel.recv::<ToHelper>() {
                tx.send(format!("{message:?} fd={}", fd.is_some())).unwrap();
                channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            }
        });
        rx
    }

    #[tokio::test]
    async fn sends_a_directory_to_be_marked_with_its_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let seen = fake_helper(socket.clone());
        let (link, _requests) = HelperLink::connect(&socket).await.unwrap();

        let handle = std::fs::File::open(dir.path()).unwrap();
        link.mark_dir(&handle).await.unwrap();

        let line = seen.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(line.starts_with("MarkDir"), "{line}");
        assert!(line.ends_with("fd=true"), "the directory must travel as a descriptor: {line}");
    }

    #[tokio::test]
    async fn a_refusal_from_the_helper_becomes_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);
            let _ = channel.recv::<ToHelper>().unwrap();
            channel.send(&ToDaemon::Ack { errno: libc::EPERM }, None).unwrap();
        });
        let (link, _requests) = HelperLink::connect(&socket).await.unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();
        let error = link.mark_dir(&handle).await.unwrap_err();
        assert!(matches!(error, HelperError::Refused(e) if e == libc::EPERM), "{error:?}");
    }

    #[tokio::test]
    async fn hydrate_requests_arrive_on_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);
            let payload = tempfile::tempfile().unwrap();
            channel
                .send(
                    &ToDaemon::HydrateRequest { req_id: 7 },
                    Some(std::os::fd::AsFd::as_fd(&payload)),
                )
                .unwrap();
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        let (_link, mut requests) = HelperLink::connect(&socket).await.unwrap();
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.req_id, 7);
    }

    /// The requirement leaves to the implementer: a call already
    /// sent and awaiting its `Ack` must not hang forever if the helper goes
    /// away mid-flight. The peer here accepts, greets, acknowledges the
    /// handshake, and then closes without ever acknowledging the `MarkDir`
    /// it receives.
    #[tokio::test]
    async fn a_dropped_connection_fails_every_outstanding_call() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);
            let _ = channel.recv::<ToHelper>().unwrap();
            // No Ack: the connection is simply dropped here.
        });
        let (link, _requests) = HelperLink::connect(&socket).await.unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), link.mark_dir(&handle))
                .await
                .expect("must not hang forever waiting for a reply that will never come");
        assert!(matches!(result, Err(HelperError::NotRunning)), "{result:?}");
    }

    /// Strengthened after a test
    /// showed the original version passed even with `pending`'s pop end
    /// mutated from front to back (a genuine cross-wiring bug): checking
    /// only receive order and that both calls returned `Ok(())` cannot
    /// distinguish "paired correctly" from "paired backwards", because the
    /// fake helper answered every request with the same errno. This version
    /// acks the two calls with *different* errnos (0, then `EACCES`), so a
    /// swapped pairing produces a wrong `Result` at the call site — not just
    /// a wrong receive order at the helper.
    ///
    /// `tokio::join!` polls its futures in argument order on a
    /// single-threaded runtime, and each `call()` sends its message
    /// synchronously before its first `.await` point, so `mark_dir`'s
    /// message reaches the wire before `unmark_dir`'s.
    #[tokio::test]
    async fn two_concurrent_callers_do_not_cross_wire() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);
            for errno in [0, libc::EACCES] {
                let (message, fd) = channel.recv::<ToHelper>().unwrap();
                tx.send(format!("{message:?} fd={}", fd.is_some())).unwrap();
                channel.send(&ToDaemon::Ack { errno }, None).unwrap();
            }
        });
        let (link, _requests) = HelperLink::connect(&socket).await.unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();

        let (mark_result, unmark_result) = tokio::join!(link.mark_dir(&handle), link.unmark_dir(&handle));

        let first = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let second = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(first.starts_with("MarkDir"), "{first}");
        assert!(second.starts_with("UnmarkDir"), "{second}");

        // The pairing proof: each call must get back *its own* answer, not
        // just "some" answer. A pairing bug that pops `pending` from the
        // wrong end swaps these two results between the callers.
        assert!(matches!(mark_result, Ok(())), "{mark_result:?}");
        assert!(
            matches!(unmark_result, Err(HelperError::Refused(e)) if e == libc::EACCES),
            "{unmark_result:?}"
        );
    }

    /// Fix 3, item 2: an unsolicited `HydrateRequest` arriving on the wire
    /// before the `Ack` for an outstanding call must not disturb that call's
    /// pairing — the reader thread tells the two apart by message type, not
    /// by position, so both resolve correctly regardless of what is
    /// interleaved between them.
    #[tokio::test]
    async fn a_hydrate_request_interleaved_before_the_ack_still_resolves_both() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);

            let (message, _) = channel.recv::<ToHelper>().unwrap();
            assert!(matches!(message, ToHelper::MarkDir), "{message:?}");
            let payload = tempfile::tempfile().unwrap();
            channel
                .send(
                    &ToDaemon::HydrateRequest { req_id: 99 },
                    Some(std::os::fd::AsFd::as_fd(&payload)),
                )
                .unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        });
        let (link, mut requests) = HelperLink::connect(&socket).await.unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();

        let (mark_result, request) = tokio::join!(
            link.mark_dir(&handle),
            tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv()),
        );
        mark_result.unwrap();
        let request = request.unwrap().unwrap();
        assert_eq!(request.req_id, 99);
    }

    /// Fix 3, item 3 (and the direct proof of): a helper that
    /// accepts, greets, reads a request, and then goes silent must make the
    /// call fail with `HelperError::Timeout` rather than hang. The call
    /// timeout is injected as a few milliseconds via `connect_with_timeout`
    /// so this test does not have to sleep the real 30 s bound.
    ///: `UnregisterRoot` walks the whole
    /// tree as `RegisterRoot` does — every directory, and now every file's
    /// ignore mark — so it gets the same long bound. With the ordinary one,
    /// a large tree's unregistration timed out, and a timeout ends the
    /// connection and every hydration in flight on it.
    #[tokio::test]
    async fn an_unregistration_gets_the_walks_bound_not_the_ordinary_one() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);
            // A walk that takes longer than an ordinary call may.
            let _ = channel.recv::<ToHelper>().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(600));
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(10));
        });
        let (link, _requests) = HelperLink::connect_with_timeout(
            &socket,
            std::time::Duration::from_millis(200),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        let result = link.unregister_root("some-root").await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn a_stuck_helper_times_out_rather_than_hanging_forever() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            ack_hello(&mut channel);
            // Reads the request and then simply never answers it — connected,
            // not closed, not crashed.
            let _ = channel.recv::<ToHelper>().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(10));
        });
        let (link, _requests) = HelperLink::connect_with_timeout(
            &socket,
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(200),
        )
        .await
        .unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), link.mark_dir(&handle))
                .await
                .expect("must not hang forever waiting on a helper that never answers");
        assert!(matches!(result, Err(HelperError::Timeout)), "{result:?}");
    }

    /// A helper that accepts a connection and then sends
    /// nothing — never even greets — must not hang `connect()` forever
    /// either. Same short-bound injection as the call-timeout test above.
    #[tokio::test]
    async fn connect_times_out_rather_than_hanging_when_the_helper_never_greets() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let _stream = seqpacket_accept(&listener);
            // Accepted, then silence: no `Welcome`, ever.
            std::thread::sleep(std::time::Duration::from_secs(10));
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            HelperLink::connect_with_timeout(
                &socket,
                std::time::Duration::from_millis(200),
                std::time::Duration::from_millis(200),
            ),
        )
        .await
        .expect("must not hang forever waiting on a helper that never greets");
        let error = match result {
            Err(e) => e,
            Ok(_) => panic!("a helper that never greets must fail the connection"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    }

    /// The concrete failure the demonstrated: before
    /// this fix, `connect()` read `Welcome` with an unbounded, raw blocking
    /// `recvmsg` directly on the calling thread. Awaited on a
    /// current-thread runtime against a helper that accepts and then sends
    /// nothing, that starved every other task on the runtime — a concurrent
    /// 50 ms ticker made zero progress. Now the only blocking recv in this
    /// path lives on the dedicated reader thread, so a concurrent task on
    /// the same runtime keeps making progress the whole time `connect()` is
    /// waiting. `#[tokio::test]` defaults to a current-thread runtime, which
    /// is what makes this test meaningful: on a multi-thread runtime the old
    /// bug would not have shown up here at all.
    #[tokio::test]
    async fn connect_does_not_starve_the_runtime_while_waiting_on_a_silent_helper() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        std::thread::spawn(move || {
            let _stream = seqpacket_accept(&listener);
            std::thread::sleep(std::time::Duration::from_secs(10));
        });

        let ticks = Arc::new(AtomicU32::new(0));
        let ticker = {
            let ticks = Arc::clone(&ticks);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    ticks.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        let _ = HelperLink::connect_with_timeout(
            &socket,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_millis(500),
        )
        .await;

        ticker.abort();
        let seen = ticks.load(Ordering::Relaxed);
        assert!(seen >= 5, "the ticker made only {seen} ticks while connect() ran; the runtime was starved");
    }

    /// Made deterministic rather than diagnosed through `/proc`
    /// (which the used only as a diagnostic, not as something to
    /// assert on in the committed suite): cancelling `connect()` from
    /// outside — here, via an outer `tokio::time::timeout` far shorter than
    /// the connection's own 30 s handshake bound — must still shut the
    /// connection down promptly, not leak the reader thread blocked in
    /// `recv()` forever.
    ///
    /// The fake helper never greets, so `connect_with_timeout` is certainly
    /// still awaiting `Welcome` when the outer timeout fires and drops it.
    /// The helper then blocks in its own `recv`; the only thing that can
    /// ever unblock it is our side's connection actually being shut down
    /// (`shutdown(Shutdown::Both)` propagates to the peer, which then reads
    /// EOF). Once it does, the helper reports that back over a plain
    /// channel. Asserting that signal arrives within a short bound after
    /// the outer timeout fires is the proof: nothing but a prompt,
    /// unconditional shutdown on our side — the `ShutdownOnDrop` guard's
    /// destructor running even though `connect_with_timeout` was dropped
    /// mid-`.await` rather than returning normally — could make the
    /// helper's blocked `recv` return at all.
    #[tokio::test]
    async fn cancelling_connect_still_shuts_the_connection_down() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let listener = seqpacket_listener(&socket);
        let (eof_tx, eof_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stream = seqpacket_accept(&listener);
            let mut channel = Channel::new(stream).unwrap();
            // Never greets: `connect_with_timeout` will still be waiting on
            // `Welcome` when the outer timeout below cancels it.
            let outcome = channel.recv::<ToHelper>();
            let _ = eof_tx.send(outcome.is_err());
        });

        let outer = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            HelperLink::connect_with_timeout(
                &socket,
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(30),
            ),
        )
        .await;
        assert!(outer.is_err(), "the outer timeout should have cancelled connect() mid-handshake");

        let saw_eof = eof_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the helper's recv must unblock promptly once connect() is cancelled and dropped");
        assert!(saw_eof, "the helper's recv should fail with EOF, not succeed");
    }

    /// probe: whether a socket is bound at the helper's path,
    /// told apart without ever connecting to it — a connection would be
    /// registered as this uid's daemon while it lived.
    #[test]
    fn the_probe_tells_a_bound_helper_from_none_without_connecting() {
        let dir = tempfile::tempdir().unwrap();

        let nothing = dir.path().join("nothing.sock");
        assert_eq!(helper_presence(&nothing), HelperPresence::Absent, "no file");

        let stale = dir.path().join("stale.sock");
        drop(seqpacket_listener(&stale));
        assert!(stale.exists(), "a helper that exits leaves its socket file behind");
        assert_eq!(helper_presence(&stale), HelperPresence::Absent, "nothing bound to it");

        let not_a_socket = dir.path().join("file.sock");
        std::fs::write(&not_a_socket, b"").unwrap();
        assert_eq!(helper_presence(&not_a_socket), HelperPresence::Absent);

        let live = dir.path().join("live.sock");
        let listener = seqpacket_listener(&live);
        assert_eq!(helper_presence(&live), HelperPresence::Present);
        // Nothing was queued for the listener to accept.
        nix::fcntl::fcntl(&listener, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK))
            .unwrap();
        let pending = accept(listener.as_raw_fd());
        assert_eq!(pending, Err(nix::errno::Errno::EAGAIN), "the probe connected");

        // Bound and not yet listening — a helper between its bind and its
        // listen — is there too.
        let bound = dir.path().join("bound.sock");
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        bind(fd.as_raw_fd(), &UnixAddr::new(&bound).unwrap()).unwrap();
        assert_eq!(helper_presence(&bound), HelperPresence::Present);

        // Something else's stream socket is nothing to be sure of.
        let stream = dir.path().join("stream.sock");
        let _stream = std::os::unix::net::UnixListener::bind(&stream).unwrap();
        assert!(matches!(helper_presence(&stream), HelperPresence::Unknown(_)));
    }
}

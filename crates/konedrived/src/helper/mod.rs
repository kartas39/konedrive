//! The daemon's end of the helper socket. A blocking thread owns the socket
//! (descriptor passing is synchronous); tokio talks to it through channels.
//!
//! Requests and acknowledgements are paired by order, not by any request id
//! carried in the wire messages themselves: the helper answers every
//! `ToHelper` message with exactly one `ToDaemon::Ack` (see
//! `konedrive-helper/src/connection.rs::serve_one`, which loops `recv` then
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

use konedrive_fs::handle::FileHandle;
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr};
use tokio::sync::{mpsc, oneshot, watch};

pub mod linked;
pub mod status;

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

/// What an `Ack` answers a call with: the descriptor it carries, if any
/// (only `OpenByHandle`'s does).
type Reply = Result<Option<OwnedFd>, HelperError>;

/// One outgoing request, plus where to deliver the eventual answer.
struct Call {
    message: ToHelper,
    fd: Option<OwnedFd>,
    reply: oneshot::Sender<Reply>,
}

/// Callers waiting on the next `Ack`, oldest first. Shared between the
/// writer thread (which pushes, in send order) and the reader thread (which
/// pops, in arrival order, and drains the rest on disconnect).
type PendingReplies = Arc<Mutex<VecDeque<oneshot::Sender<Reply>>>>;

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
        // waits for a fill slot — and `hydration::server::tests::the_reader_reaches_acks_
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
                        Ok((ToDaemon::Ack { errno }, fd)) => {
                            let Some(reply) = pending.lock().unwrap().pop_front() else {
                                continue;
                            };
                            let result =
                                if errno == 0 { Ok(fd) } else { Err(HelperError::Refused(errno)) };
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
        // A descriptor on an `Ack` nobody asked for is closed here.
        self.call_for_reply(message, fd, timeout).await.map(drop)
    }

    async fn call_for_reply(&self, message: ToHelper, fd: Option<OwnedFd>, timeout: Duration) -> Reply {
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

    /// A descriptor for the object `handle` names (`OpenByHandle`, writes
    /// design §4.6): only the helper can open a file handle. `dir` is any
    /// directory of this user's on the object's filesystem — the folder's
    /// root does — and on a device the helper has a root of this user's on.
    ///
    /// - `Ok`: a directory comes back `O_RDONLY | O_DIRECTORY`; a regular
    ///   file `O_RDONLY | O_NONBLOCK`, as the helper cannot open a user's
    ///   file for writing (`docs/kernel-behavior-7.2/open-by-handle.md` §15) —
    ///   [`reopen_for_writing`] gets a writable one. Where it is now is
    ///   `/proc/self/fd/<fd>`.
    /// - `Refused(ESTALE)`: the object is gone — the handle names nothing,
    ///   or an object with no link left.
    /// - `Refused(EPERM)`: not this user's to have: another user's, not a
    ///   file or directory, without `user.konedrive.item-id`, on another
    ///   device than `dir`, or `dir` itself not on one of this user's roots.
    /// - `Refused(EINVAL)`: not a handle the kernel gives (over 128 bytes,
    ///   empty, a negative type).
    /// - `Refused(EAGAIN)`: somebody holds a lease on the file; ask again.
    /// - `Refused(_)` otherwise: what the kernel said.
    ///
    /// The helper's own open is exempt from its interception, so asking for
    /// a placeholder, marked or not, neither fills it nor waits for a fill.
    pub async fn open_by_handle(&self, dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError> {
        let message = ToHelper::OpenByHandle { handle_type: handle.kind, handle: handle.bytes.clone() };
        match self.call_for_reply(message, Some(dup(dir)?), self.call_timeout).await? {
            Some(object) => Ok(object),
            None => Err(HelperError::Io("the helper answered OpenByHandle without a descriptor".into())),
        }
    }
}

/// A descriptor for writing to the same file as `object`, opened by this
/// process, as the file's owner, through `/proc/self/fd`: the file's own
/// permissions are checked, not any directory's, so it works wherever the file
/// has gone. For a descriptor [`HelperLink::open_by_handle`] returned, which is
/// read-only. The same inode or an error.
///
/// An open of the file like any other: if it is intercepted — it has its own
/// mark (`MarkFile`), or sits in a marked directory — the helper lets it
/// through as this daemon's own open, and fills nothing.
pub fn reopen_for_writing(object: &OwnedFd) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;
    let before = File::from(object.try_clone()?).metadata()?;
    if !before.is_file() {
        return Err(io::Error::from_raw_os_error(libc::EISDIR));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/proc/self/fd/{}", object.as_raw_fd()))?;
    let after = file.metadata()?;
    if (after.dev(), after.ino()) != (before.dev(), before.ino()) {
        return Err(io::Error::other("the reopened descriptor is not the same file"));
    }
    Ok(file)
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
        Ok(Ok(result)) => result.map(drop),
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
pub type LinkCell = Arc<std::sync::Mutex<Option<HelperLink>>>;

#[cfg(test)]
mod tests;

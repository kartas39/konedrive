//! The daemon's end of the helper socket. Two blocking threads own the socket
//! (descriptor passing is synchronous); tokio talks to them through channels.
//!
//! - **Requests and acknowledgements are paired by order.** The helper answers every
//!   `ToHelper` message with exactly one `ToDaemon::Ack`, in order
//!   (`konedrive-helper/src/connection.rs`), so a FIFO queue of the callers waiting is
//!   enough: the writer pushes a caller's reply right before it writes the caller's
//!   message, and the reader pops the front for every `Ack` ([`write_calls`],
//!   [`read_answers`]). A `HydrateRequest` is not a reply and goes straight to the channel
//!   `connect` returns.
//! - **The handshake is read by the reader thread**, like every other message, and handed
//!   to `connect` through a `oneshot`: no blocking read on the caller's thread. `Hello` is
//!   the first queued call, never fire-and-forget: a refusal fails the connection.
//! - **Every call is bounded** (30 s; 120 s for the two calls in which the helper walks the
//!   whole tree; the handshake shares the 30 s). Since pairing is by order, a call that
//!   times out cannot leave its place in the queue: the whole connection is shut down, the
//!   reader fails every other waiting call `NotRunning`, and only the one that timed out
//!   gets `Timeout`.
//! - **Dropping shuts the connection down** ([`ShutdownOnDrop`]): `connect` can be
//!   cancelled mid-handshake, and a plain `UnixStream`'s drop closes one of three
//!   duplicates, which would leave the reader blocked in `recv` for ever.
//! - **Nothing the helper sends is dropped in silence**: an `Ack` nobody waits for, a
//!   hydration request without its descriptor, a second `Welcome` and the error that ends
//!   the reading are each logged.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc as blocking_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr};
use tokio::sync::{mpsc, oneshot, watch};

use super::HelperError;

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
    /// whoever ended it. What the hub's supervisor waits on,
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

    /// As [`connect`](Self::connect), with the two bounds given: the tests of a helper that
    /// is stuck use short ones.
    async fn connect_with_timeout(
        socket_path: &Path,
        call_timeout: Duration,
        register_root_timeout: Duration,
    ) -> io::Result<(Self, mpsc::Receiver<HydrateRequest>)> {
        let stream = connect_seqpacket(socket_path)?;
        let channel = Channel::new(stream)?;

        // Three duplicates of one open file description: a `shutdown()` on any of them
        // ends the connection for all, and wakes whichever thread sits in a blocking call.
        let writer = Channel::new(channel.get_ref().try_clone()?)?;
        let reader = Channel::new(channel.get_ref().try_clone()?)?;
        // If this function returns early, or is dropped while it waits below, this local
        // shuts the connection down.
        let control_stream = ShutdownOnDrop(channel.get_ref().try_clone()?);
        drop(channel);

        let (calls_tx, calls_rx) = blocking_mpsc::channel::<Call>();
        // Exactly as deep as the helper may have requests outstanding on this connection
        // (`MAX_OUTSTANDING_HYDRATIONS`), so that every request in flight fits and the
        // reader never stops reading: an `Ack` queued behind requests it cannot take would
        // never be read, and the fill waiting for that `Ack` would never free its slot. The
        // worst case needs one less — the request loop holds one while it waits for a fill
        // slot — and `hydration::server::tests::the_reader_reaches_acks_queued_behind_
        // every_request_the_helper_may_send` fails at 62.
        let (requests_tx, requests_rx) = mpsc::channel::<HydrateRequest>(konedrive_proto::MAX_OUTSTANDING_HYDRATIONS);
        let pending: PendingReplies = Arc::new(Mutex::new(VecDeque::new()));
        let (welcome_tx, welcome_rx) = oneshot::channel::<Result<(), HelperError>>();
        let (closed_tx, closed_rx) = watch::channel(false);

        // Both threads start before the handshake: the reader is what reads it.
        {
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || write_calls(writer, calls_rx, &pending));
        }
        std::thread::spawn(move || read_answers(reader, &pending, requests_tx, welcome_tx, closed_tx));

        // A helper that accepts a connection and then says nothing is as broken as one that
        // takes a call and never answers it: both waits share `call_timeout`.
        if let Err(e) = await_welcome(welcome_rx, call_timeout).await {
            return Err(handshake_error(e));
        }
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

/// The queue of waiting callers, whatever a panic under it left: its content is replies to
/// send, and each is still to be sent.
fn waiting(pending: &PendingReplies) -> std::sync::MutexGuard<'_, VecDeque<oneshot::Sender<Reply>>> {
    crate::panic::lock(pending)
}

/// The writer thread: writes every call in order, pushing its reply onto the back of
/// `pending` right before the write, so that the reader never meets an `Ack` for a call
/// that is not queued yet. Only this thread pushes, so after a failed write the entry it
/// just pushed is still at the back: no `Ack` for it can come (the message never left), and
/// it is answered here.
fn write_calls(mut writer: Channel, calls: blocking_mpsc::Receiver<Call>, pending: &PendingReplies) {
    while let Ok(call) = calls.recv() {
        waiting(pending).push_back(call.reply);
        if let Err(e) = writer.send(&call.message, call.fd.as_ref().map(AsFd::as_fd)) {
            tracing::info!("a message to the helper could not be sent, and the connection is closed: {e}");
            if let Some(reply) = waiting(pending).pop_back() {
                let _ = reply.send(Err(HelperError::NotRunning));
            }
            // Wakes the reader out of its `recv`, which answers what else waits.
            let _ = writer.get_ref().shutdown(std::net::Shutdown::Both);
            break;
        }
    }
}

/// The reader thread: the opening `Welcome` answers the handshake, an `Ack` the oldest
/// waiting call, and a `HydrateRequest` goes to the tokio side. When the reading ends —
/// the helper gone, the connection shut down, a message that cannot be read — every call
/// still waiting, and the handshake if `Welcome` never came, is answered `NotRunning`.
fn read_answers(
    mut reader: Channel,
    pending: &PendingReplies,
    requests: mpsc::Sender<HydrateRequest>,
    welcome: oneshot::Sender<Result<(), HelperError>>,
    closed: watch::Sender<bool>,
) {
    let mut welcome = Some(welcome);
    loop {
        match reader.recv::<ToDaemon>() {
            Ok((ToDaemon::Welcome { version }, _)) => {
                let Some(welcome) = welcome.take() else {
                    tracing::warn!("the helper sent a second Welcome (version {version}); passed over");
                    continue;
                };
                let greeted = if version == PROTOCOL_VERSION {
                    Ok(())
                } else {
                    Err(HelperError::Io(format!(
                        "the helper greeted with protocol version {version}, expected {PROTOCOL_VERSION}"
                    )))
                };
                let _ = welcome.send(greeted);
            }
            Ok((ToDaemon::Ack { errno }, fd)) => {
                let Some(reply) = waiting(pending).pop_front() else {
                    tracing::warn!("the helper acknowledged (errno {errno}) a call nobody is waiting for; passed over");
                    continue;
                };
                let _ = reply.send(if errno == 0 { Ok(fd) } else { Err(HelperError::Refused(errno)) });
            }
            Ok((ToDaemon::HydrateRequest { req_id }, Some(fd))) => {
                if requests.blocking_send(HydrateRequest { req_id, fd }).is_err() {
                    tracing::info!("nobody takes hydration requests any more; the helper's connection is closed");
                    break;
                }
            }
            Ok((ToDaemon::HydrateRequest { req_id }, None)) => {
                // It cannot be filled, and without the descriptor not answered for
                // either: the helper's own bounds deny that open.
                tracing::warn!("the helper sent hydration request {req_id} without its descriptor; passed over");
            }
            Err(e) => {
                // Also the ordinary end: the helper stopped, or this side shut down.
                tracing::info!("reading from the helper ended: {e}");
                break;
            }
        }
    }
    if let Some(welcome) = welcome.take() {
        let _ = welcome.send(Err(HelperError::NotRunning));
    }
    let _ = reader.get_ref().shutdown(std::net::Shutdown::Both);
    for reply in waiting(pending).drain(..) {
        let _ = reply.send(Err(HelperError::NotRunning));
    }
    let _ = closed.send(true);
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
        .open(konedrive_fs::proc_path(object))?;
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

/// Where an account keeps the link to the helper: empty while there is none. The hub
/// (`helper::hub`) is the one writer; everything else reads it at each use, since a link
/// goes and comes with the helper.
#[derive(Clone, Default)]
pub struct LinkCell(Arc<Mutex<Option<HelperLink>>>);

impl LinkCell {
    /// A cell that starts with `link`.
    pub fn holding(link: Option<HelperLink>) -> Self {
        Self(Arc::new(Mutex::new(link)))
    }

    /// The link, if there is one right now.
    pub fn get(&self) -> Option<HelperLink> {
        crate::panic::lock(&self.0).clone()
    }

    pub fn is_linked(&self) -> bool {
        crate::panic::lock(&self.0).is_some()
    }

    /// A new link, or its loss.
    pub fn set(&self, link: Option<HelperLink>) {
        *crate::panic::lock(&self.0) = link;
    }
}

#[cfg(test)]
mod tests;

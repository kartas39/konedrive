use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use konedrive_fs::handle::FileHandle;
use konedrive_helper::{by_handle, roots};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::errno::Errno;
use nix::sys::socket::{accept, getsockopt, sockopt::PeerCredentials};

use konedrive_helper::jobs::Owner;
use konedrive_helper::outbox::{Outbox, Outgoing};

use crate::events::{dispatch, settle, Finish};
use crate::registration::{errno_of, refuse_malformed_id, register_root, unregister_root};
use crate::shared::{
    fault, Daemon, Refusal, Shared, Throttle, ACCEPT_BACKOFF, ACCEPT_RESTART,
    MAX_CONNECTIONS_PER_UID,
};

/// Accepts connections for as long as the helper runs.
///
/// A panic on this thread is contained: it used to end the thread, and a
/// helper with no accept thread answers the opens of the daemons it has and
/// never takes another, so after a daemon's next restart every open of its
/// user's placeholders was denied until the helper itself was restarted.
/// The connection in hand when it panicked is closed by the unwind; the
/// listener and the count of connections are kept, and accepting starts
/// again after [`ACCEPT_RESTART`].
pub(crate) fn serve(shared: Arc<Shared>, listener: OwnedFd) {
    // Connections are numbered here, on the one thread that
    // accepts them, in the order they were accepted — never on the
    // per-connection thread. Numbered there, two connections accepted a
    // moment apart could draw their numbers in either order, and "newer"
    // would mean "whose thread the scheduler ran first". `Registry::register`
    // relies on this order being the accept order. A local counter rather
    // than a shared one, so that nothing else can ever hand one out; it
    // outlives a panic of the loop below, so no number is given twice.
    let mut next_conn: u64 = 0;
    loop {
        // `accept_connections` returns only by unwinding.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            accept_connections(&shared, &listener, &mut next_conn)
        }));
        tracing::error!(
            "the thread that accepts daemon connections panicked; the connection it had in hand              is closed, and it accepts again in {ACCEPT_RESTART:?}"
        );
        std::thread::sleep(ACCEPT_RESTART);
    }
}

fn accept_connections(shared: &Arc<Shared>, listener: &OwnedFd, next_conn: &mut u64) -> ! {
    let mut failing = Throttle::new();
    loop {
        let fd = match accept(listener.as_raw_fd()) {
            Ok(fd) => {
                let unreported = failing.reset();
                if unreported > 0 {
                    tracing::error!(
                        "accept works again; it had failed {unreported} more time(s) since the \
                         last report"
                    );
                }
                fd
            }
            Err(Errno::EINTR) => continue,
            Err(e) => {
                // Retrying immediately is right for a transient
                // error and catastrophic for a persistent one: on `EMFILE`
                // `accept` fails as fast as the CPU can call it, so this loop
                // pinned a core and flooded the journal at the exact moment
                // the machine was already short of descriptors. Backing off
                // costs a connection setup 50 ms and nothing else.
                if let Some(occurrences) = failing.admit() {
                    tracing::error!(
                        "accept failed: {e} ({occurrences} time(s)); retrying every \
                         {ACCEPT_BACKOFF:?}"
                    );
                }
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        // SAFETY: `accept` returned a freshly opened descriptor we now own.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        // `fault-injection` builds only.
        fault::panic_on_accept();
        // A bounded number of connections per uid,
        // counted here, before a thread is spent on one. A peer whose
        // credentials cannot be read is not served at all — `serve_one`
        // would refuse it too.
        let slot = match getsockopt(&stream, PeerCredentials) {
            Ok(peer) => match shared.connections.take(peer.uid()) {
                Some(slot) => slot,
                None => {
                    let uid = peer.uid();
                    shared.refusals.report(Refusal::TooManyConnections, || {
                        format!(
                            "uid {uid} is already holding {MAX_CONNECTIONS_PER_UID} connections; \
                             closing another one as soon as it was accepted"
                        )
                    });
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!("cannot read a new connection's credentials: {e}; closing it");
                continue;
            }
        };
        *next_conn += 1;
        let conn = *next_conn;
        let shared = Arc::clone(shared);
        if let Err(e) = std::thread::Builder::new()
            .name("konedrive-daemon".into())
            .spawn(move || {
                let _slot = slot;
                if let Err(e) = serve_one(&shared, stream, conn) {
                    tracing::info!("daemon connection ended: {e}");
                }
            })
        {
            tracing::error!("cannot serve a new connection: {e}");
        }
    }
}

/// Runs a connection's cleanup exactly once, however the connection ends
///.
///
/// This used to be plain statements after an immediately-invoked closure, so
/// a panic anywhere in the request loop unwound straight past them. The
/// damage was not the lost log line: the `Daemon` stayed in `shared.daemons`,
/// so every later hydration for that uid was addressed to a connection
/// nobody was reading, and its suspended openers were never denied. A `Drop`
/// guard runs during unwinding as well as on the ordinary path, which is the
/// only version of this that is true regardless of what the loop did.
struct Disconnect<'a> {
    shared: &'a Shared,
    uid: u32,
    conn: u64,
    outbox: Arc<Outbox>,
}

impl Drop for Disconnect<'_> {
    fn drop(&mut self) {
        // Stops anything else queueing onto a connection that is finished,
        // and unblocks both of its threads.
        self.outbox.close();

        // This connection only, wherever it sits: an older one
        // going away leaves a newer one on top, and a newer one going away
        // hands the uid back to whichever live connection is under it.
        self.shared.daemons.deregister(self.uid, self.conn);
        // Everything *this connection* was going to hydrate now fails rather
        // than hangs. Not everything in the system: the socket is 0666, and
        // draining every pending job on any disconnect let any local user
        // fail every hydration on the machine with a connect-and-close loop.
        // `retire` marks the connection dead before it drains,
        // so a worker still holding a `Daemon` clone cannot slip a new job in
        // behind the drain.
        let (stranded, still_running) = self.shared.jobs.retire(self.conn);
        let suspended: usize = stranded.iter().map(Vec::len).sum();
        if suspended > 0 {
            tracing::warn!(
                "uid {} connection {} went away with {} hydrations in flight; denying {suspended} \
                 suspended opens EIO ({still_running} hydrations for other connections are \
                 untouched)",
                self.uid,
                self.conn,
                stranded.len()
            );
        }
        for waiters in stranded {
            for open in waiters {
                open.deny(libc::EIO);
            }
        }
    }
}

/// Serves one accepted connection. `conn` was assigned by [`serve`] at accept
/// time and is what orders this connection against any other
/// from the same uid.
fn serve_one(shared: &Shared, stream: UnixStream, conn: u64) -> anyhow::Result<()> {
    // `std::os::unix::net::UnixStream::peer_cred` is still unstable
    // (`peer_credentials_unix_socket`) on this toolchain, so SO_PEERCRED is
    // read via nix's getsockopt instead — the same kernel-attached
    // credential, through a stable API. SO_PEERCRED works the same way on a
    // SOCK_SEQPACKET socket as on a stream one, and the kernel attaches it at
    // connect() time, so the peer cannot forge any part of it.
    let peer = getsockopt(&stream, PeerCredentials)?;
    let uid = peer.uid();
    let pid = peer.pid();
    let owner = Owner { uid, conn };

    // Reading and writing get their own descriptor. They must: a connection
    // sitting in `recv` waiting for the daemon's next request would otherwise
    // hold whatever `HydrateRequest` needs, and the daemon would be waiting
    // for exactly that request. Interleaving is safe: the daemon's reader
    // tells `Ack` from `HydrateRequest` by variant and only pairs `Ack`s with
    // its outstanding calls (see konedrived/src/helper/mod.rs), and one
    // writer thread keeps each `send` a single datagram in queue order.
    //
    // Sending is a bounded queue plus that thread, not a mutex around the
    // socket: `Channel::send` blocks, and a peer that stops
    // reading must cost this connection, never a worker thread.
    let mut reader = Channel::new(stream.try_clone()?)?;
    let outbox =
        Arc::new(Outbox::start(stream.try_clone()?, stream, &format!("{uid}-{conn}"))?);

    // Registered before the greeting is queued, so that the cleanup is armed
    // from the first instant there is anything to clean up.
    let _disconnect =
        Disconnect { shared, uid, conn, outbox: Arc::clone(&outbox) };

    // The helper greets unprompted, before it reads anything. The client
    // relies on that, and requiring a `Hello` would buy nothing: `SO_PEERCRED`
    // already tells us who the peer is, and a `Hello` carries only a version
    // number the peer could lie about.
    if outbox
        .try_send(Outgoing { message: ToDaemon::Welcome { version: PROTOCOL_VERSION }, fd: None })
        .is_err()
    {
        anyhow::bail!("cannot greet a new daemon connection");
    }
    // Registering wakes every open that was waiting for this uid's daemon.
    if !shared.daemons.register(Daemon { conn, uid, pid, outbox: Arc::clone(&outbox) }) {
        tracing::info!(
            "uid {uid} connection {conn} registered after a newer connection from the same \
             uid; it stays underneath, and takes over only if the newer one goes"
        );
    }

    loop {
        // Only a transport or deserialisation failure ends the connection.
        // Anything `apply` runs into is this request's problem and comes back
        // as an errno on this request's `Ack`.
        let (message, fd) = reader.recv::<ToHelper>()?;
        // Any message at all is proof of life, and is what
        // keeps a daemon that is slow to read — rather than wedged — from
        // being disconnected by its own backpressure.
        outbox.heard_from_peer();
        // A peer that says it speaks another version is not served: the
        // connection ends here, with no `Ack`, and its cleanup runs as for
        // any other end. The daemon never gets this far with another
        // version: it has hung up on the `Welcome` (`konedrived/src/helper`).
        if let Some(theirs) = another_version(&message) {
            anyhow::bail!(
                "the peer speaks protocol version {theirs}, not {PROTOCOL_VERSION}; closing"
            );
        }
        let mut reply = None;
        let errno = apply(shared, owner, &outbox, message, fd, &mut reply);
        // Into the room reserved for `Ack`s. This used to end
        // the connection when the outbox was full — tearing down, on
        // backpressure, a daemon that had just proved it was alive by sending
        // this request. Now an `Ack` is never refused: a peer with more
        // replies unread than any daemon has calls in flight is made to wait
        // here, and this thread reads nothing more from it until it catches
        // up. Only a connection that is already over refuses one.
        if outbox.send_ack_with(errno, reply).is_err() {
            anyhow::bail!("the connection ended while acknowledging a request");
        }
    }
}

/// The version a `Hello` names, when it is not the helper's.
///
/// `Hello` itself stays optional: a peer that sends none is served (the
/// limitations log, F234).
fn another_version(message: &ToHelper) -> Option<u32> {
    match message {
        ToHelper::Hello { version } if *version != PROTOCOL_VERSION => Some(*version),
        _ => None,
    }
}

/// Applies one request, returning the errno to acknowledge with (0 = fine),
/// and in `reply` the descriptor the `Ack` carries, if any (`OpenByHandle`).
///
///: this never fails the connection. A `fanotify_mark` that returns
/// `ENOENT` because an evictable mark was already reclaimed is a routine
/// outcome, and turning it into a teardown took every in-flight hydration down
/// with it.
fn apply(
    shared: &Shared,
    owner: Owner,
    outbox: &Outbox,
    message: ToHelper,
    fd: Option<OwnedFd>,
    reply: &mut Option<OwnedFd>,
) -> i32 {
    let uid = owner.uid;
    let object = fd.map(File::from);
    let allowed = |object: &File| -> bool {
        match object.metadata() {
            Ok(meta) => shared.roots.may_act_on(uid, meta.dev(), meta.uid()),
            Err(e) => {
                tracing::warn!("cannot stat an object sent by uid {uid}: {e}");
                false
            }
        }
    };

    let owns_regular_file = |object: &File| -> bool {
        match object.metadata() {
            Ok(meta) => roots::may_clear_ignore(uid, meta.uid(), meta.is_file()),
            Err(e) => {
                tracing::warn!("cannot stat an object sent by uid {uid}: {e}");
                false
            }
        }
    };

    // The message's own fields, before anything is done with them. Asked
    // here and answered in the arms below, which a malformed request reaches
    // only with what a well-formed one needs attached: without it, it is
    // refused as that one would be.
    let malformed = message.validate().is_err();

    match (message, object) {
        // Its version is ours: `serve_one` has closed on any other.
        (ToHelper::Hello { .. }, _) => 0,
        (ToHelper::RegisterRoot { root_id }, Some(_)) if malformed => {
            refuse_malformed_id(shared, uid, &root_id)
        }
        (ToHelper::RegisterRoot { root_id }, Some(dir)) => register_root(shared, owner, root_id, dir),
        (ToHelper::UnregisterRoot { root_id }, _) => unregister_root(shared, uid, &root_id),
        (ToHelper::MarkDir, Some(dir)) if allowed(&dir) => act(shared.marks.mark_dir(dir.as_fd())),
        (ToHelper::UnmarkDir, Some(dir)) if allowed(&dir) => {
            act(shared.marks.unmark_dir(dir.as_fd()))
        }
        (ToHelper::MarkFile, Some(file)) if allowed(&file) => {
            // `fault-injection` builds only.
            fault::panic_on_mark_file();
            act(shared.marks.mark_file(file.as_fd()))
        }
        // On ownership of a regular file alone. Removing an
        // ignore mark can only cost an extra interception, never zeros, and
        // every punch in the daemon now asks for it whenever it has a link —
        // in a folder registered without interception too, where the uid may
        // hold no root at all.
        (ToHelper::ClearIgnore, Some(file)) if owns_regular_file(&file) => {
            act(shared.marks.clear_ignore(file.as_fd()))
        }
        (ToHelper::HydrateDone { req_id, errno }, _) => {
            // Answers its openers, and sends the hydration its credit goes to
            // next.
            let next = settle(shared, req_id, owner, errno, Finish::Reported);
            dispatch(shared, outbox, owner, next);
            0
        }
        // Authorised on the object it finds, not on the handle: see
        // `by_handle`. Refusals are not logged — they are the daemon's
        // answer, and any local user can ask.
        (ToHelper::OpenByHandle { .. }, Some(_)) if malformed => libc::EINVAL,
        (ToHelper::OpenByHandle { handle_type, handle }, Some(dir)) => {
            let handle = FileHandle { kind: handle_type, bytes: handle };
            let on_a_root = |dev| shared.roots.may_act_on(uid, dev, uid);
            match by_handle::open(uid, &dir, &handle, on_a_root) {
                Ok(opened) => {
                    *reply = Some(opened);
                    0
                }
                Err(errno) => errno,
            }
        }
        _ => libc::EPERM,
    }
}

fn act(result: io::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            tracing::warn!("a mark request failed: {e}");
            errno_of(&e)
        }
    }
}

#[cfg(test)]
mod tests;

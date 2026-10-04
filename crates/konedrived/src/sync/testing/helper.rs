//! A stand-in for konedrive-helper on a socket of the test's own.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};

/// What a fake helper was asked to do, in the order it was asked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Seen {
    RegisterRoot,
    UnregisterRoot,
    /// A `MarkDir`, and how many entries the directory held **at the
    /// moment the mark arrived** — read through the very descriptor the
    /// daemon attached. Invariant M1 says a new directory is marked
    /// before anything is created inside it, and this is the only way to
    /// measure that from outside: a mark that arrives after the
    /// directory has been filled reports a non-zero count.
    MarkDir { entries: usize },
    MarkFile,
    ClearIgnore,
    HydrateDone,
}

impl Seen {
    /// The request's kind alone: `MarkDir` with its count left out.
    pub fn kind(&self) -> Seen {
        match self {
            Seen::MarkDir { .. } => Seen::MarkDir { entries: 0 },
            other => other.clone(),
        }
    }
}

/// What the test told the helper to do with requests, by kind.
#[derive(Default)]
struct Script {
    /// Answered with an errno instead of 0. Anything absent is acknowledged.
    refusals: HashMap<Seen, i32>,
    /// Not answered until the test lets them go.
    held: HashSet<Seen>,
}

/// A fake helper that records what it was asked to do, answers as the test scripts it, and
/// can be cut off on demand: it greets, acknowledges `Hello`, and acknowledges everything
/// after that — unless the test has it [refuse](Self::refuse) a kind of request, or
/// [hold](Self::hold) its answer until [released](Self::release). It accepts connection
/// after connection, so a daemon that reconnects finds it still there.
pub struct FakeHelper {
    seen: Arc<Mutex<Vec<Seen>>>,
    script: Arc<(Mutex<Script>, Condvar)>,
    /// A duplicate of the live connection's socket, so a test can cut it
    /// the way a helper that died would.
    live: Arc<Mutex<Option<UnixStream>>>,
}

impl FakeHelper {
    /// Starts one on `path`. The listener is bound before this returns, so a connection
    /// cannot race it.
    pub fn start(path: PathBuf) -> Self {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
        let addr = UnixAddr::new(&path).unwrap();
        bind(fd.as_raw_fd(), &addr).unwrap();
        listen(&fd, Backlog::new(16).unwrap()).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<(Mutex<Script>, Condvar)> = Arc::default();
        let live: Arc<Mutex<Option<UnixStream>>> = Arc::default();
        let (recorded, scripted, current) = (Arc::clone(&seen), Arc::clone(&script), Arc::clone(&live));
        std::thread::spawn(move || {
            let listener: OwnedFd = fd;
            while let Ok(accepted) = accept(listener.as_raw_fd()) {
                // SAFETY: `accept` just returned a freshly opened
                // descriptor that this process now solely owns.
                let stream = unsafe { UnixStream::from_raw_fd(accepted) };
                *current.lock().unwrap() = stream.try_clone().ok();
                let Ok(mut channel) = Channel::new(stream) else { continue };
                if channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).is_err() {
                    continue;
                }
                while let Ok((message, fd)) = channel.recv::<ToHelper>() {
                    let note = match &message {
                        ToHelper::Hello { .. } | ToHelper::UnmarkDir | ToHelper::OpenByHandle { .. } => None,
                        ToHelper::RegisterRoot { .. } => Some(Seen::RegisterRoot),
                        ToHelper::UnregisterRoot { .. } => Some(Seen::UnregisterRoot),
                        ToHelper::MarkDir => Some(Seen::MarkDir { entries: entries_of(fd.as_ref()) }),
                        ToHelper::MarkFile => Some(Seen::MarkFile),
                        ToHelper::ClearIgnore => Some(Seen::ClearIgnore),
                        ToHelper::HydrateDone { .. } => Some(Seen::HydrateDone),
                    };
                    // Kinds are compared without `MarkDir`'s entry count.
                    let kind = note.as_ref().map(Seen::kind);
                    if let Some(note) = note {
                        recorded.lock().unwrap().push(note);
                    }
                    let errno = match kind {
                        None => 0,
                        Some(kind) => {
                            // Recorded first: the test sees the request, then lets it go.
                            let (script, released) = &*scripted;
                            let script = released.wait_while(script.lock().unwrap(), |script| script.held.contains(&kind)).unwrap();
                            script.refusals.get(&kind).copied().unwrap_or(0)
                        }
                    };
                    if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                        break;
                    }
                }
            }
        });
        Self { seen, script, live }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The list [`seen`](Self::seen) copies, for a fake that looks at it as it is called.
    pub fn log(&self) -> Arc<Mutex<Vec<Seen>>> {
        Arc::clone(&self.seen)
    }

    /// From now on, answers every request of `kind` with `errno`.
    pub fn refuse(&self, kind: Seen, errno: i32) {
        self.script.0.lock().unwrap().refusals.insert(kind.kind(), errno);
    }

    /// From now on, a request of `kind` is recorded and not answered until
    /// [`release`](Self::release): whoever sent it waits meanwhile, and so does every
    /// request behind it on the connection.
    pub fn hold(&self, kind: Seen) {
        self.script.0.lock().unwrap().held.insert(kind.kind());
    }

    /// Answers the request of `kind` that is held, and every one after it.
    pub fn release(&self, kind: Seen) {
        self.script.0.lock().unwrap().held.remove(&kind.kind());
        self.script.1.notify_all();
    }

    /// Forgets what it has seen so far.
    pub fn forget(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// Sends the daemon a hydration request for `fd` on the live
    /// connection, as the helper does for an intercepted open. A second
    /// `Channel` on the same socket is safe: every send is one datagram.
    pub fn send_request(&self, req_id: u64, fd: &OwnedFd) {
        let live = self.live.lock().unwrap();
        let stream = live.as_ref().expect("a live connection").try_clone().unwrap();
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::HydrateRequest { req_id }, Some(fd.as_fd())).unwrap();
    }

    /// Cuts the live connection, the way a helper that crashed would.
    pub fn hang_up(&self) {
        if let Some(stream) = self.live.lock().unwrap().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for FakeHelper {
    /// Nothing stays held: the helper's thread ends with its connection.
    fn drop(&mut self) {
        self.script.0.lock().unwrap().held.clear();
        self.script.1.notify_all();
    }
}

/// How many entries a directory holds, through a descriptor rather than
/// a name.
fn entries_of(fd: Option<&OwnedFd>) -> usize {
    let Some(fd) = fd else { return usize::MAX };
    std::fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd())).map(|entries| entries.count()).unwrap_or(usize::MAX)
}

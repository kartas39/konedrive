//! Test support of `helper/`: the stand-ins for the helper's end of the socket, for every
//! test of the daemon that needs a link. Two shapes over one accept loop: [`fake_helper`],
//! whose answers are a closure of the test's, and [`FakeHelper`], which records what it is
//! asked and answers as the test scripts it.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};

use super::HelperLink;

/// A `SOCK_SEQPACKET` listener bound at `path`, as the helper builds its own
/// (`konedrive-helper/src/main.rs`): a `UnixListener` is a stream socket, which `Channel`
/// refuses.
pub(crate) fn seqpacket_listener(path: &Path) -> OwnedFd {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(fd.as_raw_fd(), &UnixAddr::new(path).unwrap()).unwrap();
    listen(&fd, Backlog::new(16).unwrap()).unwrap();
    fd
}

pub(crate) fn seqpacket_accept(listener: &OwnedFd) -> UnixStream {
    let fd = accept(listener.as_raw_fd()).unwrap();
    // SAFETY: `accept` just returned a descriptor this process alone owns.
    unsafe { UnixStream::from_raw_fd(fd) }
}

/// Reads and acknowledges the daemon's opening `Hello`, as the helper does: `connect`
/// returns only after this exchange.
pub(crate) fn ack_hello(channel: &mut Channel) {
    let (hello, _) = channel.recv::<ToHelper>().unwrap();
    assert!(matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION), "{hello:?}");
    channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
}

/// A stand-in helper at `path`: accepts one connection, greets, acknowledges the `Hello`,
/// and answers every message after that with the errno `answer` gives for it. `answer` runs
/// on the helper's thread while the daemon waits for the acknowledgement.
///
/// The listener is bound on the caller's thread, before this returns, so the test's
/// `connect` cannot come before the `bind`.
pub(crate) fn fake_helper(path: &Path, mut answer: impl FnMut(&ToHelper, Option<OwnedFd>) -> i32 + Send + 'static) {
    let listener = seqpacket_listener(path);
    std::thread::spawn(move || {
        let mut channel = Channel::new(seqpacket_accept(&listener)).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        ack_hello(&mut channel);
        answer_all(&mut channel, |message, fd| (answer(message, fd), None));
    });
}

/// The answer loop of every stand-in: each message after the greeting gets an `Ack` with
/// the errno, and the descriptor if any, that `answer` gives for it, until the daemon's end
/// closes.
fn answer_all(channel: &mut Channel, mut answer: impl FnMut(&ToHelper, Option<OwnedFd>) -> (i32, Option<OwnedFd>)) {
    while let Ok((message, fd)) = channel.recv::<ToHelper>() {
        let (errno, object) = answer(&message, fd);
        if channel.send(&ToDaemon::Ack { errno }, object.as_ref().map(AsFd::as_fd)).is_err() {
            break;
        }
    }
}

/// A link to the stand-in helper at `socket_path`.
pub(crate) async fn connected(socket_path: &Path) -> HelperLink {
    HelperLink::connect(socket_path).await.unwrap().0
}

/// What a [`FakeHelper`] was asked to do, in the order it was asked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Seen {
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
    pub(crate) fn kind(&self) -> Seen {
        match self {
            Seen::MarkDir { .. } => Seen::MarkDir { entries: 0 },
            other => other.clone(),
        }
    }
}

/// One `MarkDir` as a [`FakeHelper`] saw it.
#[derive(Debug, Clone)]
pub(crate) struct Marked {
    /// The directory's item id; `None` for one that has none.
    pub id: Option<String>,
    pub ino: u64,
    /// How many entries the directory held at that moment.
    pub entries: usize,
    /// Its name at that moment.
    pub name: String,
}

/// What the test told the helper to do with requests.
#[derive(Default)]
struct Script {
    /// Kinds answered with an errno instead of 0. Anything absent is acknowledged.
    refusals: HashMap<Seen, i32>,
    /// Kinds not answered until the test lets them go.
    held: HashSet<Seen>,
    /// The `MarkDir` of a directory whose path ends so says it came on the first channel
    /// and is answered only when told to on the second.
    stall: Option<(String, mpsc::Sender<()>, mpsc::Receiver<()>)>,
    /// `OpenByHandle` looks for the object beneath this directory; with none, it is
    /// acknowledged with no descriptor.
    beneath: Option<PathBuf>,
    /// Every `OpenByHandle` is answered with this errno, while set.
    refuse_opens: Option<i32>,
}

/// What a [`FakeHelper`] keeps of what it was asked.
#[derive(Default)]
struct Kept {
    seen: Arc<Mutex<Vec<Seen>>>,
    marks: Arc<Mutex<Vec<Marked>>>,
    /// (call, where its object was) for every request that names an object.
    calls: Mutex<Vec<(&'static str, PathBuf)>>,
    opens: AtomicUsize,
}

/// The fake helper of the daemon's tests: it greets, acknowledges `Hello`, and acknowledges
/// everything after that, keeping what it was asked ([`seen`](Self::seen),
/// [`marks`](Self::marks), [`called`](Self::called)) — unless the test has it
/// [refuse](Self::refuse) a kind of request, [hold](Self::hold) its answer until
/// [released](Self::release), or [stall](Self::stall_on) on one directory. Told where to
/// look ([`finding_beneath`](Self::finding_beneath)), it answers `OpenByHandle` as the
/// helper does. It accepts connection after connection, so a daemon that reconnects finds
/// it still there, and can be [cut off](Self::hang_up) on demand.
///
/// It answers on its one thread: while it holds an answer, every request behind it on that
/// connection waits too.
pub(crate) struct FakeHelper {
    kept: Arc<Kept>,
    script: Arc<(Mutex<Script>, Condvar)>,
    /// A duplicate of the live connection's socket, so a test can cut it
    /// the way a helper that died would.
    live: Arc<Mutex<Option<UnixStream>>>,
    socket: PathBuf,
    _dir: Option<tempfile::TempDir>,
}

impl FakeHelper {
    /// Starts one on `path`. The listener is bound before this returns, so a connection
    /// cannot race it.
    pub(crate) fn start(path: PathBuf) -> Self {
        Self::start_in(path, None)
    }

    /// One on a socket in a temporary directory of its own.
    pub(crate) fn standalone() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self::start_in(dir.path().join("helper.sock"), Some(dir))
    }

    fn start_in(path: PathBuf, dir: Option<tempfile::TempDir>) -> Self {
        let listener = seqpacket_listener(&path);
        let kept: Arc<Kept> = Arc::default();
        let script: Arc<(Mutex<Script>, Condvar)> = Arc::default();
        let live: Arc<Mutex<Option<UnixStream>>> = Arc::default();
        let (recorded, scripted, current) = (Arc::clone(&kept), Arc::clone(&script), Arc::clone(&live));
        std::thread::spawn(move || {
            while let Ok(accepted) = accept(listener.as_raw_fd()) {
                // SAFETY: `accept` just returned a freshly opened
                // descriptor that this process now solely owns.
                let stream = unsafe { UnixStream::from_raw_fd(accepted) };
                *current.lock().unwrap() = stream.try_clone().ok();
                let Ok(mut channel) = Channel::new(stream) else { continue };
                if channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).is_err() {
                    continue;
                }
                answer_all(&mut channel, |message, fd| answer(&recorded, &scripted, message, fd));
            }
        });
        Self { kept, script, live, socket: path, _dir: dir }
    }

    /// A new link to it.
    pub(crate) async fn connect(&self) -> HelperLink {
        connected(&self.socket).await
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.kept.seen.lock().unwrap().clone()
    }

    /// The list [`seen`](Self::seen) copies, for a fake that looks at it as it is called.
    pub(crate) fn log(&self) -> Arc<Mutex<Vec<Seen>>> {
        Arc::clone(&self.kept.seen)
    }

    /// Every `MarkDir` so far, in order.
    pub(crate) fn marks(&self) -> Vec<Marked> {
        self.kept.marks.lock().unwrap().clone()
    }

    /// The list [`marks`](Self::marks) copies, for a test that looks from another task.
    pub(crate) fn marks_log(&self) -> Arc<Mutex<Vec<Marked>>> {
        Arc::clone(&self.kept.marks)
    }

    /// [`marks`](Self::marks) as (item id, entries at that moment).
    pub(crate) fn marked(&self) -> Vec<(Option<String>, usize)> {
        self.marks().into_iter().map(|m| (m.id, m.entries)).collect()
    }

    /// Where the object of every `call` so far was: `mark_dir`, `mark_file`, `unmark_dir`,
    /// and `open` for an `OpenByHandle` that handed its object over.
    pub(crate) fn called(&self, call: &str) -> Vec<PathBuf> {
        self.kept.calls.lock().unwrap().iter().filter(|(c, _)| *c == call).map(|(_, p)| p.clone()).collect()
    }

    /// How many times an object was asked for by its handle, whatever the answer.
    pub(crate) fn opens_asked(&self) -> usize {
        self.kept.opens.load(Ordering::SeqCst)
    }

    /// From now on, answers every request of `kind` with `errno`; 0 acknowledges again.
    pub(crate) fn refuse(&self, kind: Seen, errno: i32) {
        self.script.0.lock().unwrap().refusals.insert(kind.kind(), errno);
    }

    /// Every `MarkDir` from now on is answered with `errno`; 0 acknowledges again.
    pub(crate) fn refuse_marks(&self, errno: i32) {
        self.refuse(Seen::MarkDir { entries: 0 }, errno);
    }

    /// From now on, a request of `kind` is recorded and not answered until
    /// [`release`](Self::release): whoever sent it waits meanwhile, and so does every
    /// request behind it on the connection.
    pub(crate) fn hold(&self, kind: Seen) {
        self.script.0.lock().unwrap().held.insert(kind.kind());
    }

    /// Answers the request of `kind` that is held, and every one after it.
    pub(crate) fn release(&self, kind: Seen) {
        self.script.0.lock().unwrap().held.remove(&kind.kind());
        self.script.1.notify_all();
    }

    /// The marking of the directory whose path ends in `suffix` says so on
    /// the first channel, and is acknowledged only when told to on the second.
    pub(crate) fn stall_on(&self, suffix: &str) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        self.script.0.lock().unwrap().stall = Some((suffix.to_owned(), reached_tx, release_rx));
        (reached_rx, release_tx)
    }

    /// From now on an `OpenByHandle` is answered as the helper answers it, for the objects
    /// beneath `dir`: the object's descriptor, `ESTALE` for one that is not there, `EPERM`
    /// for one without the item id.
    pub(crate) fn finding_beneath(&self, dir: PathBuf) {
        self.script.0.lock().unwrap().beneath = Some(dir);
    }

    /// Every `OpenByHandle` from now on is answered with `errno`; `None` answers again.
    pub(crate) fn refuse_opens(&self, errno: Option<i32>) {
        self.script.0.lock().unwrap().refuse_opens = errno;
    }

    /// Forgets what it has seen so far.
    pub(crate) fn forget(&self) {
        self.kept.seen.lock().unwrap().clear();
    }

    /// Sends the daemon a hydration request for `fd` on the live
    /// connection, as the helper does for an intercepted open. A second
    /// `Channel` on the same socket is safe: every send is one datagram.
    pub(crate) fn send_request(&self, req_id: u64, fd: &OwnedFd) {
        let live = self.live.lock().unwrap();
        let stream = live.as_ref().expect("a live connection").try_clone().unwrap();
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::HydrateRequest { req_id }, Some(fd.as_fd())).unwrap();
    }

    /// Cuts the live connection, the way a helper that crashed would.
    pub(crate) fn hang_up(&self) {
        if let Some(stream) = self.live.lock().unwrap().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for FakeHelper {
    /// Nothing stays held: the helper's thread ends with its connection.
    fn drop(&mut self) {
        let mut script = self.script.0.lock().unwrap();
        script.held.clear();
        script.stall = None;
        drop(script);
        self.script.1.notify_all();
    }
}

/// What a [`FakeHelper`] answers to one message: it is kept first, so the test sees the
/// request and then lets it go.
fn answer(kept: &Kept, scripted: &(Mutex<Script>, Condvar), message: &ToHelper, fd: Option<OwnedFd>) -> (i32, Option<OwnedFd>) {
    let note = match message {
        ToHelper::Hello { .. } => None,
        ToHelper::UnmarkDir => {
            called(kept, "unmark_dir", fd.as_ref());
            None
        }
        ToHelper::OpenByHandle { handle_type, handle } => {
            kept.opens.fetch_add(1, Ordering::SeqCst);
            let (refused, beneath) = {
                let script = scripted.0.lock().unwrap();
                (script.refuse_opens, script.beneath.clone())
            };
            return match (refused, beneath) {
                (Some(errno), _) => (errno, None),
                (None, None) => (0, None),
                (None, Some(beneath)) => match open_by_handle(&beneath, &FileHandle { kind: *handle_type, bytes: handle.clone() }) {
                    Ok(object) => {
                        called(kept, "open", Some(&object));
                        (0, Some(object))
                    }
                    Err(errno) => (errno, None),
                },
            };
        }
        ToHelper::RegisterRoot { .. } => Some(Seen::RegisterRoot),
        ToHelper::UnregisterRoot { .. } => Some(Seen::UnregisterRoot),
        ToHelper::MarkDir => {
            called(kept, "mark_dir", fd.as_ref());
            Some(Seen::MarkDir { entries: entries_of(fd.as_ref()) })
        }
        ToHelper::MarkFile => {
            called(kept, "mark_file", fd.as_ref());
            Some(Seen::MarkFile)
        }
        ToHelper::ClearIgnore => Some(Seen::ClearIgnore),
        ToHelper::HydrateDone { .. } => Some(Seen::HydrateDone),
    };
    let Some(note) = note else { return (0, None) };
    // Kinds are compared without `MarkDir`'s entry count.
    let kind = note.kind();
    let at = match (&note, fd.as_ref()) {
        (Seen::MarkDir { entries }, Some(dir)) => marked(kept, dir, *entries),
        _ => None,
    };
    kept.seen.lock().unwrap().push(note);
    let (script, released) = scripted;
    let script = released.wait_while(script.lock().unwrap(), |script| script.held.contains(&kind)).unwrap();
    let errno = script.refusals.get(&kind).copied().unwrap_or(0);
    drop(script);
    if let Some(at) = at {
        stall(scripted, &at);
    }
    (errno, None)
}

fn called(kept: &Kept, call: &'static str, object: Option<&OwnedFd>) {
    let at = object.and_then(|fd| std::fs::read_link(konedrive_fs::proc_path(fd)).ok()).unwrap_or_default();
    kept.calls.lock().unwrap().push((call, at));
}

/// Keeps a `MarkDir` with what the directory was at that moment; where it is, if that can
/// be read.
fn marked(kept: &Kept, dir: &OwnedFd, entries: usize) -> Option<PathBuf> {
    let at = std::fs::read_link(konedrive_fs::proc_path(dir)).ok()?;
    let file = File::from(dir.try_clone().ok()?);
    let id = xattr::FileExt::get_xattr(&file, XATTR_ITEM_ID).ok().flatten().and_then(|v| String::from_utf8(v).ok());
    let name = at.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    kept.marks.lock().unwrap().push(Marked { id, ino: file.metadata().ok()?.ino(), entries, name });
    Some(at)
}

/// Waits where the test asked for a stall on the directory at `at`.
fn stall(scripted: &(Mutex<Script>, Condvar), at: &Path) {
    // Taken out while it waits: the test may ask something else meanwhile.
    let Some((suffix, reached, release)) = scripted.0.lock().unwrap().stall.take() else { return };
    if at.to_string_lossy().ends_with(suffix.as_str()) {
        let _ = reached.send(());
        let _ = release.recv();
    }
    scripted.0.lock().unwrap().stall.get_or_insert((suffix, reached, release));
}

/// The object `handle` names beneath `beneath`, as the helper hands it over, or the errno
/// it refuses with.
fn open_by_handle(beneath: &Path, handle: &FileHandle) -> Result<OwnedFd, i32> {
    let handle_of = |path: &Path| FileHandle::at(&File::open(path.parent()?).ok()?, path.file_name()?).ok();
    let mut dirs = vec![beneath.to_path_buf()];
    let mut found = None;
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if handle_of(&path).as_ref() == Some(handle) {
                found = Some(path);
                dirs.clear();
                break;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(path);
            }
        }
    }
    let path = found.ok_or(libc::ESTALE)?;
    let meta = std::fs::symlink_metadata(&path).map_err(|_| libc::ESTALE)?;
    let file = if meta.is_dir() {
        File::open(&path)
    } else {
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(&path)
    }
    .map_err(|_| libc::ESTALE)?;
    if FileHandle::of(&file).ok().as_ref() != Some(handle) {
        return Err(libc::ESTALE);
    }
    if xattr::get(&path, XATTR_ITEM_ID).ok().flatten().is_none() {
        return Err(libc::EPERM);
    }
    Ok(file.into())
}

/// How many entries a directory holds, through a descriptor rather than
/// a name.
fn entries_of(fd: Option<&OwnedFd>) -> usize {
    let Some(fd) = fd else { return usize::MAX };
    std::fs::read_dir(konedrive_fs::proc_path(fd)).map(|entries| entries.count()).unwrap_or(usize::MAX)
}

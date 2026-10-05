use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use std::sync::Arc;

use async_trait::async_trait;
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use tokio::sync::mpsc;
use konedrive_fs::placeholder::{read_state, State};

use crate::hydration::source::{ContentSource, Fetched, LocalDir, SourceError};
use crate::hydration::testing::Faulty;
use crate::helper::{HelperLink, HydrateRequest};
use crate::sync::tests::{CountingSource, FakeHelper, Seen, placeholder, wait_until};
use crate::status::report::Report;
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};
use crate::folder::locks::InodeLocks;
use crate::hydration::source;
use super::*;

/// A stand-in helper: accepts one connection, greets, acknowledges the
/// handshake `Hello`, then acknowledges everything and reports every
/// `HydrateDone` it sees. The listener is bound on the caller's thread,
/// before this returns, so `connect` cannot race `bind`.
///
/// It reports through a `tokio` channel rather than a `std` one because
/// the tests below wait for it *inside* the runtime: a blocking
/// `recv_timeout` on a current-thread runtime would park the one thread
/// that has to run `serve_hydrations`.
pub(crate) fn fake_helper(path: std::path::PathBuf) -> mpsc::UnboundedReceiver<(u64, i32)> {
    let (tx, rx) = mpsc::unbounded_channel();
    crate::helper::testing::fake_helper(&path, move |message, _fd| {
        if let ToHelper::HydrateDone { req_id, errno } = message {
            let _ = tx.send((*req_id, *errno));
        }
        0
    });
    rx
}

struct Panics;

#[async_trait]
impl ContentSource for Panics {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        panic!("this content source explodes on contact");
    }
}

/// A panicking fill answers no one on its own: it closes the
/// event fd by unwinding and produces no errno, so `hydrate_done` is
/// never called and the `open()` the kernel suspended is never responded
/// to at all — it hangs for the life of the helper. A panic in our code
/// must degrade to a denial, never to silence.
#[tokio::test]
async fn a_panicking_fill_still_answers_the_suspended_open() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let local = tempfile::tempdir().unwrap();
    let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);

    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, Arc::new(Panics), InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 77, fd }).await.unwrap();

    let answer = tokio::time::timeout(Duration::from_secs(5), seen.recv())
        .await
        .expect("a panicking hydration must still answer the suspended open")
        .expect("the helper connection must stay up");
    assert_eq!(answer, (77, libc::EIO));
}

/// Pinned the only way it can be: the discriminator is
/// not how many fills run at once but whether
/// the request loop stops *taking* work once [`FILL_ADMISSION`] requests are
/// under way. Acquiring no permit before spawning drains the bounded channel as
/// fast as the helper can fill it, into a pile of tasks each holding a
/// suspended open's event descriptor, and the channel never refuses
/// anything. Here it must refuse.
#[tokio::test]
async fn the_request_loop_stops_taking_work_once_the_admission_is_full() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let _seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![1u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let fds: Vec<OwnedFd> = (0..FILL_ADMISSION + 20)
        .map(|i| placeholder(local.path(), &format!("f{i}.bin"), "ITEM", 4096))
        .collect();

    // Every fill parks in `fetch` and holds its permit there.
    let source = Arc::new(Faulty::new(LocalDir::new(remote.path())).delay(Duration::from_secs(3600)));
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));

    let mut accepted = 0;
    let mut refused = false;
    for (i, fd) in fds.into_iter().enumerate() {
        match tx.try_send(HydrateRequest { req_id: i as u64, fd }) {
            Ok(()) => accepted += 1,
            Err(_) => {
                refused = true;
                break;
            }
        }
        // Let the request loop take everything it is willing to take
        // before offering it the next one.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    assert!(
        refused,
        "the channel accepted all {accepted} requests: the request loop is draining it \
         into unbounded in-flight work instead of stopping at the admission"
    );
    assert!(
        accepted <= FILL_ADMISSION + 4 + 1,
        "at most the admission in flight, four buffered and one blocked on the permit, but \
         {accepted} were accepted"
    );
}

// --- C1: a request looks again -----

/// What `serve_hydrations` answered for one request, through the plain
/// fake helper, with a source that can be watched.
async fn serve_one(fd: OwnedFd, source: Arc<dyn ContentSource>) -> (u64, i32) {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 5, fd }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), seen.recv())
        .await
        .expect("the request must be answered")
        .expect("the helper connection must stay up")
}

/// C1, on the host. A request the helper
/// sent while the file was `online-only` waits — for a fill slot, or for
/// credit — and the file is filled directly meanwhile. The request must
/// find it filled and answer at once, untouched. Before the fix it filled
/// the file again without looking; with a source that can no longer serve
/// it, the failed re-fetch rolled back — demoting a hydrated file and
/// punching it — which in the VM, where an opener had meanwhile had the
/// file ignore-marked, gave the next reader 65 536 zero bytes.
#[tokio::test]
async fn a_request_for_a_file_filled_meanwhile_is_answered_without_touching_it() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
    let waiting = fd.try_clone().unwrap();
    let source = Arc::new(Faulty::new(LocalDir::new(remote.path())));
    assert!(source::hydrate_with(fd, source.as_ref(), None).await.is_ok(), "filled directly");
    std::fs::remove_file(remote.path().join("ITEM")).unwrap();
    let fetched = source.fetches();

    let answer = serve_one(waiting, Arc::clone(&source) as Arc<dyn ContentSource>).await;

    let path = local.path().join("file.bin");
    assert_eq!(answer, (5, 0), "a file that is already there is answered success");
    assert_eq!(source.fetches(), fetched, "and it is not fetched again");
    assert_eq!(std::fs::read(&path).unwrap(), vec![4u8; 4096], "nor emptied");
    assert_eq!(
        read_state(&std::fs::File::open(&path).unwrap()).unwrap(),
        Some(State::Hydrated),
        "nor demoted"
    );
}

/// The same stale request when the re-fetch would *succeed*: the file was
/// edited in place after it was filled, and there is no upload in this
/// sub-project, so that edit is the only copy (§8). Before the fix the
/// request overwrote it with the remote content and reported success.
#[tokio::test]
async fn a_request_never_overwrites_a_hydrated_file_edited_in_place() {
    use std::os::unix::fs::FileExt;
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
    let waiting = fd.try_clone().unwrap();
    let source = Arc::new(Faulty::new(LocalDir::new(remote.path())));
    assert!(source::hydrate_with(fd, source.as_ref(), None).await.is_ok());
    let path = local.path().join("file.bin");
    std::fs::File::options().write(true).open(&path).unwrap().write_all_at(b"EDITED", 0).unwrap();
    let fetched = source.fetches();

    let answer = serve_one(waiting, Arc::clone(&source) as Arc<dyn ContentSource>).await;

    assert_eq!(answer, (5, 0), "the file is there, so the opener is let through to it");
    assert_eq!(source.fetches(), fetched, "without fetching anything");
    assert_eq!(&std::fs::read(&path).unwrap()[..6], b"EDITED", "and the edit is kept");
}

/// A content source that records, on its first fetch, whether the fake
/// helper had already been asked to `ClearIgnore`.
struct ClearedFirst {
    dir: std::path::PathBuf,
    seen: Arc<std::sync::Mutex<Vec<Seen>>>,
    fetches: Arc<std::sync::Mutex<Vec<bool>>>,
}

#[async_trait]
impl ContentSource for ClearedFirst {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let cleared = self.seen.lock().unwrap().contains(&Seen::ClearIgnore);
        self.fetches.lock().unwrap().push(cleared);
        LocalDir::new(self.dir.clone()).fetch(item_id, from, end).await
    }
}

/// A file a cancelled `Dehydrate` left `dehydrating` may still carry its
/// ignore mark (the call stopped between the state write and its
/// `ClearIgnore`), and a fill that fails punches the file. So a fill of a
/// file in any state but `online-only` makes `hydrating` durable and then
/// has the mark cleared, **before the first byte is fetched** — the
/// question small round 3's punch enumeration should have asked: can this
/// file carry a mark placed after the last clear?
#[tokio::test]
async fn a_file_left_dehydrating_is_refilled_only_after_its_ignore_mark_is_cleared() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![8u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
    let path = local.path().join("file.bin");
    konedrive_fs::placeholder::write_state(&std::fs::File::open(&path).unwrap(), State::Dehydrating)
        .unwrap();
    let fetches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let source = Arc::new(ClearedFirst {
        dir: remote.path().to_path_buf(),
        seen: helper.log(),
        fetches: Arc::clone(&fetches),
    });

    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 9, fd }).await.unwrap();
    wait_until("the request is answered", || helper.seen().contains(&Seen::HydrateDone)).await;

    assert_eq!(
        *fetches.lock().unwrap(),
        vec![true],
        "the one fetch must come after the ignore mark was cleared"
    );
    assert_eq!(std::fs::read(&path).unwrap(), vec![8u8; 4096]);
}

/// And when the mark cannot be cleared, nothing is fetched and nothing is
/// touched: the file keeps its content and the state it was found in.
#[tokio::test]
async fn a_refill_whose_ignore_mark_cannot_be_cleared_touches_nothing() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    helper.refuse(Seen::ClearIgnore, libc::EIO);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![8u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
    let path = local.path().join("file.bin");
    std::fs::write(&path, vec![3u8; 4096]).unwrap();
    konedrive_fs::placeholder::write_state(&std::fs::File::open(&path).unwrap(), State::Dehydrating)
        .unwrap();
    let source = Arc::new(Faulty::new(LocalDir::new(remote.path())));

    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, Arc::clone(&source) as Arc<dyn ContentSource>, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 9, fd }).await.unwrap();
    wait_until("the request is answered", || helper.seen().contains(&Seen::HydrateDone)).await;

    assert_eq!(source.fetches(), 0, "nothing may be fetched into a file that may be ignored");
    assert_eq!(std::fs::read(&path).unwrap(), vec![3u8; 4096], "nor may it be emptied");
    assert_eq!(
        read_state(&std::fs::File::open(&path).unwrap()).unwrap(),
        Some(State::Dehydrating),
        "and it keeps the state it was found in"
    );
}

/// The credit contract's true worst case. Four fills have finished and sent `HydrateDone`, and
/// hold their fill slots until the `Ack`s come back; the helper has
/// counted those four requests answered and sent its whole credit of new
/// requests — and the four `Ack`s are queued on the socket *behind* them.
/// The reader thread must take every one of those requests to reach the
/// `Ack`s: the request loop holds one while it waits for a slot and the
/// request queue the rest, so the queue must be at least
/// `MAX_OUTSTANDING_HYDRATIONS - 1` deep. One shallower and nothing moves
/// again: the fills wait for `Ack`s the reader never reaches, and the
/// reader waits for a queue the fills never drain.
///
/// The test above keeps its fills running, so their requests still hold
/// credit and fewer new ones can be in flight; it passed with the queue
/// cut to 59. This one fails at 62 and passes at 63 (measured by
/// mutating the queue depth in `helper/mod.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reader_reaches_acks_queued_behind_every_request_the_helper_may_send() {
    const MAX: usize = konedrive_proto::MAX_OUTSTANDING_HYDRATIONS;
    const SLOTS: usize = 4;
    let files = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let requests: Vec<OwnedFd> = (0..SLOTS + MAX)
        .map(|i| {
            std::fs::write(source_dir.path().join(format!("f{i}")), [7u8; 16]).unwrap();
            placeholder(files.path(), &format!("f{i}"), &format!("f{i}"), 16)
        })
        .collect();

    let sockets = tempfile::tempdir().unwrap();
    let path = sockets.path().join("helper.sock");
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .unwrap();
    bind(fd.as_raw_fd(), &UnixAddr::new(&path).unwrap()).unwrap();
    sock_listen(&fd, Backlog::new(4).unwrap()).unwrap();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<u64>();
    std::thread::spawn(move || {
        let listener: OwnedFd = fd;
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this thread now solely owns.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let _hello = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        let send = |channel: &mut Channel, req_id: usize| {
            channel
                .send(&ToDaemon::HydrateRequest { req_id: req_id as u64 }, Some(requests[req_id].as_fd()))
                .unwrap();
        };
        // One request per fill slot, and their `HydrateDone`s read but
        // not yet acknowledged: four fills now hold their slots.
        for req_id in 0..SLOTS {
            send(&mut channel, req_id);
        }
        for _ in 0..SLOTS {
            let (done, _) = channel.recv::<ToHelper>().unwrap();
            let ToHelper::HydrateDone { req_id, .. } = done else { panic!("{done:?}") };
            let _ = done_tx.send(req_id);
        }
        // Those four answered, the whole credit is free again: a
        // helper sends that many more before the four `Ack`s.
        for req_id in SLOTS..SLOTS + MAX {
            send(&mut channel, req_id);
        }
        for _ in 0..SLOTS {
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        }
        while let Ok((message, _)) = channel.recv::<ToHelper>() {
            if let ToHelper::HydrateDone { req_id, .. } = message {
                let _ = done_tx.send(req_id);
            }
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });

    let (link, incoming) = HelperLink::connect(&path).await.unwrap();
    let source = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations(link, incoming, source, InodeLocks::new()));

    let mut answered = 0;
    while answered < SLOTS + MAX {
        let next = tokio::time::timeout(Duration::from_secs(5), done_rx.recv()).await;
        assert!(
            matches!(next, Ok(Some(_))),
            "after {answered} HydrateDone(s) nothing more came: the daemon stopped reading \
             before the Acks queued behind {MAX} requests, and its fills wait for them"
        );
        answered += 1;
    }
}

/// A fill stopped because OneDrive removed its file answers its
/// opener like every other fill: with an errno the kernel delivers. `ENOENT`
/// is not one (`ACCEPTED_DENY_ERRNOS`); the helper turns it into `EIO`, so the
/// opener is never told the file is gone.
#[tokio::test]
async fn a_fill_stopped_by_a_removal_answers_an_errno_the_kernel_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![1u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
    let key = InodeKey::of_fd(&fd).unwrap();

    // The fill parks in `fetch`, holding the inode's lock.
    let source = Arc::new(Faulty::new(LocalDir::new(remote.path())).delay(Duration::from_secs(3600)));
    let locks = InodeLocks::new();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, Arc::clone(&source) as Arc<dyn ContentSource>, locks.clone()));
    tx.send(HydrateRequest { req_id: 31, fd }).await.unwrap();
    wait_until("the fill is fetching", || source.fetches() == 1).await;

    assert!(locks.cancel(key), "the fill holds the lock of the file being removed");

    let (req_id, errno) = tokio::time::timeout(Duration::from_secs(10), seen.recv())
        .await
        .expect("a stopped fill must still answer the suspended open")
        .expect("the helper connection must stay up");
    assert_eq!(req_id, 31);
    assert!(
        konedrive_proto::ACCEPTED_DENY_ERRNOS.contains(&errno),
        "the opener of a removed file is answered errno {errno}, which the kernel does not \
         accept in a FAN_DENY response; the helper denies with EIO instead"
    );
}

/// The per-inode lock table, from the interception side: two suspended opens of
/// one inode must not be filled at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_inode_is_filled_one_fill_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let first = placeholder(local.path(), "file.bin", "ITEM", 4096);
    // A second descriptor for the very same inode, exactly as two
    // suspended opens of one file would arrive.
    let second = std::fs::File::options()
        .read(true)
        .write(true)
        .open(local.path().join("file.bin"))
        .unwrap()
        .into();

    let (source, peak) = CountingSource::new(remote.path(), Duration::from_millis(300));
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 1, fd: first }).await.unwrap();
    tx.send(HydrateRequest { req_id: 2, fd: second }).await.unwrap();

    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("both fills must answer")
            .unwrap();
    }
    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "two suspended opens of one inode were filled at once"
    );
}

/// The other half of that, and the reason the key has to be the inode
/// rather than anything coarser: two *different* files must still be
/// filled at the same time. A lock that over-matches turns four
/// concurrent hydrations into a queue of one, which no other test here
/// would notice. The source lets neither fill go until both have begun, so
/// fills taken one after the other never end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_different_inodes_are_filled_at_the_same_time() {
    /// A directory whose fetches each wait until two are under way.
    struct Together {
        dir: std::path::PathBuf,
        both: tokio::sync::Barrier,
        met: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl ContentSource for Together {
        async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
            use std::sync::atomic::Ordering::SeqCst;
            // Only the first fetch of each fill waits: later ranges go straight through.
            if !self.met.load(SeqCst) {
                self.both.wait().await;
                self.met.store(true, SeqCst);
            }
            LocalDir::new(self.dir.clone()).fetch(item_id, from, end).await
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let one = placeholder(local.path(), "one.bin", "ITEM", 4096);
    let another = placeholder(local.path(), "another.bin", "ITEM", 4096);

    let source = Arc::new(Together {
        dir: remote.path().to_path_buf(),
        both: tokio::sync::Barrier::new(2),
        met: std::sync::atomic::AtomicBool::new(false),
    });
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 1, fd: one }).await.unwrap();
    tx.send(HydrateRequest { req_id: 2, fd: another }).await.unwrap();

    for _ in 0..2 {
        let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("two unrelated files were filled one after the other: the lock matches more than the inode it is supposed to")
            .unwrap();
        assert_eq!(answered.1, 0);
    }
}

/// What a fill on open records (item 8): the helper's request, filled through
/// `serve_hydrations_reporting`, is a `downloaded` event under the name the file has, with
/// its size — sent after the opener is answered; one that fails is a `failed` event; and a
/// full disk reads exactly "not enough disk space", the words the window's notifier turns
/// into "disk full".
#[tokio::test]
async fn a_fill_on_open_is_recorded_as_downloaded_or_failed() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("ITEM"), vec![3u8; 4096]).unwrap();
    // Events are kept only for the folder registered now.
    let report = Report::new(SyncStateHandle::new(SyncSnapshot {
        folder: crate::status::snapshot::FolderStatus { root_path: folder.display().to_string(), ..Default::default() },
        ..SyncSnapshot::default()
    }));
    let mut added = report.activity.subscribe();
    let socket_path = folder.join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));

    // (the file, the item it carries, the errno the opener gets, the event's kind and detail)
    let cases = [("opened.bin", "ITEM", 0, "downloaded", Some("4.0 KiB")), ("gone.bin", "GONE", libc::EIO, "failed", None)];
    for (n, (name, item, errno, kind, detail)) in cases.into_iter().enumerate() {
        let fd = placeholder(&folder, name, item, 4096);
        tx.send(HydrateRequest { req_id: n as u64, fd }).await.unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv()).await.unwrap().unwrap();
        assert_eq!(answered, (n as u64, errno), "{name}");
        let event = tokio::time::timeout(Duration::from_secs(10), added.recv()).await.unwrap().unwrap();
        let shown = folder.join(name).display().to_string();
        assert_eq!((event.kind.as_str(), event.path.as_str()), (kind, shown.as_str()));
        if let Some(detail) = detail {
            assert_eq!(event.detail, detail);
        }
    }

    for errno in [libc::ENOSPC, libc::EDQUOT] {
        let event = fill_event(&Answered::Failed(FillError::Errno(errno)), "/r/f.bin", None).unwrap();
        assert_eq!((event.kind.as_str(), event.detail.as_str()), ("failed", crate::status::activity::NO_DISK_SPACE));
    }
}

/// Item 6: a fill gives its slot back before it records what it did. The
/// log is held still here, so every recording waits: with four slots
/// held by fills that are only recording, a fifth request was never
/// filled at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fill_lets_go_of_its_slot_before_it_records() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let report = Report::new(SyncStateHandle::new(SyncSnapshot {
        folder: crate::status::snapshot::FolderStatus { root_path: folder.display().to_string(), ..Default::default() },
        ..SyncSnapshot::default()
    }));
    let socket_path = folder.join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(8);
    let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));

    // The log is kept in a store, and the store is held, as a write under way holds it.
    let store = konedrive_tree::Store::new(konedrive_tree::TreeStore::in_memory().unwrap());
    let (attached, at) = (store.clone(), folder.clone());
    let attaching = report.clone();
    tokio::task::spawn_blocking(move || attaching.activity.attach(attached, &at)).await.unwrap();
    let (taken, held_now) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holding = std::thread::spawn(move || {
        store
            .call_blocking(move |_| {
                let _ = taken.send(());
                let _ = released.recv();
                Ok(())
            })
            .unwrap();
    });
    held_now.recv().unwrap();
    for n in 0..5u64 {
        std::fs::write(source_dir.path().join(format!("ITEM{n}")), vec![1u8; 1024]).unwrap();
        let fd = placeholder(&folder, &format!("f{n}.bin"), &format!("ITEM{n}"), 1024);
        tx.send(HydrateRequest { req_id: n, fd }).await.unwrap();
    }
    for _ in 0..5 {
        let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("a request waited for a slot held by a fill that was only recording");
        assert_eq!(answered.unwrap().1, 0);
    }
    drop(release);
    holding.join().unwrap();
}

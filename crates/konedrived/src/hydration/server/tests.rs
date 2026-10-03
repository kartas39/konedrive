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
use crate::helper::{HelperLink, HydrateRequest};
use crate::sync::tests::{CountingSource, FakeHelper, Seen, fake_helper, placeholder, wait_until};
use crate::folder::locks::InodeLocks;
use crate::hydration::source;
use super::*;

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
    let source = Arc::new(LocalDir::new(remote.path()).delay(Duration::from_secs(3600)));
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

/// The daemon's half of `MAX_OUTSTANDING_HYDRATIONS`, in its easier form:
/// every fill slot taken by a fill that will not finish during the test,
/// the rest of the helper's credit queued — the reader thread must still
/// get as far as the `Ack` the helper queued behind them. (Its fills
/// still hold credit, so this is not the worst case; the test below is.) If it stops short (a request queue shallower than
/// the contract), that `Ack` is never read: here a call hangs, and in
/// the real burst every fill waiting for its own `HydrateDone`'s `Ack`
/// hangs with it until the daemon's call timeout ends the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_is_answered_while_every_request_the_helper_may_send_is_in_flight() {
    const MAX: usize = konedrive_proto::MAX_OUTSTANDING_HYDRATIONS;
    let files = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let requests: Vec<OwnedFd> = (0..MAX)
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
    std::thread::spawn(move || {
        let listener: OwnedFd = fd;
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this thread now solely owns.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let _hello = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        // Everything the helper may have outstanding, all at once, ahead
        // of whatever the daemon asks next — the order a burst produces.
        for (req_id, request) in requests.iter().enumerate() {
            channel
                .send(&ToDaemon::HydrateRequest { req_id: req_id as u64 }, Some(request.as_fd()))
                .unwrap();
        }
        while channel.recv::<ToHelper>().is_ok() {
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });

    let (link, incoming) = HelperLink::connect(&path).await.unwrap();
    // A minute per fetch: every fill slot stays taken for the whole test.
    let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_secs(60));
    tokio::spawn(serve_hydrations(link.clone(), incoming, source, InodeLocks::new()));

    let dir = std::fs::File::open(files.path()).unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(5), link.mark_dir(&dir)).await;
    assert!(
        matches!(answered, Ok(Ok(()))),
        "with {MAX} hydrations in flight the daemon stopped reading before the Ack queued \
         behind them: {answered:?}"
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
    let source = Arc::new(LocalDir::new(remote.path()));
    assert_eq!(source::hydrate(fd, source.as_ref()).await, 0, "filled directly");
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
    let source = Arc::new(LocalDir::new(remote.path()));
    assert_eq!(source::hydrate(fd, source.as_ref()).await, 0);
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
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
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
        seen: Arc::clone(&helper.seen),
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
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
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
    let source = Arc::new(LocalDir::new(remote.path()));

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
/// mutating the queue depth in `helper.rs`).
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

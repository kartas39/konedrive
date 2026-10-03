use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use async_trait::async_trait;
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{
    accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType,
    UnixAddr,
};
use tokio::sync::mpsc;

use super::source::{ContentSource, Fetched, LocalDir, SourceError};
use super::*;

/// Where a service persists its folder: the one account of the
/// `config.toml` at `file` (added when there is none), in a store opened
/// from the file — as each start opens it. A file that cannot be read
/// makes a store that refuses every write, as the daemon's does.
pub(super) fn persist(file: &Path) -> Persist {
    assert_eq!(file.file_name().and_then(|n| n.to_str()), Some("config.toml"));
    let paths = crate::config::Paths::in_dir(file.parent().unwrap());
    // `open` awaits nothing but the wallet check, which here is ready.
    let opening = std::pin::pin!(ConfigStore::open(&paths, async { false }));
    let std::task::Poll::Ready(store) =
        opening.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    else {
        unreachable!("ConfigStore::open waited")
    };
    let account = match store.snapshot().accounts.first() {
        Some(account) => account.id.clone(),
        None => store.add_account("Personal").map(|a| a.id).unwrap_or_else(|_| "0123456789ab".into()),
    };
    Persist { store: Arc::new(store), account }
}

/// What `config.toml` records of the account's folder, in the words of
/// version 1's file that these tests were first written in. No folder
/// reads as version 1's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Config {
    pub sync_root: String,
    pub sync_root_id: String,
    pub sync_root_intercepted: bool,
    pub sync_root_source: String,
    pub sync_root_baloo_excluded: bool,
    pub sync_root_upgrade_when_helper: Option<bool>,
}

impl Config {
    pub(super) fn load(file: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let config: crate::config::Config = toml::from_str(&text).map_err(|e| e.to_string())?;
        Ok(match config.accounts.first().and_then(|a| a.root.clone()) {
            Some(root) => Config {
                sync_root: root.path.display().to_string(),
                sync_root_id: root.id,
                sync_root_intercepted: root.intercepted,
                sync_root_source: root.source,
                sync_root_baloo_excluded: root.baloo_excluded,
                sync_root_upgrade_when_helper: root.upgrade_when_helper,
            },
            None => Config {
                sync_root: String::new(),
                sync_root_id: String::new(),
                sync_root_intercepted: true,
                sync_root_source: "local".into(),
                sync_root_baloo_excluded: false,
                sync_root_upgrade_when_helper: None,
            },
        })
    }
}

/// Writes a `config.toml` whose one account's folder is `root`, as a
/// daemon that knew less wrote it.
fn write_config(file: &Path, root: &str) {
    let text = format!("config_version = 2\n\n[[accounts]]\nid = \"0123456789ab\"\nlabel = \"Personal\"\n\n[accounts.root]\n{root}");
    std::fs::write(file, text).unwrap();
}

/// A stand-in helper: accepts one connection, greets, acknowledges the
/// handshake `Hello`, then acknowledges everything and reports every
/// `HydrateDone` it sees. The listener is bound on the caller's thread,
/// before this returns, so `connect` cannot race `bind`.
///
/// It reports through a `tokio` channel rather than a `std` one because
/// the tests below wait for it *inside* the runtime: a blocking
/// `recv_timeout` on a current-thread runtime would park the one thread
/// that has to run `serve_hydrations`.
fn fake_helper(path: std::path::PathBuf) -> mpsc::UnboundedReceiver<(u64, i32)> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .unwrap();
    let addr = UnixAddr::new(&path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let listener: OwnedFd = fd;
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this process now solely owns.
        let stream = unsafe { UnixStream::from_raw_fd(accepted) };
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        let (hello, _) = channel.recv::<ToHelper>().unwrap();
        assert!(
            matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION),
            "{hello:?}"
        );
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
            if let ToHelper::HydrateDone { req_id, errno } = message {
                let _ = tx.send((req_id, errno));
            }
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });
    rx
}

fn placeholder(dir: &std::path::Path, name: &str, item_id: &str, size: u64) -> OwnedFd {
    let handle = std::fs::File::open(dir).unwrap();
    konedrive_fs::placeholder::create_placeholder(
        &handle,
        name,
        item_id,
        size,
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    )
    .unwrap();
    std::fs::File::options()
        .read(true)
        .write(true)
        .open(dir.join(name))
        .unwrap()
        .as_fd()
        .try_clone_to_owned()
        .unwrap()
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

/// `Hydrate()` asks the same question: in an intercepted root it clears the
/// mark before refilling a file that may carry one, and with the helper
/// gone it refuses rather than fill a file it could then have to punch.
#[tokio::test]
async fn hydrate_now_clears_the_ignore_mark_before_refilling_a_file_that_may_carry_one() {
    let (service, root_dir, _source_dir, _sockets, helper) =
        populated_service(&vec![5u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    let dehydrating = || {
        let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
        konedrive_fs::placeholder::write_state(&handle, State::Dehydrating).unwrap();
    };

    dehydrating();
    let link = service.link();
    service.set_link(None);
    let refused = service.hydrate_now(&file).await;
    assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
    assert_eq!(service.item_state(&file).await, "dehydrating", "and nothing changed");

    service.set_link(link);
    helper.forget();
    service.hydrate_now(&file).await.unwrap();
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark is cleared first");
    assert_eq!(std::fs::read(&file).unwrap(), vec![5u8; 4096]);
    assert_eq!(service.item_state(&file).await, "hydrated");
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

// --- InodeLocks (Ruling: serialization) -----------

/// A file's `(dev, ino)`, the way every caller of `InodeLocks` gets one.
fn key_of(path: &std::path::Path) -> InodeKey {
    InodeKey::of(&std::fs::File::open(path).unwrap()).unwrap()
}

/// The property a path key cannot have: two names for one
/// inode are one key, and two different files are two keys.
#[test]
fn a_key_names_the_inode_and_not_the_name_it_was_reached_by() {
    let dir = tempfile::tempdir().unwrap();
    let one = dir.path().join("one.bin");
    let another = dir.path().join("another.bin");
    std::fs::write(&one, b"x").unwrap();
    std::fs::write(&another, b"x").unwrap();
    let link = dir.path().join("link.bin");
    std::fs::hard_link(&one, &link).unwrap();
    let renamed = dir.path().join("renamed.bin");

    assert_eq!(key_of(&one), key_of(&link), "a hard link is the same inode");
    assert_ne!(key_of(&one), key_of(&another), "two files are two inodes");
    let before = key_of(&one);
    std::fs::rename(&one, &renamed).unwrap();
    assert_eq!(before, key_of(&renamed), "a rename changes no inode");
}

/// The lock has two constructors for one key — `of` on the
/// `SyncService` side (`hydrate_now`, `dehydrate`) and `of_fd` on the
/// interception side (`serve_hydrations`, which must not consume the
/// event fd) — and serialization across the two sides holds only while
/// they compute the same key. Drop `st_dev` from one, or offset the inode
/// in the other, and an intercepted fill races a `Dehydrate` of the same
/// file (C1's cross-side form) with every other test green.
#[test]
fn both_constructors_give_one_file_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    let other = dir.path().join("g.bin");
    std::fs::write(&path, b"x").unwrap();
    std::fs::write(&other, b"y").unwrap();

    // What `serve_hydrations` is handed: a bare descriptor.
    let event_fd: OwnedFd = std::fs::File::open(&path).unwrap().into();
    let by_fd = InodeKey::of_fd(&event_fd).unwrap();
    // What `hydrate_now` and `dehydrate` open for themselves.
    let by_file = InodeKey::of(&std::fs::File::open(&path).unwrap()).unwrap();

    assert_eq!(
        by_fd, by_file,
        "the interception side and the service side must lock the same key for one file"
    );
    assert_ne!(
        InodeKey::of_fd(std::fs::File::open(&other).unwrap()).unwrap(),
        by_file,
        "and a different file must still be a different key"
    );
}

/// The core property: a second waiter on the *same* key does not run
/// until the first holder's guard drops. Measured by ordering, not by
/// timing alone — `order` only ever gets `"b"` pushed onto it after
/// `"a-still-holding"`, which can only happen if `locks.lock` really
/// blocked task B for the whole time A held its guard.
#[tokio::test]
async fn the_second_waiter_on_the_same_key_does_not_run_until_the_first_releases() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    let key = key_of(&dir.path().join("f"));
    let order: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let guard_a = locks.lock(key).await;

    let order_b = Arc::clone(&order);
    let locks_b = locks.clone();
    let key_b = key;
    let waiter = tokio::spawn(async move {
        let _guard_b = locks_b.lock(key_b).await;
        order_b.lock().unwrap().push("b");
    });

    // Give the waiter every chance to (wrongly) run before A releases.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    order.lock().unwrap().push("a-still-holding");

    drop(guard_a);
    waiter.await.unwrap();

    assert_eq!(
        *order.lock().unwrap(),
        vec!["a-still-holding", "b"],
        "the second waiter must not enter until the first guard is dropped"
    );
}

/// The other half: this is a per-key lock, not a single global one — an
/// unrelated file must never wait on this one's holder.
#[tokio::test]
async fn a_different_key_is_not_blocked_by_an_unrelated_one() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"x").unwrap();
    std::fs::write(dir.path().join("b"), b"x").unwrap();
    let _held = locks.lock(key_of(&dir.path().join("a"))).await;

    let other = tokio::time::timeout(
        Duration::from_millis(200),
        locks.lock(key_of(&dir.path().join("b"))),
    )
    .await;
    assert!(other.is_ok(), "an unrelated key must not block on this one's holder");
}

/// The table must not grow without bound: once the only guard for a key
/// is dropped, that key's row is gone, not merely unlocked.
#[tokio::test]
async fn a_releasing_key_is_removed_from_the_table() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let guard = locks.lock(key_of(&dir.path().join("x"))).await;
    assert_eq!(locks.tracked(), 1, "the key must be tracked while held");
    drop(guard);
    assert_eq!(
        locks.tracked(),
        0,
        "a key with no more holders or waiters must not stay in the table forever"
    );
}

/// `try_lock` is refused while the key is held, leaves no
/// row behind when refused, and once granted excludes `lock` like any
/// other holder.
#[tokio::test]
async fn try_lock_is_refused_while_held_and_leaves_nothing_behind() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let key = key_of(&dir.path().join("x"));

    let held = locks.lock(key).await;
    assert!(locks.try_lock(key).is_none(), "granted while another holder has it");
    assert_eq!(locks.users(key), 1, "a refused try_lock left itself in the count");
    drop(held);
    assert_eq!(locks.tracked(), 0);

    let taken = locks.try_lock(key).expect("free, so granted");
    let waiter = tokio::time::timeout(Duration::from_millis(100), locks.lock(key)).await;
    assert!(waiter.is_err(), "lock() got in while try_lock held the key");
    drop(taken);
    assert_eq!(locks.tracked(), 0, "rows left behind");
}

/// The other direction, and the one the previous bookkeeping got wrong:
/// a row must **survive** while somebody else still needs it. Dropping
/// it there would hand the next caller a brand-new mutex for an inode
/// another task is already working on — two fills of one file, which is
/// exactly what this table exists to prevent, arrived at through the
/// cleanup rather than through the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_stays_while_another_caller_is_still_using_it() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let key = key_of(&dir.path().join("x"));

    let held_by_b = Arc::new(tokio::sync::Notify::new());
    let guard_a = locks.lock(key).await;
    let locks_b = locks.clone();
    let told = Arc::clone(&held_by_b);
    let b = tokio::spawn(async move {
        let _guard = locks_b.lock(key).await;
        told.notify_one();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    // Wait until B is genuinely counted as waiting. Spinning on
    // `yield_now` and hoping does not do it: on a multi-threaded runtime
    // B may not have reached `lock` at all when A releases, and then the
    // cleanup this test is about never has two callers to choose
    // between — the over-eager version passes exactly as the correct one
    // does. (Measured: with the yield-only version, the "drop the row
    // while another caller holds it" mutant survived.)
    wait_until("the second caller is waiting", || locks.users(key) == 2).await;
    drop(guard_a);
    held_by_b.notified().await;

    // B holds it now. A third caller must wait for B — which it can only
    // do if A's release left B's row in the table.
    let third = tokio::time::timeout(Duration::from_millis(300), locks.lock(key)).await;
    assert!(
        third.is_err(),
        "a third caller entered while another still held the same inode: the row was \
         dropped from the table while it was in use"
    );
    b.abort();
}

/// A waiter whose future is dropped — a D-Bus method whose caller went
/// away, a `select!` that lost — must not leave its row behind. Measured
/// Issue #104: a stop reaches the fill that holds the lock (and one
/// already waiting), never one that comes for the lock after it, even
/// while the slot is still in use.
#[tokio::test]
async fn a_stop_does_not_reach_a_fill_that_starts_after_it() {
    let locks = InodeLocks::new();
    let key = InodeKey { dev: 1, ino: 2 };
    let holder = locks.lock(key).await;
    assert!(locks.cancel(key));
    tokio::time::timeout(Duration::from_secs(1), holder.cancelled()).await.expect("the holder is told");
    let later = {
        let locks = locks.clone();
        tokio::spawn(async move {
            let guard = locks.lock(key).await;
            tokio::time::timeout(Duration::from_millis(200), guard.cancelled()).await.is_err()
        })
    };
    tokio::task::yield_now().await;
    drop(holder);
    assert!(later.await.unwrap(), "a fill that came after the stop is not stopped");
}

/// before the fix: holder releases, parked waiter is cancelled, one row
/// stays in the table forever.
#[tokio::test]
async fn a_cancelled_waiter_leaves_no_row_behind() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let key = key_of(&dir.path().join("x"));

    let guard = locks.lock(key).await;
    let locks_b = locks.clone();
    let waiter = tokio::spawn(async move {
        let _guard = locks_b.lock(key).await;
    });
    wait_until("the waiter is parked", || locks.users(key) == 2).await;
    waiter.abort();
    // Awaiting the handle after `abort` is what guarantees the task's
    // future has actually been dropped, not merely told to stop.
    assert!(waiter.await.unwrap_err().is_cancelled());
    drop(guard);

    assert_eq!(
        locks.tracked(),
        0,
        "a cancelled waiter left its row in the table: the table grows by one row per \
         cancelled call, forever"
    );
}

// --- SyncService -------------------------------------------------------

/// What a fake helper was asked to do, in the order it was asked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Seen {
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
    fn kind(&self) -> Seen {
        match self {
            Seen::MarkDir { .. } => Seen::MarkDir { entries: 0 },
            other => other.clone(),
        }
    }
}

/// A fake helper that records what it was asked to do and can be cut off
/// on demand: greets, acknowledges `Hello`, and acknowledges everything
/// after that. It accepts connection after connection, so a daemon that
/// reconnects finds it still there.
struct FakeHelper {
    seen: Arc<std::sync::Mutex<Vec<Seen>>>,
    /// Requests answered with an errno instead of 0, by kind — the
    /// `Seen` a request is recorded as. Anything absent is acknowledged.
    refusals: Arc<std::sync::Mutex<HashMap<Seen, i32>>>,
    /// A duplicate of the live connection's socket, so a test can cut it
    /// the way a helper that died would.
    live: Arc<std::sync::Mutex<Option<UnixStream>>>,
}

impl FakeHelper {
    /// Starts one on `path`. `register_root_delay` holds the ack for
    /// `RegisterRoot` open, which is the only window a test has to
    /// interfere between a registration and the recovery that follows.
    fn start(path: std::path::PathBuf, register_root_delay: Duration) -> Self {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        let addr = UnixAddr::new(&path).unwrap();
        bind(fd.as_raw_fd(), &addr).unwrap();
        sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let refusals: Arc<std::sync::Mutex<HashMap<Seen, i32>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let live: Arc<std::sync::Mutex<Option<UnixStream>>> =
            Arc::new(std::sync::Mutex::new(None));
        let recorded = Arc::clone(&seen);
        let refusing = Arc::clone(&refusals);
        let current = Arc::clone(&live);
        std::thread::spawn(move || {
            let listener: OwnedFd = fd;
            while let Ok(accepted) = accept(listener.as_raw_fd()) {
                // SAFETY: `accept` just returned a freshly opened
                // descriptor that this process now solely owns.
                let stream = unsafe { UnixStream::from_raw_fd(accepted) };
                *current.lock().unwrap() = stream.try_clone().ok();
                let Ok(mut channel) = Channel::new(stream) else { continue };
                if channel
                    .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                    .is_err()
                {
                    continue;
                }
                while let Ok((message, fd)) = channel.recv::<ToHelper>() {
                    let note = match &message {
                        ToHelper::Hello { .. } | ToHelper::UnmarkDir | ToHelper::OpenByHandle { .. } => None,
                        ToHelper::RegisterRoot { .. } => Some(Seen::RegisterRoot),
                        ToHelper::UnregisterRoot { .. } => Some(Seen::UnregisterRoot),
                        ToHelper::MarkDir => {
                            Some(Seen::MarkDir { entries: entries_of(fd.as_ref()) })
                        }
                        ToHelper::MarkFile => Some(Seen::MarkFile),
                        ToHelper::ClearIgnore => Some(Seen::ClearIgnore),
                        ToHelper::HydrateDone { .. } => Some(Seen::HydrateDone),
                    };
                    // Kinds are compared without `MarkDir`'s entry count.
                    let errno = note
                        .as_ref()
                        .map(Seen::kind)
                        .and_then(|kind| refusing.lock().unwrap().get(&kind).copied())
                        .unwrap_or(0);
                    if let Some(note) = note {
                        recorded.lock().unwrap().push(note);
                    }
                    if matches!(message, ToHelper::RegisterRoot { .. }) {
                        std::thread::sleep(register_root_delay);
                    }
                    if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                        break;
                    }
                }
            }
        });
        Self { seen, refusals, live }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// From now on, answers every request of `kind` with `errno`.
    fn refuse(&self, kind: Seen, errno: i32) {
        self.refusals.lock().unwrap().insert(kind.kind(), errno);
    }

    fn forget(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// Sends the daemon a hydration request for `fd` on the live
    /// connection, as the helper does for an intercepted open. A second
    /// `Channel` on the same socket is safe: every send is one datagram.
    fn send_request(&self, req_id: u64, fd: &OwnedFd) {
        let live = self.live.lock().unwrap();
        let stream = live.as_ref().expect("a live connection").try_clone().unwrap();
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::HydrateRequest { req_id }, Some(fd.as_fd())).unwrap();
    }

    /// Cuts the live connection, the way a helper that crashed would.
    fn hang_up(&self) {
        if let Some(stream) = self.live.lock().unwrap().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// How many entries a directory holds, through a descriptor rather than
/// a name.
fn entries_of(fd: Option<&OwnedFd>) -> usize {
    let Some(fd) = fd else { return usize::MAX };
    std::fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .map(|entries| entries.count())
        .unwrap_or(usize::MAX)
}

async fn service_with_helper() -> (Arc<SyncService>, tempfile::TempDir, FakeHelper) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    (SyncService::new(Some(link), None, None), sockets, helper)
}

/// `hydrate_now` must never report success on a file it did not fill —
/// the "never serve zeros" property, exercised directly rather than only
/// through the round trip in `sync_dbus.rs`.
#[tokio::test]
async fn hydrate_now_actually_fills_the_placeholder_with_the_sources_bytes() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("f.bin"), vec![9u8; 2048]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    let target = root_dir.path().join("f.bin");

    service.hydrate_now(&target).await.unwrap();

    assert_eq!(service.item_state(&target).await, "hydrated");
    assert_eq!(std::fs::read(&target).unwrap(), vec![9u8; 2048]);
}

// --- What a download or a free-up reports -----------------

/// The newest events first, as (kind, path, detail), oldest first.
async fn activity_of(service: &SyncService) -> Vec<(String, String, String)> {
    let mut events = service.recent_activity(200).await.unwrap();
    events.reverse();
    events.into_iter().map(|e| (e.kind, e.path, e.detail)).collect()
}

/// `Hydrate` finishing is a `downloaded` event with the
/// file's size; one that fails is a `failed` event saying why.
#[tokio::test]
async fn hydrate_is_recorded_as_downloaded_and_a_failed_one_as_failed() {
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 2048]).await;
    let target = root_dir.path().join("f.bin");
    service.hydrate_now(&target).await.unwrap();
    // A placeholder whose item the source does not have.
    drop(placeholder(root_dir.path(), "gone.bin", "gone.bin", 100));
    let gone = root_dir.path().join("gone.bin");
    assert!(service.hydrate_now(&gone).await.is_err());

    let shown = |path: &Path| path.display().to_string();
    assert_eq!(
        activity_of(&service).await,
        vec![
            ("downloaded".to_owned(), shown(&target), "2.0 KiB".to_owned()),
            ("failed".to_owned(), shown(&gone), "it could not be downloaded".to_owned()),
        ]
    );
}

/// For a fill on open: the helper's request, filled through
/// `serve_hydrations_reporting`, is a `downloaded` event under the name
/// the file has — sent after the opener is answered.
#[tokio::test]
async fn a_fill_on_open_is_recorded_as_downloaded() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("ITEM"), vec![3u8; 4096]).unwrap();
    let fd = placeholder(&folder, "opened.bin", "ITEM", 4096);
    // Events are kept only for the folder registered now.
    let report = Report::new(SyncStateHandle::new(SyncSnapshot {
        root_path: folder.display().to_string(),
        ..SyncSnapshot::default()
    }));
    let mut added = report.activity.subscribe();

    let socket_path = folder.join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));
    tx.send(HydrateRequest { req_id: 9, fd }).await.unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv()).await.unwrap().unwrap();
    assert_eq!(answered, (9, 0));

    let event = tokio::time::timeout(Duration::from_secs(10), added.recv()).await.unwrap().unwrap();
    let opened = folder.join("opened.bin").display().to_string();
    assert_eq!((event.kind.as_str(), event.path.as_str(), event.detail.as_str()), ("downloaded", opened.as_str(), "4.0 KiB"));
}

/// A source whose one stream is the reading end of a pipe the test
/// writes into: how a test holds a download half-way.
struct Piped {
    reader: std::sync::Mutex<Option<tokio::io::DuplexStream>>,
    size: u64,
}

#[async_trait]
impl ContentSource for Piped {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let stream = self.reader.lock().unwrap().take().ok_or_else(|| SourceError::NotFound("fetched twice".into()))?;
        Ok(Fetched {
            served_from: from,
            size: self.size,
            mtime: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
            version: None,
            stream: Box::new(stream),
        })
    }
}

/// A source that answers "not found" once it is let go.
struct Gated(std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>);

#[async_trait]
impl ContentSource for Gated {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let gate = self.0.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        Err(SourceError::NotFound("not there".into()))
    }
}

/// `Transfers`: a download is listed, with how far it has
/// got, for as long as it runs — and not a moment after, whether it
/// finished or failed.
#[tokio::test]
async fn a_download_shows_in_transfers_until_it_ends_however_it_ends() {
    use tokio::io::AsyncWriteExt;
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 64 * 1024]).await;
    let mut transfers = service.report().transfers.subscribe();
    let target = root_dir.path().join("f.bin");
    let shown = target.display().to_string();
    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    install_source(&service, Arc::new(Piped { reader: std::sync::Mutex::new(Some(reader)), size: 64 * 1024 }));
    let filling = {
        let (service, target) = (Arc::clone(&service), target.clone());
        tokio::spawn(async move { service.hydrate_now(&target).await })
    };
    writer.write_all(&[9u8; 16 * 1024]).await.unwrap();
    let halfway = |all: &std::collections::BTreeMap<u64, activity::Transfer>| {
        all.values().any(|t| t.path == shown && (t.done, t.total) == (16 * 1024, 64 * 1024))
    };
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(halfway)).await.unwrap().unwrap();
    writer.write_all(&[9u8; 48 * 1024]).await.unwrap();
    drop(writer);
    filling.await.unwrap().unwrap();
    assert_eq!(service.transfers(), Vec::new(), "a finished download is not listed");

    drop(placeholder(root_dir.path(), "g.bin", "g.bin", 100));
    let failing_target = root_dir.path().join("g.bin");
    let failing_shown = failing_target.display().to_string();
    let (open, gate) = tokio::sync::oneshot::channel();
    install_source(&service, Arc::new(Gated(std::sync::Mutex::new(Some(gate)))));
    let failing = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.hydrate_now(&failing_target).await })
    };
    let listed = |all: &std::collections::BTreeMap<u64, activity::Transfer>| all.values().any(|t| t.path == failing_shown);
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(listed)).await.unwrap().unwrap();
    open.send(()).unwrap();
    assert!(failing.await.unwrap().is_err());
    assert_eq!(service.transfers(), Vec::new(), "a failed download is not listed either");
}

/// A service with a folder registered without interception and no
/// helper anywhere, filled from `files` (name, size), each downloaded
/// when `hydrated` says so.
async fn local_folder(files: &[(&str, usize, bool)]) -> (Arc<SyncService>, tempfile::TempDir, tempfile::TempDir) {
    let service = SyncService::new(None, None, None);
    let dir = tempfile::tempdir().unwrap();
    service.set_helper_socket(dir.path().join("no-helper.sock"));
    let source_dir = dir.path().join("source");
    std::fs::create_dir(&source_dir).unwrap();
    for (name, size, _) in files {
        std::fs::write(source_dir.join(name), vec![5u8; *size]).unwrap();
    }
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(&source_dir).await.unwrap();
    for (name, _, hydrated) in files {
        if *hydrated {
            service.hydrate_now(&root_dir.path().join(name)).await.unwrap();
        }
    }
    (service, root_dir, dir)
}

/// `FreeUpSpace`: every downloaded file freed up through the
/// per-file path, except one that is open — counted as busy, left as it
/// is, and no error. The bytes are the blocks given back, as `stat`
/// reads them before and after: how many a 64 KiB file takes is the
/// filesystem's own business (btrfs gives it 64 KiB, the runner's ext4
/// 68), so the activity is checked against that, not a fixed figure.
/// And a Forget of the folder takes its activity with it.
#[tokio::test]
async fn free_up_space_frees_what_is_not_in_use_and_counts_what_is() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true), ("b.bin", 64 * 1024, true)]).await;
    let (a, b) = (root_dir.path().join("a.bin"), root_dir.path().join("b.bin"));
    let before = data_blocks(&a);
    let _in_use = std::fs::File::open(&b).unwrap();

    let freed = service.free_up_space().await.unwrap();

    let given_back = (before - data_blocks(&a)) * 512;
    assert_eq!(freed, FreedUp { files: 1, bytes: given_back, busy: 1, modified: 0, pinned: 0 });
    assert!(freed.bytes >= 64 * 1024, "{freed:?}");
    assert_eq!(service.item_state(&a).await, "online-only");
    assert_eq!(service.item_state(&b).await, "hydrated", "an open file is left as it is");
    let folder = root_dir.path().display().to_string();
    let detail = format!("1 file, {}", activity::human_size(given_back));
    assert_eq!(activity_of(&service).await.pop().unwrap(), ("freed".to_owned(), folder, detail));

    service.unregister_root().await.unwrap();
    assert!(activity_of(&service).await.is_empty(), "a Forget drops the activity");
}

/// `LocalBytes`, for a folder with a placeholder and a
/// downloaded file: what the downloaded file takes (and the placeholder
/// its next to nothing), measured on its own after the download. On the
/// paused clock, so the five seconds between two walks cost nothing.
#[tokio::test(start_paused = true)]
async fn local_bytes_are_what_the_downloaded_file_takes() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true), ("b.bin", 64 * 1024, false)]).await;
    let (a, b) = (root_dir.path().join("a.bin"), root_dir.path().join("b.bin"));
    assert!(data_blocks(&b) < 8, "b.bin is a placeholder");
    let expected = (data_blocks(&a) + data_blocks(&b)) * 512;
    assert!(expected >= 64 * 1024);
    let mut state = service.state().subscribe();
    tokio::time::timeout(Duration::from_secs(60), state.wait_for(|s| s.local_bytes == expected))
        .await
        .unwrap_or_else(|_| panic!("LocalBytes stayed {}, not {expected}", service.status().1))
        .unwrap();
}

// --- Activity and space accounting corner cases ---------------------------

/// Item 4: a download that ends after its folder was forgotten records
/// nothing in the folder registered next — `Hydrate` takes no lifecycle
/// lock, so a Forget does not wait for it.
#[tokio::test]
async fn a_download_that_ends_after_its_folder_is_forgotten_is_not_in_the_next_ones_activity() {
    let (service, root_a, _dir) = local_folder(&[("a.bin", 4096, false)]).await;
    let (open, gate) = tokio::sync::oneshot::channel();
    install_source(&service, Arc::new(Gated(std::sync::Mutex::new(Some(gate)))));
    let mut transfers = service.report().transfers.subscribe();
    let filling = {
        let (service, a) = (Arc::clone(&service), root_a.path().join("a.bin"));
        tokio::spawn(async move { service.hydrate_now(&a).await })
    };
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| !all.is_empty())).await.unwrap().unwrap();

    service.unregister_root().await.unwrap();
    let root_b = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_b.path()).await.unwrap();
    open.send(()).unwrap();
    assert!(filling.await.unwrap().is_err());

    assert_eq!(activity_of(&service).await, Vec::new(), "folder A's download is not folder B's activity");
}

/// Item 5: the walker measuring `LocalBytes` ends with the service —
/// it held the state, so nothing waiting on it ever saw the end.
#[tokio::test(start_paused = true)]
async fn dropping_the_service_ends_its_walker() {
    let (service, _root, _dir) = local_folder(&[("a.bin", 4096, true)]).await;
    assert!(service.report().space.running(), "the registration started it");
    let mut state = service.state().subscribe();
    drop(service);
    let ended = tokio::time::timeout(Duration::from_secs(60), async { while state.changed().await.is_ok() {} }).await;
    assert!(ended.is_ok(), "something still holds the state: the walker");
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
        root_path: folder.display().to_string(),
        ..SyncSnapshot::default()
    }));
    let socket_path = folder.join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(8);
    let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));

    let held = report.activity.hold();
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
    drop(held);
}

/// Item 8: a file a download or another free-up holds the per-inode lock
/// of is busy, not waited for.
#[tokio::test]
async fn free_up_space_counts_a_file_whose_lock_is_taken_as_busy() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true)]).await;
    let a = root_dir.path().join("a.bin");
    let key = InodeKey::of(&std::fs::File::open(&a).unwrap()).unwrap();
    // The descriptor is closed again: only the lock stands in the way.
    let _held = service.locks().lock(key).await;
    let freed = tokio::time::timeout(Duration::from_secs(10), service.free_up_space())
        .await
        .expect("it waited for the lock")
        .unwrap();
    assert_eq!(freed, FreedUp { files: 0, bytes: 0, busy: 1, modified: 0, pinned: 0 });
    assert_eq!(service.item_state(&a).await, "hydrated");
}

/// Item 8: a downloaded file changed here is neither freed nor busy:
/// it is left, as `Dehydrate` would leave it.
#[tokio::test]
async fn free_up_space_counts_a_file_changed_here_in_neither() {
    let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true)]).await;
    let a = root_dir.path().join("a.bin");
    std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&a).unwrap(), b"mine").unwrap();
    let freed = service.free_up_space().await.unwrap();
    assert_eq!(freed, FreedUp { files: 0, bytes: 0, busy: 0, modified: 1, pinned: 0 });
    assert!(std::fs::read(&a).unwrap().ends_with(b"mine"), "the change is kept");
}

// --- "Always keep on this device" -----------------------------------------

/// A folder registered without interception, with no helper anywhere,
/// filled with `docs/a.bin`, `docs/b.bin` and `c.bin`, 64 KiB each, none
/// of them downloaded. Returns the folder's path as registered.
async fn folder_to_pin() -> (Arc<SyncService>, PathBuf, tempfile::TempDir, tempfile::TempDir) {
    let service = SyncService::new(None, None, None);
    let dir = tempfile::tempdir().unwrap();
    service.set_helper_socket(dir.path().join("no-helper.sock"));
    let source = dir.path().join("source");
    std::fs::create_dir_all(source.join("docs")).unwrap();
    for name in ["docs/a.bin", "docs/b.bin", "c.bin"] {
        std::fs::write(source.join(name), vec![3u8; 64 * 1024]).unwrap();
    }
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(&source).await.unwrap();
    (service, root_dir.path().canonicalize().unwrap(), root_dir, dir)
}

/// Waits until nothing a pin asked for is pending or downloading: each
/// download recorded, since a file leaves the queue only after that.
async fn pinned_downloads_done(service: &SyncService) {
    for _ in 0..1000 {
        if service.pins.queued().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("still queued: {:?}", service.pins.queued());
}

fn pin_of(path: &Path) -> Option<Vec<u8>> {
    xattr::get(path, konedrive_fs::placeholder::XATTR_PIN).unwrap()
}

/// Records the range every fetch asks for.
struct RecordsRanges {
    inner: LocalDir,
    asked: Mutex<Vec<(String, Option<u64>)>>,
}

#[async_trait]
impl ContentSource for RecordsRanges {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        self.asked.lock().unwrap().push((item_id.to_string(), end));
        self.inner.fetch(item_id, from, end).await
    }
}

/// Issue #28: a large file being opened (`Hydrate`) keeps one stream, open-ended; the
/// same kind of file pinned downloads in parts, each asking for a bounded range.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_file_being_opened_keeps_one_stream_and_a_pinned_one_goes_in_parts() {
    let service = SyncService::new(None, None, None);
    let dir = tempfile::tempdir().unwrap();
    service.set_helper_socket(dir.path().join("no-helper.sock"));
    let source = dir.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    // Placeholders as large as a large file (sparse, nothing on disk)...
    for name in ["opened.bin", "pinned.bin"] {
        File::create(source.join(name)).unwrap().set_len(crate::pool::LARGE_FROM).unwrap();
    }
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(&source).await.unwrap();
    // ...whose content turns out small: the fill takes the source's size.
    for name in ["opened.bin", "pinned.bin"] {
        std::fs::write(source.join(name), vec![7u8; 300_000]).unwrap();
    }
    let recording = Arc::new(RecordsRanges { inner: LocalDir::new(&source), asked: Mutex::default() });
    install_source(&service, Arc::clone(&recording) as Arc<dyn ContentSource>);
    let root = root_dir.path().canonicalize().unwrap();

    service.hydrate_now(&root.join("opened.bin")).await.unwrap();
    service.pin(&[root.join("pinned.bin")]).await.unwrap();
    pinned_downloads_done(&service).await;

    for name in ["opened.bin", "pinned.bin"] {
        assert_eq!(std::fs::read(root.join(name)).unwrap(), vec![7u8; 300_000], "{name}");
    }
    let asked = recording.asked.lock().unwrap().clone();
    let ends = |name: &str| asked.iter().filter(|(id, _)| id == name).map(|(_, end)| *end).collect::<Vec<_>>();
    assert_eq!(ends("opened.bin"), vec![None], "a file being opened is not split");
    assert_eq!(ends("pinned.bin"), vec![Some(source::parts::PIECE)], "a pinned one asks for its first piece");
}

/// Pinning a folder queues every online-only file in it, and each is
/// downloaded through the ordinary fill, `downloaded` event and all.
/// What is outside the folder is left alone, and a file inside it is not
/// pinned again on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinning_a_folder_downloads_what_is_in_it() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let (a, b, c) = (root.join("docs/a.bin"), root.join("docs/b.bin"), root.join("c.bin"));

    assert_eq!(service.pin(&[root.join("docs")]).await.unwrap(), 2);
    pinned_downloads_done(&service).await;

    assert_eq!(std::fs::read(&a).unwrap(), vec![3u8; 64 * 1024]);
    assert_eq!(service.item_state(&b).await, "hydrated");
    assert_eq!(service.item_state(&c).await, "online-only");
    assert_eq!(pin_of(&root.join("docs")), Some(b"1".to_vec()));
    assert_eq!(service.pinned_count(), 1);
    let downloaded: Vec<String> =
        activity_of(&service).await.into_iter().filter(|(kind, ..)| kind == "downloaded").map(|(_, path, _)| path).collect();
    assert_eq!(downloaded.len(), 2, "{downloaded:?}");

    assert_eq!(service.pin(&[a.clone()]).await.unwrap(), 0);
    assert_eq!(pin_of(&a), None, "the folder pins it already");
    assert_eq!(service.pinned_count(), 1);
}

/// Free up space on a file a pinned folder keeps is refused, naming the
/// folder — through `FreeUp` and through `Dehydrate` alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeing_up_what_a_pinned_folder_keeps_is_refused_naming_the_folder() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let (docs, a) = (root.join("docs"), root.join("docs/a.bin"));
    service.pin(&[docs.clone()]).await.unwrap();
    pinned_downloads_done(&service).await;

    let refused = service.free_up(&[a.clone()]).await.unwrap_err();
    let expected = format!("{} is pinned by {}: unpin it first", a.display(), docs.display());
    assert!(matches!(&refused, SyncError::NotAllowed(why) if *why == expected), "{refused:?}");
    assert!(matches!(service.dehydrate(&a).await, Err(SyncError::NotAllowed(_))));
    assert!(matches!(service.unpin(&[a.clone()]).await, Err(SyncError::NotAllowed(_))));
    assert_eq!(service.item_state(&a).await, "hydrated");
    assert_eq!(pin_of(&docs), Some(b"1".to_vec()));

    // With the folder in the same call, whose pin that call takes off.
    let freed = service.free_up(&[docs.clone(), a.clone()]).await.unwrap();
    assert_eq!((freed.files, freed.pinned), (2, 0), "{freed:?}");
    assert_eq!((pin_of(&docs), service.pinned_count()), (None, 0));
}

/// Pins that came off before a later one could not stay off, and
/// `PinnedCount` says so; nothing is freed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_free_up_whose_later_pin_cannot_come_off_keeps_the_count_right() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let (docs, c) = (root.join("docs"), root.join("c.bin"));
    service.pin(&[docs.clone(), c.clone()]).await.unwrap();
    pinned_downloads_done(&service).await;
    fn fails_on_files(item: &File, on: bool) -> io::Result<()> {
        if item.metadata()?.is_file() {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        pin::set_pin(item, on)
    }

    assert!(matches!(service.free_up_with(&[docs.clone(), c.clone()], fails_on_files).await, Err(SyncError::Io(_))));

    assert_eq!((pin_of(&docs), pin_of(&c)), (None, Some(b"1".to_vec())));
    assert_eq!(service.pinned_count(), 1);
    assert_eq!(service.item_state(&root.join("docs/a.bin")).await, "hydrated", "nothing was freed");
}

/// A file queued while pinned whose pin is gone by its turn is not
/// downloaded.
#[tokio::test]
async fn a_queued_file_no_longer_pinned_is_not_downloaded() {
    use pin::PinFill;
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let a = root.join("docs/a.bin");

    assert_eq!(service.fill_pinned(&a).await, pin::Filled::Done);

    assert_eq!(service.item_state(&a).await, "online-only");
    assert!(activity_of(&service).await.is_empty(), "nothing was fetched");
}

/// Free up space on a pinned folder takes its pin off and frees what is
/// in it — but a file with a pin of its own stays, counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeing_up_a_pinned_folder_unpins_it_and_frees_all_but_a_pin_below() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let (docs, a, b) = (root.join("docs"), root.join("docs/a.bin"), root.join("docs/b.bin"));
    service.pin(&[b.clone()]).await.unwrap();
    service.pin(&[docs.clone()]).await.unwrap();
    pinned_downloads_done(&service).await;
    assert_eq!(service.pinned_count(), 2);

    let freed = service.free_up(&[docs.clone()]).await.unwrap();

    assert_eq!((freed.files, freed.busy, freed.pinned), (1, 0, 1), "{freed:?}");
    assert!(freed.bytes >= 64 * 1024, "{freed:?}");
    assert_eq!(service.item_state(&a).await, "online-only");
    assert_eq!(service.item_state(&b).await, "hydrated", "its own pin keeps it");
    assert_eq!((pin_of(&docs), pin_of(&b)), (None, Some(b"1".to_vec())));
    assert_eq!(service.pinned_count(), 1);
}

/// `FreeUpSpace` leaves every file a pin keeps, and counts them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn free_up_space_leaves_pinned_files_and_counts_them() {
    let (service, root, _root_dir, _dir) = folder_to_pin().await;
    let c = root.join("c.bin");
    service.pin(&[root.join("docs")]).await.unwrap();
    service.hydrate_now(&c).await.unwrap();
    pinned_downloads_done(&service).await;

    let freed = service.free_up_space().await.unwrap();

    assert_eq!((freed.files, freed.busy, freed.pinned), (1, 0, 2), "{freed:?}");
    assert_eq!(service.item_state(&c).await, "online-only");
    assert_eq!(service.item_state(&root.join("docs/a.bin")).await, "hydrated");
    assert_eq!(service.item_state(&root.join("docs/b.bin")).await, "hydrated");
}

/// Item 8: a fill on open that fails is a `failed` event, and a full
/// disk reads exactly "not enough disk space" — the words the window's
/// notifier turns into "disk full".
#[tokio::test]
async fn a_failed_fill_on_open_is_recorded_as_failed() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let fd = placeholder(&folder, "gone.bin", "GONE", 4096);
    let report = Report::new(SyncStateHandle::new(SyncSnapshot {
        root_path: folder.display().to_string(),
        ..SyncSnapshot::default()
    }));
    let mut added = report.activity.subscribe();
    let socket_path = folder.join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
    tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));
    tx.send(HydrateRequest { req_id: 3, fd }).await.unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv()).await.unwrap().unwrap();
    assert_eq!(answered, (3, libc::EIO));
    let event = tokio::time::timeout(Duration::from_secs(10), added.recv()).await.unwrap().unwrap();
    let gone = folder.join("gone.bin").display().to_string();
    assert_eq!((event.kind.as_str(), event.path.as_str()), ("failed", gone.as_str()));

    for errno in [libc::ENOSPC, libc::EDQUOT] {
        let event = fill_event(&Answered::Failed(FillError::Errno(errno)), "/r/f.bin", None).unwrap();
        assert_eq!((event.kind.as_str(), event.detail.as_str()), ("failed", activity::NO_DISK_SPACE));
    }
}

/// Item 8: a download whose caller goes away — a D-Bus call dropped, a
/// replacement stopped with its poller — leaves `Transfers` with it.
#[tokio::test]
async fn a_cancelled_download_leaves_transfers() {
    use tokio::io::AsyncWriteExt;
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 64 * 1024]).await;
    let mut transfers = service.report().transfers.subscribe();
    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    install_source(&service, Arc::new(Piped { reader: std::sync::Mutex::new(Some(reader)), size: 64 * 1024 }));
    let filling = {
        let (service, target) = (Arc::clone(&service), root_dir.path().join("f.bin"));
        tokio::spawn(async move { service.hydrate_now(&target).await })
    };
    writer.write_all(&[9u8; 16 * 1024]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| !all.is_empty())).await.unwrap().unwrap();
    filling.abort();
    tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| all.is_empty()))
        .await
        .expect("the cancelled download is still listed")
        .unwrap();
}

/// `populate_walk` walks a directory the *user* names — unlike
/// `root::recover`'s hardened, descriptor-based walk, it is explicitly
/// the offline test path, and its threat model does not include a
/// racing or adversarial filesystem. It does include an ordinary
/// mistake, though: a symlink somewhere in a source tree that points
/// back at one of its own ancestors. If the walk ever decided to
/// recurse on the strength of what a symlink points at, this would
/// never return. `tokio::time::timeout` is the backstop in case the fix
/// regresses; on a passing run it never comes close to firing.
#[tokio::test]
async fn populate_from_directory_does_not_follow_a_symlink_cycle_in_the_source() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("real.txt"), b"hello").unwrap();
    std::os::unix::fs::symlink(source_dir.path(), source_dir.path().join("loop")).unwrap();

    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    let created = tokio::time::timeout(
        Duration::from_secs(10),
        service.populate_from_directory(source_dir.path()),
    )
    .await
    .expect(
        "populate_from_directory did not return: a symlink cycle in the source was \
         descended",
    )
    .unwrap();

    assert_eq!(created, 1, "only the real file may produce a placeholder");
    assert!(
        !root_dir.path().join("loop").exists(),
        "a symlink to a directory must not be mirrored as one"
    );
}

/// The other half of the same fix: a symlink is never descended to
/// decide whether it is a directory, but a symlink to a *regular* file
/// is still worth a placeholder — the walk's read side, not its
/// recursion decision, follows it.
#[tokio::test]
async fn populate_from_directory_creates_a_placeholder_for_a_symlink_to_a_regular_file() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("real.bin"), vec![5u8; 4096]).unwrap();
    std::os::unix::fs::symlink(
        source_dir.path().join("real.bin"),
        source_dir.path().join("link.bin"),
    )
    .unwrap();

    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    let created = service.populate_from_directory(source_dir.path()).await.unwrap();

    assert_eq!(
        created, 2,
        "both the real file and the symlink to a regular file must be mirrored"
    );
    let placeholder = root_dir.path().join("link.bin");
    assert_eq!(
        std::fs::metadata(&placeholder).unwrap().len(),
        4096,
        "the placeholder must use the symlink's target's size"
    );
    assert_eq!(service.item_state(&placeholder).await, "online-only");
}

/// `item_state` must judge a file by the *currently* registered root,
/// not by whether the file happens to carry konedrive xattrs — a file
/// left behind by a root this daemon un-registered still carries them,
/// but it is not this root's business any more. This is the specific
/// claim `populate_from_directory_mirrors_the_tree_as_placeholders`
/// (in `tests/sync_dbus.rs`) does *not* actually pin: there, the
/// "outside the root" file has no konedrive xattrs at all, so it reads
/// `not-managed` even with the containment check deleted (`read_state`
/// returns `None` on its own). This test gives the file real, valid
/// xattrs, so only the containment check can produce `not-managed`.
#[tokio::test]
async fn item_state_of_a_file_outside_the_current_root_is_not_managed_even_with_real_xattrs() {
    let (service, _sockets, _helper) = service_with_helper().await;

    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("f.bin"), vec![3u8; 512]).unwrap();
    let root_a = tempfile::tempdir().unwrap();
    service.register_root(root_a.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    let left_behind = root_a.path().join("f.bin");
    assert_eq!(service.item_state(&left_behind).await, "online-only", "sanity check");

    service.unregister_root().await.unwrap();
    let root_b = tempfile::tempdir().unwrap();
    service.register_root(root_b.path()).await.unwrap();

    assert_eq!(
        service.item_state(&left_behind).await,
        "not-managed",
        "a real konedrive-managed file left behind by a different, no-longer-registered \
         root must not be reported as this root's own"
    );
}

#[tokio::test]
async fn a_file_recovery_finds_in_use_does_not_make_the_root_an_error() {
    // an interrupted file that something has open
    // — on reconnect, the suspended opener whose request is not served
    // yet, or a fill still running from the connection before
    // — refused recovery's lease and published `RootState = error`
    // with "could not reset", about a file that was then filled normally.
    let (service, _sockets, _helper) = service_with_helper().await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    let handle = std::fs::File::open(root_dir.path()).unwrap();
    konedrive_fs::placeholder::create_placeholder(
        &handle,
        "busy.bin",
        "ITEM",
        4096,
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    )
    .unwrap();
    let path = root_dir.path().join("busy.bin");
    let busy = std::fs::File::options().read(true).write(true).open(&path).unwrap();
    konedrive_fs::placeholder::write_state(&busy, State::Hydrating).unwrap();

    service.resume().await;

    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    // logged, not a `LastError` that outlives it.
    assert_eq!(service.last_error(), "");
    assert_eq!(service.item_state(&path).await, "hydrating", "and it is left as found");
}

/// A `RecoveryReport` with `failed > 0` is "silently
/// unrecoverable" case (a refused `ClearIgnore`) — it must not stay
/// silent: `RegisterRoot` still succeeds (the root itself is usable),
/// but `RootState`/`LastError` must say so.
#[tokio::test]
async fn a_failed_recovery_surfaces_through_root_state_and_last_error() {
    let (service, _sockets, helper) = service_with_helper().await;
    let root_dir = tempfile::tempdir().unwrap();

    // A folder that already carries a root id is exempt from
    // the "must be empty on first registration" check, which is exactly
    // what this test needs — the interrupted file below has to exist
    // *before* `register_root` runs, since recovery runs as part of it.
    // This mirrors what a restart after a crash actually looks like: the
    // folder was registered before, and this is the second registration.
    xattr::set(
        root_dir.path(),
        "user.konedrive.root",
        b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d",
    )
    .unwrap();

    // A file left `dehydrating` by a "crash", whose `ClearIgnore` the
    // helper refuses. (A file merely held open used to be the way to
    // force this; it is `busy` now, not a failure —
    // m11 — and has a test of its own above.)
    let path = root_dir.path().join("stuck.bin");
    std::fs::write(&path, vec![1u8; 4096]).unwrap();
    let file = std::fs::File::options().read(true).write(true).open(&path).unwrap();
    konedrive_fs::placeholder::write_state(&file, State::Dehydrating).unwrap();
    drop(file);
    helper.refuse(Seen::ClearIgnore, libc::EIO);

    service.register_root(root_dir.path()).await.unwrap();

    assert_eq!(service.root_state(), "error", "a recovery failure must not be silent");
    assert!(
        service.last_error().contains('1'),
        "the failure count must be in LastError: {}",
        service.last_error()
    );
}

// --- Per-inode serialization, measured through the service -----------

/// A source that reports the largest number of fetches that were ever in
/// flight at once, and holds each one open for `delay` so that a second
/// one has time to arrive.
struct CountingSource {
    dir: std::path::PathBuf,
    delay: Duration,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingSource {
    fn new(dir: &std::path::Path, delay: Duration) -> (Arc<Self>, Arc<std::sync::atomic::AtomicUsize>) {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = Arc::new(Self {
            dir: dir.to_path_buf(),
            delay,
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: Arc::clone(&peak),
        });
        (source, peak)
    }
}

#[async_trait]
impl ContentSource for CountingSource {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(now, SeqCst);
        tokio::time::sleep(self.delay).await;
        let fetched = LocalDir::new(self.dir.clone()).fetch(item_id, from, end).await;
        self.in_flight.fetch_sub(1, SeqCst);
        fetched
    }
}

fn install_source(service: &SyncService, source: Arc<dyn ContentSource>) {
    *service.source.lock().unwrap() = Some(source);
}

async fn wait_for_state(service: &SyncService, path: &std::path::Path, want: &str) {
    for _ in 0..400 {
        if service.item_state(path).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{} never reached state {want}", path.display());
}

async fn populated_service(
    bytes: &[u8],
) -> (Arc<SyncService>, tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, FakeHelper) {
    let (service, sockets, helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("f.bin"), bytes).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    (service, root_dir, source_dir, sockets, helper)
}

/// The whole of C1. Two names for one inode must serialize.
/// Measured the way the review measured it: a hard link, a source slow
/// enough for both fills to overlap, and a count of how many were ever
/// in flight at once. A path-keyed table gives two — and a failing
/// source then has one fill's roll-back (`online-only` + `punch_all`)
/// land on top of the other's committed `hydrated`, which is a file
/// labelled `hydrated` over a hole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_names_for_one_inode_are_never_filled_at_the_same_time() {
    let (service, root_dir, source_dir, _sockets, _helper) =
        populated_service(&vec![9u8; 2048]).await;
    let one = root_dir.path().join("f.bin");
    let another = root_dir.path().join("g.bin");
    std::fs::hard_link(&one, &another).unwrap();
    let (source, peak) = CountingSource::new(source_dir.path(), Duration::from_millis(300));
    install_source(&service, source);

    let first = {
        let service = Arc::clone(&service);
        let path = one.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    let second = {
        let service = Arc::clone(&service);
        let path = another.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "two fills were in flight on one inode: the lock is keyed by something other than \
         the inode, so two names for one file do not serialize"
    );
    assert_eq!(std::fs::read(&one).unwrap(), vec![9u8; 2048]);
}

/// The other pair names: "a hydration request for a file being
/// dehydrated runs after the dehydration finishes", and the reverse.
/// The discriminator is the *outcome*, not the timing: a dehydration
/// that runs while the fill is still in flight sees `state=hydrating`
/// and is refused, so a `Dehydrate` that succeeds is one that waited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dehydration_waits_for_the_fill_of_the_same_inode() {
    let (service, root_dir, source_dir, _sockets, _helper) =
        populated_service(&vec![7u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_millis(400));
    install_source(&service, source);

    let fill = {
        let service = Arc::clone(&service);
        let path = file.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    wait_for_state(&service, &file, "hydrating").await;

    service.dehydrate(&file).await.expect(
        "the dehydration ran while the fill was still in flight: it saw `hydrating` and \
         refused, so nothing serialized the two",
    );
    fill.await.unwrap().unwrap();

    use std::os::unix::fs::MetadataExt;
    assert_eq!(service.item_state(&file).await, "online-only");
    assert!(std::fs::metadata(&file).unwrap().blocks() < 8, "the content is gone");
}

/// The same table, from the interception side: two suspended opens of
/// one inode must not be filled at once either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_hydrations_fills_one_inode_one_fill_at_a_time() {
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
/// would notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_hydrations_fills_two_different_inodes_at_the_same_time() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("helper.sock");
    let mut seen = fake_helper(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let one = placeholder(local.path(), "one.bin", "ITEM", 4096);
    let another = placeholder(local.path(), "another.bin", "ITEM", 4096);

    let (source, peak) = CountingSource::new(remote.path(), Duration::from_millis(300));
    let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
    tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
    tx.send(HydrateRequest { req_id: 1, fd: one }).await.unwrap();
    tx.send(HydrateRequest { req_id: 2, fd: another }).await.unwrap();

    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("both fills must answer")
            .unwrap();
    }
    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "two unrelated files were filled one after the other: the lock matches more than \
         the inode it is supposed to"
    );
}

// --- The gate `hydrate_now` writes through -------------

/// A registration that has been removed or replaced no longer authorises
/// writing inside that folder. `dehydrate` checks this
/// — through `SyncRoot::open_inside`, which verifies the folder
/// still carries *this* root's id — and `hydrate_now` reached its target
/// through a `starts_with` on a canonicalized string, which cannot.
#[tokio::test]
async fn hydrate_now_refuses_a_root_that_no_longer_carries_its_registration() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![1u8; 2048]).await;
    let file = root_dir.path().join("f.bin");
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

    let error = service.hydrate_now(&file).await.unwrap_err();
    assert!(
        matches!(error, SyncError::OutsideRoot),
        "expected a refusal, got {error:?}"
    );
    use std::os::unix::fs::MetadataExt;
    assert!(
        std::fs::metadata(&file).unwrap().blocks() < 8,
        "nothing may be written through a registration that is gone"
    );
}

/// C3, as it was measured: the window is not a microsecond race. The
/// old order canonicalized the caller's path, checked *that string*
/// against the root, awaited the per-inode lock — which has no time
/// limit, because the fill it waits for has none — and then opened the
/// checked string by name. An ordinary directory rename inside the root
/// in that window sent the write outside the root, and `Hydrate`
/// reported success.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_swapped_while_a_hydration_waits_cannot_redirect_it() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let base = tempfile::tempdir().unwrap();
    let root_dir = base.path().join("root");
    let outside_dir = base.path().join("outside");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::create_dir(&outside_dir).unwrap();

    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("f.bin"), vec![5u8; 1024]).unwrap();
    service.register_root(&root_dir).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();

    // The file the measured probe emptied: outside the root, and
    // carrying exactly the xattrs that made the old code treat whatever
    // it opened as one of ours.
    let victim = outside_dir.join("f.bin");
    std::fs::write(&victim, b"the user's own data").unwrap();
    {
        let file = std::fs::File::options().read(true).write(true).open(&victim).unwrap();
        xattr::FileExt::set_xattr(&file, "user.konedrive.item-id", b"sub/f.bin").unwrap();
        konedrive_fs::placeholder::write_state(&file, State::OnlineOnly).unwrap();
    }

    let target = root_dir.join("sub").join("f.bin");
    let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_millis(400));
    install_source(&service, source);

    let first = {
        let service = Arc::clone(&service);
        let path = target.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    wait_for_state(&service, &target, "hydrating").await;
    let second = {
        let service = Arc::clone(&service);
        let path = target.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    // Long enough for the second call to be waiting, short enough to be
    // well inside the first fill's 400 ms.
    tokio::time::sleep(Duration::from_millis(60)).await;
    std::fs::rename(root_dir.join("sub"), base.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(&outside_dir, root_dir.join("sub")).unwrap();

    let _ = first.await.unwrap();
    let _ = second.await.unwrap();

    assert_eq!(
        std::fs::read(&victim).unwrap(),
        b"the user's own data",
        "a file outside the sync root was overwritten with hydration content"
    );
}

// --- `Hydrate` never reports success without the bytes ---------------

/// The plainest form of the rule: a file this daemon does not manage is
/// refused, not called done.
#[tokio::test]
async fn hydrate_now_refuses_a_file_with_no_konedrive_xattrs() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![1u8; 512]).await;
    let stray = root_dir.path().join("stray.txt");
    std::fs::write(&stray, b"not ours").unwrap();

    let error = service.hydrate_now(&stray).await.unwrap_err();
    assert!(matches!(error, SyncError::NotManaged), "expected a refusal, got {error:?}");
}

/// A source that cannot serve the item must not be reported as success:
/// the file is still a hole afterwards, and the caller was told so.
#[tokio::test]
async fn hydrate_now_reports_a_source_that_could_not_serve_the_file() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![2u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    // A source directory with nothing in it: every fetch is NotFound.
    let empty = tempfile::tempdir().unwrap();
    install_source(&service, Arc::new(LocalDir::new(empty.path())));

    let error = service.hydrate_now(&file).await.unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "expected a failure, got {error:?}");
    assert_eq!(
        service.item_state(&file).await,
        "online-only",
        "a failed fill must leave the file where the next open can retry it"
    );
    use std::os::unix::fs::MetadataExt;
    assert!(std::fs::metadata(&file).unwrap().blocks() < 8, "and holding nothing");
}

/// With no content source at all there is nowhere for the bytes to come
/// from, so there is nothing to report success about.
#[tokio::test]
async fn hydrate_now_refuses_when_no_content_source_is_registered() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    std::fs::write(root_dir.path().join("f.bin"), b"x").unwrap();

    let error = service.hydrate_now(&root_dir.path().join("f.bin")).await.unwrap_err();
    assert!(matches!(error, SyncError::NoSource), "expected a refusal, got {error:?}");
}

/// A file labelled `hydrated` over a hole is §9's named
/// failure, and a manual "download it now" is what repairs it — so
/// `Hydrate` must not believe the label on its own.
#[tokio::test]
async fn hydrate_now_fills_a_hydrated_label_that_has_no_stamp_behind_it() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![6u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    {
        let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
        konedrive_fs::placeholder::write_state(&handle, State::Hydrated).unwrap();
    }
    assert_eq!(service.item_state(&file).await, "hydrated", "the label says it is there");

    service.hydrate_now(&file).await.unwrap();

    assert_eq!(
        std::fs::read(&file).unwrap(),
        vec![6u8; 4096],
        "the file was labelled `hydrated` over a hole and `Hydrate` did nothing about it"
    );
}

/// The other side of the same check, and the reason it is a stamp
/// comparison rather than a size one: a `hydrated` file whose stamp does
/// **not** match was edited locally, and there is no upload in this
/// sub-project, so that edit is the only copy. Refuse loudly; never
/// overwrite it with remote content.
#[tokio::test]
async fn hydrate_now_refuses_a_hydrated_file_that_was_edited_locally() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![6u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();
    std::fs::write(&file, b"what the user typed").unwrap();

    let error = service.hydrate_now(&file).await.unwrap_err();
    assert!(
        matches!(error, SyncError::ModifiedLocally),
        "expected a refusal, got {error:?}"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"what the user typed",
        "a local edit is the only copy of that data and must not be overwritten"
    );
}

/// §5.2 treats `dehydrating` as "hydrate it again". A dehydration that a
/// crash — or a cancelled call — left half-done must not make `Hydrate`
/// report success over whatever the punch had got to by then.
#[tokio::test]
async fn hydrate_now_fills_a_file_a_dehydration_left_half_done() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![5u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    {
        let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
        konedrive_fs::placeholder::write_state(&handle, State::Dehydrating).unwrap();
    }

    service.hydrate_now(&file).await.unwrap();

    assert_eq!(std::fs::read(&file).unwrap(), vec![5u8; 4096]);
    assert_eq!(service.item_state(&file).await, "hydrated");
}

/// A zero-byte file is created
/// `hydrated` with no stamp (there is nothing to download), so freeing it
/// up answered `ModifiedLocally` and the command line told the user their
/// edits would be lost. It takes no space and there is nothing to free:
/// it succeeds and changes nothing.
#[tokio::test]
async fn freeing_up_a_zero_byte_file_succeeds_and_changes_nothing() {
    let (service, root_dir, _source_dir, _sockets, helper) = populated_service(&[]).await;
    let file = root_dir.path().join("f.bin");
    assert_eq!(service.item_state(&file).await, "hydrated");
    helper.forget();

    let freed = service.dehydrate(&file).await;

    assert!(freed.is_ok(), "{freed:?}");
    assert_eq!(service.item_state(&file).await, "hydrated");
    assert!(helper.seen().is_empty(), "nothing needed the helper: {:?}", helper.seen());
}

/// Freeing up a file under the read-only lock works and leaves it 0444.
/// The root is registered the way the test above has it; the file is
/// put in it by hand, hydrated and stamped, and then locked.
#[tokio::test]
async fn freeing_up_a_locked_file_works_and_leaves_it_locked() {
    use std::os::unix::fs::PermissionsExt;
    let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&[]).await;
    let root = service.root().unwrap();
    let path = root.path.join("locked.bin");
    std::fs::write(&path, vec![1u8; 8192]).unwrap();
    {
        let file = File::options().read(true).write(true).open(&path).unwrap();
        konedrive_fs::placeholder::write_item_id(&file, "L").unwrap();
        konedrive_fs::placeholder::write_state(&file, State::Hydrated).unwrap();
        konedrive_fs::placeholder::write_stamp(&file).unwrap();
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    service.dehydrate(&path).await.unwrap();
    assert_eq!(state_of_path(&path), Some(State::OnlineOnly));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444);
    drop(root_dir);
}

/// But a file that was *made* empty here is an edit like any other.
#[tokio::test]
async fn freeing_up_a_file_emptied_here_is_still_refused_as_modified() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![3u8; 2048]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();
    std::fs::File::options().write(true).open(&file).unwrap().set_len(0).unwrap();

    let freed = service.dehydrate(&file).await;

    assert!(matches!(freed, Err(SyncError::ModifiedLocally)), "{freed:?}");
}

/// A populate source inside the
/// root is a source whose files are placeholders: `LocalDir` reads them
/// through the daemon's own exemption, or with nothing intercepting at
/// all, so a fill copies zeros into a file it then stamps `hydrated` —
/// on the offline route the user will actually run. And a source that
/// contains the root is mirrored into itself. Both are refused, and
/// nothing is created.
#[tokio::test]
async fn a_populate_source_that_overlaps_the_root_is_refused() {
    let service = SyncService::new(None, None, None);
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    service.register_root_without_interception(&root).await.unwrap();
    let inside = root.join("source");
    std::fs::create_dir(&inside).unwrap();
    std::fs::write(inside.join("a.bin"), [1u8; 64]).unwrap();
    std::fs::write(outer.path().join("b.bin"), [2u8; 64]).unwrap();

    let from_inside = service.populate_from_directory(&inside).await;
    assert!(matches!(from_inside, Err(SyncError::Unsupported(_))), "{from_inside:?}");
    assert!(!root.join("a.bin").exists(), "nothing may be created from inside the root");

    let from_around = service.populate_from_directory(outer.path()).await;
    assert!(matches!(from_around, Err(SyncError::Unsupported(_))), "{from_around:?}");
    assert!(!root.join("b.bin").exists(), "nor from a directory containing it");
}

/// A root registered without interception, with one `online-only`
/// placeholder `b.bin` in it, populated from a source outside it.
async fn root_with_a_placeholder() -> (Arc<SyncService>, tempfile::TempDir, PathBuf, PathBuf) {
    let service = SyncService::new(None, None, None);
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    service.register_root_without_interception(&root).await.unwrap();
    let first = outer.path().join("first");
    std::fs::create_dir(&first).unwrap();
    std::fs::write(first.join("b.bin"), vec![7u8; 64 * 1024]).unwrap();
    service.populate_from_directory(&first).await.unwrap();
    let placeholder = root.join("b.bin");
    assert_eq!(service.item_state(&placeholder).await, "online-only", "sanity check");
    (service, outer, root, placeholder)
}

/// Whether `path` holds `hydrated` over nothing but zeros: the outcome
/// every guard in this module exists to prevent.
fn hydrated_over_zeros(path: &Path) -> bool {
    let file = File::open(path).unwrap();
    let hydrated = read_state(&file).unwrap() == Some(State::Hydrated);
    let content = std::fs::read(path).unwrap();
    hydrated && !content.is_empty() && content.iter().all(|&b| b == 0)
}

/// A source *directory* that overlaps the root is refused, but a source
/// *file* can still lead into it: a symlink in the source pointing at a
/// placeholder in the root, or a hardlink to one. `LocalDir` reads it
/// with nothing intercepting — or through the daemon's own exemption —
/// so a fill copies the placeholder's zeros into the file it fills, and
/// stamps it `hydrated`. Such a source is refused, and nothing is
/// created from it.
#[tokio::test]
async fn a_populate_source_file_that_leads_into_the_root_is_refused() {
    let (service, outer, root, placeholder) = root_with_a_placeholder().await;
    let second = outer.path().join("second");
    std::fs::create_dir(&second).unwrap();
    std::os::unix::fs::symlink(&placeholder, second.join("a.bin")).unwrap();

    let populated = service.populate_from_directory(&second).await;
    let mirrored = root.join("a.bin");
    let filled = if mirrored.exists() { Some(service.hydrate_now(&mirrored).await) } else { None };
    assert!(
        !(mirrored.exists() && hydrated_over_zeros(&mirrored)),
        "a symlink in the source to a placeholder in the root was filled with its zeros and \
         stamped hydrated (populate → {populated:?}, Hydrate → {filled:?})"
    );
    assert!(matches!(populated, Err(SyncError::Unsupported(_))), "{populated:?}");
    assert!(!mirrored.exists(), "nothing may be created from a source that leads into the root");

    let third = outer.path().join("third");
    std::fs::create_dir(&third).unwrap();
    std::fs::hard_link(&placeholder, third.join("h.bin")).unwrap();
    let populated = service.populate_from_directory(&third).await;
    let mirrored = root.join("h.bin");
    let filled = if mirrored.exists() { Some(service.hydrate_now(&mirrored).await) } else { None };
    assert!(
        !(mirrored.exists() && hydrated_over_zeros(&mirrored)),
        "a hardlink in the source to a placeholder was filled with its zeros and stamped \
         hydrated (populate → {populated:?}, Hydrate → {filled:?})"
    );
    assert!(matches!(populated, Err(SyncError::Unsupported(_))), "{populated:?}");
}

/// The same, decided again where the bytes are read: a
/// source file that pointed somewhere harmless when the folder was
/// populated and into the root by the time it is fetched is refused
/// there, and the file it would have filled stays `online-only`.
#[tokio::test]
async fn a_source_file_that_leads_into_the_root_by_the_time_it_is_fetched_is_refused() {
    let (service, outer, root, placeholder) = root_with_a_placeholder().await;
    let second = outer.path().join("second");
    std::fs::create_dir(&second).unwrap();
    let elsewhere = outer.path().join("elsewhere.bin");
    std::fs::write(&elsewhere, vec![9u8; 64 * 1024]).unwrap();
    std::os::unix::fs::symlink(&elsewhere, second.join("a.bin")).unwrap();
    service.populate_from_directory(&second).await.unwrap();
    let mirrored = root.join("a.bin");
    assert_eq!(service.item_state(&mirrored).await, "online-only", "sanity check");

    std::fs::remove_file(second.join("a.bin")).unwrap();
    std::os::unix::fs::symlink(&placeholder, second.join("a.bin")).unwrap();
    let filled = service.hydrate_now(&mirrored).await;

    assert!(
        !hydrated_over_zeros(&mirrored),
        "a source file that now leads into the root was read, and its zeros stamped \
         hydrated (Hydrate → {filled:?})"
    );
    assert!(filled.is_err(), "{filled:?}");
    assert_eq!(service.item_state(&mirrored).await, "online-only");
}

/// And the case that must stay a no-op: a file that really is there.
#[tokio::test]
async fn hydrate_now_does_nothing_to_a_file_that_is_already_there() {
    let (service, root_dir, source_dir, _sockets, _helper) =
        populated_service(&vec![3u8; 2048]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();

    let counted = Arc::new(LocalDir::new(source_dir.path()));
    install_source(&service, Arc::clone(&counted) as Arc<dyn ContentSource>);
    service.hydrate_now(&file).await.unwrap();
    assert_eq!(counted.fetches(), 0, "a complete file must not be fetched again");
}

// --- `ItemState` is a query ----------------------------

/// `ItemState` must answer without opening the file. In production the
/// reason is that an open of an `online-only` file under a marked
/// directory *is* a download — the whole file is fetched as a side
/// effect of asking what state it is in, and where nothing can serve it
/// the denial makes the open fail and the answer comes back
/// `not-managed` for a genuinely managed placeholder.
///
/// Unprivileged, interception cannot be reached at all, so the measured
/// property here is the open itself: a FIFO inside the folder answers
/// promptly if nothing opens it, and never answers at all if something
/// does — `open(O_RDONLY)` on a FIFO blocks until a writer arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn item_state_answers_without_opening_the_file() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![1u8; 512]).await;
    let pipe = root_dir.path().join("pipe");
    nix::unistd::mkfifo(&pipe, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

    let answer = tokio::time::timeout(Duration::from_secs(3), service.item_state(&pipe)).await;
    // Unblocks anything that did open it, so the runtime can shut down
    // even when this assertion fails. `O_NONBLOCK` on the write side
    // returns `ENXIO` when there is no reader, which is the passing case.
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&pipe);
    assert_eq!(
        answer.expect("ItemState opened the file and blocked on it"),
        "not-managed"
    );
    // ... and the ordinary answer still works.
    assert_eq!(service.item_state(&root_dir.path().join("f.bin")).await, "online-only");
}

// --- Invariant M1, through the helper's own eyes ---------------------

/// M1: "every directory under a root is marked, and a new directory is
/// marked before anything is created inside it". Both halves are
/// measured from the helper's side — it counts the entries in the very
/// descriptor it was handed — because that is the only place the order
/// is observable. A mark that arrives after the directory has been
/// filled reports a non-zero count; a directory that is never marked
/// reports nothing at all.
#[tokio::test]
async fn every_new_directory_is_marked_before_anything_is_created_in_it() {
    let (service, _sockets, helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source_dir.path().join("sub").join("deeper")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("a.bin"), vec![1u8; 64]).unwrap();
    std::fs::write(
        source_dir.path().join("sub").join("deeper").join("b.bin"),
        vec![2u8; 64],
    )
    .unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    helper.forget();

    service.populate_from_directory(source_dir.path()).await.unwrap();

    let marks: Vec<Seen> = helper
        .seen()
        .into_iter()
        .filter(|s| matches!(s, Seen::MarkDir { .. }))
        .collect();
    assert_eq!(marks.len(), 2, "both new directories must be marked: {marks:?}");
    assert!(
        marks.iter().all(|s| matches!(s, Seen::MarkDir { entries: 0 })),
        "a directory was marked after its contents were created: {marks:?}"
    );
}

/// The crash window the same code left open: a directory that exists but
/// was never marked — `create_dir` succeeded, `mark_dir` did not, the
/// daemon died — is skipped by every later run, so it stays unmarked and
/// everything under it stays uninterceptable. Marking is idempotent;
/// skipping is not recoverable.
#[tokio::test]
async fn a_directory_that_already_exists_is_marked_again() {
    let (service, _sockets, helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("a.bin"), vec![1u8; 64]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    // Exactly what a crash between `create_dir` and `mark_dir` leaves.
    std::fs::create_dir(root_dir.path().join("sub")).unwrap();
    helper.forget();

    service.populate_from_directory(source_dir.path()).await.unwrap();

    assert!(
        helper.seen().iter().any(|s| matches!(s, Seen::MarkDir { .. })),
        "a directory left behind unmarked by a crash was never marked: {:?}",
        helper.seen()
    );
}

/// The item id is the path relative to the source root, which is what
/// the content source resolves a fetch by. A bare file name gives every
/// nested placeholder an id that fetches nothing — and the failure only
/// shows up later, at the one moment the user is waiting for their file.
#[tokio::test]
async fn a_nested_placeholder_carries_an_item_id_that_can_fetch_it() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![8u8; 1024]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();

    let nested = root_dir.path().join("sub").join("b.bin");
    assert_eq!(
        xattr::get(&nested, "user.konedrive.item-id").unwrap().unwrap(),
        b"sub/b.bin",
        "the id must name the file inside the source, not just its last component"
    );
    service.hydrate_now(&nested).await.unwrap();
    assert_eq!(std::fs::read(&nested).unwrap(), vec![8u8; 1024]);
}

// --- Who may register, and what a failed registration leaves ---------

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..600 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened");
}

async fn service_with_account(
    account: StateHandle,
) -> (Arc<SyncService>, tempfile::TempDir, FakeHelper) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    (SyncService::new(Some(link), Some(account), None), sockets, helper)
}

/// §3.1 refuses a registration "when nobody is signed in", which nothing
/// checked: both interfaces live on the same object, and this is what
/// wires the one to the other.
#[tokio::test]
async fn register_root_is_refused_while_nobody_is_signed_in() {
    let account = StateHandle::new(crate::state::AccountSnapshot::default());
    let (service, _sockets, _helper) = service_with_account(account.clone()).await;
    let root_dir = tempfile::tempdir().unwrap();

    let error = service.register_root(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::NotSignedIn), "{error:?}");
    assert!(
        xattr::get(root_dir.path(), "user.konedrive.root").unwrap().is_none(),
        "a refused registration must not have stamped the folder"
    );

    account.update(|s| s.state = SignInState::SignedIn);
    service.register_root(root_dir.path()).await.unwrap();
}

/// §3.1 refuses a registration that "overlaps another root". A second
/// one used to be accepted and to replace the first silently: the first
/// stayed registered with the helper — still marked, still walked — and
/// `ItemState` started calling its files `not-managed`.
#[tokio::test]
async fn register_root_refuses_a_second_root() {
    let (service, _sockets, helper) = service_with_helper().await;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    service.register_root(first.path()).await.unwrap();
    helper.forget();

    let error = service.register_root(second.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    assert_eq!(
        service.root().unwrap().path,
        std::fs::canonicalize(first.path()).unwrap(),
        "the first root must still be the root"
    );
    assert!(
        helper.seen().is_empty(),
        "the helper was told about a root the daemon refused: {:?}",
        helper.seen()
    );
    assert!(
        xattr::get(second.path(), "user.konedrive.root").unwrap().is_none(),
        "and the refused folder must not have been stamped"
    );
}

/// A `RegisterRoot` that fails must leave nothing behind —
/// no stored root, no published `RootPath` — or, now that a second root
/// is refused, one failed call would make every retry answer "already
/// registered". Measured through the only window a test has: the helper
/// holds its `RegisterRoot` ack open, and the folder's registration is
/// taken away while it does, so the recovery that follows fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_whose_recovery_fails_leaves_no_root_behind() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = FakeHelper::start(socket_path.clone(), Duration::from_millis(400));
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let service = SyncService::new(Some(link), None, None);
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

    let error = registering.await.unwrap().unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_none(), "a failed registration stored a root anyway");
    assert_eq!(service.state().get().root_path, "", "and published it");
    assert_eq!(service.root_state(), "error");
    // The retry a user would make next must not be refused.
    service.register_root(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "ready");
}

// --- Registering without interception ------------------

/// The default stays fail-closed, and for the reason that outranks
/// everything else here: no helper means no interception, and a
/// placeholder nobody intercepts reads as zeros.
#[tokio::test]
async fn register_root_is_refused_without_a_helper() {
    let service = SyncService::new(None, None, None);
    let root_dir = tempfile::tempdir().unwrap();

    let error = service.register_root(root_dir.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    assert_eq!(service.root_state(), "none");
}

/// ...and the whole surface works in the mode that says so out loud.
/// Before this, the helper-optionality already written into
/// `populate_walk`, `unregister_root` and `hydrate_now` was unreachable
/// dead code, because `RegisterRoot` gated all of it.
#[tokio::test]
async fn the_whole_flow_works_without_a_helper_when_it_is_asked_for_explicitly() {
    let service = SyncService::new(None, None, None);
    // No helper anywhere — not only no link — so a free-up goes ahead
    //, whatever this machine has at the real socket path.
    let no_helper = tempfile::tempdir().unwrap();
    service.set_helper_socket(no_helper.path().join("helper.sock"));
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![4u8; 4096]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();

    service.register_root_without_interception(root_dir.path()).await.unwrap();

    assert_eq!(service.root_state(), "no-interception");
    assert!(
        service.last_error().contains("read as zeros"),
        "the one thing a user must not have to infer: {}",
        service.last_error()
    );

    assert_eq!(service.populate_from_directory(source_dir.path()).await.unwrap(), 1);
    let file = root_dir.path().join("sub").join("b.bin");
    assert_eq!(service.item_state(&file).await, "online-only");
    service.hydrate_now(&file).await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), vec![4u8; 4096]);
    assert_eq!(service.item_state(&file).await, "hydrated");
    use std::os::unix::fs::MetadataExt;
    let hydrated = std::fs::metadata(&file).unwrap().blocks();
    service.dehydrate(&file).await.unwrap();
    assert_eq!(service.item_state(&file).await, "online-only");

    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.len(), 4096, "the size survives");
    // Every block of the 4 KiB of content is given back (8 sectors of
    // 512 bytes). Whatever else the file holds — ext4 counts an external
    // xattr block in `st_blocks`, btrfs does not — stays and is not the
    // content.
    assert!(meta.blocks() + 8 <= hydrated, "the content is gone: {hydrated} -> {} blocks", meta.blocks());
}

// --- Startup and the helper supervisor ----------

/// §3.1's "persisted, so it survives a restart" — and §4.4's walk, which
/// without it never ran at a startup at all: recovery only ever ran
/// inside a `RegisterRoot` call, so after a crash a file left
/// `hydrating` stayed that way until a human registered the folder
/// again.
///
/// order is pinned here too, for the first time: the helper
/// records what it was asked, and the registration must come before the
/// `ClearIgnore` recovery sends for the interrupted file. Both fake
/// helpers used to discard everything they received, so nothing could
/// tell the two orders apart.
#[tokio::test]
async fn a_persisted_root_comes_back_at_the_next_start_and_is_recovered_after_it() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    let resolved = std::fs::canonicalize(root_dir.path()).unwrap();

    {
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let service = SyncService::new(Some(link), None, Some(persist(&config_file)));
        service.register_root(root_dir.path()).await.unwrap();
    }
    assert_eq!(
        Config::load(&config_file).unwrap().sync_root,
        resolved.display().to_string(),
        "the root must be written down where the next start can find it"
    );

    // What a crash during a hydration leaves behind.
    let stuck = root_dir.path().join("stuck.bin");
    std::fs::write(&stuck, vec![1u8; 4096]).unwrap();
    {
        let file = std::fs::File::options().read(true).write(true).open(&stuck).unwrap();
        konedrive_fs::placeholder::write_state(&file, State::Hydrating).unwrap();
    }

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
    helper.forget();
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "ready");
    assert_eq!(restarted.root().unwrap().path, resolved, "the root must come back");
    assert_eq!(
        state_of_path(&stuck),
        Some(State::OnlineOnly),
        "startup recovery never ran: a file a crash left mid-hydration stays that way, \
         holding content nothing may trust"
    );

    let seen = helper.seen();
    let registered = seen.iter().position(|s| *s == Seen::RegisterRoot);
    let cleared = seen.iter().position(|s| *s == Seen::ClearIgnore);
    assert!(registered.is_some(), "the root must be registered again: {seen:?}");
    assert!(cleared.is_some(), "recovery must have cleared the ignore mark: {seen:?}");
    assert!(
        registered < cleared,
        "recovery ran before the registration it depends on: {seen:?}"
    );

    // And forgetting the folder must un-persist it, or the next start
    // brings back a root the user got rid of.
    restarted.unregister_root().await.unwrap();
    assert_eq!(
        Config::load(restarted.persist.as_ref().unwrap().store.file()).unwrap().sync_root,
        "",
        "a forgotten root must not come back at the next start"
    );
}

/// §8 step 2: an intercepted root's files may carry the ignore mark, and
/// a file punched while it still carries one is empty **and**
/// permanently un-intercepted — zeros with nothing left to notice them.
/// So a dehydration in that root needs a helper, and refuses without
/// one, however convenient it would be to carry on.
#[tokio::test]
async fn dehydrate_is_refused_when_an_intercepted_root_has_lost_its_helper() {
    let (service, root_dir, _source_dir, _sockets, _helper) =
        populated_service(&vec![7u8; 4096]).await;
    let file = root_dir.path().join("f.bin");
    service.hydrate_now(&file).await.unwrap();

    service.set_link(None);

    let error = service.dehydrate(&file).await.unwrap_err();
    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        vec![7u8; 4096],
        "and the file must be exactly as it was"
    );
}

/// `HelperLink` fails outstanding and later calls loudly,
/// which is right — but nothing reconnected, and the published state
/// said `ready` with an empty `LastError` the whole time the folder was
/// dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_that_goes_away_is_published_and_reconnected_to() {
    a_helper_that_goes_away_is_published_and_reconnected_to_with(false).await;
}

/// The supervisor used to find out
/// the helper was gone only when `serve_hydrations` returned, and that
/// joined every fill still running first — so with one long download in
/// flight, the loss was not published (`RootState` stayed `ready`, the
/// dead link was still handed out) and nothing reconnected until the
/// download ended, while a restarted helper, re-marking the tree from
/// `roots.json`, had no daemon to ask and denied every open `EIO` after
/// 30 s. Here the download never ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_the_helper_is_published_and_reconnected_while_a_fill_still_runs() {
    a_helper_that_goes_away_is_published_and_reconnected_to_with(true).await;
}

async fn a_helper_that_goes_away_is_published_and_reconnected_to_with(fill_running: bool) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let root_dir = tempfile::tempdir().unwrap();
    let service = SyncService::new(None, None, None);
    // Long enough that the window in which the helper is gone is
    // comfortably observable, short enough to keep the test quick.
    tokio::spawn(supervise_helper(
        Arc::clone(&service),
        socket_path.clone(),
        Duration::from_millis(300),
    ));
    wait_until("the supervisor connected", || service.link().is_some()).await;
    service.register_root(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "ready");

    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), [1u8; 64]).unwrap();
    let slow = Arc::new(LocalDir::new(remote.path()).delay(Duration::from_secs(3600)));
    let files = tempfile::tempdir().unwrap();
    let _held = fill_running.then(|| {
        install_source(&service, Arc::clone(&slow) as Arc<dyn ContentSource>);
        let fd = placeholder(files.path(), "slow.bin", "ITEM", 64);
        helper.send_request(1, &fd);
        fd
    });
    if fill_running {
        wait_until("the fill began", || slow.fetches() > 0).await;
    }
    helper.forget();

    helper.hang_up();

    wait_until("the helper's absence was published", || service.root_state() == "error").await;
    assert!(
        service.last_error().contains("not connected"),
        "the published error must say what happened: {}",
        service.last_error()
    );
    wait_until("the root was registered again", || {
        helper.seen().contains(&Seen::RegisterRoot)
    })
    .await;
    wait_until("the folder came back", || service.root_state() == "ready").await;
}

// --- Guards over verified-correct behaviour ------------

/// R3. Unregistering a root must tell the helper, or the helper keeps
/// the tree marked — and, with the uid no longer owning a root, answers
/// every placeholder open in it `EIO` (measurement).
#[tokio::test]
async fn unregister_root_tells_the_helper() {
    let (service, _sockets, helper) = service_with_helper().await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    helper.forget();

    service.unregister_root().await.unwrap();

    assert!(
        helper.seen().contains(&Seen::UnregisterRoot),
        "the helper was never told the root is gone: {:?}",
        helper.seen()
    );
}

/// R4. Unregistering a root must forget its content source. Item ids are
/// paths relative to the source, so a placeholder in the *next* root with
/// the same relative name matches the old source exactly: `Hydrate`
/// would fill it with the old folder's bytes and report success.
#[tokio::test]
async fn unregister_root_forgets_the_content_source() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let first_source = tempfile::tempdir().unwrap();
    std::fs::write(first_source.path().join("f.bin"), vec![0xAAu8; 4096]).unwrap();
    let first = tempfile::tempdir().unwrap();
    service.register_root(first.path()).await.unwrap();
    service.populate_from_directory(first_source.path()).await.unwrap();
    service.unregister_root().await.unwrap();

    let second = tempfile::tempdir().unwrap();
    service.register_root(second.path()).await.unwrap();
    drop(placeholder(second.path(), "f.bin", "f.bin", 4096));
    let file = second.path().join("f.bin");

    let outcome = service.hydrate_now(&file).await;

    assert!(
        matches!(outcome, Err(SyncError::NoSource)),
        "the second root was hydrated from the first root's source: {outcome:?}"
    );
    assert_ne!(
        std::fs::read(&file).unwrap(),
        vec![0xAAu8; 4096],
        "and it holds the first folder's bytes"
    );
    assert_eq!(service.item_state(&file).await, "online-only");
}

/// Design §8.3, review I2: an empty folder that carries another account's
/// drive holds nothing to adopt — the usual Remove, then Add, on the same
/// folder — so it is taken, and the stale drive comes off; a folder with
/// anything in it is still refused.
#[tokio::test]
async fn an_empty_folder_that_carries_another_drive_is_taken_and_a_full_one_is_not() {
    let config_dir = tempfile::tempdir().unwrap();
    let persist = persist(&config_dir.path().join("config.toml"));
    persist.store.record_drive(&persist.account, "DB").unwrap();
    let service = SyncService::new(None, None, Some(persist));
    service.set_helper_socket(config_dir.path().join("no-helper.sock"));
    let (full, empty) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for dir in [full.path(), empty.path()] {
        xattr::set(dir, "user.konedrive.drive", b"DA").unwrap();
    }
    std::fs::write(full.path().join("theirs.txt"), b"x").unwrap();

    let refused = service.register_root_without_interception(full.path()).await;
    assert!(matches!(refused, Err(SyncError::ForeignFolder)), "{refused:?}");
    assert_eq!(xattr::get(full.path(), "user.konedrive.drive").unwrap().as_deref(), Some(&b"DA"[..]));

    service.register_root_without_interception(empty.path()).await.unwrap();
    assert_eq!(xattr::get(empty.path(), "user.konedrive.drive").unwrap(), None, "the stale drive is taken off");
}

/// Review M2: an account being removed is retired under its lifecycle
/// lock, and registers nothing from then on — not even a call that was
/// waiting for that lock.
#[tokio::test]
async fn a_retired_account_registers_nothing() {
    let service = SyncService::new(None, None, None);
    service.retire().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let refused = service.register_root_without_interception(dir.path()).await;
    assert!(matches!(&refused, Err(SyncError::Io(why)) if why.contains("being removed")), "{refused:?}");
    assert_eq!(xattr::get(dir.path(), "user.konedrive.root").unwrap(), None, "the folder is not touched");
}

/// N6. The persisted "intercepted" flag must survive a restart. A root
/// registered without interception on a machine with no helper would
/// otherwise be restored as an
/// intercepted root, which waits for a helper that never comes: the
/// folder simply would not come back.
#[tokio::test]
async fn a_root_persisted_without_interception_comes_back_without_a_helper() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    {
        let service = SyncService::new(None, None, Some(persist(&config_file)));
        service.register_root_without_interception(root_dir.path()).await.unwrap();
    }
    assert!(
        !Config::load(&config_file).unwrap().sync_root_intercepted,
        "the mode must be written down with the root"
    );

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.resume().await;

    assert_eq!(
        restarted.root().map(|r| r.path),
        Some(std::fs::canonicalize(root_dir.path()).unwrap()),
        "a root registered without interception did not come back after a restart"
    );
    assert_eq!(restarted.root_state(), "no-interception");
}

/// Q3, narrowed by. A root registered without interception
/// *while a helper was connected* stays that way when the helper connects
/// again — after a reconnect, and after a restart: the user asked for
/// this mode by name with interception on offer, and quietly changing
/// what protects their files is not this daemon's call. (One registered
/// that way because no helper was connected does switch; see below.)
#[tokio::test]
async fn a_helper_reconnecting_does_not_upgrade_a_root_registered_without_interception_on_purpose() {
    let (service, helper, config_file, sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let socket_path = sockets.path().join("helper.sock");
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    assert_eq!(
        Config::load(&config_file).unwrap().sync_root_upgrade_when_helper,
        Some(false),
        "a choice made with a helper connected must be written down as one"
    );

    // What `supervise_helper` does when the connection drops and the
    // helper answers again.
    service.set_link(None);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    helper.forget();
    service.set_link(Some(link));
    service.resume().await;
    drop(service);

    // And a restart, with the helper there from the start.
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
    restarted.restore().await;
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "no-interception");
    assert!(
        restarted.last_error().contains("read as zeros"),
        "the warning must still be there: {}",
        restarted.last_error()
    );
    assert!(
        !helper.seen().contains(&Seen::RegisterRoot),
        "the root was registered with the helper behind the user's back: {:?}",
        helper.seen()
    );
}

// --- A folder registered without the helper, and the helper arriving

/// A service that persists into a config file of its own, with no link
/// and no helper at `helper.sock` yet — the machine before the helper is
/// installed — and a folder registered there without interception.
async fn registered_before_the_helper() -> (Arc<SyncService>, PathBuf, PathBuf, [tempfile::TempDir; 3]) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let service = SyncService::new(None, None, Some(persist(&config_file)));
    service.set_helper_socket(&socket_path);
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    assert_eq!(service.root_state(), "no-interception");
    (service, socket_path, config_file, [sockets, config_dir, root_dir])
}

/// Found in real use: a folder registered while the helper was
/// not installed ("Use Without the Helper") stayed that way once it was,
/// and every file in it read as zeros until a Forget and a new
/// registration. The helper connecting switches it: the root is
/// registered with the helper — whose walk marks every directory in it,
/// as at every restart — recovered, and written down as intercepted.
/// What is placed in it afterwards is marked first (invariant M1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_registered_without_the_helper_switches_to_interception_when_the_helper_connects() {
    let (service, socket_path, config_file, dirs) = registered_before_the_helper().await;
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("a/b")).unwrap();
    std::fs::write(source.path().join("a/b/f.bin"), [7u8; 64]).unwrap();
    service.populate_from_directory(source.path()).await.unwrap();

    // The helper is installed and started after the folder was registered.
    let supervisor =
        tokio::spawn(supervise_helper(Arc::clone(&service), socket_path.clone(), Duration::from_millis(10)));
    let helper = FakeHelper::start(socket_path, Duration::ZERO);
    wait_until("the folder switched to interception", || service.root_state() == "ready").await;

    assert_eq!(
        helper.seen().first(),
        Some(&Seen::RegisterRoot),
        "the root must be registered with the helper, whose walk marks every directory: {:?}",
        helper.seen()
    );
    assert_eq!(service.last_error(), "", "the no-interception warning must go");
    let config = Config::load(&config_file).unwrap();
    assert!(config.sync_root_intercepted, "the switch must be written down, or a restart undoes it");
    assert_eq!(config.sync_root, resolved(dirs[2].path()));

    // `a/` and `a/b/` are marked again as they are passed (see
    // `a_directory_that_already_exists_is_marked_again`); `c/` is new.
    helper.forget();
    std::fs::create_dir(source.path().join("c")).unwrap();
    std::fs::write(source.path().join("c/g.bin"), [8u8; 64]).unwrap();
    service.populate_from_directory(source.path()).await.unwrap();
    assert!(
        helper.seen().contains(&Seen::MarkDir { entries: 0 }),
        "a directory placed after the switch must be marked before anything is created in \
         it: {:?}",
        helper.seen()
    );
    supervisor.abort();
}

/// A folder registered without interception because no helper was
/// connected is written down as one to switch, so that a restart before
/// the helper arrives still switches it when the helper does.
#[tokio::test]
async fn a_registration_made_with_no_helper_is_written_down_to_switch_and_switches_after_a_restart() {
    let (service, socket_path, config_file, dirs) = registered_before_the_helper().await;
    assert_eq!(Config::load(&config_file).unwrap().sync_root_upgrade_when_helper, Some(true));
    drop(service);

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.set_helper_socket(&socket_path);
    restarted.restore().await;
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "no-interception");

    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    restarted.set_link(Some(link));
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
    assert_eq!(helper.seen().first(), Some(&Seen::RegisterRoot));
    let config = Config::load(&config_file).unwrap();
    assert!(config.sync_root_intercepted);
    assert_eq!(config.sync_root_upgrade_when_helper, Some(false), "nothing is left to switch");
    assert_eq!(config.sync_root, resolved(dirs[2].path()));
}

/// Ruling 4 of: a `config.toml` written before the flag existed
/// cannot say why its folder is without interception. It is read as a
/// folder to switch — the user's own registration is exactly that case,
/// and must switch once they restart the daemon or the helper reconnects
/// — while an intercepted one has nothing to switch.
#[tokio::test]
async fn a_folder_without_interception_recorded_before_the_flag_existed_switches_when_the_helper_connects() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
    xattr::set(root_dir.path(), "user.konedrive.root", root_id.as_bytes()).unwrap();
    // What the daemon before wrote for such a folder (as migrated).
    write_config(
        &config_file,
        &format!(
            "path = \"{}\"\nid = \"{root_id}\"\nintercepted = false\nsource = \"local\"\nbaloo_excluded = false\n",
            resolved(root_dir.path())
        ),
    );
    assert_eq!(Config::load(&config_file).unwrap().sync_root_upgrade_when_helper, None);

    // The daemon restarts; the helper connects.
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.set_helper_socket(&socket_path);
    restarted.restore().await;
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "no-interception");
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    restarted.set_link(Some(link));
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
    assert_eq!(helper.seen().first(), Some(&Seen::RegisterRoot));
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
}

/// Ruling 2 of: a switch that fails leaves the folder exactly as
/// it was — without interception, written down that way — says why in
/// `LastError`, and is tried again the next time the helper connects.
#[tokio::test]
async fn a_switch_the_helper_refuses_leaves_the_folder_as_it_was_and_is_tried_again_at_the_next_connect() {
    let (service, socket_path, config_file, _dirs) = registered_before_the_helper().await;
    let before = Config::load(&config_file).unwrap();
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    helper.refuse(Seen::RegisterRoot, libc::EIO);

    // What `supervise_helper` does the moment a helper answers.
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(service.root_state(), "no-interception");
    let said = service.last_error();
    assert!(said.starts_with(NO_INTERCEPTION_WARNING), "the warning must stay: {said}");
    assert!(
        said.contains("switching this folder to interception failed") && said.contains("errno 5"),
        "LastError must say why the folder is still without interception: {said}"
    );
    assert_eq!(Config::load(&config_file).unwrap(), before, "config.toml must say what it said before");
    assert!(service.root().is_some());

    // The connection drops (`supervise_helper` lets go of the link), and
    // the helper connects again, and this time accepts.
    service.set_link(None);
    helper.refuse(Seen::RegisterRoot, 0);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    assert_eq!(service.last_error(), "");
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
}

/// A failed switch the helper may still hold — its registration failed,
/// and it could not confirm it let go — is kept intercepted instead
///: a folder the helper may hold must never be one the
/// daemon holds without interception. It is brought up at the next
/// connect, as every intercepted folder is.
#[tokio::test]
async fn a_failed_switch_the_helper_may_still_hold_is_kept_intercepted_and_brought_up_at_the_next_connect() {
    let (service, socket_path, config_file, _dirs) = registered_before_the_helper().await;
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    helper.refuse(Seen::RegisterRoot, libc::EIO);
    helper.refuse(Seen::UnregisterRoot, libc::EIO);

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(service.root_state(), "error");
    assert!(
        service.last_error().contains("could not be told to let go"),
        "{}",
        service.last_error()
    );
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
    // Held as intercepted: its Forget goes through the helper, which is
    // still refusing — a folder without interception would never ask.
    let error = service.unregister_root().await.unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_some());

    service.set_link(None);
    helper.refuse(Seen::RegisterRoot, 0);
    helper.refuse(Seen::UnregisterRoot, 0);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;
    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
}

// --- The mode boundary --------------------

/// A fake helper, a service connected to it that persists into a config
/// file of its own, and everything that has to outlive the test body.
async fn service_with_config(
    register_root_delay: Duration,
) -> (Arc<SyncService>, FakeHelper, PathBuf, tempfile::TempDir, tempfile::TempDir) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), register_root_delay);
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let service = SyncService::new(Some(link), None, Some(persist(&config_file)));
    (service, helper, config_file, sockets, config_dir)
}

fn recorded_root(config_file: &Path) -> String {
    Config::load(config_file).unwrap().sync_root
}

fn resolved(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap().display().to_string()
}

/// H133. An intercepted root may carry ignore marks, and only the
/// helper's `UnregisterRoot` takes them off — its walk clears the mark of
/// every file in the tree. A Forget that could not tell the helper used
/// to be accepted anyway: the daemon forgot the folder while the helper
/// kept it, marks, ignore marks, `roots.json` entry and all, and a later
/// registration of the same folder without interception then punched a
/// file that was still ignored. Measured in the VM suite: a reader got
/// 65536 zero bytes and nothing was fetched.
#[tokio::test]
async fn forgetting_an_intercepted_root_needs_the_helper() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let link = service.link().unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    helper.forget();
    service.set_link(None);

    let error = service.unregister_root().await.unwrap_err();

    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    assert!(service.root().is_some(), "the refused Forget forgot the folder anyway");
    assert_eq!(
        recorded_root(&config_file),
        resolved(root_dir.path()),
        "and it must still be there at the next start"
    );
    let error =
        service.register_root_without_interception(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");

    // With the helper back, the same Forget goes through — to the helper.
    service.set_link(Some(link));
    service.unregister_root().await.unwrap();
    assert_eq!(helper.seen(), vec![Seen::UnregisterRoot]);
    assert!(service.root().is_none());
    assert_eq!(recorded_root(&config_file), "");
}

/// A Forget the helper answers `EPERM` has nothing left to undo: the
/// helper holds no root of this uid under that id — it lost it, or never
/// kept it — so no mark of that registration can be left, and keeping
/// the folder would only make it impossible to forget. Any other refusal
/// still keeps it, because then the helper may well hold it.
#[tokio::test]
async fn a_forget_the_helper_answers_eperm_goes_through_and_any_other_refusal_does_not() {
    let (service, helper, _config_file, _sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    helper.refuse(Seen::UnregisterRoot, libc::EIO);
    let error = service.unregister_root().await.unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_some(), "a helper that may still hold it was ignored");

    helper.refuse(Seen::UnregisterRoot, libc::EPERM);
    service.unregister_root().await.unwrap();
    assert!(service.root().is_none());
}

/// H134. A root registered without interception was never announced to
/// the helper, so forgetting it has nothing to tell the helper — and
/// telling it anyway made it impossible to forget while a helper was
/// connected: the helper refuses to unregister a root the uid does not
/// hold (`EPERM`), and the daemon kept the registration. Measured in
/// small round 3, and again in the VM suite.
#[tokio::test]
async fn forgetting_a_root_registered_without_interception_never_asks_the_helper() {
    let (service, _sockets, helper) = service_with_helper().await;
    helper.refuse(Seen::UnregisterRoot, libc::EIO);
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();

    service.unregister_root().await.unwrap();

    assert!(service.root().is_none());
    assert_eq!(service.root_state(), "none");
    assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());
}

/// H135, first half. `PopulateFromDirectory` used to mark every
/// directory it created whenever a link merely existed — in a root
/// registered without interception too. On a filesystem where the uid
/// owns no helper root the helper refuses that `EPERM`, and the populate
/// failed; where it owns one, the mark landed, the directory was
/// intercepted, and an intercepted hydration ignore-marks the file —
/// which is exactly what H135's skipped `ClearIgnore` relies on never
/// happening. Both measured in the VM suite.
#[tokio::test]
async fn populating_a_root_registered_without_interception_marks_nothing() {
    let (service, _sockets, helper) = service_with_helper().await;
    helper.refuse(Seen::MarkDir { entries: 0 }, libc::EPERM);
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![4u8; 4096]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();

    assert_eq!(service.populate_from_directory(source_dir.path()).await.unwrap(), 1);

    assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());
}

/// A root registered without interception, populated with one file
/// `b.bin` and filled, ready to be freed up.
async fn filled_without_interception(service: &SyncService) -> (tempfile::TempDir, PathBuf) {
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("b.bin"), vec![4u8; 64 * 1024]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    let file = root_dir.path().join("b.bin");
    service.hydrate_now(&file).await.unwrap();
    // The source goes; the file is what it holds now.
    std::mem::forget(source_dir);
    (root_dir, file)
}

fn data_blocks(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().blocks()
}

/// The daemon's local rule at a dehydration in a root registered
/// without interception, with a link: the helper is asked to clear the
/// file's ignore mark, as in any other root, and the punch follows. It
/// used to be skipped, on the strength of a chain of reasoning — nothing
/// in such a folder is intercepted, and interception resumes only through
/// a walk that clears every mark — but that assumption failed again: a
/// stale-marked file emptied here read zeros once the
/// folder was intercepted again. The helper grants the clear on ownership
/// alone now, so the `EPERM` that made this mode skip it is gone too.
#[tokio::test]
async fn a_dehydration_without_interception_clears_the_mark_through_its_link() {
    let (service, _sockets, helper) = service_with_helper().await;
    let (_root, file) = filled_without_interception(&service).await;

    service.dehydrate(&file).await.unwrap();

    assert_eq!(service.item_state(&file).await, "online-only");
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark must be cleared first");
}

/// And stopped by a clear that fails, as §8 step 2 has it everywhere
/// else: the file is left hydrated, content and all.
#[tokio::test]
async fn a_dehydration_without_interception_whose_mark_is_not_cleared_changes_nothing() {
    let (service, _sockets, helper) = service_with_helper().await;
    let (_root, file) = filled_without_interception(&service).await;
    helper.refuse(Seen::ClearIgnore, libc::EIO);

    let refused = service.dehydrate(&file).await;

    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(service.item_state(&file).await, "hydrated");
    assert!(data_blocks(&file) > 64, "a file whose mark was not cleared was emptied");
}

/// with no link. A helper running with no link to this
/// daemon — at startup before the first connection, or between a
/// helper's restart and the reconnect — has a group that may hold a mark
/// on the file, and nothing here can clear it: refused `NoHelper`, the
/// file untouched. With no helper bound to the socket at all — the file
/// a helper that exited left behind — no group of ours exists, and the
/// punch goes ahead.
#[tokio::test]
async fn with_no_link_a_running_helper_stops_a_dehydration_and_an_exited_one_does_not() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let service = SyncService::new(None, None, None);
    service.set_helper_socket(&socket_path);
    let (_root, file) = filled_without_interception(&service).await;

    let running = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let refused = service.dehydrate(&file).await;
    assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
    assert_eq!(service.item_state(&file).await, "hydrated");
    assert!(data_blocks(&file) > 64, "emptied while a helper ran with no link to it");
    assert!(running.seen().is_empty(), "the helper was connected to: {:?}", running.seen());

    // The helper exits: its socket file stays, with nothing bound to it.
    let stale = sockets.path().join("stale.sock");
    drop(socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .and_then(|fd| {
            bind(fd.as_raw_fd(), &UnixAddr::new(&stale).unwrap())?;
            Ok(fd)
        })
        .unwrap());
    assert!(stale.exists());
    service.set_helper_socket(&stale);
    service.dehydrate(&file).await.unwrap();
    assert_eq!(service.item_state(&file).await, "online-only");
}

/// at the other punch site: recovery of a root registered
/// without interception clears an interrupted file's mark through its
/// link, like any other recovery.
#[tokio::test]
async fn recovery_without_interception_clears_the_mark_through_its_link() {
    let (service, _sockets, helper) = service_with_helper().await;
    let (root_dir, stuck) = root_with_a_stuck_file();

    service.register_root_without_interception(root_dir.path()).await.unwrap();

    assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "recovery did not run");
    assert_eq!(service.root_state(), "no-interception");
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark must be cleared first");
}

/// A folder with the root id already on it and one file a crash left
/// `dehydrating`.
fn root_with_a_stuck_file() -> (tempfile::TempDir, PathBuf) {
    let root_dir = tempfile::tempdir().unwrap();
    xattr::set(
        root_dir.path(),
        "user.konedrive.root",
        b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d",
    )
    .unwrap();
    let stuck = root_dir.path().join("stuck.bin");
    std::fs::write(&stuck, vec![1u8; 64 * 1024]).unwrap();
    {
        let file = std::fs::File::options().read(true).write(true).open(&stuck).unwrap();
        konedrive_fs::placeholder::write_state(&file, State::Dehydrating).unwrap();
    }
    (root_dir, stuck)
}

/// With no link while a helper runs, recovery of such a root leaves the
/// interrupted file exactly as found — deferred, not failed — and runs
/// again, clearing the mark, once the link is up. Without
/// the second run the file would stay `dehydrating` until the next start.
///
/// Here the folder is one registered without interception on purpose
/// (`config.toml` says so), restored at a start that finds a helper
/// running and no link to it yet.
#[tokio::test]
async fn recovery_deferred_while_an_unlinked_helper_runs_finishes_once_linked() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let (root_dir, stuck) = root_with_a_stuck_file();
    write_config(
        &config_file,
        &format!("path = \"{}\"\nintercepted = false\nupgrade_when_helper = false\n", resolved(root_dir.path())),
    );
    let service = SyncService::new(None, None, Some(persist(&config_file)));
    service.set_helper_socket(&socket_path);

    service.resume().await;

    assert_eq!(state_of_path(&stuck), Some(State::Dehydrating), "reset with a mark unclearable");
    assert!(data_blocks(&stuck) > 64);
    assert_eq!(service.root_state(), "no-interception", "deferred is not an error");
    assert!(service.last_error().contains("not connected to it yet"), "{}", service.last_error());

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "the deferred reset never ran");
    assert_eq!(helper.seen(), vec![Seen::ClearIgnore]);
    assert_eq!(service.last_error(), NO_INTERCEPTION_WARNING);
}

/// The same deferred file in a folder registered without interception
/// because no helper was connected: the link's arrival switches the
/// folder to interception, and the switch's own recovery —
/// with the link, after the helper registered the root — resets it.
#[tokio::test]
async fn a_switch_to_interception_resets_what_recovery_deferred() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
    let service = SyncService::new(None, None, None);
    service.set_helper_socket(&socket_path);
    let (root_dir, stuck) = root_with_a_stuck_file();

    service.register_root_without_interception(root_dir.path()).await.unwrap();
    assert_eq!(state_of_path(&stuck), Some(State::Dehydrating), "reset with a mark unclearable");

    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    service.set_link(Some(link));
    service.resume().await;

    assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "the deferred reset never ran");
    assert_eq!(helper.seen(), vec![Seen::RegisterRoot, Seen::ClearIgnore]);
    assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    assert_eq!(service.last_error(), "");
}

/// The route into no-interception mode that H133 alone leaves open. A
/// root restored from `config.toml` used to exist nowhere in the daemon
/// until the helper came back — `resume` returned early — so
/// `RegisterWithoutInterception` of the very folder the helper still
/// held (marks, ignore marks and all) was accepted, and the next
/// dehydration there punched files that were still ignored. Measured in
/// the VM suite: 65536 zero bytes. The root is held as registered now,
/// and everything that has to go through the helper waits for it.
#[tokio::test]
async fn an_intercepted_root_restored_before_its_helper_is_back_is_held() {
    let (first, helper, config_file, sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let root_dir = tempfile::tempdir().unwrap();
    first.register_root(root_dir.path()).await.unwrap();
    let root_id = first.root().unwrap().root_id;
    drop(first);
    helper.forget();

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.resume().await;

    let held = restarted.root().expect("a restored root must be held before the helper");
    assert_eq!(held.path.display().to_string(), resolved(root_dir.path()));
    assert_eq!(held.root_id, root_id, "under the id the helper holds it by");
    assert_eq!(restarted.root_state(), "error");
    assert!(
        restarted.last_error().contains("not connected"),
        "the published error must say what is missing: {}",
        restarted.last_error()
    );
    let error =
        restarted.register_root_without_interception(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    let elsewhere = tempfile::tempdir().unwrap();
    let error =
        restarted.register_root_without_interception(elsewhere.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    let error = restarted.unregister_root().await.unwrap_err();
    assert!(matches!(error, SyncError::NoHelper), "{error:?}");
    let config = Config::load(&config_file).unwrap();
    assert_eq!(config.sync_root, resolved(root_dir.path()));
    assert!(config.sync_root_intercepted);
    assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());

    // The helper comes back: the same root is brought up, not a new one.
    let (link, _requests) =
        HelperLink::connect(&sockets.path().join("helper.sock")).await.unwrap();
    restarted.set_link(Some(link));
    restarted.resume().await;
    assert_eq!(restarted.root_state(), "ready");
    assert_eq!(helper.seen(), vec![Seen::RegisterRoot]);
}

/// ...and it stays held when bringing it up fails. A failed startup
/// `bind` used to leave the daemon holding no root while the helper still
/// held the folder, which is the same open door. Forgetting it still
/// works, through the helper, under the id `config.toml` recorded — the
/// folder is gone, so nothing could be read from it.
#[tokio::test]
async fn a_restored_root_that_cannot_be_brought_up_is_still_held() {
    let (first, helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let root_dir = tempfile::tempdir().unwrap();
    first.register_root(root_dir.path()).await.unwrap();
    let root_path = std::fs::canonicalize(root_dir.path()).unwrap();
    let link = first.link().unwrap();
    drop(first);
    drop(root_dir);
    helper.forget();

    let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
    restarted.resume().await;

    assert_eq!(restarted.root_state(), "error");
    assert_eq!(restarted.root().map(|r| r.path), Some(root_path.clone()));
    let elsewhere = tempfile::tempdir().unwrap();
    let error =
        restarted.register_root_without_interception(elsewhere.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");

    restarted.unregister_root().await.unwrap();
    assert_eq!(helper.seen(), vec![Seen::UnregisterRoot]);
    assert_eq!(recorded_root(&config_file), "");
}

/// A config written before the root id was recorded still restores its
/// root as held: the id is read from the folder instead.
#[tokio::test]
async fn a_restored_root_with_no_recorded_id_takes_it_from_the_folder() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let root_dir = tempfile::tempdir().unwrap();
    let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
    xattr::set(root_dir.path(), "user.konedrive.root", root_id.as_bytes()).unwrap();
    write_config(&config_file, &format!("path = \"{}\"\n", resolved(root_dir.path())));

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    restarted.resume().await;

    assert_eq!(restarted.root().map(|r| r.root_id), Some(root_id.to_owned()));
    assert_eq!(restarted.root_state(), "error");
}

/// The id the helper holds a root by is what a restored root has to be
/// forgotten by, so it is written down with the root, and removed with it.
#[tokio::test]
async fn the_root_id_is_recorded_with_the_root() {
    let (service, _helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();

    let config = Config::load(&config_file).unwrap();
    assert_eq!(config.sync_root_id, service.root().unwrap().root_id);

    service.unregister_root().await.unwrap();
    assert_eq!(Config::load(&config_file).unwrap().sync_root_id, "");
}

/// A root the helper holds and `config.toml` does not name is one the
/// daemon cannot see after a restart, and so one it would accept for
/// registration without interception. `RegisterRoot` used to write the
/// root down only after the helper had saved it *and* recovery had
/// walked the whole tree, so a crash anywhere in between left exactly
/// that. It is written down first now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_root_is_written_down_before_the_helper_hears_of_it() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::from_millis(400)).await;
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;

    assert_eq!(
        recorded_root(&config_file),
        resolved(root_dir.path()),
        "the helper was told about a root config.toml does not name"
    );
    registering.await.unwrap().unwrap();
}

/// And a root that cannot be written down is not registered at all: the
/// helper is never told about it.
///. `config.toml` is the account
/// sub-project's file too — it holds the `client_id` — and a copy that
/// could not be read used to be treated as empty and written back from
/// defaults, erasing everything in it. What could not be read is never
/// overwritten: an intercepted registration is refused (its record must
/// exist before the helper is told), and a registration without
/// interception stands but is not recorded.
#[tokio::test]
async fn an_unreadable_config_is_never_overwritten() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let unreadable = "client_id = \"the account's own\"\nthis is not [toml\n";
    std::fs::write(&config_file, unreadable).unwrap();
    let root_dir = tempfile::tempdir().unwrap();

    let refused = service.register_root(root_dir.path()).await;
    assert!(matches!(refused, Err(SyncError::Io(_))), "{refused:?}");
    assert!(helper.seen().is_empty(), "the helper was told: {:?}", helper.seen());
    assert_eq!(std::fs::read_to_string(&config_file).unwrap(), unreadable);

    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.unregister_root().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&config_file).unwrap(),
        unreadable,
        "config.toml was rewritten from defaults"
    );
}

#[tokio::test]
async fn a_root_that_cannot_be_written_down_is_not_registered() {
    let (service, _sockets, helper) = service_with_helper().await;
    let config_dir = tempfile::tempdir().unwrap();
    let blocker = config_dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"").unwrap();
    let service = SyncService::new(service.link(), None, Some(persist(&blocker.join("config.toml"))));
    let root_dir = tempfile::tempdir().unwrap();

    let error = service.register_root(root_dir.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_none());
    assert!(helper.seen().is_empty(), "the helper was told: {:?}", helper.seen());
}

/// on both sides: a `RegisterRoot` that fails after the
/// helper saved the root is undone at the helper, and in `config.toml`,
/// so that neither is left holding a root the daemon does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_registration_is_undone_at_the_helper_and_in_the_config() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::from_millis(400)).await;
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

    let error = registering.await.unwrap().unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_none(), "a failed registration stored a root anyway");
    assert_eq!(helper.seen(), vec![Seen::RegisterRoot, Seen::UnregisterRoot]);
    assert_eq!(recorded_root(&config_file), "");
}

/// ...unless the helper cannot confirm it let go. Then the root is kept,
/// intercepted, so it can only leave the way H133 allows — through the
/// helper — and not by a registration without interception on top of
/// a folder the helper may still be marking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_registration_the_helper_may_still_hold_is_kept() {
    let (service, helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::from_millis(400)).await;
    helper.refuse(Seen::UnregisterRoot, libc::EIO);
    let root_dir = tempfile::tempdir().unwrap();

    let registering = {
        let service = Arc::clone(&service);
        let path = root_dir.path().to_path_buf();
        tokio::spawn(async move { service.register_root(&path).await })
    };
    wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;
    xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

    let error = registering.await.unwrap().unwrap_err();
    assert!(matches!(error, SyncError::Io(_)), "{error:?}");
    assert!(service.root().is_some(), "a root the helper may hold was let go");
    assert_eq!(service.root_state(), "error");
    assert_eq!(recorded_root(&config_file), resolved(root_dir.path()));
    let error =
        service.register_root_without_interception(root_dir.path()).await.unwrap_err();
    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
}

/// The deterministic form of a D-Bus-activated first call: the bus name
/// is claimed before `resume` runs, so a `RegisterWithoutInterception`
/// can reach a restarted daemon before anything has looked at
/// `config.toml`. It must find the restored root all the same.
#[tokio::test]
async fn a_registration_that_arrives_before_resume_still_finds_the_restored_root() {
    let (first, helper, config_file, _sockets, _config_dir) =
        service_with_config(Duration::ZERO).await;
    let root_dir = tempfile::tempdir().unwrap();
    first.register_root(root_dir.path()).await.unwrap();
    drop(first);
    helper.forget();

    let restarted = SyncService::new(None, None, Some(persist(&config_file)));
    let error =
        restarted.register_root_without_interception(root_dir.path()).await.unwrap_err();

    assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
}

/// zbus runs every method call in a task of its own, so two
/// registrations can be in flight at once. Both used to pass the "no
/// root yet" check before either had committed, both reached the
/// helper, and the last commit won: the helper then held a root the
/// daemon did not — the state a later registration without interception
/// of that folder turns into zeros. Registrations and Forgets now take
/// turns, and the second one sees the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_registrations_at_once_leave_one_root_at_the_helper() {
    let (service, helper, _config_file, _sockets, _config_dir) =
        service_with_config(Duration::from_millis(300)).await;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    let (a, b) = tokio::join!(
        service.register_root(first.path()),
        service.register_root(second.path())
    );

    assert!(
        a.is_ok() != b.is_ok(),
        "exactly one of two concurrent registrations may succeed: {a:?}, {b:?}"
    );
    let refused = a.err().or(b.err()).unwrap();
    assert!(matches!(refused, SyncError::AlreadyRegistered), "{refused:?}");
    assert_eq!(
        helper.seen(),
        vec![Seen::RegisterRoot],
        "the helper was told about a root the daemon does not hold"
    );
}

/// A dehydration decides whether to send `ClearIgnore` from the root's
/// mode, and then may wait — for a fill of the same inode — before it
/// punches. The mode must not change under it in the meantime: a root
/// forgotten and registered again with interception while it waits
/// could have the file ignore-marked by then, and the punch would skip
/// the `ClearIgnore` that is suddenly needed.
/// it waits for the fill without the lifecycle lock — a Forget is not
/// held up by a download — and decides the mode only after, under the
/// lock: a folder forgotten meanwhile is refused, nothing punched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dehydration_waiting_for_a_fill_does_not_hold_up_a_forget() {
    let (service, _sockets, _helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("b.bin"), vec![4u8; 4096]).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root_without_interception(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    let file = root_dir.path().join("b.bin");
    service.hydrate_now(&file).await.unwrap();

    // A fill of the same inode, in progress.
    let fill = service.locks().lock(key_of(&file)).await;
    let dehydrating = {
        let service = Arc::clone(&service);
        let file = file.clone();
        tokio::spawn(async move { service.dehydrate(&file).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(Duration::from_secs(2), service.unregister_root())
        .await
        .expect("the Forget waited for a dehydration waiting for a fill")
        .unwrap();

    drop(fill);
    let refused = dehydrating.await.unwrap();
    assert!(matches!(refused, Err(SyncError::NoRoot)), "{refused:?}");
    assert_eq!(std::fs::read(&file).unwrap(), vec![4u8; 4096], "nothing was punched");
}

// --- A folder that shows OneDrive ----------

#[test]
fn the_published_state_is_computed_from_the_registration_and_the_sync() {
    let mut s = SyncSnapshot { root_state: RootState::Ready, ..SyncSnapshot::default() };
    assert_eq!(published_state(&s), "ready");
    s.listing = true;
    assert_eq!(published_state(&s), "listing");
    s.sync_trouble = Some(SyncTrouble { text: "cannot reach OneDrive".into(), blocking: false });
    assert_eq!(published_state(&s), "listing", "no network is said, not an error");
    s.sync_trouble = Some(SyncTrouble { text: "signed out".into(), blocking: true });
    assert_eq!(published_state(&s), "error");
    s.root_state = RootState::Error;
    s.last_error = "the helper is not connected".into();
    s.replacement_note = "1 file(s) changed in OneDrive could not be updated here yet: no space".into();
    s.conflict_count = 1;
    assert_eq!(
        published_error(&s),
        "the helper is not connected. signed out. 1 file(s) changed in OneDrive could not be updated here yet: no space",
        "a conflict is not a problem; it is not in LastError"
    );
    assert_eq!(published_state(&SyncSnapshot::default()), "none");

    // `listing` never hides `no-interception`.
    let s = SyncSnapshot { root_state: RootState::NoInterception, listing: true, ..SyncSnapshot::default() };
    assert_eq!(published_state(&s), "no-interception");
}

/// HS3: while a folder waits for the helper, `RootState` reads `error`
/// and `LastError` begins with what `HelperState` says — how to install
/// it, start it, or see why it failed — ahead of whatever else is said.
#[test]
fn a_folder_waiting_for_the_helper_says_how_to_start_it() {
    let said = |helper_state| {
        let s = SyncSnapshot {
            root_state: RootState::Ready,
            waits_for_helper: true,
            helper_state,
            last_error: "recovery left 1 file".into(),
            ..SyncSnapshot::default()
        };
        (published_state(&s), published_error(&s))
    };
    let (state, error) = said(HelperState::NotInstalled);
    assert_eq!(state, "error");
    assert_eq!(
        error,
        "the konedrive helper is not installed: files are not kept in step and do not download when \
         opened. Install it: sudo scripts/install-helper.sh (see README). recovery left 1 file"
    );
    assert!(said(HelperState::Stopped).1.starts_with(
        "the konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`. "
    ));
    assert!(said(HelperState::Failed).1.starts_with("the konedrive helper failed: see `systemctl status konedrive-helper`. "));
    assert!(said(HelperState::Unknown).1.starts_with("the konedrive helper is not connected. "));
    assert_eq!(said(HelperState::Connected).1, "recovery left 1 file", "nothing to say of a connected helper");

    let s = SyncSnapshot { root_state: RootState::Ready, helper_state: HelperState::Stopped, ..SyncSnapshot::default() };
    assert_eq!((published_state(&s), published_error(&s).as_str()), ("ready", ""), "a folder not waiting says nothing of it");
}

mod onedrive {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;
    use url::Url;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::*;
    use super::{persist, wait_until, Config, FakeHelper, Seen};
    use crate::drive::{DriveClient, RetryPolicy};
    use crate::state::{AccountSnapshot, SignInState, StateHandle};
    use crate::sync::listing::Schedule;
    use crate::token::{AuthError, StaticToken, TokenSource};

    struct World {
        server: MockServer,
        config: tempfile::TempDir,
        folder: tempfile::TempDir,
        /// Where the fake `balooctl6` lives: `calls` gets every
        /// `add`/`rm` it is run with, appended one per line; its
        /// `baloofilerc` is what is excluded already — nothing, unless a
        /// test writes it.; `crate::sync::baloo`.
        baloo: tempfile::TempDir,
        /// A helper that acknowledges everything, at `sockets/helper.sock`:
        /// a folder that shows OneDrive is kept in step only with one
        /// (HS2). [`connected`] links a service to it.
        helper: FakeHelper,
        sockets: tempfile::TempDir,
    }

    impl Drop for World {
        fn drop(&mut self) {
            // A locked tree cannot be removed by the temporary directory.
            let _ = std::process::Command::new("chmod")
                .args(["-R", "u+w"])
                .arg(self.folder.path())
                .status();
        }
    }

    /// A drive holding `docs/f.txt`, listed in full from the start and
    /// with no changes since from its delta link `L1`.
    async fn world() -> World {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", server.uri())})))
            .with_priority(1)
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [
                    {"id": "R", "root": {}, "folder": {}},
                    {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                    {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}}
                ],
                "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", server.uri())
            })))
            .with_priority(5)
            .mount(&server).await;
        let baloo = tempfile::tempdir().unwrap();
        write_fake_balooctl6(baloo.path());
        let sockets = tempfile::tempdir().unwrap();
        let helper = FakeHelper::start(sockets.path().join("helper.sock"), Duration::ZERO);
        World { server, config: tempfile::tempdir().unwrap(), folder: tempfile::tempdir().unwrap(), baloo, helper, sockets }
    }

    /// A new link to the world's helper.
    async fn link(w: &World) -> HelperLink {
        HelperLink::connect(&w.sockets.path().join("helper.sock")).await.unwrap().0
    }

    /// [`service`], linked to the world's helper: what a OneDrive folder
    /// is registered and kept in step with (HS2).
    async fn connected(w: &World, signed_in: bool) -> Arc<SyncService> {
        service_with(w, account(signed_in), Some(link(w).await), Arc::new(StaticToken::new("T")))
    }

    /// The world's account as the write gate lets it change OneDrive:
    /// `config.toml` says read-write and lists its drive, its token can write and was seen
    /// to reach that drive, and the account runs read-write.
    fn let_write(service: &SyncService) {
        use crate::config::{ConfigError, Mode};
        let persist = service.persist.as_ref().unwrap();
        persist
            .store
            .update(|c| {
                c.write_test_drive_ids = vec!["D1".into()];
                let account = c.accounts.iter_mut().find(|a| a.id == persist.account).unwrap();
                account.mode = Mode::ReadWrite;
                account.drive_id = "D1".into();
                Ok::<_, ConfigError>(())
            })
            .unwrap();
        service.account.as_ref().unwrap().update(|s| {
            s.mode = Mode::ReadWrite;
            s.granted_scopes = "Files.ReadWrite offline_access".into();
            s.live_drive = "D1".into();
        });
    }

    /// A fake `balooctl6`, so these tests never reach the real Baloo
    ///: `config add`/`config rm` are logged to `calls`, one
    /// call per line. What is excluded already is read from the
    /// `baloofilerc` beside it (`crate::sync::baloo`, B-I1b), never
    /// `~/.config`'s.
    fn write_fake_balooctl6(dir: &std::path::Path) {
        let script = dir.join("balooctl6");
        let log = dir.join("calls");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Every `add`/`rm` the fake `balooctl6` was run with, in order.
    fn baloo_calls(w: &World) -> String {
        std::fs::read_to_string(w.baloo.path().join("calls")).unwrap_or_default()
    }

    /// Writes the test's `baloofilerc` as if `folder` (or, passed
    /// directly, a directory above it) were already excluded — the
    /// user's own doing, which says a registration must never
    /// add to or a Forget take off. In the form KConfig writes it.
    fn mark_already_excluded(w: &World, folder: &std::path::Path) {
        let line = format!("[General]\nexclude folders[$e]={}/\n", folder.display());
        std::fs::write(w.baloo.path().join("baloofilerc"), line).unwrap();
    }

    fn account(signed_in: bool) -> StateHandle {
        StateHandle::new(AccountSnapshot {
            state: if signed_in { SignInState::SignedIn } else { SignInState::SignedOut },
            ..AccountSnapshot::default()
        })
    }

    fn service(w: &World, signed_in: bool) -> Arc<SyncService> {
        service_with(w, account(signed_in), None, Arc::new(StaticToken::new("T")))
    }

    /// A service wired as `main` wires it — a drive, its paths — with an
    /// hour between cycles, so that any cycle a test sees was asked for.
    fn service_with(
        w: &World,
        account: StateHandle,
        link: Option<HelperLink>,
        tokens: Arc<dyn TokenSource>,
    ) -> Arc<SyncService> {
        let service = SyncService::new(link, Some(account), Some(persist(&w.config.path().join("config.toml"))));
        wire(w, service, tokens)
    }

    /// A signed-in service on `hub`, wired as [`service_with`] wires one: a restart of
    /// the daemon whose hub knows what the machine's sources say.
    fn service_on(w: &World, hub: &Arc<hub::HelperHub>) -> Arc<SyncService> {
        let service = SyncService::on_hub(hub, Some(account(true)), Some(persist(&w.config.path().join("config.toml"))));
        wire(w, service, Arc::new(StaticToken::new("T")))
    }

    /// [`service_with`]'s wiring.
    fn wire(w: &World, service: Arc<SyncService>, tokens: Arc<dyn TokenSource>) -> Arc<SyncService> {
        let drive = DriveClient::new(Url::parse(&format!("{}/", w.server.uri())).unwrap(), tokens)
            .unwrap()
            .with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(10) });
        service.set_drive(drive);
        service.set_sync_paths(SyncPaths {
            tree_db: w.config.path().join("tree.sqlite"),
            rescue_dir: w.config.path().join("rescued"),
            thumbnails: Some(w.config.path().join("thumbnails")),
        });
        service.set_schedule(Schedule::polled(Duration::from_secs(3600), vec![Duration::from_millis(50)]));
        // No helper in these tests, and none running: a punch goes by "no helper at all".
        service.set_helper_socket(w.config.path().join("no-helper.sock"));
        // The fake `balooctl6` and a `baloofilerc` of the test's own
        //: never the real ones, so these tests never touch
        // ~/.config/baloofilerc.
        service.set_baloo(crate::sync::baloo::Baloo {
            program: Some(w.baloo.path().join("balooctl6")),
            settings: Some(w.baloo.path().join("baloofilerc")),
            ..crate::sync::baloo::Baloo::disabled()
        });
        service
    }

    /// Tokens while the account reads signed in, and "signed out" — as
    /// `TokenManager` answers once the refresh token is gone — otherwise.
    struct AccountTokens {
        account: StateHandle,
        refused: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TokenSource for AccountTokens {
        async fn access_token(&self) -> Result<String, AuthError> {
            if self.account.get().state == SignInState::SignedIn {
                Ok("T".into())
            } else {
                self.refused.fetch_add(1, Ordering::SeqCst);
                Err(AuthError::SignedOut)
            }
        }

        async fn invalidate(&self) {}
    }

    fn config_of(w: &World) -> Config {
        Config::load(&w.config.path().join("config.toml")).unwrap()
    }

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    async fn requests(w: &World) -> usize {
        w.server.received_requests().await.unwrap().len()
    }

    async fn deltas(w: &World) -> usize {
        w.server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/root/delta").count()
    }

    /// Delta requests that started a listing of the whole drive.
    async fn full_listings(w: &World) -> usize {
        w.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/me/drive/root/delta" && r.url.query().is_none())
            .count()
    }

    async fn wait_for_deltas(w: &World, more_than: usize) {
        for _ in 0..300 {
            if deltas(w).await > more_than {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no delta request came");
    }

    /// The first cycle is over: its counts are published only once the
    /// folder has been made to match the tree.
    async fn listed(service: &SyncService) {
        wait_until("the drive is listed into the folder", || service.items() == (2, 2, 0)).await;
    }

    /// OneDrive's answer about one item (or `root`), with the address of its page.
    async fn mount_page(w: &World, route: &str, id: &str, url: &str) {
        Mock::given(method("GET")).and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": id, "webUrl": url})))
            .mount(&w.server).await;
    }

    /// `WebUrl` (issue #53): the address of the page of a file, of a folder and of
    /// the account's folder itself, each from one GET and nothing else.
    #[tokio::test]
    async fn web_url_asks_onedrive_for_the_items_page_with_one_get() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        mount_page(&w, "/me/drive/items/F", "F", "https://onedrive.example/f").await;
        mount_page(&w, "/me/drive/items/D", "D", "https://onedrive.example/docs").await;
        mount_page(&w, "/me/drive/root", "R", "https://onedrive.example/root").await;
        let before = requests(&w).await;

        let file = w.folder.path().join("docs/f.txt");
        assert_eq!(service.web_url(&file).await.unwrap(), "https://onedrive.example/f");
        assert_eq!(service.web_url(&w.folder.path().join("docs")).await.unwrap(), "https://onedrive.example/docs");
        assert_eq!(service.root_web_url().await.unwrap(), "https://onedrive.example/root");

        let asked: Vec<(String, String)> = w.server.received_requests().await.unwrap()[before..]
            .iter()
            .map(|r| (r.method.to_string(), r.url.path().to_owned()))
            .collect();
        let get = |route: &str| ("GET".to_owned(), route.to_owned());
        assert_eq!(asked, [get("/me/drive/items/F"), get("/me/drive/items/D"), get("/me/drive/root")]);
        assert_eq!(
            konedrive_fs::placeholder::read_state(&std::fs::File::open(&file).unwrap()).unwrap(),
            Some(konedrive_fs::placeholder::State::OnlineOnly),
            "asking for the page downloads nothing"
        );
        service.stop_sync().await;
    }

    /// A file with no item id is not in OneDrive yet: refused by that name, and
    /// OneDrive is not asked.
    #[tokio::test]
    async fn web_url_of_a_file_not_uploaded_yet_is_refused_without_asking() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        std::fs::set_permissions(w.folder.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let new = w.folder.path().join("new.txt");
        std::fs::write(&new, b"new").unwrap();
        konedrive_fs::placeholder::write_state(&std::fs::File::open(&new).unwrap(), konedrive_fs::placeholder::State::Hydrated).unwrap();
        let before = requests(&w).await;

        let refused = service.web_url(&new).await.unwrap_err();
        assert!(matches!(refused, SyncError::NotInOneDrive(_)), "{refused:?}");
        assert!(matches!(dbus::to_fault(refused), dbus::SyncFault::NotUploaded(_)));
        assert_eq!(requests(&w).await, before);
        service.stop_sync().await;
    }

    /// OneDrive answering 503 until the retries run out is `Unreachable`, not a
    /// plain failure; an answer without an address, and an item gone, are failures.
    #[tokio::test]
    async fn web_url_says_when_onedrive_could_not_be_reached() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        Mock::given(method("GET")).and(path("/me/drive/items/F"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&w.server).await;
        Mock::given(method("GET")).and(path("/me/drive/items/D"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D"})))
            .mount(&w.server).await;

        let refused = service.web_url(&w.folder.path().join("docs/f.txt")).await.unwrap_err();
        assert!(matches!(refused, SyncError::Unreachable(_)), "{refused:?}");
        assert!(matches!(dbus::to_fault(refused), dbus::SyncFault::Unreachable(_)));
        let refused = service.web_url(&w.folder.path().join("docs")).await.unwrap_err();
        assert!(matches!(&refused, SyncError::Io(why) if why.contains("no address")), "{refused:?}");
        service.stop_sync().await;
    }

    #[tokio::test]
    async fn a_folder_registered_while_signed_in_shows_onedrive_read_only() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let file = w.folder.path().join("docs/f.txt");
        assert!(file.is_file());
        assert_eq!(config_of(&w).sync_root_source, "onedrive");
        assert_eq!((mode(&file), mode(&w.folder.path().join("docs"))), (0o444, 0o555));
        assert_eq!(service.root_state(), "ready");
        // A fresh OneDrive folder is excluded from KDE's
        // Baloo indexer, so reading a placeholder to index it does not
        // download the whole drive.
        let folder = std::fs::canonicalize(w.folder.path()).unwrap();
        assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));
        assert!(config_of(&w).sync_root_baloo_excluded);
        service.stop_sync().await;
    }

    /// A fresh OneDrive folder that is not already excluded
    /// from Baloo is excluded, and included again on Forget — the plain
    /// case, and the one the fake `balooctl6`'s empty `excluded` file
    /// gives by default.
    #[tokio::test]
    async fn baloo_excludes_a_fresh_onedrive_folder_and_includes_it_again_on_forget() {
        let w = world().await;
        let folder = std::fs::canonicalize(w.folder.path()).unwrap();
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));

        service.unregister_root().await.unwrap();
        assert_eq!(
            baloo_calls(&w),
            format!("config add excludeFolders {folder}\nconfig rm excludeFolders {folder}\n", folder = folder.display())
        );
    }

    /// Design §8.3 (test 7): a OneDrive folder remembers its account's
    /// drive — written once the first cycle has recorded it, and at the
    /// bring-up of a folder from before multiple accounts, which carries
    /// none — and, forgotten, it is refused `NotEmpty` to another account,
    /// while its own account may register it again.
    #[tokio::test]
    async fn a_onedrive_folder_remembers_its_drive_and_is_refused_to_another_account() {
        use std::os::unix::fs::PermissionsExt;
        let w = world().await;
        let drive = || xattr::get(w.folder.path(), konedrive_fs::placeholder::XATTR_DRIVE).unwrap();
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        assert_eq!(drive().as_deref(), Some(&b"D1"[..]), "written with the drive the first cycle recorded");
        service.stop_sync().await;
        drop(service);

        // A folder from before carries no drive: its first bring-up writes it.
        let open = |mode| std::fs::set_permissions(w.folder.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        open(0o755);
        xattr::remove(w.folder.path(), konedrive_fs::placeholder::XATTR_DRIVE).unwrap();
        open(0o555);
        {
            let restarted = connected(&w, true).await;
            restarted.restore().await;
            restarted.resume().await;
            assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
            assert_eq!(drive().as_deref(), Some(&b"D1"[..]));
            restarted.unregister_root().await.unwrap();
        }

        // The world's helper serves one connection at a time: each service
        // here goes before the next one connects.
        {
            let elsewhere = tempfile::tempdir().unwrap();
            let other = persist(&elsewhere.path().join("config.toml"));
            other.store.record_drive(&other.account, "D2").unwrap();
            let stranger = SyncService::new(Some(link(&w).await), Some(account(true)), Some(other));
            let refused = stranger.register_root(w.folder.path()).await;
            assert!(matches!(refused, Err(SyncError::ForeignFolder)), "{refused:?}");
        }

        let own = connected(&w, true).await;
        own.register_root(w.folder.path()).await.unwrap();
        own.stop_sync().await;
    }

    /// A folder the user has already excluded from Baloo —
    /// themselves, or through a parent directory — is never added again,
    /// and a later Forget must not remove an exclusion this daemon did
    /// not add.
    #[tokio::test]
    async fn baloo_leaves_a_folder_the_user_already_excluded_alone() {
        let w = world().await;
        let folder = std::fs::canonicalize(w.folder.path()).unwrap();
        mark_already_excluded(&w, &folder);
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        assert_eq!(baloo_calls(&w), "", "already excluded, so nothing is added");
        assert!(!config_of(&w).sync_root_baloo_excluded);

        service.unregister_root().await.unwrap();
        assert_eq!(baloo_calls(&w), "", "we never added it, so Forget must not remove it");
    }

    /// Whether this daemon added the exclusion is persisted
    /// (`sync_root_baloo_excluded` in `config.toml`), so a restart
    /// between a registration and its Forget still gets the Forget
    /// right — the exclusion comes off, and it is not re-checked or
    /// re-added at the restart in between.
    #[tokio::test]
    async fn baloo_exclusion_survives_a_restart_and_is_still_removed_on_forget() {
        let w = world().await;
        let folder = std::fs::canonicalize(w.folder.path()).unwrap();
        {
            let first = connected(&w, true).await;
            first.register_root(w.folder.path()).await.unwrap();
            first.stop_sync().await;
        }
        let after_first = format!("config add excludeFolders {}\n", folder.display());
        assert_eq!(baloo_calls(&w), after_first);
        assert!(config_of(&w).sync_root_baloo_excluded);

        let second = connected(&w, false).await;
        second.restore().await;
        second.resume().await;
        assert_eq!(baloo_calls(&w), after_first, "not re-checked or re-added at a restart");
        assert!(config_of(&w).sync_root_baloo_excluded, "the flag survives the restart");

        second.unregister_root().await.unwrap();
        assert_eq!(baloo_calls(&w), format!("{after_first}config rm excludeFolders {}\n", folder.display()));
    }

    /// the exclusion used to be tried only by a
    /// fresh registration's commit. A registration kept after it failed
    /// (the helper could not confirm it let go) commits nothing, and when
    /// it was brought up later nothing asked Baloo again — the folder
    /// stayed indexed, and Baloo downloaded the whole drive. Every commit
    /// of a folder not recorded as excluded asks now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_folder_kept_after_a_failed_registration_is_kept_out_of_baloo_when_brought_up() {
        let w = world().await;
        let folder = std::fs::canonicalize(w.folder.path()).unwrap();
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let service = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
        helper.refuse(Seen::RegisterRoot, libc::EIO);
        helper.refuse(Seen::UnregisterRoot, libc::EIO);
        service.register_root(w.folder.path()).await.unwrap_err();
        assert!(service.root().is_some(), "kept: the helper may still hold it");
        assert_eq!(baloo_calls(&w), "");

        helper.refuse(Seen::RegisterRoot, 0);
        service.resume().await;

        assert_eq!(service.root_state(), "ready", "{}", service.last_error());
        assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));
        assert!(config_of(&w).sync_root_baloo_excluded);
        service.stop_sync().await;
    }

    /// A `SyncService` that never had `set_baloo` called on it — as a
    /// test that forgot to, would be — starts with a `Baloo` that runs
    /// no program at all, so it never reaches the real `balooctl6` or
    /// `~/.config/baloofilerc`, on this host or the one running CI. This
    /// deliberately does not go through `service`/`service_with`, which
    /// always install the fake.
    #[tokio::test]
    async fn a_service_without_set_baloo_runs_no_program_on_registration() {
        let w = world().await;
        let account = account(true);
        let service = SyncService::new(Some(link(&w).await), Some(account), Some(persist(&w.config.path().join("config.toml"))));
        let drive = DriveClient::new(Url::parse(&format!("{}/", w.server.uri())).unwrap(), Arc::new(StaticToken::new("T")))
            .unwrap();
        service.set_drive(drive);
        service.set_sync_paths(SyncPaths {
            tree_db: w.config.path().join("tree.sqlite"),
            rescue_dir: w.config.path().join("rescued"),
            thumbnails: Some(w.config.path().join("thumbnails")),
        });
        service.set_helper_socket(w.config.path().join("no-helper.sock"));
        // No `set_baloo`: the default `Baloo::disabled()` stands.

        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;

        assert!(!w.baloo.path().join("calls").exists(), "the fake was never even pointed to");
        assert!(!config_of(&w).sync_root_baloo_excluded, "nothing ran, so nothing was excluded");
        service.stop_sync().await;
    }

    /// A folder registered signed out is local, as in part 1 — and, since
    /// HS2, so is every folder registered without interception, signed
    /// in or not: that is the developer's mode, filled from a directory.
    #[tokio::test]
    async fn a_folder_registered_while_signed_out_or_without_interception_is_local() {
        for signed_in in [false, true] {
            let w = world().await;
            let service = service(&w, signed_in);
            service.register_root_without_interception(w.folder.path()).await.unwrap();
            assert_eq!(config_of(&w).sync_root_source, "local", "signed in: {signed_in}");
            let source = tempfile::tempdir().unwrap();
            std::fs::write(source.path().join("a.txt"), b"abc").unwrap();
            assert_eq!(service.populate_from_directory(source.path()).await.unwrap(), 1);
            assert_eq!(mode(&w.folder.path().join("a.txt")), 0o644, "no lock on a local folder");
            assert_eq!(requests(&w).await, 0, "a local folder never asks OneDrive");
        }
    }

    #[tokio::test]
    async fn populating_a_onedrive_folder_from_a_directory_is_refused() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        let source = tempfile::tempdir().unwrap();
        let err = service.populate_from_directory(source.path()).await.unwrap_err();
        assert!(matches!(err, SyncError::Unsupported(_)), "{err:?}");
        service.stop_sync().await;
    }

    #[tokio::test]
    async fn forgetting_a_onedrive_folder_stops_its_sync_unlocks_it_and_drops_its_tree() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        // A reader of the test's own — another program reading the store,
        // `sqlite3` say — so that the daemon's connection is not the last
        // one: SQLite then leaves its journal files when that closes, and
        // only the Forget itself removes them.
        let reader = rusqlite::Connection::open_with_flags(w.config.path().join("tree.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0)).unwrap();
        for name in ["tree.sqlite-wal", "tree.sqlite-shm"] {
            assert!(w.config.path().join(name).exists(), "no {name} to remove");
        }
        service.unregister_root().await.unwrap();
        let file = w.folder.path().join("docs/f.txt");
        assert!(file.is_file(), "the files stay (spec §3.1)");
        assert_eq!((mode(&file), mode(&w.folder.path().join("docs"))), (0o644, 0o755));
        for name in ["tree.sqlite", "tree.sqlite-wal", "tree.sqlite-shm"] {
            assert!(!w.config.path().join(name).exists(), "{name} was left");
        }
        drop(reader);
        assert_eq!(service.items(), (0, 0, 0));
        assert_eq!(service.root_state(), "none");
        assert_eq!(config_of(&w).sync_root_source, "local");
        let before = requests(&w).await;
        service.refresh_now();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(requests(&w).await, before, "nothing syncs any more");
    }

    /// A restart brings a OneDrive folder back syncing, and its first
    /// cycle reconciles the whole folder: the stored link has
    /// no changes since, so only a Full reconcile puts back the file
    /// removed while the daemon was down.
    ///
    /// The restarted daemon reads "signed out" (its Graph token here is
    /// static, so the cycle still succeeds): a restored folder keeps the
    /// source `config.toml` records, and a restart after a sign-out must
    /// not turn a OneDrive folder into a local one.
    #[tokio::test]
    async fn a_restart_brings_a_onedrive_folder_back_and_repairs_it() {
        let w = world().await;
        {
            let first = connected(&w, true).await;
            first.register_root(w.folder.path()).await.unwrap();
            listed(&first).await;
            first.stop_sync().await;
        }
        assert_eq!(config_of(&w).sync_root_source, "onedrive");
        std::process::Command::new("chmod").args(["-R", "u+w"]).arg(w.folder.path()).status().unwrap();
        std::fs::remove_file(w.folder.path().join("docs/f.txt")).unwrap();
        let listings = full_listings(&w).await;

        let second = connected(&w, false).await;
        second.restore().await;
        second.resume().await;
        wait_until("repaired by the first cycle's Full reconcile", || {
            w.folder.path().join("docs/f.txt").is_file()
        })
        .await;
        assert_eq!(full_listings(&w).await, listings, "asked from the stored link, not listed again");
        assert_eq!(config_of(&w).sync_root_source, "onedrive");
        second.stop_sync().await;
    }

    #[tokio::test]
    async fn refresh_runs_a_cycle_now() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let before = deltas(&w).await;
        service.refresh().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(deltas(&w).await, before + 1);
        service.stop_sync().await;
    }

    #[tokio::test]
    async fn refresh_on_a_local_folder_is_refused() {
        let w = world().await;
        let service = service(&w, false);
        service.register_root_without_interception(w.folder.path()).await.unwrap();
        assert!(matches!(service.refresh().await, Err(SyncError::Unsupported(_))));
    }

    #[tokio::test]
    async fn skipped_names_what_is_not_in_the_folder_by_its_full_path() {
        let w = world().await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [
                    {"id": "R", "root": {}, "folder": {}},
                    {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                    {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}},
                    {"id": "V", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}
                ],
                "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", w.server.uri())
            })))
            .with_priority(4)
            .mount(&w.server).await;
        let service = connected(&w, true).await;
        assert_eq!(service.skipped().await.unwrap(), Vec::<(String, String)>::new());
        service.register_root(w.folder.path()).await.unwrap();
        wait_until("listed", || service.items() == (3, 2, 1)).await;
        let vault = std::fs::canonicalize(w.folder.path()).unwrap().join("Personal Vault");
        assert_eq!(
            service.skipped().await.unwrap(),
            vec![(vault.display().to_string(), "personal-vault".to_owned())]
        );
        service.stop_sync().await;
    }

    /// A folder that reads "signed out" is brought up to date the moment
    /// the account signs in again, not up to a poll interval later (an
    /// hour here).
    #[tokio::test]
    async fn signing_in_brings_a_folder_that_reads_signed_out_up_to_date_at_once() {
        let w = world().await;
        let account = account(true);
        let tokens = Arc::new(AccountTokens { account: account.clone(), refused: AtomicUsize::new(0) });
        let service = service_with(&w, account.clone(), Some(link(&w).await), Arc::clone(&tokens) as Arc<dyn TokenSource>);
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;

        account.update(|s| s.state = SignInState::SignedOut);
        service.refresh_now();
        wait_until("the folder reads signed out", || service.root_state() == "error").await;
        assert!(service.last_error().contains("signed out"), "{}", service.last_error());
        // The one retry the schedule has, and then the hour-long wait.
        wait_until("the retry failed too", || tokens.refused.load(Ordering::SeqCst) >= 2).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(tokens.refused.load(Ordering::SeqCst), 2, "the poller waits out its interval now");

        let before = deltas(&w).await;
        account.update(|s| s.state = SignInState::SigningIn);
        account.update(|s| s.state = SignInState::SignedIn);
        wait_until("the folder is in step again", || service.root_state() == "ready").await;
        assert_eq!(deltas(&w).await, before + 1);
        service.stop_sync().await;
    }

    /// A Forget the helper refuses keeps the folder registered — and so
    /// locked, and kept in step.
    #[tokio::test]
    async fn a_forget_the_helper_refuses_leaves_the_folder_locked_and_in_step() {
        let w = world().await;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let service = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        helper.refuse(Seen::UnregisterRoot, libc::EIO);

        let refused = service.unregister_root().await;

        assert!(matches!(refused, Err(SyncError::Io(_))), "{refused:?}");
        assert_eq!(mode(&w.folder.path().join("docs/f.txt")), 0o444);
        assert!(w.config.path().join("tree.sqlite").exists());
        assert_eq!(config_of(&w).sync_root_source, "onedrive");
        let before = deltas(&w).await;
        service.refresh().await.unwrap();
        wait_for_deltas(&w, before).await;
        service.stop_sync().await;
    }

    /// A listing's reconcile takes the very lock registrations and
    /// Forgets take: while that is held, the listing waits. (A replacement
    /// of a changed file does not take it — it swaps in one file under its
    /// inode lock — so this is about the listing, not every change.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_listing_waits_for_the_services_lifecycle_lock() {
        let w = world().await;
        // The first listing answers late enough for the lock to be taken first.
        Mock::given(method("GET")).and(path("/me/drive/root/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [
                    {"id": "R", "root": {}, "folder": {}},
                    {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                    {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}}
                ],
                "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", w.server.uri())
            })).set_delay(Duration::from_millis(300)))
            .with_priority(4)
            .mount(&w.server).await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();

        let held = service.lifecycle.write().await;
        wait_for_deltas(&w, 0).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!w.folder.path().join("docs").exists(), "the folder was changed under the lock");
        drop(held);
        listed(&service).await;
        service.stop_sync().await;
    }

    /// A Forget stops the sync before it waits for the lifecycle lock, so
    /// that no reconcile keeps it waiting; a helper's reconnect that takes
    /// the lock first may start the sync again in between. That one is
    /// stopped too, before the folder is let go.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sync_started_again_while_a_forget_waits_is_stopped_too() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;

        let held = service.lifecycle.write().await;
        let forgetting = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.unregister_root().await })
        };
        wait_until("the Forget stopped the sync", || service.syncing.lock().unwrap().is_none()).await;
        // What a `resume` that has the lock does to a OneDrive folder.
        service.start_sync().await;
        drop(held);
        forgetting.await.unwrap().unwrap();

        let before = requests(&w).await;
        service.refresh_now();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(requests(&w).await, before, "a sync runs on a forgotten folder");
        assert_eq!(mode(&w.folder.path().join("docs/f.txt")), 0o644);
    }

    /// `start_sync` waits for the tree store to open before it keeps the
    /// sync it starts. Two of them at once — which only the lifecycle lock
    /// its callers hold keeps from happening — must still leave one sync
    /// running, not a second one that nothing could ever stop.
    #[tokio::test]
    async fn two_starts_at_once_leave_one_sync() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.stop_sync().await;
        let before = deltas(&w).await;

        tokio::join!(service.start_sync(), service.start_sync());
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(deltas(&w).await, before + 1, "two syncs ran their first cycle");
        service.stop_sync().await;
    }

    /// A folder whose sync could not start (F18: its tree store could not
    /// be opened) is not reported as refreshed: `Refresh()` tries to start
    /// it again, says why when it still cannot, and starts it once it can.
    #[tokio::test]
    async fn refresh_starts_a_sync_that_could_not_start_or_says_why() {
        let w = world().await;
        let service = connected(&w, true).await;
        // A file where the tree store's directory has to be.
        let blocker = w.config.path().join("state");
        std::fs::write(&blocker, b"").unwrap();
        service.set_sync_paths(SyncPaths {
            tree_db: blocker.join("tree.sqlite"),
            rescue_dir: w.config.path().join("rescued"),
            thumbnails: Some(w.config.path().join("thumbnails")),
        });
        service.register_root(w.folder.path()).await.unwrap();
        assert_eq!(service.root_state(), "error");

        let refused = service.refresh().await;
        assert!(
            matches!(&refused, Err(SyncError::Io(why)) if why.contains("the tree store cannot be opened")),
            "{refused:?}"
        );
        assert_eq!(requests(&w).await, 0, "nothing synced");

        std::fs::remove_file(&blocker).unwrap();
        service.refresh().await.unwrap();
        listed(&service).await;
        assert_eq!(service.root_state(), "ready");
        service.stop_sync().await;
    }

    /// A folder held at startup until its helper is back has not been
    /// brought up — nor recovered — yet: `Refresh()` says so rather than
    /// start its sync ahead of that.
    #[tokio::test]
    async fn refresh_of_a_folder_waiting_for_its_helper_says_so() {
        let w = world().await;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        {
            let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
            let first = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
            first.register_root(w.folder.path()).await.unwrap();
            listed(&first).await;
            first.stop_sync().await;
        }
        let restarted = service(&w, true);
        restarted.restore().await;
        let before = requests(&w).await;

        let refused = restarted.refresh().await;

        assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(requests(&w).await, before, "a sync started ahead of the bring-up");
        restarted.stop_sync().await;
    }

    /// A OneDrive folder is locked read-only after its first listing,
    /// the folder itself too, and bringing it up again after a restart
    /// re-checked it with a write probe — refused, so no locked folder came
    /// back after a restart, in either mode: "cannot bring up the sync
    /// folder: Permission denied". Found by, whose switch to
    /// interception goes through the same check. A folder that already
    /// carries its root id was probed when it was first registered, and is
    /// not probed again — the helper's own re-registration skips its probe
    /// for the same reason. The mode without interception is
    /// a folder recorded that way before HS2 (`legacy_without_interception`):
    /// no new OneDrive folder is made so.
    #[tokio::test]
    async fn a_locked_onedrive_folder_comes_back_after_a_restart_in_either_mode() {
        for intercepted in [false, true] {
            let w = world().await;
            {
                let first = connected(&w, true).await;
                first.register_root(w.folder.path()).await.unwrap();
                listed(&first).await;
                first.stop_sync().await;
                first.set_link(None);
            }
            if !intercepted {
                legacy_without_interception(&w);
            }
            assert_eq!(mode(w.folder.path()), 0o555, "the folder itself is locked");

            let restarted = connected(&w, true).await;
            restarted.restore().await;
            restarted.resume().await;

            assert!(
                !restarted.last_error().contains("cannot bring up"),
                "intercepted = {intercepted}: {}",
                restarted.last_error()
            );
            assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
            assert_eq!(mode(w.folder.path()), 0o555, "and it stays locked");
            restarted.stop_sync().await;
        }
    }

    /// Write design §3.9: the account turning read-write takes the read-only lock off
    /// its folder — files `0644`, directories `0755`, the folder itself last — and the
    /// sync that starts again leaves it off through a Full reconcile; turning read-only
    /// puts it back on at once. A folder brought up read-write with the lock still on — a
    /// switch cut short — loses it as it comes up.
    #[tokio::test]
    async fn the_lock_comes_off_and_goes_back_on_with_the_mode() {
        use crate::config::Mode;
        let w = world().await;
        let modes = || {
            let folder = w.folder.path();
            (mode(&folder.join("docs/f.txt")), mode(&folder.join("docs")), mode(folder))
        };
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        assert_eq!(modes(), (0o444, 0o555, 0o555));

        let before = deltas(&w).await;
        service.follow_mode(Mode::ReadWrite).await;
        assert_eq!(modes(), (0o644, 0o755, 0o755));
        // The sync runs again: its first cycle, a Full reconcile, is over once a second
        // cycle has asked for changes.
        wait_for_deltas(&w, before).await;
        service.refresh().await.unwrap();
        wait_for_deltas(&w, before + 1).await;
        assert_eq!(modes(), (0o644, 0o755, 0o755), "a read-write folder's reconcile leaves the lock off");

        service.follow_mode(Mode::ReadOnly).await;
        assert_eq!(modes(), (0o444, 0o555, 0o555));
        service.stop_sync().await;
        service.set_link(None);

        // Read-write in config.toml again, but the walk never ran: the next bring-up runs it.
        let restarted = connected(&w, true).await;
        restarted.start_in_mode(Mode::ReadWrite);
        restarted.restore().await;
        restarted.resume().await;
        assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
        assert_eq!(modes(), (0o644, 0o755, 0o755));
        restarted.stop_sync().await;
        restarted.set_link(None);

        // Read-only again, and the lock walk never ran — the daemon stopped
        // first. The bring-up puts the lock back at once, with no Full reconcile to do it:
        // this one cannot reach OneDrive.
        let offline = Arc::new(StaticToken::new("T"));
        offline.invalidate().await;
        let read_only = service_with(&w, account(true), Some(link(&w).await), offline);
        read_only.restore().await;
        read_only.resume().await;
        assert_eq!(read_only.mode(), Mode::ReadOnly);
        assert_eq!(modes(), (0o444, 0o555, 0o555), "never writable while the account is read-only");
        read_only.stop_sync().await;
    }

    /// the watcher on the mode switch's hooks: a read-write folder's sync runs the watcher (the lock came off
    /// once it had walked the folder), and a file another process makes in the folder is
    /// counted as waiting to be uploaded the moment the switch to read-only asks, quiet
    /// spell or not (the watcher); a read-only folder has no watcher.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_made_in_a_read_write_folder_waits_to_be_uploaded() {
        use crate::account::PendingUploads;
        use crate::config::Mode;
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let watching = |service: &SyncService| service.syncing.lock().unwrap().as_ref().is_some_and(|s| s.watcher.is_some());
        assert!(!watching(&service), "read-only");

        service.follow_mode(Mode::ReadWrite).await;
        assert!(watching(&service));
        assert_eq!(mode(&w.folder.path().join("docs")), 0o755);
        let made = std::process::Command::new("sh")
            .args(["-c", "echo new > docs/new.txt"])
            .current_dir(w.folder.path())
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(service.pending_uploads().await, 1);
        // `PendingCount` as the worker counts it.
        wait_until("the worker counted the change", || service.state().get().pending_count == 1).await;

        // A forced switch (only that drops them): the drop, then the
        // folder follows.
        service.drop_pending_uploads().await;
        service.follow_mode(Mode::ReadOnly).await;
        assert!(!watching(&service), "stopped with the sync");
        // A read-only folder uploads nothing; its rows are dropped, the file stays.
        assert_eq!(service.pending_uploads().await, 0);
        // the outbox on the bus: and the bus says so.
        assert_eq!(service.state().get().pending_count, 0);
        assert!(w.folder.path().join("docs/new.txt").exists());
        service.stop_sync().await;
    }

    /// the outbox worker on the mode switch's and the watcher's hooks: a read-write folder's sync runs the outbox worker beside
    /// the watcher; a file made in the folder is examined, the worker is woken, and the
    /// file goes up and is committed — its item id on it, nothing left waiting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_made_in_a_read_write_folder_is_uploaded() {
        use crate::account::PendingUploads;
        use crate::config::Mode;
        use wiremock::matchers::path_regex;
        let w = world().await;
        let mut hasher = crate::quickxor::QuickXor::new();
        hasher.update(b"new\n");
        Mock::given(method("POST"))
            .and(path_regex("/me/drive/items/D:/new.txt:/createUploadSession$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uploadUrl": format!("{}/upload/s1", w.server.uri()),
                "expirationDateTime": "2099-01-01T00:00:00Z"
            })))
            .mount(&w.server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/s1"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "N1", "name": "new.txt", "size": 4, "eTag": "e-N1", "cTag": "c-N1",
                "parentReference": {"id": "D"},
                "file": {"hashes": {"quickXorHash": hasher.finish_base64()}}
            })))
            .mount(&w.server)
            .await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let_write(&service);
        service.follow_mode(Mode::ReadWrite).await;
        assert!(service.syncing.lock().unwrap().as_ref().is_some_and(|s| s.outbox.is_some()), "beside the watcher");
        let mut announced = service.report().activity.subscribe();
        let made = std::process::Command::new("sh")
            .args(["-c", "echo new > docs/new.txt"])
            .current_dir(w.folder.path())
            .status()
            .unwrap();
        assert!(made.success());
        let file = w.folder.path().join("docs/new.txt");
        let committed = || xattr::get(&file, konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while committed().is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(committed().as_deref(), Some(&b"N1"[..]), "uploaded and committed");
        assert_eq!(service.pending_uploads().await, 0);
        // The live signal, and the counts the bus publishes.
        let event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = announced.recv().await.unwrap();
                if event.kind == "uploaded" {
                    return event;
                }
            }
        })
        .await
        .expect("ActivityLog.Added for the upload");
        assert_eq!(event.path, file.display().to_string());
        wait_until("the outbox counted empty", || service.state().get().pending_count == 0).await;
        assert!(service.state().get().uploads.is_empty());

        service.follow_mode(Mode::ReadOnly).await;
        assert!(service.syncing.lock().unwrap().as_ref().is_some_and(|s| s.outbox.is_none()), "stopped with the sync");
        service.stop_sync().await;
    }

    /// the outbox on the bus, `Pause`: a paused account asks OneDrive for nothing — not on
    /// `Refresh`, not after a restart, since the pause is kept in the tree
    /// store — until `Resume`; a timed pause ends by itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pause_holds_the_poll_outlasts_a_restart_and_ends_by_itself() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.pause_syncing(0).await.unwrap();
        assert_eq!(service.state().get().paused_until, Some(0));
        let before = deltas(&w).await;
        service.refresh().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(deltas(&w).await, before, "a paused account asks OneDrive for nothing");
        service.stop_sync().await;
        service.set_link(None);

        let restarted = connected(&w, true).await;
        restarted.restore().await;
        restarted.resume().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(restarted.state().get().paused_until, Some(0), "the pause outlasts a restart");
        assert_eq!(deltas(&w).await, before);
        restarted.resume_syncing().await.unwrap();
        wait_for_deltas(&w, before).await;
        assert_eq!(restarted.state().get().paused_until, None);

        restarted.pause_syncing(1).await.unwrap();
        assert!(restarted.state().get().paused_until.is_some_and(|until| until > 0));
        wait_until("the timed pause ends by itself", || restarted.state().get().paused_until.is_none()).await;
        restarted.stop_sync().await;
    }

    /// the outbox on the bus: the outbox as the bus shows it — `Changes()`, `NotUploaded()`, the
    /// mass-delete guard's two answers — and a free-up of a file whose change
    /// waits to be uploaded, refused `NotUploaded`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_outbox_is_listed_decided_on_and_its_files_are_not_freed_up() {
        use crate::tree::outbox::{Base, Detection, OutboxKind, OutboxState};
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let store = service.store.lock().unwrap().clone().unwrap();
        let file = w.folder.path().join("docs/f.txt");
        let base = Base { etag: None, ctag: Some("c1".into()), parent: Some("D".into()), name: Some("f.txt".into()) };
        let change = Detection {
            kind: OutboxKind::Update,
            item_id: Some("F".into()),
            inode: None,
            rel: "docs/f.txt".into(),
            base: Some(base.clone()),
            target_parent: Some("D".into()),
            target_name: Some("f.txt".into()),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: None,
        };
        store.call(move |s| s.outbox_record(&change)).await.unwrap();

        let rows = service.outbox(0).await.unwrap();
        assert_eq!(rows.len(), 1);
        let (_, kind, path, state, _, _, _, _) = &rows[0];
        assert_eq!((kind.as_str(), path.as_str(), state.as_str()), ("update", file.to_str().unwrap(), "ready"));
        // A placeholder has nothing to lose: its own refusal.
        assert!(matches!(service.dehydrate(&file).await, Err(SyncError::NotHydrated)));
        // Downloaded (by hand: the world serves no content), it is refused
        // NotUploaded, whole calls included, and stays downloaded.
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        {
            let opened = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
            use std::io::Write as _;
            (&opened).write_all(b"abc").unwrap();
            konedrive_fs::placeholder::write_state(&opened, konedrive_fs::placeholder::State::Hydrated).unwrap();
            konedrive_fs::placeholder::write_stamp(&opened).unwrap();
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        let refused = service.dehydrate(&file).await.unwrap_err();
        assert!(matches!(refused, SyncError::NotUploaded(_)), "{refused:?}");
        assert!(matches!(service.check_free_up(std::slice::from_ref(&file)).await, Err(SyncError::NotUploaded(_))), "before anything changes");
        assert!(matches!(service.free_up(std::slice::from_ref(&file)).await, Err(SyncError::NotUploaded(_))));
        assert_eq!(std::fs::read(&file).unwrap(), b"abc", "still downloaded");

        let seq = rows[0].0 as i64;
        store.call(move |s| s.outbox_set_state(seq, OutboxState::Blocked, Some("name-characters"), None)).await.unwrap();
        assert_eq!(service.not_uploaded().await.unwrap(), vec![(file.display().to_string(), "name-characters".to_owned())]);

        store.call(move |s| s.outbox_set_state(seq, OutboxState::Held, Some("mass-delete"), None)).await.unwrap();
        assert_eq!(service.confirm_deletes().await.unwrap(), 1);
        assert_eq!(service.outbox(0).await.unwrap()[0].3, "ready");
        store.call(move |s| s.outbox_set_state(seq, OutboxState::Held, Some("mass-delete"), None)).await.unwrap();
        assert_eq!(service.restore_deletes().await.unwrap(), 1);
        assert!(service.outbox(0).await.unwrap().is_empty());
        service.stop_sync().await;
    }

    /// Issue #38: while an examination's apply holds the store, the bus still
    /// answers at once — the counts and the Not Uploaded summary from memory,
    /// `Changes()` and `NotUploadedFiles()` through the read-only connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_bus_answers_while_the_store_is_held() {
        use crate::tree::outbox::{Base, Detection, OutboxKind, OutboxState};
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.follow_mode(Mode::ReadWrite).await;
        let store = service.store.lock().unwrap().clone().unwrap();
        let blocked = Detection {
            kind: OutboxKind::Update,
            item_id: Some("F".into()),
            inode: None,
            rel: "docs/f.txt".into(),
            base: Some(Base { etag: None, ctag: Some("c1".into()), parent: Some("D".into()), name: Some("f.txt".into()) }),
            target_parent: Some("D".into()),
            target_name: Some("f.txt".into()),
            same_content: false,
            state: OutboxState::Blocked,
            reason: Some("name-characters".into()),
            next_try: None,
            size: Some(3),
        };
        store.call(move |s| s.outbox_record(&blocked)).await.unwrap();
        wait_until("BlockedCount counts it", || service.state().get().blocked_count == 1).await;
        wait_until("the summary is summed", || service.kept_back.lock().unwrap().as_ref().is_some_and(|k| k.iter().any(|r| r.1 == "name-characters"))).await;

        // An apply that holds the store for two seconds.
        let (held, release) = std::sync::mpsc::channel::<()>();
        let holder = store.clone();
        let holding = std::thread::spawn(move || {
            holder.call_blocking(move |_| {
                held.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(2));
                Ok(())
            })
        });
        release.recv().unwrap();
        let start = std::time::Instant::now();
        assert_eq!(service.state().get().blocked_count, 1);
        let summary = service.not_uploaded_summary().await.unwrap();
        assert_eq!(summary, vec![("per-file".to_owned(), "name-characters".to_owned(), 1, 3)]);
        assert_eq!(service.outbox(21).await.unwrap().len(), 1);
        assert_eq!(service.not_uploaded_files("name-characters".into(), 20).await.unwrap().1, 1);
        let took = start.elapsed();
        assert!(took < Duration::from_millis(500), "the bus waited for the store: {took:?}");
        holding.join().unwrap().unwrap();
        service.stop_sync().await;
    }

    /// the outbox on the bus: deletes held by the mass-delete guard are counted
    /// on the bus (`HeldCount`), and `RestoreDeletes` brings the files back
    /// at once — a Full reconcile, though OneDrive did not change them — and
    /// deletes nothing in OneDrive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restoring_held_deletes_brings_the_files_back_at_once() {
        use crate::tree::outbox::{Base, Detection, OutboxKind, OutboxState};
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.follow_mode(Mode::ReadWrite).await;
        let file = w.folder.path().join("docs/f.txt");
        std::fs::remove_file(&file).unwrap();
        let store = service.store.lock().unwrap().clone().unwrap();
        let held = Detection {
            kind: OutboxKind::Delete,
            item_id: Some("F".into()),
            inode: None,
            rel: "docs/f.txt".into(),
            base: Some(Base { etag: None, ctag: Some("c1".into()), parent: Some("D".into()), name: Some("f.txt".into()) }),
            target_parent: None,
            target_name: None,
            same_content: false,
            state: OutboxState::Held,
            reason: Some("mass-delete".into()),
            next_try: None,
            size: None,
        };
        store.call(move |s| s.outbox_record(&held)).await.unwrap();
        service.wake_outbox();
        wait_until("HeldCount counts it", || service.state().get().held_count == 1).await;

        assert_eq!(service.restore_deletes().await.unwrap(), 1);
        wait_until("the file is placed again at once", || file.exists()).await;
        wait_until("HeldCount is 0 again", || service.state().get().held_count == 0).await;
        let deleted = w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "DELETE").count();
        assert_eq!(deleted, 0, "nothing is deleted in OneDrive");
        service.stop_sync().await;
    }

    /// the outbox on the bus: a free-up of a downloaded file in a OneDrive folder whose
    /// outbox cannot be read is refused, not let through; the file stays.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_free_up_that_cannot_tell_whether_a_change_waits_refuses() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.follow_mode(Mode::ReadWrite).await;
        let file = w.folder.path().join("docs/f.txt");
        {
            let opened = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
            use std::io::Write as _;
            (&opened).write_all(b"abc").unwrap();
            konedrive_fs::placeholder::write_state(&opened, konedrive_fs::placeholder::State::Hydrated).unwrap();
            konedrive_fs::placeholder::write_stamp(&opened).unwrap();
        }
        service.stop_sync().await;
        *service.store.lock().unwrap() = None;
        let refused = service.dehydrate(&file).await.unwrap_err();
        assert!(matches!(&refused, SyncError::Io(why) if why.contains("cannot tell")), "{refused:?}");
        assert_eq!(std::fs::read(&file).unwrap(), b"abc", "still downloaded");
    }

    /// the outbox on the bus: a directory of the user's own that is newly ignored
    /// takes the changes waiting inside it along — the outbox empties — and
    /// nothing in it is uploaded, then or later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ignored_directory_keeps_everything_in_it_local() {
        use crate::account::PendingUploads;
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.follow_mode(Mode::ReadWrite).await;
        service.pause_syncing(0).await.unwrap();
        let made = std::process::Command::new("sh")
            .args(["-c", "mkdir -p build/obj && echo a > build/a.o && echo b > build/obj/b.o"])
            .current_dir(w.folder.path())
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(service.pending_uploads().await, 4, "the folders and the files wait");

        let mut patterns = service.ignore_patterns();
        patterns.push("build".into());
        service.set_ignore_patterns(patterns).await.unwrap();
        let mut left = u64::MAX;
        for _ in 0..200 {
            left = service.pending_uploads().await;
            if left == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(left, 0, "the outbox empties");
        std::fs::write(w.folder.path().join("build/c.o"), b"c").unwrap();
        assert_eq!(service.pending_uploads().await, 0, "and stays empty");

        service.resume_syncing().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let uploads = w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() != "GET").count();
        assert_eq!(uploads, 0, "nothing in it is sent");
        service.stop_sync().await;
    }

    /// the outbox on the bus: a new file whose folder OneDrive no longer has asks
    /// for a cycle, not a Full reconcile — which, before the read-write reconcile, put a rename
    /// still waiting to go up back where the base has it (F63, closed). The
    /// rename waiting beside it stands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_folder_asks_for_a_cycle_that_leaves_waiting_renames_alone() {
        use crate::config::Mode;
        let w = world().await;
        // The rename cannot reach OneDrive yet; the new file's folder is
        // gone there (nothing mocked for it: 404).
        Mock::given(method("PATCH"))
            .and(path("/me/drive/items/F"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&w.server)
            .await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let_write(&service);
        let before = deltas(&w).await;
        service.follow_mode(Mode::ReadWrite).await;
        // The switch's own Full cycle is over once a later one has begun.
        wait_for_deltas(&w, before).await;
        service.refresh().await.unwrap();
        wait_for_deltas(&w, before + 1).await;

        let made = std::process::Command::new("sh")
            .args(["-c", "mv docs/f.txt docs/g.txt && echo new > docs/new.txt"])
            .current_dir(w.folder.path())
            .status()
            .unwrap();
        assert!(made.success());
        async fn asked(w: &World) -> usize {
            w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("createUploadSession")).count()
        }
        for _ in 0..300 {
            if asked(&w).await > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(asked(&w).await > 0, "the new file was sent");
        // The cycle the missing folder asked for is over once a later one
        // has begun.
        let seen = deltas(&w).await;
        wait_for_deltas(&w, seen).await;
        service.refresh().await.unwrap();
        wait_for_deltas(&w, seen + 1).await;
        let docs = w.folder.path().join("docs");
        assert!(docs.join("g.txt").exists() && !docs.join("f.txt").exists(), "the rename waiting to go up stands");
        assert!(docs.join("new.txt").exists());
        service.stop_sync().await;
    }

    /// the outbox on the bus: a `Pause` that lands after the timer read the
    /// store's pause as over is not undone on the bus: the timer looks
    /// again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pause_that_lands_as_the_last_one_ends_stands() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.pause_syncing(3600).await.unwrap();
        // The timer has read the store's pause as over…
        let seen = service.pause_shown.load(std::sync::atomic::Ordering::SeqCst);
        // …when a new `Pause` lands.
        service.pause_syncing(7200).await.unwrap();
        assert!(!service.pause_timer_done(seen, true), "the timer looks again");
        assert!(service.state().get().paused_until.is_some_and(|until| until > 0), "still paused on the bus");
        service.stop_sync().await;
    }

    /// The outbox worker asks the write gate before each row. A drive
    /// taken off `write_test_drive_ids` while it runs sends nothing more: the change waits,
    /// and the folder's `LastError` says why.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drive_taken_off_the_list_while_the_worker_runs_sends_nothing_more() {
        use crate::account::PendingUploads;
        use crate::config::{ConfigError, Mode};
        use wiremock::matchers::path_regex;
        let w = world().await;
        let mut hasher = crate::quickxor::QuickXor::new();
        hasher.update(b"new\n");
        Mock::given(method("POST"))
            .and(path_regex("createUploadSession$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uploadUrl": format!("{}/upload/s1", w.server.uri()),
                "expirationDateTime": "2099-01-01T00:00:00Z"
            })))
            .mount(&w.server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/s1"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "N1", "name": "new.txt", "size": 4, "eTag": "e-N1", "cTag": "c-N1",
                "parentReference": {"id": "D"},
                "file": {"hashes": {"quickXorHash": hasher.finish_base64()}}
            })))
            .mount(&w.server)
            .await;
        async fn sent(w: &World) -> usize {
            w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() != "GET").count()
        }
        let make = |script: &str| {
            let made = std::process::Command::new("sh").args(["-c", script]).current_dir(w.folder.path()).status().unwrap();
            assert!(made.success());
        };
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let_write(&service);
        service.follow_mode(Mode::ReadWrite).await;
        make("echo new > docs/new.txt");
        let first = w.folder.path().join("docs/new.txt");
        wait_until("the first change goes up", || {
            xattr::get(&first, konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten().is_some()
        })
        .await;
        let before = sent(&w).await;

        let persist = service.persist.as_ref().unwrap();
        persist
            .store
            .update(|c| {
                c.write_test_drive_ids.clear();
                Ok::<_, ConfigError>(())
            })
            .unwrap();
        make("echo second > docs/second.txt");
        assert_eq!(service.pending_uploads().await, 1, "the change is recorded");
        wait_until("the folder says why nothing goes", || {
            crate::sync::published_error(&service.state().get()).contains("write_test_drive_ids")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(sent(&w).await, before, "nothing more is sent");
        assert_eq!(service.pending_uploads().await, 1, "the change waits");
        service.stop_sync().await;
    }

    /// A switch to read-only nobody forced keeps the changes waiting to
    /// upload, the folder is locked, and its sync holds its cycles while they wait: no
    /// read-only reconcile puts back what they describe. `expired`: the sign-in expired
    /// (`invalid_grant`, as the token manager records it); otherwise a sign-out.
    async fn a_switch_nobody_forced_keeps_the_changes(expired: bool) {
        use crate::account::PendingUploads;
        use crate::config::Mode;
        let w = world().await;
        // The rename cannot reach OneDrive yet.
        Mock::given(method("PATCH"))
            .and(path("/me/drive/items/F"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&w.server)
            .await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let_write(&service);
        let account = service.account.clone().unwrap();
        let follower = tokio::spawn(crate::sync::write_mode::follow(account.subscribe(), Arc::downgrade(&service)));
        let docs = w.folder.path().join("docs");
        wait_until("the folder is read-write", || service.mode() == Mode::ReadWrite && mode(&docs) == 0o755).await;
        let made = std::process::Command::new("sh").args(["-c", "mv docs/f.txt docs/g.txt"]).current_dir(w.folder.path()).status().unwrap();
        assert!(made.success());
        assert_eq!(service.pending_uploads().await, 1);

        account.update(|s| {
            s.state = SignInState::SignedOut;
            if expired {
                s.last_error = crate::token::SESSION_EXPIRED.into();
            }
            s.clear_account();
        });
        wait_until("the folder holds its cycles", || {
            crate::sync::published_error(&service.state().get()).contains("wait to be uploaded")
        })
        .await;
        assert_eq!(service.mode(), Mode::ReadOnly);
        let before = deltas(&w).await;
        service.nudge();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(deltas(&w).await, before, "no cycle while the change waits");
        assert!(docs.join("g.txt").exists() && !docs.join("f.txt").exists(), "nothing local is put back");
        assert_eq!(mode(&docs), 0o555, "the folder is locked");
        assert_eq!(service.pending_uploads().await, 1, "the change still waits");
        follower.abort();
        service.stop_sync().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sign_out_keeps_the_changes_waiting_to_upload() {
        a_switch_nobody_forced_keeps_the_changes(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_expired_sign_in_keeps_the_changes_waiting_to_upload() {
        a_switch_nobody_forced_keeps_the_changes(true).await;
    }

    /// A Forget — and so `Accounts.Remove`, which forgets first — is
    /// refused `PendingUploads` while changes wait to be uploaded, and changes nothing; once
    /// a forced switch to read-only has dropped them, it goes through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_folder_whose_changes_wait_is_not_forgotten() {
        use crate::account::PendingUploads;
        use crate::config::Mode;
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        // The gate stays closed: the rename waits.
        service.follow_mode(Mode::ReadWrite).await;
        let made = std::process::Command::new("sh").args(["-c", "mv docs/f.txt docs/g.txt"]).current_dir(w.folder.path()).status().unwrap();
        assert!(made.success());
        assert_eq!(service.pending_uploads().await, 1);

        let refused = service.unregister_root().await.unwrap_err();
        assert!(matches!(&refused, SyncError::PendingUploads(why) if why.starts_with("1 change")), "{refused:?}");
        assert!(matches!(crate::sync::dbus::to_fault(refused), crate::sync::dbus::SyncFault::PendingUploads(_)));
        assert!(matches!(service.retire().await, Err(SyncError::PendingUploads(_))), "Remove's first step too");
        assert!(service.registration().is_some(), "still registered");
        assert_eq!(service.pending_uploads().await, 1, "the change still waits");
        assert!(w.folder.path().join("docs/g.txt").exists());

        // A forced switch drops them: the drop, then the folder follows.
        service.drop_pending_uploads().await;
        service.follow_mode(Mode::ReadOnly).await;
        service.unregister_root().await.unwrap();
    }

    /// the outbox on the bus: a forgotten folder is no longer paused on the bus.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forgotten_folder_is_not_paused() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.pause_syncing(3600).await.unwrap();
        assert!(service.state().get().paused_until.is_some());
        service.unregister_root().await.unwrap();
        assert_eq!(service.state().get().paused_until, None);
    }

    /// Issue #80: the thumbnail setting is absent from `config.toml` until set, reads its
    /// default then, is written when set and taken at once — thumbnails off stop nothing
    /// else — and is read back by the next start; a local folder has no settings to set.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_thumbnail_setting_is_kept_in_config_toml_and_taken_at_once() {
        let local = world().await;
        let other = service(&local, true);
        other.register_root_without_interception(local.folder.path()).await.unwrap();
        assert!(matches!(other.change_run_settings(|s| s.thumbnails = false).await, Err(SyncError::Unsupported(_))));

        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        // Read afresh each time: what the file holds now.
        let written = || {
            let persist = persist(&w.config.path().join("config.toml"));
            persist.store.account(&persist.account).unwrap()
        };
        assert_eq!(service.run_settings(), running::Settings::default(), "absent means the default");
        assert_eq!(written().thumbnails, None);

        service.change_run_settings(|s| s.thumbnails = false).await.unwrap();
        assert!(!service.run_settings().thumbnails);
        assert_eq!(written().thumbnails, Some(false));
        let store = service.store.lock().unwrap().clone().unwrap();
        assert!(!service.running.stopped(&store) && !service.running.thumbnails_go(&store), "thumbnails off stop nothing else");

        service.stop_sync().await;
        service.set_link(None);
        drop(service);
        let restarted = connected(&w, true).await;
        assert_eq!(restarted.run_settings(), running::Settings { thumbnails: false });
    }

    /// Issue #95: the hold's settings are one pair for every account. A change on the hub
    /// reaches every account's hold at once and ends every account's `SyncAnyway`; the
    /// same settings told again end nothing; an account that joins later runs on them.
    #[tokio::test]
    async fn the_hold_settings_reach_every_account_and_end_every_sync_anyway() {
        use crate::config::OnBattery;
        use running::{Hold, HoldSettings};
        let hub = hub::HelperHub::new();
        let accounts = [SyncService::on_hub(&hub, None, None), SyncService::on_hub(&hub, None, None)];
        hub.set_conditions(running::Conditions { metered: true, on_battery: true, power_saver: false });
        for account in &accounts {
            assert_eq!(account.running.held(), Some(Hold::Metered));
            account.running.sync_anyway();
        }
        let ignoring_metered = HoldSettings { pause_on_metered: false, on_battery: OnBattery::Pause };
        hub.set_hold_settings(ignoring_metered);
        for account in &accounts {
            assert_eq!(account.hold_settings(), ignoring_metered);
            assert_eq!(account.running.held(), Some(Hold::OnBattery), "worked out again: the SyncAnyway ended");
            account.running.sync_anyway();
        }
        hub.set_hold_settings(ignoring_metered);
        assert!(accounts.iter().all(|a| a.running.held().is_none()), "the same again ends nothing");
        hub.set_hold_settings(HoldSettings { on_battery: OnBattery::Sync, ..ignoring_metered });
        assert!(accounts.iter().all(|a| a.running.held().is_none()), "sync on battery");
        let later = SyncService::on_hub(&hub, None, None);
        assert_eq!(later.hold_settings().on_battery, OnBattery::Sync, "a later account is told");
    }

    /// Issue #54: with the notification socket up, a change in OneDrive gives one delta
    /// within the debounce, and `LiveChanges` says `connected`; the user's pause closes the
    /// socket (`off`) and Resume opens it again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_change_in_onedrive_arrives_through_the_socket_and_a_pause_closes_it() {
        use crate::sync::live::{LiveChanges, Timing};
        use crate::sync::upload::fake::{FakeGraph, ROOT};
        let w = world().await;
        // The fake OneDrive for its socket only; the world's own server serves the rest.
        let graph = FakeGraph::start().await;
        let endpoint = graph.client().socket_endpoint().await.unwrap();
        Mock::given(method("GET")).and(path("/me/drive/root/subscriptions/socketIo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"notificationUrl": endpoint.notification_url.as_str()})))
            .mount(&w.server).await;
        let service = connected(&w, true).await;
        let live = Timing { debounce: Duration::from_millis(300), settle: Duration::from_millis(100), ..Timing::default() };
        service.set_schedule(Schedule { live: Some(live), ..Schedule::polled(Duration::from_secs(3600), vec![Duration::from_millis(50)]) });
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        wait_until("connected", || service.state().get().live_changes == LiveChanges::Connected).await;
        let before = deltas(&w).await;

        graph.with(|c| c.add_file("X", ROOT, "x.txt", b"x"));
        wait_for_deltas(&w, before).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(deltas(&w).await, before + 1, "one delta for the event");

        service.pause_syncing(0).await.unwrap();
        wait_until("closed by the pause", || service.state().get().live_changes == LiveChanges::Off && graph.sockets.open() == 0).await;
        service.resume_syncing().await.unwrap();
        wait_until("open again", || service.state().get().live_changes == LiveChanges::Connected).await;
        service.stop_sync().await;
        assert_eq!(service.state().get().live_changes, LiveChanges::Off, "no sync, no socket");
    }

    /// Issue #57: on a metered connection the account holds back — no upload, no poll, no
    /// pinned download or thumbnail (the pool gives no slot but for opens) — while an open
    /// still gets its slot, `HeldBack` says why and `Paused` stays false; when the
    /// connection is no longer metered, what waited goes at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_metered_connection_holds_the_account_back_until_it_ends() {
        use crate::account::PendingUploads;
        use crate::config::Mode;
        use crate::pool::{Class, Size};
        use wiremock::matchers::path_regex;
        let w = world().await;
        Mock::given(method("POST"))
            .and(path_regex("/me/drive/items/D:/new.txt:/createUploadSession$"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&w.server)
            .await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let_write(&service);
        service.follow_mode(Mode::ReadWrite).await;
        let before = deltas(&w).await;
        wait_for_deltas(&w, before).await;

        service.set_conditions(running::Conditions { metered: true, ..running::Conditions::default() });
        assert_eq!(service.state().get().held_back, "metered");
        assert_eq!(service.state().get().paused_until, None, "a hold is not the user's pause");
        let store = service.store.lock().unwrap().clone().unwrap();
        assert!(!service.running.thumbnails_go(&store), "no thumbnails");
        assert!(service.pool.try_acquire_sized(Class::Download, Size::Small).is_none(), "no pinned download");
        assert!(service.pool.try_acquire_sized(Class::Open, Size::Small).is_some(), "an open still downloads");
        let made = std::process::Command::new("sh").args(["-c", "echo new > docs/new.txt"]).current_dir(w.folder.path()).status().unwrap();
        assert!(made.success());
        let seen = deltas(&w).await;
        service.refresh().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(deltas(&w).await, seen, "OneDrive is not asked");
        assert_eq!(service.pending_uploads().await, 1, "the change waits");
        let asked = || async { w.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST").count() };
        assert_eq!(asked().await, 0, "nothing is uploaded");
        assert!(service.outbox(0).await.unwrap().iter().all(|row| row.3 == "paused"), "the rows read paused");

        service.set_conditions(running::Conditions::default());
        assert_eq!(service.state().get().held_back, "");
        wait_for_deltas(&w, seen).await;
        for _ in 0..250 {
            if asked().await > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(asked().await > 0, "the upload is tried at once");
        service.stop_sync().await;
    }

    /// Issue #57: the user's pause and a hold are both on — `Resume` alone does not start
    /// the account while the hold is on, nor does the hold's end alone while the pause is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_account_runs_only_when_neither_a_pause_nor_a_hold_is_on() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let metered = running::Conditions { metered: true, ..running::Conditions::default() };
        let quiet = |what: &'static str| {
            let (w, service) = (&w, &service);
            async move {
                let seen = deltas(w).await;
                service.refresh().await.unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(deltas(w).await, seen, "{what}");
            }
        };

        service.pause_syncing(0).await.unwrap();
        service.set_conditions(metered);
        service.resume_syncing().await.unwrap();
        quiet("resumed, still held").await;
        service.pause_syncing(0).await.unwrap();
        service.set_conditions(running::Conditions::default());
        quiet("the hold ended, still paused").await;
        let seen = deltas(&w).await;
        service.resume_syncing().await.unwrap();
        wait_for_deltas(&w, seen).await;
        service.stop_sync().await;
    }

    /// Issue #57: `SyncAnyway` lifts the hold at once, until a source or the global hold
    /// settings change; then the hold is worked out again. After a restart with the
    /// condition still on, the account is held again and `Paused` stays false.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_anyway_lifts_the_hold_until_something_changes_and_a_restart_holds_again() {
        use crate::config::OnBattery;
        use running::HoldSettings;
        let hold = |on_battery| HoldSettings { on_battery, ..HoldSettings::default() };
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        let on_battery = running::Conditions { on_battery: true, ..running::Conditions::default() };
        service.set_hold_settings(hold(OnBattery::Pause));
        service.set_conditions(on_battery);
        assert_eq!(service.state().get().held_back, "on-battery");

        let seen = deltas(&w).await;
        service.sync_anyway().unwrap();
        assert_eq!(service.state().get().held_back, "");
        wait_for_deltas(&w, seen).await;
        service.set_conditions(running::Conditions { power_saver: true, ..on_battery });
        assert_eq!(service.state().get().held_back, "on-battery", "the profile changed: held again");

        service.sync_anyway().unwrap();
        service.set_hold_settings(hold(OnBattery::PowerSaver));
        assert_eq!(service.state().get().held_back, "power-saver", "the setting changed: worked out again");
        service.set_hold_settings(hold(OnBattery::Sync));
        assert_eq!(service.state().get().held_back, "", "sync on battery");

        service.stop_sync().await;
        service.set_link(None);
        drop(service);
        let hub = hub::HelperHub::with_link(Some(link(&w).await));
        hub.set_conditions(on_battery);
        hub.set_hold_settings(hold(OnBattery::Pause));
        let restarted = service_on(&w, &hub);
        restarted.restore().await;
        let seen = deltas(&w).await;
        restarted.resume().await;
        wait_until("held again after a restart", || restarted.state().get().held_back == "on-battery").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(deltas(&w).await, seen, "and asks OneDrive for nothing");
        assert_eq!(restarted.state().get().paused_until, None, "and not paused");
        restarted.stop_sync().await;
    }

    /// the outbox on the bus, `SetIgnorePatterns`: the list is written to `config.toml`, read
    /// back by the next start, and a pattern that cannot match a name is
    /// refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_ignore_list_is_kept_in_config_toml() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        assert!(service.ignore_patterns().contains(&"*.swp".to_owned()), "the defaults");
        service.set_ignore_patterns(vec!["*.bak".into(), "*.bak".into(), "build-*".into()]).await.unwrap();
        assert_eq!(service.ignore_patterns(), vec!["*.bak".to_owned(), "build-*".to_owned()]);
        let persist = persist(&w.config.path().join("config.toml"));
        assert_eq!(persist.store.account(&persist.account).unwrap().ignore, Some(vec!["*.bak".to_owned(), "build-*".to_owned()]));
        assert!(matches!(service.set_ignore_patterns(vec!["a/b".into()]).await, Err(SyncError::InvalidArgs(_))));
        service.stop_sync().await;
        service.set_link(None);
        let restarted = connected(&w, true).await;
        assert_eq!(restarted.ignore_patterns(), vec!["*.bak".to_owned(), "build-*".to_owned()]);
        assert!(!restarted.machine_name().is_empty());
    }

    /// the watcher: a folder turning read-write whose watcher cannot start stays locked,
    /// and says why; nothing is ever made in it unwatched.
    #[tokio::test]
    async fn a_read_write_folder_whose_watcher_cannot_start_stays_locked() {
        use crate::config::Mode;
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        write_mode::FAIL_WATCHER.with(|fail| fail.set(true));
        service.follow_mode(Mode::ReadWrite).await;
        write_mode::FAIL_WATCHER.with(|fail| fail.set(false));
        assert_eq!((mode(w.folder.path()), mode(&w.folder.path().join("docs"))), (0o555, 0o555));
        assert!(service.last_error().contains("stays read-only"), "{}", service.last_error());
        service.stop_sync().await;
    }

    /// the watcher: a read-write folder an earlier run left unlocked, whose sync
    /// cannot start now, is locked again: no watcher looks at it.
    #[tokio::test]
    async fn a_read_write_folder_whose_sync_cannot_start_is_locked_again() {
        use crate::config::Mode;
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;
        service.follow_mode(Mode::ReadWrite).await;
        assert_eq!(mode(w.folder.path()), 0o755);
        service.stop_sync().await;
        service.set_link(None);

        // The next run cannot open its tree store.
        let tree = w.config.path().join("tree.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", tree.display()));
        }
        std::fs::create_dir(&tree).unwrap();
        let restarted = connected(&w, true).await;
        restarted.start_in_mode(Mode::ReadWrite);
        restarted.restore().await;
        restarted.resume().await;
        assert!(restarted.last_error().contains("tree store"), "{}", restarted.last_error());
        assert_eq!((mode(w.folder.path()), mode(&w.folder.path().join("docs"))), (0o555, 0o555));
        restarted.stop_sync().await;
    }

    /// Rewrites `config.toml` as a daemon from before HS2 left a folder
    /// that shows OneDrive registered without interception on purpose —
    /// with a helper connected, so not one to switch.
    fn legacy_without_interception(w: &World) {
        let persist = persist(&w.config.path().join("config.toml"));
        persist
            .store
            .update_account(&persist.account, |account| {
                let root = account.root.as_mut().expect("a folder");
                root.intercepted = false;
                root.upgrade_when_helper = Some(false);
                Ok::<_, crate::config::ConfigError>(())
            })
            .unwrap();
    }

    /// HS2: a folder that shows OneDrive and is not intercepted — as a
    /// daemon from before HS left one registered on purpose, with a helper
    /// connected — is not kept in step while there is no helper: OneDrive
    /// is not asked, `Refresh()` is refused `NoHelper`, and the folder
    /// reads `error` with the helper's advice first in `LastError`. When
    /// the helper connects it switches to interception whatever it was
    /// registered as (switch; there is no "on purpose" for a
    /// OneDrive folder any more). And, Ruling 1: the switch keeps
    /// invariant M1 for everything its sync places afterwards — the sync
    /// starts intercepted, so a folder that arrives from the drive later is
    /// marked before it is filled.
    #[tokio::test]
    async fn a_onedrive_folder_without_interception_waits_for_the_helper_then_switches() {
        let w = world().await;
        {
            let first = connected(&w, true).await;
            first.register_root(w.folder.path()).await.unwrap();
            listed(&first).await;
            first.stop_sync().await;
        }
        legacy_without_interception(&w);

        // From now on the drive holds a new folder, `new/g.txt`.
        w.server.reset().await;
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
            .mount(&w.server).await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [
                    {"id": "N", "name": "new", "folder": {}, "parentReference": {"id": "R"}},
                    {"id": "G", "name": "g.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "N"}}
                ],
                "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L2", w.server.uri())
            })))
            .mount(&w.server).await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L2", w.server.uri())})))
            .mount(&w.server).await;

        // A restart with no helper.
        let service = service(&w, true);
        service.restore().await;
        service.resume().await;
        assert_eq!(service.root_state(), "error");
        assert!(service.last_error().starts_with("the konedrive helper is not connected"), "{}", service.last_error());
        assert!(matches!(service.refresh().await, Err(SyncError::NoHelper)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(deltas(&w).await, 0, "OneDrive was asked with no helper");

        // The helper starts, and connects.
        w.helper.forget();
        service.set_link(Some(link(&w).await));
        service.resume().await;

        wait_until("the new folder was placed", || w.folder.path().join("new/g.txt").exists()).await;
        let seen = w.helper.seen();
        assert_eq!(seen.first(), Some(&Seen::RegisterRoot), "{seen:?}: {}", service.last_error());
        let marks: Vec<_> = seen.iter().filter(|s| matches!(s, Seen::MarkDir { .. })).collect();
        assert!(!marks.is_empty(), "the sync placed a directory after the switch without marking it: {seen:?}");
        assert!(
            marks.iter().all(|s| matches!(s, Seen::MarkDir { entries: 0 })),
            "a directory was filled before it was marked: {seen:?}"
        );
        assert_eq!(service.root_state(), "ready", "{}", service.last_error());
        service.stop_sync().await;
    }

    /// `Skipped()` reads the tree store under the lifecycle lock, so a
    /// Forget — which removes the store with that lock held for writing —
    /// waits for a read under way instead of removing the files under it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skipped_reads_the_tree_under_the_lifecycle_lock() {
        let w = world().await;
        let service = connected(&w, true).await;
        service.register_root(w.folder.path()).await.unwrap();
        listed(&service).await;

        let held = service.lifecycle.write().await;
        let reading = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.skipped().await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!reading.is_finished(), "Skipped() read the tree while the lock was held for writing");
        drop(held);
        assert_eq!(reading.await.unwrap().unwrap(), Vec::<(String, String)>::new());
        service.stop_sync().await;
    }
}

use std::io::{Read, Seek};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use konedrive_fs::placeholder::{
    read_ctag, read_progress, read_stamp, read_state, write_progress, Progress, State,
};
use konedrive_proto::ACCEPTED_DENY_ERRNOS;
use tokio::io::ReadBuf;

use konedrive_graph::quickxor::QuickXor;

use super::super::{Fetched, LocalDir};
use super::*;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncRead;

fn placeholder(dir: &std::path::Path, item_id: &str, size: u64) -> std::fs::File {
    let handle = std::fs::File::open(dir).unwrap();
    konedrive_fs::placeholder::create_placeholder(
        &handle,
        "file.bin",
        item_id,
        size,
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
    )
    .unwrap();
    std::fs::File::options()
        .read(true)
        .write(true)
        .open(dir.join("file.bin"))
        .unwrap()
}

/// Every errno this module produces travels to the kernel in a
/// `FAN_DENY | (errno << 24)` response word, which the kernel accepts for
/// exactly eight values; anything else makes that `write()` fail with
/// `EINVAL` and leaves the suspended `open()` hanging forever. So every
/// test that gets an errno out of `hydrate` puts it through here first —
/// the property is not "this call returns EIO", it is "no call can ever
/// return something undeliverable".
fn deliverable(errno: i32) -> i32 {
    assert!(
        ACCEPTED_DENY_ERRNOS.contains(&errno),
        "errno {errno} is not one the kernel accepts in a FAN_DENY response; \
         the helper's write() would fail with EINVAL and the opener would hang forever"
    );
    errno
}

async fn hydrate_file(file: &std::fs::File, source: &dyn ContentSource) -> i32 {
    deliverable(hydrate(file.as_fd().try_clone_to_owned().unwrap(), source).await)
}

/// A source that never produces anything, to pin the retry-then-give-up
/// path for `Transient` — which, unlike a short stream, nothing in the
/// repo exercised.
struct AlwaysTransient {
    attempts: AtomicU64,
}

#[async_trait]
impl ContentSource for AlwaysTransient {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(SourceError::Transient("the connection dropped".into()))
    }
}

/// The HTTP `200`-instead-of-`206` shape, told honestly: the second fetch
/// serves the file from the beginning again and says so.
struct RestartsFromZero {
    path: PathBuf,
    break_at: u64,
    fetches: AtomicU64,
}

#[async_trait]
impl ContentSource for RestartsFromZero {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let n = self.fetches.fetch_add(1, Ordering::SeqCst);
        let meta = std::fs::metadata(&self.path).unwrap();
        let file = tokio::fs::File::open(&self.path).await.unwrap();
        let stream: Box<dyn AsyncRead + Send + Unpin> = if n == 0 {
            Box::new(file.take(self.break_at))
        } else {
            Box::new(file)
        };
        Ok(Fetched {
            served_from: 0,
            size: meta.len(),
            mtime: meta.modified().unwrap(),
            version: None,
            stream,
        })
    }
}

/// Serves a real file but declares an mtime one second before 1970.
struct PreEpochMtime {
    inner: LocalDir,
}

#[async_trait]
impl ContentSource for PreEpochMtime {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let mut fetched = self.inner.fetch(item_id, from, end).await?;
        fetched.mtime = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        Ok(fetched)
    }
}

/// Reads the file's state from the filesystem at the moment the bytes are
/// asked for, which is the only window in which `hydrating` exists.
struct WatchesState {
    inner: LocalDir,
    path: PathBuf,
    seen: Mutex<Option<State>>,
}

#[async_trait]
impl ContentSource for WatchesState {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let opened = std::fs::File::open(&self.path).unwrap();
        *self.seen.lock().unwrap() = read_state(&opened).unwrap();
        self.inner.fetch(item_id, from, end).await
    }
}

#[tokio::test]
async fn fills_the_placeholder_in_place_and_marks_it_hydrated() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM1"), b"0123456789").unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM1", 10);

    let source = LocalDir::new(remote.path());
    assert_eq!(hydrate_file(&file, &source).await, 0);

    let mut content = String::new();
    let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    opened.read_to_string(&mut content).unwrap();
    assert_eq!(content, "0123456789");
    assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
    assert!(konedrive_fs::placeholder::stamp_matches(&opened).unwrap());
}

/// I3: the placeholder is 4 KiB and the download grows it to 512 KiB
/// before breaking, so both halves of the rollback are *visible*: the
/// size has to come back down and the blocks have to go away. The
/// original version of this test used a 4096-byte remote against a
/// 4096-byte placeholder and asserted `blocks() < 64` — a bound no
/// 4 KiB file can reach — so deleting either `punch_all` or the
/// `set_len` left it green.
#[tokio::test]
async fn a_failed_download_leaves_an_empty_placeholder_and_an_errno() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM2"), vec![7u8; 1024 * 1024]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM2", 4096);

    let source = LocalDir::new(remote.path()).fail_at(512 * 1024);
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

    let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
    let meta = opened.metadata().unwrap();
    assert_eq!(meta.len(), 4096, "the placeholder's own size must come back");
    // Measured: 0 with the punch, 8 without it — the restored size alone
    // frees everything past the first 4 KiB, so any bound looser than
    // "no data at all" lets a missing `punch_all` through. On a realistic
    // placeholder those 8 blocks are however many the download managed.
    assert_eq!(
        meta.blocks(),
        0,
        "the 512 KiB that did arrive must be punched away, not merely truncated away"
    );
    assert_eq!(read_stamp(&opened).unwrap(), None, "a failed fill leaves no stamp");
}

#[tokio::test]
async fn a_file_that_grew_remotely_is_resized() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM3"), b"much longer than before").unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM3", 4);

    let source = LocalDir::new(remote.path());
    assert_eq!(hydrate_file(&file, &source).await, 0);

    let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    assert_eq!(opened.metadata().unwrap().len(), 23);
}

/// C2: the growth direction resizes itself — `write_all_at` past the end
/// extends the file whether or not anything calls `set_len`. Shrinking is
/// the direction that needs the truncation, and it is the direction that
/// loses data without it: a 1,000,000-byte placeholder marked `hydrated`
/// while holding 4 real bytes and 999,996 zeros.
#[tokio::test]
async fn a_file_that_shrank_remotely_is_truncated() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM4"), b"tiny").unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM4", 1_000_000);

    let source = LocalDir::new(remote.path());
    assert_eq!(hydrate_file(&file, &source).await, 0);

    let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    assert_eq!(opened.metadata().unwrap().len(), 4, "the remote size wins");
    let mut content = Vec::new();
    opened.read_to_end(&mut content).unwrap();
    assert_eq!(content, b"tiny");
    assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
    assert!(konedrive_fs::placeholder::stamp_matches(&opened).unwrap());
}

/// I4: the `NotFound` arm — a deleted remote item — was mapped to `EIO`
/// with nothing exercising it. `ENOENT`, the errno it
/// obviously "should" be, is outside the kernel's accepted set and would
/// hang the opener forever.
#[tokio::test]
async fn a_missing_remote_item_is_refused_with_an_errno_the_kernel_accepts() {
    let remote = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "GONE", 4096);

    let source = LocalDir::new(remote.path());
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

    let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
    assert_eq!(opened.metadata().unwrap().len(), 4096);
}

/// I4: the `Transient` arm, the other one nothing in the repo reached.
/// (The two backoffs really do sleep — 200 ms then 400 ms — rather than
/// pulling `tokio`'s `test-util` feature into the whole workspace's
/// dependency graph to fake them.)
#[tokio::test]
async fn a_source_that_keeps_failing_is_retried_three_times_then_refused() {
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM5", 4096);

    let source = AlwaysTransient { attempts: AtomicU64::new(0) };
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
    assert_eq!(source.attempts.load(Ordering::SeqCst), 3, "three attempts, then give up");

    let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
}

/// I4: a descriptor that is not a placeholder at all. The helper only
/// ever sends managed files, but "the item id is unreadable" is a real
/// disk-error path and it must not answer with something undeliverable.
#[tokio::test]
async fn a_file_with_no_item_id_is_refused_with_an_errno_the_kernel_accepts() {
    let remote = tempfile::tempdir().unwrap();
    let file = tempfile::tempfile().unwrap();

    let source = LocalDir::new(remote.path());
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
}

/// I8: a full disk must reach the application as `ENOSPC`, which §5.2
/// step 5 and §9 both ask for by name and which the kernel does accept —
/// flattening every local failure to `EIO` throws away the one thing the
/// user can act on. Everything outside the accepted set still has to
/// become `EIO`, because the alternative is an opener that never wakes.
#[test]
fn local_write_failures_keep_the_errnos_the_kernel_accepts() {
    assert_eq!(errno_of(&io::Error::from_raw_os_error(libc::ENOSPC)), libc::ENOSPC);
    assert_eq!(errno_of(&io::Error::from_raw_os_error(libc::EDQUOT)), libc::EDQUOT);
    for outside in [libc::EROFS, libc::EFBIG, libc::EBADF, libc::ENOENT] {
        assert_eq!(errno_of(&io::Error::from_raw_os_error(outside)), libc::EIO);
    }
    assert_eq!(errno_of(&io::Error::other("no errno at all")), libc::EIO);
    for errno in [libc::ENOSPC, libc::EDQUOT, libc::EROFS, libc::EFBIG] {
        deliverable(errno_of(&io::Error::from_raw_os_error(errno)));
    }
}

/// I7: §5.3 step 1. The marker is what §4.4 startup recovery finds a
/// half-filled file by after a power loss; without it the blocks stay
/// allocated forever. It exists only while the bytes are in flight, so
/// the source is where it can be observed.
#[tokio::test]
async fn the_file_is_marked_hydrating_while_the_bytes_are_in_flight() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM6"), vec![3u8; 8192]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM6", 8192);

    let source = WatchesState {
        inner: LocalDir::new(remote.path()),
        path: local.path().join("file.bin"),
        seen: Mutex::new(None),
    };
    assert_eq!(hydrate_file(&file, &source).await, 0);

    assert_eq!(
        *source.seen.lock().unwrap(),
        Some(State::Hydrating),
        "the file must be marked hydrating before the first byte is asked for"
    );
}

/// I5, the positive half: a file completed across two
/// fetches. `fail_at` is permanent, so before this nothing followed the
/// resume arithmetic end to end — and the resume offset is exactly where
/// bytes land in the wrong place.
#[tokio::test]
async fn a_download_that_resumes_completes_the_file_byte_for_byte() {
    let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM7"), &payload).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM7", 3000);

    let source = LocalDir::new(remote.path()).fail_once_at(1000);
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.fetches(), 2, "the first fetch must have been resumed, not restarted");

    let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    let mut content = Vec::new();
    opened.read_to_end(&mut content).unwrap();
    assert_eq!(content, payload, "every byte must be where the source put it");
    assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
}

/// I5, the negative half: the source answers the resume with
/// the whole file again — an HTTP server replying `200` to a `Range`
/// request — and says so. Writing that at the resume offset produces a
/// file whose middle is its beginning, reported as a success.
#[tokio::test]
async fn a_source_that_restarts_the_stream_is_refused_instead_of_written_at_the_wrong_offset() {
    let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM8"), &payload).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM8", 3000);

    let source = RestartsFromZero {
        path: remote.path().join("ITEM8"),
        break_at: 1000,
        fetches: AtomicU64::new(0),
    };
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

    let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    assert_eq!(
        read_state(&opened).unwrap(),
        Some(State::OnlineOnly),
        "a file filled from a stream at the wrong offset must never be marked hydrated"
    );
    assert_eq!(opened.metadata().unwrap().len(), 3000);
    // Not `blocks() == 0` here: a 3000-byte file's punch range ends
    // mid-page, and no filesystem can release a page it has only
    // partially punched — measured, 8 blocks survive on tmpfs. What must
    // hold either way is that none of the bytes that did arrive are
    // still readable.
    let mut content = Vec::new();
    std::fs::File::open(local.path().join("file.bin"))
        .unwrap()
        .read_to_end(&mut content)
        .unwrap();
    assert!(
        content.iter().all(|b| *b == 0),
        "the partial content must have been punched away, not left in place"
    );
}

/// I6: `written(0) >= size(0)` ends the fill loop on the
/// first answer, so a declared size of 0 truncates a live placeholder
/// with no retry and no corroboration whatsoever. Fail closed instead.
#[tokio::test]
async fn a_declared_size_of_zero_does_not_truncate_a_live_placeholder() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM9"), b"").unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM9", 1_000_000);

    let source = LocalDir::new(remote.path());
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

    let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    assert_eq!(opened.metadata().unwrap().len(), 1_000_000, "the placeholder survives");
    assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
}

/// A source's time before 1970 is one a file can carry: the fill applies it,
/// and the stamp records it, so the file can be dehydrated again
/// (`stamp_matches`).
///
/// This test used to show that an mtime `set_mtime` refused did not cost the
/// user a file whose content was correct. `set_mtime` takes such a time now;
/// `commit` still goes on when `futimens` fails, which no test on the host
/// can make it do.
#[tokio::test]
async fn a_source_time_before_1970_is_applied_and_stamped() {
    let remote = tempfile::tempdir().unwrap();
    let payload = vec![5u8; 200_000];
    std::fs::write(remote.path().join("ITEM11"), &payload).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM11", 4096);

    let source = PreEpochMtime { inner: LocalDir::new(remote.path()) };
    assert_eq!(hydrate_file(&file, &source).await, 0);

    let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
    let mut content = Vec::new();
    opened.read_to_end(&mut content).unwrap();
    assert_eq!(content, payload, "every byte of the file is there");
    assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
    assert!(konedrive_fs::placeholder::stamp_matches(&opened).unwrap(), "the stamp records the time the file has");
    let mtime = opened.metadata().unwrap().modified().unwrap();
    assert_eq!(mtime, SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1));
}

/// Verified correct by the review and pinned here so it stays that way:
/// the fill is positioned (`pwrite`), so the file offset the suspended
/// `open()` is about to inherit is exactly where the application left it.
/// A plain `write` would move it by the size of the whole download.
#[tokio::test]
async fn the_shared_file_offset_is_untouched_by_a_fill() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM10"), vec![9u8; 100_000]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let mut file = placeholder(local.path(), "ITEM10", 100_000);
    file.seek(io::SeekFrom::Start(1234)).unwrap();

    // The duplicate shares one open file description with `file`, exactly
    // as the helper's `SCM_RIGHTS` copy shares one with the opener's.
    let source = LocalDir::new(remote.path());
    assert_eq!(hydrate_file(&file, &source).await, 0);

    assert_eq!(file.stream_position().unwrap(), 1234);
}

/// C1, pinned back onto the host suite. A test with an mtime `set_mtime`
/// refused used to be this one, before `set_mtime`'s error was made non-fatal
/// and took away the only post-data failure an unprivileged test could reach.
///
/// The injected fault fires unconditionally, right before the commit
/// write, and — from *inside* the fault itself — re-opens the placeholder
/// and asserts it is not yet observably `hydrated`. That is what actually
/// pins the ordering: a fault that merely makes some fallible call return
/// an error cannot distinguish "commit point last" from "commit point
/// early, but this particular later step happened to fail too", because
/// `hydrate`'s rollback demotes the file either way once `fill` returns
/// `Err`. Observing the state *at the moment of the fault*, before
/// `fill` has had any chance to return, is what a moved-up commit point
/// cannot survive.
#[tokio::test]
async fn a_post_data_failure_never_leaves_the_file_observably_hydrated() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM12"), vec![4u8; 65536]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM12", 65536);
    let path = local.path().join("file.bin");

    let hook_path = path.clone();
    set_post_data_fault(move || {
        let opened = std::fs::File::open(&hook_path).unwrap();
        assert_ne!(
            read_state(&opened).unwrap(),
            Some(State::Hydrated),
            "state=hydrated must be the LAST fallible step in fill's tail; this fault \
             fires before the commit write and must never observe it already landed"
        );
        Some(libc::EIO)
    });

    let source = LocalDir::new(remote.path());
    let errno = hydrate_file(&file, &source).await;
    clear_post_data_fault();

    assert_eq!(errno, libc::EIO);
    let opened = std::fs::File::open(&path).unwrap();
    assert_eq!(
        read_state(&opened).unwrap(),
        Some(State::OnlineOnly),
        "a post-data failure must leave the file demoted, never hydrated"
    );
    assert_eq!(opened.metadata().unwrap().len(), 65536, "the placeholder's size must come back");
    assert_eq!(
        opened.metadata().unwrap().blocks(),
        0,
        "the data that landed before the fault must be punched away"
    );
    assert_eq!(read_stamp(&opened).unwrap(), None, "a failed fill leaves no stamp");
}

/// A roll-back never punches a file that is no longer in the
/// state its fill put it in. Under the per-inode lock only something
/// outside the daemon can have changed it — and whatever did, the file is
/// not this fill's to empty any more: a file that reads `hydrated` may be
/// carrying an ignore mark, and punching it is the zeros case.
#[test]
fn a_roll_back_leaves_alone_a_file_that_is_no_longer_hydrating() {
    let dir = tempfile::tempdir().unwrap();
    let file = placeholder(dir.path(), "ITEM", 4096);
    std::fs::write(dir.path().join("file.bin"), vec![6u8; 4096]).unwrap();
    write_state(&file, State::Hydrated).unwrap();

    roll_back(&file, 4096, None);

    let mut content = Vec::new();
    std::fs::File::open(dir.path().join("file.bin")).unwrap().read_to_end(&mut content).unwrap();
    assert!(content == vec![6u8; 4096], "the file's content must be left alone");
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated), "and so must its state");
}

/// A byte stream that fails with a connection error at `fail_at` (an
/// offset into `data`), the way a dropped HTTP connection does. It hands
/// out at most 16 KiB per read, as a socket does, so that a fill's
/// checkpoints land where they would in a real download rather than
/// being skipped over by one read the size of the whole buffer.
struct Breaking {
    data: Vec<u8>,
    at: usize,
    fail_at: Option<usize>,
}

impl AsyncRead for Breaking {
    fn poll_read(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.fail_at == Some(self.at) {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "connection reset by peer")));
        }
        let limit = self.fail_at.unwrap_or(self.data.len()).min(self.data.len());
        let end = limit.min(self.at + buf.remaining()).min(self.at + 16 * 1024);
        let chunk = self.data[self.at..end].to_vec();
        buf.put_slice(&chunk);
        self.at = end;
        Poll::Ready(Ok(()))
    }
}

/// Serves a file from memory as a Graph source would — a cTag and a
/// quickXorHash with every answer — with the faults a download has to
/// survive. `second` is served from fetch number `switch_at` on (a new
/// version uploaded mid-download); `breaks` maps a fetch number to the
/// absolute offset its stream breaks at; `wrong_hash` makes every answer
/// carry a hash nothing matches, and `no_hash` makes every answer carry
/// none at all. Records where each fetch started.
struct Scripted {
    first: (String, Vec<u8>),
    second: Option<(String, Vec<u8>)>,
    switch_at: usize,
    breaks: std::collections::HashMap<usize, u64>,
    wrong_hash: bool,
    no_hash: bool,
    froms: Mutex<Vec<u64>>,
    /// The file being filled, when a test wants to know what checkpoint
    /// it carried at the moment each fetch was made.
    watching: Option<std::fs::File>,
    progress_seen: Mutex<Vec<Option<Progress>>>,
}

impl Scripted {
    fn new(ctag: &str, data: Vec<u8>) -> Self {
        Self { first: (ctag.into(), data), second: None, switch_at: usize::MAX, breaks: Default::default(), wrong_hash: false, no_hash: false, froms: Mutex::new(Vec::new()), watching: None, progress_seen: Mutex::new(Vec::new()) }
    }

    fn froms(&self) -> Vec<u64> {
        self.froms.lock().unwrap().clone()
    }

    fn progress_seen(&self) -> Vec<Option<Progress>> {
        self.progress_seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl ContentSource for Scripted {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let n = {
            let mut froms = self.froms.lock().unwrap();
            froms.push(from);
            froms.len() - 1
        };
        if let Some(watched) = &self.watching {
            self.progress_seen.lock().unwrap().push(read_progress(watched).unwrap());
        }
        let (ctag, data) = match &self.second {
            Some(second) if n >= self.switch_at => second,
            _ => &self.first,
        };
        let hash = if self.wrong_hash {
            [0x55u8; 20]
        } else {
            let mut h = QuickXor::new();
            h.update(data);
            h.finish()
        };
        let start = (from as usize).min(data.len());
        let fail_at = self.breaks.get(&n).map(|&at| (at as usize).saturating_sub(start));
        Ok(Fetched {
            served_from: from,
            size: data.len() as u64,
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
            version: Some(Version { ctag: ctag.clone(), quick_xor: (!self.no_hash).then_some(hash) }),
            stream: Box::new(Breaking { data: data[start..].to_vec(), at: 0, fail_at }),
        })
    }
}

fn content(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn read_back(file: &std::fs::File) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut out = vec![0u8; file.metadata().unwrap().len() as usize];
    file.read_exact_at(&mut out, 0).unwrap();
    out
}

#[tokio::test]
async fn a_verified_fill_records_its_ctag_and_leaves_no_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(300_000, 1);
    let file = placeholder(dir.path(), "I", data.len() as u64);
    let source = Scripted::new("c1", data.clone());
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(read_back(&file), data);
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
    assert_eq!(read_progress(&file).unwrap(), None);
}

#[tokio::test]
async fn content_that_does_not_match_its_hash_is_fetched_once_more_then_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(300_000, 2);
    let file = placeholder(dir.path(), "I", data.len() as u64);
    let source = Scripted { wrong_hash: true, ..Scripted::new("c1", data) };
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
    assert_eq!(source.froms(), vec![0, 0], "one more try from the start, and no third");
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert!(read_stamp(&file).unwrap().is_none());
    assert_eq!(read_progress(&file).unwrap(), None);
}

/// The same, with checkpoints small enough that the second download
/// makes some: content that has failed its hash twice is no prefix to
/// continue from, so the refusal drops the checkpoint and the roll-back
/// punches everything — the next open downloads afresh instead of from a
/// prefix already known to belong to content that does not verify.
#[tokio::test]
async fn a_second_mismatch_drops_the_checkpoint_it_made() {
    set_checkpoint_every(64 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let data = content(1 << 20, 14);
    let file = placeholder(dir.path(), "I", data.len() as u64);
    let source = Scripted { wrong_hash: true, ..Scripted::new("c1", data) };
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
    clear_checkpoint_every();

    assert_eq!(source.froms(), vec![0, 0]);
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_progress(&file).unwrap(), None, "no checkpoint survives a second mismatch");
    assert!(read_back(&file).iter().all(|b| *b == 0), "and none of the content that failed it");
}

/// case: with no quickXorHash nothing can check a resumed
/// prefix, so a checkpoint is not continued from — here one whose first
/// 8 KiB were lost is downloaded again from the start, not committed as
/// correct with 8 KiB of zeros in it.
#[tokio::test]
async fn a_checkpoint_is_not_trusted_without_a_hash() {
    use std::os::unix::fs::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let data = content(1 << 20, 15);
    let file = checkpointed(dir.path(), &data, "c1", 192 * 1024);
    file.write_all_at(&[0u8; 8192], 0).unwrap();
    let source = Scripted { no_hash: true, ..Scripted::new("c1", data.clone()) };
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![192 * 1024, 0], "the checkpoint is dropped, not continued");
    assert_eq!(read_back(&file), data);
    assert_eq!(read_progress(&file).unwrap(), None);
}

/// And a download without a hash makes no checkpoint to begin with: when
/// it gives up, nothing of it is kept, and everything is punched as in
/// part 1.
#[tokio::test]
async fn a_download_without_a_hash_writes_no_checkpoint() {
    set_checkpoint_every(64 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let data = content(1 << 20, 16);
    let file = placeholder(dir.path(), "I", data.len() as u64);
    let mut source = Scripted { no_hash: true, ..Scripted::new("c1", data) };
    source.watching = Some(file.try_clone().unwrap());
    for n in 0..3 {
        source.breaks.insert(n, 200 * 1024);
    }
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
    clear_checkpoint_every();

    assert_eq!(source.froms(), vec![0, 200 * 1024, 200 * 1024]);
    assert_eq!(source.progress_seen(), vec![None, None, None], "no checkpoint while it downloaded");
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_progress(&file).unwrap(), None);
    assert!(read_back(&file).iter().all(|b| *b == 0), "everything that arrived is punched");
}

#[tokio::test]
async fn a_dropped_connection_resumes_where_it_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(300_000, 3);
    let file = placeholder(dir.path(), "I", data.len() as u64);
    let mut source = Scripted::new("c1", data.clone());
    source.breaks.insert(0, 100_000);
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![0, 100_000]);
    assert_eq!(read_back(&file), data);
}

#[tokio::test]
async fn a_file_changed_in_the_cloud_mid_download_starts_over_with_the_new_version() {
    let dir = tempfile::tempdir().unwrap();
    let old = content(300_000, 4);
    let new = content(250_000, 5);
    let file = placeholder(dir.path(), "I", old.len() as u64);
    let mut source = Scripted::new("c1", old);
    source.second = Some(("c2".into(), new.clone()));
    source.switch_at = 1;
    source.breaks.insert(0, 100_000);
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![0, 100_000, 0]);
    assert_eq!(read_back(&file), new);
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
}

/// The cTag is the only thing that notices a new version when OneDrive
/// gives no quickXorHash: the old version's first bytes and
/// the new one's tail would otherwise be committed as one file.
#[tokio::test]
async fn a_file_changed_mid_download_starts_over_even_without_a_hash() {
    let dir = tempfile::tempdir().unwrap();
    let old = content(300_000, 12);
    let new = content(250_000, 13);
    let file = placeholder(dir.path(), "I", old.len() as u64);
    let mut source = Scripted { no_hash: true, ..Scripted::new("c1", old) };
    source.second = Some(("c2".into(), new.clone()));
    source.switch_at = 1;
    source.breaks.insert(0, 100_000);
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![0, 100_000, 0]);
    assert_eq!(read_back(&file), new, "no byte of the old version may survive into the new one");
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
}

/// A download that gives up keeps what it made durable.
#[tokio::test]
async fn a_download_that_gives_up_keeps_its_checkpoint() {
    set_checkpoint_every(64 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let data = content(1 << 20, 6);
    let file = placeholder(dir.path(), "I", data.len() as u64);
    let mut source = Scripted::new("c1", data.clone());
    for n in 0..3 {
        source.breaks.insert(n, 200 * 1024);
    }
    assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
    clear_checkpoint_every();

    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert!(read_stamp(&file).unwrap().is_none());
    assert_eq!(read_progress(&file).unwrap(), Some(Progress { ctag: "c1".into(), bytes: 192 * 1024 }));
    let back = read_back(&file);
    assert_eq!(&back[..192 * 1024], &data[..192 * 1024], "the checkpointed prefix is kept");
    assert!(back[192 * 1024..].iter().all(|b| *b == 0), "everything past it is punched");
    assert!(file.metadata().unwrap().blocks() * 512 < 400 * 1024, "the tail's blocks are freed");
}

#[tokio::test]
async fn a_checkpoint_is_resumed_and_the_whole_file_verified() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(1 << 20, 7);
    let file = checkpointed(dir.path(), &data, "c1", 192 * 1024);
    let source = Scripted::new("c1", data.clone());
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![192 * 1024], "it continued from the checkpoint");
    assert_eq!(read_back(&file), data);
    assert_eq!(read_progress(&file).unwrap(), None);
}

/// The resume is safe because the hash covers what lay on disk too.
#[tokio::test]
async fn a_damaged_checkpoint_is_caught_by_the_hash_and_downloaded_again() {
    use std::os::unix::fs::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let data = content(1 << 20, 8);
    let file = checkpointed(dir.path(), &data, "c1", 192 * 1024);
    file.write_all_at(&[data[10] ^ 0xff], 10).unwrap();
    let source = Scripted::new("c1", data.clone());
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![192 * 1024, 0]);
    assert_eq!(read_back(&file), data);
}

#[tokio::test]
async fn a_checkpoint_for_another_version_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(1 << 20, 9);
    let file = checkpointed(dir.path(), &data, "c0", 192 * 1024);
    let source = Scripted::new("c1", data.clone());
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![192 * 1024, 0]);
    assert_eq!(read_back(&file), data);
}

#[tokio::test]
async fn a_checkpoint_past_the_end_of_the_file_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(100_000, 10);
    let file = checkpointed(dir.path(), &data, "c1", 100_000);
    write_progress(&file, &Progress { ctag: "c1".into(), bytes: 200_000 }).unwrap();
    let mut source = Scripted::new("c1", data.clone());
    source.watching = Some(file.try_clone().unwrap());
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![0]);
    // Gone before the first byte is asked for — not merely by the commit
    // at the end: new bytes must not go under a count that recovery
    // could adopt after a crash.
    assert_eq!(source.progress_seen(), vec![None], "the unusable checkpoint was still there");
    assert_eq!(read_progress(&file).unwrap(), None);
}

#[tokio::test]
async fn a_checkpoint_at_the_very_end_needs_no_more_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(100_000, 11);
    let file = checkpointed(dir.path(), &data, "c1", 100_000);
    let source = Scripted::new("c1", data.clone());
    assert_eq!(hydrate_file(&file, &source).await, 0);
    assert_eq!(source.froms(), vec![100_000]);
    assert_eq!(read_back(&file), data);
}

/// A placeholder as a download that gave up at `bytes` leaves it.
fn checkpointed(dir: &std::path::Path, data: &[u8], ctag: &str, bytes: u64) -> std::fs::File {
    use std::os::unix::fs::FileExt;
    let file = placeholder(dir, "I", data.len() as u64);
    file.write_all_at(&data[..bytes as usize], 0).unwrap();
    write_progress(&file, &Progress { ctag: ctag.into(), bytes }).unwrap();
    file
}

/// `fill_file` alone decides whether a file needs its ignore mark cleared: a
/// file it finds `dehydrating` (or `hydrating`) may carry one, and a fill that
/// fails empties the file. Handed no clearance for such a file, it has no way
/// to clear the mark, so it must not fetch and must not empty.
///
/// No caller in the daemon does this today: each reads the state itself,
/// under the per-inode lock, and passes no clearance only for `online-only`.
#[tokio::test]
async fn a_file_found_dehydrating_is_not_emptied_by_a_fill_given_no_clearance() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![7u8; 1024 * 1024]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM", 4096);
    let path = local.path().join("file.bin");
    std::fs::write(&path, vec![3u8; 4096]).unwrap();
    write_state(&file, State::Dehydrating).unwrap();

    // The download breaks, so the fill rolls back.
    let source = LocalDir::new(remote.path()).fail_at(512 * 1024);
    let filled = hydrate_with(file.as_fd().try_clone_to_owned().unwrap(), &source, None).await;

    assert!(filled.is_err());
    assert!(
        std::fs::read(&path).unwrap() == vec![3u8; 4096],
        "a file that may carry an ignore mark was emptied without the mark being cleared"
    );
}

/// A fill that is refused because the way could not be cleared puts the file
/// back as it found it. A state it could not read is not "no state": taking
/// the attribute off leaves a file with an item id and no state at all.
///
/// No caller in the daemon gets here today: each refuses a file whose state
/// it cannot read before it asks for a fill.
#[tokio::test]
async fn a_refused_fill_does_not_take_off_a_state_it_could_not_read() {
    let remote = tempfile::tempdir().unwrap();
    std::fs::write(remote.path().join("ITEM"), vec![7u8; 4096]).unwrap();
    let local = tempfile::tempdir().unwrap();
    let file = placeholder(local.path(), "ITEM", 4096);
    xattr::FileExt::set_xattr(&file, XATTR_STATE, b"hydrat").unwrap();
    // Whether a helper runs cannot be told (the socket's path leads through a
    // file), so the way is not cleared.
    let clearance = Clearance::NoLink(local.path().join("file.bin").join("helper.sock"));

    let source = LocalDir::new(remote.path());
    let filled = hydrate_with(file.as_fd().try_clone_to_owned().unwrap(), &source, Some(&clearance)).await;

    assert!(matches!(filled, Err(FillError::NotCleared(_))), "{filled:?}");
    assert_eq!(source.fetches(), 0, "nothing is fetched");
    assert!(
        xattr::FileExt::get_xattr(&file, XATTR_STATE).unwrap().is_some(),
        "the refused fill took the state attribute off the file"
    );
}

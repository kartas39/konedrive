use std::future::Future;
use std::io;
use std::os::fd::AsFd;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};
use std::time::SystemTime;

use async_trait::async_trait;
use konedrive_fs::placeholder::{read_ctag, read_progress, read_state, State};
use tokio::io::{AsyncRead, ReadBuf};

use super::super::{clear_checkpoint_every, hydrate_in_parts, set_checkpoint_every};
use super::*;

const KIB: u64 = 1024;

/// Serves one file from memory as Graph would — a cTag and a quickXorHash with every
/// answer, only the range asked for — slowly enough that streams overlap: 16 KiB per read,
/// `delay` after each. `second` is served from fetch number `switch_at` on (a new version
/// uploaded mid-download); a fetch starting in `breaks.0..breaks.1` breaks after `breaks.2`
/// bytes. Records every range asked for, and how many answers are being read at once.
struct Ranged {
    first: (String, Vec<u8>),
    second: Option<(String, Vec<u8>)>,
    switch_at: usize,
    breaks: Option<(u64, u64, usize)>,
    delay: Duration,
    asked: Mutex<Vec<(u64, Option<u64>)>>,
    hashes: Mutex<std::collections::HashMap<String, [u8; crate::quickxor::LEN]>>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Ranged {
    fn new(ctag: &str, data: Vec<u8>) -> Self {
        Self {
            first: (ctag.into(), data),
            second: None,
            switch_at: usize::MAX,
            breaks: None,
            delay: Duration::from_millis(1),
            asked: Mutex::default(),
            hashes: Mutex::default(),
            in_flight: Arc::default(),
            peak: Arc::default(),
        }
    }

    fn asked(&self) -> Vec<(u64, Option<u64>)> {
        self.asked.lock().unwrap().clone()
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ContentSource for Ranged {
    async fn fetch(&self, _item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let n = {
            let mut asked = self.asked.lock().unwrap();
            asked.push((from, end));
            asked.len() - 1
        };
        let (ctag, data) = match &self.second {
            Some(second) if n >= self.switch_at => second,
            _ => &self.first,
        };
        let hash = *self.hashes.lock().unwrap().entry(ctag.clone()).or_insert_with(|| {
            let mut hash = QuickXor::new();
            hash.update(data);
            hash.finish()
        });
        let len = data.len() as u64;
        let (from_at, to) = (from.min(len) as usize, end.unwrap_or(len).min(len) as usize);
        let fail_at = self.breaks.and_then(|(lo, hi, after)| (lo..hi).contains(&from).then_some(after));
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        Ok(Fetched {
            served_from: from,
            size: len,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
            version: Some(Version { ctag: ctag.clone(), quick_xor: Some(hash) }),
            stream: Box::new(Slow {
                data: data[from_at..to.max(from_at)].to_vec(),
                at: 0,
                fail_at,
                delay: self.delay,
                sleep: None,
                in_flight: Arc::clone(&self.in_flight),
            }),
        })
    }
}

struct Slow {
    data: Vec<u8>,
    at: usize,
    fail_at: Option<usize>,
    delay: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    in_flight: Arc<AtomicUsize>,
}

impl AsyncRead for Slow {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Some(sleep) = self.sleep.as_mut() {
            std::task::ready!(sleep.as_mut().poll(cx));
            self.sleep = None;
        }
        if self.fail_at == Some(self.at) {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "connection reset by peer")));
        }
        let limit = self.fail_at.unwrap_or(usize::MAX).min(self.data.len());
        let end = limit.min(self.at + buf.remaining()).min(self.at + 16 * 1024);
        let chunk = self.data[self.at..end].to_vec();
        buf.put_slice(&chunk);
        self.at = end;
        self.sleep = Some(Box::pin(tokio::time::sleep(self.delay)));
        Poll::Ready(Ok(()))
    }
}

impl Drop for Slow {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn content(size: u64, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed).wrapping_add((i >> 12) as u8)).collect()
}

fn placeholder(dir: &std::path::Path, name: &str, size: u64) -> std::fs::File {
    let handle = std::fs::File::open(dir).unwrap();
    konedrive_fs::placeholder::create_placeholder(&handle, name, "I", size, SystemTime::UNIX_EPOCH).unwrap();
    std::fs::File::options().read(true).write(true).open(dir.join(name)).unwrap()
}

fn read_back(file: &std::fs::File) -> Vec<u8> {
    let mut out = vec![0u8; file.metadata().unwrap().len() as usize];
    file.read_exact_at(&mut out, 0).unwrap();
    out
}

async fn fill(file: &std::fs::File, source: &dyn ContentSource, split: &Split) -> i32 {
    let fd = file.as_fd().try_clone_to_owned().unwrap();
    hydrate_in_parts(fd, source, None, split).await.err().map_or(0, |e| e.errno())
}

/// A pool of four slots, all of which may be large, and a first stream's slot taken from
/// it as the pins' worker takes one.
fn pool_of_four() -> (Arc<TransferPool>, Slot) {
    let pool = TransferPool::starting_at(4, 4);
    let first = pool.try_acquire_sized(Class::Download, Size::Large).unwrap();
    (pool, first)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..5000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("{what} never happened");
}

/// One large file alone: four streams at once, each asking for a bounded range, and the
/// file complete, verified and committed; the extra slots go back.
#[tokio::test]
async fn one_large_file_downloads_in_four_streams() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(2 * 1024 * KIB, 1);
    let file = placeholder(dir.path(), "f.bin", data.len() as u64);
    let source = Ranged::new("c1", data.clone());
    let (pool, _first) = pool_of_four();
    let split = Split::with_piece(Arc::clone(&pool), Share::new(), 64 * KIB);

    assert_eq!(fill(&file, &source, &split).await, 0);

    assert_eq!(source.peak(), 4, "four range requests at once");
    assert!(source.asked().iter().all(|(from, end)| *end == Some(from + 64 * KIB)), "every piece is a bounded range");
    assert_eq!(read_back(&file), data);
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
    assert_eq!(read_progress(&file).unwrap(), None);
    assert_eq!(pool.large_held(), 1, "only the first stream's slot is still held");
}

/// A download in parts is one file (issue #50): one entry of `Transfers.Downloads`, so it
/// counts once in `ActiveDownloads` and in `LargeFiles`, and its streams in `LargeStreams`.
#[tokio::test]
async fn a_download_in_parts_is_one_file_and_its_streams() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(4 * 1024 * KIB, 5);
    let file = placeholder(dir.path(), "f.bin", data.len() as u64);
    let source: Arc<dyn ContentSource> = Arc::new(Ranged { delay: Duration::from_millis(2), ..Ranged::new("c1", data.clone()) });
    let transfers = crate::sync::activity::Transfers::default();
    let tracked = Arc::new(crate::sync::activity::Tracked::new(source, transfers.clone(), "/r/f.bin"));
    let (pool, _first) = pool_of_four();
    let share = Share::new();
    let split = Split::with_piece(Arc::clone(&pool), Arc::clone(&share), 64 * KIB);

    let filling = {
        let (tracked, split, file) = (Arc::clone(&tracked), split.clone(), file.try_clone().unwrap());
        tokio::spawn(async move { fill(&file, &*tracked, &split).await })
    };
    until("four streams", || share.streams() == [4]).await;
    assert_eq!(transfers.list().len(), 1, "one file however many streams it runs");
    assert_eq!(pool.large_held(), 4, "its streams");
    assert_eq!(filling.await.unwrap(), 0);
    assert_eq!(read_back(&file), data);
}

/// Two large files share the large slots evenly, two streams each; a transfer that
/// starts waiting for a slot — the second large file, then a small one — gets one once an
/// extra stream's piece ends, without waiting for a file to finish.
#[tokio::test]
async fn two_large_files_share_the_slots_and_a_waiting_transfer_gets_one() {
    let dir = tempfile::tempdir().unwrap();
    let (a_data, b_data) = (content(16 * 1024 * KIB, 2), content(16 * 1024 * KIB, 3));
    let a_file = placeholder(dir.path(), "a.bin", a_data.len() as u64);
    let b_file = placeholder(dir.path(), "b.bin", b_data.len() as u64);
    let a_source = Arc::new(Ranged { delay: Duration::from_millis(2), ..Ranged::new("a1", a_data.clone()) });
    let b_source = Arc::new(Ranged { delay: Duration::from_millis(2), ..Ranged::new("b1", b_data.clone()) });
    let (pool, a_first) = pool_of_four();
    let share = Share::new();
    let split = Split::with_piece(Arc::clone(&pool), Arc::clone(&share), 64 * KIB);

    let a = {
        let (source, split, file) = (Arc::clone(&a_source), split.clone(), a_file.try_clone().unwrap());
        tokio::spawn(async move {
            let filled = fill(&file, &*source, &split).await;
            drop(a_first);
            filled
        })
    };
    until("the first file in four streams", || share.streams() == [4]).await;

    // The second file waits for a large slot as the pins' worker does.
    let b = {
        let (source, split, file, pool) = (Arc::clone(&b_source), split.clone(), b_file.try_clone().unwrap(), Arc::clone(&pool));
        tokio::spawn(async move {
            let _first = pool.acquire_sized(Class::Download, Size::Large).await;
            fill(&file, &*source, &split).await
        })
    };
    until("two streams each", || share.streams() == [2, 2]).await;
    assert!(!a.is_finished() && !b.is_finished(), "shared while both download");

    let small = tokio::time::timeout(Duration::from_secs(5), pool.acquire(Class::Download)).await;
    assert!(small.is_ok(), "a small transfer waiting gets a slot");
    assert!(!a.is_finished() || !b.is_finished(), "before both files are done");
    drop(small);

    assert_eq!(a.await.unwrap(), 0);
    assert_eq!(b.await.unwrap(), 0);
    assert_eq!(read_back(&a_file), a_data);
    assert_eq!(read_back(&b_file), b_data);
    assert!(a_source.peak() <= 4 && b_source.peak() <= 4);
    assert_eq!(pool.large_held(), 0);
}

/// Three breaks of one piece fail the download; what is kept is the gap-free start its
/// checkpoint counts, and the next fill continues from there and asks only for what lies
/// beyond it.
#[tokio::test]
async fn a_failed_download_is_continued_from_its_gap_free_start() {
    set_checkpoint_every(64 * KIB);
    let dir = tempfile::tempdir().unwrap();
    let data = content(1024 * KIB, 4);
    let file = placeholder(dir.path(), "f.bin", data.len() as u64);
    let (pool, _first) = pool_of_four();
    let split = Split::with_piece(Arc::clone(&pool), Share::new(), 128 * KIB);

    let breaking = Ranged { breaks: Some((512 * KIB, 640 * KIB, 1024)), ..Ranged::new("c1", data.clone()) };
    assert_eq!(fill(&file, &breaking, &split).await, libc::EIO);
    let tries = breaking.asked().iter().filter(|(from, _)| (512 * KIB..640 * KIB).contains(from)).count();
    assert_eq!(tries, 3, "three breaks of the piece, then no more");
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    let kept = read_progress(&file).unwrap().expect("a checkpoint is kept").bytes;
    assert!(kept > 0 && kept <= 516 * KIB, "the gap-free start only: {kept}");
    let back = read_back(&file);
    assert_eq!(&back[..kept as usize], &data[..kept as usize]);
    assert!(back[kept as usize..].iter().all(|b| *b == 0), "everything past it is punched");

    let source = Ranged::new("c1", data.clone());
    assert_eq!(fill(&file, &source, &split).await, 0);
    clear_checkpoint_every();

    let asked = source.asked();
    assert_eq!(asked[0].0, kept, "it continues from the checkpoint");
    let bytes: u64 = asked.iter().map(|(from, end)| end.unwrap().min(data.len() as u64) - from).sum();
    assert_eq!(bytes, data.len() as u64 - kept, "and asks only for what lies beyond it");
    assert_eq!(read_back(&file), data);
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
}

/// A piece answered for another version starts the whole file over, with the new version.
#[tokio::test]
async fn a_new_version_mid_way_starts_the_file_over() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (content(1024 * KIB, 5), content(900 * KIB, 6));
    let file = placeholder(dir.path(), "f.bin", old.len() as u64);
    let source = Ranged { second: Some(("c2".into(), new.clone())), switch_at: 5, ..Ranged::new("c1", old) };
    let (pool, _first) = pool_of_four();
    let split = Split::with_piece(Arc::clone(&pool), Share::new(), 64 * KIB);

    assert_eq!(fill(&file, &source, &split).await, 0);

    assert_eq!(source.asked()[5..].iter().filter(|(from, _)| *from == 0).count(), 1, "from the start once more");
    assert_eq!(read_back(&file), new);
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
}

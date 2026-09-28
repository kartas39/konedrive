//! A large pinned download in parallel parts (issue #28, `docs/design/hydration.md` §7.5).
//!
//! One stream from OneDrive does not fill a fast link, so a large pinned file is cut into
//! pieces of [`PIECE`] bytes (the last one shorter). Each stream downloads one piece at a time
//! with a bounded range, writes it at its offset, and takes the next piece not yet taken, in
//! file order.
//!
//! - The file's **first stream** runs in the slot the pins' worker took for it. It may add
//!   **extra streams**, each in a large slot of the pool taken without waiting
//!   (`try_acquire_sized`), only while nothing waits for a slot, and only when no other file in
//!   parts that wants more streams has fewer ([`Share`]). After each piece an extra stream gives
//!   its slot back if anything waits for one, or if another file has at least two streams
//!   fewer than this one.
//! - Every answer must be of the version the first one was; another version starts the whole
//!   file over, once.
//! - Each piece is hashed at its offset, and the pieces combine into the whole file's
//!   QuickXorHash (`QuickXor::at`, `QuickXor::combine`); a mismatch starts over once.
//! - The checkpoint (`user.konedrive.progress`) keeps its meaning: the bytes on disk from the
//!   start without a gap, made durable every [`checkpoint_every`] as the gap-free start grows.
//!   A piece finished beyond a gap is not recorded.
//! - A dropped or short answer continues its piece from where the bytes stopped; three breaks
//!   of one piece fail the download.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use konedrive_fs::placeholder::{remove_progress, write_progress, Progress};
use tokio::io::AsyncReadExt;

use super::{checkpoint_every, errno_of, rehash, same_version, ContentSource, Downloaded, Fetched, SourceError, Version};
use crate::pool::{Class, Size, Slot, TransferPool};
use crate::quickxor::QuickXor;

/// The size of a piece. A guess (the limitations log): large enough that a request's
/// round trip is nothing beside it, small enough that four streams share a file's end evenly.
pub const PIECE: u64 = 256 * 1024 * 1024;

/// How often a download in parts looks for a free slot to add a stream in.
const LOOK_AGAIN: Duration = Duration::from_millis(100);

/// The bytes a stream reads at once.
const BUFFER: usize = 256 * 1024;

/// The large files one account is downloading in parts, and how many streams each has: who is
/// due the next free large slot. One per account, beside its transfer pool.
#[derive(Default)]
pub struct Share {
    files: Mutex<Vec<Member>>,
    next_id: AtomicU64,
}

struct Member {
    id: u64,
    streams: usize,
    /// It has pieces no stream has taken yet: another stream would have work.
    wants: bool,
}

impl Share {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    fn join(self: &Arc<Self>) -> Joined {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().push(Member { id, streams: 1, wants: false });
        Joined { share: Arc::clone(self), id }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Member>> {
        self.files.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// How many streams each file in parts has now (tests).
    #[cfg(test)]
    pub fn streams(&self) -> Vec<usize> {
        self.lock().iter().map(|m| m.streams).collect()
    }
}

/// One file's place in the [`Share`], taken out when dropped.
struct Joined {
    share: Arc<Share>,
    id: u64,
}

impl Joined {
    fn with<T>(&self, f: impl FnOnce(&mut Member, &[&Member]) -> T) -> T {
        let mut files = self.share.lock();
        let at = files.iter().position(|m| m.id == self.id).expect("a joined file stays until dropped");
        let mut me = files.remove(at);
        let others: Vec<&Member> = files.iter().collect();
        let out = f(&mut me, &others);
        files.insert(at, me);
        out
    }

    fn set_wants(&self, wants: bool) {
        self.with(|me, _| me.wants = wants);
    }

    fn add_stream(&self) {
        self.with(|me, _| me.streams += 1);
    }

    fn remove_stream(&self) {
        self.with(|me, _| me.streams -= 1);
    }

    /// The next free slot is this file's: it has work for another stream, and no other file
    /// that wants one has fewer streams.
    fn due(&self) -> bool {
        self.with(|me, others| me.wants && others.iter().filter(|o| o.wants).all(|o| me.streams <= o.streams))
    }

    /// This file holds a slot another one is owed: one that wants more streams has at least
    /// two fewer.
    fn owes(&self) -> bool {
        self.with(|me, others| others.iter().any(|o| o.wants && o.streams + 1 < me.streams))
    }
}

impl Drop for Joined {
    fn drop(&mut self) {
        self.share.lock().retain(|m| m.id != self.id);
    }
}

/// What a download in parts needs besides its source: the account's pool and [`Share`], and
/// the size of a piece.
#[derive(Clone)]
pub struct Split {
    pool: Arc<TransferPool>,
    share: Arc<Share>,
    piece: u64,
}

impl Split {
    pub fn new(pool: Arc<TransferPool>, share: Arc<Share>) -> Self {
        Self::with_piece(pool, share, PIECE)
    }

    /// With pieces of `piece` bytes (tests).
    pub fn with_piece(pool: Arc<TransferPool>, share: Arc<Share>, piece: u64) -> Self {
        Self { pool, share, piece: piece.max(1) }
    }
}

/// One stream of a download in parts, running.
type Stream<'s> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), End>> + Send + 's>>;

/// Why an attempt at the whole file ended without it.
enum End {
    /// Answered with this errno; nothing more is tried.
    Fail(i32),
    /// The file changed in the cloud mid-download: start over, once.
    Changed,
    /// The pieces do not combine into the file's quickXorHash: start over, once.
    Mismatch,
    /// The checkpoint cannot be continued from: download from the start (not a start over).
    DropCheckpoint,
}

/// A download in parts, from `resume` if there is one: what to commit, or the errno to answer
/// with. The rules for versions, checkpoints and hashes are [`super::download`]'s.
pub(super) async fn download(
    file: &File,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    mut resume: Option<Progress>,
    split: &Split,
) -> Result<Downloaded, i32> {
    let joined = split.share.join();
    let mut started_over = false;
    loop {
        match attempt(file, item_id, original_size, source, resume.take(), split, &joined).await {
            Ok(downloaded) => return Ok(downloaded),
            Err(End::Fail(errno)) => return Err(errno),
            Err(End::DropCheckpoint) => {
                remove_progress(file).map_err(|e| errno_of(&e))?;
            }
            Err(End::Changed) => {
                if started_over {
                    tracing::error!("{item_id}: the file keeps changing in the cloud while it downloads");
                    return Err(libc::EIO);
                }
                tracing::info!("{item_id}: the file changed in the cloud mid-download; starting over");
                started_over = true;
                remove_progress(file).map_err(|e| errno_of(&e))?;
            }
            Err(End::Mismatch) => {
                // Its checkpoints count bytes of content that failed the hash.
                remove_progress(file).map_err(|e| errno_of(&e))?;
                if started_over {
                    tracing::error!("{item_id}: the content does not match its quickXorHash, twice");
                    return Err(libc::EIO);
                }
                tracing::warn!("{item_id}: the content does not match its quickXorHash; downloading it once more");
                started_over = true;
            }
        }
    }
}

/// One piece of the file: `[start, end)`, and how far its bytes have come.
struct Piece {
    start: u64,
    end: u64,
    at: u64,
}

/// What the streams of one attempt share.
struct State {
    pieces: Vec<Piece>,
    /// The first piece no stream has taken yet.
    next: usize,
    /// The prefix a checkpoint gave, and every finished piece, combined.
    hash: QuickXor,
    /// Bytes on disk, the prefix included.
    written: u64,
    last_checkpoint: u64,
}

impl State {
    /// The end of the bytes on disk from the start without a gap.
    fn front(&self, start: u64) -> u64 {
        let mut front = start;
        for piece in &self.pieces {
            front = piece.at;
            if piece.at < piece.end {
                break;
            }
        }
        front
    }
}

struct Attempt<'a> {
    file: &'a File,
    item_id: &'a str,
    source: &'a dyn ContentSource,
    split: &'a Split,
    joined: &'a Joined,
    /// Where the pieces start: the checkpoint's end, or 0.
    start: u64,
    size: u64,
    version: Option<Version>,
    state: Mutex<State>,
}

async fn attempt(
    file: &File,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    resume: Option<Progress>,
    split: &Split,
    joined: &Joined,
) -> Result<Downloaded, End> {
    let start = resume.as_ref().map_or(0, |p| p.bytes);
    // The first answer, for the first piece, says which version and how large the file is.
    let mut breaks = 0u32;
    let fetched = open(source, item_id, start, start.saturating_add(split.piece), &mut breaks).await?;
    if fetched.served_from != start {
        tracing::error!(
            "{item_id}: asked for byte {start} and got a stream starting at {}; refusing rather than \
             write it at the wrong offset",
            fetched.served_from
        );
        return Err(End::Fail(libc::EIO));
    }
    if fetched.size == 0 && original_size != 0 {
        tracing::error!(
            "{item_id}: the source declares size 0 for a placeholder of {original_size} bytes; refusing \
             to truncate it here"
        );
        return Err(End::Fail(libc::EIO));
    }
    let mut hash = QuickXor::new();
    if let Some(progress) = &resume {
        let same = fetched
            .version
            .as_ref()
            .is_some_and(|v| v.ctag == progress.ctag && v.quick_xor.is_some());
        let mut buffer = vec![0u8; BUFFER];
        match same.then(|| rehash(file, progress.bytes, &mut buffer)).flatten() {
            Some(rebuilt) if progress.bytes <= fetched.size => hash = rebuilt,
            _ => {
                tracing::info!(
                    "{item_id}: the checkpoint at byte {} is for another version, has no hash to be \
                     checked against, or cannot be read back; downloading from the start",
                    progress.bytes
                );
                return Err(End::DropCheckpoint);
            }
        }
    }

    let size = fetched.size;
    let version = fetched.version.clone();
    let mtime = fetched.mtime;
    let mut pieces = Vec::new();
    let mut at = start;
    while at < size {
        let end = at.saturating_add(split.piece).min(size);
        pieces.push(Piece { start: at, end, at });
        at = end;
    }
    // With nothing left to download, the first answer only said which version the file is.
    let first = (!pieces.is_empty()).then_some((0usize, fetched));
    let state = State { next: usize::from(first.is_some()), pieces, hash, written: start, last_checkpoint: start };
    let attempt = Attempt { file, item_id, source, split, joined, start, size, version, state: Mutex::new(state) };
    attempt.run(first, breaks).await?;
    attempt.finish(mtime)
}

/// A fetch of `[from, end)`, retried while the source says the failure is passing: each
/// failure is a break of the piece, and the third ends the download.
async fn open(source: &dyn ContentSource, item_id: &str, from: u64, end: u64, breaks: &mut u32) -> Result<Fetched, End> {
    loop {
        match source.fetch(item_id, from, Some(end)).await {
            Ok(fetched) => return Ok(fetched),
            Err(SourceError::NotFound(_)) => return Err(End::Fail(libc::EIO)),
            Err(SourceError::Transient(why)) => {
                *breaks += 1;
                if *breaks >= 3 {
                    tracing::warn!("{item_id}: giving up after three failures: {why}");
                    return Err(End::Fail(libc::EIO));
                }
                tokio::time::sleep(Duration::from_millis(200 * u64::from(*breaks))).await;
            }
        }
    }
}

impl Attempt<'_> {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn untaken(&self) -> bool {
        let state = self.lock();
        state.next < state.pieces.len()
    }

    /// The next piece no stream has taken, in file order.
    fn take(&self) -> Option<usize> {
        let taken = {
            let mut state = self.lock();
            let next = state.next;
            (next < state.pieces.len()).then(|| {
                state.next += 1;
                next
            })
        };
        self.joined.set_wants(self.untaken());
        taken
    }

    /// Runs the first stream, with the first answer, and adds extra streams while slots are free
    /// for them; ends when every piece is down, or with the first stream's or piece's failure.
    async fn run(&self, first: Option<(usize, Fetched)>, first_breaks: u32) -> Result<(), End> {
        self.joined.set_wants(self.untaken());
        let mut running = FuturesUnordered::new();
        running.push(self.stream(first.map(|(i, f)| (i, f, first_breaks)), None));
        loop {
            self.add_streams(&mut running);
            tokio::select! {
                ended = running.next() => match ended {
                    Some(Ok(())) => {}
                    // The other streams go with `running`, and their slots with them.
                    Some(Err(end)) => return Err(end),
                    None => break,
                },
                () = tokio::time::sleep(LOOK_AGAIN) => {}
            }
        }
        let state = self.lock();
        if state.pieces.iter().any(|p| p.at < p.end) {
            tracing::error!("{}: every stream of a download in parts ended with pieces left", self.item_id);
            return Err(End::Fail(libc::EIO));
        }
        Ok(())
    }

    /// Extra streams, one per large slot that is free, that nothing waits for, and that this
    /// file is due.
    fn add_streams<'s>(&'s self, running: &mut FuturesUnordered<Stream<'s>>) {
        while self.untaken() && self.joined.due() && !self.split.pool.waiting() {
            let Some(slot) = self.split.pool.try_acquire_sized(Class::Download, Size::Large) else { break };
            self.joined.add_stream();
            running.push(self.stream(None, Some(slot)));
        }
    }

    /// One stream: `first` (a piece already taken, with its answer), then the next piece not
    /// taken, until none is left — or, for an extra stream (`slot`), until its slot is owed
    /// elsewhere.
    fn stream<'s>(&'s self, first: Option<(usize, Fetched, u32)>, slot: Option<Slot>) -> Stream<'s> {
        Box::pin(async move {
            // Counted off however it ends, a failure or a drop included.
            let _counted = slot.as_ref().map(|_| StreamCount(self.joined));
            let mut slot = slot;
            let mut buffer = vec![0u8; BUFFER];
            let mut next = first.map(|(i, fetched, breaks)| (i, Some(fetched), breaks));
            loop {
                let (i, answer, breaks) = match next.take() {
                    Some(taken) => taken,
                    None => match self.take() {
                        Some(i) => (i, None, 0),
                        None => break,
                    },
                };
                self.piece(i, answer, breaks, &mut buffer).await?;
                if slot.is_some() && (self.split.pool.waiting() || self.joined.owes()) {
                    break;
                }
            }
            if let Some(slot) = slot.as_mut() {
                slot.succeeded();
            }
            Ok(())
        })
    }

    /// Downloads piece `i`, from `answer` if one was fetched for it already.
    async fn piece(&self, i: usize, mut answer: Option<Fetched>, mut breaks: u32, buffer: &mut [u8]) -> Result<(), End> {
        let (start, end) = {
            let state = self.lock();
            (state.pieces[i].start, state.pieces[i].end)
        };
        let mut at = start;
        let mut hasher = QuickXor::at(start);
        loop {
            let fetched = match answer.take() {
                Some(fetched) => fetched,
                None => open(self.source, self.item_id, at, end, &mut breaks).await?,
            };
            if fetched.served_from != at {
                tracing::error!(
                    "{}: asked for byte {at} and got a stream starting at {}; refusing rather than \
                     write it at the wrong offset",
                    self.item_id,
                    fetched.served_from
                );
                return Err(End::Fail(libc::EIO));
            }
            if !same_version(&self.version, &fetched.version) || fetched.size != self.size {
                return Err(End::Changed);
            }
            let mut stream = fetched.stream;
            loop {
                let want = buffer.len().min((end - at) as usize);
                if want == 0 {
                    break;
                }
                let read = match stream.read(&mut buffer[..want]).await {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(e) => {
                        tracing::warn!("{}: the piece at byte {start} broke after byte {at}: {e}", self.item_id);
                        break;
                    }
                };
                // Positioned, as a single stream writes (`super::download`).
                self.file.write_all_at(&buffer[..read], at).map_err(|e| End::Fail(errno_of(&e)))?;
                hasher.update(&buffer[..read]);
                at += read as u64;
                self.advanced(i, at)?;
            }
            if at == end {
                self.lock().hash.combine(&hasher);
                return Ok(());
            }
            // A break, or a short answer: continue the piece from where its bytes stopped.
            breaks += 1;
            if breaks >= 3 {
                tracing::warn!("{}: the piece at byte {start} broke three times; giving up", self.item_id);
                return Err(End::Fail(libc::EIO));
            }
            tokio::time::sleep(Duration::from_millis(200 * u64::from(breaks))).await;
        }
    }

    /// Piece `i` has its bytes up to `at` on disk: the progress shown moves on, and the
    /// checkpoint with the gap-free start once that has grown by [`checkpoint_every`] — the
    /// bytes first, durably, then the count.
    fn advanced(&self, i: usize, at: u64) -> Result<(), End> {
        let written = {
            let mut state = self.lock();
            let piece = &mut state.pieces[i];
            let moved = at - piece.at;
            piece.at = at;
            state.written += moved;
            if let Some(Version { ctag, quick_xor: Some(_) }) = &self.version {
                let front = state.front(self.start);
                if front - state.last_checkpoint >= checkpoint_every() {
                    self.file.sync_data().map_err(|e| End::Fail(errno_of(&e)))?;
                    write_progress(self.file, &Progress { ctag: ctag.clone(), bytes: front })
                        .map_err(|e| End::Fail(errno_of(&e)))?;
                    state.last_checkpoint = front;
                }
            }
            state.written
        };
        self.source.progress(written, self.size);
        Ok(())
    }

    /// Every piece is down: the combined hash is checked, as a single stream's is.
    fn finish(&self, mtime: std::time::SystemTime) -> Result<Downloaded, End> {
        let state = self.lock();
        match self.version.as_ref().map(|v| v.quick_xor) {
            Some(Some(want)) if state.hash.finish() != want => return Err(End::Mismatch),
            Some(None) => {
                tracing::warn!("{}: OneDrive gave no quickXorHash; the content could not be verified", self.item_id);
            }
            _ => {}
        }
        Ok(Downloaded { size: self.size, mtime, version: self.version.clone() })
    }
}

/// An extra stream's place in its file's count in the [`Share`].
struct StreamCount<'a>(&'a Joined);

impl Drop for StreamCount<'_> {
    fn drop(&mut self) {
        self.0.remove_stream();
    }
}

#[cfg(test)]
mod tests {
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
}

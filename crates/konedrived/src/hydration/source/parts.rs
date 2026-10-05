//! A large pinned download in parallel parts (`docs/design/hydration.md` §7.5).
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
//!   start without a gap, made durable every `Tuning::checkpoint_every` as the gap-free start
//!   grows.
//!   A piece finished beyond a gap is not recorded.
//! - A dropped or short answer continues its piece from where the bytes stopped; three breaks
//!   of one piece fail the download.
//! - The file is touched in blocking sections ([`Target`]). The streams' writes run beside
//!   each other; a checkpoint, and everything else, runs alone, when the writes under way are
//!   over.

use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use konedrive_fs::placeholder::{write_progress, Progress};
use tokio::io::AsyncReadExt;

use super::download::Downloaded;
use super::guards::{check_answer, open, resume, same_version, verify, Breaks, End, StartOver, Tuning, BUFFER};
use super::target::Target;
use super::{ContentSource, Fetched, Version};
use konedrive_graph::pool::{Class, Size, Slot, TransferPool};
use konedrive_graph::quickxor::QuickXor;

/// The size of a piece. A guess (the limitations log): large enough that a request's
/// round trip is nothing beside it, small enough that four streams share a file's end evenly.
pub const PIECE: u64 = 256 * 1024 * 1024;

/// How often a download in parts looks for a free slot to add a stream in.
const LOOK_AGAIN: Duration = Duration::from_millis(100);

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
        crate::panic::lock(&self.files)
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
        Self { pool, share, piece: PIECE }
    }

    /// With pieces of `piece` bytes.
    #[cfg(test)]
    pub fn with_piece(pool: Arc<TransferPool>, share: Arc<Share>, piece: u64) -> Self {
        Self { pool, share, piece: piece.max(1) }
    }
}

/// One stream of a download in parts, running.
type Stream<'s> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), End>> + Send + 's>>;

/// A download in parts, from `checkpoint` if there is one: what to commit, or the errno to
/// answer with. The rules for versions, checkpoints and hashes are a single stream's
/// (`download`, `guards`).
pub(super) async fn download(
    target: &Target,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    mut checkpoint: Option<Progress>,
    split: &Split,
    tuning: &Tuning,
) -> Result<Downloaded, i32> {
    let joined = split.share.join();
    let mut start_over = StartOver::default();
    loop {
        match attempt(target, item_id, original_size, source, checkpoint.take(), split, &joined, tuning).await {
            Ok(downloaded) => return Ok(downloaded),
            Err(end) => start_over.after(end, target, item_id, tuning).await?,
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
    target: &'a Target,
    item_id: &'a str,
    source: &'a dyn ContentSource,
    split: &'a Split,
    joined: &'a Joined,
    tuning: &'a Tuning,
    original_size: u64,
    /// Where the pieces start: the checkpoint's end, or 0.
    start: u64,
    size: u64,
    version: Option<Version>,
    state: Mutex<State>,
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    target: &Target,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    checkpoint: Option<Progress>,
    split: &Split,
    joined: &Joined,
    tuning: &Tuning,
) -> Result<Downloaded, End> {
    let start = checkpoint.as_ref().map_or(0, |p| p.bytes);
    // The first answer, for the first piece, says which version and how large the file is.
    let mut breaks = Breaks::new(tuning);
    let fetched = open(source, item_id, start, Some(start.saturating_add(split.piece)), &mut breaks).await?;
    check_answer(item_id, &fetched, start, original_size)?;
    let hash = match &checkpoint {
        Some(progress) => resume(target, item_id, progress, &fetched.version, fetched.size).await?,
        None => QuickXor::new(),
    };

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
    let attempt = Attempt { target, item_id, source, split, joined, tuning, original_size, start, size, version, state: Mutex::new(state) };
    attempt.run(first, breaks).await?;
    attempt.finish(mtime)
}

impl Attempt<'_> {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        crate::panic::lock(&self.state)
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
    async fn run(&self, first: Option<(usize, Fetched)>, first_breaks: Breaks) -> Result<(), End> {
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
    fn stream<'s>(&'s self, first: Option<(usize, Fetched, Breaks)>, slot: Option<Slot>) -> Stream<'s> {
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
                        Some(i) => (i, None, Breaks::new(self.tuning)),
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
    async fn piece(&self, i: usize, mut answer: Option<Fetched>, mut breaks: Breaks, buffer: &mut Vec<u8>) -> Result<(), End> {
        let (start, end) = {
            let state = self.lock();
            (state.pieces[i].start, state.pieces[i].end)
        };
        let mut at = start;
        let mut hasher = QuickXor::at(start);
        loop {
            let fetched = match answer.take() {
                // The first answer of the attempt: checked there.
                Some(fetched) => fetched,
                None => {
                    let fetched = open(self.source, self.item_id, at, Some(end), &mut breaks).await?;
                    check_answer(self.item_id, &fetched, at, self.original_size)?;
                    fetched
                }
            };
            if !same_version(&self.version, &fetched.version) || fetched.size != self.size {
                return Err(End::Changed);
            }
            let mut stream = fetched.stream;
            let mut broke = None;
            loop {
                let want = buffer.len().min((end - at) as usize);
                if want == 0 {
                    break;
                }
                let read = match stream.read(&mut buffer[..want]).await {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(e) => {
                        broke = Some(e.to_string());
                        break;
                    }
                };
                hasher.update(&buffer[..read]);
                // Positioned, as a single stream writes (`super::download`). The buffer goes
                // into the section and comes back.
                let chunk = std::mem::take(buffer);
                *buffer = self
                    .target
                    .beside(move |file| file.write_all_at(&chunk[..read], at).map(|()| chunk))
                    .await?;
                at += read as u64;
                self.advanced(i, at).await?;
            }
            if at == end {
                self.lock().hash.combine(&hasher);
                return Ok(());
            }
            // A break, or a short answer: continue the piece from where its bytes stopped.
            let why = broke.unwrap_or_else(|| "the answer ended early".into());
            breaks.again(self.item_id, &format!("the piece at byte {start} broke after byte {at}: {why}")).await?;
        }
    }

    /// Piece `i` has its bytes up to `at` on disk: the progress shown moves on, and the
    /// checkpoint with the gap-free start once that has grown by `Tuning::checkpoint_every` — the
    /// bytes first, durably, then the count.
    ///
    /// The checkpoint is a section of its own, alone: it starts when the writes under way are
    /// over and no write starts until it ends. `last_checkpoint` moves before it, so that no
    /// other stream starts the same one meanwhile; a checkpoint that fails ends the attempt.
    async fn advanced(&self, i: usize, at: u64) -> Result<(), End> {
        let (written, checkpoint) = {
            let mut state = self.lock();
            let piece = &mut state.pieces[i];
            let moved = at - piece.at;
            piece.at = at;
            state.written += moved;
            let mut checkpoint = None;
            if let (Some(Version { ctag, quick_xor: Some(_) }), Some(every)) = (&self.version, self.tuning.checkpoint_every) {
                let front = state.front(self.start);
                if front - state.last_checkpoint >= every {
                    state.last_checkpoint = front;
                    checkpoint = Some(Progress { ctag: ctag.clone(), bytes: front });
                }
            }
            (state.written, checkpoint)
        };
        if let Some(progress) = checkpoint {
            self.target
                .alone(move |file| {
                    file.sync_data()?;
                    write_progress(file, &progress)
                })
                .await?;
        }
        self.source.progress(written, self.size);
        Ok(())
    }

    /// Every piece is down: the combined hash is checked, as a single stream's is.
    fn finish(&self, mtime: std::time::SystemTime) -> Result<Downloaded, End> {
        verify(self.item_id, &self.version, &self.lock().hash)?;
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
mod tests;

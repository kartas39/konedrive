//! "Always keep on this device": which items are pinned, and the downloads
//! a pin asks for (`docs/design/pinning.md`).
//!
//! A pin is the attribute `user.konedrive.pin` on a file or a folder
//! (`konedrive_fs::placeholder::XATTR_PIN`), and nothing else: an item is
//! pinned when it, or a folder above it up to the root, carries one. Every
//! question here is asked by name, with `lstat` and `lgetxattr` — never an
//! open, which in a folder with interception would download the file.
//!
//! [`Pins`] is the queue: every online-only file a pin covers is downloaded
//! through the ordinary fill path ([`PinFill`], which `SyncService`
//! implements), each in a background slot of the account's transfer pool
//! (`crate::pool`, `Class::Download`), each file once however often it is asked for.
//! They go folder by folder, in alphabetical order ([`folder_order`]); a large file waiting
//! for the pool's large-file limit lets the small ones behind it go.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use konedrive_fs::placeholder::{State, XATTR_PIN};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::SyncStateHandle;
use crate::pool::{Acquire, Class, Size, Slot, TransferPool};

/// Whether the item at `path` carries its own pin.
pub fn carries_pin(path: &Path) -> bool {
    matches!(xattr::get(path, XATTR_PIN), Ok(Some(_)))
}

/// The nearest folder above `path`, up to and including `root`, that
/// carries a pin. `None` for the root itself, and for anything outside it.
pub fn pinned_above(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut at = path.parent();
    while let Some(dir) = at {
        if !dir.starts_with(root) {
            return None;
        }
        if carries_pin(dir) {
            return Some(dir.to_path_buf());
        }
        at = dir.parent();
    }
    None
}

/// Every folder above `path`, up to and including `root`, that carries a
/// pin, the nearest first.
pub fn pinned_ancestors(root: &Path, path: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut at = path.parent();
    while let Some(dir) = at {
        if !dir.starts_with(root) {
            break;
        }
        if carries_pin(dir) {
            found.push(dir.to_path_buf());
        }
        at = dir.parent();
    }
    found
}

/// Puts a pin on the file or folder `item`, or takes it off. A folder's
/// write bit is lifted under [`super::disk::dir_modes`], so that no window
/// of the materializer's on the same folder interleaves with it; a file's
/// caller holds its per-inode lock, for the same reason against a fill.
pub fn set_pin(item: &std::fs::File, on: bool) -> std::io::Result<()> {
    let _modes = item.metadata()?.is_dir().then(super::disk::dir_modes);
    if on {
        konedrive_fs::placeholder::write_pin(item)
    } else {
        konedrive_fs::placeholder::remove_pin(item)
    }
}

/// What pins `path`: its own pin, or the nearest folder above that has one.
pub fn pinned_by(root: &Path, path: &Path) -> Option<PathBuf> {
    if carries_pin(path) {
        Some(path.to_path_buf())
    } else {
        pinned_above(root, path)
    }
}

/// Why a free-up or unpin of `path`, which `by` pins, is refused: the path
/// refused and what pins it — `by` is `path` itself for `Dehydrate` of a
/// file with a pin of its own. `konedrivectl` reads both back out of exactly
/// this shape; the Dolphin plugin shows it as it is.
pub fn refusal(path: &Path, by: &Path) -> String {
    format!("{} is pinned by {}: unpin it first", path.display(), by.display())
}

/// What [`walk`] says of an item.
#[derive(Debug, Clone, Copy)]
pub struct Seen {
    /// It carries a pin of its own.
    pub own: bool,
    /// It is pinned: by itself, by a folder the walk passed through, or by
    /// what the walk was told was pinned above where it started.
    pub pinned: bool,
}

/// Every regular file and folder from `start` down — `start` included — with
/// whether it is pinned. `inherited` is whether something above `start` pins
/// it. Walked as `activity::walk_files` walks: `.konedrive-*` skipped,
/// symbolic links never followed, no other filesystem entered, nothing
/// deeper than `konedrive_fs::MAX_DEPTH`; only directories are opened.
pub fn walk(start: &Path, inherited: bool, visit: &mut dyn FnMut(&Path, &Metadata, Seen)) {
    let Ok(meta) = std::fs::symlink_metadata(start) else { return };
    if !meta.is_dir() && !meta.is_file() {
        return;
    }
    let own = carries_pin(start);
    let seen = Seen { own, pinned: inherited || own };
    visit(start, &meta, seen);
    if !meta.is_dir() {
        return;
    }
    let dev = meta.dev();
    let mut dirs = vec![(start.to_path_buf(), 0usize, seen.pinned)];
    while let Some((dir, depth, pinned)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if super::activity::reserved(&entry.file_name()) {
                continue;
            }
            // `DirEntry::metadata` does not follow a symbolic link.
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() && !meta.is_file() {
                continue;
            }
            let path = entry.path();
            let own = carries_pin(&path);
            let seen = Seen { own, pinned: pinned || own };
            visit(&path, &meta, seen);
            if meta.is_dir() && depth < konedrive_fs::MAX_DEPTH && meta.dev() == dev {
                dirs.push((path, depth + 1, seen.pinned));
            }
        }
    }
}

fn online_only(path: &Path) -> bool {
    super::state_of_path(path) == Some(State::OnlineOnly)
}

/// A file to download, and its size — a placeholder has its full size — which says whether it
/// is a large transfer.
pub type Wanted = (PathBuf, u64);

/// What a walk of the whole folder found: every item with a pin of its own,
/// and every online-only file a pin covers, in [`folder_order`].
#[derive(Debug, Default)]
pub struct Swept {
    pub explicit: BTreeSet<PathBuf>,
    pub online_only: Vec<Wanted>,
}

/// The sweep's walk of the folder at `root`.
pub fn sweep_walk(root: &Path) -> Swept {
    let mut swept = Swept::default();
    walk(root, false, &mut |path, meta, seen| {
        if seen.own {
            swept.explicit.insert(path.to_path_buf());
        }
        if seen.pinned && meta.is_file() && online_only(path) {
            swept.online_only.push((path.to_path_buf(), meta.len()));
        }
    });
    in_folder_order(&mut swept.online_only);
    swept
}

/// Every online-only file at or under `path`, in the order the walk met them.
pub fn online_only_under(path: &Path) -> Vec<Wanted> {
    let mut found = Vec::new();
    walk(path, true, &mut |path, meta, _| {
        if meta.is_file() && online_only(path) {
            found.push((path.to_path_buf(), meta.len()));
        }
    });
    found
}

/// The order pinned downloads go in: folder by folder, alphabetically — a folder's files by
/// name first, then its subfolders by name, each the same way (depth first). Names compare
/// as Dolphin sorts them: without regard to case, and a run of digits as a number, so `file2`
/// comes before `file10`.
pub fn folder_order(path: &Path) -> Vec<(bool, Vec<NamePiece>, OsString)> {
    let names: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let last = names.len().saturating_sub(1);
    names
        .into_iter()
        .enumerate()
        // A folder on the way sorts after every file beside it: `false` before `true`.
        .map(|(i, name)| (i < last, name_pieces(&name.to_string_lossy()), name.to_os_string()))
        .collect()
}

/// A piece of a name as [`folder_order`] compares it: one character (`c`, 0, ""), or a run of
/// digits as a number (`'0'`, count of significant digits, the digits) — it sorts where a digit
/// would, and by value at any length.
pub type NamePiece = (char, usize, String);

fn name_pieces(name: &str) -> Vec<NamePiece> {
    let name = name.to_lowercase();
    let mut pieces = Vec::new();
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if !c.is_ascii_digit() {
            pieces.push((c, 0, String::new()));
            continue;
        }
        let mut digits = c.to_string();
        while let Some(d) = chars.next_if(char::is_ascii_digit) {
            digits.push(d);
        }
        let value = digits.trim_start_matches('0');
        pieces.push(('0', value.len(), value.to_string()));
    }
    pieces
}

/// Sorts `files` into [`folder_order`].
pub fn in_folder_order(files: &mut [Wanted]) {
    files.sort_by_cached_key(|(path, _)| folder_order(path));
}

/// Every downloaded file at or under `start` that holds something — what a
/// free-up frees — but those a pin keeps, which are only counted. `inherited`
/// as for [`walk`].
pub fn downloaded_under(start: &Path, inherited: bool) -> (Vec<PathBuf>, u32) {
    let (mut free, mut kept) = (Vec::new(), 0u32);
    walk(start, inherited, &mut |path, meta, seen| {
        if meta.is_file() && meta.len() > 0 && super::state_of_path(path) == Some(State::Hydrated) {
            if seen.pinned {
                kept += 1;
            } else {
                free.push(path.to_path_buf());
            }
        }
    });
    (free, kept)
}

/// What one pinned download came to, as far as the queue cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filled {
    /// Downloaded, found downloaded already, or no longer pinned.
    Done,
    /// Failed: the file is still online-only, and the sweep after the next
    /// cycle that succeeds queues it again.
    Failed,
    /// The disk is full: the rest of the queue waits for that sweep too.
    NoSpace,
}

/// Downloads one pinned file through the ordinary fill path. `SyncService`
/// is the one there is.
#[async_trait]
pub trait PinFill: Send + Sync {
    async fn fill_pinned(&self, path: &Path) -> Filled;
}

#[derive(Default)]
struct Queue {
    /// Waiting, in the order queued: small files, and large ones apart, so that a large
    /// one waiting for the pool's large-file limit never holds up the small ones behind it.
    small: VecDeque<Wanted>,
    large: VecDeque<Wanted>,
    /// The size of the files waiting, together.
    waiting_bytes: u64,
    /// Pending or downloading now: a file is queued once, however often a
    /// pin or a sweep asks for it.
    known: HashSet<PathBuf>,
    /// A worker runs. It ends when there is nothing left to do, and the next
    /// [`Pins::add`] starts another.
    working: bool,
}

impl Queue {
    fn waiting(&mut self, size: Size) -> &mut VecDeque<Wanted> {
        match size {
            Size::Small => &mut self.small,
            Size::Large => &mut self.large,
        }
    }

    /// The next file waiting of `size`, off the queue.
    fn next(&mut self, size: Size) -> Option<PathBuf> {
        let (file, bytes) = self.waiting(size).pop_front()?;
        self.waiting_bytes = self.waiting_bytes.saturating_sub(bytes);
        Some(file)
    }

    /// How many files wait (not those downloading), and their size together.
    fn left(&self) -> (u32, u64) {
        (u32::try_from(self.small.len() + self.large.len()).unwrap_or(u32::MAX), self.waiting_bytes)
    }

    fn is_empty(&self) -> bool {
        self.small.is_empty() && self.large.is_empty()
    }

    fn drain(&mut self) -> Vec<PathBuf> {
        let mut waiting: Vec<PathBuf> = self.small.drain(..).map(|(file, _)| file).collect();
        waiting.extend(self.large.drain(..).map(|(file, _)| file));
        self.waiting_bytes = 0;
        for file in &waiting {
            self.known.remove(file);
        }
        waiting
    }
}

/// A slot asked for, in [`Pins::work`]: waits for it, or for ever when none is asked for.
async fn granted(asked: &mut Option<Pin<Box<Acquire>>>) -> Slot {
    match asked {
        Some(acquire) => acquire.as_mut().await,
        None => std::future::pending().await,
    }
}

/// The items with a pin of their own, as far as the daemon knows.
#[derive(Default)]
struct Explicit {
    set: BTreeSet<PathBuf>,
    /// Sweeps walking now.
    sweeping: usize,
    /// Every pin put on (`true`) or taken off since the oldest sweep walking
    /// now began, in order: what a sweep's walk may have read too early.
    journal: Vec<(PathBuf, bool)>,
}

/// One sweep's place in [`Explicit::sweeping`]; the journal goes with the
/// last one.
struct SweepGuard<'a>(&'a Mutex<Explicit>);

impl Drop for SweepGuard<'_> {
    fn drop(&mut self) {
        let mut explicit = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        explicit.sweeping -= 1;
        if explicit.sweeping == 0 {
            explicit.journal.clear();
        }
    }
}

/// The pins the daemon knows of and the downloads they ask for.
///
/// `PinnedCount` is the number of items with a pin of their own. Each sweep
/// counts them again from the folder; `Pin` and `FreeUp` add and take off in
/// between — also while a sweep walks, whose count then takes them in.
pub struct Pins {
    queue: Mutex<Queue>,
    explicit: Mutex<Explicit>,
    wake: Notify,
    state: SyncStateHandle,
    /// `None`: nothing is ever downloaded (tests of what is queued).
    filler: Option<Weak<dyn PinFill>>,
    /// The account's transfer pool: a pinned download is background work, so an open
    /// never waits behind a big pinned folder.
    pool: Arc<TransferPool>,
    /// Cancels the downloads under way ([`clear`](Self::clear)); replaced
    /// with a fresh one each time.
    cancel: Mutex<CancellationToken>,
    /// A pinned download failed, or waited for a full disk: the sync sweeps
    /// again after its next cycle that succeeds ([`take_resweep`](Self::take_resweep)).
    resweep: AtomicBool,
}

impl Pins {
    pub fn new(state: SyncStateHandle, filler: Weak<dyn PinFill>, pool: Arc<TransferPool>) -> Arc<Self> {
        Arc::new(Self::with(state, Some(filler), pool))
    }

    /// A queue that only keeps what it is given: tests of what is queued.
    pub fn detached(state: SyncStateHandle) -> Arc<Self> {
        Arc::new(Self::with(state, None, TransferPool::new(crate::pool::DEFAULT_CEILING)))
    }

    fn with(state: SyncStateHandle, filler: Option<Weak<dyn PinFill>>, pool: Arc<TransferPool>) -> Self {
        Self {
            pool,
            queue: Mutex::new(Queue::default()),
            explicit: Mutex::new(Explicit::default()),
            wake: Notify::new(),
            state,
            filler,
            cancel: Mutex::new(CancellationToken::new()),
            resweep: AtomicBool::new(false),
        }
    }

    /// How many items have a pin of their own, as far as is known.
    pub fn count(&self) -> usize {
        self.explicit.lock().unwrap().set.len()
    }

    /// Whether a sweep is owed since a pinned download failed; asking
    /// settles it.
    pub fn take_resweep(&self) -> bool {
        self.resweep.swap(false, Ordering::SeqCst)
    }

    /// Every file pending or downloading now, sorted.
    pub fn queued(&self) -> Vec<PathBuf> {
        let mut all: Vec<PathBuf> = self.queue.lock().unwrap().known.iter().cloned().collect();
        all.sort();
        all
    }

    /// Queues `files` for download, each once, in the order given; how many were queued
    /// now — a file given twice, or pending or downloading already, is not counted
    /// again. Starts a worker when none runs.
    pub fn add(self: &Arc<Self>, files: Vec<Wanted>) -> u32 {
        let mut queue = self.queue.lock().unwrap();
        let mut added = 0u32;
        for (file, bytes) in files {
            if queue.known.insert(file.clone()) {
                queue.waiting(Size::of(bytes)).push_back((file, bytes));
                queue.waiting_bytes += bytes;
                added += 1;
            }
        }
        if added > 0 {
            self.publish_waiting(&queue);
        }
        if queue.is_empty() {
            return added;
        }
        if queue.working {
            self.wake.notify_one();
            return added;
        }
        let Some(filler) = self.filler.clone() else { return added };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            queue.working = true;
            runtime.spawn(Arc::clone(self).work(filler));
        }
        added
    }

    /// Queues every online-only file at or under each of `paths`, in [`folder_order`];
    /// how many.
    pub async fn queue_under(self: &Arc<Self>, paths: Vec<PathBuf>) -> u32 {
        if paths.is_empty() {
            return 0;
        }
        let found = tokio::task::spawn_blocking(move || {
            let mut found: Vec<Wanted> = paths.iter().flat_map(|p| online_only_under(p)).collect();
            in_folder_order(&mut found);
            found
        })
        .await
            .unwrap_or_else(|e| {
                tracing::warn!("the walk for pinned files failed: {e}");
                Vec::new()
            });
        self.add(found)
    }

    /// The sweep: walks the folder at `root`, counts the pins again, and
    /// queues every online-only file they cover — a download a crash, a
    /// failure or a full disk lost included. How many were queued. A folder
    /// forgotten, or another registered, while it walked: what it found is
    /// not about the folder there is now, and nothing comes of it.
    pub async fn sweep(self: &Arc<Self>, root: PathBuf) -> u32 {
        self.explicit.lock().unwrap().sweeping += 1;
        // Counted off however this ends, a dropped future included.
        let _sweeping = SweepGuard(&self.explicit);
        let walked = root.clone();
        let swept = tokio::task::spawn_blocking(move || sweep_walk(&walked)).await;
        let current = self.state.get().root_path == root.display().to_string();
        let swept = {
            let mut explicit = self.explicit.lock().unwrap();
            match swept {
                Ok(swept) if current => {
                    // What the walk found, with every change made while it
                    // walked on top, in order.
                    let mut set = swept.explicit;
                    for (path, on) in &explicit.journal {
                        if *on {
                            set.insert(path.clone());
                        } else {
                            set.remove(path);
                        }
                    }
                    explicit.set = set;
                    self.publish(&explicit.set);
                    Some(swept.online_only)
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!("the walk for pinned files failed: {e}");
                    None
                }
            }
        };
        let Some(online_only) = swept else { return 0 };
        let queued = self.add(online_only);
        if queued > 0 {
            tracing::info!("{queued} file(s) kept on this device are not downloaded yet; they are queued");
        }
        queued
    }

    /// `path` was pinned.
    pub fn pinned(&self, path: PathBuf) {
        self.change(path, true);
    }

    /// `path`'s own pin was taken off.
    pub fn unpinned(&self, path: &Path) {
        self.change(path.to_path_buf(), false);
    }

    fn change(&self, path: PathBuf, on: bool) {
        let mut explicit = self.explicit.lock().unwrap();
        if on {
            explicit.set.insert(path.clone());
        } else {
            explicit.set.remove(&path);
        }
        if explicit.sweeping > 0 {
            explicit.journal.push((path, on));
        }
        self.publish(&explicit.set);
    }

    /// Forgets every pin and everything queued, and cancels the downloads
    /// under way: a Forget. A cancelled download is left as any fill cut
    /// short is — its checkpoint kept, for recovery or the next fill.
    pub fn clear(&self) {
        // The queue first: a download cancelled below gives its slot back,
        // and nothing may be left for the slot to take.
        {
            let mut queue = self.queue.lock().unwrap();
            queue.drain();
            queue.known.clear();
            self.publish_waiting(&queue);
        }
        // A worker waiting for a slot looks again, and gives up the slots it asked for.
        self.wake.notify_one();
        std::mem::take(&mut *self.cancel.lock().unwrap()).cancel();
        self.resweep.store(false, Ordering::SeqCst);
        let mut explicit = self.explicit.lock().unwrap();
        explicit.set.clear();
        explicit.journal.clear();
        self.publish(&explicit.set);
    }

    /// The pinned files waiting to download, and their size: part of what is left to
    /// download (issue #16).
    fn publish_waiting(&self, queue: &Queue) {
        let left = queue.left();
        self.state.update(|s| s.pinned_waiting = left);
    }

    fn publish(&self, explicit: &BTreeSet<PathBuf>) {
        let count = explicit.len() as u32;
        self.state.update(|s| s.pinned_count = count);
    }

    /// Takes files off the queue and downloads them, each in a slot of the
    /// account's transfer pool, until the queue is empty and nothing downloads.
    async fn work(self: Arc<Self>, filler: Weak<dyn PinFill>) {
        let mut running = JoinSet::new();
        // A slot asked for the next small file, and one for the next large file.
        let mut asked: [Option<Pin<Box<Acquire>>>; 2] = [None, None];
        loop {
            while running.try_join_next().is_some() {}
            // Only while a file waits is a slot asked for, so that the pool sees work
            // queued exactly when there is some.
            let waiting = {
                let mut queue = self.queue.lock().unwrap();
                if queue.is_empty() && running.is_empty() {
                    queue.working = false;
                    return;
                }
                [!queue.small.is_empty(), !queue.large.is_empty()]
            };
            for (asked, (waits, size)) in asked.iter_mut().zip(waiting.into_iter().zip([Size::Small, Size::Large])) {
                if !waits {
                    *asked = None;
                } else if asked.is_none() {
                    *asked = Some(Box::pin(self.pool.acquire_sized(Class::Download, size)));
                }
            }
            // A slot first, and only then a file: a file is never out of
            // the queue while it waits for a slot, so a full disk drops it
            // with the rest. A download that ends meanwhile is reaped as it goes.
            let [small, large] = &mut asked;
            let permit = tokio::select! {
                permit = granted(small) => permit,
                permit = granted(large) => permit,
                Some(_) = running.join_next(), if !running.is_empty() => continue,
                () = self.wake.notified() => continue,
            };
            let size = permit.size();
            asked[usize::from(size == Size::Large)] = None;
            let next = {
                let mut queue = self.queue.lock().unwrap();
                let next = queue.next(size);
                self.publish_waiting(&queue);
                next
            };
            let Some(path) = next else {
                drop(permit);
                continue;
            };
            let Some(fill) = filler.upgrade() else {
                // The service is gone: nothing is left to download for.
                let mut queue = self.queue.lock().unwrap();
                queue.drain();
                queue.known.clear();
                queue.working = false;
                self.publish_waiting(&queue);
                return;
            };
            let (this, cancel) = (Arc::clone(&self), self.cancel.lock().unwrap().clone());
            let mut permit = permit;
            running.spawn(async move {
                // Cancelled by a Forget: the fill's future is dropped, as a
                // `Hydrate` whose caller went away is.
                let filled = cancel.run_until_cancelled(fill.fill_pinned(&path)).await.unwrap_or(Filled::Done);
                drop(fill);
                if filled == Filled::Done {
                    permit.succeeded();
                }
                // Before the slot goes back, so that no file is taken from a
                // queue a full disk is about to drop.
                this.finished(&path, filled);
                drop(permit);
            });
        }
    }

    fn finished(&self, path: &Path, filled: Filled) {
        if filled != Filled::Done {
            self.resweep.store(true, Ordering::SeqCst);
        }
        let mut queue = self.queue.lock().unwrap();
        queue.known.remove(path);
        if filled == Filled::NoSpace && !queue.is_empty() {
            let waiting = queue.drain();
            self.publish_waiting(&queue);
            tracing::warn!(
                "the disk is full: {} file(s) kept on this device wait for the next sweep",
                waiting.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use tokio::sync::Semaphore;

    use super::*;
    use crate::sync::SyncSnapshot;

    /// The pool the tests' pins download in: four slots, never more.
    const PIN_SLOTS: usize = 4;

    /// Downloads nothing: each fill counts itself, waits for the test to let
    /// it through, and answers `answer`. One whose future is dropped while it
    /// waits is counted in `dropped`.
    struct Held {
        answer: Filled,
        started: AtomicUsize,
        /// The files started, in order.
        order: Mutex<Vec<PathBuf>>,
        dropped: Arc<AtomicUsize>,
        gate: Semaphore,
    }

    impl Held {
        fn new(answer: Filled) -> Arc<Self> {
            Arc::new(Self {
                answer,
                started: AtomicUsize::new(0),
                order: Mutex::default(),
                dropped: Arc::default(),
                gate: Semaphore::new(0),
            })
        }

        fn started(&self) -> usize {
            self.started.load(Ordering::SeqCst)
        }
    }

    struct CountsDrop(Arc<AtomicUsize>);

    impl Drop for CountsDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl PinFill for Held {
        async fn fill_pinned(&self, path: &Path) -> Filled {
            self.order.lock().unwrap().push(path.to_path_buf());
            self.started.fetch_add(1, Ordering::SeqCst);
            let dropped = CountsDrop(Arc::clone(&self.dropped));
            self.gate.acquire().await.expect("never closed").forget();
            std::mem::forget(dropped);
            self.answer
        }
    }

    fn pins_for(held: &Arc<Held>) -> Arc<Pins> {
        pins_in(held, TransferPool::starting_at(PIN_SLOTS, PIN_SLOTS))
    }

    fn pins_in(held: &Arc<Held>, pool: Arc<TransferPool>) -> Arc<Pins> {
        let state = SyncStateHandle::new(SyncSnapshot { root_path: "/r".into(), ..SyncSnapshot::default() });
        let filler: Weak<dyn PinFill> = Arc::downgrade(held) as Weak<Held>;
        Pins::new(state, filler, pool)
    }

    fn files(n: usize) -> Vec<Wanted> {
        (0..n).map(|i| (PathBuf::from(format!("/r/{i}.bin")), 1024)).collect()
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} never happened");
    }

    /// A full disk drops everything still waiting, rather than failing it
    /// file by file; the downloads under way end on their own, and the next
    /// cycle is told to sweep. A file is queued once, however often given.
    #[tokio::test]
    async fn a_full_disk_drops_what_waits_and_asks_for_a_sweep() {
        let held = Held::new(Filled::NoSpace);
        let pins = pins_for(&held);
        assert_eq!(pins.add(files(10)), 10);
        assert_eq!(pins.add(files(3)), 0, "pending or under way already");
        until("four downloads under way", || held.started() == PIN_SLOTS).await;

        held.gate.add_permits(1);
        until("the waiting files dropped", || pins.queued().len() == PIN_SLOTS - 1).await;
        held.gate.add_permits(PIN_SLOTS);
        until("the queue empty", || pins.queued().is_empty()).await;

        assert_eq!(held.started(), PIN_SLOTS, "nothing that waited was tried");
        assert!(pins.take_resweep());
        assert!(!pins.take_resweep(), "asking settles it");
    }

    /// A Forget cancels the downloads under way and drops the rest.
    #[tokio::test]
    async fn a_forget_cancels_the_downloads_under_way() {
        let held = Held::new(Filled::Done);
        let pins = pins_for(&held);
        pins.add(files(6));
        until("four downloads under way", || held.started() == PIN_SLOTS).await;

        pins.clear();

        until("every download under way cancelled", || held.dropped.load(Ordering::SeqCst) == PIN_SLOTS).await;
        assert!(pins.queued().is_empty());
        assert_eq!(held.started(), PIN_SLOTS, "what waited was dropped, not started");
    }

    /// Folder by folder, alphabetically: a folder's files by name first, then its
    /// subfolders by name, each the same way.
    #[test]
    fn pinned_downloads_go_folder_by_folder_in_alphabetical_order() {
        let mut files: Vec<Wanted> = ["/r/b/z.txt", "/r/B.txt", "/r/a/c/1.txt", "/r/a.txt", "/r/a/2.txt", "/r/a/b/3.txt", "/r/C.txt"]
            .into_iter()
            .map(|p| (PathBuf::from(p), 0))
            .collect();
        in_folder_order(&mut files);
        let order: Vec<&str> = files.iter().map(|(p, _)| p.to_str().unwrap()).collect();
        assert_eq!(order, ["/r/a.txt", "/r/B.txt", "/r/C.txt", "/r/a/2.txt", "/r/a/b/3.txt", "/r/a/c/1.txt", "/r/b/z.txt"]);
    }

    /// Digits compare as numbers, as Dolphin sorts: `file2` before `file10`.
    #[test]
    fn numbers_in_names_sort_by_value() {
        let mut files: Vec<Wanted> = ["/r/file10.txt", "/r/file2.txt", "/r/File1.txt", "/r/file02b.txt", "/r/file.txt"]
            .into_iter()
            .map(|p| (PathBuf::from(p), 0))
            .collect();
        in_folder_order(&mut files);
        let order: Vec<&str> = files.iter().map(|(p, _)| p.to_str().unwrap()).collect();
        assert_eq!(order, ["/r/file.txt", "/r/File1.txt", "/r/file2.txt", "/r/file02b.txt", "/r/file10.txt"]);
    }

    /// A large file waiting for the large-file limit lets the small files queued behind it
    /// go; the queue's order holds otherwise.
    #[tokio::test]
    async fn a_large_file_waiting_for_the_limit_does_not_hold_up_the_small_ones() {
        let held = Held::new(Filled::Done);
        let pool = TransferPool::starting_at(PIN_SLOTS, PIN_SLOTS);
        pool.set_limits(PIN_SLOTS, 1);
        let pins = pins_in(&held, pool);
        let large = crate::pool::LARGE_FROM;
        pins.add(vec![
            (PathBuf::from("/r/big-1.bin"), large),
            (PathBuf::from("/r/big-2.bin"), large),
            (PathBuf::from("/r/small-1.bin"), 10),
            (PathBuf::from("/r/small-2.bin"), 10),
            (PathBuf::from("/r/small-3.bin"), 10),
        ]);
        until("four downloads under way", || held.started() == PIN_SLOTS).await;
        let started: HashSet<PathBuf> = held.order.lock().unwrap().iter().cloned().collect();
        let expected: HashSet<PathBuf> =
            ["/r/big-1.bin", "/r/small-1.bin", "/r/small-2.bin", "/r/small-3.bin"].into_iter().map(PathBuf::from).collect();
        assert_eq!(started, expected, "the second large file waits; the small ones behind it do not");

        held.gate.add_permits(PIN_SLOTS + 1);
        until("the queue empty", || pins.queued().is_empty()).await;
        assert_eq!(held.order.lock().unwrap().last().unwrap(), Path::new("/r/big-2.bin"));
    }
}

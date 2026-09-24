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
//! implements), at most [`PIN_SLOTS`] at once, each file once however often
//! it is asked for.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use konedrive_fs::placeholder::{State, XATTR_PIN};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::SyncStateHandle;

/// Pinned files downloading at once: as many as are filled on open
/// ([`super::FILL_SLOTS`]), in slots of their own, so that a big pinned
/// folder never makes an application's open wait behind it.
pub const PIN_SLOTS: usize = super::FILL_SLOTS;

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

/// What a walk of the whole folder found: every item with a pin of its own,
/// and every online-only file a pin covers.
#[derive(Debug, Default)]
pub struct Swept {
    pub explicit: BTreeSet<PathBuf>,
    pub online_only: Vec<PathBuf>,
}

/// The sweep's walk of the folder at `root`.
pub fn sweep_walk(root: &Path) -> Swept {
    let mut swept = Swept::default();
    walk(root, false, &mut |path, meta, seen| {
        if seen.own {
            swept.explicit.insert(path.to_path_buf());
        }
        if seen.pinned && meta.is_file() && online_only(path) {
            swept.online_only.push(path.to_path_buf());
        }
    });
    swept
}

/// Every online-only file at or under `path`.
pub fn online_only_under(path: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(path, true, &mut |path, meta, _| {
        if meta.is_file() && online_only(path) {
            found.push(path.to_path_buf());
        }
    });
    found
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
    pending: VecDeque<PathBuf>,
    /// Pending or downloading now: a file is queued once, however often a
    /// pin or a sweep asks for it.
    known: HashSet<PathBuf>,
    /// A worker runs. It ends when there is nothing left to do, and the next
    /// [`Pins::add`] starts another.
    working: bool,
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
    /// Cancels the downloads under way ([`clear`](Self::clear)); replaced
    /// with a fresh one each time.
    cancel: Mutex<CancellationToken>,
    /// A pinned download failed, or waited for a full disk: the sync sweeps
    /// again after its next cycle that succeeds ([`take_resweep`](Self::take_resweep)).
    resweep: AtomicBool,
}

impl Pins {
    pub fn new(state: SyncStateHandle, filler: Weak<dyn PinFill>) -> Arc<Self> {
        Arc::new(Self::with(state, Some(filler)))
    }

    /// A queue that only keeps what it is given: tests of what is queued.
    pub fn detached(state: SyncStateHandle) -> Arc<Self> {
        Arc::new(Self::with(state, None))
    }

    fn with(state: SyncStateHandle, filler: Option<Weak<dyn PinFill>>) -> Self {
        Self {
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

    /// Queues `files` for download, each once; how many were queued now —
    /// a file given twice, or pending or downloading already, is not counted
    /// again. Starts a worker when none runs.
    pub fn add(self: &Arc<Self>, files: Vec<PathBuf>) -> u32 {
        let mut queue = self.queue.lock().unwrap();
        let mut added = 0u32;
        for file in files {
            if queue.known.insert(file.clone()) {
                queue.pending.push_back(file);
                added += 1;
            }
        }
        if queue.pending.is_empty() {
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

    /// Queues every online-only file at or under each of `paths`; how many.
    pub async fn queue_under(self: &Arc<Self>, paths: Vec<PathBuf>) -> u32 {
        if paths.is_empty() {
            return 0;
        }
        let found = tokio::task::spawn_blocking(move || paths.iter().flat_map(|p| online_only_under(p)).collect())
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
            queue.pending.clear();
            queue.known.clear();
        }
        std::mem::take(&mut *self.cancel.lock().unwrap()).cancel();
        self.resweep.store(false, Ordering::SeqCst);
        let mut explicit = self.explicit.lock().unwrap();
        explicit.set.clear();
        explicit.journal.clear();
        self.publish(&explicit.set);
    }

    fn publish(&self, explicit: &BTreeSet<PathBuf>) {
        let count = explicit.len() as u32;
        self.state.update(|s| s.pinned_count = count);
    }

    /// Takes files off the queue and downloads them, [`PIN_SLOTS`] at once,
    /// until the queue is empty and nothing downloads.
    async fn work(self: Arc<Self>, filler: Weak<dyn PinFill>) {
        let slots = Arc::new(Semaphore::new(PIN_SLOTS));
        let mut running = JoinSet::new();
        loop {
            while running.try_join_next().is_some() {}
            // A slot first, and only then a file: a file is never out of
            // the queue while it waits for a slot, so a full disk drops it
            // with the rest.
            let permit = Arc::clone(&slots).acquire_owned().await.expect("the semaphore is never closed");
            let next = {
                let mut queue = self.queue.lock().unwrap();
                match queue.pending.pop_front() {
                    Some(path) => Some(path),
                    None if running.is_empty() => {
                        queue.working = false;
                        return;
                    }
                    None => None,
                }
            };
            let Some(path) = next else {
                drop(permit);
                tokio::select! {
                    _ = running.join_next() => {}
                    () = self.wake.notified() => {}
                }
                continue;
            };
            let Some(fill) = filler.upgrade() else {
                // The service is gone: nothing is left to download for.
                let mut queue = self.queue.lock().unwrap();
                queue.pending.clear();
                queue.known.clear();
                queue.working = false;
                return;
            };
            let (this, cancel) = (Arc::clone(&self), self.cancel.lock().unwrap().clone());
            running.spawn(async move {
                // Cancelled by a Forget: the fill's future is dropped, as a
                // `Hydrate` whose caller went away is.
                let filled = cancel.run_until_cancelled(fill.fill_pinned(&path)).await.unwrap_or(Filled::Done);
                drop(fill);
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
        if filled == Filled::NoSpace && !queue.pending.is_empty() {
            let waiting: Vec<PathBuf> = queue.pending.drain(..).collect();
            for file in &waiting {
                queue.known.remove(file);
            }
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

    use super::*;
    use crate::sync::SyncSnapshot;

    /// Downloads nothing: each fill counts itself, waits for the test to let
    /// it through, and answers `answer`. One whose future is dropped while it
    /// waits is counted in `dropped`.
    struct Held {
        answer: Filled,
        started: AtomicUsize,
        dropped: Arc<AtomicUsize>,
        gate: Semaphore,
    }

    impl Held {
        fn new(answer: Filled) -> Arc<Self> {
            Arc::new(Self { answer, started: AtomicUsize::new(0), dropped: Arc::default(), gate: Semaphore::new(0) })
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
        async fn fill_pinned(&self, _path: &Path) -> Filled {
            self.started.fetch_add(1, Ordering::SeqCst);
            let dropped = CountsDrop(Arc::clone(&self.dropped));
            self.gate.acquire().await.expect("never closed").forget();
            std::mem::forget(dropped);
            self.answer
        }
    }

    fn pins_for(held: &Arc<Held>) -> Arc<Pins> {
        let state = SyncStateHandle::new(SyncSnapshot { root_path: "/r".into(), ..SyncSnapshot::default() });
        let filler: Weak<dyn PinFill> = Arc::downgrade(held) as Weak<Held>;
        Pins::new(state, filler)
    }

    fn files(n: usize) -> Vec<PathBuf> {
        (0..n).map(|i| PathBuf::from(format!("/r/{i}.bin"))).collect()
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
}

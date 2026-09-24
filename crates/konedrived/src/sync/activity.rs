//! What the sync tells the user about itself: the activity log,
//! the conflicts, the downloads under way, and the space the folder takes.
//!
//! [`Report`] bundles them, so that everything that downloads, frees up or
//! reconciles — `SyncService`, the hydration loop, a OneDrive folder's
//! listing and its replacements — reports into the same four places, and
//! `sync::dbus` publishes from there.
//!
//! The activity log and the conflicts live in the folder's tree store
//! (`activity`, `conflicts`), which a Forget drops and a rebuild empties. A
//! folder with no tree store — one filled with `PopulateFromDirectory` — keeps
//! its activity in memory only, and has no conflicts: only a reconcile with
//! OneDrive rescues anything.

use std::collections::{BTreeMap, VecDeque};
use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{broadcast, watch, Notify};

use super::source::{ContentSource, Fetched, SourceError};
use super::SyncStateHandle;
use crate::tree::{ActivityRow, ConflictRow, Store, TreeError, ACTIVITY_KEPT};

/// One event of the activity log: unix seconds, a [`Kind`]'s
/// name, a full path and a detail. `RecentActivity` and `ActivityAdded` carry
/// exactly these four fields.
pub type Event = ActivityRow;

/// An incremental cycle logs at most this many events of each kind (spec
/// §16.1), plus one "and N more".
pub const PER_KIND: usize = 50;

/// `LocalBytes` is measured at most this often after a download or a free-up
///.
pub const SPACE_SPACING: Duration = Duration::from_secs(5);

/// What an event records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A file was downloaded: opened, or `Hydrate`. Detail: its size.
    Downloaded,
    /// A file's space was freed up: `Dehydrate`, or `FreeUpSpace` for the
    /// whole folder at once. Detail: how much.
    Freed,
    /// Added in OneDrive, placed here by an incremental cycle.
    Added,
    /// Changed in OneDrive: a placeholder took the new version, or a
    /// downloaded file was replaced by it.
    Updated,
    /// Removed from OneDrive, and so from here.
    Removed,
    /// Moved or renamed in OneDrive. Detail: where it was.
    Moved,
    /// A listing, or a Full reconcile: one event for the whole folder.
    Listed,
    /// A local version moved out of the way. The path is where
    /// it was; the detail, where it is now.
    Conflict,
    /// A download that failed: a fill on open, or `Hydrate`. Detail: why —
    /// exactly "not enough disk space" when the disk is full.
    Failed,
    /// A file changed in OneDrive that could not be replaced here (spec
    /// §7.3); the old version stays. Detail: why — exactly "not enough disk
    /// space" when the disk cannot hold both versions.
    UpdateFailed,
}

impl Kind {
    /// Every kind there is, for whatever has to agree with them all (the
    /// window's guard in `konedrivectl`'s tests).
    pub const ALL: [Kind; 10] = [
        Kind::Downloaded,
        Kind::Freed,
        Kind::Added,
        Kind::Updated,
        Kind::Removed,
        Kind::Moved,
        Kind::Listed,
        Kind::Conflict,
        Kind::Failed,
        Kind::UpdateFailed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Downloaded => "downloaded",
            Kind::Freed => "freed",
            Kind::Added => "added",
            Kind::Updated => "updated",
            Kind::Removed => "removed",
            Kind::Moved => "moved",
            Kind::Listed => "listed",
            Kind::Conflict => "conflict",
            Kind::Failed => "failed",
            Kind::UpdateFailed => "update-failed",
        }
    }
}

/// Unix seconds now.
pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// An event that happens now.
pub fn event(kind: Kind, path: impl Into<String>, detail: impl Into<String>) -> Event {
    Event { at: unix_now(), kind: kind.as_str().to_owned(), path: path.into(), detail: detail.into() }
}

/// `1536` → `1.5 KiB`: the size a `downloaded` or `freed` event's detail
/// gives, in the same form `konedrivectl` prints sizes in.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// What a `failed` or `update-failed` event says when the disk is full
///: exactly this, which the window's notifier tells apart from
/// every other failure.
pub const NO_DISK_SPACE: &str = "not enough disk space";

/// Why a fill failed, for a `failed` event's detail: [`NO_DISK_SPACE`] for a
/// full disk (or quota). `EIO` is everything a fill could not get from
/// OneDrive — a missing item, the network, a hash that does not match —
/// which the daemon's log tells apart.
pub fn failure_reason(errno: i32) -> String {
    match errno {
        libc::ENOSPC | libc::EDQUOT => NO_DISK_SPACE.to_owned(),
        libc::EIO => "it could not be downloaded".to_owned(),
        other => io::Error::from_raw_os_error(other).to_string(),
    }
}

/// `n items`, `1 item`: a `listed` event's detail.
pub fn items(n: u64) -> String {
    if n == 1 {
        "1 item".to_owned()
    } else {
        format!("{n} items")
    }
}

/// At most `per_kind` events of each kind, in the order they
/// came, then — for each kind that had more — one event of that kind at
/// `root` saying "and N more", in the order the kinds first appeared.
pub fn capped(events: Vec<Event>, per_kind: usize, root: &str) -> Vec<Event> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut out = Vec::new();
    for event in events {
        let seen = match counts.iter_mut().find(|(kind, _)| *kind == event.kind) {
            Some((_, seen)) => seen,
            None => {
                counts.push((event.kind.clone(), 0));
                &mut counts.last_mut().expect("just pushed").1
            }
        };
        *seen += 1;
        if *seen <= per_kind {
            out.push(event);
        }
    }
    let at = unix_now();
    for (kind, seen) in counts {
        if seen > per_kind {
            out.push(Event { at, kind, path: root.to_owned(), detail: format!("and {} more", seen - per_kind) });
        }
    }
    out
}

/// The activity log and the conflicts.
///
/// Backed by the folder's tree store while a OneDrive folder syncs
/// ([`attach`](Self::attach)), and by memory otherwise. Every event recorded
/// is also sent to [`subscribe`](Self::subscribe)rs — `sync::dbus` turns them
/// into `ActivityAdded`.
///
/// One lock holds both the store and the memory, and every write holds it for
/// as long as it writes:
///
/// - once [`detach`](Self::detach) returns, no write under way still holds a
///   clone of the store, which is what lets a Forget remove the store's files
///   (see `SyncService::store`);
/// - [`attach`](Self::attach) moves what memory holds into the store and
///   hands the store over in the same hold, so nothing recorded meanwhile can
/// fall between the two.
///
/// An event is kept only while its path is inside the folder registered now
/// (`SyncSnapshot::root_path`): a download that ends after its folder was
/// forgotten — `Hydrate` does not hold the lifecycle lock — records nothing,
/// in memory or in the next folder's store.
pub struct Activity {
    backing: Mutex<Backing>,
    added: broadcast::Sender<Event>,
    state: SyncStateHandle,
}

/// Where the log is kept: the store when there is one, memory when not.
struct Backing {
    store: Option<Store>,
    /// Newest first, at most `ACTIVITY_KEPT`.
    memory: VecDeque<Event>,
}

/// Whether `path` is inside `root` (both full paths); nothing is inside no
/// folder.
fn inside(root: &str, path: &str) -> bool {
    !root.is_empty() && Path::new(path).starts_with(root)
}

impl Activity {
    pub fn new(state: SyncStateHandle) -> Self {
        let (added, _) = broadcast::channel(1024);
        Self { backing: Mutex::new(Backing { store: None, memory: VecDeque::new() }), added, state }
    }

    /// Every event recorded from now on, as it is recorded.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.added.subscribe()
    }

    fn backing(&self) -> std::sync::MutexGuard<'_, Backing> {
        self.backing.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Keeps the log of the folder at `root` in `store` from now on: what was
    /// recorded in memory meanwhile about that folder moves into it — and
    /// anything about another folder is dropped — and `ConflictCount` is
    /// what it holds. Blocking: the store is SQLite.
    pub fn attach(&self, store: Store, root: &Path) {
        {
            let mut backing = self.backing();
            let held: Vec<Event> =
                backing.memory.drain(..).rev().filter(|event| Path::new(&event.path).starts_with(root)).collect();
            if !held.is_empty() {
                if let Err(e) = store.with(|s| s.add_activity(&held)) {
                    tracing::warn!("cannot keep the activity recorded so far: {e}");
                }
            }
            backing.store = Some(store);
        }
        self.prune();
    }

    /// Lets go of the store — after every write under way has finished —
    /// and forgets what memory held: a Forget, or a sync that stops.
    /// Blocking.
    pub fn detach(&self) {
        {
            let mut backing = self.backing();
            backing.store = None;
            backing.memory.clear();
        }
        self.state.update(|s| s.conflict_count = 0);
    }

    /// Records `events`, oldest first, and announces each — those inside the
    /// folder registered now; the rest are dropped. Blocking: call it from a
    /// blocking thread, or through [`record`](Self::record).
    pub fn record_blocking(&self, events: Vec<Event>) {
        let kept: Vec<Event> = {
            let mut backing = self.backing();
            let root = self.state.get().root_path;
            let (kept, dropped): (Vec<Event>, Vec<Event>) = events.into_iter().partition(|e| inside(&root, &e.path));
            if !dropped.is_empty() {
                tracing::debug!("{} event(s) of a folder no longer registered are not recorded", dropped.len());
            }
            if kept.is_empty() {
                return;
            }
            match backing.store.as_ref() {
                Some(store) => {
                    if let Err(e) = store.with(|s| s.add_activity(&kept)) {
                        tracing::warn!("cannot record {} activity event(s): {e}", kept.len());
                    }
                }
                None => {
                    for event in &kept {
                        backing.memory.push_front(event.clone());
                    }
                    backing.memory.truncate(ACTIVITY_KEPT);
                }
            }
            kept
        };
        for event in kept {
            // Nobody listening is not an error: a daemon with no bus, a test.
            let _ = self.added.send(event);
        }
    }

    /// Holds the log still, as a write under way does: tests only.
    #[cfg(test)]
    pub(crate) fn hold(&self) -> impl Sized + '_ {
        self.backing()
    }

    /// [`record_blocking`](Self::record_blocking) on a blocking thread.
    pub async fn record(self: &Arc<Self>, events: Vec<Event>) {
        let this = Arc::clone(self);
        if let Err(e) = tokio::task::spawn_blocking(move || this.record_blocking(events)).await {
            tracing::warn!("the task recording activity failed: {e}");
        }
    }

    /// The newest `limit` events, newest first. Blocking.
    pub fn recent(&self, limit: usize) -> Result<Vec<Event>, TreeError> {
        let backing = self.backing();
        match backing.store.as_ref() {
            Some(store) => store.with(|s| s.recent_activity(limit)),
            None => Ok(backing.memory.iter().take(limit.min(ACTIVITY_KEPT)).cloned().collect()),
        }
    }

    /// Records local versions a reconcile moved out of the way, and drops
    /// any whose file is gone. Blocking. Nothing without a store: only a
    /// OneDrive folder's reconcile rescues anything.
    pub fn add_conflicts(&self, rows: Vec<ConflictRow>) {
        if !rows.is_empty() {
            let backing = self.backing();
            if let Some(store) = backing.store.as_ref() {
                if let Err(e) = store.with(|s| s.add_conflicts(&rows)) {
                    tracing::warn!("cannot record {} conflict(s): {e}", rows.len());
                }
            }
        }
        self.prune();
    }

    /// The conflicts whose rescued file is still there, newest first; the
    /// rest are dropped (a conflict whose file is gone drops off
    /// by itself). Whether it is there is asked with `lstat`, never an open.
    /// `ConflictCount` follows. Blocking.
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        let kept = {
            let backing = self.backing();
            let Some(store) = backing.store.as_ref() else {
                return Ok(Vec::new());
            };
            let rows = store.with(|s| s.conflicts())?;
            let mut kept = Vec::with_capacity(rows.len());
            for row in rows {
                match std::fs::symlink_metadata(&row.rescued) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        store.with(|s| s.remove_conflict(&row.rescued))?;
                    }
                    _ => kept.push(row),
                }
            }
            kept
        };
        let count = kept.len() as u32;
        self.state.update(|s| s.conflict_count = count);
        Ok(kept)
    }

    /// [`conflicts`](Self::conflicts), for its side effects only: the rows
    /// whose file is gone dropped, and `ConflictCount` set. Blocking.
    pub fn prune(&self) {
        if let Err(e) = self.conflicts() {
            tracing::warn!("cannot read the conflicts: {e}");
        }
    }

    /// Takes the conflict whose rescued file is `rescued` off the list; the
    /// file itself is not touched. Whether there was one. Blocking.
    pub fn dismiss(&self, rescued: &str) -> Result<bool, TreeError> {
        let removed = {
            let backing = self.backing();
            match backing.store.as_ref() {
                Some(store) => store.with(|s| s.remove_conflict(rescued))?,
                None => false,
            }
        };
        self.prune();
        Ok(removed)
    }
}

/// One download under way, as `Transfers` publishes it: (full path, bytes
/// done, bytes total).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub path: String,
    pub done: u64,
    pub total: u64,
}

/// The downloads under way (`Transfers`): fills on open,
/// `Hydrate`, and replacements of changed files — not thumbnails.
///
/// Watched, so that `sync::dbus` can publish it coalesced. An entry is added
/// by [`start`](Self::start) and removed when the [`TransferEntry`] it
/// returns is dropped, however the download ends.
#[derive(Clone)]
pub struct Transfers {
    tx: Arc<watch::Sender<BTreeMap<u64, Transfer>>>,
    next: Arc<AtomicU64>,
}

impl Default for Transfers {
    fn default() -> Self {
        Self { tx: Arc::new(watch::Sender::new(BTreeMap::new())), next: Arc::new(AtomicU64::new(0)) }
    }
}

impl Transfers {
    /// A download of `path`, `total` bytes as far as is known yet.
    pub fn start(&self, path: String, total: u64) -> TransferEntry {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.tx.send_modify(|all| {
            all.insert(id, Transfer { path, done: 0, total });
        });
        TransferEntry { handle: TransferHandle { id, transfers: self.clone() } }
    }

    /// Every download under way, oldest first.
    pub fn list(&self) -> Vec<Transfer> {
        self.tx.borrow().values().cloned().collect()
    }

    pub fn subscribe(&self) -> watch::Receiver<BTreeMap<u64, Transfer>> {
        self.tx.subscribe()
    }

    fn progress(&self, id: u64, done: u64, total: u64) {
        self.tx.send_if_modified(|all| match all.get_mut(&id) {
            Some(entry) if (entry.done, entry.total) != (done, total) => {
                entry.done = done;
                entry.total = total;
                true
            }
            _ => false,
        });
    }

    fn remove(&self, id: u64) {
        self.tx.send_if_modified(|all| all.remove(&id).is_some());
    }
}

/// A download's entry in [`Transfers`], removed when this is dropped.
pub struct TransferEntry {
    handle: TransferHandle,
}

impl TransferEntry {
    pub fn progress(&self, done: u64, total: u64) {
        self.handle.progress(done, total);
    }

    /// Something that can move the entry on but does not keep it: the
    /// stream a download reads.
    pub fn handle(&self) -> TransferHandle {
        self.handle.clone()
    }

    /// How big the download is, as far as its source has said.
    pub fn total(&self) -> u64 {
        self.handle.transfers.tx.borrow().get(&self.handle.id).map_or(0, |t| t.total)
    }
}

impl Drop for TransferEntry {
    fn drop(&mut self) {
        self.handle.transfers.remove(self.handle.id);
    }
}

#[derive(Clone)]
pub struct TransferHandle {
    id: u64,
    transfers: Transfers,
}

impl TransferHandle {
    pub fn progress(&self, done: u64, total: u64) {
        self.transfers.progress(self.id, done, total);
    }
}

/// A [`ContentSource`] whose downloads show in [`Transfers`] as `path`.
///
/// The entry is added at the first fetch — a fill that finds nothing to do
/// never fetches, and never shows — and moves on as the bytes are read. It
/// goes when this is dropped: the caller keeps it for exactly as long as
/// the download it is for.
pub struct Tracked {
    source: Arc<dyn ContentSource>,
    transfers: Transfers,
    path: String,
    entry: OnceLock<TransferEntry>,
}

impl Tracked {
    pub fn new(source: Arc<dyn ContentSource>, transfers: Transfers, path: impl Into<String>) -> Self {
        Self { source, transfers, path: path.into(), entry: OnceLock::new() }
    }

    /// The size of what was downloaded, if anything was asked for at all.
    pub fn fetched(&self) -> Option<u64> {
        self.entry.get().map(TransferEntry::total)
    }
}

#[async_trait]
impl ContentSource for Tracked {
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
        let entry = self.entry.get_or_init(|| self.transfers.start(self.path.clone(), 0));
        let Fetched { served_from, size, mtime, version, stream } = self.source.fetch(item_id, from).await?;
        entry.progress(served_from, size);
        let stream = Box::new(Counting { inner: stream, at: served_from, total: size, handle: entry.handle() });
        Ok(Fetched { served_from, size, mtime, version, stream })
    }
}

/// A download's stream, moving its [`Transfers`] entry on as it is read.
struct Counting {
    inner: Box<dyn AsyncRead + Send + Unpin>,
    at: u64,
    total: u64,
    handle: TransferHandle,
}

impl AsyncRead for Counting {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            let read = (buf.filled().len() - before) as u64;
            if read > 0 {
                self.at += read;
                let (at, total) = (self.at, self.total);
                self.handle.progress(at, total);
            }
        }
        polled
    }
}

/// How `LocalBytes` is measured: a function of the folder's path.
type Measure = Arc<dyn Fn(&Path) -> u64 + Send + Sync>;

/// `LocalBytes`: measured by a walk ([`local_bytes`]) on a
/// blocking thread each time it is [`kick`](Self::kick)ed — after every
/// cycle, download and free-up — but never within [`SPACE_SPACING`] of the
/// last walk: a kick meanwhile makes one more walk when that time is up.
///
/// The walker is a task of its own that holds the sync state, so it is
/// stopped with this (`Drop`), and whenever [`stop`](Self::stop) is asked —
/// a sync that stops, a Forget — rather than left to run for the life of
/// the runtime: nothing waiting on the state would ever see it end
///. A kick after a stop starts it again.
pub struct LocalSpace {
    kick: Arc<Notify>,
    state: SyncStateHandle,
    measure: Measure,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// False for a report that reports nowhere ([`Report::nowhere`]): no
    /// walker is ever started for it.
    enabled: bool,
}

impl LocalSpace {
    pub fn new(state: SyncStateHandle) -> Self {
        Self::measuring(state, Arc::new(local_bytes))
    }

    fn measuring(state: SyncStateHandle, measure: Measure) -> Self {
        Self { kick: Arc::new(Notify::new()), state, measure, task: Mutex::new(None), enabled: true }
    }

    fn inert(state: SyncStateHandle) -> Self {
        let mut space = Self::new(state);
        space.enabled = false;
        space
    }

    #[cfg(test)]
    pub(crate) fn running(&self) -> bool {
        self.task.lock().unwrap().as_ref().is_some_and(|task| !task.is_finished())
    }

    /// Stops the walker, if one runs; the next kick starts another.
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            task.abort();
        }
    }

    /// Asks for a walk. The walker starts with the first kick made on a
    /// runtime, so that nothing is spawned where there is none.
    pub fn kick(&self) {
        if !self.enabled {
            return;
        }
        {
            let mut task = self.task.lock().unwrap();
            if task.is_none() {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let (kick, state, measure) = (Arc::clone(&self.kick), self.state.clone(), Arc::clone(&self.measure));
                    *task = Some(runtime.spawn(walker(kick, state, measure)));
                }
            }
        }
        self.kick.notify_one();
    }
}

impl Drop for LocalSpace {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn walker(kick: Arc<Notify>, state: SyncStateHandle, measure: Measure) {
    loop {
        kick.notified().await;
        let root = state.get().root_path;
        let bytes = if root.is_empty() {
            0
        } else {
            let (measure, at) = (Arc::clone(&measure), root.clone());
            match tokio::task::spawn_blocking(move || measure(Path::new(&at))).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!("the walk measuring the folder's space failed: {e}");
                    continue;
                }
            }
        };
        // A folder forgotten, or another registered, while it walked: what
        // it found is not about the folder there is now.
        state.update(|s| {
            if s.root_path == root {
                s.local_bytes = bytes;
            }
        });
        tokio::time::sleep(SPACE_SPACING).await;
    }
}

/// Whether a name is one konedrive keeps for itself (`.konedrive-holding`, a
/// replacement's `.konedrive-new-<id>`, ...), never a user's file.
pub(super) fn reserved(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(crate::drive::item::RESERVED_PREFIX.as_bytes())
}

/// Every regular file under `root` with its `lstat` metadata: `.konedrive-*`
/// entries skipped, symbolic links never followed, no mount point below the
/// root's own filesystem crossed, and nothing deeper than
/// `konedrive_fs::MAX_DEPTH`. No file is opened — opening a placeholder
/// would download it — only directories are.
pub fn walk_files(root: &Path, visit: &mut dyn FnMut(&Path, &Metadata)) {
    let Ok(root_meta) = std::fs::symlink_metadata(root) else { return };
    let mut dirs = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if reserved(&entry.file_name()) {
                continue;
            }
            // `DirEntry::metadata` does not follow a symbolic link.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if depth < konedrive_fs::MAX_DEPTH && meta.dev() == root_meta.dev() {
                    dirs.push((entry.path(), depth + 1));
                }
            } else if meta.is_file() {
                visit(&entry.path(), &meta);
            }
        }
    }
}

/// `LocalBytes`: what the folder's regular files take on disk, `st_blocks ×
/// 512` — a placeholder counts as what it occupies, not its size.
pub fn local_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    walk_files(root, &mut |_, meta| total += meta.blocks() * 512);
    total
}

/// Everything the sync reports into (see the module's doc comment). Cheap to
/// clone; every clone reports into the same places.
#[derive(Clone)]
pub struct Report {
    pub activity: Arc<Activity>,
    pub transfers: Transfers,
    pub space: Arc<LocalSpace>,
}

impl Report {
    pub fn new(state: SyncStateHandle) -> Self {
        Self {
            activity: Arc::new(Activity::new(state.clone())),
            transfers: Transfers::default(),
            space: Arc::new(LocalSpace::new(state)),
        }
    }

    /// A report that goes nowhere: the plain `serve_hydrations`', which no
    /// service reads. It records into a log nobody reads and starts no
    /// walker.
    pub fn nowhere() -> Self {
        let state = SyncStateHandle::new(super::SyncSnapshot::default());
        Self {
            activity: Arc::new(Activity::new(state.clone())),
            transfers: Transfers::default(),
            space: Arc::new(LocalSpace::inert(state)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::sync::atomic::Ordering::SeqCst;

    use super::*;
    use crate::sync::SyncSnapshot;

    fn event_at(kind: &str, path: &str) -> Event {
        Event { at: 1, kind: kind.into(), path: path.into(), detail: String::new() }
    }

    /// The cap on its own: the first `per_kind` of each kind in
    /// the order they came, then one "and N more" per kind that had more.
    #[test]
    fn capped_keeps_the_first_of_each_kind_and_counts_the_rest() {
        let mut events: Vec<Event> = (0..4).map(|n| event_at("added", &format!("/r/a{n}"))).collect();
        events.push(event_at("removed", "/r/x"));
        events.extend((4..6).map(|n| event_at("added", &format!("/r/a{n}"))));
        let out = capped(events, 3, "/r");
        let shown: Vec<_> = out.iter().map(|e| (e.kind.as_str(), e.path.as_str(), e.detail.as_str())).collect();
        assert_eq!(
            shown,
            vec![
                ("added", "/r/a0", ""),
                ("added", "/r/a1", ""),
                ("added", "/r/a2", ""),
                ("removed", "/r/x", ""),
                ("added", "/r", "and 3 more"),
            ]
        );
    }

    /// `LocalBytes` is what the files occupy (`st_blocks ×
    /// 512`): a placeholder counts as the nothing it takes, a downloaded file
    /// as its blocks, and neither konedrive's own `.konedrive-*` entries nor
    /// a symbolic link to a file count at all.
    #[test]
    fn local_bytes_count_what_files_occupy_not_their_size() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let handle = File::open(root).unwrap();
        konedrive_fs::placeholder::create_placeholder(&handle, "online.bin", "P", 1 << 20, SystemTime::UNIX_EPOCH)
            .unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/here.bin"), vec![7u8; 64 * 1024]).unwrap();
        std::fs::create_dir(root.join(".konedrive-holding")).unwrap();
        std::fs::write(root.join(".konedrive-holding/held.bin"), vec![1u8; 64 * 1024]).unwrap();
        std::fs::write(root.join("docs/.konedrive-new-X"), vec![1u8; 64 * 1024]).unwrap();
        std::os::unix::fs::symlink(root.join("docs/here.bin"), root.join("link.bin")).unwrap();

        let blocks = |rel: &str| std::fs::symlink_metadata(root.join(rel)).unwrap().blocks() * 512;
        assert!(blocks("online.bin") < 8 * 512, "a placeholder takes next to nothing");
        assert!(blocks("docs/here.bin") >= 64 * 1024);
        assert_eq!(local_bytes(root), blocks("online.bin") + blocks("docs/here.bin"));
    }

    fn in_folder(root: &str) -> SyncStateHandle {
        SyncStateHandle::new(SyncSnapshot { root_path: root.into(), ..SyncSnapshot::default() })
    }

    fn fresh_store() -> Store {
        Store::new(crate::tree::TreeStore::in_memory().unwrap())
    }

    /// An event of the folder registered before is
    /// not carried into the store of the one attached now.
    #[test]
    fn an_event_of_another_folder_is_not_carried_into_the_one_attached() {
        let state = in_folder("/a");
        let activity = Activity::new(state.clone());
        activity.record_blocking(vec![event(Kind::Downloaded, "/a/f.bin", "1 B")]);
        state.update(|s| s.root_path = "/b".into());
        activity.record_blocking(vec![event(Kind::Downloaded, "/b/g.bin", "1 B")]);
        activity.attach(fresh_store(), Path::new("/b"));
        let paths: Vec<String> = activity.recent(10).unwrap().into_iter().map(|e| e.path).collect();
        assert_eq!(paths, vec!["/b/g.bin".to_owned()]);
    }

    /// Nothing recorded while a store is attached is
    /// lost — not what memory held, nor what is recorded while it moves
    /// into the store. A writer records the whole time the store is
    /// attached; every event it recorded must be in the store after.
    #[test]
    fn nothing_recorded_while_a_store_is_attached_is_lost() {
        use std::sync::atomic::AtomicBool;
        let activity = Arc::new(Activity::new(in_folder("/r")));
        activity.record_blocking((0..100).map(|n| event(Kind::Downloaded, format!("/r/held{n}"), "")).collect());
        // Made first, so that the attach starts the moment the writer runs.
        let store = fresh_store();
        let (recorded, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let writer = {
            let (activity, recorded, stop) = (Arc::clone(&activity), Arc::clone(&recorded), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(SeqCst) && n < 100 {
                    activity.record_blocking(vec![event(Kind::Downloaded, format!("/r/live{n}"), "")]);
                    n += 1;
                    recorded.store(n, SeqCst);
                    std::thread::yield_now();
                }
                n
            })
        };
        while recorded.load(SeqCst) < 5 {
            std::thread::yield_now();
        }
        activity.attach(store, Path::new("/r"));
        stop.store(true, SeqCst);
        let written = writer.join().unwrap();
        let kept: std::collections::HashSet<String> = activity.recent(200).unwrap().into_iter().map(|e| e.path).collect();
        let lost: Vec<String> = (0..written)
            .map(|n| format!("/r/live{n}"))
            .chain((0..100).map(|n| format!("/r/held{n}")))
            .filter(|path| !kept.contains(path))
            .collect();
        assert!(lost.is_empty(), "{} of {} lost: {lost:?}", lost.len(), written + 100);
    }

    /// A `Report` that reports nowhere — the plain
    /// `serve_hydrations`' — starts no walker, and a walker stopped starts
    /// again at the next kick.
    #[tokio::test(start_paused = true)]
    async fn a_report_for_nowhere_starts_no_walker_and_a_stopped_one_starts_again() {
        let nowhere = Report::nowhere();
        nowhere.space.kick();
        assert!(!nowhere.space.running(), "a throwaway report spawned a walker");

        let report = Report::new(in_folder("/r"));
        report.space.kick();
        assert!(report.space.running());
        report.space.stop();
        tokio::task::yield_now().await;
        assert!(!report.space.running(), "stopped");
        report.space.kick();
        assert!(report.space.running(), "and started again when asked");
    }

    /// Measured at once when asked, then at most every five
    /// seconds — asking twice meanwhile makes one more walk, when the time is
    /// up. On the paused clock: no real second passes.
    #[tokio::test(start_paused = true)]
    async fn local_space_is_measured_at_once_then_at_most_every_five_seconds() {
        let state = SyncStateHandle::new(SyncSnapshot { root_path: "/r".into(), ..SyncSnapshot::default() });
        let walks = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&walks);
        // Each walk "measures" how many walks there have been.
        let space = LocalSpace::measuring(state.clone(), Arc::new(move |_: &Path| counted.fetch_add(1, SeqCst) + 1));
        let mut seen = state.subscribe();

        let start = tokio::time::Instant::now();
        space.kick();
        seen.wait_for(|s| s.local_bytes == 1).await.unwrap();
        assert!(start.elapsed() < SPACE_SPACING, "the first walk waits for nothing");

        let asked = tokio::time::Instant::now();
        space.kick();
        space.kick();
        seen.wait_for(|s| s.local_bytes == 2).await.unwrap();
        assert!(asked.elapsed() >= SPACE_SPACING - Duration::from_millis(1), "{:?}", asked.elapsed());
        tokio::time::sleep(SPACE_SPACING * 3).await;
        assert_eq!(walks.load(SeqCst), 2, "two kicks meanwhile make one walk");
    }
}

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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{broadcast, watch, Notify};

use crate::status::snapshot::SyncStateHandle;
use konedrive_tree::{ActivityRow, ConflictRow, Store, TreeError, ACTIVITY_KEPT};

/// One event of the activity log: unix seconds, a [`Kind`]'s
/// name, a full path and a detail. `ActivityLog.Recent` and `ActivityLog.Added` carry
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
    /// Content made or changed here went up to OneDrive (a folder made
    /// here too). Detail: its size, or "folder".
    Uploaded,
    /// Moved or renamed here, and so in OneDrive. Detail: where it was.
    CloudMoved,
    /// Deleted here, and so in OneDrive, to its recycle bin.
    CloudDeleted,
    /// A change made here that cannot go up until the user acts (a name
    /// OneDrive refuses, OneDrive full, a sign-in without write access):
    /// once per change and reason. Detail: the reason.
    UploadFailed,
    /// OneDrive's version was kept, or put back, where both sides changed
    /// one item (`docs/design/writes.md` §7). Detail: why.
    Restored,
    /// Made here, then removed here before its upload finished: it never
    /// goes up, and its rows leave the outbox. Detail: why.
    NotUploaded,
}

impl Kind {
    /// Every kind there is, for whatever has to agree with them all (the
    /// window's guard in `konedrivectl`'s tests).
    pub const ALL: [Kind; 16] = [
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
        Kind::Uploaded,
        Kind::CloudMoved,
        Kind::CloudDeleted,
        Kind::UploadFailed,
        Kind::Restored,
        Kind::NotUploaded,
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
            Kind::Uploaded => "uploaded",
            Kind::CloudMoved => "cloud-moved",
            Kind::CloudDeleted => "cloud-deleted",
            Kind::UploadFailed => "upload-failed",
            Kind::Restored => "restored",
            Kind::NotUploaded => "not-uploaded",
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
/// into `ActivityLog.Added`.
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
/// Conflicts looked over at the end of each cycle ([`Activity::prune`], a
/// guess): the whole list, a batch at a time.
pub const PRUNE_BATCH: usize = 200;

/// Whether the file at `path` is still there, by `lstat`: anything but "not
/// found" says it is.
fn there(path: &str) -> bool {
    !matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

pub struct Activity {
    backing: Mutex<Backing>,
    /// The rescued path the last [`prune`](Self::prune) stopped at.
    pruned_to: Mutex<String>,
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
        Self { backing: Mutex::new(Backing { store: None, memory: VecDeque::new() }), pruned_to: Mutex::new(String::new()), added, state }
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
    /// anything about another folder is dropped — and `Conflicts.Count` is
    /// what it holds. Blocking: the store is SQLite.
    pub fn attach(&self, store: Store, root: &Path) {
        {
            let mut backing = self.backing();
            let held: Vec<Event> =
                backing.memory.drain(..).rev().filter(|event| Path::new(&event.path).starts_with(root)).collect();
            if !held.is_empty() {
                if let Err(e) = store.call_blocking(move |s| s.add_activity(&held)) {
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
                    let stored = kept.clone();
                    if let Err(e) = store.call_blocking(move |s| s.add_activity(&stored)) {
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

    /// Announces `event` (`ActivityLog.Added`) without recording it: the outbox
    /// worker writes its events into the store in the same transaction as
    /// the commit they belong to. Dropped when it is not inside the folder
    /// registered now.
    pub fn announce(&self, event: Event) {
        if inside(&self.state.get().root_path, &event.path) {
            // Nobody listening is not an error.
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
            Some(store) => store.call_blocking(move |s| s.recent_activity(limit)),
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
                let n = rows.len();
                if let Err(e) = store.call_blocking(move |s| s.add_conflicts(&rows)) {
                    tracing::warn!("cannot record {n} conflict(s): {e}");
                }
            }
        }
        self.prune();
    }

    /// The conflicts whose rescued file is still there, newest first; the
    /// rest are dropped (a conflict whose file is gone drops off
    /// by itself). Whether it is there is asked with `lstat`, never an open.
    /// `Conflicts.Count` follows. Read on the store's read-only connection,
    /// and what is gone dropped in one job (issue #39). Blocking.
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        let kept = {
            let backing = self.backing();
            let Some(store) = backing.store.as_ref() else {
                return Ok(Vec::new());
            };
            let rows = store.read_blocking(|s| s.conflicts())?;
            let (kept, gone): (Vec<ConflictRow>, Vec<ConflictRow>) = rows.into_iter().partition(|row| there(&row.rescued));
            if !gone.is_empty() {
                let gone: Vec<String> = gone.into_iter().map(|row| row.rescued).collect();
                store.call_blocking(move |s| s.remove_conflicts(&gone))?;
            }
            kept
        };
        let count = kept.len() as u32;
        self.state.update(|s| s.conflict_count = count);
        Ok(kept)
    }

    /// Looks over the next [`PRUNE_BATCH`] conflicts, in the order of their
    /// rescued paths from where the last look stopped — the whole list, a
    /// batch at a time, round and round (issue #39) — drops those whose file
    /// is gone in one job, and sets `Conflicts.Count` to what is left on
    /// record. Blocking.
    pub fn prune(&self) {
        let counted = {
            let backing = self.backing();
            let Some(store) = backing.store.as_ref() else { return };
            let after = std::mem::take(&mut *self.pruned_to.lock().unwrap_or_else(|p| p.into_inner()));
            let looked = {
                let after = after.clone();
                store.read_blocking(move |s| s.conflicts_after(&after, PRUNE_BATCH))
            };
            let batch = match looked {
                Ok(batch) => batch,
                Err(e) => {
                    tracing::warn!("cannot read the conflicts: {e}");
                    return;
                }
            };
            if batch.len() == PRUNE_BATCH {
                *self.pruned_to.lock().unwrap_or_else(|p| p.into_inner()) = batch[PRUNE_BATCH - 1].rescued.clone();
            }
            let gone: Vec<String> = batch.into_iter().filter(|row| !there(&row.rescued)).map(|row| row.rescued).collect();
            store.call_blocking(move |s| {
                if !gone.is_empty() {
                    s.remove_conflicts(&gone)?;
                }
                s.conflict_count()
            })
        };
        match counted {
            Ok(count) => self.state.update(|s| s.conflict_count = u32::try_from(count).unwrap_or(u32::MAX)),
            Err(e) => tracing::warn!("cannot read the conflicts: {e}"),
        }
    }

    /// Takes the conflict whose rescued file is `rescued` off the list; the
    /// file itself is not touched. Whether there was one. Blocking.
    pub fn dismiss(&self, rescued: &str) -> Result<bool, TreeError> {
        let removed = {
            let backing = self.backing();
            match backing.store.as_ref() {
                Some(store) => {
                    let rescued = rescued.to_owned();
                    store.call_blocking(move |s| s.remove_conflict(&rescued))?
                }
                None => false,
            }
        };
        self.prune();
        Ok(removed)
    }
}

/// One download under way, as `Transfers.Downloads` publishes it: (full path,
/// bytes done, bytes total); and whether it is a file being opened (or
/// `Hydrate`), which `LargeFiles` leaves out (issue #50).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub path: String,
    pub done: u64,
    pub total: u64,
    pub open: bool,
}

/// `Transfers.LargeFiles` (issue #50): the large files ([`LARGE_FROM`](konedrive_graph::pool::LARGE_FROM)
/// and up) the sync moves now, each once however many streams it runs — the downloads of that
/// size but the files being opened, and the uploads of that size.
pub fn large_files(downloads: &BTreeMap<u64, Transfer>, uploads: &[(String, u64, u64)]) -> u32 {
    let large = |total: u64| total >= konedrive_graph::pool::LARGE_FROM;
    let down = downloads.values().filter(|t| !t.open && large(t.total)).count();
    let up = uploads.iter().filter(|(_, _, total)| large(*total)).count();
    u32::try_from(down + up).unwrap_or(u32::MAX)
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
        self.start_as(path, total, false)
    }

    /// As [`start`](Self::start), for a file being opened (or `Hydrate`) when `open`.
    pub fn start_as(&self, path: String, total: u64, open: bool) -> TransferEntry {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.tx.send_modify(|all| {
            all.insert(id, Transfer { path, done: 0, total, open });
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
pub(crate) fn reserved(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(konedrive_graph::drive::item::RESERVED_PREFIX.as_bytes())
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
        let state = SyncStateHandle::new(crate::status::snapshot::SyncSnapshot::default());
        Self {
            activity: Arc::new(Activity::new(state.clone())),
            transfers: Transfers::default(),
            space: Arc::new(LocalSpace::inert(state)),
        }
    }
}

#[cfg(test)]
mod tests;

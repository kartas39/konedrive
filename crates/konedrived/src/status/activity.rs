//! The activity log and the conflicts: what the sync did, and the local
//! versions it moved out of the way.
//!
//! Both live in the folder's tree store (`activity`, `conflicts`), which a
//! Forget drops and a rebuild empties. A folder with no tree store — one
//! filled with `PopulateFromDirectory` — keeps its activity in memory only,
//! and has no conflicts: only a reconcile with OneDrive rescues anything.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::clock::unix_now;
use crate::status::snapshot::SyncStateHandle;
use konedrive_tree::{ActivityRow, ConflictRow, Store, TreeError, ACTIVITY_KEPT};

/// What an event records, by the name the store and the bus spell it with.
pub use konedrive_tree::ActivityKind as Kind;

/// One event of the activity log: unix seconds, a [`Kind`], a full path and a
/// detail. `ActivityLog.Recent` and `ActivityLog.Added` carry exactly these
/// four fields, the kind by its name.
pub type Event = ActivityRow;

/// An incremental cycle logs at most this many events of each kind (spec
/// §16.1), plus one "and N more".
pub const PER_KIND: usize = 50;

/// An event that happens now.
pub fn event(kind: Kind, path: impl Into<String>, detail: impl Into<String>) -> Event {
    Event { at: unix_now(), kind, path: path.into(), detail: detail.into() }
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

/// What a `failed` or `update-failed` event says when the disk is full:
/// exactly this, which the window's notifier tells apart from
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
    let mut counts: Vec<(Kind, usize)> = Vec::new();
    let mut out = Vec::new();
    for event in events {
        let seen = match counts.iter_mut().find(|(kind, _)| *kind == event.kind) {
            Some((_, seen)) => seen,
            None => {
                counts.push((event.kind, 0));
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

/// Conflicts looked over at the end of each cycle ([`Activity::prune`], a
/// guess): the whole list, a batch at a time.
pub const PRUNE_BATCH: usize = 200;

/// Whether the file at `path` is still there, by `lstat`: anything but "not
/// found" says it is.
fn there(path: &str) -> bool {
    !matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// The activity log and the conflicts.
///
/// Backed by the folder's tree store while a OneDrive folder syncs
/// ([`attach`](Self::attach)), and by memory otherwise. Every event recorded
/// is also sent to [`subscribe`](Self::subscribe)rs — `dbus::signals` turns them
/// into `ActivityLog.Added`.
///
/// One lock holds both the store and the memory, and every write holds it for
/// as long as it writes:
///
/// - once [`detach`](Self::detach) returns, no write under way still holds a
///   clone of the store, which is what lets a Forget remove the store's files
///   (`sync/start_stop.rs` detaches before the store goes);
/// - [`attach`](Self::attach) moves what memory holds into the store and
///   hands the store over in the same hold, so nothing recorded meanwhile can
///   fall between the two.
///
/// The lock is therefore held across SQLite calls, and every method that takes
/// it says "Blocking": it is called from a blocking thread. With a store
/// attached that is enforced — `Store::call_blocking` panics on a runtime
/// thread; with memory alone nothing blocks.
///
/// An event is kept only while its path is inside the folder registered now
/// (`SyncSnapshot::folder`'s `root_path`): a download that ends after its folder was
/// forgotten — `Hydrate` does not hold the folder's lock (`SyncService`'s `folder`) — records nothing,
/// in memory or in the next folder's store.
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
        crate::panic::lock(&self.backing)
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
        self.state.update(|s| s.local.conflict_count = 0);
    }

    /// Records `events`, oldest first, and announces each — those inside the
    /// folder registered now; the rest are dropped. Blocking: call it from a
    /// blocking thread, or through [`record`](Self::record).
    pub fn record_blocking(&self, events: Vec<Event>) {
        let kept: Vec<Event> = {
            let mut backing = self.backing();
            let root = self.state.get().folder.root_path;
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
        if inside(&self.state.get().folder.root_path, &event.path) {
            // Nobody listening is not an error.
            let _ = self.added.send(event);
        }
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
    /// and what is gone dropped in one job. Blocking.
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
        self.state.update(|s| s.local.conflict_count = count);
        Ok(kept)
    }

    /// Looks over the next [`PRUNE_BATCH`] conflicts, in the order of their
    /// rescued paths from where the last look stopped — the whole list, a
    /// batch at a time, round and round — drops those whose file
    /// is gone in one job, and sets `Conflicts.Count` to what is left on
    /// record. Blocking.
    pub fn prune(&self) {
        let counted = {
            let backing = self.backing();
            let Some(store) = backing.store.as_ref() else { return };
            let after = std::mem::take(&mut *crate::panic::lock(&self.pruned_to));
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
                *crate::panic::lock(&self.pruned_to) = batch[PRUNE_BATCH - 1].rescued.clone();
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
            Ok(count) => self.state.update(|s| s.local.conflict_count = u32::try_from(count).unwrap_or(u32::MAX)),
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

#[cfg(test)]
mod tests;

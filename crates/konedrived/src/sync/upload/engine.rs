//! The worker's loop: which rows run now, how many at once, and what their
//! outcomes do to the rows and to the worker (throttling, sign-in, pause).

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::local::{self, SYNC_BLOCKED, SYNC_PENDING, SYNC_UPLOADING};
use super::{kind, reason, space, Fault, OutboxCounts, Upload, WorkerConfig, WorkerStatus, BACKOFF_FIRST, BACKOFF_MAX, THROTTLE_FIRST};
use crate::drive::write::MAX_RETRY_AFTER;
use crate::pool::{Class as PoolClass, Size, Slot};
use crate::drive::{DriveError, WriteError};
use crate::sync::disk::Disk;
use crate::tree::outbox::{OutboxKind, OutboxRow, OutboxState, Pick, Picked};
use crate::tree::{ActivityRow, Store, TreeError, TreeStore};

/// The slot of the account's transfer pool a row of `class` takes: content is an upload;
/// metadata, and a move out of the folder (a download, then a delete), go before transfers.
fn pool_class(class: Class) -> PoolClass {
    match class {
        Class::Content => PoolClass::Upload,
        Class::Meta | Class::Out => PoolClass::Metadata,
    }
}

/// Unix seconds now.
pub(super) fn now() -> i64 {
    crate::sync::activity::unix_now()
}

/// The worker looks at the outbox at least this often, woken or not.
const IDLE_CHECK: i64 = 300;

/// A row rewritten and sent again at once more often than this backs off.
const AGAIN_LIMIT: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// `mkdir`, `move`, `delete`: one at a time (§3.5).
    Meta,
    /// `create`, `update`: as many as the account's transfer pool gives, small or large.
    Content,
    /// `move-out`: a download, then a delete. One at a time, beside
    /// the others: its dependencies order it (a folder's removal waits for
    /// what left it first).
    Out,
}

/// How one run of a row ended.
#[derive(Debug)]
pub(super) enum Outcome {
    /// Committed, dropped, or turned into another row by its own
    /// transaction: nothing more to write.
    Done,
    /// Back in line.
    Again { state: OutboxState, reason: Option<String>, next_try: Option<i64>, backoff: bool },
    /// OneDrive asked the whole account to wait (§4.10).
    Throttled(Option<Duration>),
    SignedOut,
    /// `403`: the sign-in does not allow writes.
    Forbidden,
    /// A fault point fired: the row stays `running`, as after a crash.
    Crashed,
    /// OneDrive refused the content for lack of space: the step reads the
    /// quota and turns this into [`Outcome::Space`] (`space`).
    NoSpace,
    /// Ready, in its place, but not taken until a quota read lets it go:
    /// `waiting-for-space` or `too-big:…` (`space`).
    Space(String),
}

impl Outcome {
    /// Ready again at once: the row was rewritten (a fresh guard, a
    /// temporary name, a copy) and runs as it now is.
    pub fn again() -> Self {
        Outcome::Again { state: OutboxState::Ready, reason: None, next_try: None, backoff: false }
    }

    /// Waiting (not quiet): looked at again after `after`.
    pub fn wait(reason: &str, after: Duration) -> Self {
        Outcome::Again { state: OutboxState::Waiting, reason: Some(reason.into()), next_try: Some(now() + after.as_secs() as i64), backoff: false }
    }

    pub fn later(reason: &str, after: Duration) -> Self {
        Outcome::Again { state: OutboxState::Retry, reason: Some(reason.into()), next_try: Some(now() + after.as_secs() as i64), backoff: false }
    }

    /// In backoff: 1 s doubling to an hour with each attempt.
    pub fn backoff(reason: impl Into<String>) -> Self {
        Outcome::Again { state: OutboxState::Retry, reason: Some(reason.into()), next_try: None, backoff: true }
    }

    pub fn blocked(reason: impl Into<String>) -> Self {
        Outcome::Again { state: OutboxState::Blocked, reason: Some(reason.into()), next_try: None, backoff: false }
    }
}

/// Why a step stopped before it could decide on an [`Outcome`] itself.
#[derive(Debug)]
pub(super) enum Fail {
    Write(WriteError),
    Store(TreeError),
    Io(io::Error),
    /// Stop now with this outcome.
    Now(Outcome),
    Crashed,
}

/// How the worker's `last_error` begins while the write gate is closed.
const GATE_CLOSED: &str = "nothing is uploaded: ";

impl From<WriteError> for Fail {
    fn from(e: WriteError) -> Self {
        Fail::Write(e)
    }
}

impl From<DriveError> for Fail {
    fn from(e: DriveError) -> Self {
        Fail::Write(e.into())
    }
}

impl From<TreeError> for Fail {
    fn from(e: TreeError) -> Self {
        Fail::Store(e)
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Io(e)
    }
}

impl From<nix::errno::Errno> for Fail {
    fn from(e: nix::errno::Errno) -> Self {
        Fail::Io(e.into())
    }
}

/// What §3.6's table does with an answer no step settled itself.
pub(super) fn outcome_of(fail: Fail) -> Outcome {
    match fail {
        Fail::Now(outcome) => outcome,
        Fail::Crashed => Outcome::Crashed,
        Fail::Store(e) => Outcome::backoff(e.to_string()),
        Fail::Io(e) => Outcome::backoff(e.to_string()),
        Fail::Write(e) => match e {
            WriteError::QuotaExceeded => Outcome::NoSpace,
            WriteError::Throttled { retry_after } => Outcome::Throttled(retry_after),
            WriteError::Locked => Outcome::backoff(reason::LOCKED),
            WriteError::Forbidden => Outcome::Forbidden,
            WriteError::SignedOut => Outcome::SignedOut,
            WriteError::Refused(message) => Outcome::blocked(format!("{}: {message}", reason::REFUSED)),
            other => Outcome::backoff(other.to_string()),
        },
    }
}

struct InFlight {
    class: Class,
    rel: PathBuf,
    /// The row's reason when it was taken: an `upload-failed` event is
    /// written once per row and reason.
    reason: Option<String>,
    upload: Option<(u64, u64)>,
}

struct Mark {
    rel: PathBuf,
    value: &'static str,
    item: Option<String>,
}

/// The counts are summed again at most this often while the outbox changes
/// (issue #38).
const TALLY_EVERY: Duration = Duration::from_secs(1);

/// Rows a pick looks for: once this many can run, no more portions are read
/// (a guess: more than the transfer pool runs at once).
const PICK_WANT: usize = 32;

pub(super) struct Shared {
    started: bool,
    online: bool,
    throttled_until: Option<i64>,
    throttle_step: Duration,
    needs_sign_in: bool,
    last_error: String,
    in_flight: HashMap<i64, InFlight>,
    crashed: bool,
    /// The `user.konedrive.sync` value last written for each row, where,
    /// and the row's item.
    marks: HashMap<i64, Mark>,
    /// The marks were written for every row once: from then on, only for
    /// the rows that changed.
    marks_read: bool,
    /// What the last pick found in front of the rows that could not run.
    pub(super) waits: Picked,
    /// What the outbox held when the worker last looked.
    pub(super) counts: OutboxCounts,
    /// A delta cycle has gone through since the worker was told to wait for
    /// one (`docs/design/writes.md` §3 and §4.9: the cycle before the outbox).
    cycled: bool,
    /// The network came back: rows in backoff go once the cycle is done.
    network_back: bool,
    /// What is known of the space in OneDrive (issue #2).
    pub(super) space: space::Space,
}

pub(crate) struct Engine {
    pub(super) cfg: WorkerConfig,
    shared: Mutex<Shared>,
    status: watch::Sender<WorkerStatus>,
    wake: Notify,
    faults: Mutex<Vec<Fault>>,
    /// What the pending `move-out` rows name, re-marked on this helper
    /// connection.
    protection: Mutex<super::move_out::Protection>,
    /// One quota read at a time: refusals of rows running together share it.
    pub(super) quota_lock: tokio::sync::Mutex<()>,
    /// The counts are wanted again though the outbox did not change (OneDrive
    /// turned full, or not).
    recount: Notify,
}

/// The `user.konedrive.sync` value for a row's file (§9).
fn wanted_mark(row: &OutboxRow) -> Option<&'static str> {
    if !matches!(row.kind, OutboxKind::Create | OutboxKind::Update | OutboxKind::Move) {
        return None;
    }
    Some(match row.state {
        OutboxState::Blocked => SYNC_BLOCKED,
        OutboxState::Held => return None,
        OutboxState::Running if row.kind.sends_content() => SYNC_UPLOADING,
        _ => SYNC_PENDING,
    })
}

fn backoff_after(attempts: u32) -> i64 {
    let secs = BACKOFF_FIRST.as_secs().saturating_mul(1u64 << attempts.saturating_sub(1).min(20));
    secs.min(BACKOFF_MAX.as_secs()) as i64
}

impl Engine {
    pub(crate) fn new(cfg: WorkerConfig) -> Self {
        let (status, _) = watch::channel(WorkerStatus::default());
        let space = space::Space::default();
        Self {
            cfg,
            shared: Mutex::new(Shared {
                started: false,
                online: true,
                throttled_until: None,
                throttle_step: THROTTLE_FIRST,
                needs_sign_in: false,
                last_error: String::new(),
                in_flight: HashMap::new(),
                crashed: false,
                cycled: true,
                network_back: false,
                marks: HashMap::new(),
                marks_read: false,
                waits: Picked::default(),
                counts: OutboxCounts::default(),
                space,
            }),
            status,
            wake: Notify::new(),
            faults: Mutex::new(Vec::new()),
            protection: Mutex::new(super::move_out::Protection::default()),
            quota_lock: tokio::sync::Mutex::new(()),
            recount: Notify::new(),
        }
    }

    pub(super) fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn protection(&self) -> MutexGuard<'_, super::move_out::Protection> {
        self.protection.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The helper is back without its marks: the `move-out` rows' objects
    /// are marked again at the next look, before any row runs.
    pub(super) fn helper_back(&self) {
        self.protection().helper_back();
        self.wake();
    }

    pub(super) fn store(&self) -> &Store {
        &self.cfg.store
    }

    pub(super) fn fault(&self, fault: Fault) -> Result<(), Fail> {
        let mut armed = self.faults.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = armed.iter().position(|f| *f == fault) {
            armed.remove(i);
            tracing::warn!("fault point {fault:?}: the outbox worker stops here");
            return Err(Fail::Crashed);
        }
        Ok(())
    }

    #[cfg(any(test, feature = "fault-injection"))]
    pub(crate) fn arm(&self, fault: Fault) {
        self.faults.lock().unwrap_or_else(|p| p.into_inner()).push(fault);
    }

    pub(super) fn set_started(&self, started: bool) {
        self.shared().started = started;
        self.publish();
    }

    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }

    /// `Some(until)` while paused (0: until resumed).
    fn paused(&self) -> Option<i64> {
        super::paused(self.store())
    }

    pub(super) fn pause(&self, for_: Option<Duration>) -> Result<(), TreeError> {
        let until = for_.map(|d| now() + d.as_secs().max(1) as i64).unwrap_or(0);
        super::set_paused_blocking(self.store(), Some(until))?;
        self.publish();
        self.wake();
        Ok(())
    }

    pub(super) fn resume(&self) -> Result<(), TreeError> {
        super::set_paused_blocking(self.store(), None)?;
        self.publish();
        self.wake();
        Ok(())
    }

    pub(crate) async fn set_online(&self, online: bool) {
        self.shared().online = online;
        if online {
            // Rows that backed off on network errors the host never reported
            // go now, not up to an hour later.
            if let Err(e) = self.store().call(move |s| s.outbox_retry_now()).await {
                tracing::warn!("cannot make the outbox's waiting rows due: {e}");
            }
        }
        self.publish();
        self.wake();
    }

    /// Nothing is sent until the next [`cycle_done`](Self::cycle_done): the
    /// folder's first cycle, and the one after the network came back, run
    /// before the outbox (`docs/design/writes.md` §3, §9).
    pub(super) fn wait_for_cycle(&self, network_back: bool) {
        {
            let mut shared = self.shared();
            shared.cycled = false;
            shared.network_back |= network_back;
        }
        self.publish();
    }

    /// A delta cycle went through: the base caught up. Rows in backoff go
    /// now after the first cycle and after the network came back.
    pub(crate) async fn cycle_done(&self) {
        let due = {
            let mut shared = self.shared();
            let first = !shared.cycled;
            shared.cycled = true;
            first || std::mem::take(&mut shared.network_back)
        };
        if due {
            if let Err(e) = self.store().call(move |s| s.outbox_retry_now()).await {
                tracing::warn!("cannot make the outbox's waiting rows due: {e}");
            }
            self.publish();
            self.wake();
        }
    }

    pub(crate) async fn signed_in(&self) -> Result<(), TreeError> {
        {
            let mut shared = self.shared();
            shared.needs_sign_in = false;
            shared.last_error.clear();
        }
        self.store().call(move |s| s.outbox_unblock(&[reason::FORBIDDEN])).await?;
        self.publish();
        self.wake();
        Ok(())
    }

    pub(crate) async fn retry_now(&self) -> Result<(), TreeError> {
        self.store().call(move |s| s.outbox_retry_now()).await?;
        self.wake();
        Ok(())
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<WorkerStatus> {
        self.status.subscribe()
    }

    pub(super) fn status(&self) -> WorkerStatus {
        let paused = self.paused();
        let now = now();
        let shared = self.shared();
        let mut uploads: Vec<(i64, Upload)> = shared
            .in_flight
            .iter()
            .filter_map(|(&seq, f)| f.upload.map(|(sent, total)| (seq, Upload { rel: f.rel.clone(), sent, total })))
            .collect();
        uploads.sort_by_key(|(seq, _)| *seq);
        WorkerStatus {
            started: shared.started,
            paused: paused.is_some(),
            paused_until: paused.unwrap_or(0),
            throttled_until: shared.throttled_until.filter(|&at| at > now),
            online: shared.online,
            needs_sign_in: shared.needs_sign_in,
            last_error: shared.last_error.clone(),
            running: shared.in_flight.len(),
            uploads: uploads.into_iter().map(|(_, u)| u).collect(),
            counts: shared.counts,
            quota_full: shared.space.full,
            free_space: shared.space.free,
            quota_state: shared.space.state(),
        }
    }

    /// Publishes the status, and hands it to the host when it changed.
    pub(super) fn publish(&self) {
        let status = self.status();
        let changed = self.status.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status.clone();
                true
            }
        });
        if changed {
            self.cfg.host.status(&status);
        }
    }

    pub(super) fn upload_progress(&self, seq: i64, sent: u64, total: u64) {
        if let Some(flight) = self.shared().in_flight.get_mut(&seq) {
            flight.upload = Some((sent, total));
        }
        self.publish();
    }

    /// Writes an activity event outside a commit, and hands it to the host.
    pub(super) fn activity(&self, event: ActivityRow) {
        let stored = event.clone();
        if let Err(e) = self.store().call_blocking(move |s| s.add_activity(std::slice::from_ref(&stored))) {
            tracing::warn!("cannot record an activity event: {e}");
        }
        self.cfg.host.activity(&event);
    }

    pub(super) fn event(&self, kind: &str, rel: &std::path::Path, detail: impl Into<String>) -> ActivityRow {
        ActivityRow { at: now(), kind: kind.into(), path: self.cfg.root.path.join(rel).display().to_string(), detail: detail.into() }
    }

    fn may_start(&self) -> bool {
        let paused = self.paused().is_some();
        let now = now();
        let ready = {
            let shared = self.shared();
            !paused
                && !shared.crashed
                && shared.cycled
                && shared.online
                && !shared.needs_sign_in
                && shared.throttled_until.is_none_or(|at| at <= now)
        };
        ready && self.gate_open()
    }

    /// The write gate, asked again before every row (`docs/design/writes.md` §2.3): the
    /// host says whether the account may change OneDrive now. Closed, nothing more is taken,
    /// the rows wait, and the worker's `last_error` says why.
    fn gate_open(&self) -> bool {
        match self.cfg.host.may_write() {
            Ok(()) => {
                let mut shared = self.shared();
                if shared.last_error.starts_with(GATE_CLOSED) {
                    shared.last_error.clear();
                }
                true
            }
            Err(why) => {
                let message = format!("{GATE_CLOSED}{why}");
                let mut shared = self.shared();
                if shared.last_error != message {
                    tracing::warn!("{message}");
                    shared.last_error = message;
                }
                false
            }
        }
    }

    /// Whether a row of `class` may start beside those running: metadata and move-outs
    /// one at a time; content as the pool allows.
    fn slot_free(&self, class: Class) -> bool {
        let shared = self.shared();
        let busy = shared.in_flight.values().filter(|f| f.class == class).count();
        match class {
            Class::Meta | Class::Out => busy < 1,
            Class::Content => true,
        }
    }

    fn class_of(&self, row: &OutboxRow) -> Class {
        if row.kind == OutboxKind::MoveOut {
            return Class::Out;
        }
        if !row.kind.sends_content() {
            return Class::Meta;
        }
        Class::Content
    }

    /// The rows that may run now, in `seq` order: due, waiting for no other
    /// row, not held here already, and as the space allows; move-outs only
    /// with what they need. Read a portion at a time ([`TreeStore::outbox_pick`]),
    /// off the async runtime. What stands in front of the rest is kept
    /// (`Shared::waits`), and a time it names wakes the worker.
    ///
    /// [`TreeStore::outbox_pick`]: crate::tree::TreeStore::outbox_pick
    pub(crate) async fn candidates(&self) -> Result<Vec<(OutboxRow, Class)>, TreeError> {
        let now = now();
        let flying: HashSet<i64> = self.shared().in_flight.keys().copied().collect();
        let move_outs = self.cfg.moved_out.is_some();
        let (full, looked) = self.space_seen();
        let picked = self
            .store()
            .call(move |s| {
                let allows = |s: &TreeStore, r: &OutboxRow| space::allows(r, full, &looked, || s.outbox_removed_behind(r));
                s.outbox_pick(&Pick { now, flying: &flying, move_outs, want: PICK_WANT, allows: &allows })
            })
            .await?;
        if !picked.stalled.is_empty() {
            tracing::error!("{} outbox row(s) can run and wait for nothing that runs, has a time or needs the user: {:?}", picked.stalled.len(), picked.stalled);
        }
        let rows: Vec<(OutboxRow, Class)> = picked.rows.iter().cloned().map(|r| {
            let class = self.class_of(&r);
            (r, class)
        }).collect();
        self.shared().waits = Picked { rows: Vec::new(), ..picked };
        Ok(rows)
    }

    /// Sets `user.konedrive.sync` on the files of rows whose state changed,
    /// and takes it off those whose row went: the rows written or removed
    /// since the last look ([`OutboxChanges`]), every row the first time.
    /// The attributes are written with no lock held. The counts follow.
    ///
    /// [`OutboxChanges`]: crate::tree::outbox::OutboxChanges
    fn mark_rows(&self, disk: &Disk) {
        let store = self.store();
        let first = !self.shared().marks_read;
        let dirty = store.changes().take_dirty();
        let (read, asked): (Result<Vec<OutboxRow>, TreeError>, Option<HashSet<i64>>) = match dirty.filter(|_| !first) {
            Some(seqs) if seqs.is_empty() => (Ok(Vec::new()), Some(seqs)),
            Some(seqs) => {
                let list: Vec<i64> = seqs.iter().copied().collect();
                (store.call_blocking(move |s| s.outbox_rows_of(&list)), Some(seqs))
            }
            None => (store.call_blocking(move |s| s.outbox_rows()), None),
        };
        let mut rows = match read {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("cannot read the outbox for its marks: {e}");
                // Every row, next time.
                self.shared().marks_read = false;
                return;
            }
        };
        let present: HashSet<i64> = rows.iter().map(|r| r.seq).collect();
        let gone: Vec<Mark> = {
            let mut shared = self.shared();
            shared.marks_read = true;
            let gone: Vec<i64> = match &asked {
                Some(seqs) => seqs.iter().filter(|seq| !present.contains(seq)).copied().collect(),
                None => shared.marks.keys().filter(|seq| !present.contains(seq)).copied().collect(),
            };
            gone.into_iter().filter_map(|seq| shared.marks.remove(&seq)).collect()
        };
        let mut cleared = HashSet::new();
        for mark in gone {
            local::mark(disk, &mark.rel, None);
            cleared.insert(mark.rel);
            // A move taken back (the file went back to its base place): the
            // mark is on the file there.
            if let Some(id) = mark.item {
                if let Ok(Some(at)) = store.call_blocking(move |s| s.locate(crate::tree::Table::Items, &id)) {
                    if !at.rel.as_os_str().is_empty() {
                        local::mark(disk, &at.rel, None);
                        cleared.insert(at.rel);
                    }
                }
            }
        }
        // A row behind the one that went writes its mark again.
        if !cleared.is_empty() {
            let again: Vec<i64> = {
                let mut shared = self.shared();
                let again: Vec<i64> = shared.marks.iter().filter(|(_, m)| cleared.contains(&m.rel)).map(|(&seq, _)| seq).collect();
                for seq in &again {
                    shared.marks.remove(seq);
                }
                again.into_iter().filter(|seq| !present.contains(seq)).collect()
            };
            if !again.is_empty() {
                rows.extend(store.call_blocking(move |s| s.outbox_rows_of(&again)).unwrap_or_default());
            }
        }
        let wanted: Vec<(PathBuf, &'static str)> = {
            let mut shared = self.shared();
            rows.iter()
                .filter_map(|row| {
                    let value = wanted_mark(row)?;
                    if shared.marks.get(&row.seq).is_some_and(|m| m.rel == row.rel && m.value == value) {
                        return None;
                    }
                    shared.marks.insert(row.seq, Mark { rel: row.rel.clone(), value, item: row.item_id.clone() });
                    Some((row.rel.clone(), value))
                })
                .collect()
        };
        for (rel, value) in wanted {
            local::mark(disk, &rel, Some(value));
        }
    }

    /// `PendingCount` and the rest, and the Not Uploaded summary, summed by
    /// SQL through the store's read-only connection — never waiting for a
    /// writer — and kept in memory for the bus.
    pub(super) async fn recount(&self) {
        let full = self.space_full();
        match self.store().read(|s| Ok((s.outbox_groups()?, s.skipped_groups()?))).await {
            Ok((groups, skipped)) => {
                self.shared().counts = OutboxCounts::of(&groups, full);
                self.cfg.host.kept_back(&crate::sync::kept_back::summary(&skipped, &groups, full));
                self.publish();
            }
            Err(e) => tracing::warn!("cannot count the outbox: {e}"),
        }
    }

    /// The counts wanted again though the outbox did not change.
    pub(super) fn recount_soon(&self) {
        self.recount.notify_one();
    }

    /// Sums the outbox again whenever a change to it is committed, at most
    /// every [`TALLY_EVERY`].
    async fn tally(self: Arc<Self>, cancel: CancellationToken) {
        let mut changes = self.store().changes().subscribe();
        loop {
            self.recount().await;
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(TALLY_EVERY) => {}
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                changed = changes.changed() => if changed.is_err() { return },
                _ = self.recount.notified() => {}
            }
        }
    }

    /// [`mark_rows`](Self::mark_rows) off the async runtime.
    async fn mark_rows_blocking(self: &Arc<Self>, disk: &Arc<Disk>) {
        let (engine, disk) = (Arc::clone(self), Arc::clone(disk));
        if let Err(e) = tokio::task::spawn_blocking(move || engine.mark_rows(&disk)).await {
            tracing::warn!("the outbox's marks task failed: {e}");
        }
    }

    /// Runs rows until none can run now (or `cancel`): what [`run`] does each
    /// time it is woken, and what tests call directly. A fault point stops
    /// it as a crash would, leaving the row `running`: this worker takes
    /// nothing more until it is built again.
    ///
    /// [`run`]: Engine::run
    pub(crate) async fn drain(self: &Arc<Self>, cancel: &CancellationToken) {
        let disk = match Disk::open(&self.cfg.root, false) {
            Ok(disk) => Arc::new(disk),
            Err(e) => {
                self.shared().last_error = format!("the OneDrive folder cannot be opened: {e}");
                self.publish();
                return;
            }
        };
        self.space_start().await;
        // The quota, read again when it is due (while full, while a file is
        // too big, once after a start that found waiting rows).
        if self.may_start() {
            self.space_check(now()).await;
        }
        let mut set: JoinSet<(i64, Outcome)> = JoinSet::new();
        let mut tasks: HashMap<tokio::task::Id, i64> = HashMap::new();
        let pool = Arc::clone(self.cfg.drive.pool());
        // Slots of the account's transfer pool that came while the loop waited, each for the
        // next row of its class and size; one that no row takes goes back at once.
        let mut spare: Vec<Slot> = Vec::new();
        loop {
            // The pool classes and sizes whose rows waited for a slot this time round: a
            // large file waiting for the large-file limit does not hold up the small ones.
            let mut wanting: Vec<(PoolClass, Size)> = Vec::new();
            // What left the folder is marked again first, whether or not rows
            // may run now, and at every wake while rows are in flight — a new
            // move out, the helper back (`docs/design/writes.md` §8).
            self.protect(&disk).await;
            self.mark_rows_blocking(&disk).await;
            if self.may_start() {
                match self.candidates().await {
                    Ok(rows) => {
                        for (row, class) in rows {
                            if !self.slot_free(class) {
                                continue;
                            }
                            // The local file's size says whether its upload is large.
                            let size = match class {
                                Class::Content => Size::of(local::size_at(&disk, &row.rel).unwrap_or(0)),
                                Class::Meta | Class::Out => Size::Small,
                            };
                            let wants = (pool_class(class), size);
                            if wanting.contains(&wants) {
                                continue;
                            }
                            // Asked again right before each row is taken.
                            if !self.gate_open() {
                                break;
                            }
                            let slot = match spare.iter().position(|slot| (slot.class(), slot.size()) == wants) {
                                Some(at) => spare.swap_remove(at),
                                None => match pool.try_acquire_sized(wants.0, wants.1) {
                                    Some(slot) => slot,
                                    None => {
                                        wanting.push(wants);
                                        continue;
                                    }
                                },
                            };
                            let (seq, state) = (row.seq, row.state);
                            let claimed = match self.store().call(move |s| s.outbox_claim(seq, state)).await {
                                Ok(Some(claimed)) => claimed,
                                Ok(None) => {
                                    spare.push(slot);
                                    continue;
                                }
                                Err(e) => {
                                    tracing::warn!("cannot take outbox row {}: {e}", row.seq);
                                    spare.push(slot);
                                    continue;
                                }
                            };
                            let seq = claimed.seq;
                            self.shared().in_flight.insert(seq, InFlight { class, rel: claimed.rel.clone(), reason: row.reason.clone(), upload: None });
                            let engine = Arc::clone(self);
                            let disk = Arc::clone(&disk);
                            let handle = set.spawn(async move {
                                // The row holds its slot for all it sends, every fragment.
                                let mut slot = slot;
                                let outcome = super::steps::run(&engine, &disk, claimed).await;
                                if matches!(outcome, Outcome::Done) {
                                    slot.succeeded();
                                }
                                drop(slot);
                                (seq, outcome)
                            });
                            tasks.insert(handle.id(), seq);
                        }
                    }
                    Err(e) => tracing::warn!("cannot read the outbox: {e}"),
                }
                self.publish();
            }
            spare.clear();
            if set.is_empty() && wanting.is_empty() {
                break;
            }
            // Whichever slot comes first; the others' waits are dropped, a slot granted
            // meanwhile going back.
            let waited = async {
                if wanting.is_empty() {
                    return std::future::pending().await;
                }
                let waits = wanting.iter().map(|&(class, size)| pool.acquire_sized(class, size));
                futures_util::future::select_all(waits).await.0
            };
            tokio::select! {
                // A slot for a row that waited for one: taken at the top of the loop.
                slot = waited => spare.push(slot),
                // New rows, or the helper back: looked at while the others run.
                _ = self.wake.notified() => {}
                _ = cancel.cancelled() => {
                    set.shutdown().await;
                    self.shared().in_flight.clear();
                    break;
                }
                joined = set.join_next_with_id(), if !set.is_empty() => match joined {
                    Some(Ok((id, (seq, outcome)))) => {
                        tasks.remove(&id);
                        self.settle_blocking(seq, outcome).await;
                    }
                    Some(Err(e)) => {
                        if let Some(seq) = tasks.remove(&e.id()) {
                            // Replayed later, in backoff, never at once.
                            tracing::error!("outbox row {seq} failed: {e}; it is tried again later");
                            self.settle_blocking(seq, Outcome::backoff("the step failed")).await;
                        }
                    }
                    None => {}
                }
            }
        }
        self.mark_rows_blocking(&disk).await;
        self.recount().await;
        self.publish();
    }

    /// [`settle`](Self::settle) off the async runtime.
    async fn settle_blocking(self: &Arc<Self>, seq: i64, outcome: Outcome) {
        let engine = Arc::clone(self);
        if let Err(e) = tokio::task::spawn_blocking(move || engine.settle(seq, outcome)).await {
            tracing::warn!("settling outbox row {seq} failed: {e}");
        }
    }

    /// What `outcome` does to row `seq` and to the worker.
    fn settle(&self, seq: i64, outcome: Outcome) {
        let flight = self.shared().in_flight.remove(&seq);
        let rel = flight.as_ref().map(|f| f.rel.clone()).unwrap_or_default();
        let before = flight.and_then(|f| f.reason);
        let now = now();
        let store = self.store();
        if !matches!(outcome, Outcome::Done) {
            // Its state changes: the mark is written again.
            self.shared().marks.remove(&seq);
        }
        let result: Result<(), TreeError> = match outcome {
            Outcome::Done => {
                self.shared().throttle_step = THROTTLE_FIRST;
                Ok(())
            }
            Outcome::Again { state, reason, next_try, backoff } => (|| {
                let (mut state, mut reason, mut next_try) = (state, reason, next_try);
                if backoff {
                    next_try = Some(now + backoff_after(store.call_blocking(move |s| s.outbox_count_attempt(seq))?));
                } else if state == OutboxState::Ready {
                    // Rewritten and ready at once (a temporary name, a copy,
                    // a fresh guard): never more than a few times in a row,
                    // unless OneDrive keeps changing under it — then it backs
                    // off like a failure.
                    let attempts = store.call_blocking(move |s| s.outbox_count_attempt(seq))?;
                    if attempts > AGAIN_LIMIT {
                        (state, reason, next_try) = (OutboxState::Retry, Some("changing in OneDrive again and again".into()), Some(now + backoff_after(attempts)));
                    }
                }
                let written = reason.clone();
                store.call_blocking(move |s| s.outbox_set_state(seq, state, written.as_deref(), next_try))?;
                if state == OutboxState::Blocked && reason != before {
                    self.activity(self.event(kind::UPLOAD_FAILED, &rel, reason.unwrap_or_default()));
                }
                Ok(())
            })(),
            Outcome::Throttled(wait) => {
                {
                    let mut shared = self.shared();
                    let wait = match wait {
                        Some(wait) => wait.min(MAX_RETRY_AFTER),
                        None => {
                            let wait = shared.throttle_step;
                            shared.throttle_step = (wait * 2).min(BACKOFF_MAX);
                            wait
                        }
                    };
                    let until = now + wait.as_secs().max(1) as i64;
                    shared.throttled_until = Some(until);
                    shared.last_error = format!("OneDrive asked to wait {} s before sending more", until - now);
                }
                store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Ready, None, None))
            }
            Outcome::SignedOut => {
                {
                    let mut shared = self.shared();
                    shared.needs_sign_in = true;
                    shared.last_error = "signed out: sign in again to upload changes".into();
                }
                store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Ready, None, None))
            }
            Outcome::Forbidden => {
                {
                    let mut shared = self.shared();
                    shared.needs_sign_in = true;
                    shared.last_error = "OneDrive does not allow changes with this sign-in: sign in again".into();
                }
                let set = store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Blocked, Some(reason::FORBIDDEN), None));
                if before.as_deref() != Some(reason::FORBIDDEN) {
                    self.activity(self.event(kind::UPLOAD_FAILED, &rel, reason::FORBIDDEN));
                }
                set
            }
            Outcome::Crashed => {
                self.shared().crashed = true;
                Ok(())
            }
            // In its place, with no timer: a quota read lets it go. No event
            // per file: the account's `QuotaFull` says it once.
            Outcome::Space(why) => store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Ready, Some(&why), None)),
            Outcome::NoSpace => store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Ready, Some(space::WAITING), None)),
        };
        if let Err(e) = result {
            tracing::warn!("cannot settle outbox row {seq}: {e}");
        }
        self.publish();
    }

    /// The start's look at the space (`space::Space::start`), once, before
    /// the first drain: off the constructor, which may run on the runtime.
    pub(crate) async fn space_start(&self) {
        if self.shared().space.started {
            return;
        }
        // What a quota read found meanwhile stays.
        let start = space::Space::start(self.store()).await;
        let mut shared = self.shared();
        shared.space.full |= start.full;
        shared.space.wanted |= start.wanted;
        shared.space.started = true;
    }

    /// When something may become runnable without a wake: a backoff, a
    /// throttle or a timed pause running out.
    async fn next_due(&self) -> Duration {
        let now = now();
        let mut at = now + IDLE_CHECK;
        if let Some(until) = self.shared().throttled_until.filter(|&u| u > now) {
            at = at.min(until);
        }
        if let Some(until) = self.paused().filter(|&u| u > 0) {
            at = at.min(until);
        }
        if let Some(check) = self.space_check_at() {
            at = at.min(check);
        }
        // While nothing can start, only the end of a pause or throttle
        // matters; rows already due wait for a wake.
        if self.may_start() {
            if let Ok(Some(next)) = self.store().call(move |s| s.outbox_next_due(now)).await {
                at = at.min(next);
            }
            if let Some(until) = self.shared().waits.until.filter(|&u| u > now) {
                at = at.min(until);
            }
        }
        Duration::from_secs((at - now).max(1) as u64)
    }

    /// The worker's life: drain, then sleep until woken or something falls
    /// due.
    pub(super) async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let tally = tokio::spawn(Arc::clone(&self).tally(cancel.clone()));
        loop {
            self.drain(&cancel).await;
            if cancel.is_cancelled() {
                break;
            }
            let wait = self.next_due().await;
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
        let _ = tally.await;
    }
}

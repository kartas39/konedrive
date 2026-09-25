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
use super::{kind, reason, Fault, OutboxCounts, Upload, WorkerConfig, WorkerStatus, BACKOFF_FIRST, BACKOFF_MAX, QUOTA_RETRY, THROTTLE_FIRST};
use crate::drive::write::MAX_RETRY_AFTER;
use crate::drive::{DriveError, WriteError};
use crate::sync::disk::Disk;
use crate::tree::outbox::{OutboxKind, OutboxRow, OutboxState};
use crate::tree::{ActivityRow, Store, TreeError};

/// Unix seconds now.
pub(super) fn now() -> i64 {
    crate::sync::activity::unix_now()
}

/// The worker looks at the outbox at least this often, woken or not.
const IDLE_CHECK: i64 = 300;

/// A row rewritten and sent again at once more often than this backs off.
const AGAIN_LIMIT: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Class {
    /// `mkdir`, `move`, `delete`: one at a time (§3.5).
    Meta,
    Small,
    Large,
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
            WriteError::QuotaExceeded => Outcome::Again {
                state: OutboxState::Blocked,
                reason: Some(reason::QUOTA.into()),
                next_try: Some(now() + QUOTA_RETRY.as_secs() as i64),
                backoff: false,
            },
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
    /// The file's size when the mark was written: what `PendingBytes`
    /// counts until the worker takes the row (its snapshot says then).
    size: u64,
}

struct Shared {
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
    /// What the outbox held when the worker last looked.
    counts: OutboxCounts,
    /// A delta cycle has gone through since the worker was told to wait for
    /// one (`docs/design/writes.md` §3 and §4.9: the cycle before the outbox).
    cycled: bool,
    /// The network came back: rows in backoff go once the cycle is done.
    network_back: bool,
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
}

fn due(row: &OutboxRow, now: i64) -> bool {
    match row.state {
        // A `running` row nobody holds: a crash or a stop left it; replayed.
        OutboxState::Ready | OutboxState::Running => true,
        OutboxState::Retry => row.next_try.is_none_or(|at| at <= now),
        OutboxState::Waiting => row.next_try.is_some_and(|at| at <= now),
        OutboxState::Blocked => row.reason.as_deref() == Some(reason::QUOTA) && row.next_try.is_some_and(|at| at <= now),
        OutboxState::Held => false,
    }
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
                counts: OutboxCounts::default(),
            }),
            status,
            wake: Notify::new(),
            faults: Mutex::new(Vec::new()),
            protection: Mutex::new(super::move_out::Protection::default()),
        }
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
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
        super::set_paused(self.store(), Some(until))?;
        self.publish();
        self.wake();
        Ok(())
    }

    pub(super) fn resume(&self) -> Result<(), TreeError> {
        super::set_paused(self.store(), None)?;
        self.publish();
        self.wake();
        Ok(())
    }

    pub(super) fn set_online(&self, online: bool) {
        self.shared().online = online;
        if online {
            // Rows that backed off on network errors the host never reported
            // go now, not up to an hour later.
            if let Err(e) = self.store().with(|s| s.outbox_retry_now()) {
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
    pub(super) fn cycle_done(&self) {
        let due = {
            let mut shared = self.shared();
            let first = !shared.cycled;
            shared.cycled = true;
            first || std::mem::take(&mut shared.network_back)
        };
        if due {
            if let Err(e) = self.store().with(|s| s.outbox_retry_now()) {
                tracing::warn!("cannot make the outbox's waiting rows due: {e}");
            }
            self.publish();
            self.wake();
        }
    }

    pub(super) fn signed_in(&self) -> Result<(), TreeError> {
        {
            let mut shared = self.shared();
            shared.needs_sign_in = false;
            shared.last_error.clear();
        }
        self.store().with(|s| s.outbox_unblock(&[reason::FORBIDDEN]))?;
        self.publish();
        self.wake();
        Ok(())
    }

    pub(super) fn quota_changed(&self) -> Result<(), TreeError> {
        self.store().with(|s| s.outbox_unblock(&[reason::QUOTA]))?;
        self.wake();
        Ok(())
    }

    pub(super) fn retry_now(&self) -> Result<(), TreeError> {
        self.store().with(|s| s.outbox_retry_now())?;
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
        }
    }

    /// Publishes the status, and hands it to the host when it changed.
    fn publish(&self) {
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

    pub(super) fn counts(&self) -> Result<OutboxCounts, TreeError> {
        let rows = self.store().with(|s| s.outbox_rows())?;
        let disk = Disk::open(&self.cfg.root, false).ok();
        let mut counts = OutboxCounts::default();
        for row in rows {
            match row.state {
                OutboxState::Blocked => counts.blocked += 1,
                OutboxState::Held => counts.held += 1,
                _ => {
                    counts.pending += 1;
                    if row.kind.sends_content() {
                        counts.pending_bytes += disk.as_ref().and_then(|d| local::size_at(d, &row.rel)).unwrap_or(0);
                    }
                }
            }
        }
        Ok(counts)
    }

    pub(super) fn upload_progress(&self, seq: i64, sent: u64, total: u64) {
        if let Some(flight) = self.shared().in_flight.get_mut(&seq) {
            flight.upload = Some((sent, total));
        }
        self.publish();
    }

    /// Writes an activity event outside a commit, and hands it to the host.
    pub(super) fn activity(&self, event: ActivityRow) {
        if let Err(e) = self.store().with(|s| s.add_activity(std::slice::from_ref(&event))) {
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

    fn slot_free(&self, class: Class) -> bool {
        let shared = self.shared();
        let busy = shared.in_flight.values().filter(|f| f.class == class).count();
        busy < match class {
            Class::Meta => 1,
            Class::Small => self.cfg.limits.small_slots,
            Class::Large => self.cfg.limits.large_slots,
            Class::Out => 1,
        }
    }

    fn class_of(&self, disk: &Disk, row: &OutboxRow) -> Class {
        if row.kind == OutboxKind::MoveOut {
            return Class::Out;
        }
        if !row.kind.sends_content() {
            return Class::Meta;
        }
        match local::size_at(disk, &row.rel) {
            Some(size) if size > self.cfg.limits.small_max => Class::Large,
            _ => Class::Small,
        }
    }

    /// The rows that may run now, in `seq` order: due, waiting for no other
    /// row, not held here already. Move-outs only with what they need.
    fn candidates(&self, disk: &Disk) -> Result<Vec<(OutboxRow, Class)>, TreeError> {
        let now = now();
        let (rows, deps) = self.store().with(|s| Ok((s.outbox_rows()?, s.outbox_dependencies()?)))?;
        let flying: HashSet<i64> = self.shared().in_flight.keys().copied().collect();
        let move_outs = self.cfg.moved_out.is_some();
        Ok(rows
            .into_iter()
            .filter(|r| !flying.contains(&r.seq) && (r.kind != OutboxKind::MoveOut || move_outs) && due(r, now))
            .filter(|r| deps.get(&r.seq).is_none_or(Vec::is_empty))
            .map(|r| {
                let class = self.class_of(disk, &r);
                (r, class)
            })
            .collect())
    }

    /// Sets `user.konedrive.sync` on the files of rows whose state changed,
    /// and takes it off those whose row went.
    fn mark_rows(&self, disk: &Disk) {
        let Ok(rows) = self.store().with(|s| s.outbox_rows()) else { return };
        let live: HashSet<i64> = rows.iter().map(|r| r.seq).collect();
        let mut shared = self.shared();
        let gone: Vec<i64> = shared.marks.keys().filter(|seq| !live.contains(seq)).copied().collect();
        let mut cleared = HashSet::new();
        for seq in gone {
            let Some(mark) = shared.marks.remove(&seq) else { continue };
            local::mark(disk, &mark.rel, None);
            cleared.insert(mark.rel);
            // A move taken back (the file went back to its base place): the
            // mark is on the file there.
            if let Some(id) = mark.item {
                if let Ok(Some(at)) = self.store().with(|s| s.locate(crate::tree::Table::Items, &id)) {
                    if !at.rel.as_os_str().is_empty() {
                        local::mark(disk, &at.rel, None);
                        cleared.insert(at.rel);
                    }
                }
            }
        }
        // A row behind the one that went writes its mark again.
        shared.marks.retain(|_, mark| !cleared.contains(&mark.rel));
        let mut counts = OutboxCounts::default();
        for row in &rows {
            if let Some(value) = wanted_mark(row) {
                if !shared.marks.get(&row.seq).is_some_and(|m| m.rel == row.rel && m.value == value) {
                    local::mark(disk, &row.rel, Some(value));
                    let size = if row.kind.sends_content() { local::size_at(disk, &row.rel).unwrap_or(0) } else { 0 };
                    shared.marks.insert(row.seq, Mark { rel: row.rel.clone(), value, item: row.item_id.clone(), size });
                }
            }
            match row.state {
                OutboxState::Blocked => counts.blocked += 1,
                OutboxState::Held => counts.held += 1,
                _ => {
                    counts.pending += 1;
                    if row.kind.sends_content() {
                        let sent = row.snapshot.as_deref().and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok());
                        counts.pending_bytes += sent.or_else(|| shared.marks.get(&row.seq).map(|m| m.size)).unwrap_or(0);
                    }
                }
            }
        }
        shared.counts = counts;
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
        let mut set: JoinSet<(i64, Outcome)> = JoinSet::new();
        let mut tasks: HashMap<tokio::task::Id, i64> = HashMap::new();
        loop {
            // What left the folder is marked again first, whether or not rows
            // may run now, and at every wake while rows are in flight — a new
            // move out, the helper back (`docs/design/writes.md` §8).
            self.protect(&disk).await;
            self.mark_rows(&disk);
            if self.may_start() {
                match self.candidates(&disk) {
                    Ok(rows) => {
                        for (row, class) in rows {
                            if !self.slot_free(class) {
                                continue;
                            }
                            // Asked again right before each row is taken.
                            if !self.gate_open() {
                                break;
                            }
                            let claimed = match self.store().with(|s| s.outbox_claim(row.seq, row.state)) {
                                Ok(Some(claimed)) => claimed,
                                Ok(None) => continue,
                                Err(e) => {
                                    tracing::warn!("cannot take outbox row {}: {e}", row.seq);
                                    continue;
                                }
                            };
                            let seq = claimed.seq;
                            self.shared().in_flight.insert(seq, InFlight { class, rel: claimed.rel.clone(), reason: row.reason.clone(), upload: None });
                            let engine = Arc::clone(self);
                            let disk = Arc::clone(&disk);
                            let handle = set.spawn(async move {
                                let outcome = super::steps::run(&engine, &disk, claimed).await;
                                (seq, outcome)
                            });
                            tasks.insert(handle.id(), seq);
                        }
                    }
                    Err(e) => tracing::warn!("cannot read the outbox: {e}"),
                }
                self.publish();
            }
            if set.is_empty() {
                break;
            }
            tokio::select! {
                // New rows, or the helper back: looked at while the others run.
                _ = self.wake.notified() => {}
                _ = cancel.cancelled() => {
                    set.shutdown().await;
                    self.shared().in_flight.clear();
                    break;
                }
                joined = set.join_next_with_id() => match joined {
                    Some(Ok((id, (seq, outcome)))) => {
                        tasks.remove(&id);
                        self.settle(seq, outcome);
                    }
                    Some(Err(e)) => {
                        if let Some(seq) = tasks.remove(&e.id()) {
                            // Replayed later, in backoff, never at once.
                            tracing::error!("outbox row {seq} failed: {e}; it is tried again later");
                            self.settle(seq, Outcome::backoff("the step failed"));
                        }
                    }
                    None => {}
                }
            }
        }
        self.mark_rows(&disk);
        self.publish();
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
                    next_try = Some(now + backoff_after(store.with(|s| s.outbox_count_attempt(seq))?));
                } else if state == OutboxState::Ready {
                    // Rewritten and ready at once (a temporary name, a copy,
                    // a fresh guard): never more than a few times in a row,
                    // unless OneDrive keeps changing under it — then it backs
                    // off like a failure.
                    let attempts = store.with(|s| s.outbox_count_attempt(seq))?;
                    if attempts > AGAIN_LIMIT {
                        (state, reason, next_try) = (OutboxState::Retry, Some("changing in OneDrive again and again".into()), Some(now + backoff_after(attempts)));
                    }
                }
                store.with(|s| s.outbox_set_state(seq, state, reason.as_deref(), next_try))?;
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
                store.with(|s| s.outbox_set_state(seq, OutboxState::Ready, None, None))
            }
            Outcome::SignedOut => {
                {
                    let mut shared = self.shared();
                    shared.needs_sign_in = true;
                    shared.last_error = "signed out: sign in again to upload changes".into();
                }
                store.with(|s| s.outbox_set_state(seq, OutboxState::Ready, None, None))
            }
            Outcome::Forbidden => {
                {
                    let mut shared = self.shared();
                    shared.needs_sign_in = true;
                    shared.last_error = "OneDrive does not allow changes with this sign-in: sign in again".into();
                }
                let set = store.with(|s| s.outbox_set_state(seq, OutboxState::Blocked, Some(reason::FORBIDDEN), None));
                if before.as_deref() != Some(reason::FORBIDDEN) {
                    self.activity(self.event(kind::UPLOAD_FAILED, &rel, reason::FORBIDDEN));
                }
                set
            }
            Outcome::Crashed => {
                self.shared().crashed = true;
                Ok(())
            }
        };
        if let Err(e) = result {
            tracing::warn!("cannot settle outbox row {seq}: {e}");
        }
        self.publish();
    }

    /// When something may become runnable without a wake: a backoff, a
    /// throttle or a timed pause running out.
    fn next_due(&self) -> Duration {
        let now = now();
        let mut at = now + IDLE_CHECK;
        if let Some(until) = self.shared().throttled_until.filter(|&u| u > now) {
            at = at.min(until);
        }
        if let Some(until) = self.paused().filter(|&u| u > 0) {
            at = at.min(until);
        }
        // While nothing can start, only the end of a pause or throttle
        // matters; rows already due wait for a wake.
        if self.may_start() {
            if let Ok(rows) = self.store().with(|s| s.outbox_rows()) {
                for row in rows {
                    if matches!(row.state, OutboxState::Retry | OutboxState::Waiting | OutboxState::Blocked) {
                        if let Some(next) = row.next_try.filter(|&n| n > now) {
                            at = at.min(next);
                        }
                    }
                }
            }
        }
        Duration::from_secs((at - now).max(1) as u64)
    }

    /// The worker's life: drain, then sleep until woken or something falls
    /// due.
    pub(super) async fn run(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            self.drain(&cancel).await;
            if cancel.is_cancelled() {
                break;
            }
            let wait = self.next_due();
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }
}

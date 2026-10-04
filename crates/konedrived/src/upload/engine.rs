//! The worker's loop: which rows run now, how many at once, and what their
//! outcomes do to the rows and to the worker (throttling, sign-in, pause).

mod drain;
mod outcome;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use super::local::{self, SYNC_BLOCKED, SYNC_PENDING, SYNC_UPLOADING};
use super::{space, Fault, OutboxCounts, Upload, WorkerConfig, WorkerStatus, BACKOFF_FIRST, BACKOFF_MAX, THROTTLE_FIRST};
use konedrive_graph::pool::Class as PoolClass;
use crate::folder::disk::Disk;
use konedrive_tree::outbox::{OutboxKind, OutboxRow, OutboxState, Pick, Picked, Reason};
use konedrive_tree::{ActivityRow, Store, TreeError, TreeStore};

use outcome::GATE_CLOSED;
pub(crate) use outcome::Class;
pub(super) use outcome::{outcome_of, Fail, Outcome};
#[cfg(test)]
pub(super) use outcome::without_urls;

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
    crate::status::activity::unix_now()
}

/// The worker looks at the outbox at least this often, woken or not.
const IDLE_CHECK: i64 = 300;

/// A row rewritten and sent again at once more often than this backs off.
const AGAIN_LIMIT: u32 = 20;

/// Upload sessions given up that one look cancels at most (issue #47).
const CANCELS_PER_LOOK: usize = 32;

/// After a cancel that failed, the sessions given up are looked at again no
/// sooner than this (issue #47; provisional).
const CANCEL_AGAIN: i64 = 60;

struct InFlight {
    class: Class,
    rel: PathBuf,
    /// The row's reason when it was taken: an `upload-failed` event is
    /// written once per row and reason.
    reason: Option<Reason>,
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
    throttled_until: Option<i64>,
    throttle_step: Duration,
    /// The token source said the account is signed out: nothing more is taken. Never
    /// cleared: the sign-out stops the folder's sync, and this worker with it.
    needs_sign_in: bool,
    /// The rows a `403` blocked were let go once, when this worker could first send
    /// ([`Engine::release_forbidden`]).
    forbidden_released: bool,
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
    /// No session given up is cancelled before this (Unix seconds): a
    /// cancel failed (issue #47).
    cancel_after: i64,
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
    /// The daemon is stopping (issue #84): no row is taken any more, an
    /// upload in fragments stops after the fragment in flight, and the
    /// worker's run ends once the rows in flight have. For good: a worker
    /// closed is not started again.
    closing: CancellationToken,
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
                throttled_until: None,
                throttle_step: THROTTLE_FIRST,
                needs_sign_in: false,
                forbidden_released: false,
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
                cancel_after: 0,
            }),
            status,
            wake: Notify::new(),
            faults: Mutex::new(Vec::new()),
            protection: Mutex::new(super::move_out::Protection::default()),
            quota_lock: tokio::sync::Mutex::new(()),
            recount: Notify::new(),
            closing: CancellationToken::new(),
        }
    }

    /// The daemon is stopping: nothing new is taken, and what is in flight
    /// finishes its request (issue #84). See [`Engine::closing`].
    pub(super) fn close(&self) {
        self.closing.cancel();
        self.wake();
    }

    /// Whether the daemon is stopping ([`close`](Self::close)).
    pub(super) fn closing(&self) -> bool {
        self.closing.is_cancelled()
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

    /// `Some(until)` while the user paused the account (0: until resumed): what the status
    /// shows, and when a timed pause ends.
    fn paused(&self) -> Option<i64> {
        crate::conditions::running::user_pause(self.store())
    }

    /// Whether nothing may be sent now: asked of the account's one place (`conditions::running`)
    /// through the host.
    pub(super) fn stopped(&self) -> bool {
        self.cfg.host.stopped(self.store())
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

    /// The rows a `403` blocked are ready again, once in this worker's life, at the first
    /// drain in which it may send (while it may not, they stay blocked and listed): a worker
    /// begins after a sign-in (the sign-out before it stopped the folder's sync), and also
    /// after a restart or a mode switch, where the rows are tried once more and blocked
    /// again if OneDrive still refuses (`docs/design/writes.md` §6.2).
    pub(super) async fn release_forbidden(&self) {
        if self.shared().forbidden_released {
            return;
        }
        match self.store().call(move |s| s.outbox_unblock(&[Reason::Forbidden])).await {
            Ok(_) => self.shared().forbidden_released = true,
            Err(e) => tracing::warn!("cannot let the rows a 403 blocked go: {e}"),
        }
    }

    pub(crate) async fn retry_now(&self) -> Result<(), TreeError> {
        self.store().call(move |s| s.outbox_retry_now()).await?;
        self.wake();
        Ok(())
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
            needs_sign_in: shared.needs_sign_in,
            last_error: shared.last_error.clone(),
            running: shared.in_flight.len(),
            uploads: uploads.into_iter().map(|(_, u)| u).collect(),
            counts: shared.counts,
            quota_full: shared.space.full,
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
        if self.closing() {
            return false;
        }
        let paused = self.stopped();
        let now = now();
        let ready = {
            let shared = self.shared();
            !paused
                && !shared.crashed
                && shared.cycled
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
    /// [`TreeStore::outbox_pick`]: konedrive_tree::TreeStore::outbox_pick
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
    /// [`OutboxChanges`]: konedrive_tree::outbox::OutboxChanges
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
                if let Ok(Some(at)) = store.call_blocking(move |s| s.locate(konedrive_tree::Table::Items, &id)) {
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
                self.cfg.host.kept_back(&crate::upload::kept_back::summary(&skipped, &groups, full));
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
    ///
    /// Closed ([`close`](Self::close)), it ends once the rows in flight have.
    pub(super) async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let stop_tally = CancellationToken::new();
        let tally = tokio::spawn(Arc::clone(&self).tally(stop_tally.clone()));
        loop {
            self.drain(&cancel).await;
            if cancel.is_cancelled() || self.closing() {
                break;
            }
            let wait = self.next_due().await;
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.closing.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
        stop_tally.cancel();
        let _ = tally.await;
    }
}

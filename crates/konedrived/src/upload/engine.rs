//! The worker's loop: which rows run now, how many at once, and what their
//! outcomes do to the rows and to the worker (throttling, sign-in, pause).

mod drain;
mod marks;
mod outcome;
mod settle;
mod state;

use konedrive_tree::ActivityKind;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{space, Fault, Limits, OutboxCounts, OutboxHost, WorkerConfig, WorkerStatus, BACKOFF_FIRST, BACKOFF_MAX};
use crate::folder::locks::InodeLocks;
use crate::folder::root::SyncRoot;
use konedrive_graph::drive::DriveClient;
use konedrive_graph::pool::Class as PoolClass;
use konedrive_tree::outbox::{OutboxKind, OutboxRow, Pick, Picked, Reason};
use konedrive_tree::{ActivityRow, Store, TreeError, TreeStore};

use state::Shared;
pub(crate) use outcome::Class;
pub(super) use outcome::{outcome_of, Fail, NoSpace, Outcome};
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
    crate::clock::unix_now()
}

/// The worker looks at the outbox at least this often, woken or not.
const IDLE_CHECK: i64 = 300;

/// A row rewritten and sent again at once more often than this backs off.
const AGAIN_LIMIT: u32 = 20;

/// Upload sessions given up that one look cancels at most.
const CANCELS_PER_LOOK: usize = 32;

/// After a cancel that failed, the sessions given up are looked at again no
/// sooner than this (provisional).
const CANCEL_AGAIN: i64 = 60;

/// The counts are summed again at most this often while the outbox changes.
const TALLY_EVERY: Duration = Duration::from_secs(1);

/// Rows a pick looks for: once this many can run, no more portions are read
/// (a guess: more than the transfer pool runs at once).
const PICK_WANT: usize = 32;

#[cfg(test)]
pub(super) type RecordHook = Box<dyn FnOnce(std::sync::mpsc::Receiver<()>) + Send>;

pub(crate) struct Engine {
    /// What the worker works with; the other files ask through the accessors below.
    cfg: WorkerConfig,
    /// The loop's own state ([`state`]): the files of `engine/` only.
    shared: Mutex<Shared>,
    /// What is known of the space in OneDrive: `space`'s.
    space: Mutex<space::Space>,
    /// The status last handed to the host; its lock makes publishing one at a time
    /// ([`publish`](Self::publish)).
    published: Mutex<Published>,
    wake: Notify,
    /// The fault points a test armed ([`arm`](Self::arm)).
    #[cfg(test)]
    faults: Mutex<Vec<Fault>>,
    /// What the pending `move-out` rows name, re-marked on this helper
    /// connection.
    protection: Mutex<super::move_out::Protection>,
    /// The blocking sections the rows in flight have under way, the askings
    /// of the write gate among them: a stop waits for them.
    pub(super) sections: super::steps::Sections,
    /// Run once inside the next section that changes the folder and records
    /// it, between the two, with a receiver that ends when the row's task is
    /// dropped.
    #[cfg(test)]
    pub(super) record_hook: Mutex<Option<RecordHook>>,
    /// One quota read at a time: refusals of rows running together share it.
    pub(super) quota_lock: tokio::sync::Mutex<()>,
    /// The counts are wanted again though the outbox did not change (OneDrive
    /// turned full, or not).
    recount: Notify,
    /// The daemon is stopping: no row is taken any more, an
    /// upload in fragments stops after the fragment in flight, and the
    /// worker's run ends once the rows in flight have. For good: a worker
    /// closed is not started again.
    closing: CancellationToken,
}

#[derive(Default)]
struct Published {
    last: WorkerStatus,
    /// The worker was stopped: nothing more is handed over.
    silent: bool,
}

fn backoff_after(attempts: u32) -> i64 {
    let secs = BACKOFF_FIRST.as_secs().saturating_mul(1u64 << attempts.saturating_sub(1).min(20));
    secs.min(BACKOFF_MAX.as_secs()) as i64
}

impl Engine {
    pub(crate) fn new(cfg: WorkerConfig) -> Self {
        Self {
            cfg,
            shared: Mutex::new(Shared::new()),
            space: Mutex::new(space::Space::default()),
            published: Mutex::new(Published::default()),
            wake: Notify::new(),
            #[cfg(test)]
            faults: Mutex::new(Vec::new()),
            protection: Mutex::new(super::move_out::Protection::default()),
            sections: super::steps::Sections::default(),
            #[cfg(test)]
            record_hook: Mutex::new(None),
            quota_lock: tokio::sync::Mutex::new(()),
            recount: Notify::new(),
            closing: CancellationToken::new(),
        }
    }

    pub(super) fn store(&self) -> &Store {
        &self.cfg.store
    }

    pub(super) fn drive(&self) -> &DriveClient {
        &self.cfg.drive
    }

    pub(super) fn root(&self) -> &SyncRoot {
        &self.cfg.root
    }

    pub(super) fn host(&self) -> &Arc<dyn OutboxHost> {
        &self.cfg.host
    }

    /// The folder's per-inode locks.
    pub(super) fn locks(&self) -> &InodeLocks {
        &self.cfg.locks
    }

    /// The per-root tree mutex (§3.7).
    pub(super) fn tree_lock(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.cfg.tree_lock
    }

    pub(super) fn limits(&self) -> Limits {
        self.cfg.limits
    }

    pub(super) fn machine_name(&self) -> &str {
        &self.cfg.machine_name
    }

    /// The account's one quota.
    pub(super) fn quota(&self) -> &crate::account::quota::Quota {
        &self.cfg.quota
    }

    /// The helper and the fills `move-out` rows need, where the worker has them.
    pub(super) fn move_outs(&self) -> Option<&super::move_out::MoveOuts> {
        self.cfg.moved_out.as_ref()
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        crate::panic::lock(&self.shared)
    }

    /// What is known of the space in OneDrive. Never held together with the loop's state.
    pub(super) fn space(&self) -> MutexGuard<'_, space::Space> {
        crate::panic::lock(&self.space)
    }

    /// What the outbox held when the worker last counted.
    pub(super) fn counts(&self) -> OutboxCounts {
        self.shared().counts
    }

    /// The daemon is stopping: nothing new is taken, and what is in flight
    /// finishes its request. See [`Engine::closing`].
    pub(super) fn close(&self) {
        self.closing.cancel();
        self.wake();
    }

    /// Whether the daemon is stopping ([`close`](Self::close)).
    pub(super) fn closing(&self) -> bool {
        self.closing.is_cancelled()
    }

    pub(super) fn protection(&self) -> MutexGuard<'_, super::move_out::Protection> {
        crate::panic::lock(&self.protection)
    }

    /// The helper is back without its marks: the `move-out` rows' objects
    /// are marked again at the next look, before any row runs.
    pub(super) fn helper_back(&self) {
        self.protection().helper_back();
        self.wake();
    }

    #[cfg(test)]
    pub(super) fn before_record(&self, row_dropped: std::sync::mpsc::Receiver<()>) {
        let hook = crate::panic::lock(&self.record_hook).take();
        if let Some(hook) = hook {
            hook(row_dropped);
        }
    }

    /// A fault point of a step: nothing, in the daemon.
    #[cfg(not(test))]
    #[inline(always)]
    pub(super) fn fault(&self, _fault: Fault) -> Result<(), Fail> {
        Ok(())
    }

    /// A fault point of a step: the step stops here if a test armed it.
    #[cfg(test)]
    pub(super) fn fault(&self, fault: Fault) -> Result<(), Fail> {
        let mut armed = crate::panic::lock(&self.faults);
        if let Some(i) = armed.iter().position(|f| *f == fault) {
            armed.remove(i);
            tracing::warn!("fault point {fault:?}: the outbox worker stops here");
            return Err(Fail::Crashed);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn arm(&self, fault: Fault) {
        crate::panic::lock(&self.faults).push(fault);
    }

    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }

    /// `Some(until)` while the user paused the account (0: until resumed): what the status
    /// shows, and when a timed pause ends.
    fn paused(&self) -> Option<i64> {
        crate::conditions::running::user_pause(self.store(), self.cfg.host.now())
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
        self.shared().cycle.wait(network_back);
        self.publish();
    }

    /// A delta cycle went through: the base caught up. Rows in backoff go
    /// now after the first cycle and after the network came back.
    pub(crate) async fn cycle_done(&self) {
        let due = self.shared().cycle.went_through();
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

    /// Whether no row is in flight.
    fn idle(&self) -> bool {
        self.shared().flights.is_empty()
    }

    /// The worker's status as it is now.
    pub(super) fn status(&self) -> WorkerStatus {
        let now = now();
        let quota_full = self.space().full;
        let shared = self.shared();
        WorkerStatus { throttled_until: shared.throttle.until(now), folder_closed: shared.trouble.folder_closed(), uploads: shared.flights.uploads(), counts: shared.counts, quota_full }
    }

    /// Hands the status to the host when it changed. One publisher at a time, and the
    /// status is read inside its turn: of two that publish together (a row's progress, a
    /// row settled, the counts), the host's last word is the later state, never an earlier
    /// one read before the other's change and handed over after it. A worker that was
    /// stopped hands over nothing ([`silence`](Self::silence)).
    pub(super) fn publish(&self) {
        let mut published = crate::panic::lock(&self.published);
        if published.silent {
            return;
        }
        let status = self.status();
        if published.last != status {
            self.cfg.host.status(&status);
            published.last = status;
        }
    }

    /// The worker was stopped (`silent`), or is started: once this returns for a stopped
    /// one, the host hears no more of it — not from a task that outlives the stop (a
    /// detached `cycle_done`), which would put back what the host cleared after the stop.
    pub(super) fn silence(&self, silent: bool) {
        crate::panic::lock(&self.published).silent = silent;
    }

    pub(super) fn upload_progress(&self, seq: i64, sent: u64, total: u64) {
        self.shared().flights.progress(seq, sent, total);
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

    pub(super) fn event(&self, kind: ActivityKind, rel: &std::path::Path, detail: impl Into<String>) -> ActivityRow {
        ActivityRow { at: now(), kind, path: self.cfg.root.path.join(rel).display().to_string(), detail: detail.into() }
    }

    /// Whether the worker itself keeps from sending now: it is closing, the account's
    /// work is stopped (the user's pause among the reasons), it is signed out or met a
    /// fault point, the folder could not be opened, the cycle it waits for has not gone
    /// through, or OneDrive asked to wait.
    fn held(&self) -> bool {
        if self.closing() || self.stopped() {
            return true;
        }
        let now = now();
        let shared = self.shared();
        shared.trouble.holds() || !shared.cycle.done() || shared.throttle.holds(now)
    }

    /// Whether a row may be taken now: the worker is not [`held`](Self::held) and the
    /// write gate, asked now, is open ([`ask_gate`](Self::ask_gate)).
    async fn may_send(&self) -> bool {
        !self.held() && self.ask_gate().await
    }

    /// The host's write gate ([`OutboxHost::may_write`](super::OutboxHost::may_write)), asked
    /// on a blocking thread, as one section: the answer takes reading `config.toml` again.
    /// The section is awaited here; where the asking task is cut off instead (a row between
    /// two fragments, at a stop), the drain that cut it waits for the section as for any
    /// other of the worker's ([`Sections::ended`](super::steps::Sections::ended)).
    pub(super) async fn may_write(&self) -> Result<(), String> {
        let running = self.sections.running().await;
        let host = Arc::clone(&self.cfg.host);
        let asked = tokio::task::spawn_blocking(move || {
            let _running = running;
            host.may_write()
        });
        match asked.await {
            Ok(answer) => answer,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Err("the daemon is stopping".into()),
        }
    }

    /// Asks the write gate (`docs/design/writes.md` §2.3) and keeps its answer: whether the
    /// host lets the account change OneDrive now. Closed, nothing more is taken, the rows
    /// wait, the host says why (its own note), and the journal gets each new reason once.
    async fn ask_gate(&self) -> bool {
        let closed = self.may_write().await.err();
        if self.shared().trouble.gate(closed.clone()) {
            tracing::warn!("nothing is uploaded: {}", closed.as_deref().unwrap_or_default());
        }
        closed.is_none()
    }

    /// Whether a row of `class` may start beside those running: metadata and move-outs
    /// one at a time; content as the pool allows.
    fn slot_free(&self, class: Class) -> bool {
        let busy = self.shared().flights.of(class);
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
    /// (`Shared::waits`, [`next_due`](Self::next_due)), and a time it names wakes the worker.
    ///
    /// [`TreeStore::outbox_pick`]: konedrive_tree::TreeStore::outbox_pick
    pub(crate) async fn candidates(&self) -> Result<Vec<(OutboxRow, Class)>, TreeError> {
        let now = now();
        let flying: HashSet<i64> = self.shared().flights.seqs();
        let move_outs = self.move_outs().is_some();
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

    /// `PendingCount` and the rest, and the Not Uploaded summary, summed by
    /// SQL through the store's read-only connection — never waiting for a
    /// writer — and kept in memory for the bus.
    pub(super) async fn recount(&self) {
        let full = self.space_full();
        match self.store().read(|s| Ok((s.outbox_groups()?, s.outbox_groups_unlisted()?, s.skipped_groups()?))).await {
            Ok((groups, unlisted, skipped)) => {
                self.shared().counts = OutboxCounts::of(&groups, full);
                self.cfg.host.kept_back(&crate::upload::kept_back::summary(&skipped, &unlisted, full));
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

    /// How long until something may become runnable without a wake: a throttle or a timed
    /// pause running out; and, while the worker may send, a backoff running out, a time the
    /// pick named, the quota's next read. Five minutes at most, a second at least.
    ///
    /// While the worker may not send, only what ends that counts: the rows that are due and
    /// a quota read that is due wait with it, and do not bring it back every second.
    async fn next_due(&self) -> Duration {
        let now = now();
        let mut at = now + IDLE_CHECK;
        if let Some(until) = self.shared().throttle.until(now) {
            at = at.min(until);
        }
        if let Some(until) = self.paused().filter(|&u| u > 0) {
            // The pause ends by the account's clock: as far from now as it is from that.
            at = at.min(now + (until - self.cfg.host.now()).max(0));
        }
        if self.may_send().await {
            // Read only with no row in flight (`take_rows`): until then it does not count.
            if let Some(check) = self.space_check_at().filter(|_| self.idle()) {
                at = at.min(check);
            }
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

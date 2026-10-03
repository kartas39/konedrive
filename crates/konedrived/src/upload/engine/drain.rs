use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use crate::upload::local;
use crate::upload::{kind, reason, space, BACKOFF_MAX, THROTTLE_FIRST};
use konedrive_graph::drive::write::MAX_RETRY_AFTER;
use konedrive_graph::pool::{Class as PoolClass, Size, Slot};
use crate::folder::disk::Disk;
use konedrive_tree::outbox::OutboxState;
use konedrive_tree::TreeError;

use super::outcome::{Class, Outcome, without_urls};
use super::{AGAIN_LIMIT, backoff_after, CANCEL_AGAIN, CANCELS_PER_LOOK, Engine, InFlight, now, pool_class};

impl Engine {
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
        self.release_forbidden().await;
        self.space_start().await;
        // The quota, read again when it is due (while full, while a file is
        // too big, once after a start that found waiting rows).
        if self.may_start() {
            self.space_check(now()).await;
            self.cancel_given_up().await;
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
                                let outcome = crate::upload::steps::run(&engine, &disk, claimed).await;
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
                            self.settle_blocking(seq, Outcome::backoff(reason::FAILED)).await;
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

    /// Cancels the upload sessions given up (issue #47): listed, and pointed
    /// at by no row — the row left the outbox, moved on to other content, or
    /// its own cancel failed. At every drain the worker may send in, the
    /// start's included; a cancel that fails stops the look, and the next
    /// one waits [`CANCEL_AGAIN`].
    pub(in crate::upload) async fn cancel_given_up(&self) {
        if self.shared().cancel_after > now() {
            return;
        }
        if !crate::upload::cancel_given_up(self.store(), &self.cfg.drive, CANCELS_PER_LOOK).await {
            self.shared().cancel_after = now() + CANCEL_AGAIN;
        }
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
            Outcome::Again { state, reason, next_try, backoff, detail } => (|| {
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
                // Once per row and key, as the event: a long network drop
                // writes one line, not one per retry.
                if let Some(detail) = detail.filter(|_| reason != before) {
                    tracing::warn!("{} is tried again later ({}): {}", rel.display(), reason.as_deref().unwrap_or_default(), without_urls(&detail));
                }
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
            // Its own row only: a `403` can be about one item, and whether the sign-in
            // allows writes at all is the write gate's to say. The other rows go on.
            Outcome::Forbidden => {
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

}

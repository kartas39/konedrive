//! One drain of the outbox: which row is taken next, and what the worker waits for.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use crate::upload::local;
use konedrive_graph::pool::{Class as PoolClass, Size, Slot};
use crate::folder::disk::Disk;
use konedrive_tree::outbox::{OutboxRow, Reason};

use super::outcome::{Class, Outcome};
use super::state::Flight;
use super::{CANCEL_AGAIN, CANCELS_PER_LOOK, Engine, now, pool_class};

/// A pool class and size whose row waits for a slot.
type Wants = (PoolClass, Size);

/// The rows one drain has in flight, and the pool slots it holds for the next ones.
#[derive(Default)]
struct Flying {
    set: JoinSet<(i64, Outcome)>,
    tasks: HashMap<tokio::task::Id, i64>,
    /// Slots of the account's transfer pool that came while the loop waited, each for the
    /// next row of its class and size; one that no row takes goes back at once.
    spare: Vec<Slot>,
}

/// What became of a row the worker looked at.
enum Taken {
    /// In flight now.
    Yes,
    /// Not now: its class is busy, it waits for a slot, or it was taken elsewhere. The
    /// rows behind it are looked at.
    No,
    /// The worker may not send any more: no row behind this one is looked at.
    Held,
}

impl Engine {
    /// Runs rows until none can run now (or `cancel`): what [`run`] does each
    /// time it is woken, and what tests call directly. A fault point stops
    /// it as a crash would, leaving the row `running`: this worker takes
    /// nothing more until it is built again.
    ///
    /// **The order.** Each look, in turn:
    ///
    /// 1. what left the folder is marked again, and the rows' marks are written, whether
    ///    or not rows may run (`docs/design/writes.md` §8);
    /// 2. if the worker may send ([`may_send`](Engine::may_send)), the quota is read when
    ///    its read is due and no row is in flight, and the rows that may run are taken in `seq` order
    ///    ([`candidates`](Engine::candidates)): metadata rows one at a time, move-outs one
    ///    at a time, content rows as the account's transfer pool gives slots. A row with
    ///    no slot waits for one and holds back the later rows of its pool class and size,
    ///    and no others: a large file waiting for the large-file limit does not hold up
    ///    the small ones. Before each row the worker asks again whether it may send — the
    ///    pause, the stop, the write gate — and takes nothing more once it may not;
    /// 3. with nothing in flight and no row waiting for a slot, the drain ends. Otherwise
    ///    the worker waits for the first of: a row ending, which is settled
    ///    (`engine/settle.rs`); a slot for a row that waited; a wake (new rows, the helper
    ///    back, a quota read); the time something falls due ([`next_due`](Engine::next_due):
    ///    a backoff, a throttle or a timed pause running out), so that a row that is due
    ///    does not wait for an unrelated long upload to end; the stop. Then it looks again.
    ///
    /// A stop cuts the rows in flight, waits for the blocking sections they have under
    /// way, and leaves the rows `running` for the next start.
    ///
    /// [`run`]: Engine::run
    pub(crate) async fn drain(self: &Arc<Self>, cancel: &CancellationToken) {
        let disk = match Disk::open(self.root(), false) {
            Ok(disk) => {
                self.shared().trouble.folder(None);
                Arc::new(disk)
            }
            // Nothing is taken until a drain opens it: at the next wake, or the worker's
            // five-minute look. A folder that is gone stops the sync, and this worker
            // with it (`sync/watcher.rs`); for any other failure the folder's note says
            // that uploads wait, and a throttle that ran out meanwhile is no longer said.
            Err(e) => {
                tracing::warn!("the OneDrive folder cannot be opened, so nothing is uploaded: {e}");
                self.shared().trouble.folder(Some(e.to_string()));
                self.publish();
                return;
            }
        };
        self.space_start().await;
        if self.may_send().await {
            // Only when the worker may send: until then the rows stay blocked, and listed.
            self.release_forbidden().await;
            self.cancel_given_up().await;
        }
        let mut flying = Flying::default();
        loop {
            self.protect(&disk).await;
            self.mark_rows_blocking(&disk).await;
            let wanting = self.take_rows(&disk, &mut flying).await;
            if flying.set.is_empty() && wanting.is_empty() {
                break;
            }
            if !self.wait_for_event(&mut flying, &wanting, cancel).await {
                break;
            }
        }
        self.mark_rows_blocking(&disk).await;
        self.recount().await;
        self.publish();
    }

    /// Takes every row that may run now, in order; the pool classes and sizes whose rows
    /// wait for a slot.
    async fn take_rows(self: &Arc<Self>, disk: &Arc<Disk>, flying: &mut Flying) -> Vec<Wants> {
        let mut wanting = Vec::new();
        if self.may_send().await {
            // The quota, read again when it is due (while full, while a file is too big,
            // once after a start that found waiting rows): what it lets go is picked below.
            // Only with no row in flight: the read is a request awaited here, and while it
            // lasts no row that ended would be settled and no stop heard.
            if flying.set.is_empty() {
                self.space_check(now()).await;
            }
            match self.candidates().await {
                Ok(rows) => {
                    for (row, class) in rows {
                        if let Taken::Held = self.take_row(disk, flying, &mut wanting, row, class).await {
                            break;
                        }
                    }
                }
                Err(e) => tracing::warn!("cannot read the outbox: {e}"),
            }
        }
        // Also when nothing may be taken: a throttle that ran out is no longer said.
        self.publish();
        flying.spare.clear();
        wanting
    }

    /// Takes `row` if its class has room and the pool a slot for it: claimed in the store
    /// (`running`), and run as a task that holds the slot for all it sends.
    async fn take_row(self: &Arc<Self>, disk: &Arc<Disk>, flying: &mut Flying, wanting: &mut Vec<Wants>, row: OutboxRow, class: Class) -> Taken {
        if !self.slot_free(class) {
            return Taken::No;
        }
        // The local file's size says whether its upload is large.
        let size = match class {
            Class::Content => Size::of(local::size_at(disk, &row.rel).unwrap_or(0)),
            Class::Meta | Class::Out => Size::Small,
        };
        let wants = (pool_class(class), size);
        if wanting.contains(&wants) {
            return Taken::No;
        }
        // Asked again right before each row is taken.
        if !self.may_send().await {
            return Taken::Held;
        }
        let slot = match flying.spare.iter().position(|slot| (slot.class(), slot.size()) == wants) {
            Some(at) => flying.spare.swap_remove(at),
            None => match self.drive().pool().try_acquire_sized(wants.0, wants.1) {
                Some(slot) => slot,
                None => {
                    wanting.push(wants);
                    return Taken::No;
                }
            },
        };
        let (seq, state) = (row.seq, row.state);
        let claimed = match self.store().call(move |s| s.outbox_claim(seq, state)).await {
            Ok(Some(claimed)) => claimed,
            Ok(None) => {
                flying.spare.push(slot);
                return Taken::No;
            }
            Err(e) => {
                tracing::warn!("cannot take outbox row {}: {e}", row.seq);
                flying.spare.push(slot);
                return Taken::No;
            }
        };
        let seq = claimed.seq;
        self.shared().flights.took(seq, Flight { class, rel: claimed.rel.clone(), reason: row.reason });
        let engine = Arc::clone(self);
        let disk = Arc::clone(disk);
        let handle = flying.set.spawn(async move {
            // The row holds its slot for all it sends, every fragment.
            let mut slot = slot;
            let outcome = engine.sections.of(crate::upload::steps::run(&engine, &disk, claimed)).await;
            if matches!(outcome, Outcome::Done) {
                slot.succeeded();
            }
            drop(slot);
            (seq, outcome)
        });
        flying.tasks.insert(handle.id(), seq);
        Taken::Yes
    }

    /// Waits for the first thing that can change what runs — a row ending (settled here),
    /// a slot for a row that waited, a wake, a time falling due, the stop — and says
    /// whether the drain goes on: `false` after the stop.
    async fn wait_for_event(self: &Arc<Self>, flying: &mut Flying, wanting: &[Wants], cancel: &CancellationToken) -> bool {
        let due = self.next_due().await;
        let pool = Arc::clone(self.drive().pool());
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
            // A slot for a row that waited for one: taken at the next look.
            slot = waited => flying.spare.push(slot),
            // New rows, or the helper back: looked at while the others run.
            _ = self.wake.notified() => {}
            // A row in backoff is due, a throttle or a timed pause ran out.
            _ = tokio::time::sleep(due) => {}
            _ = cancel.cancelled() => {
                flying.set.shutdown().await;
                // A row dropped while it waited for a blocking section: the
                // section ends before the worker has stopped.
                self.sections.ended().await;
                self.shared().flights.clear();
                return false;
            }
            joined = flying.set.join_next_with_id(), if !flying.set.is_empty() => match joined {
                Some(Ok((id, (seq, outcome)))) => {
                    flying.tasks.remove(&id);
                    self.settle_blocking(seq, outcome).await;
                }
                Some(Err(e)) => {
                    if let Some(seq) = flying.tasks.remove(&e.id()) {
                        // Replayed later, in backoff, never at once.
                        tracing::error!("outbox row {seq} failed: {e}; it is tried again later");
                        self.settle_blocking(seq, Outcome::backoff(Reason::Failed)).await;
                    }
                }
                None => {}
            }
        }
        true
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
        if !crate::upload::cancel_given_up(self.store(), self.drive(), CANCELS_PER_LOOK).await {
            self.shared().cancel_after = now() + CANCEL_AGAIN;
        }
    }
}

//! A full OneDrive (issue #2, `docs/design/writes.md` §4.11): what the
//! worker does when OneDrive refuses content for lack of space.
//!
//! **Free space** is Graph's `quota.remaining`, never `total - used`, less
//! what the worker uploaded since it was read. On a refusal (`507`,
//! `quotaLimitReached`) the quota is read again at once, one request:
//!
//! - **no space left** — `state` is `exceeded`, or less than [`NO_SPACE`] is
//!   free: the account is *full*. No content row is taken, and uploads under
//!   way stop at their next fragment, keeping their session. Moves, renames,
//!   deletes and folders go on;
//! - **space left** — only the refused file waits, *too big*
//!   (`too-big:<needs>:<free>`); the rest keep going. A *too big* file is not
//!   sent again until a quota read shows it fits. Any other file is sent
//!   whatever the known free space says: OneDrive has the last word.
//!
//! A waiting row stays `ready` in its place in the outbox, with no timer of
//! its own; only its reason says it waits. A quota read — `Refresh`,
//! `RefreshInfo`, and every [`QUOTA_RECHECK`] while full or while a
//! file is too big — ends *full* when there is space again and frees the
//! files that now fit.
//!
//! The quota itself is the account's one (`crate::quota`, served by
//! `Account`): the worker reads into it, takes what it uploads off it, and
//! keeps no copy of its own.

use std::collections::HashSet;
use std::time::Duration;

use super::engine::{now, Engine, Outcome};
use super::local;
use konedrive_graph::drive::DriveQuota;
use crate::sync::disk::Disk;
use konedrive_tree::outbox::{OutboxRow, OutboxState};
use konedrive_tree::{Store, TreeError};

/// Less free space than this is none (a guess: the smallest file still goes,
/// a real one does not).
pub const NO_SPACE: u64 = 1 << 20;

/// While full, or while a file is too big, the quota is read again this
/// often (one request; a guess).
pub const QUOTA_RECHECK: Duration = Duration::from_secs(30 * 60);

/// A read this recent answers another refusal too: refusals of rows running
/// together cost one request.
const REUSE: i64 = 10;

/// The reason of a row refused while OneDrive is full.
pub const WAITING: &str = "waiting-for-space";

/// The reason of a row refused while space is left: it needs `needs` bytes
/// and `free` are free.
pub fn too_big(needs: u64, free: u64) -> String {
    format!("{TOO_BIG}{needs}:{free}")
}

const TOO_BIG: &str = "too-big:";

/// The key every *too big* reason is summed under (`NotUploadedSummary()`).
pub const TOO_BIG_KEY: &str = "too-big";

/// `(needs, free)` of a *too big* reason.
pub fn parse_too_big(reason: &str) -> Option<(u64, u64)> {
    let (needs, free) = reason.strip_prefix(TOO_BIG)?.split_once(':')?;
    Some((needs.parse().ok()?, free.parse().ok()?))
}

/// Whether a row with `reason` waits for space: not taken until a quota
/// read lets it go.
pub fn waits(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| r == WAITING || r.starts_with(TOO_BIG))
}

/// Whether `quota` says anything of the space: Graph gave `remaining` or
/// `state`. One that does not is ignored.
pub fn known(quota: &DriveQuota) -> bool {
    quota.remaining.is_some() || !quota.state.is_empty()
}

/// Whether `quota` leaves no space at all.
pub fn no_space(quota: &DriveQuota) -> bool {
    quota.state == "exceeded" || quota.remaining.is_some_and(|free| free < NO_SPACE)
}

/// What the worker knows of the space in OneDrive.
#[derive(Debug, Default)]
pub(super) struct Space {
    /// No content goes up.
    pub full: bool,
    /// The next automatic read, while one is wanted.
    next_check: i64,
    /// A read is wanted whatever the rows say: the outbox held waiting rows
    /// at start-up.
    pub(super) wanted: bool,
    /// The start's look at the outbox is done ([`Space::start`]): the first
    /// drain does it, off the constructor.
    pub(super) started: bool,
    /// Waiting rows taken, since the last quota read, to see whether their
    /// file is gone, and found it was not ([`Engine::space_holds`]): not
    /// taken again for that until the next read.
    looked: HashSet<i64>,
}

impl Space {
    /// The space as a start finds it: rows a previous version blocked on a
    /// full OneDrive become waiting rows, and the quota is read once before
    /// they go. Rows waiting for space keep the worker full until then.
    pub(super) async fn start(store: &Store) -> Self {
        let converted = store.call(move |s| s.outbox_space_convert(super::reason::QUOTA, WAITING)).await.unwrap_or_else(|e| {
            tracing::warn!("cannot convert the outbox's rows blocked on a full OneDrive: {e}");
            0
        });
        if converted > 0 {
            tracing::info!("{converted} change(s) blocked on a full OneDrive wait for space now");
        }
        let groups = store.call(move |s| s.outbox_groups()).await.unwrap_or_default();
        let full = groups.iter().any(|g| g.reason().as_deref() == Some(WAITING));
        let wanted = full || groups.iter().any(|g| waits(g.reason().as_deref()));
        Self { full, wanted, started: true, ..Self::default() }
    }
}

/// Whether `row` may be taken as the space stands (`full`, and the waiting
/// rows `looked` at since the last quota read). A `create` that waits for
/// space is taken all the same when a removal of its object stands behind it
/// (`removed`): a file removed before its upload finished leaves the outbox at
/// once (issue #27), full or not. Its run sends no content: it ends if the
/// file is gone, and waits on if not ([`Engine::space_holds`]).
pub(super) fn allows(row: &OutboxRow, full: bool, looked: &HashSet<i64>, removed: impl FnOnce() -> Result<bool, TreeError>) -> Result<bool, TreeError> {
    if !waits(row.reason.as_deref()) && !(full && row.kind.sends_content()) {
        return Ok(true);
    }
    Ok(row.kind == konedrive_tree::outbox::OutboxKind::Create && !looked.contains(&row.seq) && removed()?)
}

/// The size a waiting row sends: its snapshot's, or the file's now.
fn size_of(row: &OutboxRow, disk: Option<&Disk>) -> u64 {
    row.snapshot
        .as_deref()
        .and_then(|s| s.split(' ').next())
        .and_then(|s| s.parse().ok())
        .or(row.size)
        .or_else(|| disk.and_then(|d| local::size_at(d, &row.rel)))
        .unwrap_or(0)
}

impl Engine {
    /// No content row is taken, and none sends more.
    pub(super) fn space_full(&self) -> bool {
        self.shared().space.full
    }

    /// `bytes` went up: the account's quota says as much (`crate::quota`).
    pub(super) fn space_used(&self, bytes: u64) {
        self.cfg.quota.uploaded(bytes);
    }

    /// What a pick needs to know of the space: whether OneDrive is full, and
    /// the waiting rows already looked at since the last quota read.
    pub(super) fn space_seen(&self) -> (bool, HashSet<i64>) {
        let shared = self.shared();
        (shared.space.full, shared.space.looked.clone())
    }

    /// The reason a content row taken while it waits for space waits on
    /// with, if it does: its own `waiting-for-space` or `too-big:…`, or
    /// `waiting-for-space` while OneDrive is full.
    /// Such a row is not taken again for its removal until the next quota
    /// read.
    pub(super) fn space_holds(&self, row: &OutboxRow) -> Option<String> {
        let why = match row.reason.as_deref() {
            Some(r) if waits(Some(r)) => Some(r.to_owned()),
            _ => (self.space_full() && row.kind.sends_content()).then(|| WAITING.to_owned()),
        };
        if why.is_some() {
            self.shared().space.looked.insert(row.seq);
        }
        why
    }

    /// The quota, read now — or, `reuse`, the account's read of a moment
    /// ago, which a refusal of another row, `Refresh` or the account's info
    /// made (`false` then: it was applied already).
    async fn read_quota(&self, reuse: bool) -> Option<(DriveQuota, bool)> {
        let _one = self.quota_lock.lock().await;
        if let Some(quota) = self.cfg.quota.read_within(REUSE).filter(|q| reuse && known(q)) {
            return Some((quota, false));
        }
        match self.cfg.drive.quota().await {
            Ok(quota) if known(&quota) => Some((quota, true)),
            Ok(_) => {
                tracing::warn!("OneDrive gave no quota");
                None
            }
            Err(e) => {
                tracing::warn!("cannot read the OneDrive quota: {e}");
                None
            }
        }
    }

    /// OneDrive refused row `seq`'s content for lack of space: the quota is
    /// read at once and decides whether the account is full or only this
    /// file, of `size` bytes, too big. A quota that cannot be read counts as
    /// full: OneDrive just said so, and the next read decides.
    pub(super) async fn space_refused(&self, size: u64) -> Outcome {
        match self.read_quota(true).await {
            Some((quota, fresh)) => {
                if fresh {
                    self.apply_quota(&quota).await;
                }
                if no_space(&quota) {
                    self.turn_full();
                    Outcome::Space(WAITING.into())
                } else {
                    Outcome::Space(too_big(size, quota.remaining.unwrap_or(0)))
                }
            }
            None => {
                self.turn_full();
                self.shared().space.next_check = now() + QUOTA_RECHECK.as_secs() as i64;
                Outcome::Space(WAITING.into())
            }
        }
    }

    fn turn_full(&self) {
        let mut shared = self.shared();
        if !shared.space.full {
            tracing::warn!("OneDrive is full: nothing more is uploaded until there is space again");
            shared.space.full = true;
            drop(shared);
            self.recount_soon();
        }
    }

    /// A quota the worker just read (the automatic read, a refusal): into
    /// the account's quota, then decided by ([`decide_quota`](Self::decide_quota)).
    pub(crate) async fn apply_quota(&self, quota: &DriveQuota) {
        if !known(quota) {
            return;
        }
        self.cfg.quota.read(quota);
        self.decide_quota(quota).await;
    }

    /// A quota just read, into the account's quota already (`Refresh`,
    /// `RefreshInfo`, [`apply_quota`](Self::apply_quota)): *full* or not, and
    /// every waiting file that fits now is free to go; one that does not
    /// stays *too big*, with the free space said again. A quota that says
    /// nothing of the space is ignored.
    pub(crate) async fn decide_quota(&self, quota: &DriveQuota) {
        if !known(quota) {
            return;
        }
        let now = now();
        let full = no_space(quota);
        {
            let mut shared = self.shared();
            let space = &mut shared.space;
            if space.full && !full {
                tracing::info!("OneDrive has space again: uploads go on");
            } else if full && !space.full {
                tracing::warn!("OneDrive is full: nothing more is uploaded until there is space again");
            }
            if space.full != full {
                self.recount_soon();
            }
            space.full = full;
            space.next_check = now + QUOTA_RECHECK.as_secs() as i64;
            space.wanted = false;
            space.looked.clear();
        }
        if !full {
            if let Err(e) = self.release_fitting(quota.remaining.unwrap_or(0)).await {
                tracing::warn!("cannot let the files waiting for space go: {e}");
            }
        }
        self.publish();
        self.wake();
    }

    /// Every row waiting for space whose file fits in `free` goes again;
    /// the rest are *too big* for it.
    async fn release_fitting(&self, free: u64) -> Result<(), TreeError> {
        let rows = self.store().call(move |s| s.outbox_waiting_for_space()).await?;
        let disk = Disk::open(&self.cfg.root, false).ok();
        for row in rows.iter().filter(|r| r.state == OutboxState::Ready && waits(r.reason.as_deref())) {
            let size = size_of(row, disk.as_ref());
            let reason = (size > free).then(|| too_big(size, free));
            if reason != row.reason {
                let seq = row.seq;
                self.store().call(move |s| s.outbox_set_state(seq, OutboxState::Ready, reason.as_deref(), None)).await?;
            }
        }
        Ok(())
    }

    /// When the quota is read again by itself, if it is: while full, while
    /// a file is too big, or once after a start that found waiting rows.
    pub(super) fn space_check_at(&self) -> Option<i64> {
        let shared = self.shared();
        let wanted = shared.space.full || shared.counts.too_big > 0 || shared.space.wanted;
        wanted.then_some(shared.space.next_check)
    }

    /// The automatic read (one request), when it is due at `now`: the drain
    /// asks each time it runs; tests hand it a later `now`.
    pub(crate) async fn space_check(&self, now: i64) {
        if !self.space_check_at().is_some_and(|at| at <= now) {
            return;
        }
        match self.read_quota(false).await {
            Some((quota, _)) => self.apply_quota(&quota).await,
            None => self.shared().space.next_check = now + QUOTA_RECHECK.as_secs() as i64,
        }
    }
}

#[cfg(test)]
mod tests;

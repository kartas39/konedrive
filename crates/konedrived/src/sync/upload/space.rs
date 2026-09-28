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
//! `RefreshAccountInfo`, and every [`QUOTA_RECHECK`] while full or while a
//! file is too big — ends *full* when there is space again and frees the
//! files that now fit.

use std::time::Duration;

use super::engine::{now, Engine, Outcome};
use super::local;
use crate::drive::DriveQuota;
use crate::sync::disk::Disk;
use crate::tree::outbox::{OutboxRow, OutboxState};
use crate::tree::{Store, TreeError};

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
    /// The last quota read, and when (unix seconds).
    last: Option<(DriveQuota, i64)>,
    /// Graph's `remaining` as last read, less what went up since.
    pub free: Option<u64>,
    /// The next automatic read, while one is wanted.
    next_check: i64,
    /// A read is wanted whatever the rows say: the outbox held waiting rows
    /// at start-up.
    wanted: bool,
}

impl Space {
    /// Graph's `quota.state` as last read.
    pub(super) fn state(&self) -> String {
        self.last.as_ref().map(|(q, _)| q.state.clone()).unwrap_or_default()
    }

    /// The space as a start finds it: rows a previous version blocked on a
    /// full OneDrive become waiting rows, and the quota is read once before
    /// they go. Rows waiting for space keep the worker full until then.
    pub(super) fn start(store: &Store) -> Self {
        let converted = store.with(|s| s.outbox_space_convert(super::reason::QUOTA, WAITING)).unwrap_or_else(|e| {
            tracing::warn!("cannot convert the outbox's rows blocked on a full OneDrive: {e}");
            0
        });
        if converted > 0 {
            tracing::info!("{converted} change(s) blocked on a full OneDrive wait for space now");
        }
        let rows = store.with(|s| s.outbox_rows()).unwrap_or_default();
        let full = rows.iter().any(|r| r.reason.as_deref() == Some(WAITING));
        let wanted = full || rows.iter().any(|r| waits(r.reason.as_deref()));
        Self { full, wanted, ..Self::default() }
    }
}

/// The size a waiting row sends: its snapshot's, or the file's now.
fn size_of(row: &OutboxRow, disk: Option<&Disk>) -> u64 {
    row.snapshot
        .as_deref()
        .and_then(|s| s.split(' ').next())
        .and_then(|s| s.parse().ok())
        .or_else(|| disk.and_then(|d| local::size_at(d, &row.rel)))
        .unwrap_or(0)
}

impl Engine {
    /// No content row is taken, and none sends more.
    pub(super) fn space_full(&self) -> bool {
        self.shared().space.full
    }

    /// `bytes` went up: the free space known shrinks by as much.
    pub(super) fn space_used(&self, bytes: u64) {
        let mut shared = self.shared();
        if let Some(free) = shared.space.free.as_mut() {
            *free = free.saturating_sub(bytes);
        }
    }

    /// Whether `row` may be taken as the space stands.
    pub(super) fn space_allows(&self, row: &OutboxRow, full: bool) -> bool {
        !waits(row.reason.as_deref()) && !(full && row.kind.sends_content())
    }

    /// The quota, read now — or, `reuse`, the read of a moment ago, which a
    /// refusal of another row made (`false` then: it was applied already).
    async fn read_quota(&self, reuse: bool) -> Option<(DriveQuota, bool)> {
        let _one = self.quota_lock.lock().await;
        if let Some((quota, at)) = self.shared().space.last.clone().filter(|_| reuse) {
            if now() - at < REUSE {
                return Some((quota, false));
            }
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
                    self.apply_quota(&quota);
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
        }
    }

    /// A quota just read (`Refresh`, `RefreshAccountInfo`, the automatic
    /// read, a refusal): *full* or not, and every waiting file that fits now
    /// is free to go; one that does not stays *too big*, with the free space
    /// said again. A quota that says nothing of the space is ignored.
    pub(super) fn apply_quota(&self, quota: &DriveQuota) {
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
            space.full = full;
            space.free = quota.remaining;
            space.last = Some((quota.clone(), now));
            space.next_check = now + QUOTA_RECHECK.as_secs() as i64;
            space.wanted = false;
        }
        if !full {
            if let Err(e) = self.release_fitting(quota.remaining.unwrap_or(0)) {
                tracing::warn!("cannot let the files waiting for space go: {e}");
            }
        }
        self.publish();
        self.wake();
    }

    /// Every row waiting for space whose file fits in `free` goes again;
    /// the rest are *too big* for it.
    fn release_fitting(&self, free: u64) -> Result<(), TreeError> {
        let rows = self.store().with(|s| s.outbox_rows())?;
        let disk = Disk::open(&self.cfg.root, false).ok();
        for row in rows.iter().filter(|r| r.state == OutboxState::Ready && waits(r.reason.as_deref())) {
            let size = size_of(row, disk.as_ref());
            let reason = (size > free).then(|| too_big(size, free));
            if reason != row.reason {
                self.store().with(|s| s.outbox_set_state(row.seq, OutboxState::Ready, reason.as_deref(), None))?;
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
            Some((quota, _)) => self.apply_quota(&quota),
            None => self.shared().space.next_check = now + QUOTA_RECHECK.as_secs() as i64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_is_exceeded_or_less_than_a_mebibyte_free() {
        let quota = |remaining: Option<u64>, state: &str| DriveQuota { total: 10 << 30, used: 0, remaining, state: state.into() };
        assert!(no_space(&quota(Some(5 << 30), "exceeded")));
        assert!(no_space(&quota(Some(NO_SPACE - 1), "critical")));
        assert!(!no_space(&quota(Some(NO_SPACE), "critical")));
        assert!(!known(&quota(None, "")), "no remaining and no state say nothing");
    }

    #[test]
    fn a_too_big_reason_says_what_it_needs_and_what_is_free() {
        assert_eq!(parse_too_big(&too_big(300, 20)), Some((300, 20)));
        assert!(waits(Some(&too_big(1, 0))) && waits(Some(WAITING)));
        assert!(!waits(Some("quota-exceeded")) && !waits(None));
    }
}

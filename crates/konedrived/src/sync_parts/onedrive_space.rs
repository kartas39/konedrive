//! Whether OneDrive is full, as the plan is told and as a refusal for lack of space is
//! read.
//!
//! **State owned:** whether the account's OneDrive is full, and when the quota is read
//! next while it is. Changed by a quota read and by a 507, through the functions here;
//! what waits for space per file is the step memory's (`SpaceRefusal`), not kept here.
//! It is the ONE owner of "OneDrive is full": the plan is given it as a value, an upload
//! asks it between two fragments, the bus reads it. It is not kept across a restart: the
//! folder's start reads the quota once before the first plan (`sync_tasks::start_order`).
//!
//! **Answers for:** the plan's `is_onedrive_full`; what is not sent: content, while
//! OneDrive is full, moves and removals going on (`docs/design/writes.md` §4.7);
//! `UploadQueue.QuotaFull`.
//!
//! **Tested without the rest** (`refuse_for_space` over wiremock directly; no fixture):
// test: a_quota_that_says_exceeded_or_less_than_a_mebibyte_left_is_full
// test: a_quota_that_says_nothing_of_the_space_changes_nothing
// test: a_507_with_space_left_is_the_files_own_refusal_and_not_a_full_onedrive
// test: a_507_with_no_space_left_makes_onedrive_full
// test: a_quota_read_with_space_again_ends_full_and_says_how_much_is_free
// test: a_refresh_reads_the_quota_and_a_due_read_waits_after_one_that_failed

use konedrive_graph::drive::{DriveClient, DriveQuota};

use crate::account::quota::Quota;
use crate::sync_parts::step_outcome::StepRefusal;

/// What a quota read changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpaceChange {
    /// The read says nothing of the space: nothing changed.
    NotKnown,
    /// OneDrive is full from now on.
    BecameFull,
    /// OneDrive is full, as before.
    StillFull,
    /// OneDrive has `free_space` bytes free: what waited for space is asked again
    /// (`StepMemory::forget_fitting_space_refusals`).
    HasSpace { free_space: u64, was_full: bool },
}

/// Less free space than this is none (a guess: the smallest file still goes, a real one
/// does not).
const LEAST_FREE_SPACE: u64 = 1 << 20;

/// While OneDrive is full, or while a file waits as too big, the quota is read again this
/// often.
const QUOTA_READ_INTERVAL: i64 = 30 * 60;

/// For how many seconds a read of the quota answers another refusal too: refusals of
/// steps that run together cost one request.
const QUOTA_READ_REUSE: i64 = 10;

/// Whether `quota` says OneDrive is full: its state is "exceeded", or less than a
/// mebibyte is left. `None` for a quota that says nothing of
/// the space: neither what is left nor a state. Pure.
pub(crate) fn decide_is_full(quota: &DriveQuota) -> Option<bool> {
    if quota.remaining.is_none() && quota.state.is_empty() {
        return None;
    }
    Some(quota.state == "exceeded" || quota.remaining.is_some_and(|free_space| free_space < LEAST_FREE_SPACE))
}

/// The space of one account's OneDrive, as its folder's sync sees it.
pub(crate) struct OneDriveSpace {
    /// The account's one quota, which this reads and adjusts and keeps no copy of.
    quota: Quota,
    is_full: std::sync::atomic::AtomicBool,
    /// Whether the quota applied here last said what is left (`DriveQuota::remaining`);
    /// `None` until a quota that says something of the space was applied here: before
    /// that the account's figures are not this folder's to go by. One that gave a state
    /// and no `remaining` leaves the account's figure at what an earlier read said, which
    /// is not what this read said: nothing is taken to be free by it.
    is_remaining_applied: std::sync::Mutex<Option<bool>>,
    /// Unix seconds of the next read while full or while a file waits as too big.
    next_read_at: std::sync::Mutex<Option<i64>>,
}

/// Whether the plan's task reads the quota before the plan it is about to make, at `now`.
/// A refresh the user asked for reads it, whatever was read before: the task is the one
/// reader of a running sync's quota, so a refresh is one read. Else it is read when its
/// next read is due (`OneDriveSpace::find_next_read`: while OneDrive is full, or a file
/// waits as too big) — and not within `retry_wait` seconds of a read that failed or said
/// nothing of the space (`last_failed_read_at`). Pure.
pub(crate) fn decide_is_quota_read(is_refresh_asked: bool, next_read_at: Option<i64>, last_failed_read_at: Option<i64>, retry_wait: i64, now: i64) -> bool {
    let is_read_due = next_read_at.is_some_and(|at| at <= now);
    let is_retry_due = last_failed_read_at.is_none_or(|failed_at| now.saturating_sub(failed_at) >= retry_wait);
    is_refresh_asked || (is_read_due && is_retry_due)
}

impl OneDriveSpace {
    /// What `quota`, which says something of the space, changes: whether OneDrive is
    /// full, and when the quota is read next.
    fn decide_change(&self, quota: &DriveQuota, is_full: bool, now: i64) -> SpaceChange {
        let was_full = self.is_full.swap(is_full, std::sync::atomic::Ordering::SeqCst);
        *crate::panic::lock(&self.is_remaining_applied) = Some(quota.remaining.is_some());
        let mut next_read_at = crate::panic::lock(&self.next_read_at);
        if is_full || next_read_at.is_some() {
            *next_read_at = Some(now + QUOTA_READ_INTERVAL);
        }
        match (is_full, was_full) {
            (true, false) => SpaceChange::BecameFull,
            (true, true) => SpaceChange::StillFull,
            (false, was_full) => SpaceChange::HasSpace { free_space: quota.remaining.unwrap_or(0), was_full },
        }
    }

    /// OneDrive refused content for lack of space and the quota says nothing else, or
    /// could not be read: it is full until a read says otherwise, and that read comes.
    fn turn_full(&self, now: i64) {
        self.is_full.store(true, std::sync::atomic::Ordering::SeqCst);
        *crate::panic::lock(&self.next_read_at) = Some(now + QUOTA_READ_INTERVAL);
    }

    /// Not full, nothing read yet. Made by the folder's start, with the account's quota.
    pub fn new(quota: Quota) -> Self {
        OneDriveSpace { quota, is_full: std::sync::atomic::AtomicBool::new(false), is_remaining_applied: std::sync::Mutex::new(None), next_read_at: std::sync::Mutex::new(None) }
    }

    /// Whether OneDrive is full. Called by the plan's task for `is_onedrive_full`, and by
    /// the queue's answers for `QuotaFull`.
    pub(crate) fn is_full(&self) -> bool {
        self.is_full.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A quota was read, by this folder or by the account: what it changed. Called by
    /// WHOEVER read the quota, with what it read: the plan's task for its own read (one
    /// that is due, and the one of a refresh the user asked for: [`decide_is_quota_read`]),
    /// the folder's start (`sync_tasks::start_order::read_quota_before_first_plan`), and
    /// the owner of the folder's state for the account's read
    /// (`AccountFolder::quota_read`). A caller that is not the plan's task then wakes it
    /// (`WakeCause::QuotaRead`); the task asks nothing of OneDrive on that cause: it takes
    /// what is free from here ([`find_free_space`](Self::find_free_space)), calls
    /// `StepMemory::forget_fitting_space_refusals` with it and makes the plan again.
    ///
    /// The read is written into the account's quota here, as made at `now`, whoever made
    /// it: one path for the folder's own reads and the account's. For a read the account
    /// wrote already this writes the same figures again, and what was uploaded between
    /// that read and this call comes back onto what is left until the next read. A quota
    /// that says nothing of the space is written nowhere. `HasSpace.free_space` is 0 when
    /// Graph gave a state and no `remaining`: nothing that waits is taken to fit.
    pub(crate) fn apply_quota(&self, quota: &DriveQuota, now: i64) -> SpaceChange {
        let Some(is_full) = decide_is_full(quota) else { return SpaceChange::NotKnown };
        self.quota.read_at(quota, now);
        self.decide_change(quota, is_full, now)
    }

    /// OneDrive answered 507 to content of `size` bytes: reads the quota at once
    /// (`DriveClient::quota`; a read less
    /// than 10 s old is used again) and answers the refusal: with space left the file
    /// alone is too big (`free_space` given), with none OneDrive is full. Called by the
    /// steps that send content.
    ///
    /// A quota that cannot be read, or that says nothing of the space, counts as full:
    /// OneDrive just said so, and the read in 30 minutes decides. `size` decides nothing
    /// here (the caller keeps it with the refusal, in the step memory); it is in the
    /// journal's line. No lock is held across the request, so two refusals that come at
    /// the same instant may each ask once; the second of two that come one after the
    /// other uses the first's read.
    pub(crate) async fn refuse_for_space(&self, drive: &DriveClient, size: u64, now: i64) -> StepRefusal {
        // The account's read of a moment ago, when it says something of the space, was
        // applied when it was made; a new one is applied here.
        let reused = self.quota.read_within(QUOTA_READ_REUSE).filter(|quota| decide_is_full(quota).is_some());
        let quota = match reused {
            Some(quota) => Some(quota),
            None => match drive.quota().await {
                Ok(quota) if decide_is_full(&quota).is_some() => {
                    self.quota.read_at(&quota, now);
                    Some(quota)
                }
                Ok(_) => {
                    tracing::warn!("OneDrive gave no quota after it refused {size} bytes for lack of space");
                    None
                }
                Err(error) => {
                    tracing::warn!("cannot read the OneDrive quota after it refused {size} bytes for lack of space: {error}");
                    None
                }
            },
        };
        match quota {
            Some(quota) if decide_is_full(&quota) == Some(false) => {
                // The file alone is too big: the quota is read again while it waits.
                self.decide_change(&quota, false, now);
                *crate::panic::lock(&self.next_read_at) = Some(now + QUOTA_READ_INTERVAL);
                StepRefusal::NoSpace { free_space: Some(quota.remaining.unwrap_or(0)) }
            }
            // No space left, or a quota that cannot be read: OneDrive just said it is full.
            _ => {
                self.turn_full(now);
                StepRefusal::NoSpace { free_space: None }
            }
        }
    }

    /// An upload of `bytes` was committed: they come off what the account's quota says is
    /// left, until the next read (`Quota::uploaded`).
    /// Called by the steps that send content, after their commit was written.
    pub(crate) fn count_uploaded(&self, bytes: u64) {
        self.quota.uploaded(bytes);
    }

    /// How many bytes OneDrive has free, as the last quota applied here said
    /// ([`apply_quota`](Self::apply_quota), [`refuse_for_space`](Self::refuse_for_space)),
    /// less what was uploaded since (`Quota::uploaded`), and 0 when that quota gave a
    /// state and no `remaining`, as its `HasSpace.free_space`; `None` while OneDrive is full,
    /// and before any quota that says something of the space was applied. What the plan's
    /// task forgets the fitting refusals for lack of space by after
    /// `WakeCause::QuotaRead`: the one who read the quota applied it and gave the task no
    /// figures.
    pub(crate) fn find_free_space(&self) -> Option<u64> {
        if self.is_full() {
            return None;
        }
        match *crate::panic::lock(&self.is_remaining_applied) {
            None => None,
            // As `SpaceChange::HasSpace::free_space` of that quota: nothing that waits is taken to fit.
            Some(false) => Some(0),
            // The account's figure is that read's, less what went up since.
            Some(true) => Some(self.quota.state().get().quota.remaining),
        }
    }

    /// When the quota is to be read next by itself, Unix seconds: every 30 minutes while
    /// full or while a file waits as too big. Called by the plan's task for its timer.
    ///
    /// Whether a file still waits as too big is the step memory's to say, not known here:
    /// the time is set by every read while full and by every 507, and kept by a read that
    /// finds space once it was set, since that space may still be too little for the file
    /// that waits. It is `None` only for a OneDrive that was never full and refused
    /// nothing since the folder's start.
    pub(crate) fn find_next_read(&self) -> Option<i64> {
        *crate::panic::lock(&self.next_read_at)
    }
}

#[cfg(test)]
mod tests;

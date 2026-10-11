//! Whether OneDrive is full, as the plan is told and as a refusal for lack of space is
//! read.
//!
//! **State owned:** whether the account's OneDrive is full, and when the quota is read
//! next while it is. Changed by a quota read and by a 507, through the functions here;
//! what waits for space per file is the step memory's (`SpaceRefusal`), not kept here.
//! Today: `space.rs`'s `Space` and the rows' `waiting-for-space`.
//! It is the ONE owner of "OneDrive is full": the plan is given it as a value, an upload
//! asks it between two fragments, the bus reads it. It is not kept across a restart (the
//! map's C3): the folder's start reads the quota once before the first plan
//! (`sync_tasks::start_order`).
//!
//! **Answers for:** the plan's `is_onedrive_full`; design §6, "what is not sent: content,
//! while OneDrive is full (moves and removals go on)"; `UploadQueue.QuotaFull`.
//!
//! **Tested without the rest** (`refuse_for_space` over wiremock directly, as today's
//! `upload/space` tests; no fixture):
// test: a_quota_that_says_exceeded_or_less_than_a_mebibyte_left_is_full
// test: a_quota_that_says_nothing_of_the_space_changes_nothing
// test: a_507_with_space_left_is_the_files_own_refusal_and_not_a_full_onedrive
// test: a_507_with_no_space_left_makes_onedrive_full
// test: a_quota_read_with_space_again_ends_full_and_says_how_much_is_free
#![allow(dead_code, unused_variables)]

use konedrive_graph::drive::{DriveClient, DriveQuota};

use crate::account::quota::Quota;
use crate::sync_parts::step_outcome::StepRefusal;

/// What a quota read changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceChange {
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
/// mebibyte is left (`space.rs:62`, `no_space`). `None` for a quota that says nothing of
/// the space: neither what is left nor a state. Pure.
pub fn decide_is_full(quota: &DriveQuota) -> Option<bool> {
    if quota.remaining.is_none() && quota.state.is_empty() {
        return None;
    }
    Some(quota.state == "exceeded" || quota.remaining.is_some_and(|free_space| free_space < LEAST_FREE_SPACE))
}

/// The space of one account's OneDrive, as its folder's sync sees it.
pub struct OneDriveSpace {
    /// The account's one quota, which this reads and adjusts and keeps no copy of.
    quota: Quota,
    is_full: std::sync::atomic::AtomicBool,
    /// Unix seconds of the next read while full or while a file waits as too big.
    next_read_at: std::sync::Mutex<Option<i64>>,
}

impl OneDriveSpace {
    /// What `quota`, which says something of the space, changes: whether OneDrive is
    /// full, and when the quota is read next.
    fn decide_change(&self, quota: &DriveQuota, is_full: bool, now: i64) -> SpaceChange {
        let was_full = self.is_full.swap(is_full, std::sync::atomic::Ordering::SeqCst);
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
        OneDriveSpace { quota, is_full: std::sync::atomic::AtomicBool::new(false), next_read_at: std::sync::Mutex::new(None) }
    }

    /// Whether OneDrive is full. Called by the plan's task for `is_onedrive_full`, and by
    /// the queue's answers for `QuotaFull`.
    pub fn is_full(&self) -> bool {
        self.is_full.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A quota was read, by this folder or by the account: what it changed. Called by the
    /// plan's task, which then calls `StepMemory::forget_fitting_space_refusals` for
    /// `HasSpace` and makes the plan again
    /// (`WakeCause::QuotaRead`). Replaces `space.rs:234`, `:247` (`apply_quota`,
    /// `decide_quota`) and `:279` (`release_fitting`).
    ///
    /// The read is written into the account's quota here, as made at `now`, whoever made
    /// it: one path for the folder's own reads and the account's. For a read the account
    /// wrote already this writes the same figures again, and what was uploaded between
    /// that read and this call comes back onto what is left until the next read. A quota
    /// that says nothing of the space is written nowhere. `HasSpace.free_space` is 0 when
    /// Graph gave a state and no `remaining`: nothing that waits is taken to fit.
    pub fn apply_quota(&self, quota: &DriveQuota, now: i64) -> SpaceChange {
        let Some(is_full) = decide_is_full(quota) else { return SpaceChange::NotKnown };
        self.quota.read_at(quota, now);
        self.decide_change(quota, is_full, now)
    }

    /// OneDrive answered 507 to content of `size` bytes: reads the quota at once
    /// (`DriveClient::quota`, `crates/konedrive-graph/src/drive/mod.rs:194`; a read less
    /// than 10 s old is used again) and answers the refusal: with space left the file
    /// alone is too big (`free_space` given), with none OneDrive is full. Called by the
    /// steps that send content. Replaces `space.rs:201` (`space_refused`).
    ///
    /// A quota that cannot be read, or that says nothing of the space, counts as full:
    /// OneDrive just said so, and the read in 30 minutes decides. `size` decides nothing
    /// here (the caller keeps it with the refusal, in the step memory); it is in the
    /// journal's line. No lock is held across the request, so two refusals that come at
    /// the same instant may each ask once; the second of two that come one after the
    /// other uses the first's read.
    pub async fn refuse_for_space(&self, drive: &DriveClient, size: u64, now: i64) -> StepRefusal {
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
    /// left, until the next read (`Quota::uploaded`; today `space.rs:149`, `space_used`).
    /// Called by the steps that send content, after their commit was written.
    pub fn count_uploaded(&self, bytes: u64) {
        self.quota.uploaded(bytes);
    }

    /// When the quota is to be read next by itself, Unix seconds: every 30 minutes while
    /// full or while a file waits as too big. Called by the plan's task for its timer.
    ///
    /// Whether a file still waits as too big is the step memory's to say, not known here:
    /// the time is set by every read while full and by every 507, and kept by a read that
    /// finds space once it was set, since that space may still be too little for the file
    /// that waits. It is `None` only for a OneDrive that was never full and refused
    /// nothing since the folder's start.
    pub fn find_next_read(&self) -> Option<i64> {
        *crate::panic::lock(&self.next_read_at)
    }
}

#[cfg(test)]
mod tests;

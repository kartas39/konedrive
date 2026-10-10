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
//! (`sync::sync_tasks::start_order`).
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
use crate::folder::step_outcome::StepRefusal;

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

/// Whether `quota` says OneDrive is full: its state is "exceeded", or less than a
/// mebibyte is left (`space.rs:62`, `no_space`). Pure.
pub fn decide_is_full(quota: &DriveQuota) -> Option<bool> {
    todo!()
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
    /// Not full, nothing read yet. Made by the folder's start, with the account's quota.
    pub fn new(quota: Quota) -> Self {
        todo!()
    }

    /// Whether OneDrive is full. Called by the plan's task for `is_onedrive_full`, and by
    /// the queue's answers for `QuotaFull`.
    pub fn is_full(&self) -> bool {
        todo!()
    }

    /// A quota was read, by this folder or by the account: what it changed. Called by the
    /// plan's task, which then calls `StepMemory::forget_fitting_space_refusals` for
    /// `HasSpace` and makes the plan again
    /// (`WakeCause::QuotaRead`). Replaces `space.rs:234`, `:247` (`apply_quota`,
    /// `decide_quota`) and `:279` (`release_fitting`).
    pub fn apply_quota(&self, quota: &DriveQuota, now: i64) -> SpaceChange {
        todo!()
    }

    /// OneDrive answered 507 to content of `size` bytes: reads the quota at once
    /// (`DriveClient::quota`, `crates/konedrive-graph/src/drive/mod.rs:194`; a read less
    /// than 10 s old is used again) and answers the refusal: with space left the file
    /// alone is too big (`free_space` given), with none OneDrive is full. Called by the
    /// steps that send content. Replaces `space.rs:201` (`space_refused`).
    pub async fn refuse_for_space(&self, drive: &DriveClient, size: u64, now: i64) -> StepRefusal {
        todo!()
    }

    /// When the quota is to be read next by itself, Unix seconds: every 30 minutes while
    /// full or while a file waits as too big. Called by the plan's task for its timer.
    pub fn find_next_read(&self) -> Option<i64> {
        todo!()
    }
}

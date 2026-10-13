//! Whether the download of a pinned file leaves the free space the account keeps on the
//! filesystem the backing directory is on (`free_space_reserve_gb` of its section of
//! `config.toml`): what the plan's task asks before it starts the downloads of a plan.
//!
//! **State owned:** none. The free space is measured anew for every choice, the reserve
//! is the account's setting as it is then, and what the downloads that run still take is
//! counted from the steps that run (`RunningStep::total` of the class
//! `StepClass::Download`).
//!
//! **What is held.** The downloads are taken in the plan's order: one whose content fits
//! in what is free beyond the reserve and beyond what the downloads before it take is
//! started, one that does not is left, and a smaller one after it is still started. A
//! download that runs counts with its whole size although part of it is on disk already
//! and counted in what is free no more: nothing here knows how far it got. So a file may
//! wait until the downloads that run ended although it fitted beside them. A read of a
//! file and `Files.Hydrate` are not asked: the program that reads is answered `ENOSPC`
//! by the disk itself.
//!
//! **When it is asked again.** With every plan and every round that starts steps, and
//! `SPACE_CHECKED_EVERY` seconds after a plan that left a download for lack of space:
//! nothing says that space was freed on the filesystem.
//!
//! **Tested without the rest:**
// test: every_download_that_fits_beyond_the_reserve_is_started
// test: a_download_that_does_not_fit_is_held_and_a_smaller_one_after_it_is_not
// test: the_downloads_that_run_count_with_their_whole_size
// test: with_less_free_than_the_reserve_every_download_with_content_is_held
// test: an_empty_file_is_never_held
// test: free_space_that_cannot_be_measured_holds_nothing
// test: the_shortage_counts_the_held_downloads_and_none_is_no_shortage
// test: a_download_that_would_not_leave_the_free_space_kept_is_held_and_said (`sync_plan_task/tests.rs`)

use std::collections::BTreeSet;

use konedrive_tree::{LocalItemId, SyncStep};

use super::step_choice::find_download_size;
use crate::status::snapshot::DiskSpaceShortage;
use crate::sync_parts::running_steps::{RunningStep, StepClass};

/// The steps of a plan, with the downloads of pinned files that wait for space taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadsHeldForSpace {
    /// What is left of the plan's steps, in its order: what may start.
    pub(crate) steps: Vec<SyncStep>,
    /// The downloads taken out, in the plan's order.
    pub(crate) held: Vec<SyncStep>,
    /// What is said of them; `None` when none is held.
    pub(crate) shortage: Option<DiskSpaceShortage>,
}

/// Takes out of `planned` the downloads of pinned files that would not leave `reserve`
/// bytes of `free_space` ([`decide_downloads_held_for_space`]) beside the downloads of
/// `running`. A download whose item has a step running is not asked: it is not started
/// anyway, and counts as running. Pure.
pub(crate) fn hold_downloads_for_space(planned: Vec<SyncStep>, running: &[RunningStep], free_space: Option<u64>, reserve: u64) -> DownloadsHeldForSpace {
    let rows_with_running_step: BTreeSet<LocalItemId> = running.iter().map(|step| step.item).collect();
    let size_of = |step: &SyncStep| find_download_size(step).filter(|_| !rows_with_running_step.contains(&step.local_item_id()));
    let sizes: Vec<u64> = planned.iter().filter_map(size_of).collect();
    let running_bytes = running.iter().filter(|step| step.class == StepClass::Download).map(|step| step.total).sum();
    let is_held = decide_downloads_held_for_space(&sizes, running_bytes, free_space, reserve);
    let shortage = describe_shortage(&sizes, &is_held, free_space, reserve);
    let mut is_held = is_held.into_iter();
    let (steps, held) = planned.into_iter().partition(|step| !(size_of(step).is_some() && is_held.next().unwrap_or(false)));
    DownloadsHeldForSpace { steps, held, shortage }
}

/// How long after a plan that left a download for lack of space the next plan is made,
/// in seconds.
pub(crate) const SPACE_CHECKED_EVERY: i64 = 60;

/// The bytes of `gigabytes` as the setting counts them.
pub(crate) fn count_reserve_bytes(gigabytes: u64) -> u64 {
    gigabytes.saturating_mul(1 << 30)
}

/// Which of the downloads of `sizes` (the sizes of their contents, in the plan's order)
/// are held for lack of space: `true` at the position of each that is. `free_space` is
/// what the filesystem has free for the daemon, `None` when it could not be measured;
/// `reserve` what of it is kept free; `running_bytes` what the downloads that run take.
/// Pure.
pub(crate) fn decide_downloads_held_for_space(sizes: &[u64], running_bytes: u64, free_space: Option<u64>, reserve: u64) -> Vec<bool> {
    let Some(free_space) = free_space else { return vec![false; sizes.len()] };
    let mut left = free_space.saturating_sub(reserve).saturating_sub(running_bytes);
    sizes
        .iter()
        .map(|size| {
            if *size <= left {
                left -= size;
                false
            } else {
                true
            }
        })
        .collect()
}

/// What is said of the downloads of `sizes` of which `held` are held
/// ([`decide_downloads_held_for_space`]): how many wait and for how many bytes, with the
/// free space and the reserve they were measured against; `None` when none is held. Pure.
pub(crate) fn describe_shortage(sizes: &[u64], held: &[bool], free_space: Option<u64>, reserve: u64) -> Option<DiskSpaceShortage> {
    let waiting: Vec<u64> = sizes.iter().zip(held).filter(|(_, is_held)| **is_held).map(|(size, _)| *size).collect();
    let free_space = free_space?;
    (!waiting.is_empty()).then(|| DiskSpaceShortage { count: u32::try_from(waiting.len()).unwrap_or(u32::MAX), bytes: waiting.iter().sum(), free_space, reserve })
}

#[cfg(test)]
mod tests;

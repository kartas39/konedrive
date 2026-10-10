//! A full listing that was asked for and has not begun, kept across a stop of the daemon.
//!
//! **State owned:** one key of `meta`. Set by [`ItemStore::request_full_listing`]; taken
//! away by `begin_full_listing`, in its transaction (the one line that module gains when
//! this is filled in). Today the request is a flag in memory (`remote/listing.rs:207-218`)
//! and a cycle that failed asks again at the next start because every start does.
//!
//! **Answers for:** the plan's `PlanFault::FullListingNeeded` and a listing cycle that
//! failed part-way reaching the next cycle, also after a restart.
//!
//! **Tested without the rest:**
// test: a_full_listing_that_was_asked_for_is_still_asked_for_after_the_store_is_opened_again
// test: beginning_a_full_listing_takes_the_request_away
// test: beginning_a_listing_of_changes_leaves_the_request
#![allow(dead_code, unused_variables)]

use super::ItemStore;
use crate::model::LocalItemId;
use crate::TreeError;

/// Why a full listing is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullListingCause {
    /// The plan found a row whose OneDrive side cannot be judged, with no listing under
    /// way that would say it (`PlanFault::FullListingNeeded`): which item.
    PlanFault(LocalItemId),
    /// The user took removals back (`RestoreDeletes`), or asked for it (`SyncAnyway`).
    UserAsked,
    /// The check at start bound an entry by a mark no row had: OneDrive's version of it
    /// is not known (design §10, "a store that was lost").
    BoundByMarkAtStart,
}

impl ItemStore<'_> {
    /// Asks that the next listing be a full one. Called by the plan's task for a fault of
    /// the plan, by `RestoreDeletes`, and by the start of a folder whose check bound
    /// entries by their marks. One transaction; asking twice is asking once. The cause is
    /// for the journal. MISSING-1.d.
    pub fn request_full_listing(&mut self, cause: FullListingCause) -> Result<(), TreeError> {
        todo!()
    }
}

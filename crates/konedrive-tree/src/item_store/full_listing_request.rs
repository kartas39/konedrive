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
use rusqlite::Connection;

use super::ItemStore;
use crate::model::LocalItemId;
use crate::{meta, TreeError};

/// Why a full listing is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullListingCause {
    /// The plan found a row whose OneDrive side cannot be judged, with no listing under
    /// way that would say it (`PlanFault::FullListingNeeded`): which item.
    PlanFault(LocalItemId),
    /// The user asked for it (`SyncAnyway`).
    UserAsked,
    /// The check at start bound an entry by a mark no row had: OneDrive's version of it
    /// is not known (design §10, "a store that was lost").
    BoundByMarkAtStart,
}

/// Whether a full listing was asked for and has not begun.
pub(super) fn is_full_listing_requested(conn: &Connection) -> Result<bool, TreeError> {
    Ok(meta::get(conn, meta::FULL_LISTING_REQUESTED)?.is_some())
}

/// A full listing begins: what was asked for is under way. Called by the beginning of a
/// full listing, in its transaction.
pub(super) fn forget_full_listing_request(conn: &Connection) -> Result<(), TreeError> {
    meta::set(conn, meta::FULL_LISTING_REQUESTED, None)
}

impl FullListingCause {
    /// The cause in words, for the journal: what the store keeps while the request stands.
    fn describe(self) -> String {
        match self {
            Self::PlanFault(item) => format!("the plan cannot judge item {item}"),
            Self::UserAsked => "the user asked".to_owned(),
            Self::BoundByMarkAtStart => "the check at start bound an entry by its mark".to_owned(),
        }
    }
}

impl ItemStore<'_> {
    /// Asks that the next listing be a full one. Called by the plan's task for a fault of
    /// the plan, and by the start of a folder whose check bound
    /// entries by their marks. One transaction; asking twice is asking once, and the
    /// first cause is the one kept. The cause is for the journal. Asked for while a full
    /// listing is under way, it is the next one that is asked for. MISSING-1.d.
    pub fn request_full_listing(&mut self, cause: FullListingCause) -> Result<(), TreeError> {
        self.write(|tx| {
            if is_full_listing_requested(tx)? {
                return Ok(());
            }
            tracing::info!("a full listing is asked for: {}", cause.describe());
            meta::set(tx, meta::FULL_LISTING_REQUESTED, Some(&cause.describe()))
        })
    }
}

#[cfg(test)]
mod tests;

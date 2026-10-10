//! The removals this client sent a short time ago: how many items they took from OneDrive,
//! counted since the window of that time began.
//!
//! **State owned:** two keys of `meta` — how many items the removals took that OneDrive
//! confirmed to a step of this client since the window began, and the count of
//! confirmations at which it began. Written here and nowhere else: the commit of a removal
//! that is written adds what it took ([`add_sent_removals`]), in its transaction, and the
//! caller begins the window anew ([`ItemStore::begin_sent_removal_window`]). No row is
//! counted: the row of a confirmed removal goes when a listing ends that began after it,
//! and a row another client's removal left was stamped by some earlier step. Both are
//! outside what was asked — what this client removed within the window.
//!
//! **Answers for:** the guard against a mass removal counting the removals that come one
//! by one, with a plan and a listing between them, like those of one plan
//! ([`SyncPlanInput::sent_removal_count`](crate::SyncPlanInput::sent_removal_count)).

use rusqlite::{Connection, Transaction};

use super::onedrive_versions::read_confirmation_count;
use super::ItemStore;
use crate::{meta, TreeError};

/// The removals this client sent since its window began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentRemovalWindow {
    /// The count of confirmations
    /// ([`ItemStore::confirmation_count`]) at which the window began; 0 in a store in
    /// which none was begun.
    pub began_at_count: u64,
    /// How many items the removals took from OneDrive that OneDrive confirmed to a step of
    /// this client since then: a removed file, a removed directory, and each item that
    /// was removed with a directory.
    pub removal_count: u64,
}

impl ItemStore<'_> {
    /// The removals this client sent since their window began: what the plan is given as
    /// [`SyncPlanInput::sent_removal_count`](crate::SyncPlanInput::sent_removal_count).
    /// Kept by the store, whatever listing ended meanwhile, and across a stop.
    pub fn find_sent_removal_window(&self) -> Result<SentRemovalWindow, TreeError> {
        find_sent_removal_window(&self.store.conn)
    }

    /// Begins the window anew: no removal was sent in it yet, and it begins at the count
    /// of confirmations as it is now. The caller does that when the time it takes for
    /// "a short time ago" has passed with no removal sent, and when the removals of a mass
    /// removal the user confirmed have been sent — else the next single removal would be
    /// held as one more of them.
    pub fn begin_sent_removal_window(&mut self) -> Result<SentRemovalWindow, TreeError> {
        self.write(|tx| {
            let window = SentRemovalWindow { began_at_count: read_confirmation_count(tx)?, removal_count: 0 };
            write_sent_removal_window(tx, window)?;
            Ok(window)
        })
    }
}

fn read_count(conn: &Connection, key: &str) -> Result<u64, TreeError> {
    Ok(meta::get(conn, key)?.and_then(|count| count.parse().ok()).unwrap_or(0))
}

pub(super) fn find_sent_removal_window(conn: &Connection) -> Result<SentRemovalWindow, TreeError> {
    Ok(SentRemovalWindow { began_at_count: read_count(conn, meta::SENT_REMOVAL_WINDOW_BEGAN_AT_COUNT)?, removal_count: read_count(conn, meta::SENT_REMOVAL_COUNT)? })
}

fn write_sent_removal_window(tx: &Transaction<'_>, window: SentRemovalWindow) -> Result<(), TreeError> {
    meta::set(tx, meta::SENT_REMOVAL_WINDOW_BEGAN_AT_COUNT, Some(&window.began_at_count.to_string()))?;
    meta::set(tx, meta::SENT_REMOVAL_COUNT, Some(&window.removal_count.to_string()))
}

/// OneDrive confirmed to a step of this client a removal that took `removal_count` items:
/// they are counted in the window. Inside the transaction of that step's commit.
pub(super) fn add_sent_removals(tx: &Transaction<'_>, removal_count: u64) -> Result<(), TreeError> {
    let counted = read_count(tx, meta::SENT_REMOVAL_COUNT)? + removal_count;
    meta::set(tx, meta::SENT_REMOVAL_COUNT, Some(&counted.to_string()))
}

//! The removals this client sent a short time ago: how many items they took from OneDrive,
//! counted while one follows another within that time.
//!
//! **State owned:** two keys of `meta` — how many items the removals took that OneDrive
//! confirmed to a step of this client in the window, and the time of the last of them.
//! Written here and nowhere else: the commit of a removal that is written adds what it
//! took ([`add_sent_removals`]), in its transaction, with the time its caller gives; and
//! the user's confirmation of a mass removal empties the window
//! ([`ItemStore::begin_sent_removal_window`]). No row is counted: the row of a confirmed
//! removal goes when a listing ends that began after it, and a row another client's
//! removal left was stamped by some earlier step. Both are outside what was asked — what
//! this client removed within the window.
//!
//! **The window ends by itself**: once [`SENT_REMOVAL_WINDOW_SECONDS`] have passed since
//! the last removal counted, nothing is counted any more, whoever asks and whether or not
//! anybody ever began a window ([`SentRemovalWindow::is_over_at`]). The store has no clock:
//! the time is the caller's, given with each removal's commit and with each reading, as
//! the plan is given its `now`. Nothing but a time tells the two halves of one `rm -r`,
//! sent by two plans with a listing between them, from a single removal a day later:
//! between the two, in both, nothing is unsent, a listing ended and the same commits were
//! written. The time counted from is that of the last removal, not of the first: a removal
//! that takes longer than the window to arrive is still one, and the cost of that choice
//! is that removals which keep coming, each within the window of the one before, add up
//! until the user confirms them.
//!
//! **Answers for:** the guard against a mass removal counting the removals that come one
//! by one, with a plan and a listing between them, like those of one plan
//! ([`SyncPlanInput::sent_removal_count`](crate::SyncPlanInput::sent_removal_count)).

use rusqlite::{Connection, Transaction};

use super::ItemStore;
use crate::{meta, TreeError};

/// How long after the last removal this client sent the removals before it are still
/// "sent a short time ago", in seconds.
pub const SENT_REMOVAL_WINDOW_SECONDS: i64 = 600;

/// The removals this client sent a short time ago.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentRemovalWindow {
    /// The time of the last removal counted, in Unix seconds as the commit's caller gave
    /// it; `None` when none is counted.
    pub last_removal_at: Option<i64>,
    /// How many items the removals of the window took from OneDrive that OneDrive
    /// confirmed to a step of this client: a removed file, a removed directory, and each
    /// item that was removed with a directory.
    pub removal_count: u64,
}

impl SentRemovalWindow {
    /// The window in which nothing is counted.
    pub const EMPTY: Self = Self { last_removal_at: None, removal_count: 0 };

    /// Whether the window is over at the time `now`, in Unix seconds: nothing is counted
    /// in it, or [`SENT_REMOVAL_WINDOW_SECONDS`] have passed since the last removal
    /// counted. A `now` before that removal — a clock that was set back — is within the
    /// window: more is held, never less.
    pub fn is_over_at(&self, now: i64) -> bool {
        self.last_removal_at.is_none_or(|last_removal_at| now.saturating_sub(last_removal_at) >= SENT_REMOVAL_WINDOW_SECONDS)
    }
}

impl ItemStore<'_> {
    /// The removals this client sent a short time ago, as of the time `now`, in Unix
    /// seconds: its `removal_count` is what the plan is given as
    /// [`SyncPlanInput::sent_removal_count`](crate::SyncPlanInput::sent_removal_count),
    /// with the same `now`. [`SentRemovalWindow::EMPTY`] once the window is over. Kept by
    /// the store, whatever listing ended meanwhile, and across a stop.
    pub fn find_sent_removal_window(&self, now: i64) -> Result<SentRemovalWindow, TreeError> {
        find_sent_removal_window(&self.store.conn, now)
    }

    /// Empties the window: no removal is counted in it. Called when the user confirmed a
    /// mass removal, in the job that writes the confirmation: the removals they confirmed
    /// are sent within the window, and the next single removal is no part of them. Nothing
    /// else needs to begin a window: it ends by itself.
    pub fn begin_sent_removal_window(&mut self) -> Result<SentRemovalWindow, TreeError> {
        self.write(|tx| {
            write_sent_removal_window(tx, SentRemovalWindow::EMPTY)?;
            Ok(SentRemovalWindow::EMPTY)
        })
    }
}

/// The window as the keys have it, whatever the time is.
fn read_sent_removal_window(conn: &Connection) -> Result<SentRemovalWindow, TreeError> {
    let read = |key: &str| Ok::<_, TreeError>(meta::get(conn, key)?.and_then(|value| value.parse::<i64>().ok()));
    Ok(SentRemovalWindow { last_removal_at: read(meta::LAST_SENT_REMOVAL_AT)?, removal_count: read(meta::SENT_REMOVAL_COUNT)?.map_or(0, |count| count.max(0) as u64) })
}

pub(super) fn find_sent_removal_window(conn: &Connection, now: i64) -> Result<SentRemovalWindow, TreeError> {
    let window = read_sent_removal_window(conn)?;
    Ok(if window.is_over_at(now) { SentRemovalWindow::EMPTY } else { window })
}

fn write_sent_removal_window(tx: &Transaction<'_>, window: SentRemovalWindow) -> Result<(), TreeError> {
    meta::set(tx, meta::LAST_SENT_REMOVAL_AT, window.last_removal_at.map(|at| at.to_string()).as_deref())?;
    meta::set(tx, meta::SENT_REMOVAL_COUNT, Some(&window.removal_count.to_string()))
}

/// OneDrive confirmed to a step of this client, at the time `now`, a removal that took
/// `removal_count` items: they are counted in the window, with what the window held when
/// it is not over at `now`, and the window's last removal is of `now`. Inside the
/// transaction of that step's commit.
pub(super) fn add_sent_removals(tx: &Transaction<'_>, removal_count: u64, now: i64) -> Result<(), TreeError> {
    let window = find_sent_removal_window(tx, now)?;
    write_sent_removal_window(tx, SentRemovalWindow { last_removal_at: Some(now), removal_count: window.removal_count + removal_count })
}

#[cfg(test)]
mod tests;

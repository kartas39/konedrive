//! The folder's state of sending as the store keeps it across a restart: read-write, or
//! read-only with the stamp of the switch.
//!
//! **State owned:** one key of `meta`, the last stamp `unsent_since` that was given before
//! the folder was switched to read-only; absent while the folder is read-write. Written by
//! the two operations here and by nothing else. Today's mode in `config.toml` says what the
//! user wants; this says since when, which only the store's counter can.
//!
//! **Answers for:** design §6, "the folder's state of sending", and §13 (3): what was unsent
//! when the folder was switched is sent first, and nothing that came to be unsent later.
//!
//! **Tested without the rest:**
// test: a_store_that_was_never_switched_is_read_write
// test: the_switch_to_read_only_keeps_the_last_stamp_given_before_it
// test: a_second_switch_to_read_only_keeps_the_first_stamp
// test: the_switch_back_to_read_write_forgets_the_stamp
// test: the_stamp_of_the_switch_is_read_again_after_the_store_is_opened_again
use rusqlite::Connection;

use super::ItemStore;
use crate::sync_plan::SendingState;
use crate::{meta, TreeError};

/// The folder's state of sending as the store keeps it: read-only while the stamp of the
/// switch is kept, with that stamp. A stamp that cannot be read is the earliest one:
/// nothing that came to be unsent is taken for sent first by mistake.
pub(super) fn read_sending_state(conn: &Connection) -> Result<SendingState, TreeError> {
    Ok(match meta::get(conn, meta::SENDING_READ_ONLY_SINCE)? {
        Some(stamp) => SendingState::ReadOnly { last_unsent_since: stamp.parse().unwrap_or(0) },
        None => SendingState::ReadWrite,
    })
}

impl ItemStore<'_> {
    /// The folder is read-only from now on: keeps
    /// [`last_unsent_since`](Self::last_unsent_since) as the stamp of the switch and
    /// answers the state the plan is given. A folder that is read-only already keeps the
    /// stamp it has. One transaction.
    ///
    /// Called by the switch of the access mode (`sync_tasks::access_mode_change`)
    /// after the mount's switch was awaited: from then on the mount writes only the last
    /// closes of files that were open, and a close gives no new stamp, so the stamp read
    /// here is the last one given before the switch. MISSING-1.c.
    pub fn switch_sending_to_read_only(&mut self) -> Result<SendingState, TreeError> {
        let last_unsent_since = self.last_unsent_since()?;
        self.write(|tx| {
            if meta::get(tx, meta::SENDING_READ_ONLY_SINCE)?.is_none() {
                meta::set(tx, meta::SENDING_READ_ONLY_SINCE, Some(&last_unsent_since.to_string()))?;
            }
            read_sending_state(tx)
        })
    }

    /// The folder is read-write again: the stamp of the switch is forgotten. Called by the
    /// switch of the access mode before the mount takes changes again. One transaction.
    pub fn switch_sending_to_read_write(&mut self) -> Result<SendingState, TreeError> {
        self.write(|tx| {
            meta::set(tx, meta::SENDING_READ_ONLY_SINCE, None)?;
            read_sending_state(tx)
        })
    }
}

#[cfg(test)]
mod tests;

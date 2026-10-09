//! What is shown of a file of OneDrive: online-only, being downloaded, or downloaded. It is
//! the value of the `user.konedrive.state` attribute the mount shows, and what the daemon's
//! bus interface says of a path.
//!
//! [`to_state_attribute`] is the one place that works it out, from the file's sync state and
//! from whether the file is being downloaded; whoever shows it asks here.

use konedrive_fs::placeholder::State;

use super::directory_contents::FileSyncState;

/// The state attribute of a file in `sync_state`, which is being downloaded whole when
/// `is_downloading`. `None` for a file that is not in OneDrive yet: it has none.
pub(super) fn to_state_attribute(sync_state: &FileSyncState, is_downloading: bool) -> Option<State> {
    match sync_state {
        FileSyncState::LocalOnly(_) => None,
        _ if is_downloading => Some(State::Hydrating),
        FileSyncState::Synced(..) | FileSyncState::ModifiedHere(..) => Some(State::Hydrated),
        FileSyncState::RemoteOnly(_) => Some(State::OnlineOnly),
    }
}

#[cfg(test)]
mod tests;

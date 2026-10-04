//! What the bus shows of the folder's state, worked out in one place: [`publish`] derives
//! `Path`, `State`, the registration's part of `LastError`, whether the folder waits for
//! the helper, and the [`View`] from a [`Folder`], and nothing else writes them.
//!
//! What the folder's sync says of itself (its trouble, its notes, the counters) is beside
//! this, in the snapshot, written by the parts that run; `status::snapshot` puts the two
//! together (`published_state`, `published_error`).

use super::folder::{Down, Folder, Interception, Is, Recovery, Standing, View};
use super::{RootSource, SyncService, NO_INTERCEPTION_WARNING};
use crate::status::snapshot::{RootState, SwitchNote, SyncSnapshot};

/// What a folder's state comes to on the bus, and for the readers.
#[derive(Debug, Clone)]
pub(super) struct Published {
    /// `Path`: the folder recorded, up or not; empty with none.
    pub path: String,
    /// `State`, before the helper and the sync have their say (`published_state`).
    pub state: RootState,
    /// The registration's part of `LastError`.
    pub error: String,
    /// Why a switch to interception failed, said right behind `error`.
    pub switch_note: Option<SwitchNote>,
    pub helper: Helper,
    pub view: View,
}

/// What the folder's state says of the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Helper {
    /// The folder is what it is without one.
    NotNeeded,
    /// The folder is intercepted, or shows OneDrive: it waits for the helper whenever
    /// there is no link.
    Needed,
    /// The folder waits for the helper whatever the link: it is brought up, or switched to
    /// interception, at the next connect.
    Waited,
}

/// What `folder` comes to. Pure: the same state always publishes the same.
pub(super) fn publish(folder: &Folder) -> Published {
    let path = folder.record().map(|record| record.root.path.display().to_string()).unwrap_or_default();
    // An account held back says so over whatever its folder is: nothing acts on the
    // folder but a Forget.
    if let Standing::HeldBack(why) = &folder.standing {
        return Published {
            path,
            state: RootState::Error,
            error: why.clone(),
            switch_note: None,
            helper: Helper::NotNeeded,
            view: View { record: None, down: None, wanted: folder.wanted },
        };
    }
    let view = |down: Option<String>| View { record: folder.acted_on().cloned(), down, wanted: folder.wanted };
    match &folder.is {
        Is::Absent => {
            Published { path, state: RootState::None, error: String::new(), switch_note: None, helper: Helper::NotNeeded, view: view(None) }
        }
        Is::Down(record, down) => {
            let error = down.why(&record.root);
            let (state, helper) = match down {
                // Nothing is known to be wrong: it is brought up in a moment, or when the
                // helper connects.
                Down::NotYetUp => (RootState::Waiting, Helper::NotNeeded),
                Down::WaitsForHelper => (RootState::Waiting, Helper::Waited),
                Down::UnreadSource { .. } => (RootState::Error, Helper::NotNeeded),
                Down::Kept { .. } | Down::Failed { .. } if record.needs_helper() => (RootState::Error, Helper::Needed),
                Down::Kept { .. } | Down::Failed { .. } => (RootState::Error, Helper::NotNeeded),
            };
            Published { path, state, error: error.clone(), switch_note: None, helper, view: view(Some(error)) }
        }
        Is::Up(up) => {
            let intercepted = up.record.intercepted();
            // A recovery that could not reset a file outranks the mode in `State`, because
            // it is the louder of the two problems; `LastError` still carries the
            // no-interception warning.
            let state = match (&up.recovery, intercepted) {
                (Recovery::Unreset(_), _) => RootState::Error,
                (_, true) => RootState::Ready,
                (_, false) => RootState::NoInterception,
            };
            let error = match (intercepted, up.recovery.text()) {
                (true, None) => String::new(),
                (true, Some(trouble)) => trouble.to_owned(),
                (false, None) => NO_INTERCEPTION_WARNING.to_owned(),
                (false, Some(trouble)) => format!("{NO_INTERCEPTION_WARNING}. {trouble}"),
            };
            let helper = match up.record.interception {
                // HS2: a folder that shows OneDrive is kept in step only with
                // interception; without it, it waits for the helper, and switches when it
                // connects.
                Interception::Without { .. } if up.record.source == RootSource::OneDrive => Helper::Waited,
                Interception::Without { .. } => Helper::NotNeeded,
                Interception::Intercepted => Helper::Needed,
            };
            let switch_note = up.switch_failed.clone().map(|why| SwitchNote { why });
            Published { path, state, error, switch_note, helper, view: view(None) }
        }
    }
}

impl SyncService {
    /// Publishes `folder`: the view for the readers first, then what the bus shows, with
    /// `also` applied in the same update. Called only by a [`Stopped`](super::folder::Stopped).
    pub(super) fn publish(&self, folder: &Folder, also: impl FnOnce(&mut SyncSnapshot)) {
        let published = publish(folder);
        self.view.send_replace(published.view);
        let waits = match published.helper {
            Helper::NotNeeded => false,
            Helper::Needed => self.link().is_none(),
            Helper::Waited => true,
        };
        self.state.update_if_changed(|s| {
            s.root_path = published.path;
            s.root_state = published.state;
            s.last_error = published.error;
            s.switch_note = published.switch_note;
            s.waits_for_helper = waits;
            s.scan.follow(folder.wanted);
            also(s);
        });
    }

    /// Publishes the helper's disappearance: `State` used to stay `ready` with an empty
    /// `LastError` while the sync folder was, in the only sense that matters, dead —
    /// nothing intercepting, nothing reconnecting, and every un-hydrated file reading as
    /// zeros. Now it reads `error`, and `LastError` says what `HelperState` says (HS3):
    /// how to start the helper. The rest of what `LastError` said stays.
    ///
    /// Without the state lock: a change under way (a long registration, a fill it waits
    /// for) must not hold this back. The folder is read as last published.
    pub fn report_helper_lost(&self) {
        if self.record().is_some_and(|record| record.needs_helper()) {
            self.state.update(|s| s.waits_for_helper = true);
        }
    }
}

#[cfg(test)]
mod tests;

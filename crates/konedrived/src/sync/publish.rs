//! What the bus shows of the folder's state, worked out in one place: [`publish`] derives
//! `Path`, `State`, the registration's part of `LastError`, whether the folder waits for
//! the helper, and the [`View`] from a [`Folder`], and nothing else writes them.
//!
//! What the folder's sync says of itself (its trouble, its notes, the counters) is beside
//! this, in the snapshot, written by the parts that run; `status::snapshot` puts the two
//! together (`published_state`, `published_error`).

use std::sync::Arc;

use super::folder::{Down, Folder, Interception, Is, Recovery, Standing, SyncView, View};
use super::running_sync::{Lock, RunningSync, Sync, Uploading, Why};
use super::{RootSource, SyncService, NO_INTERCEPTION_WARNING};
use crate::config::Mode;
use crate::status::snapshot::{RootState, SwitchNote, SyncSnapshot, SyncTrouble};

/// What a folder's state comes to on the bus, and for the readers.
#[derive(Clone)]
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
    /// `Writable`: what is changed in the folder is uploaded now.
    pub writable: bool,
    /// Why the folder is locked though its account is read-write.
    pub locked_note: String,
    /// Why the folder's sync could not start: blocking trouble, until one does.
    pub cannot_start: Option<String>,
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

impl Helper {
    /// Whether a folder that says this of the helper waits for it, with a `link` or none.
    pub(super) fn waits(self, link: bool) -> bool {
        match self {
            Helper::NotNeeded => false,
            Helper::Needed => !link,
            Helper::Waited => true,
        }
    }
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
            writable: false,
            locked_note: String::new(),
            cannot_start: None,
            view: View { wanted: folder.wanted, ..View::default() },
        };
    }
    let sync = match folder.onedrive().map(|onedrive| &onedrive.sync) {
        None => SyncView::None,
        // One that was told to stop is not running: a change that told it may have been
        // cut before it took it out of the state.
        Some(Sync::Running(sync)) if sync.handles().told_to_stop() => SyncView::Stopped(None),
        Some(Sync::Running(sync)) => SyncView::Running(sync.handles().clone()),
        Some(Sync::Stopped(Why::CannotStart(why))) => SyncView::Stopped(Some(why.clone())),
        Some(Sync::Stopped(_)) => SyncView::Stopped(None),
    };
    let view = |down: Option<String>| View {
        record: folder.acted_on().cloned(),
        down,
        wanted: folder.wanted,
        source: folder.source(),
        tree_lock: folder.acted_on().map(|record| Arc::clone(&record.kept.tree_lock)),
        store: folder.store(),
        sync: sync.clone(),
    };
    let quiet = |path, state, error, helper, view| Published {
        path,
        state,
        error,
        switch_note: None,
        helper,
        writable: false,
        locked_note: String::new(),
        cannot_start: None,
        view,
    };
    match &folder.is {
        Is::Absent => quiet(path, RootState::None, String::new(), Helper::NotNeeded, view(None)),
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
            let why = down.waits_for().map_or_else(|| error.clone(), str::to_owned);
            quiet(path, state, error, helper, view(Some(why)))
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
            let (writable, locked_note) = uploads(folder.wanted, folder.running().filter(|sync| !sync.handles().told_to_stop()).map(RunningSync::uploading));
            let cannot_start = match &sync {
                SyncView::Stopped(why) => why.clone(),
                _ => None,
            };
            Published {
                path,
                state,
                error,
                switch_note,
                helper,
                writable,
                locked_note,
                cannot_start,
                view: view(None),
            }
        }
    }
}

/// `Writable`, and why a folder is locked though its account is read-write, for a folder
/// that follows `wanted` and whose sync, if one runs, does `uploading` about local changes.
/// Writable only once the lock is off: not while the watcher still walks the folder.
pub(super) fn uploads(wanted: Mode, uploading: Option<Uploading>) -> (bool, String) {
    match uploading {
        Some(Uploading::Open(Lock::Off)) => (wanted == Mode::ReadWrite, String::new()),
        Some(Uploading::Open(Lock::Walking)) | None => (false, String::new()),
        Some(Uploading::Open(Lock::Stays(note))) => (false, note),
        Some(Uploading::Locked(note)) => (false, note.unwrap_or_default()),
    }
}

impl SyncService {
    /// Publishes `folder`: the view for the readers first, then what the bus shows, with
    /// `also` applied in the same update. Called by a [`Stopped`](super::folder::Stopped),
    /// and by the one task of a running sync that says its watcher's walk is over, which
    /// holds the state for reading (`start_sync`).
    pub(super) fn publish(&self, folder: &Folder, also: impl FnOnce(&mut SyncSnapshot)) {
        let published = publish(folder);
        self.view.send_replace(published.view);
        let waits = published.helper.waits(self.link().is_some());
        self.state.update_if_changed(|s| {
            s.folder.root_path = published.path;
            s.folder.root_state = published.state;
            s.folder.last_error = published.error;
            s.folder.switch_note = published.switch_note;
            s.folder.waits_for_helper = waits;
            s.folder.writable = published.writable;
            s.folder.locked_note = published.locked_note;
            if let Some(text) = published.cannot_start {
                s.cycle.sync_trouble = Some(SyncTrouble { text, blocking: true });
            }
            s.local.scan.follow(folder.wanted);
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
            self.state.update(|s| s.folder.waits_for_helper = true);
        }
    }
}

#[cfg(test)]
mod tests;

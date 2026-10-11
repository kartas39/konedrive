//! What the bus shows of the folder's state, worked out in one place: [`publish`] derives
//! `Path`, `State`, the registration's part of `LastError`, `Writable` and the [`View`] from
//! a [`Folder`], and nothing else writes them.
//!
//! What the folder's sync says of itself (its trouble, its notes, the counters) is beside
//! this, in the snapshot, written by the parts that run; `status::snapshot` puts the two
//! together (`published_state`, `published_error`).

use std::sync::Arc;

use super::folder::{Folder, Is, Record, Standing, Sync, SyncView, View, Why};
use super::SyncService;
use crate::status::snapshot::{RootState, SyncSnapshot, SyncTrouble, TroubleKind};

/// What a folder's state comes to on the bus, and for the readers.
#[derive(Clone)]
pub(super) struct Published {
    /// `Path`: the folder recorded, up or not; empty with none.
    pub path: String,
    /// `State`, before the sync has its say (`published_state`).
    pub state: RootState,
    /// The registration's part of `LastError`: what it ran into.
    pub error: String,
    /// `Writable`: the folder takes changes, and what is changed in it is uploaded.
    pub writable: bool,
    /// Why the folder's sync could not start: blocking trouble, until one does.
    pub cannot_start: Option<String>,
    pub view: View,
}

/// What `folder` comes to. It reads the folder's state, and of its mount whether it takes
/// changes now.
pub(super) fn publish(folder: &Folder) -> Published {
    let path = folder.record().map(|record| record.root.path.display().to_string()).unwrap_or_default();
    // An account held back says so over whatever its folder is: nothing acts on the
    // folder but a Forget.
    if let Standing::HeldBack(why) = &folder.standing {
        return Published {
            path,
            state: RootState::Error,
            error: why.clone(),
            writable: false,
            cannot_start: None,
            view: View { wanted: folder.wanted, ..View::default() },
        };
    }
    let sync = match folder.up().map(|up| &up.sync) {
        None => SyncView::None,
        // One that was told to stop is not running: a change that told it may have been
        // cut before it took it out of the state.
        // TODO: check that the stop's token (`SyncTasks::stop_handle`) is all a reader needs of
        // a running sync, and fix if needed.
        Some(Sync::Running(started)) if started.tasks.stop_handle().is_cancelled() => SyncView::Stopped(None),
        Some(Sync::Running(started)) => SyncView::Running(started.tasks.stop_handle()),
        Some(Sync::Stopped(Why::CannotStart(why) | Why::DriveNotKnown(why))) => SyncView::Stopped(Some(why.clone())),
        // A folder taken down says its cause as blocking trouble until a start stays up.
        Some(Sync::Stopped(Why::TakenDown { cause, .. })) => SyncView::Stopped(Some(cause.clone())),
        Some(Sync::Stopped(_)) => SyncView::Stopped(None),
    };
    let view = |down: Option<String>| View {
        record: folder.acted_on().map(Record::bare),
        down,
        wanted: folder.wanted,
        tree_lock: folder.acted_on().map(|record| Arc::clone(&record.kept.tree_lock)),
        parts: folder.acted_on().and_then(|_| folder.parts()),
        // TODO: `poll`, `sending_holds` and `onedrive_space` from the running sync (`SyncTasks::poll`, `SyncTasks::sending_holds`, `StartedFolder::onedrive_space`): `SyncTasks`' accessors are `todo!()` until the folder's start is written.
        poll: None,
        sending_holds: None,
        onedrive_space: None,
        sync: sync.clone(),
    };
    let (state, error, down) = match &folder.is {
        Is::Absent => (RootState::None, String::new(), None),
        Is::Down(record, down) => {
            let error = down.why(&record.root);
            // Nothing is known to be wrong with a folder that only waits: it is brought up
            // in a moment.
            let state = if down.waits_for().is_some() { RootState::Waiting } else { RootState::Error };
            let why = down.waits_for().map_or_else(|| error.clone(), str::to_owned);
            (state, error, Some(why))
        }
        Is::Up(_) => (RootState::Ready, String::new(), None),
    };
    let cannot_start = match &sync {
        SyncView::Stopped(why) => why.clone(),
        _ => None,
    };
    let writable = folder.up().is_some() && folder.mount().is_some_and(|mount| mount.access_mode_switch().is_read_write());
    Published { path, state, error, writable, cannot_start, view: view(down) }
}

impl SyncService {
    /// Publishes `folder`: the view for the readers first, then what the bus shows, with
    /// `also` applied in the same update. Called by a [`Stopped`](super::folder::Stopped).
    pub(super) fn publish(&self, folder: &Folder, also: impl FnOnce(&mut SyncSnapshot)) {
        let published = publish(folder);
        self.view.send_replace(published.view);
        self.state.update_if_changed(|s| {
            s.folder.root_path = published.path;
            s.folder.root_state = published.state;
            s.folder.last_error = published.error;
            s.folder.writable = published.writable;
            if let Some(text) = published.cannot_start {
                s.cycle.sync_trouble = Some(SyncTrouble { text, blocking: true, kind: TroubleKind::Other });
            }
            also(s);
        });
    }
}

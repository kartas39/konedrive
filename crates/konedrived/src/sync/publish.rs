//! What the bus shows of the folder's state, worked out in one place: [`publish`] derives
//! `Path`, `State`, the registration's part of `LastError`, `Writable` and the [`View`] from
//! a [`Folder`], and nothing else writes them.
//!
//! What the folder's sync says of itself (its trouble, its notes, the counters) is beside
//! this, in the snapshot, written by the parts that run; `status::snapshot` puts the two
//! together (`published_state`, `published_error`).

use std::sync::Arc;

use super::folder::{Folder, Is, Record, Standing, SyncView, View};
use super::running_sync::{Sync, Why};
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
        Some(Sync::Running(sync)) if sync.handles().told_to_stop() => SyncView::Stopped(None),
        Some(Sync::Running(sync)) => SyncView::Running(sync.handles().clone()),
        Some(Sync::Stopped(Why::CannotStart(why))) => SyncView::Stopped(Some(why.clone())),
        Some(Sync::Stopped(_)) => SyncView::Stopped(None),
    };
    let view = |down: Option<String>| View {
        record: folder.acted_on().map(Record::bare),
        down,
        wanted: folder.wanted,
        tree_lock: folder.acted_on().map(|record| Arc::clone(&record.kept.tree_lock)),
        store: folder.store(),
        mount: folder.acted_on().and_then(|record| record.kept.mount.clone()),
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

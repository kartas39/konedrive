//! Selective sync at the service's level (issue #58): which folders of the
//! drive are on this computer.
//!
//! The selection lives in the account's section of `config.toml`
//! (`sync_only`), is published in the folder's state, and is applied by the
//! tree store (`tree::select`), which holds a copy while it is open. A
//! change writes `config.toml` and the store's placements in one job of the
//! store, then asks for a Full reconcile, which makes the folder follow.
//! When no sync runs, the change is stored all the same, and the folder
//! follows at the next cycle.

use std::sync::{Arc, Mutex};

use super::{Persist, RootSource, SyncError, SyncService, SyncStateHandle};
use crate::config::ConfigError;
use crate::tree::{FolderChild, Selection, SelectionSink, TreeError, TreeStore};

/// Writes the selection where it is kept: `config.toml`, then the published
/// state, under one lock.
#[derive(Clone)]
struct Keeper {
    lock: Arc<Mutex<()>>,
    persist: Option<Persist>,
    state: SyncStateHandle,
}

impl Keeper {
    fn write(&self, selection: Option<Selection>) -> Result<(), SyncError> {
        let _held = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(persist) = &self.persist {
            let kept = selection.clone();
            persist
                .store
                .update_account(&persist.account, |a| {
                    a.sync_only = kept;
                    Ok::<_, ConfigError>(())
                })
                .map_err(|e| SyncError::Io(format!("cannot write config.toml: {e}")))?;
        }
        self.state.update(|s| s.selection = selection);
        Ok(())
    }
}

impl SyncService {
    fn keeper(&self) -> Keeper {
        Keeper { lock: Arc::clone(&self.selecting), persist: self.persist.clone(), state: self.state.clone() }
    }

    /// The selection: `None` while everything is synced.
    pub fn selection(&self) -> Option<Selection> {
        self.state.get().selection
    }

    /// What the tree store tells when it changes the list by itself — a
    /// chosen folder deleted, or moved into another chosen one: written to
    /// `config.toml` and published.
    pub(super) fn selection_sink(&self) -> SelectionSink {
        let keeper = self.keeper();
        Arc::new(move |selection| {
            if let Err(e) = keeper.write(Some(selection.clone())) {
                tracing::error!("the chosen folders changed, and {e}");
            }
        })
    }

    /// Whether the account has a OneDrive folder; `Unsupported` for a folder
    /// that is not connected to OneDrive, as `SetThumbnails` refuses it.
    fn selectable(&self) -> Result<bool, SyncError> {
        match self.registration() {
            None => Ok(false),
            Some(reg) if reg.source == RootSource::OneDrive => Ok(true),
            Some(_) => Err(SyncError::Unsupported("this folder is not connected to OneDrive".into())),
        }
    }

    /// Runs `job` on the folder's tree store, with `lifecycle` held for
    /// reading until it is done: the store the sync opened, or — when none
    /// ran yet — the store's file, opened for this one job, with the
    /// selection `config.toml` has.
    async fn on_tree<T: Send + 'static>(
        &self,
        lifecycle: tokio::sync::OwnedRwLockReadGuard<()>,
        job: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, SyncError> {
        let open = self.store.lock().unwrap().clone();
        let file = self.sync_paths.lock().unwrap().as_ref().map(|p| p.tree_db.clone());
        let selection = self.selection();
        tokio::task::spawn_blocking(move || {
            let _lifecycle = lifecycle;
            match (open, file) {
                (Some(store), _) => store.call_blocking(job),
                (None, Some(file)) => {
                    let mut store = TreeStore::open(&file)?;
                    store.set_selection(selection, None)?;
                    job(&mut store)
                }
                (None, None) => Err(TreeError::Io(std::io::Error::other("no tree store is configured"))),
            }
        })
        .await
        .map_err(|e| SyncError::Io(format!("the store task failed: {e}")))?
        .map_err(|e| SyncError::Io(e.to_string()))
    }

    /// `SetSelection(ids, root_files)`: only these folders, by item id, are
    /// on this computer from now on, with the root's own files if
    /// `root_files`. The list is made normal: no id twice, and no folder
    /// inside another chosen one.
    ///
    /// `InvalidArgs` for an id that is not in the list already and is not in
    /// the store, is not a folder, or is, or lies inside, a folder skipped
    /// for another reason. Before a folder is bound only an empty list is
    /// taken, and only `config.toml` is written: the folder bound next lists
    /// the drive and places nothing.
    pub async fn set_selection(&self, ids: Vec<String>, root_files: bool) -> Result<(), SyncError> {
        let lifecycle = Arc::clone(&self.lifecycle).read_owned().await;
        let keeper = self.keeper();
        if !self.selectable()? {
            if !ids.is_empty() {
                return Err(SyncError::InvalidArgs("no folder is bound yet, so no folder of OneDrive is known: only an empty list can be set".into()));
            }
            return tokio::task::spawn_blocking(move || keeper.write(Some(Selection { folders: Vec::new(), root_files })))
                .await
                .map_err(|e| SyncError::Io(format!("the settings task failed: {e}")))?;
        }
        let sink = self.selection_sink();
        self.on_tree(lifecycle, move |s| {
            let folders = match s.check_selection(&ids)? {
                Ok(folders) => folders,
                Err(why) => return Ok(Err(SyncError::InvalidArgs(why))),
            };
            let selection = Selection { folders, root_files };
            // `config.toml` first: what the store then holds is never more
            // than the file says.
            if let Err(e) = keeper.write(Some(selection.clone())) {
                return Ok(Err(e));
            }
            s.set_selection(Some(selection), Some(sink))?;
            Ok(Ok(()))
        })
        .await??;
        self.nudge_full();
        Ok(())
    }

    /// `SyncEverything()`: no selection; every folder is on this computer
    /// again. Also on an account with no folder bound yet.
    pub async fn sync_everything(&self) -> Result<(), SyncError> {
        let lifecycle = Arc::clone(&self.lifecycle).read_owned().await;
        let keeper = self.keeper();
        if !self.selectable()? {
            return tokio::task::spawn_blocking(move || keeper.write(None))
                .await
                .map_err(|e| SyncError::Io(format!("the settings task failed: {e}")))?;
        }
        let sink = self.selection_sink();
        self.on_tree(lifecycle, move |s| {
            if let Err(e) = keeper.write(None) {
                return Ok(Err(e));
            }
            s.set_selection(None, Some(sink))?;
            Ok(Ok(()))
        })
        .await??;
        self.nudge_full();
        Ok(())
    }

    /// `SelectedFolders`: the chosen folders, each with its path in OneDrive
    /// relative to the root — empty for an id the store does not know.
    pub async fn selected_folders(&self) -> Result<Vec<(String, String)>, SyncError> {
        let lifecycle = Arc::clone(&self.lifecycle).read_owned().await;
        let Some(selection) = self.selection() else { return Ok(Vec::new()) };
        if !matches!(self.selectable(), Ok(true)) {
            return Ok(selection.folders.into_iter().map(|id| (id, String::new())).collect());
        }
        self.on_tree(lifecycle, |s| s.selected_folders()).await
    }

    /// `FolderChildren(id)`: the sub-folders of a folder of the drive (`""`:
    /// the root), by name, with their state. Read from the store: also the
    /// folders that are not on this computer. Nothing before a folder is
    /// bound.
    pub async fn folder_children(&self, id: &str) -> Result<Vec<FolderChild>, SyncError> {
        let lifecycle = Arc::clone(&self.lifecycle).read_owned().await;
        if !self.selectable()? {
            return Ok(Vec::new());
        }
        let id = id.to_owned();
        self.on_tree(lifecycle, move |s| s.folder_children(&id)).await
    }
}

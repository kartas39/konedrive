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

/// How many paths a `LocalChanges` refusal lists.
const REFUSAL_PATHS: usize = 10;

/// The message of a `LocalChanges` refusal: a first line that says what
/// happened, then up to [`REFUSAL_PATHS`] lines `<path>: <why>`, the paths
/// relative to the folder, and a last line counting the rest, if any.
fn refusal(lost: &[(std::path::PathBuf, String)]) -> String {
    let mut text = format!(
        "{} file(s) or folder(s) that would be removed from this computer exist only here; nothing was changed:",
        lost.len()
    );
    for (rel, why) in lost.iter().take(REFUSAL_PATHS) {
        text.push_str(&format!("\n{}: {why}", rel.display()));
    }
    if lost.len() > REFUSAL_PATHS {
        text.push_str(&format!("\nand {} more", lost.len() - REFUSAL_PATHS));
    }
    text
}

/// The store takes `selection`, which `config.toml` and the published state
/// already have. When it cannot, both get `before` back — the selection the
/// store keeps — and the error is the answer: nothing changed.
fn applied(s: &mut TreeStore, keeper: &Keeper, before: Option<Selection>, selection: Option<Selection>, sink: SelectionSink) -> Result<(), SyncError> {
    let Err(e) = s.set_selection(selection, Some(sink)) else { return Ok(()) };
    tracing::error!("the store could not take the chosen folders; they stay as they were: {e}");
    if let Err(back) = keeper.write(before) {
        tracing::error!("and config.toml could not be put back: {back}");
    }
    Err(SyncError::Io(e.to_string()))
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
            keeper.write(Some(selection.clone())).map_err(|e| {
                tracing::error!("the chosen folders changed, and {e}");
                e.to_string()
            })
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
        self.on_tree_holding(None, lifecycle, job).await
    }

    /// [`Self::on_tree`], with the tree lock `tree` held too until the job
    /// is done.
    async fn on_tree_holding<T: Send + 'static>(
        &self,
        tree: Option<tokio::sync::OwnedMutexGuard<()>>,
        lifecycle: tokio::sync::OwnedRwLockReadGuard<()>,
        job: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, SyncError> {
        let open = self.store.lock().unwrap().clone();
        let file = self.sync_paths.lock().unwrap().as_ref().map(|p| p.tree_db.clone());
        let selection = self.selection();
        tokio::task::spawn_blocking(move || {
            let _tree = tree;
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
    /// for another reason. `LocalChanges`, with nothing changed, while what
    /// would leave this computer holds a change waiting to be uploaded or
    /// an object that is never uploaded
    /// ([`TreeStore::selection_would_lose`]). Before a folder is bound only an empty list is
    /// taken, and only `config.toml` is written: the folder bound next lists
    /// the drive and places nothing.
    pub async fn set_selection(&self, ids: Vec<String>, root_files: bool) -> Result<(), SyncError> {
        let tree = self.change_lock().await;
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
        let before = self.selection();
        self.on_tree_holding(Some(tree), lifecycle, move |s| {
            let folders = match s.check_selection(&ids)? {
                Ok(folders) => folders,
                Err(why) => return Ok(Err(SyncError::InvalidArgs(why))),
            };
            let selection = Selection { folders, root_files };
            // Nothing that exists only on this computer leaves it: a change
            // waiting to be uploaded, or an object that is never uploaded,
            // refuses the whole change, before anything is written.
            let lost = s.selection_would_lose(&selection)?;
            if !lost.is_empty() {
                return Ok(Err(SyncError::LocalChanges(refusal(&lost))));
            }
            // `config.toml` first: what the store then holds is never more
            // than the file says.
            if let Err(e) = keeper.write(Some(selection.clone())) {
                return Ok(Err(e));
            }
            Ok(applied(s, &keeper, before, Some(selection), sink))
        })
        .await??;
        self.follow_selection();
        Ok(())
    }

    /// `SyncEverything()`: no selection; every folder is on this computer
    /// again. Also on an account with no folder bound yet.
    pub async fn sync_everything(&self) -> Result<(), SyncError> {
        let tree = self.change_lock().await;
        let lifecycle = Arc::clone(&self.lifecycle).read_owned().await;
        let keeper = self.keeper();
        if !self.selectable()? {
            return tokio::task::spawn_blocking(move || keeper.write(None))
                .await
                .map_err(|e| SyncError::Io(format!("the settings task failed: {e}")))?;
        }
        let sink = self.selection_sink();
        let before = self.selection();
        self.on_tree_holding(Some(tree), lifecycle, move |s| {
            if let Err(e) = keeper.write(None) {
                return Ok(Err(e));
            }
            Ok(applied(s, &keeper, before, None, sink))
        })
        .await??;
        self.follow_selection();
        Ok(())
    }

    /// The tree lock, taken before a change of the selection, in the order
    /// the outbox worker and a read-write cycle take it (the tree lock, then
    /// the store): a cycle between its staging and its swap would otherwise
    /// write the placements from before the change back into `items`.
    /// Taken in read-only mode too, where nothing else holds it for long.
    async fn change_lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.tree_lock).lock_owned().await
    }

    /// The folder follows a change of the selection: a Full reconcile takes
    /// off this computer what left and places what came, and — on a
    /// read-write folder — a Full local scan sends what was kept back as
    /// `not-selected` and is not any more. With no sync running, both happen
    /// when one starts.
    fn follow_selection(&self) {
        self.nudge_full();
        if let Some(watcher) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()) {
            watcher.selection_scan();
        }
    }

    /// `SelectedFolders`: the chosen folders, each with its path in OneDrive
    /// relative to the root — empty for an id the store does not know.
    pub async fn selected_folders(&self) -> Result<Vec<(String, String)>, SyncError> {
        // Asked first, with no lock: with no selection there is nothing to
        // read, and a property read must not wait for a registration.
        if self.selection().is_none() {
            return Ok(Vec::new());
        }
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

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::sync::SyncService;
use crate::helper::HelperError;
use crate::folder::root::SyncRoot;
use crate::sync::{Registration, RootSource, SyncError};
use crate::status::snapshot::RootState;
use crate::folder::disk;
use crate::folder::root;

/// A Forget's refusal while `waiting` changes wait to be uploaded.
fn refuse_waiting(waiting: u64) -> Result<(), SyncError> {
    match waiting {
        0 => Ok(()),
        n => Err(SyncError::PendingUploads(format!(
            "{n} change(s) made here have not been uploaded yet, and would be lost with the folder's \
             record; wait until they are uploaded, or drop them with a forced switch of the account \
             to read-only (the files stay here as they are), then try again"
        ))),
    }
}

impl SyncService {
    /// No registration, bring-up or switch for this account from now on
    /// (`Accounts.Remove`). Called with `lifecycle` held for writing.
    fn retire_locked(&self) {
        self.retiring.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether `Accounts.Remove` is taking this account away.
    pub(super) fn is_retiring(&self) -> bool {
        self.retiring.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Takes [`retire`](Self::retire) back, for an `Accounts.Remove` whose
    /// later steps failed: the account stays, so it registers a folder again.
    /// The folder it had is not brought back: it is forgotten, at the helper
    /// too, and a OneDrive folder's tree store is gone. An account that was
    /// held back is held back again, and says why.
    pub async fn unretire(&self) {
        let _lifecycle = self.lifecycle.write().await;
        self.retiring.store(false, std::sync::atomic::Ordering::SeqCst);
        self.publish_held();
    }

    /// The folder a held-back account records, as a registration to forget
    /// through: its folder is never brought up (§3.1), but one registered with
    /// interception in an earlier session is still the helper's until the
    /// helper lets go of it. `None` for an account not held back, or one that
    /// records no folder. The root id comes from `config.toml` or, for a
    /// config written before the id was recorded, from the folder, as
    /// [`hold`](Self::hold) finds it; an intercepted folder with neither
    /// cannot be named to the helper, and is refused.
    async fn recorded_for_forget(&self) -> Result<Option<Registration>, SyncError> {
        if self.held.lock().unwrap().is_none() {
            return Ok(None);
        }
        let Some(persisted) = self.persisted_root() else { return Ok(None) };
        let root_id = if root::looks_like_a_root_id(&persisted.root_id) {
            persisted.root_id.clone()
        } else if let Some(root_id) = root::recorded_root_id(&persisted.path).await {
            root_id
        } else if !persisted.intercepted {
            persisted.root_id.clone()
        } else {
            return Err(SyncError::Io(format!(
                "cannot forget {}: config.toml does not record its root id, and the folder carries \
                 none that can be read",
                persisted.path.display()
            )));
        };
        Ok(Some(Registration {
            dev: None,
            root: SyncRoot { path: persisted.path, root_id },
            intercepted: persisted.intercepted,
            recovery_deferred: false,
            source: persisted.source,
            brought_up: false,
            baloo_excluded: persisted.baloo_excluded,
            upgrade_when_helper: false,
        }))
    }

    /// Forgets the root and clears the published state. The files themselves
    /// are left exactly as they are.
    ///
    /// # An intercepted root is forgotten through the helper, or not at all
    ///
    /// The helper's `UnregisterRoot` is the one thing that takes an
    /// intercepted root's marks off — its directory marks, and the ignore
    /// mark on every hydrated file in it. This used to tell the helper only
    /// *if* a link happened to be up, and forget the root either way: the
    /// helper kept the registration, the marks and the ignore marks, the
    /// orphan never cleared (`resume` has nothing to renew for a root the
    /// daemon no longer holds), and a folder registered again without
    /// interception then had its files freed up with no `ClearIgnore` while
    /// they were still ignored — measured in the VM suite: a reader got
    /// 65536 zero bytes and nothing was fetched. So with no link this
    /// refuses `NoHelper`, exactly as `RegisterRoot` does, and nothing
    /// changes.
    ///
    /// The helper answering `EPERM` is not a refusal to forget: it holds no
    /// root of this uid under that id — it lost it, or never kept it — so no
    /// mark of that registration is left to take off, and keeping the root
    /// would only make it impossible to forget. Any other failure keeps it,
    /// because then the helper may well still hold it.
    ///
    /// # A root registered without interception never involves the helper
    ///
    /// It was never announced to the helper, so there is nothing to tell it.
    /// Telling it anyway made such a root impossible to forget while a helper
    /// was connected: the helper refuses `EPERM` to unregister a root the uid
    /// does not hold, and the daemon kept the registration (measured).
    ///
    /// # A OneDrive folder
    ///
    /// Its sync is stopped first, the read-only lock is taken off the folder
    /// once it is forgotten, and its tree store is dropped. A Forget that is
    /// refused leaves it registered — so it is kept in step again.
    ///
    /// The sync is stopped before `lifecycle` is taken for writing: a
    /// reconcile holds it for reading while it changes the folder, and checks
    /// for a stop between its steps, so stopped first it lets go at its next
    /// step rather than at the end of the whole reconcile. A helper's
    /// reconnect that takes the lock in between brings the folder up again,
    /// and so starts its sync again; that one is stopped under the lock, where
    /// stopping cannot wait for a reconcile — none can hold the lock.
    pub async fn unregister_root(&self) -> Result<(), SyncError> {
        self.forget(false).await
    }

    /// `Accounts.Remove`'s first step: the folder forgotten exactly as
    /// [`unregister_root`](Self::unregister_root) forgets it — refused under
    /// the same rule — and, under the same `lifecycle` lock so that nothing
    /// comes in between, the account retired: no registration, bring-up or
    /// switch is made for it from then on. An account with no folder is
    /// retired all the same.
    pub async fn retire(&self) -> Result<(), SyncError> {
        self.forget(true).await
    }

    /// [`unregister_root`](Self::unregister_root) and [`retire`](Self::retire).
    ///
    /// Refused `PendingUploads` while changes wait to be uploaded: the tree
    /// store that holds them goes with the folder. Asked before anything changes — the watcher
    /// hands over what it holds first — and again once the sync has stopped.
    async fn forget(&self, retire: bool) -> Result<(), SyncError> {
        // Without the lifecycle lock: a reconcile, or a switch waiting for it, must not keep
        // the Forget from stopping the sync first.
        self.flush_watcher().await;
        refuse_waiting(self.changes_in_store().await?)?;
        // The tasks only, outside the lock; the activity is let go of under
        // it (B-M1), where no reconnect can have started a sync meanwhile.
        let was_syncing = self.stop_tasks().await;
        let _lifecycle = self.lifecycle.write().await;
        let was_syncing = self.stop_tasks().await || was_syncing;
        if was_syncing {
            self.let_go_of_activity().await;
        }
        if let Err(refused) = self.changes_in_store().await.and_then(refuse_waiting) {
            if was_syncing {
                self.start_sync().await;
            }
            return Err(refused);
        }
        self.restore_locked().await;
        // A held-back account never brings its folder up, but a folder the
        // helper may still hold leaves through the helper all the same: its
        // record is the only name the helper holds it by.
        let (reg, recorded) = match self.registration() {
            Some(reg) => (reg, false),
            None => match self.recorded_for_forget().await? {
                Some(reg) => (reg, true),
                None if retire => {
                    self.retire_locked();
                    return Ok(());
                }
                None => return Err(SyncError::NoRoot),
            },
        };
        // A move out of the folder still in its store goes with it (the count above leaves none):
        // what it left outside is tidied first, while the helper still holds the folder, and the
        // hub stops routing its ids.
        if reg.source == RootSource::OneDrive {
            self.drop_moved_out(&reg.root).await;
        }
        let result = self.forget_locked(&reg).await;
        if result.is_ok() {
            if retire {
                self.retire_locked();
            } else if recorded {
                self.publish_held();
            }
        }
        if reg.source == RootSource::OneDrive {
            match &result {
                Ok(()) => {
                    self.let_go_of_onedrive(&reg.root).await;
                    // Only when *this daemon* is the one that
                    // excluded the folder from Baloo — never a folder that
                    // arrived already excluded, and this survives a restart
                    // in between (`commit` carries `baloo_excluded` forward
                    // from `config.toml` for a root that is brought back up,
                    // not freshly registered).
                    if reg.baloo_excluded {
                        let baloo = Arc::clone(&self.baloo.lock().unwrap());
                        baloo.include_again(&reg.root.path).await;
                    }
                }
                Err(_) if was_syncing => self.start_sync().await,
                Err(_) => {}
            }
        }
        result
    }

    /// The Forget itself, under `lifecycle` held for writing: through the
    /// helper for an intercepted root, then everything published about the
    /// root and its sync cleared at once.
    async fn forget_locked(&self, reg: &Registration) -> Result<(), SyncError> {
        if reg.intercepted {
            let link = self.require_link()?;
            match link.unregister_root(&reg.root.root_id).await {
                Ok(()) => {}
                Err(HelperError::Refused(libc::EPERM)) => tracing::warn!(
                    "the helper holds no root {} for {}; forgetting it here",
                    reg.root.root_id,
                    reg.root.path.display()
                ),
                Err(e) => return Err(SyncError::Io(e.to_string())),
            }
        }
        *self.root.lock().unwrap() = None;
        *self.source.lock().unwrap() = None;
        self.persist_or_log(None);
        // One update, so that nothing is ever published about a folder that
        // is no longer registered.
        self.state.update(|s| {
            s.root_path.clear();
            s.root_state = RootState::None;
            s.last_error.clear();
            s.listing = false;
            s.items_listed = 0;
            s.items_placed = 0;
            s.skipped_count = 0;
            s.sync_trouble = None;
            s.replacement_note.clear();
            s.outbox_note.clear();
            s.last_checked = 0;
            s.local_bytes = 0;
            s.conflict_count = 0;
            s.waits_for_helper = false;
        });
        // The pins stay on the files; what they queued is dropped, and
        // `PinnedCount` reads 0.
        self.pins.clear();
        // A Forget drops the activity: a OneDrive folder's with
        // its store, which `stop_sync` has already let go of, and a local
        // folder's from memory here — after the folder stopped being the one
        // registered, so that a download still ending in it records nothing
        // from now on. Its walker stops too.
        self.report.space.stop();
        let report = self.report.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || report.activity.detach()).await {
            tracing::warn!("the task forgetting the activity failed: {e}");
        }
        Ok(())
    }

    /// What a Forget adds for a OneDrive folder, still under
    /// `lifecycle` held for writing: the read-only lock taken off the whole
    /// folder — whose files stay — and its tree store dropped.
    /// The folder still carries its root id (a Forget leaves it), which is
    /// what proves it is still this folder before anything in it is changed.
    async fn let_go_of_onedrive(&self, root: &SyncRoot) {
        let root = root.clone();
        let unlocked = tokio::task::spawn_blocking(move || {
            disk::Disk::open(&root, false)
                .and_then(|disk| disk.unlock_tree())
                .map_err(|e| format!("cannot take the read-only lock off {}: {e}", root.path.display()))
        })
        .await
        .unwrap_or_else(|e| Err(format!("the unlock task failed: {e}")));
        if let Err(e) = unlocked {
            tracing::warn!("{e}");
        }
        self.remove_tree_store().await;
    }

    /// Removes the tree store: a forgotten folder's, or one left from a
    /// folder forgotten earlier when a new one is registered.
    pub(super) async fn remove_tree_store(&self) {
        *self.store.lock().unwrap() = None;
        // Its pause went with it (the outbox on the bus).
        self.forget_pause();
        let Some(paths) = self.sync_paths.lock().unwrap().clone() else { return };
        if let Err(e) = tokio::task::spawn_blocking(move || remove_tree_files(&paths.tree_db)).await {
            tracing::warn!("the task removing the tree store failed: {e}");
        }
    }
}

/// The tree store's files: the database and SQLite's journal beside it.
fn remove_tree_files(tree_db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = tree_db.as_os_str().to_owned();
        name.push(suffix);
        let file = PathBuf::from(name);
        if let Err(e) = std::fs::remove_file(&file) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!("cannot remove {}: {e}", file.display());
            }
        }
    }
}

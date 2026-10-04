//! Taking a folder down: a Forget, `Accounts.Remove`'s first step, and a folder that was
//! moved or deleted under its sync.

use std::io;
use std::path::{Path, PathBuf};

use super::folder::{Down, Is, Record, Standing, Stopped};
use super::{RootSource, SyncError, SyncService};
use crate::folder::disk;
use crate::folder::root::{self, SyncRoot};
use crate::helper::HelperError;

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
    /// Takes a retirement back, for an `Accounts.Remove` whose
    /// later steps failed: the account stays, so it registers a folder again.
    /// The folder it had is not brought back: it is forgotten, at the helper
    /// too, and a OneDrive folder's tree store is gone. An account that was
    /// held back is held back again, and says why.
    pub async fn unretire(&self) {
        let mut stopped = self.change().await;
        let folder = stopped.folder_mut();
        if let Standing::Retiring { held } = &folder.standing {
            folder.standing = match held {
                Some(why) => Standing::HeldBack(why.clone()),
                None => Standing::Active,
            };
        }
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
    /// orphan never cleared, and a folder registered again without
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
    /// Its sync is stopped first ([`change`](Self::change)), the read-only lock is taken
    /// off the folder once it is forgotten, and its tree store is dropped. A Forget that
    /// is refused leaves it registered — so it is kept in step again.
    ///
    /// # A folder that is not up
    ///
    /// One held until the helper is back, kept after a registration that failed, held
    /// with a `source` that cannot be read, or recorded by an account that is held back,
    /// is forgotten the same way: its record is the only name the helper holds it by.
    pub async fn unregister_root(&self) -> Result<(), SyncError> {
        self.forget(false).await.map(drop)
    }

    /// `Accounts.Remove`'s first step: the folder forgotten exactly as
    /// [`unregister_root`](Self::unregister_root) forgets it — refused under
    /// the same rule — and, in the same change so that nothing
    /// comes in between, the account retired: no registration, bring-up or
    /// switch is made for it from then on. An account with no folder is
    /// retired all the same. The answer is the folder that was forgotten: `None` for an
    /// account with no folder.
    pub async fn retire(&self) -> Result<Option<PathBuf>, SyncError> {
        self.forget(true).await
    }

    /// [`unregister_root`](Self::unregister_root) and [`retire`](Self::retire).
    ///
    /// Refused `PendingUploads` while changes wait to be uploaded: the tree
    /// store that holds them goes with the folder. Asked before anything changes — the
    /// watcher hands over what it holds first — and again once the sync has stopped.
    /// The folder that was forgotten.
    async fn forget(&self, retire: bool) -> Result<Option<PathBuf>, SyncError> {
        // Before the change: a reconcile, or a switch waiting for it, must not keep
        // the Forget from stopping the sync first.
        self.flush_watcher().await;
        refuse_waiting(self.changes_in_store().await?)?;
        let mut stopped = self.change().await;
        if let Err(refused) = self.changes_in_store().await.and_then(refuse_waiting) {
            self.start_again(&mut stopped).await;
            return Err(refused);
        }
        self.restore_in(&mut stopped).await;
        let Some(record) = stopped.folder().record().cloned() else {
            if retire {
                retire_in(&mut stopped);
                return Ok(None);
            }
            return Err(SyncError::NoRoot);
        };
        // A move out of the folder still in its store goes with it (the count above leaves
        // none): what it left outside is tidied first, while the helper still holds the
        // folder, and the hub stops routing its ids.
        if record.source == RootSource::OneDrive {
            self.drop_moved_out(&record.root).await;
        }
        if let Err(refused) = self.let_go_at_the_helper(&record).await {
            self.start_again(&mut stopped).await;
            return Err(refused);
        }
        stopped.folder_mut().is = Is::Absent;
        if retire {
            retire_in(&mut stopped);
        }
        self.forgotten(&stopped).await;
        if record.source == RootSource::OneDrive {
            self.let_go_of_onedrive(&record.root).await;
            // Only when *this daemon* is the one that excluded the folder from Baloo —
            // never a folder that arrived already excluded, and this survives a restart
            // in between (the record carries it forward from `config.toml`).
            if record.baloo {
                self.wiring.baloo.include_again(&record.root.path).await;
            }
        }
        Ok(Some(record.root.path))
    }

    /// The helper lets go of an intercepted folder, or the Forget is refused.
    async fn let_go_at_the_helper(&self, record: &Record) -> Result<(), SyncError> {
        if !record.intercepted() {
            return Ok(());
        }
        // An intercepted folder cannot be named to the helper without its id: only the
        // daemon's record of it goes, and nothing in the folder is touched (`F241`).
        if !root::looks_like_a_root_id(&record.root.root_id) {
            tracing::warn!(
                "forgetting {} here only: config.toml does not record its root id and the folder carries none \
                 that can be read, so the konedrive helper could not be told to let go of it; restarting \
                 konedrive-helper clears whatever it still holds of it",
                record.root.path.display()
            );
            return Ok(());
        }
        let link = self.require_link()?;
        match link.unregister_root(&record.root.root_id).await {
            Ok(()) => Ok(()),
            Err(HelperError::Refused(libc::EPERM)) => {
                tracing::warn!("the helper holds no root {} for {}; forgetting it here", record.root.root_id, record.root.path.display());
                Ok(())
            }
            Err(e) => Err(SyncError::Helper(e.to_string())),
        }
    }

    /// The folder is forgotten: `config.toml` records none, and everything published about
    /// it and its sync is cleared in one update, so that nothing is ever published about a
    /// folder that is no longer registered.
    async fn forgotten(&self, stopped: &Stopped<'_>) {
        *self.source.lock().unwrap() = None;
        self.persist_or_log(None);
        stopped.publish_with(|s| {
            s.listing = false;
            s.items_listed = 0;
            s.items_placed = 0;
            s.skipped_count = 0;
            s.sync_trouble = None;
            s.replacement_note.clear();
            s.outbox_note = None;
            s.last_checked = 0;
            s.local_bytes = 0;
            s.conflict_count = 0;
        });
        // The pins stay on the files; what they queued is dropped, and
        // `PinnedCount` reads 0.
        self.pins.clear();
        // A Forget drops the activity: a OneDrive folder's with
        // its store, which the change has already let go of, and a local
        // folder's from memory here — after the folder stopped being the one
        // registered, so that a download still ending in it records nothing
        // from now on. Its walker stops too.
        self.let_go_of_activity().await;
    }

    /// What a Forget adds for a OneDrive folder, still inside the change: the read-only
    /// lock taken off the whole folder — whose files stay — and its tree store dropped.
    /// The folder still carries its root id (a Forget leaves it), which is
    /// what proves it is still this folder before anything in it is changed.
    async fn let_go_of_onedrive(&self, root: &SyncRoot) {
        self.unlock(root).await;
        self.remove_tree_store().await;
    }

    /// The walk that takes the read-only lock off a folder (`docs/design/writes.md` §2.2),
    /// on a blocking thread: files `0644`, directories `0755`, the root last.
    pub(super) async fn unlock(&self, root: &SyncRoot) {
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
    }

    /// Removes the tree store: a forgotten folder's, or one left from a
    /// folder forgotten earlier when a new one is registered.
    pub(super) async fn remove_tree_store(&self) {
        *self.store.lock().unwrap() = None;
        // Its pause went with it (the outbox on the bus).
        self.forget_pause();
        let Some(tree_db) = self.sync_paths().map(|paths| paths.tree_db.clone()) else { return };
        if let Err(e) = tokio::task::spawn_blocking(move || remove_tree_files(&tree_db)).await {
            tracing::warn!("the task removing the tree store failed: {e}");
        }
    }

    /// The folder itself was moved or deleted (the watcher saw it, §3.3): its sync stops —
    /// nothing is deleted in the cloud because it went — and the folder is down, saying
    /// `why`, until a bring-up finds it again or it is forgotten.
    pub(super) async fn root_gone(&self, why: String) {
        let mut stopped = self.change().await;
        let folder = stopped.folder_mut();
        if let Is::Up(up) = &folder.is {
            folder.is = Is::Down(up.record.clone(), Down::Failed { why });
        }
    }
}

/// No registration, bring-up or switch for this account from now on (`Accounts.Remove`).
fn retire_in(stopped: &mut Stopped<'_>) {
    let folder = stopped.folder_mut();
    let held = match &folder.standing {
        Standing::HeldBack(why) => Some(why.clone()),
        Standing::Retiring { held } => held.clone(),
        Standing::Active => None,
    };
    folder.standing = Standing::Retiring { held };
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

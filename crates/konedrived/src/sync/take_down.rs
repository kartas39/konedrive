//! Taking a folder down: a Forget, `Accounts.Remove`'s first step, and a folder that was
//! moved or deleted under its sync.

use std::io;
use std::path::{Path, PathBuf};

use super::folder::{Down, Is, Standing, Stopped};
use super::{SyncError, SyncService};

/// How long a Forget waits for the tree store's connections to close before it removes
/// the store's files.
const STORE_CLOSES_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// The folder it had is not brought back: it is forgotten, and its tree store is
    /// gone. An account that was
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

    /// Forgets the root and clears the published state. The files that are downloaded
    /// are left exactly as they are.
    ///
    /// Its sync is stopped first ([`change`](Self::change)), its mount is taken off, which
    /// leaves the folder on disk the user's own again, and its tree store is dropped. A
    /// Forget that is refused leaves it registered — so it is kept in step again.
    ///
    /// A folder that is not up — held with a `source` that cannot be read, one whose
    /// bring-up failed, or recorded by an account that is held back — is forgotten the same
    /// way.
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
    /// store that holds them goes with the folder. Asked before anything changes, and
    /// again once the sync has stopped.
    /// The folder that was forgotten.
    async fn forget(&self, retire: bool) -> Result<Option<PathBuf>, SyncError> {
        // Before the change: a reconcile, or a switch waiting for it, must not keep
        // the Forget from stopping the sync first.
        refuse_waiting(self.changes_waiting(self.tree_store()).await?)?;
        let mut stopped = self.change().await;
        if let Err(refused) = self.changes_waiting(stopped.folder().store()).await.and_then(refuse_waiting) {
            self.start_again(&mut stopped).await;
            return Err(refused);
        }
        self.restore_in(&mut stopped).await;
        let Some(mut record) = stopped.folder().record().cloned() else {
            if retire {
                retire_in(&mut stopped);
                return Ok(None);
            }
            return Err(SyncError::NoRoot);
        };
        // The folder is gone from the state, and with it everything kept with it: its
        // source and its tree store. The copy of the record this call holds gives its
        // store up too, and no reader holds one: what the view hands out carries none
        // (`Record::bare`).
        stopped.folder_mut().is = Is::Absent;
        // Whether the store's connections are closed is waited for below, once the view
        // and the activity log have let go of theirs too.
        let mut kept = std::mem::take(&mut record.kept);
        // The folder is unmounted: on disk it then shows its downloaded files alone.
        if let Some(mount) = kept.mount.take() {
            self.report.space.measure_at(None);
            let _ = tokio::task::spawn_blocking(move || mount.forget()).await;
        }
        let closing = kept.store.map(konedrive_tree::Store::close);
        if retire {
            retire_in(&mut stopped);
        }
        self.forgotten(&stopped).await;
        if let Some(closing) = closing {
            if tokio::time::timeout(STORE_CLOSES_WITHIN, closing).await.is_err() {
                tracing::warn!("the tree store of {} is still open somewhere; its files are removed all the same", record.root.path.display());
            }
        }
        self.remove_tree_store().await;
        // Only when *this daemon* is the one that excluded the folder from Baloo —
        // never a folder that arrived already excluded, and this survives a restart
        // in between (the record carries it forward from `config.toml`).
        if record.baloo {
            self.wiring.baloo.include_again(&record.root.path).await;
        }
        Ok(Some(record.root.path))
    }

    /// The folder is forgotten: `config.toml` records none, and everything published about
    /// it and its sync is cleared in one update, so that nothing is ever published about a
    /// folder that is no longer registered.
    async fn forgotten(&self, stopped: &Stopped<'_>) {
        self.persist_or_log(None);
        stopped.publish_with(|s| {
            s.cycle.listing = false;
            s.cycle.items_listed = 0;
            s.cycle.items_placed = 0;
            s.cycle.skipped_count = 0;
            s.cycle.sync_trouble = None;
            s.outbox.note = None;
            s.cycle.last_checked = 0;
            s.local.local_bytes = 0;
            s.local.conflict_count = 0;
        });
        // The pins went with the folder's store.
        self.state.update(|s| s.local.pinned_count = 0);
        // A Forget drops the activity with the folder's store, which the change has
        // already let go of. Its walker stops too.
        self.let_go_of_activity().await;
    }

    /// Removes the tree store's files: a forgotten folder's, or one left from a
    /// folder forgotten earlier when a new one is registered. Inside a change, once
    /// the folder is absent: no sync holds the store, and the change has let go of its own.
    pub(super) async fn remove_tree_store(&self) {
        // Its pause goes with it.
        self.drop_pause().await;
        let Some(tree_db) = self.sync_paths().map(|paths| paths.tree_db.clone()) else { return };
        if let Err(e) = tokio::task::spawn_blocking(move || remove_tree_files(&tree_db)).await {
            tracing::warn!("the task removing the tree store failed: {e}");
        }
    }
}

impl SyncService {
    /// A bring-up of the folder panicked, saying `panic`: a defect of the daemon. The
    /// folder's sync is stopped and the folder is down, saying so, until `Refresh()` or
    /// the next start brings it up again.
    pub(crate) async fn bring_up_panicked(&self, panic: &str) {
        let mut stopped = self.change().await;
        let folder = stopped.folder_mut();
        let record = match &folder.is {
            Is::Up(up) => up.record.clone(),
            Is::Down(record, _) => record.clone(),
            Is::Absent => return,
        };
        let why = format!(
            "bringing up the sync folder {} failed inside konedrive ({panic}); nothing is synced until it is brought up again",
            record.root.path.display()
        );
        folder.is = Is::Down(record, Down::Failed { why });
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

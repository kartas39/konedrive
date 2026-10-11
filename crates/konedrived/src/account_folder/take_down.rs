//! Taking a folder down: a Forget, `Accounts.Remove`'s first step, and a folder that was
//! moved or deleted under its sync.
// The skeleton of the switch: bodies that drove the old mechanism are `todo!()`.
#![allow(dead_code, unused_variables, unused_imports, unused_mut)]

use std::io;
use std::path::{Path, PathBuf};

use std::sync::Arc;

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
            "{n} change(s) made here have not been uploaded yet, and what the daemon knows of them \
             would be lost with the folder's record; wait until they are uploaded, or forget the \
             folder with force (the files stay here as they are)"
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
    ///
    /// `is_forced`: the folder is forgotten whatever is unsent — also a change OneDrive
    /// refused for good, which nothing else would ever take out of the way.
    pub async fn unregister_root(&self, is_forced: bool) -> Result<(), SyncError> {
        self.forget(false, is_forced).await.map(drop)
    }

    /// `Accounts.Remove`'s first step: the folder forgotten exactly as
    /// [`unregister_root`](Self::unregister_root) forgets it — refused under
    /// the same rule — and, in the same change so that nothing
    /// comes in between, the account retired: no registration, bring-up or
    /// switch is made for it from then on. An account with no folder is
    /// retired all the same. The answer is the folder that was forgotten: `None` for an
    /// account with no folder.
    pub async fn retire(&self, is_forced: bool) -> Result<Option<PathBuf>, SyncError> {
        self.forget(true, is_forced).await
    }

    /// [`unregister_root`](Self::unregister_root) and [`retire`](Self::retire).
    ///
    /// Refused `PendingUploads` while changes wait to be uploaded: the tree
    /// store that holds them goes with the folder. Asked before anything changes, and
    /// again once the sync has stopped.
    /// The folder that was forgotten.
    async fn forget(&self, retire: bool, is_forced: bool) -> Result<Option<PathBuf>, SyncError> {
        // TODO: `sync_tasks::stop_order::decide_forget(unsent, is_forced)` (over `ItemReader::count_unsent`, or
        // `count_unsent_of_closed_store`), then `stop_folder_sync` and `forget_folder`, or
        // `forget_folder_without_mount` for a folder that keeps no parts; `forgotten` and
        // `remove_tree_store` afterwards. A retry after a take-down that waits is given up with it.
        todo!()
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

/// How long a start of a folder must stay up for the count of the tries after a
/// take-down to start over.
pub(super) const STAYED_UP_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// How long the owner waits before it starts a folder again by itself after a fault took
/// its mount down, by how many such starts were tried since one stayed up: 1 minute
/// before the first (`attempt` 0), 5 minutes before the second, 30 before the third, and
/// an hour before every later one. Pure.
#[allow(unused_variables)]
pub(super) fn decide_retry_wait(attempt: u32) -> std::time::Duration {
    todo!()
}

/// The sentence the folder shows while it is down for `cause`, with the folder's path and
/// what the user can do ("sync now" tries at once). Pure.
#[allow(unused_variables)]
pub(super) fn describe_take_down(cause: &crate::mount::TakeDownCause, folder: &Path) -> String {
    todo!()
}

impl SyncService {
    /// Who the folder's mount tells that it must be taken down: what a start is given as
    /// `FolderStartInput::take_down_report`. Called from the write-operation thread, it
    /// never blocks: it spawns [`take_folder_down_for_fault`](Self::take_folder_down_for_fault)
    /// on the service's runtime, holding the service weakly.
    pub(super) fn take_down_report(self: &Arc<Self>) -> crate::mount::TakeDownReport {
        todo!()
    }

    /// The owner's task for a mount that asked for its take-down. Inside ONE change of
    /// the folder ([`change`](Self::change), which stops the sync): the view published
    /// without the parts and the activity log detached, then
    /// `sync_tasks::stop_order::take_folder_down` over the parts the stop answered; the
    /// folder's sync is `Why::TakenDown` with the sentence of `cause`
    /// ([`describe_take_down`]), the attempt as it was (0 after a start that stayed up
    /// for [`STAYED_UP_AFTER`]) and the time of the next try ([`decide_retry_wait`]; none
    /// when the store could not be closed); the desktop told to list the folder's
    /// directories again, which show the backing directory now
    /// (`desktop::dirnotify::read_again` of `mount::list_directories`). Then the wait:
    /// [`retry_after_take_down`](Self::retry_after_take_down).
    #[allow(unused_variables)]
    pub(super) async fn take_folder_down_for_fault(self: Arc<Self>, cause: crate::mount::TakeDownCause) {
        todo!()
    }

    /// Waits until the folder's next try is due (`Why::TakenDown::next_try_at`, by the
    /// account's clock) and starts the folder again, inside a change, with nothing kept:
    /// the check at start runs. The attempt is counted before the start. A start that is
    /// refused, or whose mount is taken down again, leaves `Why::TakenDown` with the next
    /// wait; one that stays up for [`STAYED_UP_AFTER`] ends the count. Ends without a
    /// start when the folder is no longer `Why::TakenDown` with this attempt when the
    /// time comes: a `Refresh()` started it, it was forgotten, or the daemon stops. The
    /// one timer of a taken-down folder; it holds the service weakly.
    pub(super) async fn retry_after_take_down(self: Arc<Self>) {
        todo!()
    }

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

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
    /// Never refused because something is unsent, and there is no forced variant: who
    /// asks for it accepts that the unsent files stay only here, as plain local files —
    /// also a change OneDrive refused for good. The warning is the caller's, BEFORE it
    /// calls: the window and `konedrivectl` read how many changes are unsent
    /// ([`count_unsent`](Self::count_unsent)) and show "N changes were not sent to
    /// OneDrive and will stay only on this computer".
    pub async fn unregister_root(&self) -> Result<(), SyncError> {
        self.forget(false).await.map(drop)
    }

    /// How many changes made here are unsent, for the warning before a Forget and a
    /// removal of the account: `ItemReader::count_unsent` through the folder's open store
    /// (`View::parts`), whether or not its sync runs; for a folder that keeps no open
    /// store (not up, taken down) `sync_tasks::stop_order::count_unsent_of_closed_store`
    /// of its store's file, off the runtime; 0 with no folder. While a sync runs the bus
    /// has the same number already, as `UploadQueue.PendingCount + BlockedCount +
    /// HeldCount` with the rows below held directories, so a client may use those.
    // TODO: for a folder whose sync does not run the properties of the bus are empty: the bus needs one member that answers this function (a property `UploadQueue.UnsentCount`), for the window and `konedrivectl` to warn by; it is not in the XML yet. Check and fix if needed.
    pub async fn count_unsent(&self) -> Result<u64, SyncError> {
        todo!()
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
    /// Not refused for what is unsent: the tree store that holds it goes with the folder,
    /// and the files stay.
    /// The folder that was forgotten.
    async fn forget(&self, retire: bool) -> Result<Option<PathBuf>, SyncError> {
        // TODO: inside the change — which stopped the sync and left its parts in `Kept::parts` —
        // `sync_tasks::stop_order::forget_folder` over those parts, or `forget_folder_without_mount` for a
        // folder that keeps no parts; `forgotten` and `remove_tree_store` afterwards. Nothing is asked of what
        // is unsent. A retry after a take-down that waits is given up with it.
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

/// Which try the next start by the owner is, after a fault took the mount down at `now`:
/// one more than `last`'s, the try the taken-down sync was started as — but 1 when that
/// start stayed up for [`STAYED_UP_AFTER`], and when the sync was no such try (`None`).
/// What `Why::TakenDown::attempt` is set to, less one (the starts tried so far). Pure.
// test: a_folder_taken_down_for_the_first_time_is_tried_as_the_first
// test: a_try_that_was_taken_down_again_soon_is_followed_by_the_next
// test: a_try_that_stayed_up_starts_the_count_over
#[allow(unused_variables)]
pub(super) fn decide_take_down_attempt(last: Option<super::folder::TakeDownTry>, now: i64) -> u32 {
    todo!()
}

impl SyncService {
    /// Who the folder's mount tells that it must be taken down: what a start is given as
    /// `FolderStartInput::take_down_report`. Called from the write-operation thread, it
    /// never blocks: it spawns [`take_folder_down_for_fault`](Self::take_folder_down_for_fault)
    /// on the service's runtime, with a weak reference to the service.
    pub(super) fn take_down_report(self: &Arc<Self>) -> crate::mount::TakeDownReport {
        todo!()
    }

    /// The owner's task for a mount that asked for its take-down; nothing when the
    /// service is gone. Inside ONE change of the folder ([`change`](Self::change), which
    /// stopped the sync and left its parts in `Kept::parts`): the view published without
    /// the parts and the activity log detached, then
    /// `sync_tasks::stop_order::take_folder_down` over those parts, taken out of the
    /// record; the folder's sync is `Why::TakenDown` with the sentence of `cause`
    /// ([`describe_take_down`]: the ONE maker of it, published by `publish` as the
    /// folder's blocking trouble), the starts tried so far
    /// ([`decide_take_down_attempt`] of `Up::take_down_try`) and the time of the next try
    /// ([`decide_retry_wait`]; none when the store could not be closed); the desktop told to list the folder's
    /// directories again, which show the backing directory now
    /// (`desktop::dirnotify::read_again` of `mount::list_directories`). Then the wait:
    /// [`retry_after_take_down`](Self::retry_after_take_down).
    #[allow(unused_variables)]
    pub(super) async fn take_folder_down_for_fault(service: std::sync::Weak<Self>, cause: crate::mount::TakeDownCause) {
        todo!()
    }

    /// Waits until the folder's next try is due (`Why::TakenDown::next_try_at`, by the
    /// account's clock) and starts the folder again, inside a change, with nothing kept:
    /// the check at start runs. The try is written beside the sync before the start
    /// (`Up::take_down_try`). A start that is refused leaves the folder `Why::TakenDown`
    /// — never `Why::CannotStart`, which nothing would try again — with the cause as it
    /// was, the try counted and the next wait; one whose mount is taken down again comes
    /// back through [`take_folder_down_for_fault`](Self::take_folder_down_for_fault);
    /// one that stays up for [`STAYED_UP_AFTER`] ends the count. Ends without a
    /// start when the folder is no longer `Why::TakenDown` with this attempt when the
    /// time comes: a `Refresh()` started it, it was forgotten, or the daemon stops. The
    /// one timer of a taken-down folder; across its wait it holds no strong reference to
    /// the service, and ends when the service is gone.
    pub(super) async fn retry_after_take_down(service: std::sync::Weak<Self>) {
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

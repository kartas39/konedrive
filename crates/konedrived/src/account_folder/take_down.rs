//! Taking a folder down: a Forget, `Accounts.Remove`'s first step, and a folder that was
//! moved or deleted under its sync.

use std::io;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use super::folder::{Down, Is, Standing, Stopped, Sync, TakeDownTry, Why};
use super::{SyncError, SyncService};
use crate::mount::TakeDownCause;
use crate::sync_tasks::stop_order::TakeDownOutcome;

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
    /// of its store's file, off the runtime; 0 with no folder. Every unsent change counts,
    /// whatever holds it — the three counts of the queue's properties do not add up to
    /// it (the rows below a held directory are in none). The bus's
    /// `UploadQueue.UnsentCount()` answers exactly this: the one member the window and
    /// `konedrivectl` read before `Folder.Unregister` and `Accounts.Remove`.
    pub async fn count_unsent(&self) -> Result<u64, SyncError> {
        // The open store of the folder, whether or not its sync runs.
        if let Some(parts) = self.view().parts {
            let unsent = parts.store.read_items(|reader| reader.count_unsent()).await;
            return unsent.map(|unsent| unsent.count).map_err(|e| SyncError::Store(e.to_string()));
        }
        // No folder; or one that keeps no open store: its store's file is asked, through a reader of its own.
        if self.folders().is_empty() {
            return Ok(0);
        }
        let Some(tree_db) = self.sync_paths().map(|paths| paths.tree_db.clone()) else { return Ok(0) };
        tokio::task::spawn_blocking(move || crate::sync_tasks::stop_order::count_unsent_of_closed_store(&tree_db))
            .await
            .map_err(|e| SyncError::Io(format!("the task counting what is unsent failed: {e}")))?
            .map(|unsent| unsent.count)
            .map_err(|e| SyncError::Store(e.to_string()))
    }

    /// `Accounts.Remove`'s first step: the folder forgotten exactly as
    /// [`unregister_root`](Self::unregister_root) forgets it — never refused for what
    /// is unsent; the caller of `Accounts.Remove` warns first by the same member,
    /// `UploadQueue.UnsentCount()` — and, in the same change so that nothing
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
        let mut stopped = self.change().await;
        self.restore_in(&mut stopped).await;
        let Some(mut record) = stopped.folder().record().cloned() else {
            if retire {
                retire_in(&mut stopped);
                return Ok(None);
            }
            return Err(SyncError::NoRoot);
        };
        // The folder is gone from the state, and with it everything kept with it — and the try after a take-down
        // that waited: its timer finds no folder taken down. The copy of the record this call holds has the parts
        // the stopped sync kept; the state's own went with the folder.
        stopped.folder_mut().is = Is::Absent;
        let kept = std::mem::take(&mut record.kept);
        if retire {
            retire_in(&mut stopped);
        }
        // Before the store is closed: `config.toml` records no folder, the view and the bus let go of the parts, and
        // the activity log of the store.
        self.forgotten(&stopped).await;
        self.report.space.measure_at(None);
        let closed = match kept.parts {
            // In a task of its own: a caller that is cut does not leave a mount half stopped.
            Some(parts) => tokio::spawn(crate::sync_tasks::stop_order::forget_folder(parts)).await.unwrap_or(TakeDownOutcome::StoreStillOpen),
            // No mount whose stop would give the write bits back.
            None => {
                let root = record.root.clone();
                let _ = tokio::spawn(async move { crate::sync_tasks::stop_order::forget_folder_without_mount(&root).await }).await;
                TakeDownOutcome::StoreClosed
            }
        };
        if closed == TakeDownOutcome::StoreStillOpen {
            tracing::warn!("the tree store of {} is still open somewhere; its files are removed all the same", record.root.path.display());
        }
        self.remove_tree_store().await;
        // Only when *this daemon* is the one that excluded the folder from Baloo — never a folder that arrived
        // already excluded, and this survives a restart in between (the record carries it forward from
        // `config.toml`).
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
pub(super) fn decide_retry_wait(attempt: u32) -> std::time::Duration {
    let minutes = match attempt {
        0 => 1,
        1 => 5,
        2 => 30,
        _ => 60,
    };
    std::time::Duration::from_secs(minutes * 60)
}

/// The sentence the folder shows while it is down for `cause`, with the folder's path and
/// what the user can do ("sync now" tries at once). Pure.
pub(super) fn describe_take_down(cause: &crate::mount::TakeDownCause, folder: &Path) -> String {
    // TODO: wording
    let what = match cause {
        TakeDownCause::UndoFailed { path } => format!("a change of {} could not be recorded and could not be undone", path.display()),
        TakeDownCause::BackingFileBehindRow { path } => format!("{} could not be made or cut on disk after the change was recorded", path.display()),
        TakeDownCause::StoreFailed { detail } => format!("konedrive's records of the folder could not be written ({detail})"),
    };
    format!(
        "the sync folder {} was stopped: {what}; konedrive checks the folder and starts it again by itself, and \"sync now\" tries at once",
        folder.display()
    )
}

/// Which try the next start by the owner is, after a fault took the mount down at `now`:
/// one more than `last`'s, the try the taken-down sync was started as — but 1 when that
/// start stayed up for [`STAYED_UP_AFTER`], and when the sync was no such try (`None`).
/// What `Why::TakenDown::attempt` is set to, less one (the starts tried so far). Pure.
// test: a_folder_taken_down_for_the_first_time_is_tried_as_the_first
// test: a_try_that_was_taken_down_again_soon_is_followed_by_the_next
// test: a_try_that_stayed_up_starts_the_count_over
pub(super) fn decide_take_down_attempt(last: Option<super::folder::TakeDownTry>, now: i64) -> u32 {
    match last {
        Some(tried) if now.saturating_sub(tried.started_at) < STAYED_UP_AFTER.as_secs() as i64 => tried.attempt.saturating_add(1),
        _ => 1,
    }
}

impl SyncService {
    /// Who the folder's mount tells that it must be taken down: what a start is given as
    /// `FolderStartInput::take_down_report`. Called from the write-operation thread, it
    /// never blocks: it spawns [`take_folder_down_for_fault`](Self::take_folder_down_for_fault)
    /// on the service's runtime, with a weak reference to the service.
    pub(super) fn take_down_report(self: &Arc<Self>) -> crate::mount::TakeDownReport {
        let service = Arc::downgrade(self);
        // The service's runtime, taken now: the report is called on the mount's write-operation thread, which has none.
        let runtime = tokio::runtime::Handle::current();
        Arc::new(move |cause| {
            runtime.spawn(Self::take_folder_down_for_fault(service.clone(), cause));
        })
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
    pub(super) async fn take_folder_down_for_fault(service: std::sync::Weak<Self>, cause: crate::mount::TakeDownCause) {
        let is_tried_again = {
            let Some(me) = service.upgrade() else { return };
            let mut stopped = me.change().await;
            // The mount that asked: the one the stopped sync kept. A report of a mount that is the folder's no more
            // (forgotten, taken down already, the daemon at its end) takes nothing down.
            // TODO: contract — `ItemMount` hands out no question "did you ask for your take-down" (`mount/item_mount.rs`): it is asked of the parts of the folder steps, which are handed out for another purpose.
            let is_asked = stopped.folder().record().and_then(|record| record.kept.parts.as_ref()).is_some_and(|kept| kept.mount.folder_step_parts().mount_take_down.is_asked());
            let Some(kept) = stopped.folder_mut().record_mut().filter(|_| is_asked).and_then(|record| record.kept.parts.take()) else {
                me.start_again(&mut stopped).await;
                return;
            };
            let now = me.clock.now();
            let Some(up) = stopped.folder_mut().up_mut() else {
                // Kept by a folder that is not up: there is no sync to say taken down.
                drop(tokio::task::spawn_blocking(move || drop(kept)));
                return;
            };
            let root = up.record.root.clone();
            let tried = decide_take_down_attempt(up.take_down_try.take(), now).saturating_sub(1);
            let said = describe_take_down(&cause, &root.path);
            tracing::error!("{said}");
            up.sync = Sync::Stopped(Why::TakenDown { cause: said.clone(), attempt: tried, next_try_at: None });
            // Before the store is closed: the view and the bus let go of the parts (the record keeps none now), and
            // the activity log of the store.
            stopped.publish();
            me.let_go_of_activity().await;
            me.report.space.measure_at(None);
            // In a task of its own: a mount is not left half stopped.
            let closed = tokio::spawn(crate::sync_tasks::stop_order::take_folder_down(kept)).await.unwrap_or(TakeDownOutcome::StoreStillOpen);
            // The next try, by the count; none while somebody still holds the store.
            let next_try_at = (closed == TakeDownOutcome::StoreClosed).then(|| me.clock.now().saturating_add(decide_retry_wait(tried).as_secs() as i64));
            if next_try_at.is_none() {
                tracing::error!("the tree store of {} is still open somewhere: the folder is not started again until konedrive is", root.path.display());
            }
            if let Some(up) = stopped.folder_mut().up_mut() {
                up.sync = Sync::Stopped(Why::TakenDown { cause: said, attempt: tried, next_try_at });
            }
            me.report.space.kick();
            // The folder's directories show the backing directory now: whoever lists one lists it again.
            let folder = root.path.clone();
            tokio::spawn(async move {
                if let Ok(directories) = tokio::task::spawn_blocking(move || crate::mount::list_directories(&folder)).await {
                    crate::desktop::dirnotify::read_again(directories).await;
                }
            });
            next_try_at.is_some()
        };
        if is_tried_again {
            Self::retry_after_take_down(service).await;
        }
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
        loop {
            // The try that is waited for; no strong reference to the service is held across the wait.
            let (clock, attempt, due_at) = {
                let Some(me) = service.upgrade() else { return };
                let folder = me.folder.read().await;
                match folder.up().map(|up| &up.sync) {
                    Some(Sync::Stopped(Why::TakenDown { attempt, next_try_at: Some(due_at), .. })) => (Arc::clone(&me.wiring.clock), *attempt, *due_at),
                    _ => return,
                }
            };
            // The clock's sleep knows nothing of a suspend or of a clock that was set: the time is looked at again.
            while clock.now() < due_at {
                clock.sleep_until(due_at).await;
            }
            let Some(me) = service.upgrade() else { return };
            let mut stopped = me.change().await;
            if stopped.folder().standing != Standing::Active {
                return;
            }
            let now = me.clock.now();
            // Still the try that was waited for: a `Refresh()` that started the folder, a Forget and the daemon's end
            // leave another state, and a folder taken down again since has a timer of its own.
            match stopped.folder_mut().up_mut() {
                Some(up) if matches!(&up.sync, Sync::Stopped(Why::TakenDown { attempt: tried, next_try_at: Some(due), .. }) if *tried == attempt && *due == due_at) => {
                    up.take_down_try = Some(TakeDownTry { attempt: attempt.saturating_add(1), started_at: now });
                }
                _ => return,
            }
            // With nothing kept: the check at start runs.
            me.start_sync(&mut stopped).await;
            if stopped.folder().running().is_some() {
                return;
            }
            // Refused: the folder is taken down still, the try counted, and the next is waited for here.
        }
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

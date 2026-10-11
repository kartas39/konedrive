//! The folder's side of the account's mode (`docs/design/writes.md` §2, §2.2): a OneDrive folder follows
//! the mode its account runs in (`Account.Mode`). A read-only folder's mount refuses every
//! change; a read-write one's takes them, and its outbox worker uploads them.
//!
//! A switch stops the folder's sync, changes the mode inside a change of its state, and
//! starts the sync again, whose first cycle is a Full reconcile under the new mode. The
//! sync that starts says to the mount whether it takes changes (`start_stop`).
//!
//! What the mode starts and stops with the folder's sync is beside this file: the outbox
//! worker with its rows (`outbox`), which [`PendingUploads`] counts before a switch to
//! read-only and drops when that switch is forced.
// The skeleton of the switch: bodies that drove the old mechanism are `todo!()`.
#![allow(dead_code, unused_variables, unused_imports, unused_mut)]

use std::sync::{Arc, Weak};

use tokio::sync::watch;

use super::folder::{Stopped, View};
use super::SyncService;
use crate::account::state::AccountSnapshot;
use crate::account::PendingUploads;
use crate::config::{Mode, WriteStanding};
use crate::status::snapshot::OutboxNote;

impl SyncService {
    /// The mode the folder follows now, as last published.
    pub fn mode(&self) -> Mode {
        self.view().wanted
    }

    /// Follows the account to `mode` (`docs/design/writes.md` §2, §2.2). The folder's sync is stopped as
    /// a Forget stops it, and the mode changed inside one change of the folder's state, so
    /// no reconcile, registration or free-up runs meanwhile. The sync then starts again, if
    /// it ran: its first cycle is a Full reconcile, in read-write mode it starts the outbox
    /// worker, and the folder's mount takes changes or refuses them as the new mode says.
    ///
    /// The accounts manager calls this once before the folder is restored, with the mode
    /// the account starts in: nothing is up then, so the folder only takes the mode.
    pub async fn follow_mode(&self, mode: Mode) {
        // TODO: `sync_tasks::access_mode_change::switch_to_read_only` or `switch_to_read_write`, while the
        // tasks run; a folder that is not up only takes the mode.
        todo!()
    }

}

/// The write gate of a folder: whether its account may change OneDrive now — asked by
/// the outbox worker before each row and between an upload's fragments
/// ([`OutboxHost::may_write`](crate::upload::OutboxHost::may_write)). It may while the folder and
/// the account are read-write, `config.toml` — read again now — says read-write and records a
/// drive for the account, the drive its token was last seen to reach is that one, its
/// token can write, and the folder's sync is not stopped by blocking trouble (`CycleError::blocking`:
/// another account's drive, a sign-out, a failure of the tree store; the cycle that
/// clears it wakes the worker, `Writes::reopened`). Closed, the folder's `LastError`
/// says why until it opens, and the account's mode is worked out again, which turns it
/// read-only and says why in the account's `LastError`.
///
/// It holds what it reads, and no way back to the service.
// TODO: check whether the gate stays as what `SendingHolds::set_writes_refusal` and
// `StandingSendingHolds::writes_refusal` are given, and fix if needed: nothing asks it now.
pub(super) struct Gate {
    view: watch::Receiver<View>,
    account: Arc<dyn crate::account::FolderAccount>,
    persist: super::Persist,
    state: crate::status::snapshot::SyncStateHandle,
}

impl Gate {
    /// Blocking: it reads `config.toml`, here and in the mode check. The worker asks its
    /// host from a blocking thread, as one section (`Engine::may_write`).
    pub(super) fn check(&self) -> Result<(), String> {
        let refusal = self.refusal();
        let note = OutboxNote::after_gate(&self.state.get().outbox.note, refusal.as_deref());
        let changed = note.is_some();
        if let Some(note) = note {
            self.state.update(|s| s.outbox.note = note);
        }
        match refusal {
            None => Ok(()),
            Some(why) => {
                if changed {
                    self.account.recheck_mode();
                }
                Err(why)
            }
        }
    }

    /// Why the gate is closed now, if it is.
    fn refusal(&self) -> Option<String> {
        if self.view.borrow().wanted != Mode::ReadWrite {
            return Some("the folder is read-only".into());
        }
        let persist = &self.persist;
        let snapshot = self.account.snapshot();
        if snapshot.mode != Mode::ReadWrite {
            return Some("the account is read-only".into());
        }
        if !konedrive_graph::oauth::grants_writes(&snapshot.granted_scopes) {
            return Some("the account's sign-in does not allow changes".into());
        }
        match persist.store.write_standing(&persist.account) {
            None => return Some("config.toml cannot be read".into()),
            Some(WriteStanding { mode: Mode::ReadOnly, .. }) => return Some("config.toml says the account is read-only".into()),
            Some(WriteStanding { recorded_drive: None, .. }) => {
                return Some("config.toml records no drive for the account".into())
            }
            Some(WriteStanding { recorded_drive: Some(drive), .. }) if drive != snapshot.live_drive => {
                return Some(format!(
                    "the account's token was last seen to reach drive {:?}, not drive {:?}, which config.toml records",
                    snapshot.live_drive,
                    drive.as_str()
                ))
            }
            Some(_) => {}
        }
        if let Some(trouble) = self.state.get().cycle.sync_trouble.filter(|t| t.blocking) {
            return Some(format!("the folder's sync is stopped ({})", trouble.text));
        }
        None
    }
}

impl SyncService {
    /// The folder's write gate ([`Gate`]).
    pub(super) fn gate(&self) -> Gate {
        Gate {
            view: self.view.subscribe(),
            account: Arc::clone(&self.wiring.account),
            persist: self.wiring.persist.clone(),
            state: self.state.clone(),
        }
    }
}

#[async_trait::async_trait]
impl PendingUploads for SyncService {
    /// `RefreshInfo` read the quota: the outbox decides by it whether OneDrive is
    /// still full.
    fn quota_read(&self, quota: &konedrive_graph::drive::DriveQuota) {
        // TODO: `OneDriveSpace::apply_quota` (`sync_parts::onedrive_space`) of the running sync.
        todo!()
    }

    /// How many changes wait to be uploaded — the outbox's live rows
    /// (`konedrive_tree::outbox`). A switch to read-only is refused `PendingUploads` while
    /// this is not 0 and the switch is not forced. A change saved a moment ago counts: the
    /// folder's mount records each one before it answers the program that made it.
    async fn pending_uploads(&self) -> u64 {
        // TODO: `ItemReader::count_unsent`, read through the folder's store (`Store::read_items`).
        todo!()
    }

    /// A forced switch to read-only drops the outbox's rows (`docs/design/writes.md`
    /// §2); the files stay, as ordinary local changes, protected by the read phase's stamp
    /// check and rescue. Called once `config.toml` says read-only. The worker is told to
    /// stop with every other part, before anything is waited for, so nothing more is sent.
    ///
    /// The folder's sync stops for the drop and starts again after it: the rows go inside
    /// a change of the folder's state, with no task running, as at any switch, so nothing
    /// here holds the state or the tree lock while it waits for the other with a cycle
    /// under way (`docs/design/writes.md` §9).
    ///
    /// A read-write folder turns read-only here, in the same step, rather than when it
    /// follows its account ([`SyncService::follow_mode`], which then finds it turned): its
    /// mount takes no change from then on, so nothing records again what was dropped,
    /// whether or not the folder is ever told to follow.
    ///
    /// A folder read-only already — its sync holding its cycles for these changes — starts
    /// its sync again without them, as a read-only start does: what a read-write cycle
    /// deferred is the base's, and the first cycle is a Full reconcile.
    async fn drop_pending_uploads(&self) {
        // TODO: `sync_tasks::access_mode_change::switch_to_read_only`: a forced switch drops nothing
        // (`access_mode_change`'s module doc) — check whether this member of `PendingUploads` stays, and fix if needed.
        todo!()
    }
}

/// Makes `sync` follow its account's mode (`AccountSnapshot::mode`, which the account works
/// out from `config.toml`, its recorded drive and its token) for as long as both exist. The accounts
/// manager starts one per account, once the folder has taken the mode the account started
/// in.
pub async fn follow(mut account: watch::Receiver<AccountSnapshot>, sync: Weak<SyncService>) {
    loop {
        let mode = account.borrow_and_update().mode;
        let Some(service) = sync.upgrade() else { return };
        service.follow_mode(mode).await;
        drop(service);
        if account.changed().await.is_err() {
            return;
        }
    }
}

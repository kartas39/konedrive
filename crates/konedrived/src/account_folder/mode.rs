//! The folder's side of its account: the ONE watcher of the account's state
//! ([`watch_account`]), and what the folder does when that changes — it follows the
//! account's mode, holds its sending while the account is signed out or may not write
//! ([`decide_account_holds`]), and starts a sync that waited for the account's drive.
//!
//! A read-only folder's mount refuses every change, and nothing of it is sent; a
//! read-write one's takes them, and they are sent. A switch stops no task of the sync
//! (`sync_tasks::access_mode_change`). What the account asks of the folder for a switch
//! is [`PendingUploads`]: how many changes are unsent, and to hold them here at the
//! switch itself. Nothing is ever dropped.
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

    /// Follows the account to `mode`. No task of the folder's sync is stopped for it: the
    /// folder's mount takes changes or refuses them as the new mode says, and a read-only
    /// folder sends nothing (`sync_tasks::access_mode_change`). The mode is changed inside
    /// one change of the folder's state.
    ///
    /// The accounts manager calls this once before the folder is restored, with the mode
    /// the account starts in: nothing is up then, so the folder only takes the mode.
    pub async fn follow_mode(&self, mode: Mode) {
        // TODO: `sync_tasks::access_mode_change::switch_to_read_only` or `switch_to_read_write`, over the parts
        // of the running sync and while its tasks run; a folder that is not up, or whose sync does not run, only
        // takes the mode (`Folder::wanted`), which its next start applies. Inside `hold_folder()`, not a
        // `change()`: no task is stopped. The mode applied is the one the ACCOUNT says once the folder's lock is
        // held (`FolderAccount::snapshot`), not `mode` as the watcher woke with it: a switch the account made
        // meanwhile through `hold_unsent_changes` is then found made, and is never turned back by an older
        // snapshot.
        todo!()
    }

}

/// What of the account holds the folder's sending, beside the folder's own access mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountHolds {
    /// Nobody is signed in.
    pub is_signed_out: bool,
    /// Why the account may not write to OneDrive now; `None` when it may, and when the
    /// only cause is that the account or the folder is read-only (that is the folder's
    /// own hold, `SendingHold::FolderReadOnly`).
    pub writes_refusal: Option<String>,
}

/// What `snapshot`, the account's state, and `standing`, what `config.toml` says of its
/// mode and drive now (`ConfigStore::write_standing`; `None` when the file cannot be
/// read), hold of the folder's sending. Pure. `is_signed_out` for any state but signed
/// in. The refusal, the first that holds, for an account `config.toml` says is
/// read-write: `config.toml` cannot be read; it records no drive; the token was last seen
/// to reach another drive than the recorded one (with both); the sign-in does not allow
/// changes (`oauth::grants_writes`). None for an account `config.toml` says is read-only:
/// its folder is read-only, and held as such. The folder's own trouble
/// (`cycle.sync_trouble`) is no part of it: a cycle that fails for another account's
/// drive tells the account (`drive_seen`), and the refusal follows from the snapshot.
/// One function for the start (`StandingSendingHolds::is_signed_out`, `writes_refusal`)
/// and for every later change ([`watch_account`]).
pub fn decide_account_holds(snapshot: &AccountSnapshot, standing: Option<&WriteStanding>) -> AccountHolds {
    todo!()
}

impl SyncService {
    /// The account's state changed to `snapshot`: what the folder does about it, all of
    /// it here. In this order:
    ///
    /// 1. the mode followed ([`follow_mode`](Self::follow_mode)), which applies the mode
    ///    the account says when the folder's lock is held — never `snapshot`'s, which may
    ///    be older than a switch the account made since;
    /// 2. what holds the sending of a running sync set to what [`decide_account_holds`]
    ///    answers of `snapshot`, each when it is not what the holds say: signed out —
    ///    `SyncTasks::hold_for_sign_out`; signed in while the holds say signed out —
    ///    `stop_order::release_after_sign_in`; the refusal —
    ///    `SendingHolds::set_writes_refusal`. This is the ONE place that sets and ends
    ///    them from outside the sync, and it applies the snapshots in their order: no
    ///    hold set from an older snapshot outlives a newer one;
    /// 3. a folder whose sync could not start because no drive was known
    ///    (`Why::DriveNotKnown`) started again, inside a change, when the snapshot or
    ///    `config.toml` names a drive now.
    ///
    /// Called by [`watch_account`] and by nobody else.
    pub(super) async fn follow_account(&self, snapshot: &AccountSnapshot) {
        todo!()
    }
}

#[async_trait::async_trait]
impl PendingUploads for SyncService {
    /// `RefreshInfo` read the quota: applied to the running sync's `OneDriveSpace`
    /// (`apply_quota(quota, now)`: who reads the quota applies it), and then the plan's
    /// task woken (`WakeCause::QuotaRead`), which takes what is free from `OneDriveSpace`
    /// and forgets the refusals that fit. Nothing with no running sync: its start reads
    /// the quota itself.
    fn quota_read(&self, quota: &konedrive_graph::drive::DriveQuota) {
        // TODO: `OneDriveSpace::apply_quota` of the running sync (`View::onedrive_space`), then `SyncPlanWake::wake(WakeCause::QuotaRead)` (`View::parts`).
        todo!()
    }

    /// How many changes made here are unsent. A change saved a moment ago counts: the
    /// folder's mount writes each one before it answers the program that made it. 0 for
    /// an account with no folder and for a folder whose store is not open.
    async fn pending_uploads(&self) -> u64 {
        // TODO: `ItemReader::count_unsent`, read through the folder's store (`Store::read_items`).
        todo!()
    }

    /// The account switches to read-only now (forced, or with nothing unsent): the
    /// folder turns read-only here, in this call, rather than when it follows its
    /// account ([`SyncService::follow_mode`], which then finds it turned) —
    /// inside [`hold_folder`](SyncService::hold_folder), `sync_tasks::access_mode_change::switch_to_read_only` over the parts of the running
    /// sync, which answers only when the folder takes no change, sends nothing and no
    /// step towards OneDrive runs; `wanted` set to read-only in the same change of the
    /// folder's state. The account gives its token up after this returned. Nothing is
    /// dropped: what is unsent stays in the rows and is sent when the folder is
    /// read-write again. Answers how many changes stay unsent
    /// (`ItemReader::count_unsent`, read after the switch). For a folder whose sync does
    /// not run there is nothing to hold: only `wanted` changes, and the next start
    /// applies it before its first plan.
    async fn hold_unsent_changes(&self) -> u64 {
        // TODO: `sync_tasks::access_mode_change::switch_to_read_only(mount, sending_holds, running_steps)`, then `ItemReader::count_unsent`.
        todo!()
    }
}

/// The owner's watcher of the account, for as long as both exist: at every change of the
/// account's state (`FolderAccount::changes`) it has the folder follow it
/// ([`SyncService::follow_account`]) — its mode, its sign-out, whether it may write, and
/// a drive that came to be known. The ONE task of the folder that watches the account;
/// the accounts manager starts one per account, once the folder has taken the mode the
/// account started in.
pub async fn watch_account(mut account: watch::Receiver<AccountSnapshot>, sync: Weak<SyncService>) {
    loop {
        let snapshot = account.borrow_and_update().clone();
        let Some(service) = sync.upgrade() else { return };
        service.follow_account(&snapshot).await;
        drop(service);
        if account.changed().await.is_err() {
            return;
        }
    }
}

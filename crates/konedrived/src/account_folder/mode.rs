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

use std::sync::{Arc, Weak};

use tokio::sync::watch;

use super::folder::{Standing, Sync, Why};
use super::SyncService;
use crate::account::state::{AccountSnapshot, SignInState};
use crate::account::{HeldUnsentChanges, PendingUploads};
use crate::config::{Mode, WriteStanding};
use crate::sync_parts::sending_holds::SendingHold;
use crate::sync_parts::sync_plan_wake::WakeCause;
use crate::sync_parts::running_steps::StepClass;
use crate::sync_tasks::access_mode_change::{self, StepsTowardsOneDrive};

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
        // Inside the second door: no task of the sync is stopped for a switch.
        let mut held = self.hold_folder().await;
        // The mode the ACCOUNT says now that the lock is held, not `mode` as the caller woke with it: a switch the
        // account made meanwhile (`hold_unsent_changes`) is found made, and is never turned back by an older snapshot.
        let woken_with = mode;
        let mode = self.wiring.account.snapshot().mode;
        if mode != woken_with {
            tracing::debug!("the account {:?} is {} by now, not {} as its folder was told", self.id(), mode.as_str(), woken_with.as_str());
        }
        if held.folder().wanted == mode {
            return;
        }
        held.set_wanted(mode);
        // A folder whose sync does not run takes the mode, which its next start applies; the mount it keeps, still
        // served, is switched now: it takes no change for an account that is read-only.
        let Some(started) = held.folder().running() else {
            if let Some(mount) = held.folder().mount() {
                switch_kept_mount(mount, mode).await;
            }
            return;
        };
        let sending_holds = started.tasks.sending_holds();
        let switched = match mode {
            // An account that turned read-only for a cause of its own holds no token that can write any more:
            // whether a step towards OneDrive still runs changes nothing here.
            Mode::ReadOnly => access_mode_change::switch_to_read_only(&started.mount, sending_holds, &started.running_steps).await.map(drop),
            Mode::ReadWrite => access_mode_change::switch_to_read_write(&started.mount, sending_holds).await,
        };
        match switched {
            Ok(()) => tracing::info!("the folder of the account {:?} is {} now", self.id(), mode.as_str()),
            Err(why) => tracing::warn!("the folder of the account {:?} could not be switched to {}: {why}", self.id(), mode.as_str()),
        }
    }

}

/// Switches the mount a stopped sync keeps to `mode`: the mount's own switch alone — there is
/// no sending to hold, and the next start sets the hold with the mode. A mount that asked for
/// its take-down made itself read-only for a fault, and refuses to take changes again.
async fn switch_kept_mount(mount: &crate::mount::ItemMount, mode: Mode) {
    match mode {
        Mode::ReadOnly => {
            if let Err(error) = mount.switch_to_read_only().await {
                tracing::warn!("the folder's mount could not be switched to read-only on its own thread: {error}");
            }
        }
        // `Err` for a mount whose take-down was asked: it stays read-only, as it made itself.
        Mode::ReadWrite => {
            if let Err(error) = mount.switch_to_read_write() {
                tracing::warn!("the folder's mount is not switched to read-write: {error}");
            }
        }
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
/// read-write: `config.toml` cannot be read; it records no drive; which drive the token
/// reaches has not been seen yet; the token was last seen
/// to reach another drive than the recorded one (with both); the sign-in does not allow
/// changes (`oauth::grants_writes`). None for an account `config.toml` says is read-only:
/// its folder is read-only, and held as such. The folder's own trouble
/// (`cycle.sync_trouble`) is no part of it: a cycle that fails for another account's
/// drive tells the account (`drive_seen`), and the refusal follows from the snapshot.
/// One function for the start (`StandingSendingHolds::is_signed_out`, `writes_refusal`)
/// and for every later change ([`watch_account`]).
pub fn decide_account_holds(snapshot: &AccountSnapshot, standing: Option<&WriteStanding>) -> AccountHolds {
    let is_signed_out = snapshot.state != SignInState::SignedIn;
    let writes_refusal = match standing {
        None => Some("config.toml cannot be read".to_owned()),
        // A read-only account's folder is read-only, and held as such.
        Some(WriteStanding { mode: Mode::ReadOnly, .. }) => None,
        Some(WriteStanding { recorded_drive: None, .. }) => Some("config.toml records no drive for the account".to_owned()),
        Some(WriteStanding { recorded_drive: Some(_), .. }) if snapshot.live_drive.is_empty() => {
            Some("which OneDrive drive the account's sign-in reaches has not been seen yet".to_owned())
        }
        Some(WriteStanding { recorded_drive: Some(drive), .. }) if *drive != snapshot.live_drive => Some(format!(
            "the account's token was last seen to reach drive {:?}, not drive {:?}, which config.toml records",
            snapshot.live_drive,
            drive.as_str()
        )),
        Some(_) if !konedrive_graph::oauth::grants_writes(&snapshot.granted_scopes) => Some("the account's sign-in does not allow changes".to_owned()),
        Some(_) => None,
    };
    AccountHolds { is_signed_out, writes_refusal }
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
        // 1: the mode.
        self.follow_mode(snapshot.mode).await;
        // 2: what of the account holds the sending of a running sync. `config.toml` is read off the runtime.
        let persist = self.wiring.persist.clone();
        let standing = tokio::task::spawn_blocking(move || persist.store.write_standing(&persist.account)).await.ok().flatten();
        let holds = decide_account_holds(snapshot, standing.as_ref());
        let now = self.clock.now();
        // The handles of the running sync, taken under the folder's lock and used without it.
        let (signed_in_again, waits_for_drive) = {
            let folder = self.folder.read().await;
            let signed_in_again = folder.running().and_then(|started| {
                let sending_holds = started.tasks.sending_holds();
                let says_signed_out = sending_holds.find(now) == Some(SendingHold::SignedOut);
                sending_holds.set_writes_refusal(holds.writes_refusal.clone());
                if holds.is_signed_out {
                    if !says_signed_out {
                        started.tasks.hold_for_sign_out();
                    }
                    return None;
                }
                says_signed_out.then(|| (started.store.clone(), Arc::clone(sending_holds), started.tasks.poll()))
            });
            let waits_for_drive = matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::DriveNotKnown(_))));
            (signed_in_again, waits_for_drive)
        };
        if let Some((store, sending_holds, poll)) = signed_in_again {
            crate::sync_tasks::stop_order::release_after_sign_in(&store, &sending_holds, &poll).await;
        }
        // 3: a folder that waited for the account's drive, when one is named now.
        if waits_for_drive && (!snapshot.live_drive.is_empty() || standing.is_some_and(|standing| standing.recorded_drive.is_some())) {
            let mut stopped = self.change().await;
            if matches!(stopped.folder().up().map(|up| &up.sync), Some(Sync::Stopped(Why::DriveNotKnown(_)))) && stopped.folder().standing == Standing::Active {
                self.start_sync(&mut stopped).await;
            } else {
                // Another change came first: what it left is left, and a sync this one stopped runs again.
                self.start_again(&mut stopped).await;
            }
        }
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
        let view = self.view();
        // Of a running sync alone: a start reads the quota itself.
        let (Some(onedrive_space), Some(parts)) = (view.onedrive_space, view.parts) else { return };
        onedrive_space.apply_quota(quota, self.clock.now());
        parts.sync_plan_wake.wake(WakeCause::QuotaRead);
    }

    /// Whether a step towards OneDrive runs now (`RunningSteps::list_steps` of the view's parts: one that sends
    /// content or metadata). It takes no lock of the folder and waits for nothing.
    fn has_request_in_flight(&self) -> bool {
        let Some(parts) = self.view().parts else { return false };
        parts.running_steps.list_steps().iter().any(|step| matches!(step.class, StepClass::Content | StepClass::Metadata))
    }

    /// How many changes made here are unsent. A change saved a moment ago counts: the
    /// folder's mount writes each one before it answers the program that made it. 0 for
    /// an account with no folder and for a folder whose store is not open.
    async fn pending_uploads(&self) -> u64 {
        let Some(store) = self.tree_store() else { return 0 };
        match store.read_items(|reader| reader.count_unsent()).await {
            Ok(unsent) => unsent.count,
            Err(error) => {
                tracing::warn!("what is unsent could not be counted: {error}");
                0
            }
        }
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
    /// (`ItemReader::count_unsent`, read after the switch), and whether a step towards
    /// OneDrive still ran when the switch's bounded wait for them ended
    /// (`StepsTowardsOneDrive::StillRunning`): the account then keeps its token and asks
    /// again — the switch made once more changes nothing and waits anew. For a folder
    /// whose sync does not run there is nothing to hold and no request in flight: only
    /// `wanted` changes, and the next start applies it before its first plan.
    async fn hold_unsent_changes(&self) -> HeldUnsentChanges {
        let (store, is_request_in_flight) = {
            let mut held = self.hold_folder().await;
            held.set_wanted(Mode::ReadOnly);
            // For a folder whose sync does not run there is nothing to hold: its next start applies the mode.
            let is_request_in_flight = match held.folder().running() {
                Some(started) => match access_mode_change::switch_to_read_only(&started.mount, started.tasks.sending_holds(), &started.running_steps).await {
                    Ok(steps) => steps == StepsTowardsOneDrive::StillRunning,
                    Err(why) => {
                        tracing::warn!("the folder of the account {:?} holds what is unsent, but its mount could not be switched to read-only: {why}", self.id());
                        // The answer does not say whether a step still runs: the running steps are asked.
                        started.running_steps.list_steps().iter().any(|step| matches!(step.class, StepClass::Content | StepClass::Metadata))
                    }
                },
                // No sync, no request; the mount a stopped sync keeps, still served, takes no change from here on.
                None => {
                    if let Some(mount) = held.folder().mount() {
                        switch_kept_mount(mount, Mode::ReadOnly).await;
                    }
                    false
                }
            };
            (held.folder().store(), is_request_in_flight)
        };
        // Counted after the switch: what stays here unsent.
        let unsent = match store {
            Some(store) => match store.read_items(|reader| reader.count_unsent()).await {
                Ok(unsent) => unsent.count,
                Err(error) => {
                    tracing::warn!("what stays unsent could not be counted: {error}");
                    0
                }
            },
            None => 0,
        };
        HeldUnsentChanges { unsent, is_request_in_flight }
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

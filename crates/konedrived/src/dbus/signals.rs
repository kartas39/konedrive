//! What the daemon announces by itself: `PropertiesChanged` for the properties of
//! `properties` (what it decides of the account as a whole among them), `ActivityLog.Added`,
//! and `Accounts.HelperState`.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::HelperState;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::ObjectPath;
use zbus::Connection;

use crate::dbus::accounts::Accounts;
use crate::account::state::AccountSnapshot;
use crate::dbus::properties::{self, Changed, Seen, AT_ONCE, COALESCED, DECIDED, PATH};
use crate::dbus::{ActivityLog, Folder};
use crate::status::snapshot::SyncSnapshot;
use crate::status::transfers::Transfer;
use crate::sync::SyncService;

/// The shortest time between two coalesced `PropertiesChanged`: at most four
/// a second.
pub const COALESCE: Duration = Duration::from_millis(250);

/// Turns the changes of `service`'s published state into signals of the folder's
/// interfaces at `path`, which are on the bus already; the tasks that send them, to stop
/// when the account goes.
pub(crate) async fn start_signals(
    connection: &Connection,
    path: &ObjectPath<'_>,
    service: Arc<SyncService>,
) -> zbus::Result<Vec<JoinHandle<()>>> {
    let folder = connection.object_server().interface::<_, Folder>(path).await?;
    // Every interface of the folder is on the same object: one emitter sends for all.
    let emitter = folder.signal_emitter().to_owned();
    // Every subscription is taken here, not inside its task: a change that lands between
    // now and the task's first poll is announced, not taken for where things stood.
    let mut changes = service.state().subscribe();
    let mut previous = Seen { snapshot: changes.borrow_and_update().clone(), ..Seen::default() };
    // A second subscription, for the properties that change with every page of a listing
    // and every read of a download: sent on their own schedule, they never hold up the
    // ones sent at once.
    let mut counters = service.state().subscribe();
    let mut downloads = service.report().transfers.subscribe();
    let shown = Seen { snapshot: counters.borrow_and_update().clone(), downloads: downloads.borrow_and_update().clone(), ..Seen::default() };
    let mut added = service.report().activity.subscribe();
    // A third, for what is decided of the account as a whole: it follows the account's
    // sign-in and whether anything downloads too.
    let (mut whole, mut sign_in, mut moving) = (service.state().subscribe(), service.account().changes(), service.report().transfers.subscribe());
    let decided_from = seen(&mut whole, &mut sign_in, &mut moving);
    let decided_emitter = emitter.clone();
    let decided = tokio::spawn(decide(whole, sign_in, moving, decided_from, move |changed| {
        let emitter = decided_emitter.clone();
        async move {
            if let Err(e) = emit(&emitter, changed).await {
                tracing::warn!("cannot emit PropertiesChanged for the account's overall state: {e}");
            }
        }
    }));

    let at_once = emitter.clone();
    let states = tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            // Nothing sent at once is read from the downloads.
            let current = Seen { snapshot: changes.borrow_and_update().clone(), ..Seen::default() };
            let changed = properties::changed(AT_ONCE, &previous, &current);
            // `Source` is the folder's record, decided when the folder is registered and
            // kept with it: it changes only when the path does.
            let moved = changed.get(PATH.interface).is_some_and(|folder| folder.contains_key(PATH.name));
            if let Err(e) = emit(&at_once, changed).await {
                tracing::warn!("cannot emit PropertiesChanged for the folder: {e}");
            }
            if moved {
                if let Err(e) = folder.get().await.source_changed(folder.signal_emitter()).await {
                    tracing::warn!("cannot emit PropertiesChanged for the folder's source: {e}");
                }
            }
            previous = current;
        }
    });
    let coalesced_emitter = emitter.clone();
    let coalesced = tokio::spawn(coalesce(counters, downloads, shown, move |changed| {
        let emitter = coalesced_emitter.clone();
        async move {
            if let Err(e) = emit(&emitter, changed).await {
                tracing::warn!("cannot emit PropertiesChanged for the sync counters: {e}");
            }
        }
    }));
    let activity = tokio::spawn(async move {
        loop {
            match added.recv().await {
                Ok(e) => {
                    if let Err(err) = ActivityLog::added(&emitter, e.at, e.kind.as_str(), &e.path, &e.detail).await {
                        tracing::warn!("cannot emit ActivityLog.Added: {err}");
                    }
                }
                // `Recent` still has them; only the live signal is lost.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!("{missed} ActivityLog.Added signal(s) were not sent: too many events at once");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Ok(vec![states, coalesced, activity, decided])
}

/// Announces `Accounts.HelperState` at every change of `helper`, the hub's own state; the
/// task that does. It ends only with the process: through `Accounts` it holds the account
/// manager, and so the hub whose state it follows. What `helper` holds now is where things
/// stand: a change before the task first runs is still announced.
pub(crate) fn announce_helper_state(accounts: InterfaceRef<Accounts>, mut helper: watch::Receiver<HelperState>) -> JoinHandle<()> {
    helper.borrow_and_update();
    tokio::spawn(async move {
        while helper.changed().await.is_ok() {
            helper.borrow_and_update();
            if let Err(e) = accounts.get().await.helper_state_changed(accounts.signal_emitter()).await {
                tracing::warn!("cannot emit PropertiesChanged for HelperState: {e}");
            }
        }
    })
}

/// What the three hold now, each taken as seen.
pub(crate) fn seen(
    state: &mut watch::Receiver<SyncSnapshot>,
    account: &mut watch::Receiver<AccountSnapshot>,
    downloads: &mut watch::Receiver<BTreeMap<u64, Transfer>>,
) -> Seen {
    Seen { snapshot: state.borrow_and_update().clone(), downloads: downloads.borrow_and_update().clone(), sign_in: account.borrow_and_update().state }
}

/// Hands `emit` the properties of [`DECIDED`] — `Overall`, `Trouble` and `NotUpdated` —
/// whenever what they say changes since they were last sent (`shown` at first), each change
/// by itself and at once; nothing for a change of the published state, the sign-in or the
/// downloads that leaves all three as they were. Returns when one of the three goes away.
pub(crate) async fn decide<F, Fut>(
    mut state: watch::Receiver<SyncSnapshot>,
    mut account: watch::Receiver<AccountSnapshot>,
    mut downloads: watch::Receiver<BTreeMap<u64, Transfer>>,
    mut shown: Seen,
    mut emit: F,
) where
    F: FnMut(Changed) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        tokio::select! {
            changed = state.changed() => if changed.is_err() { return },
            changed = account.changed() => if changed.is_err() { return },
            changed = downloads.changed() => {
                if changed.is_err() {
                    return;
                }
                // Every read of a download changes its entry; only whether anything
                // downloads is decided by.
                if downloads.borrow_and_update().is_empty() == shown.downloads.is_empty() {
                    continue;
                }
            }
        }
        let now = seen(&mut state, &mut account, &mut downloads);
        let changed = properties::changed(DECIDED, &shown, &now);
        if !changed.is_empty() {
            emit(changed).await;
        }
        shown = now;
    }
}

/// Hands `emit` the properties of [`COALESCED`] that changed since they were last sent
/// (`shown` at first) — at most once per [`COALESCE`]: a change during the wait is sent
/// when it is over, together with every other. Nothing is sent for a change that leaves
/// all of them as they were (a `State` change, say). Returns when either side goes away.
pub(crate) async fn coalesce<F, Fut>(
    mut state: watch::Receiver<SyncSnapshot>,
    mut downloads: watch::Receiver<BTreeMap<u64, Transfer>>,
    mut shown: Seen,
    mut emit: F,
) where
    F: FnMut(Changed) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        tokio::select! {
            changed = state.changed() => if changed.is_err() { return },
            changed = downloads.changed() => if changed.is_err() { return },
        }
        let now = Seen { snapshot: state.borrow_and_update().clone(), downloads: downloads.borrow_and_update().clone(), ..Seen::default() };
        let changed = properties::changed(COALESCED, &shown, &now);
        if !changed.is_empty() {
            emit(changed).await;
            shown = now;
            tokio::time::sleep(COALESCE).await;
        }
    }
}

/// One `PropertiesChanged` for each interface of `changed`, carrying every property of it
/// that changed with its value, through `fdo::Properties::properties_changed` and not the
/// `*_changed` each `#[zbus(property)]` generates: those would put a message on the bus
/// for every property.
async fn emit(emitter: &SignalEmitter<'_>, changed: Changed) -> zbus::Result<()> {
    for (interface, changed) in changed {
        zbus::fdo::Properties::properties_changed(
            emitter,
            zbus::names::InterfaceName::from_static_str(interface).expect("a valid interface name"),
            changed,
            std::borrow::Cow::Borrowed(&[]),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::{CONFLICTS_INTERFACE_NAME, FOLDER_INTERFACE_NAME, LOCAL_SCAN_INTERFACE_NAME, TRANSFERS_INTERFACE_NAME, UPLOAD_QUEUE_INTERFACE_NAME};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::ObjectPath;
use zbus::Connection;

use crate::status::transfers::Transfer;
use crate::status::snapshot::{published_error, published_state, SyncSnapshot};
use crate::sync::SyncService;
use crate::dbus::{ActivityLog, Folder, UploadQueue};

/// The shortest time between two coalesced `PropertiesChanged`: at most four
/// a second.
pub const COALESCE: Duration = Duration::from_millis(250);

/// Turns `SyncService`'s state changes into `PropertiesChanged`, the same
/// `StateHandle` → `PropertiesChanged` mechanism `crate::dbus::export` uses
/// for `Account`; each under the interface that holds the property.
pub(crate) async fn start_signals(
    connection: &Connection,
    path: &ObjectPath<'_>,
    service: Arc<SyncService>,
) -> zbus::Result<Vec<JoinHandle<()>>> {
    let server = connection.object_server();
    let folder = server.interface::<_, Folder>(path).await?;
    let queue = server.interface::<_, UploadQueue>(path).await?;
    // Captured before spawning (not inside the task): otherwise a state
    // change landing between attaching the interfaces and the task's first
    // poll would be absorbed into this baseline instead of being emitted as
    // a PropertiesChanged signal — see `crate::dbus::serve`'s identical
    // comment for `Account`.
    let mut changes = service.state().subscribe();
    let mut previous = changes.borrow_and_update().clone();
    // A second subscription so the counters — and the status properties
    // of — can be coalesced on their own schedule: a listing
    // changes the counters with every page, and a download its transfer with
    // every read, far more often than `State`, `Path` and
    // `LastError` change, and neither may hold those up.
    let mut counters = service.state().subscribe();
    let mut transfers = service.report().transfers.subscribe();
    let shown = Coalesced::of(&counters.borrow_and_update(), &transfers.borrow_and_update());
    // Every interface of the folder is on the same object: one emitter sends for all.
    let counters_emitter = folder.signal_emitter().to_owned();
    // Taken here too, for the same reason as `changes`: an event recorded
    // between here and the task's first poll is still sent.
    let mut added = service.report().activity.subscribe();
    let activity_emitter = folder.signal_emitter().to_owned();
    // The queue totals, counted from the rest into the state (issue #16).
    let totals = tokio::spawn(crate::status::totals::run(service.state().clone(), service.report().transfers.clone()));
    let states = tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let current = changes.borrow_and_update().clone();
            if let Err(e) = emit_changes(&folder, &queue, &previous, &current).await {
                tracing::warn!("cannot emit PropertiesChanged for the folder: {e}");
            }
            previous = current;
        }
    });
    let coalesced = tokio::spawn(coalesce(counters, transfers, shown, move |old, new| {
        let emitter = counters_emitter.clone();
        async move {
            if let Err(e) = emit_coalesced(&emitter, &old, &new).await {
                tracing::warn!("cannot emit PropertiesChanged for the sync counters: {e}");
            }
        }
    }));
    let activity = tokio::spawn(async move {
        loop {
            match added.recv().await {
                Ok(e) => {
                    if let Err(err) = ActivityLog::added(&activity_emitter, e.at, e.kind.as_str(), &e.path, &e.detail).await {
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
    Ok(vec![states, coalesced, activity, totals])
}

/// What travels in the coalesced `PropertiesChanged`: the counters (spec
/// §3.1) and the status properties, as last sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Coalesced {
    items_listed: u64,
    items_placed: u64,
    skipped_count: u64,
    last_checked: i64,
    local_bytes: u64,
    conflict_count: u32,
    pinned_count: u32,
    downloads: Vec<(String, u64, u64)>,
    pending_count: u32,
    pending_bytes: u64,
    blocked_count: u32,
    held_count: u32,
    uploads: Vec<(String, u64, u64)>,
    /// Files moving each way now, and the large files among them the sync moves (issue #50).
    active_downloads: u32,
    active_uploads: u32,
    large_files: u32,
    space_waiting_count: u32,
    space_waiting_bytes: u64,
    too_big_count: u32,
    throughput: konedrive_graph::pool::Throughput,
    queue: crate::status::totals::QueueTotals,
    scan: crate::status::snapshot::LocalScan,
}

/// The properties that changed, by interface, then by name, with their values now.
pub(crate) type Changed = BTreeMap<&'static str, HashMap<&'static str, zbus::zvariant::Value<'static>>>;

impl Coalesced {
    fn of(s: &SyncSnapshot, transfers: &BTreeMap<u64, Transfer>) -> Self {
        Self {
            items_listed: s.items_listed,
            items_placed: s.items_placed,
            skipped_count: s.skipped_count,
            last_checked: s.last_checked,
            local_bytes: s.local_bytes,
            conflict_count: s.conflict_count,
            pinned_count: s.pinned_count,
            downloads: transfers.values().map(|t| (t.path.clone(), t.done, t.total)).collect(),
            pending_count: s.pending_count,
            pending_bytes: s.pending_bytes,
            blocked_count: s.blocked_count,
            held_count: s.held_count,
            uploads: s.uploads.clone(),
            active_downloads: u32::try_from(transfers.len()).unwrap_or(u32::MAX),
            active_uploads: u32::try_from(s.uploads.len()).unwrap_or(u32::MAX),
            large_files: crate::status::transfers::large_files(transfers, &s.uploads),
            space_waiting_count: s.space_waiting_count,
            space_waiting_bytes: s.space_waiting_bytes,
            too_big_count: s.too_big_count,
            throughput: s.throughput,
            queue: s.queue,
            scan: s.scan.clone(),
        }
    }

    /// The properties that differ from `old`, under the interface that holds each, with
    /// their values now. An interface with nothing changed is not there.
    fn changed_since(&self, old: &Self) -> Changed {
        let mut changed = Changed::new();
        let mut put = |interface: &'static str, name: &'static str, value: zbus::zvariant::Value<'static>| {
            changed.entry(interface).or_default().insert(name, value);
        };
        let folder = FOLDER_INTERFACE_NAME;
        if old.items_listed != self.items_listed {
            put(folder, "ItemsListed", self.items_listed.into());
        }
        if old.items_placed != self.items_placed {
            put(folder, "ItemsPlaced", self.items_placed.into());
        }
        if old.skipped_count != self.skipped_count {
            put(folder, "SkippedCount", self.skipped_count.into());
        }
        if old.last_checked != self.last_checked {
            put(folder, "LastChecked", self.last_checked.into());
        }
        if old.local_bytes != self.local_bytes {
            put(folder, "LocalBytes", self.local_bytes.into());
        }
        if old.pinned_count != self.pinned_count {
            put(folder, "PinnedCount", self.pinned_count.into());
        }
        if old.conflict_count != self.conflict_count {
            put(CONFLICTS_INTERFACE_NAME, "Count", self.conflict_count.into());
        }
        let queue = UPLOAD_QUEUE_INTERFACE_NAME;
        if old.pending_count != self.pending_count {
            put(queue, "PendingCount", self.pending_count.into());
        }
        if old.pending_bytes != self.pending_bytes {
            put(queue, "PendingBytes", self.pending_bytes.into());
        }
        if old.blocked_count != self.blocked_count {
            put(queue, "BlockedCount", self.blocked_count.into());
        }
        if old.held_count != self.held_count {
            put(queue, "HeldCount", self.held_count.into());
        }
        if old.space_waiting_count != self.space_waiting_count {
            put(queue, "QuotaWaitingCount", self.space_waiting_count.into());
        }
        if old.space_waiting_bytes != self.space_waiting_bytes {
            put(queue, "QuotaWaitingBytes", self.space_waiting_bytes.into());
        }
        if old.too_big_count != self.too_big_count {
            put(queue, "TooBigCount", self.too_big_count.into());
        }
        let moving = TRANSFERS_INTERFACE_NAME;
        if old.downloads != self.downloads {
            put(moving, "Downloads", self.downloads.clone().into());
        }
        if old.uploads != self.uploads {
            put(moving, "Uploads", self.uploads.clone().into());
        }
        let (was, now) = (old.throughput, self.throughput);
        for (name, before, after) in [("DownloadSpeed", was.down_speed, now.down_speed), ("UploadSpeed", was.up_speed, now.up_speed)] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        for (name, before, after) in [
            ("ActiveDownloads", old.active_downloads, self.active_downloads),
            ("ActiveUploads", old.active_uploads, self.active_uploads),
            ("LargeFiles", old.large_files, self.large_files),
            ("PoolInUse", was.in_use, now.in_use),
            ("PoolSize", was.size, now.size),
            ("PoolCeiling", was.ceiling, now.ceiling),
            ("LargeStreams", was.large, now.large),
            ("LargeStreamLimit", was.large_limit, now.large_limit),
            ("RetryAfter", was.retry_after, now.retry_after),
        ] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        let (was, now) = (old.queue, self.queue);
        for (name, before, after) in [
            ("DownloadLeftBytes", was.down.left_bytes, now.down.left_bytes),
            ("DownloadDoneBytes", was.down.done_bytes, now.down.done_bytes),
            ("UploadLeftBytes", was.up.left_bytes, now.up.left_bytes),
            ("UploadDoneBytes", was.up.done_bytes, now.up.done_bytes),
        ] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        for (name, before, after) in [
            ("DownloadLeftCount", was.down.left_count, now.down.left_count),
            ("DownloadTimeLeft", was.down.time_left, now.down.time_left),
            ("UploadLeftCount", was.up.left_count, now.up.left_count),
            ("UploadTimeLeft", was.up.time_left, now.up.time_left),
        ] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        let scan = LOCAL_SCAN_INTERFACE_NAME;
        let (was, now) = (&old.scan, &self.scan);
        if was.state != now.state {
            put(scan, "State", now.state.as_str().to_owned().into());
        }
        if was.reason != now.reason {
            put(scan, "Reason", now.reason.clone().into());
        }
        if was.started != now.started {
            put(scan, "Started", now.started.into());
        }
        if was.directories != now.directories {
            put(scan, "Directories", now.directories.into());
        }
        if was.files != now.files {
            put(scan, "Files", now.files.into());
        }
        if was.expected != now.expected {
            put(scan, "Expected", now.expected.into());
        }
        if was.finished != now.finished {
            put(scan, "Finished", now.finished.into());
        }
        if was.took != now.took {
            put(scan, "Took", now.took.into());
        }
        changed
    }
}

/// Hands `emit` what changed — the value last sent and the one now — at most
/// once per [`COALESCE`]: a change during the wait is sent when it is over,
/// together with every other, as one message per interface. Nothing is sent
/// for a change that leaves all of it as it was (a `State` change, say).
/// Returns when either side goes away.
pub(crate) async fn coalesce<F, Fut>(
    mut state: watch::Receiver<SyncSnapshot>,
    mut transfers: watch::Receiver<BTreeMap<u64, Transfer>>,
    mut shown: Coalesced,
    mut emit: F,
) where
    F: FnMut(Coalesced, Coalesced) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        tokio::select! {
            changed = state.changed() => if changed.is_err() { return },
            changed = transfers.changed() => if changed.is_err() { return },
        }
        let now = Coalesced::of(&state.borrow_and_update(), &transfers.borrow_and_update());
        if now != shown {
            emit(shown, now.clone()).await;
            shown = now;
            tokio::time::sleep(COALESCE).await;
        }
    }
}

async fn emit_changes(
    folder: &InterfaceRef<Folder>,
    queue: &InterfaceRef<UploadQueue>,
    old: &SyncSnapshot,
    new: &SyncSnapshot,
) -> zbus::Result<()> {
    let emitter = folder.signal_emitter();
    let folder = folder.get().await;
    if old.root_path != new.root_path {
        folder.path_changed(emitter).await?;
        // The source is decided when a folder is registered and
        // kept with it for good, so it only ever changes alongside the path.
        folder.source_changed(emitter).await?;
    }
    // What is published is computed from the registration and the sync
    // together, so that is what is compared.
    if published_state(old) != published_state(new) {
        folder.state_changed(emitter).await?;
    }
    if published_error(old) != published_error(new) {
        folder.last_error_changed(emitter).await?;
    }
    // Not coalesced: a pause and a resume within one coalescing window would
    // leave a client that read in between with the pause for good.
    if old.paused_until != new.paused_until {
        folder.paused_changed(emitter).await?;
        folder.paused_until_changed(emitter).await?;
    }
    if old.held_back != new.held_back {
        folder.held_back_changed(emitter).await?;
    }
    if old.live_changes != new.live_changes {
        folder.live_changes_changed(emitter).await?;
    }
    // Not coalesced either: the tray says once that OneDrive is full.
    if old.quota_full != new.quota_full {
        queue.get().await.quota_full_changed(queue.signal_emitter()).await?;
    }
    // `HelperState` itself is `Accounts`'s; a change of it shows here
    // only as the `LastError` it changes (the comparison above).
    Ok(())
}

/// As [`emit_changes`], for the counters (`ItemsListed`, `ItemsPlaced`,
/// `SkippedCount`), the status properties (`LastChecked`, `LocalBytes`,
/// `Conflicts.Count`, `PinnedCount`), the transfers and the queue — kept separate so their own
/// coalescing ([`coalesce`]: at most four `PropertiesChanged` a second per interface,
/// since a listing changes the counters with every page and a download its
/// transfer with every read) never holds up `State`, `Path` or
/// `LastError`.
///
/// Sent as one `PropertiesChanged` signal per interface carrying every property of it that
/// changed since the last tick, through `fdo::Properties::properties_changed`
/// directly rather than the per-property `*_changed` helpers each
/// property's own `#[zbus(property)]` generates: calling those separately
/// would put up to eight signals on the bus per tick — eight times the ≤4-a-
/// second asks for, not one within it.
async fn emit_coalesced(emitter: &SignalEmitter<'_>, old: &Coalesced, new: &Coalesced) -> zbus::Result<()> {
    for (interface, changed) in new.changed_since(old) {
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

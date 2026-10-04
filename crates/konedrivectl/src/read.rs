//! What the commands read of the daemon into the types `text/` prints.

use konedrive_dbus::accounts::FolderProxies;
use konedrivectl::text::accounts::AccountRow;
use konedrivectl::text::status::{AccountStatus, FolderStatus, LocalScan};
use konedrivectl::text::transfers::{QueueTotals, TransferSummary};
use zbus::zvariant::OwnedObjectPath;

use crate::daemon::Daemon;

/// What a read of a property gave, or `None` when the daemon has no such property: a daemon
/// of an older build, not restarted, must not make a command of this build fail. Every other
/// failure is the read's.
async fn served<T>(read: impl std::future::Future<Output = zbus::Result<T>>) -> zbus::Result<Option<T>> {
    match read.await {
        Ok(value) => Ok(Some(value)),
        Err(error) if konedrive_dbus::is_unknown_property(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// [`served`], for a value whose line is printed only when it says something: nothing when
/// the daemon has no such property.
async fn said<T: Default>(read: impl std::future::Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    Ok(served(read).await?.unwrap_or_default())
}

/// [`served`], for two values of one line.
async fn both<A, B>(
    first: impl std::future::Future<Output = zbus::Result<A>>,
    second: impl std::future::Future<Output = zbus::Result<B>>,
) -> zbus::Result<Option<(A, B)>> {
    Ok(served(first).await?.zip(served(second).await?))
}

/// The account at `path`, as `status` shows it.
pub(crate) async fn account_status(daemon: &Daemon, path: &OwnedObjectPath) -> zbus::Result<AccountStatus> {
    let account = daemon.account(path).await?;
    Ok(AccountStatus {
        label: served(account.label()).await?,
        state: served(account.state()).await?,
        mode: served(account.mode()).await?,
        display_name: served(account.display_name()).await?,
        email: said(account.email()).await?,
        quota: both(account.quota_used(), account.quota_total()).await?,
        last_error: said(account.last_error()).await?,
    })
}

/// The account at `path`, as `account list` shows it.
pub(crate) async fn account_row(daemon: &Daemon, path: &OwnedObjectPath) -> zbus::Result<AccountRow> {
    let (account, sync) = (daemon.account(path).await?, daemon.sync(path).await?);
    Ok(AccountRow {
        id: account.id().await?,
        label: account.label().await?,
        email: account.email().await?,
        state: account.state().await?,
        mode: account.mode().await?,
        folder: sync.folder.path().await?,
        root_state: sync.folder.state().await?,
    })
}

/// The folder of the account at `path`, as `sync status` shows it.
pub(crate) async fn folder_status(daemon: &Daemon, path: &OwnedObjectPath) -> zbus::Result<FolderStatus> {
    let FolderProxies { folder, transfers, queue, conflicts, scan, .. } = daemon.sync(path).await?;
    let account = daemon.account(path).await?;
    let scan = match served(scan.state()).await? {
        None => None,
        Some(state) => Some(LocalScan {
            state,
            reason: said(scan.reason()).await?,
            started: said(scan.started()).await?,
            directories: said(scan.directories()).await?,
            files: said(scan.files()).await?,
            expected: said(scan.expected()).await?,
            finished: said(scan.finished()).await?,
            took: said(scan.took()).await?,
        }),
    };
    Ok(FolderStatus {
        path: said(folder.path()).await?,
        state: served(folder.state()).await?,
        last_error: said(folder.last_error()).await?,
        source: said(folder.source()).await?,
        items: both(folder.items_listed(), folder.items_placed()).await?,
        skipped: said(folder.skipped_count()).await?,
        last_checked: served(folder.last_checked()).await?,
        live_changes: said(folder.live_changes()).await?,
        mode: served(account.mode()).await?,
        download_left: both(transfers.download_left_count(), transfers.download_left_bytes()).await?,
        scan,
        pending: both(queue.pending_count(), queue.pending_bytes()).await?,
        blocked: said(queue.blocked_count()).await?,
        quota_full: said(queue.quota_full()).await?,
        quota_waiting: said(queue.quota_waiting_count()).await?,
        quota_waiting_bytes: said(queue.quota_waiting_bytes()).await?,
        too_big: said(queue.too_big_count()).await?,
        held_deletes: said(queue.held_count()).await?,
        paused: said(folder.paused()).await?,
        paused_until: said(folder.paused_until()).await?,
        held_back: said(folder.held_back()).await?,
        local_bytes: served(folder.local_bytes()).await?,
        pinned: served(folder.pinned_count()).await?,
        conflicts: said(conflicts.count()).await?,
    })
}

/// What `sync transfers` and `sync outbox` say first, from `Transfers`.
pub(crate) async fn transfer_summary(proxy: &FolderProxies<'_>) -> zbus::Result<TransferSummary> {
    let transfers = &proxy.transfers;
    Ok(TransferSummary {
        active_downloads: transfers.active_downloads().await?,
        download_speed: transfers.download_speed().await?,
        downloads: QueueTotals {
            left_count: transfers.download_left_count().await?,
            left_bytes: transfers.download_left_bytes().await?,
            done_bytes: transfers.download_done_bytes().await?,
            time_left: transfers.download_time_left().await?,
        },
        active_uploads: transfers.active_uploads().await?,
        upload_speed: transfers.upload_speed().await?,
        uploads: QueueTotals {
            left_count: transfers.upload_left_count().await?,
            left_bytes: transfers.upload_left_bytes().await?,
            done_bytes: transfers.upload_done_bytes().await?,
            time_left: transfers.upload_time_left().await?,
        },
        pool_in_use: transfers.pool_in_use().await?,
        pool_size: transfers.pool_size().await?,
        large_files: transfers.large_files().await?,
        large_streams: transfers.large_streams().await?,
        large_stream_limit: transfers.large_stream_limit().await?,
        retry_after: transfers.retry_after().await?,
    })
}

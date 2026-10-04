//! What the commands read of the daemon into the types `text/` prints.

use konedrive_dbus::accounts::FolderProxies;
use konedrivectl::text::accounts::AccountRow;
use konedrivectl::text::status::{AccountStatus, FolderStatus, LocalScan};
use konedrivectl::text::transfers::{QueueTotals, TransferSummary};
use zbus::zvariant::OwnedObjectPath;

use crate::daemon::Daemon;

/// The account at `path`, as `status` shows it.
pub(crate) async fn account_status(daemon: &Daemon, path: &OwnedObjectPath) -> zbus::Result<AccountStatus> {
    let account = daemon.account(path).await?;
    Ok(AccountStatus {
        label: account.label().await?,
        state: account.state().await?,
        mode: account.mode().await?,
        display_name: account.display_name().await?,
        email: account.email().await?,
        quota_used: account.quota_used().await?,
        quota_total: account.quota_total().await?,
        last_error: account.last_error().await?,
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
    Ok(FolderStatus {
        path: folder.path().await?,
        state: folder.state().await?,
        last_error: folder.last_error().await?,
        source: folder.source().await?,
        items_listed: folder.items_listed().await?,
        items_placed: folder.items_placed().await?,
        skipped: folder.skipped_count().await?,
        last_checked: folder.last_checked().await?,
        live_changes: folder.live_changes().await?,
        mode: daemon.account(path).await?.mode().await?,
        download_left: transfers.download_left_count().await?,
        download_left_bytes: transfers.download_left_bytes().await?,
        scan: LocalScan {
            state: scan.state().await?,
            reason: scan.reason().await?,
            started: scan.started().await?,
            directories: scan.directories().await?,
            files: scan.files().await?,
            expected: scan.expected().await?,
            finished: scan.finished().await?,
            took: scan.took().await?,
        },
        pending: queue.pending_count().await?,
        pending_bytes: queue.pending_bytes().await?,
        blocked: queue.blocked_count().await?,
        quota_full: queue.quota_full().await?,
        quota_waiting: queue.quota_waiting_count().await?,
        quota_waiting_bytes: queue.quota_waiting_bytes().await?,
        too_big: queue.too_big_count().await?,
        held_deletes: queue.held_count().await?,
        paused: folder.paused().await?,
        paused_until: folder.paused_until().await?,
        held_back: folder.held_back().await?,
        local_bytes: folder.local_bytes().await?,
        pinned: folder.pinned_count().await?,
        conflicts: conflicts.count().await?,
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

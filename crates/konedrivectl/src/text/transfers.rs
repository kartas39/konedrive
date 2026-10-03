use konedrive_dbus::accounts::FolderProxies;

use super::formats::{grouped, human_bytes};

/// One direction's queue totals (issue #16): `Transfers`' `DownloadLeftCount`,
/// `DownloadLeftBytes`, `DownloadDoneBytes`, `DownloadTimeLeft`, or the same four for uploads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueTotals {
    /// Files (downloads) or changes (uploads) left.
    pub left_count: u32,
    pub left_bytes: u64,
    /// Bytes moved in this run.
    pub done_bytes: u64,
    /// Seconds; 0 when unknown.
    pub time_left: u32,
}

/// What `sync transfers` says first: the files moving each way and the account's transfer
/// pool (`Transfers`' `ActiveDownloads`, `DownloadSpeed`, `ActiveUploads`, `UploadSpeed`,
/// `PoolInUse`, `PoolSize`, `LargeFiles`, `LargeStreams`, `LargeStreamLimit`, `RetryAfter`)
/// and the queue totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferSummary {
    /// Files downloading and uploading now, each once.
    pub active_downloads: u32,
    pub download_speed: u64,
    pub downloads: QueueTotals,
    pub active_uploads: u32,
    pub upload_speed: u64,
    pub uploads: QueueTotals,
    /// Slots held now, and the pool's size: in use may be above the size.
    pub pool_in_use: u32,
    pub pool_size: u32,
    /// Large files the sync moves now; the streams of large sync transfers, and their limit.
    pub large_files: u32,
    pub large_streams: u32,
    pub large_stream_limit: u32,
    /// Seconds left of OneDrive's `Retry-After`; 0 when there is none.
    pub retry_after: u32,
}

/// Reads a [`TransferSummary`] from `Transfers`.
pub async fn transfer_summary(proxy: &FolderProxies<'_>) -> zbus::Result<TransferSummary> {
    Ok(TransferSummary {
        active_downloads: proxy.transfers.active_downloads().await?,
        download_speed: proxy.transfers.download_speed().await?,
        downloads: QueueTotals {
            left_count: proxy.transfers.download_left_count().await?,
            left_bytes: proxy.transfers.download_left_bytes().await?,
            done_bytes: proxy.transfers.download_done_bytes().await?,
            time_left: proxy.transfers.download_time_left().await?,
        },
        active_uploads: proxy.transfers.active_uploads().await?,
        upload_speed: proxy.transfers.upload_speed().await?,
        uploads: QueueTotals {
            left_count: proxy.transfers.upload_left_count().await?,
            left_bytes: proxy.transfers.upload_left_bytes().await?,
            done_bytes: proxy.transfers.upload_done_bytes().await?,
            time_left: proxy.transfers.upload_time_left().await?,
        },
        pool_in_use: proxy.transfers.pool_in_use().await?,
        pool_size: proxy.transfers.pool_size().await?,
        large_files: proxy.transfers.large_files().await?,
        large_streams: proxy.transfers.large_streams().await?,
        large_stream_limit: proxy.transfers.large_stream_limit().await?,
        retry_after: proxy.transfers.retry_after().await?,
    })
}

/// The pool's line, as the window shows it too (issue #50): the slots in use of the pool's
/// size, then the large files and their streams — "Pool: 7 of 32 · large files: 1 (4 of 4
/// streams)" — with "— OneDrive asked to wait 30 s" during a `Retry-After`. In use may be
/// above the size (an open's reserve; slots still held after a throttle halved the pool), and
/// is shown as it is.
pub fn pool_text(summary: &TransferSummary) -> String {
    let mut line = format!(
        "Pool: {} of {} · large files: {} ({} of {} streams)",
        summary.pool_in_use, summary.pool_size, summary.large_files, summary.large_streams, summary.large_stream_limit
    );
    if summary.retry_after > 0 {
        line.push_str(&format!(" — OneDrive asked to wait {} s", summary.retry_after));
    }
    line
}

/// A queue's time left: `about 45 s`, `about 12 min`, `about 2 h 5 min`, `about 3 d 4 h`.
pub fn time_left_text(seconds: u32) -> String {
    let seconds = u64::from(seconds);
    let minutes = seconds.div_ceil(60);
    if seconds < 60 {
        format!("about {seconds} s")
    } else if minutes < 60 {
        format!("about {minutes} min")
    } else if seconds < 86_400 {
        let (h, m) = (minutes / 60, minutes % 60);
        if m == 0 { format!("about {h} h") } else { format!("about {h} h {m} min") }
    } else {
        let hours = seconds.div_ceil(3600);
        let (d, h) = (hours / 24, hours % 24);
        if h == 0 { format!("about {d} d") } else { format!("about {d} d {h} h") }
    }
}

/// One direction's summary line of `sync transfers` (and of `sync outbox`, for uploads):
/// `Downloading: 12 now, 1 234 files left (48.2 GiB, about 12 min), 3.1 GiB done, 8.4 MiB/s`.
/// What is left, and what is done, only while anything is left; the time only when known.
fn direction_line(label: &str, active: u32, totals: &QueueTotals, noun: (&str, &str), speed: u64) -> String {
    let mut line = format!("{label:<12} {active:>2} now, ");
    if totals.left_count > 0 {
        let noun = if totals.left_count == 1 { noun.0 } else { noun.1 };
        line.push_str(&format!("{} {noun} left", grouped(totals.left_count.into())));
        let mut about = Vec::new();
        if totals.left_bytes > 0 {
            about.push(human_bytes(totals.left_bytes));
        }
        if totals.time_left > 0 {
            about.push(time_left_text(totals.time_left));
        }
        if !about.is_empty() {
            line.push_str(&format!(" ({})", about.join(", ")));
        }
        line.push_str(&format!(", {} done, ", human_bytes(totals.done_bytes)));
    }
    line.push_str(&format!("{}/s", human_bytes(speed)));
    line
}

/// The `Downloading:` summary line.
pub fn downloading_line(summary: &TransferSummary) -> String {
    direction_line("Downloading:", summary.active_downloads, &summary.downloads, ("file", "files"), summary.download_speed)
}

/// The `Uploading:` summary line: what is left to upload is counted in changes.
pub fn uploading_line(summary: &TransferSummary) -> String {
    direction_line("Uploading:", summary.active_uploads, &summary.uploads, ("change", "changes"), summary.upload_speed)
}

/// `sync status`'s `Waiting to download:` line: `1 234 files (48.2 GiB)`.
pub fn waiting_download_text(count: u32, bytes: u64) -> String {
    match count {
        0 => "nothing".to_owned(),
        1 => format!("1 file ({})", human_bytes(bytes)),
        n => format!("{} files ({})", grouped(n.into()), human_bytes(bytes)),
    }
}

/// `sync transfers`: how many files go each way now, what is left and done, how fast, and
/// the pool; then one line per download and upload under way — its direction, path, how
/// far, and the whole size.
pub fn transfers_text(summary: &TransferSummary, downloads: &[(String, u64, u64)], uploads: &[(String, u64, u64)]) -> String {
    let mut out = format!("{}\n{}\n{}\n", downloading_line(summary), uploading_line(summary), pool_text(summary));
    if downloads.is_empty() && uploads.is_empty() {
        out.push_str("Nothing is downloading or uploading.\n");
        return out;
    }
    let lines = downloads.iter().map(|t| ("down", t)).chain(uploads.iter().map(|t| ("up", t)));
    for (direction, (path, done, total)) in lines {
        let percent = if *total == 0 { 0 } else { done.saturating_mul(100) / total };
        out.push_str(&format!("{direction:<4} {path}  {percent}%  {}\n", human_bytes(*total)));
    }
    out
}

#[cfg(test)]
mod tests;

use zbus::interface;

use crate::dbus::Transfers;

#[interface(name = "org.konedrive.Transfers")]
impl Transfers {
    #[zbus(property)]
    async fn downloads(&self) -> Vec<(String, u64, u64)> {
        self.service.transfers()
    }

    /// Uploads under way, shaped as `Downloads`.
    #[zbus(property)]
    async fn uploads(&self) -> Vec<(String, u64, u64)> {
        self.service.state().get().uploads
    }

    /// Bytes a second downloaded, the average of the last 3 s.
    #[zbus(property)]
    async fn download_speed(&self) -> u64 {
        self.service.state().get().throughput.down_speed
    }

    /// Bytes a second uploaded, the average of the last 3 s.
    #[zbus(property)]
    async fn upload_speed(&self) -> u64 {
        self.service.state().get().throughput.up_speed
    }

    /// Files downloading now: the entries of `Downloads`, each file once however many
    /// streams it runs (issue #50).
    #[zbus(property)]
    async fn active_downloads(&self) -> u32 {
        u32::try_from(self.service.transfers().len()).unwrap_or(u32::MAX)
    }

    /// Files uploading now: the entries of `Uploads`.
    #[zbus(property)]
    async fn active_uploads(&self) -> u32 {
        u32::try_from(self.service.state().get().uploads.len()).unwrap_or(u32::MAX)
    }

    /// Every slot of the pool held now, all four classes, the opens' reserve included: may be
    /// above `PoolSize` (issue #50).
    #[zbus(property)]
    async fn pool_in_use(&self) -> u32 {
        self.service.state().get().throughput.in_use
    }

    /// The large files (100 MiB and up) the sync moves now, each once however many streams it
    /// runs; files being opened left out (issue #50).
    #[zbus(property)]
    async fn large_files(&self) -> u32 {
        self.service.large_files()
    }

    /// The size of the account's transfer pool now.
    #[zbus(property)]
    async fn pool_size(&self) -> u32 {
        self.service.state().get().throughput.size
    }

    /// Its ceiling (`[transfers] max` in `config.toml`).
    #[zbus(property)]
    async fn pool_ceiling(&self) -> u32 {
        self.service.state().get().throughput.ceiling
    }

    /// The streams of large sync transfers (100 MiB and up) under way now; a file being opened
    /// is never one.
    #[zbus(property)]
    async fn large_streams(&self) -> u32 {
        self.service.state().get().throughput.large
    }

    /// How many streams of large sync transfers may run at once (`[transfers] large` in
    /// `config.toml`).
    #[zbus(property)]
    async fn large_stream_limit(&self) -> u32 {
        self.service.state().get().throughput.large_limit
    }

    /// Seconds left of OneDrive's `Retry-After` wait, during which no transfer starts; 0
    /// when there is none.
    #[zbus(property)]
    async fn retry_after(&self) -> u32 {
        self.service.state().get().throughput.retry_after
    }

    /// Files left to download: the pinned files waiting and every download under way
    /// (issue #16, `sync::totals`).
    #[zbus(property)]
    async fn download_left_count(&self) -> u32 {
        self.service.state().get().queue.down.left_count
    }

    /// Their size, less what the downloads under way have received.
    #[zbus(property)]
    async fn download_left_bytes(&self) -> u64 {
        self.service.state().get().queue.down.left_bytes
    }

    /// Bytes downloaded since nothing was last left to download, or since the daemon started.
    #[zbus(property)]
    async fn download_done_bytes(&self) -> u64 {
        self.service.state().get().queue.down.done_bytes
    }

    /// Seconds the downloads left take at the last 30 s's speed; 0 when unknown.
    #[zbus(property)]
    async fn download_time_left(&self) -> u32 {
        self.service.state().get().queue.down.time_left
    }

    /// Changes left to upload: `PendingCount` less those waiting for space or too big for it.
    #[zbus(property)]
    async fn upload_left_count(&self) -> u32 {
        self.service.state().get().queue.up.left_count
    }

    /// `PendingBytes`, less what the uploads under way have sent.
    #[zbus(property)]
    async fn upload_left_bytes(&self) -> u64 {
        self.service.state().get().queue.up.left_bytes
    }

    /// Bytes uploaded since nothing was last left to upload, or since the daemon started.
    #[zbus(property)]
    async fn upload_done_bytes(&self) -> u64 {
        self.service.state().get().queue.up.done_bytes
    }

    /// Seconds the uploads left take at the last 30 s's speed; 0 when unknown, and while paused.
    #[zbus(property)]
    async fn upload_time_left(&self) -> u32 {
        self.service.state().get().queue.up.time_left
    }
}

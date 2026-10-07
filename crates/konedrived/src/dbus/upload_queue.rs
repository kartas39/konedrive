use konedrive_dbus::rows;
use zbus::interface;

use crate::dbus::fault::Result;
use crate::dbus::properties::*;
use crate::dbus::UploadQueue;

fn kept_back(kept: crate::sync::outbox::KeptBack) -> rows::KeptBack {
    rows::KeptBack { path: kept.path, reason: kept.reason }
}

#[interface(name = "org.konedrive.UploadQueue")]
impl UploadQueue {
    /// The changes waiting to be uploaded, oldest first, at most `limit` (0 for all).
    async fn changes(&self, limit: u32) -> Result<Vec<rows::Change>> {
        let changes = self.service.outbox(limit).await?;
        Ok(changes
            .into_iter()
            .map(|c| rows::Change { seq: c.seq, kind: c.kind, path: c.path, state: c.state, sent: c.sent, total: c.total, reason: c.reason, next_try: c.next_try })
            .collect())
    }

    /// The removals the mass-delete guard held go ahead; how many.
    async fn confirm_deletes(&self) -> Result<u32> {
        Ok(self.service.confirm_deletes().await?)
    }

    /// The removals the mass-delete guard held are dropped, and their items
    /// placed again; how many.
    async fn restore_deletes(&self) -> Result<u32> {
        Ok(self.service.restore_deletes().await?)
    }

    /// What stays on this computer and why.
    async fn not_uploaded(&self) -> Result<Vec<rows::KeptBack>> {
        Ok(self.service.not_uploaded().await?.into_iter().map(kept_back).collect())
    }

    /// What is kept back, one row per reason.
    async fn not_uploaded_summary(&self) -> Result<Vec<rows::KeptBackReason>> {
        let summary = self.service.not_uploaded_summary().await?;
        Ok(summary.into_iter().map(|(group, reason, count, bytes)| rows::KeptBackReason { group, reason, count, bytes }).collect())
    }

    /// The files kept back for one reason, at most `limit` (0 for all), and how many there are.
    #[zbus(out_args("items", "total"))]
    async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<rows::KeptBack>, u32)> {
        let (items, total) = self.service.not_uploaded_files(reason, limit).await?;
        Ok((items.into_iter().map(kept_back).collect(), total))
    }

    #[zbus(property)]
    async fn pending_count(&self) -> u32 {
        PENDING_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn pending_bytes(&self) -> u64 {
        PENDING_BYTES.of(&self.service)
    }

    #[zbus(property)]
    async fn blocked_count(&self) -> u32 {
        BLOCKED_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn held_count(&self) -> u32 {
        HELD_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn quota_full(&self) -> bool {
        QUOTA_FULL.of(&self.service)
    }

    #[zbus(property)]
    async fn quota_waiting_count(&self) -> u32 {
        QUOTA_WAITING_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn quota_waiting_bytes(&self) -> u64 {
        QUOTA_WAITING_BYTES.of(&self.service)
    }

    #[zbus(property)]
    async fn too_big_count(&self) -> u32 {
        TOO_BIG_COUNT.of(&self.service)
    }
}

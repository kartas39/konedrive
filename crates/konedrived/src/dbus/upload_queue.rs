use zbus::interface;

use crate::dbus::fault::{Result, to_fault};
use crate::dbus::UploadQueue;

#[interface(name = "org.konedrive.UploadQueue")]
impl UploadQueue {
    /// The changes waiting to be uploaded, oldest first, at most `limit` (0 for
    /// all): (seq, kind, full path, state, bytes sent, bytes in all, reason,
    /// next try).
    async fn changes(&self, limit: u32) -> Result<Vec<(u64, String, String, String, u64, u64, String, i64)>> {
        self.service.outbox(limit).await.map_err(to_fault)
    }

    /// The removals the mass-delete guard held go ahead; how many.
    async fn confirm_deletes(&self) -> Result<u32> {
        self.service.confirm_deletes().await.map_err(to_fault)
    }

    /// The removals the mass-delete guard held are dropped, and their items
    /// placed again; how many.
    async fn restore_deletes(&self) -> Result<u32> {
        self.service.restore_deletes().await.map_err(to_fault)
    }

    /// What stays on this computer and why: (full path, reason).
    async fn not_uploaded(&self) -> Result<Vec<(String, String)>> {
        self.service.not_uploaded().await.map_err(to_fault)
    }

    /// What is kept back, one row per reason: (group, reason, count, bytes).
    async fn not_uploaded_summary(&self) -> Result<Vec<(String, String, u32, u64)>> {
        self.service.not_uploaded_summary().await.map_err(to_fault)
    }

    /// The files kept back for one reason, at most `limit` (0 for all), and how many there are.
    #[zbus(out_args("items", "total"))]
    async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<(String, String)>, u32)> {
        self.service.not_uploaded_files(reason, limit).await.map_err(to_fault)
    }

    /// Changes waiting to be uploaded (not blocked, not held).
    #[zbus(property)]
    async fn pending_count(&self) -> u32 {
        self.service.state().get().outbox.pending_count
    }

    /// The size of the files those changes send.
    #[zbus(property)]
    async fn pending_bytes(&self) -> u64 {
        self.service.state().get().outbox.pending_bytes
    }

    /// Changes that need the user to go up.
    #[zbus(property)]
    async fn blocked_count(&self) -> u32 {
        self.service.state().get().outbox.blocked_count
    }

    /// Removals the mass-delete guard holds for `ConfirmDeletes` or
    /// `RestoreDeletes`.
    #[zbus(property)]
    async fn held_count(&self) -> u32 {
        self.service.state().get().outbox.held_count
    }

    /// OneDrive is full: no content goes up (issue #2).
    #[zbus(property)]
    async fn quota_full(&self) -> bool {
        self.service.state().get().outbox.quota_full
    }

    #[zbus(property)]
    async fn quota_waiting_count(&self) -> u32 {
        self.service.state().get().outbox.space_waiting_count
    }

    #[zbus(property)]
    async fn quota_waiting_bytes(&self) -> u64 {
        self.service.state().get().outbox.space_waiting_bytes
    }

    #[zbus(property)]
    async fn too_big_count(&self) -> u32 {
        self.service.state().get().outbox.too_big_count
    }
}

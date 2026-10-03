use zbus::object_server::SignalEmitter;
use zbus::interface;

use crate::dbus::ActivityLog;
use crate::dbus::fault::{Result, to_fault};

#[interface(name = "org.konedrive.ActivityLog")]
impl ActivityLog {
    /// The newest `limit` events, newest first: (unix time, kind, full path,
    /// detail).
    async fn recent(&self, limit: u32) -> Result<Vec<(i64, String, String, String)>> {
        let events = self.service.recent_activity(limit).await.map_err(to_fault)?;
        Ok(events.into_iter().map(|e| (e.at, e.kind, e.path, e.detail)).collect())
    }

    /// One per event, as it is recorded; the same fields as `Recent`.
    #[zbus(signal)]
    pub(crate) async fn added(emitter: &SignalEmitter<'_>, time: i64, kind: &str, path: &str, detail: &str) -> zbus::Result<()>;
}

use zbus::interface;

use crate::dbus::Conflicts;
use crate::dbus::fault::{Result, to_fault};

#[interface(name = "org.konedrive.Conflicts")]
impl Conflicts {
    /// (unix time, original full path, full path of the kept version, how it
    /// was kept: `rescued` or `copy`), newest first; one whose kept file is
    /// gone is dropped.
    async fn list(&self) -> Result<Vec<(i64, String, String, String)>> {
        let rows = self.service.conflicts().await.map_err(to_fault)?;
        Ok(rows.into_iter().map(|c| (c.at, c.original, c.rescued, c.kind.as_str().to_owned())).collect())
    }

    async fn dismiss(&self, rescued_path: &str) -> Result<()> {
        self.service.dismiss_conflict(rescued_path).await.map_err(to_fault)
    }

    #[zbus(property)]
    async fn count(&self) -> u32 {
        self.service.status().2
    }

    #[zbus(property)]
    async fn machine_name(&self) -> String {
        self.service.machine_name()
    }
}

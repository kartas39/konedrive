use konedrive_dbus::rows::Conflict;
use zbus::interface;

use crate::dbus::Conflicts;
use crate::dbus::fault::Result;
use crate::dbus::properties::CONFLICT_COUNT;

#[interface(name = "org.konedrive.Conflicts")]
impl Conflicts {
    /// (unix time, original full path, full path of the kept version, how it
    /// was kept: `rescued` or `copy`), newest first; one whose kept file is
    /// gone is dropped.
    async fn list(&self) -> Result<Vec<Conflict>> {
        let rows = self.service.conflicts().await?;
        Ok(rows.into_iter().map(|c| Conflict { at: c.at, original: c.original, kept: c.rescued, how: c.kind.as_str().to_owned() }).collect())
    }

    async fn dismiss(&self, rescued_path: &str) -> Result<()> {
        Ok(self.service.dismiss_conflict(rescued_path).await?)
    }

    #[zbus(property)]
    async fn count(&self) -> u32 {
        CONFLICT_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn machine_name(&self) -> String {
        self.service.machine_name()
    }
}
